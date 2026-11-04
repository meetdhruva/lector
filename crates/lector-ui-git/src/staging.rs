//! Git write operations run from the UI: stage / unstage / discard files or
//! hunks, off the main thread, followed by an immediate refresh of every git view.
//!
//! Every function here is the direct result of an explicit user action (button,
//! menu item, key binding); nothing writes on its own.

use gpui::{App, AppContext, Entity, Task};
use lector_diff::Algorithm;
use lector_git::{Changeset, FileChange, OperationKind, Repo};
use lector_workspace::review::{self, HunkAction, LineSelection};

use crate::store::{GitStore, GitStoreEvent};

/// One write operation.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // built once per user action
pub enum GitOp {
    /// `git add -A -- <paths>`.
    Stage(Vec<String>),
    /// `git restore --staged -- <paths>` (or `rm --cached` on an unborn branch).
    Unstage(Vec<String>),
    /// `git add -A`.
    StageAll,
    /// `git reset -q`.
    UnstageAll,
    /// Restore tracked paths from the index; delete untracked ones.
    Discard(Vec<String>),
    /// Stage / unstage / discard a hunk or line selection of one file.
    Hunk {
        /// `Unstaged` or `Staged`.
        changeset: Changeset,
        /// The file.
        change: FileChange,
        /// What to do.
        action: HunkAction,
        /// Rows of the file's diff.
        selection: LineSelection,
    },
    /// `git <op> --continue` (rebase, cherry-pick, revert, am; merge commits
    /// go through the commit composer).
    Continue(OperationKind),
    /// `git <op> --abort` (confirmed by the caller).
    Abort(OperationKind),
}

impl GitOp {
    /// Verb for error messages ("Staging failed").
    pub fn verb(&self) -> &'static str {
        match self {
            GitOp::Stage(_) | GitOp::StageAll => "Staging",
            GitOp::Unstage(_) | GitOp::UnstageAll => "Unstaging",
            GitOp::Discard(_) => "Discarding",
            GitOp::Continue(_) => "Continuing",
            GitOp::Abort(_) => "Aborting",
            GitOp::Hunk { action, .. } => match action {
                HunkAction::Stage => "Staging",
                HunkAction::Unstage => "Unstaging",
                HunkAction::Discard => "Discarding",
            },
        }
    }

    /// Runs the operation (blocking). `Ok(false)` when there was nothing to do.
    pub fn run(&self, repo: &Repo) -> Result<bool, String> {
        fn paths(p: &[String]) -> Vec<&str> {
            p.iter().map(String::as_str).collect()
        }
        let r = match self {
            GitOp::Stage(p) => repo.stage_paths(&paths(p)).map(|_| true),
            GitOp::Unstage(p) => repo.unstage_paths(&paths(p)).map(|_| true),
            GitOp::StageAll => repo.stage_all().map(|_| true),
            GitOp::UnstageAll => repo.unstage_all().map(|_| true),
            GitOp::Discard(p) => repo.discard_paths(&paths(p)).map(|_| true),
            GitOp::Continue(kind) => repo.continue_operation(*kind).map(|_| true),
            GitOp::Abort(kind) => repo.abort_operation(*kind).map(|_| true),
            GitOp::Hunk {
                changeset,
                change,
                action,
                selection,
            } => {
                return review::apply_hunk_action(
                    repo,
                    changeset,
                    change,
                    *action,
                    selection,
                    Algorithm::Histogram,
                )
                .map_err(|e| workspace_error_text(&e));
            }
        };
        r.map_err(|e| git_error_text(&e))
    }
}

/// git's own message for a failed command (stderr verbatim), else the error.
pub fn git_error_text(e: &lector_git::Error) -> String {
    match e {
        lector_git::Error::GitCommand { stderr, .. } if !stderr.is_empty() => stderr.clone(),
        other => other.to_string(),
    }
}

/// [`git_error_text`] through a workspace error.
pub fn workspace_error_text(e: &lector_workspace::Error) -> String {
    match e {
        lector_workspace::Error::Git(g) => git_error_text(g),
        other => other.to_string(),
    }
}

/// Tells every git view that the repository changed now (even when `git status`
/// looks the same, e.g. after staging one more hunk of a partially staged file),
/// then re-reads HEAD and status.
pub fn notify_repo_changed(store: &Entity<GitStore>, cx: &mut App) {
    store.update(cx, |s, cx| {
        s.generation += 1;
        cx.emit(GitStoreEvent::Changed);
        cx.notify();
        s.refresh(cx);
    });
}

/// Runs `op` in the background, then refreshes the git views. Resolves to the
/// error text on failure (callers show it inline).
pub fn run_op(store: &Entity<GitStore>, op: GitOp, cx: &mut App) -> Task<Result<bool, String>> {
    let Some(repo) = store.read(cx).repo().cloned() else {
        return Task::ready(Err("Not a git repository".into()));
    };
    let store = store.clone();
    let work = cx.background_spawn(async move {
        let verb = op.verb();
        op.run(&repo).map_err(|e| format!("{verb} failed:\n{e}"))
    });
    cx.spawn(async move |cx| {
        let result = work.await;
        let _ = cx.update(|cx| notify_repo_changed(&store, cx));
        result
    })
}

/// Paths to unstage for a staged change (both sides of a rename).
pub fn unstage_paths_for(change: &FileChange) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in [&change.old_path, &change.new_path].into_iter().flatten() {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}
