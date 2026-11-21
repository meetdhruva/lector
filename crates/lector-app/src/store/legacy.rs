//! One-time import of the JSON state files of 0.3 to 0.7.1 (ADR 0004, decision 3).
//!
//! Files, relative to the data directory (every layout 0.7.0 and 0.7.1 wrote, and the v0.3
//! stores they still had on disk):
//!
//! | file | written by | concern |
//! |---|---|---|
//! | `recent.json` | 0.3 (0.4+ kept it in the database) | `recent_projects` |
//! | `sessions/<key>.json` | 0.3 (0.4+: database) | `window_sessions` |
//! | `review.json` | 0.3 (0.4+: database) | `review_state` |
//! | `review-state.json` | no release (same format; accepted) | `review_state` |
//! | `notices.json` | 0.7.0, 0.7.1 | `notices` |
//! | `update-state.json` | 0.7.0, 0.7.1 | `update_state` |
//! | `terminal/sessions.json` | 0.7.0 (0.7.1 only read it) | `terminal_sessions` |
//! | `terminal/sessions/<hash>.json` | 0.7.1 | `terminal_sessions` |
//!
//! **Algorithm, per file.** (1) If its import marker (`import:<relative path>` in
//! `store_meta`) exists, the data is already in the database: only step 4. (2) Read it
//! (regular files only: a symbolic link, or a directory on the way that is one, is never
//! followed; at most [`MAX_LEGACY_BYTES`]) and parse it with the old format's parser below.
//! A file that cannot be read or parsed is left in place untouched, logged, and does not
//! stop the others. (3) In ONE write transaction: check the marker again (another process
//! may have imported it meanwhile), write the values, write the marker, commit. Values never
//! overwrite what the database already holds (a newer process may have saved it), except
//! that files 0.4 to 0.7.1 had already copied into the database (the v0.3 stores, flag
//! `imported_v03`) are not copied again at all. (4) Rename the file to `<name>.imported`
//! (`.imported.2`, ... if taken); never delete it. A failed rename is logged and skipped:
//! the marker prevents a second import and the next start retries the rename.
//!
//! The terminal's 0.7.1 per-project files win over the 0.7.0 single file, as when 0.7.1
//! read them: the single file's entry of a project is skipped when that project has a
//! readable file of its own (imported already or not yet, e.g. after an interrupted run),
//! so an emptied project stays empty; a project whose own file is unreadable gets its entry
//! from the single file.
//!
//! Interrupted at any point the import is safe: before the commit nothing was written;
//! after it the marker is in the same transaction as the data. Nothing runs in a process
//! whose state is read-only ([`super::StoreMode::ReadOnly`]): its files stay for a process
//! that can write.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use redb::{ReadableDatabase, ReadableTable, WriteTransaction};
use serde::Deserialize;

use super::keyspace::{self, Concern, SINGLE_KEY, STORE_META};
use super::review::{review_key, ReviewMark};
use super::sessions::Session;
use super::terminal::{SavedTab, TerminalSession};
use super::{concerns, Store, StoreError, StoreResult};
use crate::db::{now_nanos, KEY_IMPORTED_V03, META, RECENT, REVIEW, SESSIONS};

/// Largest legacy file read (larger files are left in place and logged).
pub const MAX_LEGACY_BYTES: u64 = 8 * 1024 * 1024;

/// What one import run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Files whose values were written to the store in this run (relative paths).
    pub imported: Vec<String>,
    /// Files found already imported (their marker exists) in this run.
    pub already_imported: Vec<String>,
    /// Files renamed to `*.imported` in this run.
    pub renamed: Vec<String>,
    /// Files left in place, with the reason (unreadable, malformed, too large, a link...).
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Recent,
    Review,
    WindowSession,
    Notices,
    UpdateState,
    TerminalSingle,
    TerminalProject,
}

#[derive(Debug, Clone)]
struct LegacyFile {
    /// Relative path with `/` separators (the marker key).
    rel: String,
    path: PathBuf,
    kind: Kind,
}

/// A step of the per-file algorithm, for failure injection in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Reading the file fails.
    Read,
    /// The transaction is abandoned before its commit.
    Commit,
    /// The process stops right after the commit (before the rename).
    AfterCommit,
    /// The rename fails.
    Rename,
}

