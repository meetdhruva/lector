//! Executing an [`UndoRecord`]: restore from the shadow, invoke the inverse command,
//! check that nothing changed since, and record the undo itself.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use lector_git::{Repo, ShadowRepo};

use super::{ActivityEntry, Journal, UndoRecord};
use crate::commands::CommandContext;

/// How strictly [`Journal::undo`] checks that the files are still as the command left them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoMode {
    /// Refuse with [`UndoError::Changed`] when files changed since.
    Check,
    /// "Restore anyway": overwrite regardless. The current state is backed up first,
    /// so this is undoable too.
    Force,
}

/// Whether an entry can be undone now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoStatus {
    /// The Undo button is enabled.
    Available,
    /// Disabled; the reason is the tooltip.
    Unavailable(String),
}

impl UndoStatus {
    /// `true` for [`UndoStatus::Available`].
    pub fn is_available(&self) -> bool {
        matches!(self, UndoStatus::Available)
    }
}

/// Why an undo did not happen. Nothing was written in any of these cases.
#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    /// No such journal entry.
    #[error("that activity entry no longer exists")]
    NotFound(u64),
    /// Already undone (by the entry with this id, if known).
    #[error("this operation was already undone")]
    AlreadyUndone(u64),
    /// The operation has no undo record; the text says why.
    #[error("{0}")]
    NotUndoable(String),
    /// Files changed after the operation. The UI turns this into a choice: cancel, or
    /// "Restore anyway" ([`UndoMode::Force`]).
    #[error("{} changed since this operation; undoing would overwrite those changes", describe_paths(.paths))]
    Changed {
        /// The entry being undone.
        id: u64,
        /// Repository-relative paths that differ from what the operation left.
        paths: Vec<String>,
    },
    /// The backup was cleaned up (shadow repository pruned) or is unreadable.
    #[error("the backup for this operation is gone ({0})")]
    BackupGone(String),
    /// The undo ran into an error.
    #[error("{0}")]
    Failed(String),
}

fn describe_paths(paths: &[String]) -> String {
    match paths {
        [] => "Files".to_owned(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, rest @ ..] => format!("{a} and {} other files", rest.len()),
    }
}

fn no_undo_reason(command: &str) -> String {
    match command {
        "git.commit" => "Commits cannot be undone from here yet. Use `git reset --soft HEAD~1` in a terminal to take the last commit back.".into(),
        "git.abort_operation" => "Aborting an operation cannot be undone.".into(),
        "git.continue_operation" => "Continuing an operation cannot be undone from here.".into(),
        _ => "This operation has no undo.".into(),
    }
}

impl Journal {
    fn shadow_for(&self, repo: &Path) -> Result<ShadowRepo, UndoError> {
        let repo = Repo::discover(repo).map_err(|e| UndoError::Failed(e.to_string()))?;
        ShadowRepo::open(self.data_dir(), &repo).map_err(|e| UndoError::BackupGone(e.to_string()))
    }

    /// Whether `entry` can be undone now, with the reason when it cannot (already undone,
    /// no undo record, backup pruned).
    pub fn undo_status(&self, entry: &ActivityEntry) -> UndoStatus {
        if entry.undone {
            return UndoStatus::Unavailable("Already undone".into());
        }
        let Some(record) = &entry.undo else {
            return UndoStatus::Unavailable(no_undo_reason(&entry.command));
        };
        let snaps = record.snapshots();
        if !snaps.is_empty() {
            let Some(repo) = &entry.repo else {
                return UndoStatus::Unavailable("The repository of this entry is unknown".into());
            };
            match self.shadow_for(repo) {
                Ok(shadow) => {
                    if snaps.iter().any(|s| !shadow.contains(s)) {
                        return UndoStatus::Unavailable(
                            "The backup for this operation has been cleaned up".into(),
                        );
                    }
                }
                Err(err) => return UndoStatus::Unavailable(err.to_string()),
            }
        }
        UndoStatus::Available
    }

