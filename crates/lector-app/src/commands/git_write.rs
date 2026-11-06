//! Git write commands (v0.4): stage / unstage / discard (paths, hunks, selected lines,
//! all), commit, merge-conflict resolution, continue / abort an operation.
//!
//! The rule: **undo instead of confirm.** Before a command destroys anything it records
//! what will be lost in the shadow repository (`lector_git::ShadowRepo`, Lector's own
//! data dir, never the user's `.git`) and returns an [`UndoRecord`]; the journal turns
//! that into an Undo button. Staging is reversible by its inverse command or an index
//! snapshot; commits are journaled but not undoable; aborting an operation is
//! irreversible and stays behind a confirmation in the UI (`destructive`).
//!
//! If the backup cannot be taken, destructive commands fail with [`BackupUnavailable`]
//! instead of discarding without a net; the UI then asks for confirmation and retries
//! with `allow_without_backup: true`. [`backup_available`] lets it ask beforehand.
//!
//! No command sets `ui_action`: the git views in `lector-ui-git` own the GPUI `git::*` actions (they
//! need the selection, the commit message, toasts with Undo) and call these commands through
//! `staging::run_op`; the generic action bridge must not register a second handler.
//!
//! Path arguments are repository-relative (`/`-separated) or absolute inside the work tree.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _};
use lector_diff::Algorithm;
use lector_git::{
    Changeset, CommitOptions, OperationKind, Repo, ShadowRepo, ShadowSnapshot, StatusEntry,
};
use lector_workspace::conflicts::{self, Resolution};
use lector_workspace::review::{self, HunkAction, LineSelection};
use serde_json::{json, Value};

use super::schema::{object, prop};
use super::{
    Availability, CommandContext, CommandRegistry, CommandSpec, FnCommand, Outcome, UiRequest,
};
use crate::core::AppCore;
use crate::journal::{Journal, UndoRecord};

/// The backup that makes a destructive command undoable could not be taken. Downcast the
/// command's error to it: ask the user to confirm, then retry with
/// `allow_without_backup: true`.
#[derive(Debug, thiserror::Error)]
#[error("Lector cannot back up your changes first: {0}")]
pub struct BackupUnavailable(pub String);

/// Registers the git write commands.
pub fn register(r: &CommandRegistry) {
    r.register(stage());
    r.register(unstage());
    r.register(discard());
    r.register(stage_hunk());
    r.register(unstage_hunk());
    r.register(discard_hunk());
    r.register(stage_all());
    r.register(unstage_all());
    r.register(discard_all());
    r.register(commit());
    r.register(accept_file());
    r.register(mark_resolved());
    r.register(resolve_delete());
    r.register(continue_operation());
    r.register(abort_operation());
}

