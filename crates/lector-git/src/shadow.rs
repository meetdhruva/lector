//! The shadow repository (v0.4): Lector's private safety net for destructive writes.
//!
//! Before Lector throws something away (discard, accept-all, revert, ...) it records
//! the affected files in a **bare repository in its own data directory**
//! (`<data_dir>/shadow/<hash of canonical workdir>.git`), never in the user's `.git`.
//! The shadow's `objects/info/alternates` points at the user repository's object
//! directory, so existing blobs are shared instead of copied, and a restore brings the
//! exact bytes back.
//!
//! Guarantees (all enforced by `tests/shadow.rs`, which compares the user's `.git`
//! byte-for-byte, mtimes included, around every operation):
//! * Nothing here writes into the user's git directory. All object writes go through
//!   `git fast-import` into the shadow (it never "freshens" objects found through the
//!   alternates, unlike `hash-object -w` / `write-tree`, which would touch their mtimes).
//!   Reads of the user's repository use `GIT_OPTIONAL_LOCKS=0`.
//! * The only operations that write at all outside the shadow are the two explicit
//!   restores: [`ShadowRepo::restore_paths`] writes *working-tree files* and
//!   [`ShadowRepo::restore_index`] rewrites *index entries* (`git update-index`); both are
//!   called only from an undo the user asked for.
//! * Snapshot contents are byte-exact: no filters, no CRLF conversion, symlinks are
//!   recorded as links, the executable bit is kept.
//!
//! Layout of a snapshot commit (no parents, one ref `refs/lector/snapshots/<secs>-<nonce>`
//! each): `files/<path>` = recorded working-tree contents, `blobs/<oid>` = blobs the
//! recorded index entries point at (keeps them reachable) and `manifest` = what the
//! snapshot covers (absent paths, index entries). A path that is *absent* in the
//! snapshot is restored by deleting it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::{ChangeKind, FileChange, Oid, Repo, Side};

/// Default size cap of one shadow repository for [`ShadowRepo::prune`].
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// Files larger than this are skipped (and recorded as skipped) by
/// [`ShadowRepo::snapshot_worktree`]; explicit [`ShadowRepo::snapshot_paths`] never skips.
const WORKTREE_MAX_FILE: u64 = 256 * 1024 * 1024;
/// Files up to this size are buffered; larger ones are streamed to `fast-import`.
const BUFFER_LIMIT: u64 = 64 * 1024 * 1024;
const REF_PREFIX: &str = "refs/lector/snapshots/";
const MANIFEST_MAGIC: &str = "lector-shadow";
const PACK_CONSOLIDATE_AT: usize = 40;

/// A recorded state in the shadow repository. Plain data: store it in the activity
/// journal and hand it back to [`ShadowRepo`] to restore from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ShadowSnapshot {
    /// The snapshot commit in the shadow repository.
    pub commit: Oid,
    /// Unix seconds when the snapshot was taken.
    pub time: i64,
    /// Human-readable note ("Before discarding 3 hunks in parser.rs").
    pub note: String,
}

/// What a snapshot recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotKind {
    /// Working-tree contents of the requested paths ([`ShadowRepo::snapshot_paths`]).
    Paths,
    /// Index entries of the requested paths ([`ShadowRepo::snapshot_index`]).
    Index,
    /// The whole working tree incl. untracked files, minus ignored ([`ShadowRepo::snapshot_worktree`]).
    Worktree,
}

/// One working-tree entry of a snapshot ([`ShadowRepo::entries`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotEntry {
    /// A regular file or symlink with recorded contents.
    Present {
        /// Repository-relative path.
        path: String,
        /// `true` when the executable bit was set.
        executable: bool,
        /// `true` for a symlink (contents are the link target).
        symlink: bool,
        /// Blob id in the shadow repository.
        oid: Oid,
    },
    /// The path did not exist when the snapshot was taken.
    Absent {
        /// Repository-relative path.
        path: String,
    },
    /// The file was too large for a whole-worktree snapshot and was not recorded.
    Skipped {
        /// Repository-relative path.
        path: String,
    },
}

impl SnapshotEntry {
    /// The entry's path.
    pub fn path(&self) -> &str {
        match self {
            SnapshotEntry::Present { path, .. }
            | SnapshotEntry::Absent { path }
            | SnapshotEntry::Skipped { path } => path,
        }
    }
}

/// One index entry of an index snapshot ([`ShadowRepo::index_entries`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexRecord {
    /// Repository-relative path.
    pub path: String,
    /// Octal mode as git prints it (`100644`, `160000`, ...).
    pub mode: String,
    /// Blob (or gitlink commit) id.
    pub oid: Oid,
    /// Merge stage 0 (normal) ..= 3.
    pub stage: u8,
}

/// How [`ShadowRepo::prune_with`] trims the shadow repository.
#[derive(Clone, Copy, Debug)]
pub struct PruneOptions {
    /// Snapshots older than this are deleted.
    pub keep: Duration,
    /// Delete the oldest remaining snapshots until the repository is at most this big
    /// (the newest snapshot is always kept).
    pub max_bytes: Option<u64>,
}

/// Outcome of [`ShadowRepo::prune_with`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Snapshots deleted.
    pub removed: usize,
    /// Snapshots left.
    pub kept: usize,
    /// Size of the shadow's object store afterwards, in bytes.
    pub bytes: u64,
}

/// Handle to the shadow repository of one working tree. Cheap to clone.
#[derive(Clone, Debug)]
pub struct ShadowRepo {
    git_dir: PathBuf,
    workdir: PathBuf,
}

// ---------------------------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------------------------