    /// Undoes the entry `id`: restores what the operation overwrote (from the shadow
    /// repository) or invokes the inverse command, then records the undo as a new entry
    /// (itself undoable: redo) and marks `id` undone. Returns the new entry.
    ///
    /// With [`UndoMode::Check`], files that changed since the operation make it fail with
    /// [`UndoError::Changed`] before anything is written.
    pub fn undo(
        &self,
        id: u64,
        cx: &CommandContext,
        mode: UndoMode,
    ) -> Result<ActivityEntry, UndoError> {
        let _serial = self.inner.undo_lock.lock();
        let entry = self.get(id).ok_or(UndoError::NotFound(id))?;
        if entry.undone {
            return Err(UndoError::AlreadyUndone(entry.undone_by.unwrap_or(0)));
        }
        let record = entry
            .undo
            .clone()
            .ok_or_else(|| UndoError::NotUndoable(no_undo_reason(&entry.command)))?;
        let repo = entry.repo.clone();

        if mode == UndoMode::Check {
            self.check(id, &record, repo.as_deref())?;
        } else {
            self.check_snapshots(&record, repo.as_deref())?;
        }
        let mut redo: Vec<UndoRecord> = Vec::new();
        let mut invoked: Option<u64> = None;
        self.apply(
            &entry,
            &record,
            repo.as_deref(),
            cx,
            &mut redo,
            &mut invoked,
        )?;

        // The entry for the undo itself: the inverse command's own entry when one ran
        // (the registry journaled it), else a new `journal.undo` entry carrying the redo.
        let undo_entry_id = match (invoked, redo.is_empty()) {
            (Some(inv), true) => {
                self.update(inv, |e| e.undo_of = Some(id));
                inv
            }
            _ => {
                let undo = match redo.len() {
                    0 => None,
                    1 => redo.pop(),
                    _ => Some(UndoRecord::Multi { records: redo }),
                };
                self.append(ActivityEntry {
                    id: 0,
                    time: SystemTime::now(),
                    command: "journal.undo".into(),
                    summary: format!("Undid: {}", entry.summary),
                    repo: entry.repo.clone(),
                    paths: entry.paths.clone(),
                    undo,
                    undone: false,
                    args: serde_json::json!({ "entry": id }),
                    source: cx.source,
                    undo_of: Some(id),
                    undone_by: None,
                })
                .ok_or_else(|| {
                    UndoError::Failed("the undo worked but could not be recorded".into())
                })?
            }
        };
        self.update(id, |e| {
            e.undone = true;
            e.undone_by = Some(undo_entry_id);
        });
        // Undoing an undo (redo) makes the original operation current again.
        if let Some(original) = entry.undo_of {
            self.update(original, |e| {
                e.undone = false;
                e.undone_by = None;
            });
        }
        self.get(undo_entry_id)
            .ok_or_else(|| UndoError::Failed("the undo entry vanished".into()))
    }

    /// Undoes the newest undoable operation ("Undo last operation") of the context's
    /// repository (any, without a project).
    pub fn undo_last(
        &self,
        cx: &CommandContext,
        mode: UndoMode,
    ) -> Result<ActivityEntry, UndoError> {
        let workdir = cx
            .project
            .as_ref()
            .and_then(|p| p.repo())
            .map(|r| r.workdir().to_path_buf());
        let entry = self
            .last_undoable(workdir.as_deref())
            .ok_or_else(|| UndoError::NotUndoable("Nothing to undo.".into()))?;
        self.undo(entry.id, cx, mode)
    }

    fn check_snapshots(&self, record: &UndoRecord, repo: Option<&Path>) -> Result<(), UndoError> {
        let snaps = record.snapshots();
        if snaps.is_empty() {
            return Ok(());
        }
        let repo = repo.ok_or_else(|| UndoError::Failed("the entry has no repository".into()))?;
        let shadow = self.shadow_for(repo)?;
        for snap in snaps {
            if !shadow.contains(snap) {
                return Err(UndoError::BackupGone(
                    "the shadow repository was pruned".into(),
                ));
            }
        }
        Ok(())
    }

    /// Verifies backups exist and that the files still match what the operation left.
    fn check(&self, id: u64, record: &UndoRecord, repo: Option<&Path>) -> Result<(), UndoError> {
        self.check_snapshots(record, repo)?;
        let mut changed: Vec<String> = Vec::new();
        self.collect_changed(record, repo, &mut changed)?;
        if changed.is_empty() {
            Ok(())
        } else {
            changed.sort();
            changed.dedup();
            Err(UndoError::Changed { id, paths: changed })
        }
    }