/// Whether destructive commands can back up before writing in `repo` right now.
/// `Err` carries the reason (show it in the confirmation that replaces the undo toast).
pub fn backup_available(core: &AppCore, repo: &Repo) -> Result<(), String> {
    ShadowRepo::open(&data_dir(core), repo)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// How long backups are kept (the journal keeps entries longer; their Undo then reads
/// "the backup was cleaned up").
const BACKUP_RETENTION: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Opens the shadow repository of `repo`; the first time per process it also trims it
/// ([`BACKUP_RETENTION`], [`lector_git::shadow::DEFAULT_MAX_BYTES`]) so backups never grow without bound.
fn open_shadow(core: &AppCore, repo: &Repo) -> Result<ShadowRepo, lector_git::Error> {
    static TRIMMED: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    let shadow = ShadowRepo::open(&data_dir(core), repo)?;
    let first = TRIMMED
        .get_or_init(Default::default)
        .lock()
        .insert(shadow.git_dir().to_path_buf());
    if first {
        if let Err(err) = shadow.prune(BACKUP_RETENTION) {
            tracing::warn!("cannot trim the shadow repository: {err}");
        }
    }
    Ok(shadow)
}

fn data_dir(core: &AppCore) -> PathBuf {
    Journal::of(core)
        .map(|j| j.data_dir().to_path_buf())
        .unwrap_or_else(lector_config::paths::data_dir)
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

fn needs_repo(cx: &CommandContext) -> Availability {
    match &cx.project {
        None => Availability::no("No folder is open"),
        Some(p) if p.repo().is_none() => Availability::no("Not a git repository"),
        Some(p) if p.repo().is_some_and(Repo::is_bare) => {
            Availability::no("The repository is bare")
        }
        Some(_) => Availability::Available,
    }
}

fn paths_schema(desc: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": desc })
}

fn bool_prop(desc: &str) -> Value {
    prop("boolean", desc)
}

fn allow_prop() -> Value {
    bool_prop("Proceed even if the backup that makes this undoable cannot be taken (only after the user confirmed)")
}

/// Repo-relative path of an argument (absolute paths must lie inside the work tree).
fn rel_path(repo: &Repo, given: &str) -> anyhow::Result<String> {
    let path = Path::new(given);
    let rel = if path.is_absolute() {
        let workdir = repo.workdir();
        let stripped = path
            .strip_prefix(workdir)
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| {
                let canonical = std::fs::canonicalize(path).ok()?;
                canonical.strip_prefix(workdir).ok().map(Path::to_path_buf)
            });
        let stripped = stripped.ok_or_else(|| anyhow!("{given} is outside the repository"))?;
        stripped
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    } else {
        given.replace('\\', "/").trim_start_matches("./").to_owned()
    };
    if rel.is_empty() {
        bail!("empty path");
    }
    Ok(rel)
}

fn path_args(cx: &CommandContext, repo: &Repo, args: &Value) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    match args.get("paths").and_then(Value::as_array) {
        Some(list) => {
            for item in list {
                let s = item
                    .as_str()
                    .ok_or_else(|| anyhow!("`paths` must be strings"))?;
                let rel = rel_path(repo, s)?;
                if !out.contains(&rel) {
                    out.push(rel);
                }
            }
        }
        None => {
            let focus = cx
                .focus
                .path
                .as_ref()
                .ok_or_else(|| anyhow!("no `paths` given and no file is active"))?;
            out.push(rel_path(repo, &focus.to_string_lossy())?);
        }
    }
    if out.is_empty() {
        bail!("no paths given");
    }
    Ok(out)
}