/// FNV-1a, 128 bit: a stable, dependency-free hash for directory names and fingerprints.
fn fnv128(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013B;
    let mut hash = OFFSET;
    for part in parts {
        for byte in part.as_ref() {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // Separator, so ["ab","c"] and ["a","bc"] differ.
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:032x}")
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The object directory of a repository (honours linked worktrees' `commondir`).
fn objects_dir(repo: &Repo) -> PathBuf {
    let git_dir = repo.git_dir();
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => {
            let joined = git_dir.join(rel.trim());
            fs::canonicalize(&joined).unwrap_or(joined)
        }
        Err(_) => git_dir.to_path_buf(),
    };
    common.join("objects")
}

impl ShadowRepo {
    /// Where the shadow repository of `workdir` lives under `data_dir`.
    pub fn path_for(data_dir: &Path, workdir: &Path) -> PathBuf {
        let canonical = fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
        let key = fnv128([canonical.to_string_lossy().as_bytes()]);
        data_dir.join("shadow").join(format!("{key}.git"))
    }

    /// Opens (creating on first use) the shadow repository of `repo` below `data_dir`.
    ///
    /// Fails for bare repositories and for SHA-256 repositories (object ids are 20 bytes
    /// in this crate). Never writes into `repo`'s git directory.
    pub fn open(data_dir: &Path, repo: &Repo) -> Result<ShadowRepo> {
        let workdir = repo.workdir().to_path_buf();
        if repo.is_bare() {
            return Err(Error::Bare);
        }
        let git_dir = Self::path_for(data_dir, &workdir);
        let shadow = ShadowRepo { git_dir, workdir };
        let user_objects = objects_dir(repo);
        shadow.ensure_initialized(&user_objects)?;
        Ok(shadow)
    }

    /// The shadow repository's git directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The working tree this shadow belongs to.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    fn ensure_initialized(&self, user_objects: &Path) -> Result<()> {
        // Refuse SHA-256 repositories up front.
        if let Ok(out) = self
            .user_git(&["rev-parse", "--show-object-format"])
            .output()
        {
            let format = String::from_utf8_lossy(&out.stdout);
            if out.status.success() && !format.trim().is_empty() && format.trim() != "sha1" {
                return Err(Error::Git(format!(
                    "unsupported object format {:?} (the shadow repository needs sha1)",
                    format.trim()
                )));
            }
        }
        if !self.git_dir.join("HEAD").is_file() {
            if let Some(parent) = self.git_dir.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut cmd = self.base_command();
            cmd.args(["init", "--bare", "-q", "--template="])
                .arg(&self.git_dir);
            exec(cmd, "git init", None)?;
        }
        // Alternates: the user's objects are read-only here; keep the file in sync with
        // where the objects are now (the repository may have moved).
        let alternates = self.git_dir.join("objects").join("info").join("alternates");
        let wanted = format!("{}\n", user_objects.display());
        if fs::read_to_string(&alternates).ok().as_deref() != Some(wanted.as_str()) {
            if let Some(parent) = alternates.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&alternates, wanted)?;
        }
        let marker = self.git_dir.join("lector-workdir");
        let workdir_line = format!("{}\n", self.workdir.display());
        if fs::read_to_string(&marker).ok().as_deref() != Some(workdir_line.as_str()) {
            fs::write(&marker, workdir_line)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Running git
    // -----------------------------------------------------------------------------------------

    /// `git` with the shadow as `GIT_DIR`, hermetic config, no editor, no prompts.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.cwd());
        cmd.args([
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "core.fsmonitor=false",
            // Always keep a fast-import pack: loosening would go through
            // unpack-objects, which freshens (touches) objects in the alternates.
            "-c",
            "fastimport.unpackLimit=0",
            "-c",
            "core.quotepath=off",
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("LC_ALL", "C");
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_OPTIONAL_LOCKS",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    fn cwd(&self) -> &Path {
        if self.workdir.is_dir() {
            &self.workdir
        } else {
            self.git_dir.parent().unwrap_or(Path::new("."))
        }
    }

    fn shadow_git(&self) -> Command {
        let mut cmd = self.base_command();
        cmd.arg("--git-dir").arg(&self.git_dir);
        cmd
    }

    /// Read-only `git` in the *user's* work tree: user config, `GIT_OPTIONAL_LOCKS=0`,
    /// literal pathspecs.
    fn user_git(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.workdir)
            .args([
                "--literal-pathspecs",
                "--no-pager",
                "-c",
                "core.quotepath=off",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null());
        cmd
    }

    fn lock(&self) -> Result<LockGuard> {
        let path = self.git_dir.join("lector.lock");
        let started = std::time::Instant::now();
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(LockGuard(path));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(120));
                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() > Duration::from_secs(20) {
                        return Err(Error::Git(
                            "the shadow repository is locked by another operation".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Taking snapshots
    // -----------------------------------------------------------------------------------------

    /// Records the working-tree contents of `paths` (files, symlinks; directories are
    /// recorded recursively, `.git` skipped). Paths that do not exist are recorded as
    /// absent, so restoring them deletes whatever is there later. Contents are exact bytes.
    pub fn snapshot_paths(&self, paths: &[&str], note: &str) -> Result<ShadowSnapshot> {
        let mut plan = Plan::new(SnapshotKind::Paths);
        for path in paths {
            let rel = check_rel(path)?;
            self.no_symlink_parents(&rel)?;
            self.collect(&rel, &mut plan, true, u64::MAX)?;
        }
        self.write_snapshot(plan, note)
    }

    /// Records the index entries (mode, blob, stage) of `paths`; paths without an entry
    /// are recorded as "not in the index". Restore with [`ShadowRepo::restore_index`].
    pub fn snapshot_index(&self, paths: &[&str], note: &str) -> Result<ShadowSnapshot> {
        let mut plan = Plan::new(SnapshotKind::Index);
        let mut rels = Vec::new();
        for path in paths {
            rels.push(check_rel(path)?);
        }
        let mut seen: BTreeMap<String, bool> = rels.iter().map(|r| (r.clone(), false)).collect();
        for chunk in rels.chunks(500) {
            let mut args = vec!["ls-files", "-s", "-z", "--"];
            args.extend(chunk.iter().map(String::as_str));
            let out = exec(self.user_git(&args), "git ls-files", None)?;
            for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                let entry = parse_ls_files_stage(record)?;
                for rel in &rels {
                    if is_within(&entry.path, rel) {
                        seen.insert(rel.clone(), true);
                    }
                }
                plan.index.push(entry);
            }
        }
        plan.index
            .sort_by(|a, b| (&a.path, a.stage).cmp(&(&b.path, b.stage)));
        plan.index.dedup();
        for (rel, found) in seen {
            if !found {
                plan.no_index.push(rel);
            }
        }
        self.write_snapshot(plan, note)
    }

    /// Records the whole working tree: tracked and untracked files, minus what
    /// `.gitignore` (and `info/exclude`, global ignores) exclude. Used for agent
    /// checkpoints. Files over 256 MiB are skipped and recorded as such.
    pub fn snapshot_worktree(&self, note: &str) -> Result<ShadowSnapshot> {
        let mut plan = Plan::new(SnapshotKind::Worktree);
        let out = exec(
            self.user_git(&[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ]),
            "git ls-files",
            None,
        )?;
        let mut seen = BTreeSet::new();
        for raw in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Ok(name) = std::str::from_utf8(raw) else {
                continue;
            };
            // Nested repositories are listed as `dir/`; submodules as a bare directory.
            if name.ends_with('/') || !seen.insert(name.to_owned()) {
                continue;
            }
            let Ok(rel) = check_rel(name) else {
                continue;
            };
            if self.no_symlink_parents(&rel).is_err() {
                continue;
            }
            self.collect(&rel, &mut plan, false, WORKTREE_MAX_FILE)?;
        }
        // Tracked files deleted from the working tree are simply not in the snapshot;
        // `Worktree` snapshots treat "not recorded" as absent.
        plan.absent.clear();
        self.write_snapshot(plan, note)
    }

    /// Stats `rel` and adds it (or, for directories when `expand`, everything below) to `plan`.
    fn collect(&self, rel: &str, plan: &mut Plan, expand: bool, max_file: u64) -> Result<()> {
        let abs = self.workdir.join(rel);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                plan.absent.push(rel.to_owned());
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        let ty = meta.file_type();
        if ty.is_symlink() {
            plan.files.push(PlannedFile {
                rel: rel.to_owned(),
                kind: FileKind::Link,
                size: 0,
            });
        } else if ty.is_file() {
            if meta.len() > max_file {
                plan.skipped.push(rel.to_owned());
            } else {
                plan.files.push(PlannedFile {
                    rel: rel.to_owned(),
                    kind: FileKind::File {
                        executable: is_executable(&meta),
                    },
                    size: meta.len(),
                });
            }
        } else if ty.is_dir() && expand {
            let mut names: Vec<String> = fs::read_dir(&abs)?
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.eq_ignore_ascii_case(".git"))
                .collect();
            names.sort();
            for name in names {
                self.collect(&format!("{rel}/{name}"), plan, true, max_file)?;
            }
        }
        // Sockets, fifos, devices and (when not expanding) directories: nothing to record.
        Ok(())
    }

    /// Refuses paths that reach the working tree through a symlinked directory.
    fn no_symlink_parents(&self, rel: &str) -> Result<()> {
        let mut current = self.workdir.clone();
        let comps: Vec<&str> = rel.split('/').collect();
        for comp in &comps[..comps.len().saturating_sub(1)] {
            current.push(comp);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(Error::Parse(format!(
                        "{rel:?} goes through a symbolic link; refusing"
                    )))
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Streams a whole snapshot (blobs, trees, commit, ref) into the shadow through one
    /// `git fast-import`; the ref only appears if the import succeeds.
    fn write_snapshot(&self, mut plan: Plan, note: &str) -> Result<ShadowSnapshot> {
        let _lock = self.lock()?;
        let note = sanitize_note(note);
        let time = now_secs();
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let nonce = format!(
            "{time}-{:08x}{:04x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0),
            NONCE.fetch_add(1, Ordering::Relaxed) & 0xffff
        );
        let ref_name = format!("{REF_PREFIX}{nonce}-{}", std::process::id());

        let mut cmd = self.shadow_git();
        cmd.args(["fast-import", "--quiet", "--done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let stderr = drain(child.stderr.take());
        let result = (|| -> Result<()> {
            let mut w = io::BufWriter::new(
                child
                    .stdin
                    .take()
                    .ok_or_else(|| Error::Git("fast-import has no stdin".into()))?,
            );
            writeln!(w, "commit {ref_name}")?;
            writeln!(w, "committer Lector <lector@localhost> {time} +0000")?;
            writeln!(w, "data {}", note.len())?;
            w.write_all(note.as_bytes())?;
            writeln!(w)?;

            let mut present: Vec<&PlannedFile> = Vec::new();
            for file in &plan.files {
                if self.stream_file(&mut w, file)? {
                    present.push(file);
                } else {
                    plan.vanished.push(file.rel.clone());
                }
            }
            let mut blobs = BTreeSet::new();
            for entry in &plan.index {
                if entry.mode != "160000" && blobs.insert(entry.oid) {
                    writeln!(w, "M 100644 {} blobs/{}", entry.oid, entry.oid)?;
                }
            }
            let manifest = plan.manifest();
            writeln!(w, "M 100644 inline manifest")?;
            writeln!(w, "data {}", manifest.len())?;
            w.write_all(&manifest)?;
            writeln!(w)?;
            writeln!(w, "done")?;
            w.flush()?;
            Ok(())
        })();
        let output = if result.is_err() {
            let _ = child.kill();
            child.wait_with_output()
        } else {
            child.wait_with_output()
        };
        let output = output?;
        let stderr_text = stderr.join().unwrap_or_default();
        if let Err(err) = result {
            // A broken pipe means fast-import died first; its message explains why.
            if !stderr_text.trim().is_empty() {
                return Err(Error::GitCommand {
                    command: "fast-import".into(),
                    status: output.status.to_string(),
                    stderr: stderr_text.trim().to_owned(),
                });
            }
            return Err(err);
        }
        if !output.status.success() {
            return Err(Error::GitCommand {
                command: "fast-import".into(),
                status: output.status.to_string(),
                stderr: stderr_text.trim().to_owned(),
            });
        }
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args(["rev-parse", "--verify", "-q", &ref_name]);
                c
            },
            "git rev-parse",
            None,
        )?;
        let commit = Oid::from_hex(String::from_utf8_lossy(&out).trim())
            .ok_or_else(|| Error::Parse("fast-import produced no commit".into()))?;
        let snapshot = ShadowSnapshot { commit, time, note };
        self.consolidate_if_needed();
        Ok(snapshot)
    }

    /// Writes `M <mode> inline files/<rel>` + data. `Ok(false)` if the file vanished.
    fn stream_file(&self, w: &mut impl Write, file: &PlannedFile) -> Result<bool> {
        let abs = self.workdir.join(&file.rel);
        let tree_path = quote_path(&format!("files/{}", file.rel));
        match file.kind {
            FileKind::Link => {
                let target = match fs::read_link(&abs) {
                    Ok(t) => t,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(err) => return Err(err.into()),
                };
                let bytes = path_bytes(&target);
                writeln!(w, "M 120000 inline {tree_path}")?;
                writeln!(w, "data {}", bytes.len())?;
                w.write_all(&bytes)?;
                writeln!(w)?;
                Ok(true)
            }
            FileKind::File { executable } => {
                let mut handle = match fs::File::open(&abs) {
                    Ok(f) => f,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(err) => return Err(err.into()),
                };
                let mode = if executable { "100755" } else { "100644" };
                let len = handle.metadata()?.len();
                writeln!(w, "M {mode} inline {tree_path}")?;
                if len <= BUFFER_LIMIT {
                    let mut data = Vec::with_capacity(len as usize);
                    handle.read_to_end(&mut data)?;
                    writeln!(w, "data {}", data.len())?;
                    w.write_all(&data)?;
                } else {
                    writeln!(w, "data {len}")?;
                    let copied = io::copy(&mut (&mut handle).take(len), w)?;
                    if copied != len {
                        return Err(Error::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("{} changed while it was being recorded", file.rel),
                        )));
                    }
                }
                writeln!(w)?;
                Ok(true)
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Reading snapshots
    // -----------------------------------------------------------------------------------------

    /// All snapshots, newest first.
    pub fn snapshots(&self) -> Result<Vec<ShadowSnapshot>> {
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args([
                    "for-each-ref",
                    "--format=%(objectname) %(refname) %(contents:subject)",
                    REF_PREFIX,
                ]);
                c
            },
            "git for-each-ref",
            None,
        )?;
        let mut found: Vec<(i64, String, ShadowSnapshot)> = String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, ' ');
                let oid = Oid::from_hex(parts.next()?)?;
                let name = parts.next()?;
                let note = parts.next().unwrap_or("").to_owned();
                let time = ref_time(name)?;
                // Ref names end in sub-second time and a counter: a total order within a second.
                let rest = name.rsplit('/').next().unwrap_or("").to_owned();
                Some((
                    time,
                    rest,
                    ShadowSnapshot {
                        commit: oid,
                        time,
                        note,
                    },
                ))
            })
            .collect();
        found.sort_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
        let snaps = found.into_iter().map(|(_, _, s)| s).collect::<Vec<_>>();
        Ok(snaps)
    }

    /// `true` when the snapshot commit still exists (not pruned).
    pub fn contains(&self, snap: &ShadowSnapshot) -> bool {
        let mut c = self.shadow_git();
        c.args(["cat-file", "-e"])
            .arg(format!("{}^{{commit}}", snap.commit))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c.status().map(|s| s.success()).unwrap_or(false)
    }

    fn manifest(&self, snap: &ShadowSnapshot) -> Result<Manifest> {
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args(["cat-file", "blob"])
                    .arg(format!("{}:manifest", snap.commit));
                c
            },
            "git cat-file",
            None,
        )
        .map_err(|err| match err {
            Error::GitCommand { .. } => Error::Git(format!(
                "snapshot {} is no longer in the shadow repository (pruned?)",
                snap.commit.short()
            )),
            other => other,
        })?;
        Manifest::parse(&out)
    }

    /// What the snapshot recorded.
    pub fn kind(&self, snap: &ShadowSnapshot) -> Result<SnapshotKind> {
        Ok(self.manifest(snap)?.kind)
    }

    /// Recorded `files/` tree entries: path -> (mode, blob).
    fn tree_files(&self, snap: &ShadowSnapshot) -> Result<BTreeMap<String, (String, Oid)>> {
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args(["ls-tree", "-r", "-z", "--full-tree"])
                    .arg(snap.commit.to_string())
                    .args(["--", "files"]);
                c
            },
            "git ls-tree",
            None,
        )?;
        let mut map = BTreeMap::new();
        for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = record.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&record[..tab]).into_owned();
            let name = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
            let mut it = meta.split(' ');
            let (Some(mode), Some(_ty), Some(oid)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let (Some(rel), Some(oid)) = (name.strip_prefix("files/"), Oid::from_hex(oid)) else {
                continue;
            };
            map.insert(rel.to_owned(), (mode.to_owned(), oid));
        }
        Ok(map)
    }

    /// The working-tree entries of a `Paths` / `Worktree` snapshot, sorted by path.
    pub fn entries(&self, snap: &ShadowSnapshot) -> Result<Vec<SnapshotEntry>> {
        let manifest = self.manifest(snap)?;
        let mut out: Vec<SnapshotEntry> = self
            .tree_files(snap)?
            .into_iter()
            .map(|(path, (mode, oid))| SnapshotEntry::Present {
                path,
                executable: mode == "100755",
                symlink: mode == "120000",
                oid,
            })
            .collect();
        out.extend(
            manifest
                .absent
                .into_iter()
                .map(|path| SnapshotEntry::Absent { path }),
        );
        out.extend(
            manifest
                .skipped
                .into_iter()
                .map(|path| SnapshotEntry::Skipped { path }),
        );
        out.sort_by(|a, b| a.path().cmp(b.path()));
        Ok(out)
    }

    /// The index entries of an `Index` snapshot.
    pub fn index_entries(&self, snap: &ShadowSnapshot) -> Result<Vec<IndexRecord>> {
        let manifest = self.manifest(snap)?;
        if manifest.kind != SnapshotKind::Index {
            return Err(Error::Parse("not an index snapshot".into()));
        }
        Ok(manifest.index)
    }

    /// Recorded bytes of `path`: `Ok(None)` when the snapshot recorded it absent or does
    /// not cover it.
    pub fn file_at(&self, snap: &ShadowSnapshot, path: &str) -> Result<Option<Vec<u8>>> {
        let rel = check_rel(path)?;
        let Some((_, oid)) = self.tree_files(snap)?.remove(&rel) else {
            return Ok(None);
        };
        let mut out = None;
        self.cat_blobs(&[oid], |_, reader, _| {
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            out = Some(data);
            Ok(())
        })?;
        Ok(out)
    }

    /// Streams blobs through one `git cat-file --batch`; `sink(index, reader, size)`.
    fn cat_blobs(
        &self,
        oids: &[Oid],
        mut sink: impl FnMut(usize, &mut dyn Read, u64) -> Result<()>,
    ) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        let mut cmd = self.shadow_git();
        cmd.args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let mut stdin = child.stdin.take();
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let writer = std::thread::spawn(move || {
            if let Some(stdin) = stdin.as_mut() {
                let _ = stdin.write_all(input.as_bytes());
            }
        });
        let stderr = drain(child.stderr.take());
        let mut reader = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| Error::Git("cat-file has no stdout".into()))?,
        );
        let mut outcome = Ok(());
        for (index, oid) in oids.iter().enumerate() {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                outcome = Err(Error::Git("git cat-file ended early".into()));
                break;
            }
            let mut parts = header.split_whitespace();
            let (_, Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else {
                outcome = Err(Error::Git(format!(
                    "object {oid} is missing from the shadow"
                )));
                break;
            };
            let Ok(size) = size.parse::<u64>() else {
                outcome = Err(Error::Parse(format!("bad cat-file header {header:?}")));
                break;
            };
            if kind != "blob" {
                outcome = Err(Error::Parse(format!("{oid} is not a blob")));
                break;
            }
            let mut limited = (&mut reader).take(size);
            let sunk = sink(index, &mut limited, size);
            // Drain what the sink left, then the trailing newline.
            let _ = io::copy(&mut limited, &mut io::sink());
            let mut newline = [0u8; 1];
            let _ = reader.read_exact(&mut newline);
            if let Err(err) = sunk {
                outcome = Err(err);
                break;
            }
        }
        drop(reader);
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let _ = stderr.join();
        outcome
    }

    // -----------------------------------------------------------------------------------------
    // Comparing
    // -----------------------------------------------------------------------------------------

    /// File changes from snapshot `a` to snapshot `b` (both `Paths` / `Worktree`
    /// snapshots), sorted by path. Only paths covered by both snapshots are compared,
    /// so a narrower snapshot never reports the rest as deleted. Sides are
    /// [`Side::Blob`]s living in the shadow repository: load them with
    /// [`ShadowRepo::file_at`] or [`ShadowRepo::load`].
    pub fn changes_between(
        &self,
        a: &ShadowSnapshot,
        b: &ShadowSnapshot,
    ) -> Result<Vec<FileChange>> {
        let (ma, mb) = (self.manifest(a)?, self.manifest(b)?);
        if ma.kind == SnapshotKind::Index || mb.kind == SnapshotKind::Index {
            return Err(Error::Parse(
                "index snapshots have no file contents to compare".into(),
            ));
        }
        let (fa, fb) = (self.tree_files(a)?, self.tree_files(b)?);
        let covered_a = ma.covers_fn(&fa);
        let covered_b = mb.covers_fn(&fb);
        let mut paths: BTreeSet<&String> = fa.keys().collect();
        paths.extend(fb.keys());
        let mut changes = Vec::new();
        for path in paths {
            if !covered_a(path) || !covered_b(path) {
                continue;
            }
            let change = match (fa.get(path), fb.get(path)) {
                (Some((ma_, oa)), Some((mb_, ob))) => {
                    if oa == ob && ma_ == mb_ {
                        continue;
                    }
                    let kind = if (ma_ == "120000") != (mb_ == "120000") {
                        ChangeKind::TypeChange
                    } else {
                        ChangeKind::Modified
                    };
                    FileChange {
                        old_path: Some(path.clone()),
                        new_path: Some(path.clone()),
                        kind,
                        old: Side::Blob(*oa),
                        new: Side::Blob(*ob),
                        binary: None,
                    }
                }
                (Some((_, oa)), None) => FileChange {
                    old_path: Some(path.clone()),
                    new_path: None,
                    kind: ChangeKind::Deleted,
                    old: Side::Blob(*oa),
                    new: Side::None,
                    binary: None,
                },
                (None, Some((_, ob))) => FileChange {
                    old_path: None,
                    new_path: Some(path.clone()),
                    kind: ChangeKind::Added,
                    old: Side::None,
                    new: Side::Blob(*ob),
                    binary: None,
                },
                (None, None) => continue,
            };
            changes.push(change);
        }
        Ok(changes)
    }

    /// File changes from snapshot `a` to the *current* working tree for the paths the
    /// snapshot covers (restricted to `only` if given). Old sides are shadow blobs, new
    /// sides [`Side::Workdir`]. Shows what an undo would change.
    pub fn changes_to_worktree(
        &self,
        a: &ShadowSnapshot,
        only: Option<&[&str]>,
    ) -> Result<Vec<FileChange>> {
        let manifest = self.manifest(a)?;
        if manifest.kind == SnapshotKind::Index {
            return Err(Error::Parse(
                "index snapshots have no file contents to compare".into(),
            ));
        }
        let files = self.tree_files(a)?;
        let mut candidates: BTreeSet<String> = files.keys().cloned().collect();
        candidates.extend(manifest.absent.iter().cloned());
        if let Some(only) = only {
            let wanted: Vec<String> = only.iter().filter_map(|p| check_rel(p).ok()).collect();
            candidates.retain(|c| wanted.iter().any(|w| is_within(c, w)));
        }
        let rels: Vec<String> = candidates.into_iter().collect();
        let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let current = self.worktree_state(&refs)?;
        let mut changes = Vec::new();
        for (rel, state) in rels.into_iter().zip(current) {
            let before = files.get(&rel);
            match (before, state) {
                (Some((mode, oid)), Some((cur_mode, cur_oid))) => {
                    if *oid == cur_oid && *mode == cur_mode {
                        continue;
                    }
                    changes.push(FileChange {
                        old_path: Some(rel.clone()),
                        new_path: Some(rel.clone()),
                        kind: ChangeKind::Modified,
                        old: Side::Blob(*oid),
                        new: Side::Workdir(rel),
                        binary: None,
                    });
                }
                (Some((_, oid)), None) => changes.push(FileChange {
                    old_path: Some(rel),
                    new_path: None,
                    kind: ChangeKind::Deleted,
                    old: Side::Blob(*oid),
                    new: Side::None,
                    binary: None,
                }),
                (None, Some(_)) => changes.push(FileChange {
                    old_path: None,
                    new_path: Some(rel.clone()),
                    kind: ChangeKind::Added,
                    old: Side::None,
                    new: Side::Workdir(rel),
                    binary: None,
                }),
                (None, None) => {}
            }
        }
        Ok(changes)
    }

    /// Loads a [`Side::Blob`] living in the shadow repository (or, for any other side,
    /// the working-tree file of [`Side::Workdir`]); `Ok(None)` for missing sides.
    pub fn load(&self, side: &Side) -> Result<Option<Vec<u8>>> {
        match side {
            Side::None | Side::Index(_) => Ok(None),
            Side::Workdir(path) => {
                let rel = check_rel(path)?;
                match fs::read(self.workdir.join(rel)) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(err.into()),
                }
            }
            Side::Blob(oid) => {
                let mut out = None;
                self.cat_blobs(&[*oid], |_, reader, _| {
                    let mut data = Vec::new();
                    reader.read_to_end(&mut data)?;
                    out = Some(data);
                    Ok(())
                })?;
                Ok(out)
            }
        }
    }

    /// Current `(mode, blob id)` of working-tree paths (`None` = missing), hashed exactly
    /// like a snapshot would record them (no filters). Nothing is written.
    fn worktree_state(&self, rels: &[&str]) -> Result<Vec<Option<(String, Oid)>>> {
        let mut result: Vec<Option<(String, Oid)>> = vec![None; rels.len()];
        let mut file_indices = Vec::new();
        let mut file_args = String::new();
        for (i, rel) in rels.iter().enumerate() {
            let rel = check_rel(rel)?;
            self.no_symlink_parents(&rel)?;
            let abs = self.workdir.join(&rel);
            let Ok(meta) = fs::symlink_metadata(&abs) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&abs)?;
                let mut c = self.shadow_git();
                c.args(["hash-object", "--stdin"]);
                let out = exec(c, "git hash-object", Some(&path_bytes(&target)))?;
                if let Some(oid) = Oid::from_hex(String::from_utf8_lossy(&out).trim()) {
                    result[i] = Some(("120000".into(), oid));
                }
            } else if meta.is_file() {
                file_indices.push((i, is_executable(&meta)));
                file_args.push_str(&quote_path(&rel));
                file_args.push('\n');
            }
        }
        if !file_indices.is_empty() {
            let mut c = self.shadow_git();
            c.args(["hash-object", "--no-filters", "--stdin-paths"]);
            let out = exec(c, "git hash-object", Some(file_args.as_bytes()))?;
            let text = String::from_utf8_lossy(&out);
            for ((i, exec_bit), line) in file_indices.into_iter().zip(text.lines()) {
                if let Some(oid) = Oid::from_hex(line) {
                    let mode = if exec_bit { "100755" } else { "100644" };
                    result[i] = Some((mode.into(), oid));
                }
            }
        }
        Ok(result)
    }

    /// Per-file fingerprints of the current working-tree state of `paths`: `"<mode> <blob id>"`
    /// (no filters, like a snapshot would record it) or `"absent"`. Directories expand to
    /// the files below them; requested paths that do not exist map to `"absent"`. The
    /// journal stores this after a write and compares it before an undo to notice which
    /// files changed in the meantime. Nothing is written.
    pub fn fingerprints(&self, paths: &[&str]) -> Result<BTreeMap<String, String>> {
        let mut rels: BTreeSet<String> = BTreeSet::new();
        for path in paths {
            let rel = check_rel(path)?;
            self.no_symlink_parents(&rel)?;
            let abs = self.workdir.join(&rel);
            if fs::symlink_metadata(&abs).is_ok_and(|m| m.is_dir()) {
                let mut plan = Plan::new(SnapshotKind::Paths);
                self.collect(&rel, &mut plan, true, u64::MAX)?;
                rels.extend(plan.files.into_iter().map(|f| f.rel));
            } else {
                rels.insert(rel);
            }
        }
        let rels: Vec<String> = rels.into_iter().collect();
        let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let state = self.worktree_state(&refs)?;
        Ok(rels
            .into_iter()
            .zip(state)
            .map(|(rel, st)| {
                let value = match st {
                    Some((mode, oid)) => format!("{mode} {oid}"),
                    None => "absent".to_owned(),
                };
                (rel, value)
            })
            .collect())
    }

    /// One stable fingerprint for [`ShadowRepo::fingerprints`] of `paths`.
    pub fn fingerprint_worktree(&self, paths: &[&str]) -> Result<String> {
        let map = self.fingerprints(paths)?;
        Ok(fnv128(
            map.iter().flat_map(|(k, v)| [k.as_bytes(), v.as_bytes()]),
        ))
    }

    /// Per-path fingerprints of the *index* entries of `paths` (all stages, mode and blob
    /// id), `"absent"` for requested paths without an entry. Compare with a stored map
    /// before an index undo.
    pub fn index_fingerprints(&self, paths: &[&str]) -> Result<BTreeMap<String, String>> {
        let mut rels = Vec::new();
        for path in paths {
            rels.push(check_rel(path)?);
        }
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        let mut covered: BTreeSet<String> = BTreeSet::new();
        for chunk in rels.chunks(500) {
            let mut args = vec!["ls-files", "-s", "-z", "--"];
            args.extend(chunk.iter().map(String::as_str));
            let output = exec(self.user_git(&args), "git ls-files", None)?;
            for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                let entry = parse_ls_files_stage(record)?;
                for rel in &rels {
                    if is_within(&entry.path, rel) {
                        covered.insert(rel.clone());
                    }
                }
                let item = format!("{} {} {}", entry.mode, entry.oid, entry.stage);
                out.entry(entry.path)
                    .and_modify(|v| {
                        v.push(';');
                        v.push_str(&item);
                    })
                    .or_insert(item);
            }
        }
        for rel in rels {
            if !covered.contains(&rel) {
                out.insert(rel, "absent".to_owned());
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------------------------
    // Restoring (explicit undo only)
    // -----------------------------------------------------------------------------------------

    /// Puts the recorded contents of `paths` back into the working tree: exact bytes,
    /// recreated or deleted as recorded, executable bit and symlinks kept. Directories
    /// restore every file recorded below them. **Writes user files — call it only from
    /// an explicit undo.** Never writes inside `.git` or through symlinked directories.
    pub fn restore_paths(&self, snap: &ShadowSnapshot, paths: &[&str]) -> Result<()> {
        let manifest = self.manifest(snap)?;
        if manifest.kind == SnapshotKind::Index {
            return Err(Error::Parse(
                "this snapshot recorded the index, not files; use restore_index".into(),
            ));
        }
        let files = self.tree_files(snap)?;
        let full = manifest.kind == SnapshotKind::Worktree;

        // Plan first: nothing is written when any requested path is unusable.
        let mut writes: Vec<(String, String, Oid)> = Vec::new(); // (rel, mode, oid)
        let mut deletes: Vec<String> = Vec::new();
        for path in paths {
            let rel = check_rel(path)?;
            self.no_symlink_parents(&rel)?;
            let mut found = false;
            for (name, (mode, oid)) in &files {
                if is_within(name, &rel) {
                    found = true;
                    writes.push((name.clone(), mode.clone(), *oid));
                }
            }
            if manifest.skipped.iter().any(|s| is_within(s, &rel)) {
                found = true;
            }
            if manifest.absent.iter().any(|a| a == &rel) || (!found && full) {
                found = true;
                deletes.push(rel.clone());
            }
            if !found {
                return Err(Error::Parse(format!(
                    "{rel:?} is not part of this snapshot"
                )));
            }
        }
        // A path restored from a file and deleted at the same time can't be both.
        writes.sort();
        writes.dedup();
        deletes.sort();
        deletes.dedup();
        for (rel, ..) in &writes {
            self.no_symlink_parents(rel)?;
            let abs = self.workdir.join(rel);
            if fs::symlink_metadata(&abs).is_ok_and(|m| m.is_dir()) {
                return Err(Error::Io(io::Error::other(format!(
                    "{rel} is now a directory; move it away to restore the file"
                ))));
            }
        }
        for rel in &deletes {
            let abs = self.workdir.join(rel);
            if fs::symlink_metadata(&abs).is_ok_and(|m| m.is_dir()) {
                return Err(Error::Io(io::Error::other(format!(
                    "{rel} is now a directory; it was absent when the snapshot was taken"
                ))));
            }
        }

        let oids: Vec<Oid> = writes.iter().map(|(_, _, o)| *o).collect();
        self.cat_blobs(&oids, |i, reader, size| {
            let (rel, mode, _) = &writes[i];
            self.write_restored(rel, mode, reader, size)
        })?;
        for rel in &deletes {
            let abs = self.workdir.join(rel);
            match fs::remove_file(&abs) {
                Ok(()) => self.remove_empty_parents(&abs),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    fn remove_empty_parents(&self, file: &Path) {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d == self.workdir || !d.starts_with(&self.workdir) {
                break;
            }
            if fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    /// Atomically writes one restored file (temp file in the same directory + rename).
    fn write_restored(
        &self,
        rel: &str,
        mode: &str,
        reader: &mut dyn Read,
        size: u64,
    ) -> Result<()> {
        let abs = self.workdir.join(rel);
        let parent = abs
            .parent()
            .ok_or_else(|| Error::Parse(format!("bad path {rel:?}")))?;
        fs::create_dir_all(parent)?;
        let existing = fs::symlink_metadata(&abs).ok();
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let tmp = parent.join(format!(
            ".lector-restore-{}-{}.tmp",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<()> {
            if mode == "120000" {
                let mut target = Vec::with_capacity(size as usize);
                reader.read_to_end(&mut target)?;
                make_symlink(&target, &tmp)?;
            } else {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                io::copy(reader, &mut file)?;
                file.sync_all()?;
                drop(file);
                apply_permissions(&tmp, existing.as_ref(), mode == "100755")?;
            }
            fs::rename(&tmp, &abs)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Puts the recorded index entries of `paths` back (`git update-index --index-info`
    /// in the user's repository; the working tree is untouched). The entries' blobs
    /// already exist in the user's object database — they were staged there. **Writes the
    /// user's index — call it only from an explicit undo.**
    pub fn restore_index(&self, snap: &ShadowSnapshot, paths: &[&str]) -> Result<()> {
        let manifest = self.manifest(snap)?;
        if manifest.kind != SnapshotKind::Index {
            return Err(Error::Parse(
                "this snapshot did not record the index".into(),
            ));
        }
        let mut rels = Vec::new();
        for path in paths {
            let rel = check_rel(path)?;
            let covered = manifest.index.iter().any(|e| is_within(&e.path, &rel))
                || manifest.no_index.iter().any(|p| p == &rel);
            if !covered {
                return Err(Error::Parse(format!(
                    "{rel:?} is not part of this snapshot"
                )));
            }
            rels.push(rel);
        }
        // Everything currently in the index for these paths goes first (all stages), then
        // the recorded entries come back.
        let mut input: Vec<u8> = Vec::new();
        let zero = "0".repeat(40);
        let mut current = BTreeSet::new();
        for chunk in rels.chunks(500) {
            let mut args = vec!["ls-files", "-s", "-z", "--"];
            args.extend(chunk.iter().map(String::as_str));
            let out = exec(self.user_git(&args), "git ls-files", None)?;
            for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                current.insert(parse_ls_files_stage(record)?.path);
            }
        }
        for path in &current {
            input.extend_from_slice(format!("0 {zero}\t{path}").as_bytes());
            input.push(0);
        }
        for entry in manifest
            .index
            .iter()
            .filter(|e| rels.iter().any(|r| is_within(&e.path, r)))
        {
            input.extend_from_slice(
                format!(
                    "{} {} {}\t{}",
                    entry.mode, entry.oid, entry.stage, entry.path
                )
                .as_bytes(),
            );
            input.push(0);
        }
        if input.is_empty() {
            return Ok(());
        }
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.workdir)
            .args([
                "--literal-pathspecs",
                "--no-pager",
                "update-index",
                "-z",
                "--index-info",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OPTIONAL_LOCKS");
        exec(cmd, "git update-index", Some(&input)).map(drop)
    }

    // -----------------------------------------------------------------------------------------
    // Housekeeping
    // -----------------------------------------------------------------------------------------

    /// Deletes snapshots older than `keep` and trims to [`DEFAULT_MAX_BYTES`].
    pub fn prune(&self, keep: Duration) -> Result<()> {
        self.prune_with(PruneOptions {
            keep,
            max_bytes: Some(DEFAULT_MAX_BYTES),
        })
        .map(drop)
    }

    /// Deletes snapshots older than `opts.keep`, then the oldest ones until the shadow's
    /// objects fit in `opts.max_bytes` (the newest snapshot always stays), then
    /// repacks and drops unreachable objects. Only the shadow repository is touched.
    pub fn prune_with(&self, opts: PruneOptions) -> Result<PruneReport> {
        let _lock = self.lock()?;
        let mut snaps = self.snapshots()?;
        let cutoff = now_secs() - opts.keep.as_secs() as i64;
        let mut removed = 0;
        let mut doomed: Vec<ShadowSnapshot> = Vec::new();
        snaps.retain(|s| {
            if s.time <= cutoff {
                doomed.push(s.clone());
                false
            } else {
                true
            }
        });
        let mut report = PruneReport::default();
        removed += self.delete_snapshots(&doomed)?;
        let mut bytes = dir_size(&self.git_dir.join("objects"));
        if let Some(cap) = opts.max_bytes {
            // `snaps` is newest first: drop from the back while over the cap. Sizes only
            // shrink once the objects are repacked, so measure after a gc each round.
            if bytes > cap && snaps.len() > 1 {
                self.gc()?;
                bytes = dir_size(&self.git_dir.join("objects"));
            }
            while bytes > cap && snaps.len() > 1 {
                let batch = (snaps.len() / 4).max(1).min(snaps.len() - 1);
                let victims: Vec<ShadowSnapshot> = snaps.split_off(snaps.len() - batch);
                removed += self.delete_snapshots(&victims)?;
                self.gc()?;
                bytes = dir_size(&self.git_dir.join("objects"));
            }
        }
        if removed > 0 {
            self.gc()?;
            bytes = dir_size(&self.git_dir.join("objects"));
        }
        report.removed = removed;
        report.kept = snaps.len();
        report.bytes = bytes;
        Ok(report)
    }

    fn delete_snapshots(&self, snaps: &[ShadowSnapshot]) -> Result<usize> {
        if snaps.is_empty() {
            return Ok(0);
        }
        let wanted: BTreeSet<Oid> = snaps.iter().map(|s| s.commit).collect();
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args([
                    "for-each-ref",
                    "--format=%(objectname) %(refname)",
                    REF_PREFIX,
                ]);
                c
            },
            "git for-each-ref",
            None,
        )?;
        let mut input = String::new();
        let mut count = 0;
        for line in String::from_utf8_lossy(&out).lines() {
            let Some((oid, name)) = line.split_once(' ') else {
                continue;
            };
            if Oid::from_hex(oid).is_some_and(|o| wanted.contains(&o)) {
                input.push_str(&format!("delete {name}\n"));
                count += 1;
            }
        }
        if !input.is_empty() {
            let mut c = self.shadow_git();
            c.args(["update-ref", "--stdin"]);
            exec(c, "git update-ref", Some(input.as_bytes()))?;
        }
        Ok(count)
    }

    /// Repack local objects, drop unreachable ones. Callers hold the lock.
    fn gc(&self) -> Result<()> {
        let mut c = self.shadow_git();
        c.args(["gc", "--prune=now", "--quiet"]);
        exec(c, "git gc", None).map(drop)
    }

    /// Every snapshot adds one small pack (see `fastimport.unpackLimit`); merge them
    /// once there are many so lookups stay fast. Best effort.
    fn consolidate_if_needed(&self) {
        let packs = fs::read_dir(self.git_dir.join("objects").join("pack"))
            .map(|dir| {
                dir.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                    .count()
            })
            .unwrap_or(0);
        if packs >= PACK_CONSOLIDATE_AT {
            let mut c = self.shadow_git();
            c.args(["repack", "-a", "-d", "-l", "-q"]);
            let _ = exec(c, "git repack", None);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Planning a snapshot
// ---------------------------------------------------------------------------------------------

enum FileKind {
    File { executable: bool },
    Link,
}

struct PlannedFile {
    rel: String,
    kind: FileKind,
    #[allow(dead_code)]
    size: u64,
}

struct Plan {
    kind: SnapshotKind,
    files: Vec<PlannedFile>,
    absent: Vec<String>,
    skipped: Vec<String>,
    /// Files that disappeared between stat and read (recorded absent).
    vanished: Vec<String>,
    index: Vec<IndexRecord>,
    no_index: Vec<String>,
}

impl Plan {
    fn new(kind: SnapshotKind) -> Plan {
        Plan {
            kind,
            files: Vec::new(),
            absent: Vec::new(),
            skipped: Vec::new(),
            vanished: Vec::new(),
            index: Vec::new(),
            no_index: Vec::new(),
        }
    }

    /// NUL-separated records: header, kind, then tagged records.
    fn manifest(&self) -> Vec<u8> {
        let mut tokens: Vec<Vec<u8>> = vec![
            MANIFEST_MAGIC.into(),
            b"1".to_vec(),
            match self.kind {
                SnapshotKind::Paths => b"paths".to_vec(),
                SnapshotKind::Index => b"index".to_vec(),
                SnapshotKind::Worktree => b"worktree".to_vec(),
            },
        ];
        for path in self.absent.iter().chain(&self.vanished) {
            tokens.push(b"a".to_vec());
            tokens.push(path.as_bytes().to_vec());
        }
        for path in &self.skipped {
            tokens.push(b"s".to_vec());
            tokens.push(path.as_bytes().to_vec());
        }
        for entry in &self.index {
            tokens.push(b"i".to_vec());
            tokens.push(entry.mode.as_bytes().to_vec());
            tokens.push(entry.oid.to_string().into_bytes());
            tokens.push(entry.stage.to_string().into_bytes());
            tokens.push(entry.path.as_bytes().to_vec());
        }
        for path in &self.no_index {
            tokens.push(b"x".to_vec());
            tokens.push(path.as_bytes().to_vec());
        }
        let mut out = Vec::new();
        for token in tokens {
            out.extend_from_slice(&token);
            out.push(0);
        }
        out
    }
}

struct Manifest {
    kind: SnapshotKind,
    absent: Vec<String>,
    skipped: Vec<String>,
    index: Vec<IndexRecord>,
    no_index: Vec<String>,
}

impl Manifest {
    fn parse(bytes: &[u8]) -> Result<Manifest> {
        let bad = |what: &str| Error::Parse(format!("corrupt shadow manifest: {what}"));
        let mut tokens = bytes
            .split(|b| *b == 0)
            .map(|t| String::from_utf8_lossy(t).into_owned());
        if tokens.next().as_deref() != Some(MANIFEST_MAGIC) {
            return Err(bad("bad magic"));
        }
        if tokens.next().as_deref() != Some("1") {
            return Err(bad("unknown version"));
        }
        let kind = match tokens.next().as_deref() {
            Some("paths") => SnapshotKind::Paths,
            Some("index") => SnapshotKind::Index,
            Some("worktree") => SnapshotKind::Worktree,
            _ => return Err(bad("unknown kind")),
        };
        let mut manifest = Manifest {
            kind,
            absent: Vec::new(),
            skipped: Vec::new(),
            index: Vec::new(),
            no_index: Vec::new(),
        };
        while let Some(tag) = tokens.next() {
            match tag.as_str() {
                "" => break,
                "a" => manifest.absent.push(tokens.next().ok_or_else(|| bad("a"))?),
                "s" => manifest
                    .skipped
                    .push(tokens.next().ok_or_else(|| bad("s"))?),
                "x" => manifest
                    .no_index
                    .push(tokens.next().ok_or_else(|| bad("x"))?),
                "i" => {
                    let mode = tokens.next().ok_or_else(|| bad("i mode"))?;
                    let oid = tokens.next().ok_or_else(|| bad("i oid"))?;
                    let stage = tokens.next().ok_or_else(|| bad("i stage"))?;
                    let path = tokens.next().ok_or_else(|| bad("i path"))?;
                    manifest.index.push(IndexRecord {
                        path,
                        mode,
                        oid: Oid::from_hex(&oid).ok_or_else(|| bad("i oid"))?,
                        stage: stage.parse().map_err(|_| bad("i stage"))?,
                    });
                }
                _ => return Err(bad("unknown record")),
            }
        }
        Ok(manifest)
    }

    /// Whether this snapshot says something about `path` (present, absent or skipped);
    /// whole-worktree snapshots cover everything.
    fn covers_fn<'a>(
        &'a self,
        files: &'a BTreeMap<String, (String, Oid)>,
    ) -> impl Fn(&str) -> bool + 'a {
        move |path: &str| {
            self.kind == SnapshotKind::Worktree
                || files.contains_key(path)
                || self.absent.iter().any(|a| a == path)
                || self.skipped.iter().any(|a| a == path)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Normalizes a repository-relative path and refuses anything that could leave the work
/// tree or reach into a `.git` directory.
fn check_rel(path: &str) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| Error::Parse(format!("path is not valid UTF-8: {path:?}")))?;
                if name.eq_ignore_ascii_case(".git") {
                    return Err(Error::Parse(format!(
                        "refusing to touch a .git directory: {path:?}"
                    )));
                }
                parts.push(name.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Parse(format!(
                    "refusing to use a path outside the working tree: {path:?}"
                )))
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::Parse(format!("empty path: {path:?}")));
    }
    Ok(parts.join("/"))
}

/// `path == base` or `path` lies below the directory `base`.
fn is_within(path: &str, base: &str) -> bool {
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn sanitize_note(note: &str) -> String {
    let single: String = note
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    single.trim().to_owned()
}

fn ref_time(refname: &str) -> Option<i64> {
    refname
        .strip_prefix(REF_PREFIX)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// C-style quoting for fast-import / hash-object paths when needed.
fn quote_path(path: &str) -> String {
    let needs = path
        .bytes()
        .any(|b| b == b'"' || b == b'\\' || b < 0x20 || b == 0x7f);
    if !needs {
        return path.to_owned();
    }
    let mut out = String::from("\"");
    for ch in path.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn parse_ls_files_stage(record: &[u8]) -> Result<IndexRecord> {
    let bad = || {
        Error::Parse(format!(
            "unexpected ls-files output: {:?}",
            String::from_utf8_lossy(record)
        ))
    };
    let tab = record.iter().position(|b| *b == b'\t').ok_or_else(bad)?;
    let meta = String::from_utf8_lossy(&record[..tab]).into_owned();
    let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
    let mut it = meta.split(' ');
    let (Some(mode), Some(oid), Some(stage)) = (it.next(), it.next(), it.next()) else {
        return Err(bad());
    };
    Ok(IndexRecord {
        path,
        mode: mode.to_owned(),
        oid: Oid::from_hex(oid).ok_or_else(bad)?,
        stage: stage.parse().map_err(|_| bad())?,
    })
}

fn spawn_error(err: io::Error) -> Error {
    if err.kind() == io::ErrorKind::NotFound {
        Error::GitNotInstalled("the shadow repository")
    } else {
        Error::Io(err)
    }
}

/// Collects a child's stderr on a thread so a chatty child can't block on a full pipe.
fn drain(pipe: Option<std::process::ChildStderr>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    })
}

/// Runs `cmd` to completion, feeding `stdin`; non-zero exit is [`Error::GitCommand`].
fn exec(mut cmd: Command, what: &str, stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child: Child = cmd.spawn().map_err(spawn_error)?;
    let writer = match (stdin, child.stdin.take()) {
        (Some(input), Some(mut pipe)) => {
            let input = input.to_vec();
            Some(std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            }))
        }
        _ => None,
    };
    let output = child.wait_with_output()?;
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Err(Error::GitCommand {
        command: what.to_owned(),
        status: output.status.to_string(),
        stderr: if stderr.is_empty() { stdout } else { stderr },
    })
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().replace('\\', "/").into_bytes()
}

#[cfg(unix)]
fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), link)
}

#[cfg(not(unix))]
fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    // No portable symlink creation: keep the link target as the file's content.
    fs::write(link, target)
}

/// Keeps the permission bits of the file being replaced; otherwise the freshly created
/// file's. Only the executable bits follow the recorded mode.
#[cfg(unix)]
fn apply_permissions(
    tmp: &Path,
    existing: Option<&fs::Metadata>,
    executable: bool,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut mode = match existing {
        Some(meta) if meta.is_file() => meta.permissions().mode() & 0o7777,
        _ => fs::metadata(tmp)?.permissions().mode() & 0o7777,
    };
    let has_exec = mode & 0o111 != 0;
    if executable && !has_exec {
        // Like git: x wherever r is.
        mode |= (mode & 0o444) >> 2;
    } else if !executable && has_exec {
        mode &= !0o111;
    }
    fs::set_permissions(tmp, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn apply_permissions(
    tmp: &Path,
    existing: Option<&fs::Metadata>,
    _executable: bool,
) -> io::Result<()> {
    if let Some(meta) = existing {
        if meta.is_file() {
            fs::set_permissions(tmp, meta.permissions())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_paths_are_checked() {
        assert_eq!(check_rel("a/b.txt").unwrap(), "a/b.txt");
        assert_eq!(check_rel("./a//b").unwrap(), "a/b");
        for bad in [
            "",
            "..",
            "../x",
            "/etc/passwd",
            "a/../../x",
            ".git/config",
            "sub/.GIT/x",
        ] {
            assert!(check_rel(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn within() {
        assert!(is_within("a/b", "a"));
        assert!(is_within("a", "a"));
        assert!(!is_within("ab", "a"));
        assert!(!is_within("a", "a/b"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_path("plain name.txt"), "plain name.txt");
        assert_eq!(quote_path("a\nb"), "\"a\\nb\"");
        assert_eq!(quote_path("q\"x\\"), "\"q\\\"x\\\\\"");
        assert_eq!(quote_path("tab\there"), "\"tab\\there\"");
    }

    #[test]
    fn manifest_round_trips() {
        let mut plan = Plan::new(SnapshotKind::Index);
        let oid = Oid::from_hex("1111111111111111111111111111111111111111").unwrap();
        plan.index.push(IndexRecord {
            path: "weird\nname".into(),
            mode: "100644".into(),
            oid,
            stage: 2,
        });
        plan.no_index.push("gone".into());
        plan.absent.push("missing".into());
        let parsed = Manifest::parse(&plan.manifest()).unwrap();
        assert_eq!(parsed.kind, SnapshotKind::Index);
        assert_eq!(parsed.index.len(), 1);
        assert_eq!(parsed.index[0].path, "weird\nname");
        assert_eq!(parsed.index[0].stage, 2);
        assert_eq!(parsed.no_index, ["gone"]);
        assert_eq!(parsed.absent, ["missing"]);
        assert!(Manifest::parse(b"nope\0").is_err());
    }

    #[test]
    fn ref_names_carry_the_time() {
        assert_eq!(
            ref_time("refs/lector/snapshots/1700000000-0001-9"),
            Some(1_700_000_000)
        );
        assert_eq!(ref_time("refs/heads/main"), None);
    }

    #[test]
    fn hash_is_stable_and_separates_parts() {
        assert_eq!(fnv128(["abc"]), fnv128(["abc"]));
        assert_ne!(fnv128(["ab", "c"]), fnv128(["a", "bc"]));
        assert_eq!(fnv128(["x"]).len(), 32);
    }
}
