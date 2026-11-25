//! Backups that keep every agent write undoable (ADR 0010 item 6, ADR 0011).
//!
//! **Not a registry command.** Agent file writes are the known second write path beside the
//! registry (ADR 0010, "Not decided here"); this module is their backup half. It lives with the
//! command modules because they are the code that takes backups through the `lector-git` write
//! API (`arch/git-callers.allow`); `lector-agent` calls it and never that API itself.
//!
//! The rule, for one agent session in one project:
//!
//! * **In a repository** a turn starts with a whole-worktree checkpoint
//!   ([`AgentBackups::begin_turn`], `ShadowRepo::snapshot_worktree`). A write to a file the
//!   checkpoint covers needs nothing more. A file it leaves out (git-ignored, such as `.env`)
//!   is backed up on its first touch in the turn (`ShadowRepo::snapshot_paths`).
//! * **In a folder without git** nothing is created when the folder is opened or the agent starts.
//!   Every file is backed up on its first touch in the turn, path by path, into the folder's
//!   shadow repository (`ShadowRepo::open_folder`), which is created at that first backup. The
//!   folder is never listed or walked (it may be a home directory): the cost of a turn depends on
//!   the files it touches, not on the folder. A file larger than [`MAX_FOLDER_BACKUP`] is not
//!   backed up and its write is refused.
//! * **When the turn ends** ([`AgentBackups::finish_turn`]) its "before" state is the checkpoint
//!   and the first-touch backups combined (`ShadowRepo::merge_snapshots`), its "after" state the
//!   same paths recorded again, and its undo one [`UndoRecord::RestoreFiles`] over the changed
//!   paths with their fingerprints, so the journal's check / force / redo apply unchanged.
//! * A write whose backup cannot be taken is refused ([`AgentBackups::before_write`] returns
//!   the reason for the agent).

use std::path::{Path, PathBuf};

use lector_git::{FileChange, Repo, ShadowRepo, ShadowSnapshot};
use parking_lot::Mutex;

use super::git_write::trim_once;
use super::pull_request::shadow_data_dir;
use crate::core::AppCore;
use crate::journal::UndoRecord;
use crate::project::Project;

/// Files larger than this are not backed up in a folder without git (ADR 0011); a write to such a
/// file, or a write of more than this, is refused there.
pub const MAX_FOLDER_BACKUP: u64 = 64 * 1024 * 1024;

/// What one turn has backed up so far: the checkpoint (repository) and the first-touch backups.
/// Kept by the session for the running turn and handed to [`AgentBackups::before_write`] and
/// [`AgentBackups::finish_turn`].
#[derive(Debug, Clone, Default)]
pub struct TurnLedger {
    note: String,
    /// The whole-worktree checkpoint taken when the turn started (repository only).
    pub checkpoint: Option<ShadowSnapshot>,
    /// (shadow-relative path, its backup) in the order of first touch.
    first_touch: Vec<(String, ShadowSnapshot)>,
}

impl TurnLedger {
    /// Shadow-relative paths backed up on first touch, in order.
    pub fn touched(&self) -> impl Iterator<Item = &str> {
        self.first_touch.iter().map(|(p, _)| p.as_str())
    }

    fn has(&self, rel: &str) -> bool {
        self.first_touch.iter().any(|(p, _)| p == rel)
    }
}

/// The backups of a finished turn that changed files.
#[derive(Debug, Clone)]
pub struct TurnBackup {
    /// The state before the turn (the checkpoint and the first-touch backups combined).
    pub before: ShadowSnapshot,
    /// The state when the turn ended (the same coverage).
    pub after: ShadowSnapshot,
    /// What changed between the two (shadow-relative paths).
    pub changes: Vec<FileChange>,
    /// Restores the changed paths to `before`; `None` when nothing changed.
    pub undo: Option<UndoRecord>,
}

/// The backups of one agent session's project. Cheap to create: nothing is opened or created
/// until a backup is taken.
pub struct AgentBackups {
    data_dir: PathBuf,
    project: Project,
    repo: Option<Repo>,
    shadow: Mutex<Option<ShadowRepo>>,
}