fn one_path(cx: &CommandContext, repo: &Repo, args: &Value) -> anyhow::Result<String> {
    match args.get("path").and_then(Value::as_str) {
        Some(p) => rel_path(repo, p),
        None => {
            let focus = cx
                .focus
                .path
                .as_ref()
                .ok_or_else(|| anyhow!("no `path` given and no file is active"))?;
            rel_path(repo, &focus.to_string_lossy())
        }
    }
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn abs_paths(repo: &Repo, rels: &[String]) -> Vec<PathBuf> {
    rels.iter().map(|r| repo.workdir().join(r)).collect()
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// "parser.rs" for one path, "3 files" for several.
fn files_phrase(rels: &[String]) -> String {
    match rels {
        [one] => file_name(one).to_owned(),
        many => plural(many.len(), "file", "files"),
    }
}

/// A backup taken before a write.
struct Backup {
    shadow: ShadowRepo,
    snap: ShadowSnapshot,
}

/// Backs up the working-tree contents of `rels` before a destructive write. Without a
/// backup: `Err(BackupUnavailable)`, or `Ok(None)` when `allow_without` says the user
/// accepted it.
fn backup_files(
    cx: &CommandContext,
    repo: &Repo,
    rels: &[String],
    note: &str,
    allow_without: bool,
) -> anyhow::Result<Option<Backup>> {
    let taken = (|| -> Result<Backup, lector_git::Error> {
        let shadow = open_shadow(&cx.core, repo)?;
        let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let snap = shadow.snapshot_paths(&refs, note)?;
        Ok(Backup { shadow, snap })
    })();
    match taken {
        Ok(b) => Ok(Some(b)),
        Err(err) if allow_without => {
            tracing::warn!("proceeding without a backup: {err}");
            Ok(None)
        }
        Err(err) => Err(BackupUnavailable(err.to_string()).into()),
    }
}

/// Best-effort index backup (staging is not destructive; no backup = no undo record).
fn backup_index(cx: &CommandContext, repo: &Repo, rels: &[String], note: &str) -> Option<Backup> {
    let shadow = open_shadow(&cx.core, repo).ok()?;
    let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
    let snap = shadow.snapshot_index(&refs, note).ok()?;
    Some(Backup { shadow, snap })
}

fn files_undo(backup: Option<Backup>, rels: &[String]) -> Option<UndoRecord> {
    let b = backup?;
    let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
    Some(UndoRecord::RestoreFiles {
        after: b.shadow.fingerprints(&refs).unwrap_or_default(),
        shadow: b.snap,
        paths: rels.to_vec(),
    })
}

fn index_undo(backup: Option<Backup>, rels: &[String]) -> Option<UndoRecord> {
    let b = backup?;
    let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
    Some(UndoRecord::RestoreIndex {
        after: b.shadow.index_fingerprints(&refs).unwrap_or_default(),
        shadow: b.snap,
        paths: rels.to_vec(),
    })
}

fn status_of(entries: &[StatusEntry], rel: &str) -> Option<StatusEntry> {
    entries.iter().find(|e| e.path == rel).cloned()
}

fn any_below(entries: &[StatusEntry], rel: &str) -> bool {
    let prefix = format!("{rel}/");
    entries.iter().any(|e| e.path.starts_with(&prefix))
}

fn finish(outcome: Outcome, undo: Option<UndoRecord>, repo: &Repo, rels: &[String]) -> Outcome {
    let outcome = outcome
        .with_paths(abs_paths(repo, rels))
        .with_ui(UiRequest::Refresh);
    match undo {
        Some(u) => outcome.with_undo(u),
        None => outcome,
    }
}

// ---------------------------------------------------------------------------------------------
// Stage / unstage
// ---------------------------------------------------------------------------------------------

fn stage() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.stage",
            "Stage Changes",
            "Git",
            object(
                &[(
                    "paths",
                    paths_schema("Files to stage (default: the active file)"),
                )],
                &[],
            ),
        )
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let rels = path_args(cx, repo, args)?;
            let status = repo.status()?;
            // The inverse is `unstage` only if nothing was staged for these paths before (and none
            // is conflicted: unstaging would drop its conflict stages).
            let simple = rels.iter().all(|p| match status_of(&status, p) {
                Some(e) => e.staged.is_none() && e.renamed_from.is_none() && !e.conflicted,
                None => !any_below(&status, p),
            });
            let backup = if simple {
                None
            } else {
                backup_index(
                    cx,
                    repo,
                    &rels,
                    &format!("Before staging {}", files_phrase(&rels)),
                )
            };
            let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
            repo.stage_paths(&refs)?;
            let undo = if simple {
                Some(UndoRecord::Invoke {
                    command: "git.unstage".into(),
                    args: json!({ "paths": rels }),
                })
            } else {
                index_undo(backup, &rels)
            };
            let summary = format!("Staged {}", files_phrase(&rels));
            Ok(finish(Outcome::new(summary), undo, repo, &rels))
        },
    )
    .with_available(needs_repo)
}

