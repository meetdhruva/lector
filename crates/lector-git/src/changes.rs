//! [`Repo::changes`]: file lists for every [`Changeset`].

use gix::bstr::ByteSlice;
use gix::object::tree::diff::ChangeDetached;

use crate::error::{Error, Result, ResultExt};
use crate::repo::{head_tree_id, resolve_commit, resolve_tree};
use crate::status::{full_status, index_status, worktree_status, IndexChange, WorktreeChange};
use crate::{ChangeKind, Changeset, FileChange, Oid, Repo, Side};

impl Repo {
    /// The changed files of a [`Changeset`], sorted by path.
    ///
    /// Renames are detected for staged, commit, range and branch changesets
    /// (50% similarity, like git's default). [`FileChange::binary`] is left
    /// `None`; use [`Repo::detect_binary`] when a file is opened.
    pub fn changes(&self, cs: &Changeset) -> Result<Vec<FileChange>> {
        let repo = self.local();
        let mut out = match cs {
            Changeset::Unstaged => {
                self.require_workdir()?;
                worktree_changes(worktree_status(&repo, false)?)
            }
            Changeset::Untracked => {
                self.require_workdir()?;
                worktree_changes(
                    worktree_status(&repo, true)?
                        .into_iter()
                        .filter(|c| matches!(c, WorktreeChange::Untracked { .. }))
                        .collect(),
                )
            }
            Changeset::Staged => {
                self.require_workdir()?;
                index_status(&repo)?
                    .into_iter()
                    .map(staged_change)
                    .collect()
            }
            Changeset::WorkingTree => {
                let root = self.require_workdir()?.to_path_buf();
                working_tree_changes(&repo, &root)?
            }
            Changeset::Commit(oid) => {
                let commit = repo.find_commit(oid.to_gix()).git()?;
                let new_tree = commit.tree().git()?;
                let old_tree = match commit.parent_ids().next() {
                    Some(parent) => Some(repo.find_commit(parent.detach()).git()?.tree().git()?),
                    None => None,
                };
                tree_changes(&repo, old_tree.as_ref(), Some(&new_tree))?
            }
            Changeset::Range { from, to } => {
                let old_tree = resolve_tree(&repo, from)?;
                let new_tree = resolve_tree(&repo, to)?;
                tree_changes(&repo, Some(&old_tree), Some(&new_tree))?
            }
            Changeset::BranchVsBase { branch, base } => {
                let branch_id = resolve_commit(&repo, branch)?;
                let base_id = resolve_commit(&repo, base)?;
                let merge_base = repo
                    .merge_base(base_id, branch_id)
                    .git()?
                    .ok_or_else(|| Error::NoMergeBase(base.clone(), branch.clone()))?;
                let old_tree = repo.find_commit(merge_base.detach()).git()?.tree().git()?;
                let new_tree = repo.find_commit(branch_id).git()?.tree().git()?;
                tree_changes(&repo, Some(&old_tree), Some(&new_tree))?
            }
            Changeset::Snapshots { from, to } => {
                let shadow =
                    crate::shadow::known_shadow_with(self.workdir(), *from).ok_or_else(|| {
                        Error::Parse(format!(
                            "snapshot {from} is not in a known shadow repository"
                        ))
                    })?;
                let snap = |commit: Oid| crate::ShadowSnapshot {
                    commit,
                    time: 0,
                    note: String::new(),
                };
                shadow.changes_between(&snap(*from), &snap(*to))?
            }
        };
        out.sort_by(|a, b| a.path().cmp(b.path()));
        Ok(out)
    }
}

fn worktree_changes(changes: Vec<WorktreeChange>) -> Vec<FileChange> {
    changes
        .into_iter()
        .map(|change| match change {
            WorktreeChange::Tracked { path, kind } => match kind {
                ChangeKind::Deleted => FileChange {
                    old_path: Some(path.clone()),
                    new_path: None,
                    kind,
                    old: Side::Index(path),
                    new: Side::None,
                    binary: None,
                },
                ChangeKind::Added => FileChange {
                    old_path: None,
                    new_path: Some(path.clone()),
                    kind,
                    old: Side::None,
                    new: Side::Workdir(path),
                    binary: None,
                },
                _ => FileChange {
                    old_path: Some(path.clone()),
                    new_path: Some(path.clone()),
                    kind,
                    old: Side::Index(path.clone()),
                    new: Side::Workdir(path),
                    binary: None,
                },
            },
            WorktreeChange::Conflict { path } => FileChange {
                old_path: Some(path.clone()),
                new_path: Some(path.clone()),
                kind: ChangeKind::Modified,
                old: Side::Index(path.clone()),
                new: Side::Workdir(path),
                binary: None,
            },
            WorktreeChange::Untracked { path } => FileChange {
                old_path: None,
                new_path: Some(path.clone()),
                kind: ChangeKind::Added,
                old: Side::None,
                new: Side::Workdir(path),
                binary: None,
            },
        })
        .collect()
}

fn staged_change(change: IndexChange) -> FileChange {
    let old = change
        .old_id
        .map(|id| Side::Blob(Oid::from(id)))
        .unwrap_or(Side::None);
    match change.kind {
        ChangeKind::Added => FileChange {
            old_path: None,
            new_path: Some(change.path.clone()),
            kind: change.kind,
            old: Side::None,
            new: Side::Index(change.path),
            binary: None,
        },
        ChangeKind::Deleted => FileChange {
            old_path: Some(change.path),
            new_path: None,
            kind: change.kind,
            old,
            new: Side::None,
            binary: None,
        },
        _ => FileChange {
            old_path: Some(change.source.unwrap_or_else(|| change.path.clone())),
            new_path: Some(change.path.clone()),
            kind: change.kind,
            old,
            new: Side::Index(change.path),
            binary: None,
        },
    }
}