/// Imports every legacy file under `data_dir` (see the module doc).
pub(crate) fn import(store: &Store, data_dir: &Path) -> ImportReport {
    import_with(store, data_dir, &|_, _| false)
}

/// [`import`] with a failure hook: `fail(step, rel)` returns true to make that step fail.
pub(crate) fn import_with(
    store: &Store,
    data_dir: &Path,
    fail: &dyn Fn(Step, &str) -> bool,
) -> ImportReport {
    let mut report = ImportReport::default();
    if !matches!(store.mode(), super::StoreMode::ReadWrite) {
        return report;
    }
    for file in discover(data_dir, &mut report) {
        match import_one(store, &file, fail, &mut report) {
            Flow::Continue => {}
            Flow::Stop => break,
        }
    }
    if !report.imported.is_empty() || !report.skipped.is_empty() {
        tracing::info!(
            "imported {} state file(s) into the database; {} left in place",
            report.imported.len(),
            report.skipped.len()
        );
    }
    for (rel, why) in &report.skipped {
        tracing::warn!("state file {rel} not imported: {why}");
    }
    report
}

enum Flow {
    Continue,
    Stop,
}

/// A directory under the data dir that is a real directory (not a link).
fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

fn discover(data_dir: &Path, report: &mut ImportReport) -> Vec<LegacyFile> {
    let mut out = Vec::new();
    let mut fixed = |rel: &str, kind: Kind| {
        let path = rel
            .split('/')
            .fold(data_dir.to_path_buf(), |p, part| p.join(part));
        if std::fs::symlink_metadata(&path).is_ok() {
            out.push(LegacyFile {
                rel: rel.to_string(),
                path,
                kind,
            });
        }
    };
    fixed("recent.json", Kind::Recent);
    fixed("review.json", Kind::Review);
    fixed("review-state.json", Kind::Review);
    fixed("notices.json", Kind::Notices);
    fixed("update-state.json", Kind::UpdateState);
    let mut in_dir = |dir_rel: &str, kind: Kind, out: &mut Vec<LegacyFile>| {
        let dir = dir_rel
            .split('/')
            .fold(data_dir.to_path_buf(), |p, part| p.join(part));
        // Every directory on the way must be a real directory, not a link out of the data dir.
        let mut on_the_way = data_dir.to_path_buf();
        for part in dir_rel.split('/') {
            on_the_way.push(part);
            if std::fs::symlink_metadata(&on_the_way).is_err() {
                return;
            }
            if !real_dir(&on_the_way) {
                report.skipped.push((
                    dir_rel.to_string(),
                    "not a real directory (a link?): not followed".into(),
                ));
                return;
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut found: Vec<_> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                name.ends_with(".json").then_some((name, e.path()))
            })
            .collect();
        found.sort();
        for (name, path) in found {
            out.push(LegacyFile {
                rel: format!("{dir_rel}/{name}"),
                path,
                kind,
            });
        }
    };
    in_dir("sessions", Kind::WindowSession, &mut out);
    in_dir("terminal/sessions", Kind::TerminalProject, &mut out);
    let single = data_dir.join("terminal").join("sessions.json");
    if real_dir(&data_dir.join("terminal")) && std::fs::symlink_metadata(&single).is_ok() {
        out.push(LegacyFile {
            rel: "terminal/sessions.json".into(),
            path: single,
            kind: Kind::TerminalSingle,
        });
    }
    out
}

fn marker_key(rel: &str) -> String {
    format!("import:{rel}")
}