    fn collect_changed(
        &self,
        record: &UndoRecord,
        repo: Option<&Path>,
        out: &mut Vec<String>,
    ) -> Result<(), UndoError> {
        match record {
            UndoRecord::RestoreFiles { after, .. } if !after.is_empty() => {
                let shadow = self.shadow_for(repo.unwrap_or(Path::new("")))?;
                let keys: Vec<&str> = after.keys().map(String::as_str).collect();
                let now = shadow
                    .fingerprints(&keys)
                    .map_err(|e| UndoError::Failed(e.to_string()))?;
                diff_maps(after, &now, out);
            }
            UndoRecord::RestoreIndex { after, .. } if !after.is_empty() => {
                let shadow = self.shadow_for(repo.unwrap_or(Path::new("")))?;
                let keys: Vec<&str> = after.keys().map(String::as_str).collect();
                let now = shadow
                    .index_fingerprints(&keys)
                    .map_err(|e| UndoError::Failed(e.to_string()))?;
                diff_maps(after, &now, out);
            }
            UndoRecord::Multi { records } => {
                for r in records {
                    self.collect_changed(r, repo, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Runs the record's writes (last first for `Multi`), collecting redo records.
    fn apply(
        &self,
        entry: &ActivityEntry,
        record: &UndoRecord,
        repo: Option<&Path>,
        cx: &CommandContext,
        redo: &mut Vec<UndoRecord>,
        invoked: &mut Option<u64>,
    ) -> Result<(), UndoError> {
        let fail = |e: lector_git::Error| UndoError::Failed(e.to_string());
        match record {
            UndoRecord::RestoreFiles {
                shadow: snap,
                paths,
                ..
            } => {
                let shadow = self.shadow_for(repo.unwrap_or(Path::new("")))?;
                let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
                // Back up what the restore is about to overwrite: this is what makes
                // "Restore anyway" and the undo itself undoable.
                let backup = shadow
                    .snapshot_paths(&refs, &format!("Before undoing: {}", entry.summary))
                    .map_err(fail)?;
                shadow.restore_paths(snap, &refs).map_err(fail)?;
                let after = shadow.fingerprints(&refs).map_err(fail)?;
                redo.push(UndoRecord::RestoreFiles {
                    shadow: backup,
                    paths: paths.clone(),
                    after,
                });
            }
            UndoRecord::RestoreIndex {
                shadow: snap,
                paths,
                ..
            } => {
                let shadow = self.shadow_for(repo.unwrap_or(Path::new("")))?;
                let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
                let backup = shadow
                    .snapshot_index(&refs, &format!("Before undoing: {}", entry.summary))
                    .map_err(fail)?;
                shadow.restore_index(snap, &refs).map_err(fail)?;
                let after = shadow.index_fingerprints(&refs).map_err(fail)?;
                redo.push(UndoRecord::RestoreIndex {
                    shadow: backup,
                    paths: paths.clone(),
                    after,
                });
            }
            UndoRecord::Invoke { command, args } => {
                let cx = cx.clone();
                let project_repo = cx
                    .project
                    .as_ref()
                    .and_then(|p| p.repo())
                    .map(|r| r.workdir().to_path_buf());
                if let (Some(wanted), Some(have)) = (repo, project_repo.as_deref()) {
                    if wanted != have {
                        return Err(UndoError::Failed(format!(
                            "open {} to undo this operation",
                            wanted.display()
                        )));
                    }
                }
                let outcome = cx
                    .core
                    .registry()
                    .invoke(command, args.clone(), &cx)
                    .map_err(|e| UndoError::Failed(e.to_string()))?;
                *invoked = outcome.journal_id;
            }
            UndoRecord::Multi { records } => {
                for r in records.iter().rev() {
                    self.apply(entry, r, repo, cx, redo, invoked)?;
                }
                // Redo replays in the original order: reverse what we collected.
                redo.reverse();
            }
        }
        Ok(())
    }
}

fn diff_maps(
    expected: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
    out: &mut Vec<String>,
) {
    for (path, value) in expected {
        if now.get(path) != Some(value) {
            out.push(path.clone());
        }
    }
}