impl std::fmt::Debug for AgentBackups {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBackups")
            .field("root", &self.project.root())
            .field("folder", &self.repo.is_none())
            .finish()
    }
}

impl AgentBackups {
    /// The backups of `project` (shadow repositories below the core's data directory). Creates
    /// nothing.
    pub fn new(core: &AppCore, project: &Project) -> AgentBackups {
        AgentBackups {
            data_dir: shadow_data_dir(core),
            project: project.clone(),
            repo: project.repo().filter(|r| !r.is_bare()).cloned(),
            shadow: Mutex::new(None),
        }
    }

    /// `true` when the project is a folder without git (path-scoped backups only).
    pub fn in_folder(&self) -> bool {
        self.repo.is_none()
    }

    /// The directory backups are relative to and journal entries name as their `repo`: the
    /// repository's work tree, or the folder itself.
    pub fn workdir(&self) -> &Path {
        match &self.repo {
            Some(repo) => repo.workdir(),
            None => self.project.root(),
        }
    }

    /// The shadow-relative spelling of a project-relative path (they differ when the project is
    /// a subfolder of its repository).
    pub fn shadow_path(&self, rel: &str) -> String {
        match &self.repo {
            Some(_) => self.project.repo_path(rel),
            None => rel.to_owned(),
        }
    }

    /// Opens (creating on first use) the shadow to back up into, trimmed once per process.
    fn store(&self) -> Result<ShadowRepo, lector_git::Error> {
        let mut slot = self.shadow.lock();
        if let Some(shadow) = slot.as_ref() {
            return Ok(shadow.clone());
        }
        let shadow = match &self.repo {
            Some(repo) => ShadowRepo::open(&self.data_dir, repo)?,
            None => ShadowRepo::open_folder(&self.data_dir, self.project.root())?,
        };
        trim_once(&self.data_dir, &shadow);
        *slot = Some(shadow.clone());
        Ok(shadow)
    }

    /// Starts a turn: in a repository the "before" checkpoint of the whole work tree (`None`
    /// in the ledger when it fails; writes are then backed up path by path); in a folder without
    /// git nothing at all (backups happen on first touch).
    pub fn begin_turn(&self, note: &str) -> TurnLedger {
        let mut ledger = TurnLedger {
            note: note.to_owned(),
            ..TurnLedger::default()
        };
        if self.repo.is_some() {
            match self.store().and_then(|s| s.snapshot_worktree(note)) {
                Ok(snap) => ledger.checkpoint = Some(snap),
                Err(err) => tracing::warn!("agent checkpoint failed: {err}"),
            }
        }
        ledger
    }

    /// Makes the write of `new_len` bytes to the project-relative `rel` (already confined)
    /// undoable before it happens. Within a turn (`ledger`), a file the checkpoint covers or one
    /// already touched in the turn needs nothing; otherwise the file is backed up and, within a
    /// turn, remembered as touched. Returns the backup taken for this write, if one was.
    ///
    /// `Err` is the reason the write must be refused (the backup failed, or the file is too large
    /// to back up in a folder without git), worded for the agent.
    pub fn before_write(
        &self,
        ledger: Option<&mut TurnLedger>,
        rel: &str,
        new_len: usize,
    ) -> Result<Option<ShadowSnapshot>, String> {
        let shadow_rel = self.shadow_path(rel);
        if let Some(ledger) = ledger.as_deref() {
            if ledger.has(&shadow_rel) {
                return Ok(None);
            }
            if ledger.checkpoint.is_some() {
                let ignored = self.store().is_ok_and(|s| s.is_ignored(&shadow_rel));
                if !ignored {
                    return Ok(None);
                }
            }
        }
        let failed = |e: &dyn std::fmt::Display| {
            format!("{rel}: the backup that makes this write undoable failed ({e})")
        };
        if self.repo.is_none() {
            let current = std::fs::symlink_metadata(self.project.absolute_path(rel))
                .map(|m| m.len())
                .unwrap_or(0);
            if current > MAX_FOLDER_BACKUP || new_len as u64 > MAX_FOLDER_BACKUP {
                return Err(format!(
                    "{rel}: Lector does not back up files larger than {} MiB in a folder without git, so this write could not be undone and was refused",
                    MAX_FOLDER_BACKUP / (1024 * 1024)
                ));
            }
        }
        let shadow = self.store().map_err(|e| failed(&e))?;
        let snap = shadow
            .snapshot_paths(&[shadow_rel.as_str()], "Before an agent write")
            .map_err(|e| failed(&e))?;
        if let Some(ledger) = ledger {
            ledger.first_touch.push((shadow_rel, snap.clone()));
        }
        Ok(Some(snap))
    }

