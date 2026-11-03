//! `git status` and the working-tree based changesets, via gix's parallel status.
//!
//! Read-only: the status iterator collects index stat updates in memory
//! (`Outcome::write_changes`) but we never call it, so `.git/index` is untouched.

use std::collections::BTreeMap;

use gix::bstr::{BString, ByteSlice};
use gix::diff::index::ChangeRef;
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};

use crate::error::{Result, ResultExt};
use crate::{ChangeKind, Repo, StatusEntry};

/// A normalized index→worktree change.
#[derive(Clone, Debug)]
pub(crate) enum WorktreeChange {
    /// Tracked path changed in the worktree (`kind` is Modified, Deleted, TypeChange or Added for intent-to-add).
    Tracked { path: String, kind: ChangeKind },
    /// The path has conflict stages in the index.
    Conflict { path: String },
    /// An untracked file.
    Untracked { path: String },
}

/// A normalized HEAD→index change.
#[derive(Clone, Debug)]
pub(crate) struct IndexChange {
    pub path: String,
    pub kind: ChangeKind,
    /// Object id in HEAD (for modifications, deletions and rename sources).
    pub old_id: Option<gix::ObjectId>,
    pub source: Option<String>,
}

fn path_string(path: &[u8]) -> String {
    path.to_str_lossy().into_owned()
}

fn is_type_change(old: gix::index::entry::Mode, new: gix::index::entry::Mode) -> bool {
    use gix::index::entry::Mode;
    let class = |m: Mode| {
        if m.contains(Mode::SYMLINK) {
            1
        } else if m.contains(Mode::COMMIT) {
            2
        } else {
            0
        }
    };
    class(old) != class(new)
}

pub(crate) fn convert_index_change(change: &ChangeRef<'_, '_>) -> IndexChange {
    match change {
        ChangeRef::Addition { location, .. } => IndexChange {
            path: path_string(location),
            kind: ChangeKind::Added,
            old_id: None,
            source: None,
        },
        ChangeRef::Deletion { location, id, .. } => IndexChange {
            path: path_string(location),
            kind: ChangeKind::Deleted,
            old_id: Some(id.as_ref().to_owned()),
            source: None,
        },
        ChangeRef::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            ..
        } => IndexChange {
            path: path_string(location),
            kind: if is_type_change(*previous_entry_mode, *entry_mode) {
                ChangeKind::TypeChange
            } else {
                ChangeKind::Modified
            },
            old_id: Some(previous_id.as_ref().to_owned()),
            source: None,
        },
        ChangeRef::Rewrite {
            source_location,
            source_id,
            location,
            copy,
            ..
        } => IndexChange {
            path: path_string(location),
            kind: if *copy {
                ChangeKind::Copied
            } else {
                ChangeKind::Renamed
            },
            old_id: Some(source_id.as_ref().to_owned()),
            source: Some(path_string(source_location)),
        },
    }
}

pub(crate) fn convert_worktree_item(item: &WorktreeItem) -> Option<WorktreeChange> {
    match item {
        WorktreeItem::Modification {
            rela_path, status, ..
        } => {
            let path = path_string(rela_path);
            let kind = match status {
                EntryStatus::Conflict { .. } => return Some(WorktreeChange::Conflict { path }),
                EntryStatus::NeedsUpdate(_) => return None,
                EntryStatus::IntentToAdd => ChangeKind::Added,
                EntryStatus::Change(change) => match change {
                    Change::Removed => ChangeKind::Deleted,
                    Change::Type { .. } => ChangeKind::TypeChange,
                    Change::Modification {
                        executable_bit_changed,
                        content_change,
                        ..
                    } => {
                        if !executable_bit_changed && content_change.is_none() {
                            return None;
                        }
                        ChangeKind::Modified
                    }
                    Change::SubmoduleModification(_) => ChangeKind::Modified,
                },
            };
            Some(WorktreeChange::Tracked { path, kind })
        }
        WorktreeItem::DirectoryContents { entry, .. } => {
            if entry.status == gix::dir::entry::Status::Untracked
                && entry.disk_kind != Some(gix::dir::entry::Kind::Directory)
            {
                Some(WorktreeChange::Untracked {
                    path: path_string(&entry.rela_path),
                })
            } else {
                None
            }
        }
        // Index→worktree rename tracking is disabled; treat defensively as untracked.
        WorktreeItem::Rewrite { dirwalk_entry, .. } => Some(WorktreeChange::Untracked {
            path: path_string(&dirwalk_entry.rela_path),
        }),
    }
}

