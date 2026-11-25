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
//!
//! **Folders without git** (ADR 0011) get a shadow repository too ([`ShadowRepo::open_folder`]),
//! at the same place a repository of that folder would have it (keyed by the canonical path),
//! created at the first backup and never by looking for one ([`ShadowRepo::existing_folder`]).
//! It has no alternates and is *path-scoped*: only [`ShadowRepo::snapshot_paths`], restores and
//! fingerprints of named paths work; whole-tree and index operations refuse
//! ([`Error::NotARepository`]), so nothing ever lists or walks the folder (it may be a home
//! directory) and no git command runs in it. Every command names the shadow explicitly
//! (`--git-dir`, which turns repository discovery off) and the shadow is bare, so neither a
//! repository above the folder nor the folder's `.gitattributes` is ever read.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::{ChangeKind, FileChange, Oid, Repo, Side};

mod capture;
mod command;
mod files;
mod folder;
mod gc;
mod merge;
mod open;
mod read;
mod restore;
mod util;

use command::exec;
pub(crate) use open::{known_shadow_with, load_from_known_shadows};
use open::{known_shadows, objects_dir, remember_shadow};
use util::{check_rel, dir_size, is_within, parse_ls_files_stage, ref_time};

/// Default size cap of one shadow repository for [`ShadowRepo::prune`].
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// Snapshots younger than this are never removed to meet the size cap by
/// [`ShadowRepo::prune`] (the trim every backup path runs).
pub const SIZE_TRIM_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest file the shadow of a folder without git stores (ADR 0011): larger ones are
/// recorded as skipped ([`ShadowRepo::snapshot_paths`]) or refused ([`Oversize::Refuse`]).
pub const FOLDER_MAX_FILE: u64 = 64 * 1024 * 1024;

/// What [`ShadowRepo::snapshot_files`] does with a path it will not store: a file over the
/// size limit, a directory, or a FIFO, socket or device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Oversize {
    /// Fail the whole snapshot (nothing is recorded): for a backup a write depends on.
    Refuse,
    /// Record a file over the limit as skipped without reading it (a restore of that path then
    /// refuses); still fail for a directory, FIFO, socket or device.
    Skip,
    /// Record every such path as skipped without reading it, and also a path now reached through
    /// a symlinked parent: for recording the state at the end of an agent turn, which must not
    /// fail (and so lose the turn's undo) because a file was replaced by a directory or a parent
    /// by a link. A restore of a skipped path refuses, naming it.
    SkipAll,
}
/// Files larger than this are skipped (and recorded as skipped) by
/// [`ShadowRepo::snapshot_worktree`]; explicit [`ShadowRepo::snapshot_paths`] never skips.
pub const WORKTREE_MAX_FILE: u64 = 256 * 1024 * 1024;
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

/// Options of [`ShadowRepo::fetch_with`].
#[derive(Clone, Default)]
pub struct FetchOptions {
    /// Extra HTTP header for git (`Authorization: Basic ...`); never logged, passed through
    /// `GIT_CONFIG_*` (not argv), and sent with `http.followRedirects=false` so it reaches only
    /// the URL's host.
    pub http_header: Option<String>,
}