fn unstage() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.unstage",
            "Unstage Changes",
            "Git",
            object(
                &[(
                    "paths",
                    paths_schema("Files to unstage (default: the active file)"),
                )],
                &[],
            ),
        )
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let mut rels = path_args(cx, repo, args)?;
            let status = repo.status()?;
            // A staged rename needs both of its paths.
            for p in rels.clone() {
                if let Some(old) = status_of(&status, &p).and_then(|e| e.renamed_from) {
                    if !rels.contains(&old) {
                        rels.push(old);
                    }
                }
            }
            // The inverse is `stage` only when the index equals the working tree afterwards
            // would be restored: no unstaged edits on top, no renames.
            let simple = rels.iter().all(|p| match status_of(&status, p) {
                Some(e) => e.unstaged.is_none() && !e.untracked && e.renamed_from.is_none(),
                None => !any_below(&status, p),
            }) && !status
                .iter()
                .any(|e| e.renamed_from.is_some() && rels.contains(&e.path));
            let backup = if simple {
                None
            } else {
                backup_index(
                    cx,
                    repo,
                    &rels,
                    &format!("Before unstaging {}", files_phrase(&rels)),
                )
            };
            let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
            repo.unstage_paths(&refs)?;
            let undo = if simple {
                Some(UndoRecord::Invoke {
                    command: "git.stage".into(),
                    args: json!({ "paths": rels }),
                })
            } else {
                index_undo(backup, &rels)
            };
            let summary = format!("Unstaged {}", files_phrase(&rels));
            Ok(finish(Outcome::new(summary), undo, repo, &rels))
        },
    )
    .with_available(needs_repo)
}

fn all_paths_for_stage(status: &[StatusEntry]) -> Vec<String> {
    status
        .iter()
        .filter(|e| e.unstaged.is_some() || e.untracked || e.conflicted)
        .map(|e| e.path.clone())
        .collect()
}

fn stage_all() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.stage_all",
            "Stage All Changes",
            "Git",
            object(&[], &[]),
        )
        .undoable(),
        |cx, _args| {
            let repo = cx.require_repo()?;
            let rels = all_paths_for_stage(&repo.status()?);
            if rels.is_empty() {
                return Ok(Outcome::new("Nothing to stage"));
            }
            let backup = backup_index(
                cx,
                repo,
                &rels,
                &format!("Before staging {}", files_phrase(&rels)),
            );
            repo.stage_all()?;
            let summary = format!(
                "Staged all changes ({})",
                plural(rels.len(), "file", "files")
            );
            Ok(finish(
                Outcome::new(summary),
                index_undo(backup, &rels),
                repo,
                &rels,
            ))
        },
    )
    .with_available(needs_repo)
}

fn unstage_all() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.unstage_all",
            "Unstage All Changes",
            "Git",
            object(&[], &[]),
        )
        .undoable(),
        |cx, _args| {
            let repo = cx.require_repo()?;
            let status = repo.status()?;
            let mut rels: Vec<String> = Vec::new();
            for e in status.iter().filter(|e| e.staged.is_some()) {
                rels.push(e.path.clone());
                if let Some(old) = &e.renamed_from {
                    rels.push(old.clone());
                }
            }
            if rels.is_empty() {
                return Ok(Outcome::new("Nothing to unstage"));
            }
            let backup = backup_index(
                cx,
                repo,
                &rels,
                &format!("Before unstaging {}", files_phrase(&rels)),
            );
            repo.unstage_all()?;
            let summary = format!(
                "Unstaged all changes ({})",
                plural(rels.len(), "file", "files")
            );
            Ok(finish(
                Outcome::new(summary),
                index_undo(backup, &rels),
                repo,
                &rels,
            ))
        },
    )
    .with_available(needs_repo)
}

// ---------------------------------------------------------------------------------------------
// Discard
// ---------------------------------------------------------------------------------------------

fn describe_discard(status: &[StatusEntry], rels: &[String]) -> String {
    let untracked = rels
        .iter()
        .filter(|p| status_of(status, p).is_some_and(|e| e.untracked))
        .count();
    match rels {
        [one] if untracked == 1 => format!("Deleted untracked {}", file_name(one)),
        [one] => format!("Discarded changes in {}", file_name(one)),
        many if untracked == many.len() => {
            format!("Deleted {} untracked", plural(many.len(), "file", "files"))
        }
        many if untracked > 0 => format!(
            "Discarded changes in {} (deleted {} untracked)",
            plural(many.len(), "file", "files"),
            untracked
        ),
        many => format!(
            "Discarded changes in {}",
            plural(many.len(), "file", "files")
        ),
    }
}