fn has_marker(store: &Store, rel: &str) -> StoreResult<bool> {
    let tx = store
        .db()
        .raw()
        .begin_read()
        .map_err(crate::db::DbError::from)?;
    match tx.open_table(STORE_META) {
        Ok(t) => Ok(t.get(marker_key(rel).as_str())?.is_some()),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn import_one(
    store: &Store,
    file: &LegacyFile,
    fail: &dyn Fn(Step, &str) -> bool,
    report: &mut ImportReport,
) -> Flow {
    let rel = file.rel.as_str();
    match has_marker(store, rel) {
        Ok(true) => {
            report.already_imported.push(rel.to_string());
            rename_aside(file, fail, report);
            return Flow::Continue;
        }
        Ok(false) => {}
        Err(e) => {
            report.skipped.push((rel.to_string(), e.to_string()));
            return Flow::Continue;
        }
    }
    let bytes = if fail(Step::Read, rel) {
        Err("injected read failure".to_string())
    } else {
        read_capped(&file.path)
    };
    let bytes = match bytes {
        Ok(b) => b,
        // Gone since it was found: another process imported and renamed it.
        Err(_) if std::fs::symlink_metadata(&file.path).is_err() => {
            if has_marker(store, rel).unwrap_or(false) {
                report.already_imported.push(rel.to_string());
            }
            return Flow::Continue;
        }
        Err(why) => {
            report.skipped.push((rel.to_string(), why));
            return Flow::Continue;
        }
    };
    let mut parsed = match parse(file, &bytes) {
        Ok(p) => p,
        Err(why) => {
            report.skipped.push((rel.to_string(), why));
            return Flow::Continue;
        }
    };
    if let (Kind::TerminalSingle, Parsed::Terminal(projects)) = (file.kind, &mut parsed) {
        // A project with a readable 0.7.1 file of its own takes its tabs from there (0.7.1
        // read that file first), whether it is imported already or later.
        projects.retain(|(root, _)| !has_own_terminal_file(&file.path, root));
    }
    match commit(store, rel, &parsed, fail) {
        Ok(Committed::Written) => report.imported.push(rel.to_string()),
        Ok(Committed::AlreadyThere) => report.already_imported.push(rel.to_string()),
        Err(e) => {
            report.skipped.push((rel.to_string(), e.to_string()));
            return Flow::Continue;
        }
    }
    if fail(Step::AfterCommit, rel) {
        return Flow::Stop;
    }
    rename_aside(file, fail, report);
    Flow::Continue
}

/// Reads a regular file (never through a symbolic link), at most [`MAX_LEGACY_BYTES`].
fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("a symbolic link: not followed".into());
    }
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_LEGACY_BYTES {
        return Err(format!(
            "{} bytes, more than the {MAX_LEGACY_BYTES} read",
            meta.len()
        ));
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_LEGACY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_LEGACY_BYTES {
        return Err(format!("more than the {MAX_LEGACY_BYTES} bytes read"));
    }
    Ok(bytes)
}

/// FNV-1a of `text`: the name 0.7.1 gave a project's terminal file.
fn stable_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Whether `root` has a readable 0.7.1 per-project file next to the 0.7.0 file `single`
/// (`terminal/sessions/<hash>.json`, or its `.imported` copy).
fn has_own_terminal_file(single: &Path, root: &str) -> bool {
    let Some(dir) = single.parent().map(|d| d.join("sessions")) else {
        return false;
    };
    if !real_dir(&dir) {
        return false;
    }
    let name = format!("{:016x}.json", stable_hash(root));
    [dir.join(&name), dir.join(format!("{name}.imported"))]
        .iter()
        .any(|p| {
            read_capped(p)
                .ok()
                .and_then(|b| serde_json::from_slice::<TerminalProjectFile>(&b).ok())
                .is_some_and(|f| f.root == root)
        })
}

// ---- the old formats (read-only) ------------------------------------------------------

/// `recent.json` (lector-config `RecentProjects`, 0.3).
#[derive(Deserialize)]
struct RecentFile {
    #[serde(default)]
    projects: Vec<PathBuf>,
}

/// `review.json` (lector-config `ReviewStore`, 0.3 to 0.7.1).
#[derive(Deserialize)]
struct ReviewFile {
    #[serde(default)]
    entries: Vec<ReviewEntry>,
}

#[derive(Deserialize)]
struct ReviewEntry {
    repo: String,
    changeset: String,
    path: String,
    hash: String,
    #[serde(default)]
    at: u64,
}

/// `terminal/sessions.json` (lector-ui-terminal, 0.7.0).
#[derive(Deserialize)]
struct TerminalSingleFile {
    #[serde(default)]
    projects: BTreeMap<String, Vec<SavedTab>>,
}

/// `terminal/sessions/<hash>.json` (lector-ui-terminal, 0.7.1).
#[derive(Deserialize)]
struct TerminalProjectFile {
    #[serde(default)]
    root: String,
    #[serde(default)]
    tabs: Vec<SavedTab>,
}

enum Parsed {
    Recent(Vec<PathBuf>),
    Review(Vec<ReviewEntry>),
    WindowSession {
        key: String,
        session: Box<Session>,
    },
    /// A JSON object stored as-is (its type lives in the crate that uses it).
    Single(&'static Concern, Vec<u8>),
    Terminal(Vec<(String, Vec<SavedTab>)>),
}

fn parse(file: &LegacyFile, bytes: &[u8]) -> Result<Parsed, String> {
    let malformed = |e: serde_json::Error| format!("malformed: {e}");
    Ok(match file.kind {
        Kind::Recent => Parsed::Recent(
            serde_json::from_slice::<RecentFile>(bytes)
                .map_err(malformed)?
                .projects,
        ),
        Kind::Review => Parsed::Review(
            serde_json::from_slice::<ReviewFile>(bytes)
                .map_err(malformed)?
                .entries,
        ),
        Kind::WindowSession => {
            let key = file
                .rel
                .rsplit('/')
                .next()
                .and_then(|n| n.strip_suffix(".json"))
                .unwrap_or_default()
                .to_string();
            if key.is_empty() {
                return Err("no session key in the file name".into());
            }
            let session = serde_json::from_slice::<Session>(bytes).map_err(malformed)?;
            Parsed::WindowSession {
                key,
                session: Box::new(session),
            }
        }
        Kind::Notices | Kind::UpdateState => {
            let value = serde_json::from_slice::<serde_json::Value>(bytes).map_err(malformed)?;
            if !value.is_object() {
                return Err("malformed: not a JSON object".into());
            }
            let concern = if file.kind == Kind::Notices {
                &concerns::NOTICES
            } else {
                &concerns::UPDATE_STATE
            };
            Parsed::Single(concern, serde_json::to_vec(&value).map_err(malformed)?)
        }
        Kind::TerminalSingle => Parsed::Terminal(
            serde_json::from_slice::<TerminalSingleFile>(bytes)
                .map_err(malformed)?
                .projects
                .into_iter()
                .collect(),
        ),
        Kind::TerminalProject => {
            let f = serde_json::from_slice::<TerminalProjectFile>(bytes).map_err(malformed)?;
            if f.root.is_empty() {
                return Err("malformed: no project root".into());
            }
            Parsed::Terminal(vec![(f.root, f.tabs)])
        }
    })
}

enum Committed {
    Written,
    AlreadyThere,
}

fn concern_of(parsed: &Parsed) -> &'static Concern {
    match parsed {
        Parsed::Recent(_) => &concerns::RECENT_PROJECTS,
        Parsed::Review(_) => &concerns::REVIEW_STATE,
        Parsed::WindowSession { .. } => &concerns::WINDOW_SESSIONS,
        Parsed::Single(c, _) => c,
        Parsed::Terminal(_) => &concerns::TERMINAL_SESSIONS,
    }
}

/// Step 3: values and marker in one transaction.
fn commit(
    store: &Store,
    rel: &str,
    parsed: &Parsed,
    fail: &dyn Fn(Step, &str) -> bool,
) -> StoreResult<Committed> {
    let tx = store.db().begin_write().map_err(crate::db::DbError::from)?;
    let marker = marker_key(rel);
    if tx.open_table(STORE_META)?.get(marker.as_str())?.is_some() {
        return Ok(Committed::AlreadyThere);
    }
    keyspace::prepare(&tx, concern_of(parsed))?;
    let v03_done = tx.open_table(META)?.get(KEY_IMPORTED_V03)?.is_some();
    // 0.4 to 0.7.1 copied the v0.3 stores into the database already (and the database is
    // what they used since): copying them again would bring back what the user removed.
    let already_in_db = v03_done
        && matches!(
            (rel, parsed),
            ("recent.json" | "review.json", _) | (_, Parsed::WindowSession { .. })
        );
    if !already_in_db {
        write_values(&tx, parsed)?;
    }
    tx.open_table(STORE_META)?.insert(marker.as_str(), 1)?;
    if fail(Step::Commit, rel) {
        // Dropping the transaction abandons it.
        return Err(StoreError::Db(crate::db::DbError::Redb(
            "injected failure before the commit".into(),
        )));
    }
    tx.commit().map_err(crate::db::DbError::from)?;
    Ok(Committed::Written)
}

fn write_values(tx: &WriteTransaction, parsed: &Parsed) -> StoreResult<()> {
    match parsed {
        Parsed::Recent(projects) => {
            let mut t = tx.open_table(RECENT)?;
            // Newest first in the file: older entries get older stamps.
            let base = now_nanos().saturating_sub(1_000_000_000);
            for (i, p) in projects.iter().enumerate() {
                let key = p.to_string_lossy();
                if t.get(key.as_ref())?.is_none() {
                    t.insert(key.as_ref(), base.saturating_sub(i as u64))?;
                }
            }
        }
        Parsed::Review(entries) => {
            let mut t = tx.open_table(REVIEW)?;
            for e in entries {
                let key = review_key(&e.repo, &e.changeset, &e.path);
                if t.get(key.as_str())?.is_none() {
                    let v = serde_json::to_vec(&ReviewMark {
                        hash: e.hash.clone(),
                        at: e.at,
                    })
                    .map_err(|source| StoreError::Value {
                        concern: concerns::REVIEW_STATE.name,
                        source,
                    })?;
                    t.insert(key.as_str(), v.as_slice())?;
                }
            }
        }
        Parsed::WindowSession { key, session } => {
            let mut t = tx.open_table(SESSIONS)?;
            if t.get(key.as_str())?.is_none() {
                let v =
                    serde_json::to_vec(session.as_ref()).map_err(|source| StoreError::Value {
                        concern: concerns::WINDOW_SESSIONS.name,
                        source,
                    })?;
                t.insert(key.as_str(), v.as_slice())?;
            }
        }
        Parsed::Single(concern, bytes) => {
            let mut t = tx.open_table(concern.table())?;
            if t.get(SINGLE_KEY)?.is_none() {
                t.insert(SINGLE_KEY, bytes.as_slice())?;
            }
        }
        Parsed::Terminal(projects) => {
            let mut t = tx.open_table(concerns::TERMINAL_SESSIONS.table())?;
            for (root, tabs) in projects {
                if t.get(root.as_str())?.is_none() {
                    let v = serde_json::to_vec(&TerminalSession { tabs: tabs.clone() }).map_err(
                        |source| StoreError::Value {
                            concern: concerns::TERMINAL_SESSIONS.name,
                            source,
                        },
                    )?;
                    t.insert(root.as_str(), v.as_slice())?;
                }
            }
        }
    }
    Ok(())
}

/// Step 4: `<name>.imported` (or `.imported.2`, ...). Never deletes; a failure is logged.
fn rename_aside(file: &LegacyFile, fail: &dyn Fn(Step, &str) -> bool, report: &mut ImportReport) {
    // Never rename through a link (the link itself would be renamed, which is harmless, but
    // a link was not imported either: leave it as it is).
    match std::fs::symlink_metadata(&file.path) {
        Ok(m) if m.is_file() => {}
        _ => return,
    }
    if fail(Step::Rename, &file.rel) {
        tracing::warn!("cannot rename {} (injected failure)", file.path.display());
        return;
    }
    let mut base = file.path.as_os_str().to_os_string();
    base.push(".imported");
    let mut target = PathBuf::from(&base);
    let mut n = 2;
    while std::fs::symlink_metadata(&target).is_ok() {
        if n > 100 {
            tracing::warn!(
                "cannot rename {}: too many earlier copies",
                file.path.display()
            );
            return;
        }
        let mut name = base.clone();
        name.push(format!(".{n}"));
        target = PathBuf::from(name);
        n += 1;
    }
    match std::fs::rename(&file.path, &target) {
        Ok(()) => report.renamed.push(file.rel.clone()),
        // Another process renamed it first.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            "imported {} but cannot rename it to {}: {e} (it will not be imported again)",
            file.path.display(),
            target.display()
        ),
    }
}