impl std::fmt::Debug for FetchOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchOptions")
            .field(
                "http_header",
                &self.http_header.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
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
    /// The shadow of a folder without git: path-scoped operations only.
    folder: bool,
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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
        let shadow = ShadowRepo {
            git_dir,
            workdir,
            folder: false,
        };
        let user_objects = objects_dir(repo);
        shadow.ensure_initialized(Some(&user_objects))?;
        remember_shadow(&shadow);
        Ok(shadow)
    }

    /// The note (commit subject) of the snapshot commit `commit`, if it is in this shadow.
    pub fn note_of(&self, commit: Oid) -> Option<String> {
        let mut c = self.shadow_git();
        c.args(["log", "-1", "--format=%s"])
            .arg(commit.to_string())
            .arg("--");
        let out = exec(c, "git log", None).ok()?;
        let note = String::from_utf8_lossy(&out).trim().to_owned();
        (!note.is_empty()).then_some(note)
    }

    /// The note of a snapshot commit found in any known shadow repository of `repo`.
    pub fn known_note(repo: &Repo, commit: Oid) -> Option<String> {
        known_shadow_with(repo.workdir(), commit).and_then(|s| s.note_of(commit))
    }

    /// Shadow repositories of `repo` opened by this process (newest first), e.g. to read
    /// a [`crate::Changeset::Snapshots`]. Empty until [`ShadowRepo::open`] ran once.
    pub fn known_for(repo: &Repo) -> Vec<ShadowRepo> {
        known_shadows(repo.workdir())
    }

    /// The shadow repository's git directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The working tree this shadow belongs to.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    // -----------------------------------------------------------------------------------------
    // Taking snapshots
    // -----------------------------------------------------------------------------------------

    /// Records the working-tree contents of `paths` (files, symlinks; directories are
    /// recorded recursively, `.git` skipped). Paths that do not exist are recorded as
    /// absent, so restoring them deletes whatever is there later. Contents are exact bytes.
    ///
    /// For the shadow of a folder without git this is path-scoped: it is
    /// [`ShadowRepo::snapshot_files`] with [`FOLDER_MAX_FILE`] and [`Oversize::Skip`], so a
    /// directory is never walked (it is refused, as are FIFOs, sockets and devices) and a larger
    /// file is never stored (it is recorded as skipped).
    pub fn snapshot_paths(&self, paths: &[&str], note: &str) -> Result<ShadowSnapshot> {
        if self.folder {
            return self.snapshot_files(paths, FOLDER_MAX_FILE, Oversize::Skip, note);
        }
        let mut plan = Plan::new(SnapshotKind::Paths);
        for path in paths {
            let rel = check_rel(path)?;
            self.no_symlink_parents(&rel)?;
            self.collect(&rel, &mut plan, true, u64::MAX)?;
        }
        self.write_snapshot(plan, note)
    }

    /// Records the index entries (mode, blob, stage, intent-to-add flag) of `paths`; paths
    /// without an entry are recorded as "not in the index". Restore with
    /// [`ShadowRepo::restore_index`].
    pub fn snapshot_index(&self, paths: &[&str], note: &str) -> Result<ShadowSnapshot> {
        self.needs_repository()?;
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
        plan.intent_to_add = self.intent_to_add(&rels)?.into_iter().collect();
        for (rel, found) in seen {
            if !found {
                plan.no_index.push(rel);
            }
        }
        self.write_snapshot(plan, note)
    }

    /// Records the whole working tree: tracked and untracked files, minus what
    /// `.gitignore` (and `info/exclude`, global ignores) exclude. Used for agent
    /// checkpoints. Files over 256 MiB are skipped and recorded as such. Refused for the shadow
    /// of a folder without git (it never walks a folder).
    pub fn snapshot_worktree(&self, note: &str) -> Result<ShadowSnapshot> {
        self.needs_repository()?;
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

    /// What the snapshot recorded.
    pub fn kind(&self, snap: &ShadowSnapshot) -> Result<SnapshotKind> {
        Ok(self.manifest(snap)?.kind)
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

    // -----------------------------------------------------------------------------------------
    // Comparing
    // -----------------------------------------------------------------------------------------

    /// File changes from snapshot `a` to snapshot `b` (both `Paths` / `Worktree`
    /// snapshots), sorted by path. Only paths covered by both snapshots are compared,
    /// so a narrower snapshot never reports the rest as deleted. Sides are
    /// [`Side::Blob`]s living in the shadow repository: load them with
    /// [`ShadowRepo::file_at`] or [`ShadowRepo::load`]. A side the snapshot did not store
    /// (skipped) is [`Side::Workdir`] (the file now) for `b` and [`Side::None`] for `a`.
    ///
    /// Each covered path is in one of three states per snapshot: *absent*, *stored* (a file or
    /// link with its blob) or *skipped* (not stored: a file over the size limit, or not a regular
    /// file; the manifest does not say which, so both behave alike here). For an agent turn in a
    /// folder without git (ADR 0011), `a` is the before-state (first-touch backups, which are
    /// never skipped: a write whose backup would be is refused) and `b` the state at turn end;
    /// the turn's undo restores `a` for every reported path through the journal, which first
    /// backs the present state up (folder shadow: [`ShadowRepo::snapshot_paths`]: files over
    /// 64 MiB skipped, directories refused) and keeps that backup for redo:
    ///
    /// | before (`a`) | after (`b`) | reported | undo (check / force) | redo |
    /// |---|---|---|---|---|
    /// | absent | absent | nothing | – | – |
    /// | absent | file | Added | deletes the file | restores the file |
    /// | absent | large | Added | deletes it; check passes only when it is unchanged since turn end (hash, not stored); force refuses if it changed since (no backup possible); the undo says the file cannot be redone | refuses: not backed up |
    /// | absent | not a file | Added | refuses: a directory is never backed up or removed | – |
    /// | file | absent | Deleted | recreates the file | deletes it again |
    /// | file | file | Modified (if different) | restores the bytes | restores the turn's bytes |
    /// | file | large | Modified | restores the bytes; check / force as for absent → large | refuses: not backed up |
    /// | file | not a file | Modified | refuses (the path is now a directory) | – |
    /// | large or not a file | absent | Deleted | refuses: not backed up | – |
    /// | large or not a file | file | Modified | refuses: not backed up | – |
    /// | large or not a file | large or not a file | nothing (neither side stored) | – | – |
    ///
    /// An undo restores all of its paths or none (a refusal for one path writes nothing). Whole
    /// work-tree snapshots treat an unrecorded path as absent; their skipped files (over
    /// 256 MiB) follow the same rows.
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
        // Every path either snapshot says something about: stored, absent or skipped.
        let mut paths: BTreeSet<&String> = fa.keys().collect();
        paths.extend(fb.keys());
        paths.extend(ma.absent.iter().chain(&ma.skipped));
        paths.extend(mb.absent.iter().chain(&mb.skipped));
        let skipped = |m: &Manifest, path: &String| m.skipped.iter().any(|s| s == path);
        let mut changes = Vec::new();
        for path in paths {
            if !covered_a(path) || !covered_b(path) {
                continue;
            }
            let (skip_a, skip_b) = (skipped(&ma, path), skipped(&mb, path));
            let change = |kind, old: Side, new: Side| FileChange {
                old_path: (kind != ChangeKind::Added).then(|| path.clone()),
                new_path: (kind != ChangeKind::Deleted).then(|| path.clone()),
                kind,
                old,
                new,
                binary: None,
            };
            let now = || Side::Workdir(path.clone());
            let found = match (fa.get(path), fb.get(path)) {
                (Some((ma_, oa)), Some((mb_, ob))) => {
                    if oa == ob && ma_ == mb_ {
                        continue;
                    }
                    let kind = if (ma_ == "120000") != (mb_ == "120000") {
                        ChangeKind::TypeChange
                    } else {
                        ChangeKind::Modified
                    };
                    change(kind, Side::Blob(*oa), Side::Blob(*ob))
                }
                // Not stored by `b`: still there, changed.
                (Some((_, oa)), None) if skip_b => {
                    change(ChangeKind::Modified, Side::Blob(*oa), now())
                }
                (Some((_, oa)), None) => change(ChangeKind::Deleted, Side::Blob(*oa), Side::None),
                (None, Some((_, ob))) if skip_a => {
                    change(ChangeKind::Modified, Side::None, Side::Blob(*ob))
                }
                (None, Some((_, ob))) => change(ChangeKind::Added, Side::None, Side::Blob(*ob)),
                // Neither stored: absent -> skipped is a creation (it grew past the limit or is
                // not a file), skipped -> absent a deletion, skipped -> skipped not comparable.
                (None, None) => match (skip_a, skip_b) {
                    (false, true) => change(ChangeKind::Added, Side::None, now()),
                    (true, false) => change(ChangeKind::Deleted, Side::None, Side::None),
                    _ => continue,
                },
            };
            changes.push(found);
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

    /// Per-file fingerprints of the current working-tree state of `paths`: `"<mode> <blob id>"`
    /// (no filters, like a snapshot would record it) or `"absent"`. Directories expand to
    /// the files below them; requested paths that do not exist map to `"absent"`. The
    /// journal stores this after a write and compares it before an undo to notice which
    /// files changed in the meantime. Nothing is written.
    pub fn fingerprints(&self, paths: &[&str]) -> Result<BTreeMap<String, String>> {
        let mut rels: BTreeSet<String> = BTreeSet::new();
        let mut special: BTreeSet<String> = BTreeSet::new();
        for path in paths {
            let rel = check_rel(path)?;
            self.no_symlink_parents(&rel)?;
            let abs = self.workdir.join(&rel);
            let meta = fs::symlink_metadata(&abs).ok();
            if self.folder
                && meta
                    .as_ref()
                    .is_some_and(|m| !m.is_file() && !m.is_symlink())
            {
                // Path-scoped: a directory (or FIFO, device) is one opaque entry, never walked.
                special.insert(rel);
            } else if meta.is_some_and(|m| m.is_dir()) {
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
        let mut out: BTreeMap<String, String> = rels
            .into_iter()
            .zip(state)
            .map(|(rel, st)| {
                let value = match st {
                    Some((mode, oid)) => format!("{mode} {oid}"),
                    None => "absent".to_owned(),
                };
                (rel, value)
            })
            .collect();
        out.extend(
            special
                .into_iter()
                .map(|rel| (rel, "not a file".to_owned())),
        );
        Ok(out)
    }

    /// One stable fingerprint for [`ShadowRepo::fingerprints`] of `paths`.
    pub fn fingerprint_worktree(&self, paths: &[&str]) -> Result<String> {
        let map = self.fingerprints(paths)?;
        Ok(fnv128(
            map.iter().flat_map(|(k, v)| [k.as_bytes(), v.as_bytes()]),
        ))
    }

    /// Per-path fingerprints of the *index* entries of `paths` (all stages, mode and blob
    /// id, intent-to-add flag), `"absent"` for requested paths without an entry. Compare with
    /// a stored map before an index undo.
    pub fn index_fingerprints(&self, paths: &[&str]) -> Result<BTreeMap<String, String>> {
        self.needs_repository()?;
        let mut rels = Vec::new();
        for path in paths {
            rels.push(check_rel(path)?);
        }
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        let mut covered: BTreeSet<String> = BTreeSet::new();
        let ita = self.intent_to_add(&rels)?;
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
                let mut item = format!("{} {} {}", entry.mode, entry.oid, entry.stage);
                if ita.contains(&entry.path) {
                    item.push_str(" ita");
                }
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
    ///
    /// **All or nothing.** It restores every requested path to the snapshot's recorded state,
    /// or fails as a whole, naming the paths it cannot restore, before writing anything. A path
    /// cannot be restored when the snapshot does not cover it, when the snapshot recorded it (or
    /// a file below it) as *skipped* (not stored: too large, or not a regular file; for every
    /// snapshot kind, so an unavailable backup is never a successful no-op), when it is now a
    /// folder, FIFO, socket or device (never replaced or removed), or when a parent is now a file
    /// or a link. The files are then staged next to their destinations and renamed into place,
    /// so a failure while reading or writing data changes nothing; only a failing rename or
    /// delete (the last step) can leave the restore partly done, and the error says so.
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
        let mut not_backed_up: Vec<String> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        for path in paths {
            let rel = check_rel(path)?;
            if self.no_symlink_parents(&rel).is_err() {
                blocked.push(format!("{rel} is now reached through a link"));
                continue;
            }
            let mut found = false;
            for (name, (mode, oid)) in &files {
                if is_within(name, &rel) {
                    found = true;
                    writes.push((name.clone(), mode.clone(), *oid));
                }
            }
            if manifest.skipped.iter().any(|s| is_within(s, &rel)) {
                not_backed_up.push(rel.clone());
                continue;
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
        if !not_backed_up.is_empty() {
            return Err(Error::Io(io::Error::other(format!(
                "{} was not backed up (too large, or not a regular file), so it cannot be restored from this backup; nothing was restored",
                not_backed_up.join(", ")
            ))));
        }
        // A path restored from a file and deleted at the same time can't be both.
        writes.sort();
        writes.dedup();
        deletes.sort();
        deletes.dedup();
        for rel in writes.iter().map(|(r, ..)| r).chain(&deletes) {
            if self.no_symlink_parents(rel).is_err() {
                blocked.push(format!("{rel} is now reached through a link"));
            } else if let Some(why) = self.blocks_restore(rel) {
                blocked.push(why);
            }
        }
        if !blocked.is_empty() {
            blocked.sort();
            blocked.dedup();
            return Err(Error::Io(io::Error::other(format!(
                "{}; move it away to restore; nothing was restored",
                blocked.join("; ")
            ))));
        }

        // Stage every file, then put them in place.
        let oids: Vec<Oid> = writes.iter().map(|(_, _, o)| *o).collect();
        let mut staged: Vec<(PathBuf, String)> = Vec::with_capacity(writes.len());
        let staging = self.cat_blobs(&oids, |i, reader, size| {
            let (rel, mode, _) = &writes[i];
            staged.push((self.stage_restored(rel, mode, reader, size)?, rel.clone()));
            Ok(())
        });
        if staging.is_err() || staged.len() != writes.len() {
            for (tmp, rel) in &staged {
                let _ = fs::remove_file(tmp);
                self.remove_empty_parents(&self.workdir.join(rel));
            }
            return staging.and(Err(Error::Git(
                "the backup's contents could not be read; nothing was restored".into(),
            )));
        }
        let mut done = 0usize;
        let partly = |done: usize, err: Error| {
            if done == 0 {
                err
            } else {
                Error::Io(io::Error::other(format!(
                    "the restore stopped after {done} of {} paths ({err}); the rest is unchanged",
                    writes.len() + deletes.len()
                )))
            }
        };
        for (i, (tmp, rel)) in staged.iter().enumerate() {
            if let Err(err) = self.commit_restored(tmp, rel) {
                for (tmp, _) in &staged[i..] {
                    let _ = fs::remove_file(tmp);
                }
                return Err(partly(done, err));
            }
            done += 1;
        }
        for rel in &deletes {
            let abs = self.workdir.join(rel);
            // On a case-insensitive or normalising file system `A.txt` (recorded absent) can be
            // the file just restored as `a.txt`: removing it would undo the restore.
            if writes
                .iter()
                .any(|(w, ..)| util::same_entry(&abs, &self.workdir.join(w)))
            {
                done += 1;
                continue;
            }
            match fs::remove_file(&abs) {
                Ok(()) => self.remove_empty_parents(&abs),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(partly(done, err.into())),
            }
            done += 1;
        }
        Ok(())
    }

    /// Puts the recorded index entries of `paths` back (`git update-index --index-info`
    /// in the user's repository; the working tree is untouched). The entries' blobs
    /// already exist in the user's object database — they were staged there. **Writes the
    /// user's index — call it only from an explicit undo.**
    pub fn restore_index(&self, snap: &ShadowSnapshot, paths: &[&str]) -> Result<()> {
        self.needs_repository()?;
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
        // Intent-to-add entries come back with `git add -N` (update-index cannot set the
        // flag); one whose file is gone from the working tree comes back as a plain entry.
        let mut ita: Vec<&str> = Vec::new();
        for entry in manifest
            .index
            .iter()
            .filter(|e| rels.iter().any(|r| is_within(&e.path, r)))
        {
            if entry.stage == 0
                && manifest.intent_to_add.contains(&entry.path)
                && fs::symlink_metadata(self.workdir.join(&entry.path)).is_ok()
            {
                ita.push(&entry.path);
                continue;
            }
            input.extend_from_slice(
                format!(
                    "{} {} {}\t{}",
                    entry.mode, entry.oid, entry.stage, entry.path
                )
                .as_bytes(),
            );
            input.push(0);
        }
        if !input.is_empty() {
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
            exec(cmd, "git update-index", Some(&input))?;
        }
        for chunk in ita.chunks(500) {
            let mut args = vec!["add", "--force", "--intent-to-add", "--"];
            args.extend(chunk.iter().copied());
            let mut cmd = self.user_git(&args);
            cmd.env_remove("GIT_OPTIONAL_LOCKS");
            exec(cmd, "git add --intent-to-add", None)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Fetching pull requests (v0.6, Agent FORGE)
    // -----------------------------------------------------------------------------------------

    /// Fetches `refspecs` from `url` into the shadow repository (never into the user's
    /// repository). Destinations must be under `refs/lector/` (and not the snapshot
    /// namespace). Objects the user's repository already has are not downloaded again
    /// (alternates); received objects stay packed in the shadow.
    pub fn fetch(&self, url: &str, refspecs: &[&str]) -> Result<()> {
        self.fetch_with(url, refspecs, &FetchOptions::default())
    }

    /// [`ShadowRepo::fetch`] with options (an HTTP `Authorization` header for private
    /// repositories, passed through the environment so it never shows in `ps`).
    pub fn fetch_with(&self, url: &str, refspecs: &[&str], opts: &FetchOptions) -> Result<()> {
        if url.is_empty() || url.starts_with('-') {
            return Err(Error::Git(format!("invalid fetch URL {url:?}")));
        }
        if refspecs.is_empty() {
            return Err(Error::Git("nothing to fetch".into()));
        }
        for spec in refspecs {
            let spec = spec.strip_prefix('+').unwrap_or(spec);
            let dst = spec.split_once(':').map(|(_, d)| d).unwrap_or("");
            if spec.starts_with('-')
                || !dst.starts_with("refs/lector/")
                || dst.starts_with("refs/lector/snapshots")
            {
                return Err(Error::Git(format!(
                    "refspec {spec:?} must store into refs/lector/ (not snapshots)"
                )));
            }
        }
        let _lock = self.lock()?;
        let mut cmd = self.shadow_git();
        cmd.args([
            // Keep what arrives packed: unpacking would go through unpack-objects,
            // which freshens objects that also exist in the alternates.
            "-c",
            "fetch.unpackLimit=1",
            "-c",
            "transfer.unpackLimit=1",
            "-c",
            "fetch.writeCommitGraph=false",
            "-c",
            "fetch.recurseSubmodules=false",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-gc",
            "--no-auto-maintenance",
            "--no-recurse-submodules",
        ])
        .arg(url)
        .args(refspecs);
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        if let Some(header) = &opts.http_header {
            // The header goes to the URL's host only: no redirects (which could lead elsewhere).
            cmd.env("GIT_CONFIG_COUNT", "2")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", header)
                .env("GIT_CONFIG_KEY_1", "http.followRedirects")
                .env("GIT_CONFIG_VALUE_1", "false");
        }
        exec(cmd, "git fetch", None).map_err(|e| match (&opts.http_header, e) {
            // Never echo the header back in an error.
            (
                Some(h),
                Error::GitCommand {
                    command,
                    status,
                    stderr,
                },
            ) => Error::GitCommand {
                command,
                status,
                stderr: stderr.replace(h.as_str(), "<redacted>"),
            },
            (_, e) => e,
        })?;
        Ok(())
    }

    /// Whether git ignores the work-tree path `rel` (`git check-ignore`), i.e. whether
    /// [`ShadowRepo::snapshot_worktree`] leaves it out. Errors count as "not ignored", and so
    /// does every path of a folder without git (no git runs there).
    pub fn is_ignored(&self, rel: &str) -> bool {
        if self.folder {
            return false;
        }
        let Ok(rel) = check_rel(rel) else {
            return false;
        };
        // Not `user_git`: check-ignore rejects `--literal-pathspecs` (paths are taken
        // literally by this command anyway).
        Command::new("git")
            .arg("-C")
            .arg(&self.workdir)
            .args(["check-ignore", "-q", "--no-index", "--"])
            .arg(&rel)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Resolves a ref or commit-ish of the shadow (which also sees the user's objects).
    pub fn resolve(&self, rev: &str) -> Result<Option<Oid>> {
        if rev.starts_with('-') {
            return Ok(None);
        }
        let mut cmd = self.shadow_git();
        cmd.args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{rev}^{{commit}}"));
        match exec(cmd, "git rev-parse", None) {
            Ok(out) => Ok(String::from_utf8_lossy(&out).trim().parse().ok()),
            Err(Error::GitCommand { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The merge base of two commits of the shadow.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<Oid>> {
        if a.starts_with('-') || b.starts_with('-') {
            return Ok(None);
        }
        let mut cmd = self.shadow_git();
        cmd.args(["merge-base", a, b]);
        match exec(cmd, "git merge-base", None) {
            Ok(out) => Ok(String::from_utf8_lossy(&out).trim().parse().ok()),
            Err(Error::GitCommand { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Housekeeping
    // -----------------------------------------------------------------------------------------

    /// Deletes snapshots older than `keep` and trims to [`DEFAULT_MAX_BYTES`].
    pub fn prune(&self, keep: Duration) -> Result<()> {
        self.prune_capped(keep, DEFAULT_MAX_BYTES)
    }

    /// [`ShadowRepo::prune`] with another size cap. Trimming for size never removes a snapshot
    /// taken in the last [`SIZE_TRIM_GRACE`]: another session's running agent turn (or one that
    /// just ended) keeps its backups even while the shadow is over the cap, so its undo stays
    /// possible; age-based pruning (`keep`) is not affected.
    pub fn prune_capped(&self, keep: Duration, max_bytes: u64) -> Result<()> {
        self.prune_impl(
            PruneOptions {
                keep,
                max_bytes: Some(max_bytes),
            },
            now_secs() - SIZE_TRIM_GRACE.as_secs() as i64,
        )
        .map(drop)
    }

    /// Deletes snapshots older than `opts.keep`, then the oldest ones until the shadow's
    /// objects fit in `opts.max_bytes` (the newest snapshot always stays), then
    /// repacks and drops unreachable objects. Only the shadow repository is touched.
    pub fn prune_with(&self, opts: PruneOptions) -> Result<PruneReport> {
        self.prune_impl(opts, i64::MAX)
    }

    /// [`ShadowRepo::prune_with`]; size trimming only removes snapshots taken at or before
    /// `trimmable_until` (Unix seconds).
    fn prune_impl(&self, opts: PruneOptions, trimmable_until: i64) -> Result<PruneReport> {
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
            loop {
                let trimmable = snaps.iter().filter(|s| s.time <= trimmable_until).count();
                if bytes <= cap || snaps.len() <= 1 || trimmable == 0 {
                    break;
                }
                // The trimmable snapshots are the oldest: at the back.
                let batch = (trimmable / 4).max(1).min(trimmable).min(snaps.len() - 1);
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
    /// Entries whose blob is already in the shadow: (path, mode, blob) ([`ShadowRepo::merge_snapshots`]).
    recorded: Vec<(String, String, Oid)>,
    absent: Vec<String>,
    skipped: Vec<String>,
    /// Files that disappeared between stat and read (recorded absent).
    vanished: Vec<String>,
    /// Files larger than this when opened are not stored ([`Plan::oversize`] says what then).
    max_file: u64,
    oversize: Oversize,
    index: Vec<IndexRecord>,
    no_index: Vec<String>,
    /// Index entries that are intent-to-add (`git add -N`), a flag `ls-files -s` does not show.
    intent_to_add: Vec<String>,
}

impl Plan {
    fn new(kind: SnapshotKind) -> Plan {
        Plan {
            kind,
            files: Vec::new(),
            recorded: Vec::new(),
            absent: Vec::new(),
            skipped: Vec::new(),
            vanished: Vec::new(),
            max_file: u64::MAX,
            oversize: Oversize::Skip,
            index: Vec::new(),
            no_index: Vec::new(),
            intent_to_add: Vec::new(),
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
        for path in &self.intent_to_add {
            tokens.push(b"n".to_vec());
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
    intent_to_add: Vec<String>,
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
            intent_to_add: Vec::new(),
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
                "n" => manifest
                    .intent_to_add
                    .push(tokens.next().ok_or_else(|| bad("n"))?),
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

#[cfg(test)]
mod tests;