fn discard_impl(
    cx: &CommandContext,
    repo: &Repo,
    rels: Vec<String>,
    allow_without: bool,
) -> anyhow::Result<Outcome> {
    let status = repo.status()?;
    let summary = describe_discard(&status, &rels);
    let backup = backup_files(
        cx,
        repo,
        &rels,
        &format!("Before: {summary}"),
        allow_without,
    )?;
    let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
    repo.discard_paths(&refs)?;
    let note = if backup.is_none() {
        " (no backup: cannot be undone)"
    } else {
        ""
    };
    let undo = files_undo(backup, &rels);
    Ok(finish(
        Outcome::new(format!("{summary}{note}")),
        undo,
        repo,
        &rels,
    ))
}

fn discard() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.discard",
            "Discard Changes",
            "Git",
            object(
                &[
                    ("paths", paths_schema("Files to discard (default: the active file). Tracked files return to the index; untracked files are deleted")),
                    ("allow_without_backup", allow_prop()),
                ],
                &[],
            ),
        )
        .destructive()
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let rels = path_args(cx, repo, args)?;
            discard_impl(cx, repo, rels, flag(args, "allow_without_backup"))
        },
    )
    .with_available(needs_repo)
}

fn discard_all() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.discard_all",
            "Discard All Changes",
            "Git",
            object(
                &[
                    ("scope", json!({"type": "string", "enum": ["unstaged", "untracked", "all"], "description": "What to discard (default unstaged: tracked files' working-tree changes)"})),
                    ("allow_without_backup", allow_prop()),
                ],
                &[],
            ),
        )
        .destructive()
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let scope = args.get("scope").and_then(Value::as_str).unwrap_or("unstaged");
            let rels: Vec<String> = repo
                .status()?
                .iter()
                .filter(|e| match scope {
                    "untracked" => e.untracked,
                    "all" => e.untracked || e.unstaged.is_some(),
                    _ => e.unstaged.is_some(),
                })
                .map(|e| e.path.clone())
                .collect();
            if rels.is_empty() {
                return Ok(Outcome::new("Nothing to discard"));
            }
            discard_impl(cx, repo, rels, flag(args, "allow_without_backup"))
        },
    )
    .with_available(needs_repo)
}

// ---------------------------------------------------------------------------------------------
// Hunks and selected lines
// ---------------------------------------------------------------------------------------------

fn hunk_schema() -> Value {
    object(
        &[
            ("path", prop("string", "File (repository-relative)")),
            ("changeset", prop("string", "unstaged (stage / discard) or staged (unstage); default by action")),
            ("hunk", json!({"type": "integer", "minimum": 0, "description": "Index of the hunk in the file's diff"})),
            ("old_lines", json!({"type": "array", "items": {"type": "array", "items": {"type": "integer", "minimum": 0}}, "description": "Selected rows of the old side as [start, end) pairs (0-based)"})),
            ("new_lines", json!({"type": "array", "items": {"type": "array", "items": {"type": "integer", "minimum": 0}}, "description": "Selected rows of the new side as [start, end) pairs (0-based)"})),
            ("content_id", prop("string", "FileDiff content id of the diff the rows were selected in; the command refuses when the file changed since")),
            ("allow_without_backup", allow_prop()),
        ],
        &["path"],
    )
}

fn ranges(args: &Value, key: &str) -> anyhow::Result<Vec<std::ops::Range<u32>>> {
    let mut out = Vec::new();
    if let Some(list) = args.get(key).and_then(Value::as_array) {
        for pair in list {
            let pair = pair.as_array().filter(|p| p.len() == 2);
            let (Some(a), Some(b)) = (
                pair.and_then(|p| p[0].as_u64()),
                pair.and_then(|p| p[1].as_u64()),
            ) else {
                bail!("`{key}` must be [start, end) pairs");
            };
            out.push(a as u32..b as u32);
        }
    }
    Ok(out)
}

