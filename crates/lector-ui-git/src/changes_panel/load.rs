//! Assembling the sections of the working-tree view.

use std::collections::HashSet;

use lector_git::{ChangeKind, FileChange, Side};
use lector_review::review::{ChangesetView, ReviewFile};

/// The Conflicts section: the unstaged files that are conflicted, plus a placeholder row for
/// each conflicted path git does not list as unstaged. The conflicted files are removed from
/// `unstaged`.
pub(super) fn conflict_section(
    unstaged: &mut ChangesetView,
    conflicted: &HashSet<&str>,
) -> ChangesetView {
    let mut conflicts = unstaged.clone();
    conflicts.files.retain(|f| conflicted.contains(f.path()));
    for path in conflicted {
        if conflicts.index_of(path).is_none() {
            let change = FileChange {
                old_path: Some(path.to_string()),
                new_path: Some(path.to_string()),
                kind: ChangeKind::Modified,
                old: Side::Index(path.to_string()),
                new: Side::Workdir(path.to_string()),
                binary: None,
            };
            conflicts.files.push(ReviewFile {
                display_path: path.to_string(),
                change,
                stats: None,
                viewed: false,
                content_hash: String::new(),
                binary: false,
            });
        }
    }
    unstaged.files.retain(|f| !conflicted.contains(f.path()));
    conflicts
}