/// HEAD → worktree, derived from the full status: each touched path is compared
/// between its HEAD blob (following staged renames) and the file on disk.
fn working_tree_changes(repo: &gix::Repository, root: &std::path::Path) -> Result<Vec<FileChange>> {
    let (staged, worktree) = full_status(repo)?;
    let entries = crate::status::merge_entries(staged, worktree);
    let head_tree = match head_tree_id(repo)? {
        Some(id) => Some(repo.find_tree(id).git()?),
        None => None,
    };
    let head_blob = |path: &str| -> Result<Option<gix::ObjectId>> {
        let Some(tree) = &head_tree else {
            return Ok(None);
        };
        Ok(tree
            .lookup_entry_by_path(path)
            .git()?
            .filter(|e| !e.mode().is_tree())
            .map(|e| e.object_id()))
    };
    let mut out = Vec::new();
    for entry in entries {
        let on_disk = std::fs::symlink_metadata(root.join(&entry.path)).is_ok_and(|m| !m.is_dir());
        let renamed = entry.staged == Some(ChangeKind::Renamed);
        let old_path = entry
            .renamed_from
            .clone()
            .filter(|_| renamed)
            .unwrap_or_else(|| entry.path.clone());
        let old_id = head_blob(&old_path)?;
        let type_change = entry.staged == Some(ChangeKind::TypeChange)
            || entry.unstaged == Some(ChangeKind::TypeChange);
        let change = match (old_id, on_disk) {
            (Some(id), true) => FileChange {
                old_path: Some(old_path.clone()),
                new_path: Some(entry.path.clone()),
                kind: if old_path != entry.path {
                    ChangeKind::Renamed
                } else if type_change {
                    ChangeKind::TypeChange
                } else {
                    ChangeKind::Modified
                },
                old: Side::Blob(Oid::from(id)),
                new: Side::Workdir(entry.path.clone()),
                binary: None,
            },
            (Some(id), false) => FileChange {
                old_path: Some(old_path),
                new_path: None,
                kind: ChangeKind::Deleted,
                old: Side::Blob(Oid::from(id)),
                new: Side::None,
                binary: None,
            },
            (None, true) => FileChange {
                old_path: None,
                new_path: Some(entry.path.clone()),
                kind: ChangeKind::Added,
                old: Side::None,
                new: Side::Workdir(entry.path.clone()),
                binary: None,
            },
            (None, false) => continue,
        };
        out.push(change);
    }
    Ok(out)
}

/// Tree → tree diff with rename detection; only non-tree entries are reported.
pub(crate) fn tree_changes(
    repo: &gix::Repository,
    old: Option<&gix::Tree<'_>>,
    new: Option<&gix::Tree<'_>>,
) -> Result<Vec<FileChange>> {
    let options = gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));
    let changes = repo.diff_tree_to_tree(old, new, Some(options)).git()?;
    let path = |p: &gix::bstr::BString| p.to_str_lossy().into_owned();
    let mut out = Vec::with_capacity(changes.len());
    for change in changes {
        let fc = match change {
            ChangeDetached::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                FileChange {
                    old_path: None,
                    new_path: Some(path(&location)),
                    kind: ChangeKind::Added,
                    old: Side::None,
                    new: Side::Blob(id.into()),
                    binary: None,
                }
            }
            ChangeDetached::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                FileChange {
                    old_path: Some(path(&location)),
                    new_path: None,
                    kind: ChangeKind::Deleted,
                    old: Side::Blob(id.into()),
                    new: Side::None,
                    binary: None,
                }
            }
            ChangeDetached::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
            } => {
                if entry_mode.is_tree() && previous_entry_mode.is_tree() {
                    continue;
                }
                let kind = if previous_entry_mode.kind() == entry_mode.kind()
                    || (previous_entry_mode.is_blob() && entry_mode.is_blob())
                {
                    ChangeKind::Modified
                } else {
                    ChangeKind::TypeChange
                };
                FileChange {
                    old_path: Some(path(&location)),
                    new_path: Some(path(&location)),
                    kind,
                    old: if previous_entry_mode.is_tree() {
                        Side::None
                    } else {
                        Side::Blob(previous_id.into())
                    },
                    new: if entry_mode.is_tree() {
                        Side::None
                    } else {
                        Side::Blob(id.into())
                    },
                    binary: None,
                }
            }
            ChangeDetached::Rewrite {
                source_location,
                source_entry_mode,
                source_id,
                entry_mode,
                id,
                location,
                copy,
                ..
            } => {
                if entry_mode.is_tree() || source_entry_mode.is_tree() {
                    continue;
                }
                FileChange {
                    old_path: Some(path(&source_location)),
                    new_path: Some(path(&location)),
                    kind: if copy {
                        ChangeKind::Copied
                    } else {
                        ChangeKind::Renamed
                    },
                    old: Side::Blob(source_id.into()),
                    new: Side::Blob(id.into()),
                    binary: None,
                }
            }
        };
        out.push(fc);
    }
    Ok(out)
}