fn apply_hunk(cx: &CommandContext, args: &Value, action: HunkAction) -> anyhow::Result<Outcome> {
    let repo = cx.require_repo()?;
    let project = cx.require_project()?;
    let rel = one_path(cx, repo, args)?;
    let changeset = match args.get("changeset").and_then(Value::as_str) {
        Some(key) => {
            review::parse_changeset_key(key).ok_or_else(|| anyhow!("unknown changeset `{key}`"))?
        }
        None => match action {
            HunkAction::Unstage => Changeset::Staged,
            _ => Changeset::Unstaged,
        },
    };
    if !HunkAction::available_for(&changeset).contains(&action) {
        bail!(
            "this action does not apply to the `{}` changeset",
            review::changeset_key(&changeset)
        );
    }
    let change = repo
        .changes(&changeset)?
        .into_iter()
        .find(|c| {
            c.new_path.as_deref() == Some(rel.as_str())
                || c.old_path.as_deref() == Some(rel.as_str())
        })
        .ok_or_else(|| anyhow!("{rel} has no {} changes", review::changeset_key(&changeset)))?;

    let (old, new) = (ranges(args, "old_lines")?, ranges(args, "new_lines")?);
    let given_id = args
        .get("content_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (selection, content_id, what) = if !old.is_empty() || !new.is_empty() {
        let lines: usize = old.iter().chain(&new).map(|r| r.len()).sum();
        let id = match given_id {
            Some(id) => id,
            None => load_diff(project, &change)?.content_id().to_owned(),
        };
        (
            LineSelection { old, new },
            id,
            plural(lines, "line", "lines"),
        )
    } else if let Some(ix) = args.get("hunk").and_then(Value::as_u64) {
        let diff = load_diff(project, &change)?;
        let selection = diff
            .hunk_selection(ix as usize)
            .ok_or_else(|| anyhow!("{rel} has no hunk {ix}"))?;
        let id = given_id.unwrap_or_else(|| diff.content_id().to_owned());
        (selection, id, "hunk".to_owned())
    } else {
        bail!("give `hunk` or `old_lines` / `new_lines`");
    };

    let verb_past = match action {
        HunkAction::Stage => "Staged",
        HunkAction::Unstage => "Unstaged",
        HunkAction::Discard => "Discarded",
    };
    let summary = format!("{verb_past} {what} in {}", file_name(&rel));
    let rels = vec![rel.clone()];
    let backup = match action {
        HunkAction::Discard => backup_files(
            cx,
            repo,
            &rels,
            &format!("Before: {summary}"),
            flag(args, "allow_without_backup"),
        )?,
        _ => backup_index(cx, repo, &rels, &format!("Before: {summary}")),
    };
    let applied = review::apply_hunk_action_if_unchanged(
        repo,
        &changeset,
        &change,
        action,
        &selection,
        Algorithm::Histogram,
        &content_id,
    )
    .map_err(|e| match e {
        lector_workspace::Error::Git(lector_git::Error::GitCommand { stderr, .. })
            if !stderr.is_empty() =>
        {
            anyhow!(stderr)
        }
        other => anyhow!(other.to_string()),
    })?;
    if !applied {
        return Ok(Outcome::new(format!(
            "Nothing to apply in {}",
            file_name(&rel)
        )));
    }
    let undo = match action {
        HunkAction::Discard => files_undo(backup, &rels),
        _ => index_undo(backup, &rels),
    };
    let mut paths = vec![rel];
    paths.dedup();
    Ok(finish(Outcome::new(summary), undo, repo, &paths))
}

fn load_diff(
    project: &lector_workspace::Project,
    change: &lector_git::FileChange,
) -> anyhow::Result<review::FileDiff> {
    let opts = review::diff_options(&project.settings().diff);
    review::load_file_diff(project, change, &opts).map_err(|e| anyhow!(e.to_string()))
}

fn stage_hunk() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.stage_hunk",
            "Stage Hunk or Selected Lines",
            "Git",
            hunk_schema(),
        )
        .undoable(),
        |cx, args| apply_hunk(cx, args, HunkAction::Stage),
    )
    .with_available(needs_repo)
}