    /// Fingerprints of the project-relative `rel` now (the "after" state of a single write's
    /// undo record); empty when there is no shadow.
    pub fn fingerprints_now(&self, rel: &str) -> std::collections::BTreeMap<String, String> {
        let shadow_rel = self.shadow_path(rel);
        self.store()
            .and_then(|s| s.fingerprints(&[shadow_rel.as_str()]))
            .unwrap_or_default()
    }

    /// Ends a turn: records the "after" state (the whole work tree when there is a checkpoint,
    /// plus every first-touch path), combines the "before" state, compares them and builds the
    /// turn's undo. `None` when the turn backed nothing up (a folder turn that wrote nothing) or
    /// the "after" state could not be recorded. In a folder without git only the touched paths
    /// are recorded.
    pub fn finish_turn(&self, ledger: &TurnLedger, note: &str) -> Option<TurnBackup> {
        let touched: Vec<&str> = ledger.touched().collect();
        if ledger.checkpoint.is_none() && touched.is_empty() {
            return None;
        }
        let shadow = self.store().ok()?;
        let warn = |what: &str, err: lector_git::Error| {
            tracing::warn!("agent turn backup: cannot {what}: {err}");
        };
        let parts: Vec<&ShadowSnapshot> = ledger.first_touch.iter().map(|(_, s)| s).collect();
        let (before, after) = match &ledger.checkpoint {
            Some(checkpoint) => {
                let whole = shadow
                    .snapshot_worktree(note)
                    .map_err(|e| warn("record the work tree", e))
                    .ok()?;
                if touched.is_empty() {
                    (checkpoint.clone(), whole)
                } else {
                    let paths = shadow
                        .snapshot_paths(&touched, note)
                        .map_err(|e| warn("record the touched files", e))
                        .ok()?;
                    let mut before_parts = vec![checkpoint];
                    before_parts.extend(parts.iter().copied());
                    let before = shadow
                        .merge_snapshots(&before_parts, &ledger.note)
                        .map_err(|e| warn("combine the backups", e))
                        .ok()?;
                    let after = shadow
                        .merge_snapshots(&[&whole, &paths], note)
                        .map_err(|e| warn("combine the turn's state", e))
                        .ok()?;
                    (before, after)
                }
            }
            None => {
                let before = shadow
                    .merge_snapshots(&parts, &ledger.note)
                    .map_err(|e| warn("combine the backups", e))
                    .ok()?;
                let after = shadow
                    .snapshot_paths(&touched, note)
                    .map_err(|e| warn("record the touched files", e))
                    .ok()?;
                (before, after)
            }
        };
        let changes = shadow
            .changes_between(&before, &after)
            .map_err(|e| warn("compare the turn's states", e))
            .unwrap_or_default();
        let undo = (!changes.is_empty()).then(|| {
            let paths: Vec<String> = changes.iter().map(|c| c.path().to_owned()).collect();
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            let after = shadow.fingerprints(&refs).unwrap_or_default();
            UndoRecord::RestoreFiles {
                shadow: before.clone(),
                paths,
                after,
            }
        });
        Some(TurnBackup {
            before,
            after,
            changes,
            undo,
        })
    }
}
