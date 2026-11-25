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
    /// The shadow repository of an entry's `repo` directory: the repository whose work tree it
    /// is, else the backups of that folder without git (ADR 0011; found without creating
    /// anything), else (an older entry naming a subfolder) the repository around it.
    fn shadow_for(&self, dir: &Path) -> Result<ShadowRepo, UndoError> {
        let repo = Repo::discover(dir).ok().filter(|r| !r.is_bare());
        let canonical = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let open = |repo: &Repo| {
            ShadowRepo::open(self.data_dir(), repo)
                .map_err(|e| UndoError::BackupGone(e.to_string()))
        };
        match &repo {
            Some(repo) if canonical(repo.workdir()) == canonical(dir) => open(repo),
            _ => match ShadowRepo::existing_folder(self.data_dir(), dir) {
                Some(shadow) => Ok(shadow),
                None => match &repo {
                    Some(repo) => open(repo),
                    None => Err(UndoError::BackupGone(format!(
                        "there are no backups for {}",
                        dir.display()
                    ))),
                },
            },
        }
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
        let mut invoked: Vec<u64> = Vec::new();
        let mut not_redoable: Vec<String> = Vec::new();
        self.apply(
            &entry,
            &record,
            repo.as_deref(),
            cx,
            &mut redo,
            &mut invoked,
            &mut not_redoable,
        )?;

        // The entry for the undo itself: the inverse command's own entry when the record was a
        // single inverse command (the registry journaled it), else a new `journal.undo` entry
        // carrying the redo of every part. Inverse commands that ran as part of a larger record
        // are folded into that redo (their own entries become parts of this undo), so redo
        // repeats all of the operation, and "Undo last" does not pick one part up on its own.
        let single_invoke = matches!(record, UndoRecord::Invoke { .. });
        let undo_entry_id = match invoked.as_slice() {
            [inv] if single_invoke => {
                let inv = *inv;
                self.update(inv, |e| e.undo_of = Some(id));
                inv
            }
            _ => {
                for inv in &invoked {
                    self.update(*inv, |e| {
                        e.undo_of = Some(id);
                        e.undo = None;
                    });
                }
                let undo = match redo.len() {
                    0 => None,
                    1 => redo.pop(),
                    _ => Some(UndoRecord::Multi { records: redo }),
                };
                self.append(ActivityEntry {
                    id: 0,
                    time: SystemTime::now(),
                    command: "journal.undo".into(),
                    summary: if not_redoable.is_empty() {
                        format!("Undid: {}", entry.summary)
                    } else {
                        format!(
                            "Undid: {} ({} cannot be redone: too large for Lector to back up)",
                            entry.summary,
                            describe_paths(&not_redoable)
                        )
                    },
                    repo: entry.repo.clone(),
                    paths: entry.paths.clone(),
                    undo,
                    undone: false,
                    args: if not_redoable.is_empty() {
                        serde_json::json!({ "entry": id })
                    } else {
                        serde_json::json!({ "entry": id, "not_redoable": not_redoable })
                    },
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
    /// repository, or of its folder when that is not a git repository (any, without a project).
    pub fn undo_last(
        &self,
        cx: &CommandContext,
        mode: UndoMode,
    ) -> Result<ActivityEntry, UndoError> {
        let workdir = cx.project.as_ref().map(|p| match p.repo() {
            Some(r) => r.workdir().to_path_buf(),
            None => p.root().to_path_buf(),
        });
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

    /// Runs the record's writes (last first for `Multi`), collecting redo records and the paths
    /// whose present state could not be backed up first (`not_redoable`).
    ///
    /// The shadow of a folder without git does not store files over 64 MiB (ADR 0011). Undoing
    /// over such a file (overwriting or deleting it) is allowed only while it is exactly as the
    /// operation left it (its fingerprint, a hash taken without storing, matches the record's
    /// `after`), in [`UndoMode::Check`] and [`UndoMode::Force`] alike: then nothing anyone made
    /// since is lost, the undo restores the before-state, and the redo of that file is not
    /// possible (the undo's summary and `args.not_redoable` say so). If it changed since, the
    /// undo refuses with the reason and writes nothing, because the change could not be backed up.
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        entry: &ActivityEntry,
        record: &UndoRecord,
        repo: Option<&Path>,
        cx: &CommandContext,
        redo: &mut Vec<UndoRecord>,
        invoked: &mut Vec<u64>,
        not_redoable: &mut Vec<String>,
    ) -> Result<(), UndoError> {
        let fail = |e: lector_git::Error| UndoError::Failed(e.to_string());
        match record {
            UndoRecord::RestoreFiles {
                shadow: snap,
                paths,
                after: left,
            } => {
                let shadow = self.shadow_for(repo.unwrap_or(Path::new("")))?;
                let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
                // Back up what the restore is about to overwrite: this is what makes
                // "Restore anyway" and the undo itself undoable.
                let backup = shadow
                    .snapshot_paths(&refs, &format!("Before undoing: {}", entry.summary))
                    .map_err(fail)?;
                let unsaved = unsaved_paths(&shadow, &backup).map_err(fail)?;
                if !unsaved.is_empty() {
                    let keys: Vec<&str> = unsaved.iter().map(String::as_str).collect();
                    let now = shadow.fingerprints(&keys).map_err(fail)?;
                    let changed: Vec<String> = unsaved
                        .iter()
                        .filter(|p| left.get(*p).is_none() || left.get(*p) != now.get(*p))
                        .cloned()
                        .collect();
                    if !changed.is_empty() {
                        return Err(UndoError::Failed(format!(
                            "{} changed since this operation and is too large for Lector to back up, so undoing would destroy that change with no way back; nothing was restored",
                            describe_paths(&changed)
                        )));
                    }
                    not_redoable.extend(unsaved);
                }
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
                invoked.extend(outcome.journal_id);
                // The inverse command's own undo is the redo of this part.
                redo.extend(outcome.undo);
            }
            UndoRecord::Multi { records } => {
                // Undo runs the parts last first and collects each part's redo in that order;
                // a `Multi` of those redos (undone last first again) then repeats the
                // original parts first to last.
                for r in records.iter().rev() {
                    self.apply(entry, r, repo, cx, redo, invoked, not_redoable)?;
                }
            }
        }
        Ok(())
    }
}

/// Paths a backup did not store (the shadow of a folder without git skips files over 64 MiB);
/// whole-repository shadows never skip in a path backup, so they are not asked.
fn unsaved_paths(
    shadow: &ShadowRepo,
    backup: &lector_git::ShadowSnapshot,
) -> Result<Vec<String>, lector_git::Error> {
    if !shadow.is_folder() {
        return Ok(Vec::new());
    }
    Ok(shadow
        .entries(backup)?
        .into_iter()
        .filter_map(|e| match e {
            lector_git::shadow::SnapshotEntry::Skipped { path } => Some(path),
            _ => None,
        })
        .collect())
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