fn unstage_hunk() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.unstage_hunk",
            "Unstage Hunk or Selected Lines",
            "Git",
            hunk_schema(),
        )
        .undoable(),
        |cx, args| apply_hunk(cx, args, HunkAction::Unstage),
    )
    .with_available(needs_repo)
}

fn discard_hunk() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.discard_hunk",
            "Discard Hunk or Selected Lines",
            "Git",
            hunk_schema(),
        )
        .destructive()
        .undoable(),
        |cx, args| apply_hunk(cx, args, HunkAction::Discard),
    )
    .with_available(needs_repo)
}

// ---------------------------------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------------------------------

fn commit() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.commit",
            "Commit",
            "Git",
            object(
                &[
                    ("message", prop("string", "Commit message (subject, blank line, body)")),
                    ("amend", bool_prop("Replace the tip commit")),
                    ("sign_off", bool_prop("Add a Signed-off-by trailer")),
                ],
                &["message"],
            ),
        ),
        |cx, args| {
            let repo = cx.require_repo()?;
            let message = args.get("message").and_then(Value::as_str).unwrap_or_default();
            if message.trim().is_empty() {
                bail!("the commit message is empty");
            }
            let oid = repo.create_commit(&CommitOptions {
                message: message.to_owned(),
                amend: flag(args, "amend"),
                sign_off: flag(args, "sign_off"),
            })?;
            let subject = message.lines().next().unwrap_or("").trim();
            let verb = if flag(args, "amend") { "Amended" } else { "Committed" };
            Ok(Outcome::new(format!(
                "{verb} {} {subject} (commits cannot be undone here; `git reset --soft HEAD~1` takes it back)",
                oid.short()
            ))
            .with_data(json!({ "oid": oid.to_string(), "subject": subject }))
            .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

// ---------------------------------------------------------------------------------------------
// Merge conflicts
// ---------------------------------------------------------------------------------------------

fn accept_file() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.accept_file",
            "Accept All Current / Incoming Changes",
            "Git",
            object(
                &[
                    ("path", prop("string", "Conflicted file (default: the active file)")),
                    ("side", json!({"type": "string", "enum": ["ours", "theirs"], "description": "ours = current, theirs = incoming"})),
                    ("allow_without_backup", allow_prop()),
                ],
                &["side"],
            ),
        )
        .destructive()
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let rel = one_path(cx, repo, args)?;
            let (resolution, label) = match args.get("side").and_then(Value::as_str) {
                Some("ours") => (Resolution::Ours, "current"),
                Some("theirs") => (Resolution::Theirs, "incoming"),
                _ => bail!("`side` must be `ours` or `theirs`"),
            };
            let abs = repo.workdir().join(&rel);
            let bytes = std::fs::read(&abs).with_context(|| format!("cannot read {rel}"))?;
            let text = String::from_utf8(bytes).map_err(|_| anyhow!("{rel} is not UTF-8 text"))?;
            let parse = conflicts::parse_conflicts(&text);
            if parse.regions.is_empty() {
                bail!("{rel} has no conflict markers");
            }
            let rels = vec![rel.clone()];
            let summary = format!("Accepted all {label} changes in {}", file_name(&rel));
            let backup = backup_files(
                cx,
                repo,
                &rels,
                &format!("Before: {summary}"),
                flag(args, "allow_without_backup"),
            )?;
            let edits = conflicts::resolve_all(&text, &parse.regions, resolution);
            let resolved = conflicts::apply_edits(&text, &edits);
            lector_fs::write_atomic(&abs, resolved.as_bytes())
                .with_context(|| format!("cannot write {rel}"))?;
            Ok(finish(Outcome::new(summary), files_undo(backup, &rels), repo, &rels))
        },
    )
    .with_available(needs_repo)
}

