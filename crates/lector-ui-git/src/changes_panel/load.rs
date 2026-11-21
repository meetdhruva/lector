//! Assembling the sections of the working-tree view, and the cached default base of the
//! current branch.

use std::collections::HashSet;

use gpui::{Context, Task};
use lector_git::{ChangeKind, FileChange, Side};
use lector_review::review::{ChangesetView, ReviewFile};
use lector_ui::background::{background, view_key, JobKind, Priority};

/// The default base of the current branch ("Branch vs main" in the mode menu). It runs git, so
/// it is computed through the background bridge whenever the repository state changes, and the
/// menu only reads the cached value (never git on the UI thread).
#[derive(Default)]
pub(super) struct BaseCache {
    /// The branch it was computed for and its base.
    known: Option<(String, Option<String>)>,
    task: Option<Task<()>>,
}

impl BaseCache {
    /// The cached base of `branch`; `None` while unknown or computed for another branch.
    pub(super) fn base_for(&self, branch: Option<&str>) -> Option<String> {
        match (&self.known, branch) {
            (Some((known, base)), Some(branch)) if known == branch => base.clone(),
            _ => None,
        }
    }
}

impl super::ChangesPanel {
    /// Recomputes the default base of the current branch off the UI thread.
    pub(super) fn refresh_default_base(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let branch = store.head.as_ref().and_then(|h| h.branch.clone());
        let (Some(repo), Some(branch)) = (store.repo().cloned(), branch) else {
            self.base = BaseCache::default();
            return;
        };
        let review = lector_ui::commands_bridge::core(cx).review();
        let key = view_key(JobKind::GitLookup, cx.entity_id());
        let b = branch.clone();
        let work = background(cx, key, Priority::Visible, move |cancel| {
            review.default_base(&repo, &b, cancel)
        });
        self.base.task = Some(cx.spawn(async move |this, cx| {
            let Ok(base) = work.await else { return };
            let _ = this.update(cx, |this, _| this.base.known = Some((branch, base)));
        }));
    }
}

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

#[cfg(test)]
mod base_tests {
    use super::BaseCache;

    #[test]
    fn the_cached_base_answers_only_for_the_branch_it_was_computed_for() {
        let mut cache = BaseCache::default();
        assert_eq!(
            cache.base_for(Some("feature")),
            None,
            "unknown yet: no entry"
        );
        cache.known = Some(("feature".into(), Some("main".into())));
        assert_eq!(cache.base_for(Some("feature")).as_deref(), Some("main"));
        assert_eq!(
            cache.base_for(Some("other")),
            None,
            "switched branch: stale"
        );
        assert_eq!(cache.base_for(None), None, "detached HEAD");
    }
}