fn configured_platform(
    repo: &gix::Repository,
    untracked: bool,
) -> Result<gix::status::Platform<'_, gix::progress::Discard>> {
    let mode = if untracked {
        gix::status::UntrackedFiles::Files
    } else {
        gix::status::UntrackedFiles::None
    };
    let platform = repo
        .status(gix::progress::Discard)
        .git()?
        .untracked_files(mode)
        // Status must not depend on the process' current directory.
        .dirwalk_options(|opts| opts.empty_patterns_match_prefix(false))
        .index_worktree_rewrites(None)
        .index_worktree_submodules(gix::status::Submodule::Given {
            ignore: gix::submodule::config::Ignore::Dirty,
            check_dirty: false,
        })
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Given(
            gix::diff::Rewrites::default(),
        ));
    Ok(platform)
}

/// Run the full parallel status (HEAD→index and index→worktree incl. untracked).
pub(crate) fn full_status(
    repo: &gix::Repository,
) -> Result<(Vec<IndexChange>, Vec<WorktreeChange>)> {
    let mut staged = Vec::new();
    let mut worktree = Vec::new();
    let iter = configured_platform(repo, true)?
        .into_iter(Vec::<BString>::new())
        .git()?;
    for item in iter {
        match item.git()? {
            gix::status::Item::TreeIndex(change) => staged.push(convert_index_change(&change)),
            gix::status::Item::IndexWorktree(item) => {
                if let Some(change) = convert_worktree_item(&item) {
                    worktree.push(change);
                }
            }
        }
    }
    Ok((staged, worktree))
}

/// Index→worktree only (optionally with untracked files).
pub(crate) fn worktree_status(
    repo: &gix::Repository,
    untracked: bool,
) -> Result<Vec<WorktreeChange>> {
    let mut out = Vec::new();
    let iter = configured_platform(repo, untracked)?
        .into_index_worktree_iter(Vec::<BString>::new())
        .git()?;
    for item in iter {
        if let Some(change) = convert_worktree_item(&item.git()?) {
            out.push(change);
        }
    }
    Ok(out)
}

/// HEAD→index only.
pub(crate) fn index_status(repo: &gix::Repository) -> Result<Vec<IndexChange>> {
    let tree_id = repo.head_tree_id_or_empty().git()?;
    let index = repo.index_or_empty().git()?;
    let mut out = Vec::new();
    repo.tree_index_status(
        &tree_id,
        &index,
        None,
        gix::status::tree_index::TrackRenames::Given(gix::diff::Rewrites::default()),
        |change, _, _| {
            out.push(convert_index_change(&change));
            Ok(std::ops::ControlFlow::Continue(()))
        },
    )
    .git()?;
    Ok(out)
}

/// Merge staged and worktree changes into per-path status entries, sorted by path.
pub(crate) fn merge_entries(
    staged: Vec<IndexChange>,
    worktree: Vec<WorktreeChange>,
) -> Vec<StatusEntry> {
    let mut map: BTreeMap<String, StatusEntry> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for change in worktree {
        match change {
            WorktreeChange::Tracked { path, kind } => {
                map.entry(path.clone())
                    .or_insert_with(|| entry(path))
                    .unstaged = Some(kind);
            }
            WorktreeChange::Conflict { path } => conflicts.push(path),
            WorktreeChange::Untracked { path } => {
                map.entry(path.clone())
                    .or_insert_with(|| entry(path))
                    .untracked = true;
            }
        }
    }
    for change in staged {
        let e = map
            .entry(change.path.clone())
            .or_insert_with(|| entry(change.path));
        e.staged = Some(change.kind);
        e.renamed_from = change.source;
    }
    for path in conflicts {
        let e = map.entry(path.clone()).or_insert_with(|| entry(path));
        e.conflicted = true;
        e.staged = None;
        e.unstaged = None;
        e.renamed_from = None;
    }
    map.into_values().collect()
}

fn entry(path: String) -> StatusEntry {
    StatusEntry {
        path,
        ..StatusEntry::default()
    }
}

impl Repo {
    /// The working tree status: staged and unstaged changes of tracked files,
    /// untracked files (individually, not collapsed into directories) and
    /// conflicts. Staged renames are detected. Sorted by path.
    ///
    /// Runs the HEAD→index diff, the index→worktree check and the directory walk
    /// in parallel. Never writes the index (no stat refresh write-back).
    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        self.require_workdir()?;
        let repo = self.local();
        let (staged, worktree) = full_status(&repo)?;
        Ok(merge_entries(staged, worktree))
    }
}