fn mark_resolved() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.mark_resolved",
            "Mark Resolved",
            "Git",
            object(
                &[
                    (
                        "path",
                        prop("string", "Conflicted file (default: the active file)"),
                    ),
                    (
                        "allow_markers",
                        bool_prop("Stage even though conflict markers remain in the file"),
                    ),
                ],
                &[],
            ),
        )
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let rel = one_path(cx, repo, args)?;
            if !flag(args, "allow_markers") {
                if let Ok(text) = std::fs::read_to_string(repo.workdir().join(&rel)) {
                    if conflicts::has_conflict_markers(&text) {
                        bail!("{rel} still contains conflict markers");
                    }
                }
            }
            let rels = vec![rel.clone()];
            let backup = backup_index(
                cx,
                repo,
                &rels,
                &format!("Before marking {} resolved", file_name(&rel)),
            );
            repo.stage_paths(&[rel.as_str()])?;
            let summary = format!("Marked {} resolved", file_name(&rel));
            Ok(finish(
                Outcome::new(summary),
                index_undo(backup, &rels),
                repo,
                &rels,
            ))
        },
    )
    .with_available(needs_repo)
}

fn resolve_delete() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.resolve_delete",
            "Resolve by Deleting File",
            "Git",
            object(
                &[
                    (
                        "path",
                        prop("string", "Conflicted file (modify/delete conflict)"),
                    ),
                    ("allow_without_backup", allow_prop()),
                ],
                &["path"],
            ),
        )
        .destructive()
        .undoable(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let rel = one_path(cx, repo, args)?;
            let rels = vec![rel.clone()];
            let summary = format!("Resolved {} by deleting it", file_name(&rel));
            let files = backup_files(
                cx,
                repo,
                &rels,
                &format!("Before: {summary}"),
                flag(args, "allow_without_backup"),
            )?;
            let index = backup_index(cx, repo, &rels, &format!("Before: {summary}"));
            let abs = repo.workdir().join(&rel);
            match std::fs::remove_file(&abs) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(anyhow!("cannot delete {rel}: {e}")),
            }
            repo.stage_paths(&[rel.as_str()])?;
            let mut records: Vec<UndoRecord> = Vec::new();
            records.extend(files_undo(files, &rels));
            records.extend(index_undo(index, &rels));
            let undo = match records.len() {
                0 => None,
                1 => records.pop(),
                _ => Some(UndoRecord::Multi { records }),
            };
            Ok(finish(Outcome::new(summary), undo, repo, &rels))
        },
    )
    .with_available(needs_repo)
}

fn operation_kind(repo: &Repo, args: &Value) -> anyhow::Result<OperationKind> {
    let current = repo.operation()?;
    match args.get("kind").and_then(Value::as_str) {
        Some(name) => [
            OperationKind::Merge,
            OperationKind::Rebase,
            OperationKind::CherryPick,
            OperationKind::Revert,
            OperationKind::Am,
        ]
        .into_iter()
        .find(|k| k.command() == name)
        .ok_or_else(|| anyhow!("unknown operation `{name}`")),
        None => current
            .map(|o| o.kind)
            .ok_or_else(|| anyhow!("no merge, rebase, cherry-pick, revert or am is in progress")),
    }
}

fn kind_schema() -> Value {
    json!({"type": "string", "enum": ["merge", "rebase", "cherry-pick", "revert", "am"], "description": "Default: the operation in progress"})
}

fn continue_operation() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.continue_operation",
            "Continue Merge / Rebase",
            "Git",
            object(&[("kind", kind_schema())], &[]),
        ),
        |cx, args| {
            let repo = cx.require_repo()?;
            let kind = operation_kind(repo, args)?;
            repo.continue_operation(kind)?;
            Ok(Outcome::new(format!("Continued the {}", kind.command()))
                .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}

fn abort_operation() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "git.abort_operation",
            "Abort Merge / Rebase",
            "Git",
            object(&[("kind", kind_schema())], &[]),
        )
        .destructive(),
        |cx, args| {
            let repo = cx.require_repo()?;
            let kind = operation_kind(repo, args)?;
            repo.abort_operation(kind)?;
            Ok(Outcome::new(format!(
                "Aborted the {} (conflict resolutions were discarded; this cannot be undone)",
                kind.command()
            ))
            .with_ui(UiRequest::Refresh))
        },
    )
    .with_available(needs_repo)
}
