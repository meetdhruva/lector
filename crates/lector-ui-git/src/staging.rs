//! Git write operations run from the UI: stage / unstage / discard files or
//! hunks, off the main thread, followed by an immediate refresh of every git view.
//!
//! Every function here is the direct result of an explicit user action (button,
//! menu item, key binding); nothing writes on its own. Since v0.4 each operation is a
//! registry command (`lector_app::commands::git_write`), so it is journaled in the
//! activity log. Destructive ones (discard) run immediately after Lector backed up what
//! will be lost, and show an "Undo" toast instead of asking first; only when the backup
//! cannot be taken does a confirmation come up.

use gpui::{App, AppContext, Entity, SharedString, Task};
use lector_app::commands::git_write::BackupUnavailable;
use lector_app::commands::{CommandContext, CommandError, Source};
use lector_app::journal::Journal;
use lector_git::{Changeset, FileChange, OperationKind};
use lector_workspace::review::{self, HunkAction, LineSelection};
use serde_json::{json, Value};

use crate::activity;
use crate::confirm::{confirm, ConfirmSpec};
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
    /// Restore tracked paths from the index; delete untracked ones (backed up first).
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
        /// [`lector_workspace::review::FileDiff::content_id`] of the diff the
        /// selection was made in: the op refuses when the file changed since.
        content_id: String,
    },
    /// Resolve a modify/delete conflict by deleting the file and staging the deletion
    /// (backed up first).
    ResolveDelete(String),
    /// `git <op> --continue` (rebase, cherry-pick, revert, am; merge commits
    /// go through the commit composer).
    Continue(OperationKind),
    /// `git <op> --abort` (confirmed by the caller; irreversible).
    Abort(OperationKind),
}

fn ranges_json(ranges: &[std::ops::Range<u32>]) -> Value {
    Value::Array(ranges.iter().map(|r| json!([r.start, r.end])).collect())
}

impl GitOp {
    /// Verb for error messages ("Staging failed").
    pub fn verb(&self) -> &'static str {
        match self {
            GitOp::Stage(_) | GitOp::StageAll => "Staging",
            GitOp::Unstage(_) | GitOp::UnstageAll => "Unstaging",
            GitOp::Discard(_) | GitOp::ResolveDelete(_) => "Discarding",
            GitOp::Continue(_) => "Continuing",
            GitOp::Abort(_) => "Aborting",
            GitOp::Hunk { action, .. } => match action {
                HunkAction::Stage => "Staging",
                HunkAction::Unstage => "Unstaging",
                HunkAction::Discard => "Discarding",
            },
        }
    }

    /// Whether the operation throws changes away (and so shows an Undo toast).
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            GitOp::Discard(_)
                | GitOp::ResolveDelete(_)
                | GitOp::Hunk {
                    action: HunkAction::Discard,
                    ..
                }
        )
    }

    /// The registry command id and arguments that perform this operation.
    pub fn command(&self, allow_without_backup: bool) -> (&'static str, Value) {
        match self {
            GitOp::Stage(p) => ("git.stage", json!({ "paths": p })),
            GitOp::Unstage(p) => ("git.unstage", json!({ "paths": p })),
            GitOp::StageAll => ("git.stage_all", json!({})),
            GitOp::UnstageAll => ("git.unstage_all", json!({})),
            GitOp::Discard(p) => (
                "git.discard",
                json!({ "paths": p, "allow_without_backup": allow_without_backup }),
            ),
            GitOp::ResolveDelete(p) => (
                "git.resolve_delete",
                json!({ "path": p, "allow_without_backup": allow_without_backup }),
            ),
            GitOp::Continue(kind) => ("git.continue_operation", json!({ "kind": kind.command() })),
            GitOp::Abort(kind) => ("git.abort_operation", json!({ "kind": kind.command() })),
            GitOp::Hunk {
                changeset,
                change,
                action,
                selection,
                content_id,
            } => {
                let id = match action {
                    HunkAction::Stage => "git.stage_hunk",
                    HunkAction::Unstage => "git.unstage_hunk",
                    HunkAction::Discard => "git.discard_hunk",
                };
                (
                    id,
                    json!({
                        "path": change.path(),
                        "changeset": review::changeset_key(changeset),
                        "old_lines": ranges_json(&selection.old),
                        "new_lines": ranges_json(&selection.new),
                        "content_id": content_id,
                        "allow_without_backup": allow_without_backup,
                    }),
                )
            }
        }
    }

    /// Paths named in a "discard without a backup?" confirmation.
    fn confirm_items(&self) -> Vec<SharedString> {
        match self {
            GitOp::Discard(p) => p.iter().cloned().map(SharedString::from).collect(),
            GitOp::ResolveDelete(p) => vec![p.clone().into()],
            GitOp::Hunk { change, .. } => vec![change.path().to_string().into()],
            _ => Vec::new(),
        }
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

/// The text a failed command shows inline: git's stderr verbatim when git failed.
pub fn command_error_text(e: &CommandError) -> String {
    match e {
        CommandError::Failed(err) => {
            if let Some(g) = err.downcast_ref::<lector_git::Error>() {
                git_error_text(g)
            } else if let Some(w) = err.downcast_ref::<lector_workspace::Error>() {
                workspace_error_text(w)
            } else {
                err.to_string()
            }
        }
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
        // Every window on this repository reloads too.
        s.refresh_and_notify(cx);
    });
}

/// Runs `op` in the background as a registry command (journaled), then refreshes the git
/// views. Resolves to the error text on failure (callers show it inline); `Ok(false)`
/// when there was nothing to do or when a confirmation was opened instead (no backup).
pub fn run_op(store: &Entity<GitStore>, op: GitOp, cx: &mut App) -> Task<Result<bool, String>> {
    run_op_with(store, op, false, cx)
}

fn run_op_with(
    store: &Entity<GitStore>,
    op: GitOp,
    allow_without_backup: bool,
    cx: &mut App,
) -> Task<Result<bool, String>> {
    let project = match store.read(cx).project() {
        Some(p) if p.repo().is_some() => p.clone(),
        _ => return Task::ready(Err("Not a git repository".into())),
    };
    let core = lector_ui::commands_bridge::core(cx);
    let (id, args) = op.command(allow_without_backup);
    let work = {
        let core = core.clone();
        cx.background_spawn(async move {
            // The journal records every write command; idempotent.
            Journal::install(&core);
            let ctx = CommandContext::new(core.clone(), Some(project), Source::Ui);
            core.registry().invoke(id, args, &ctx)
        })
    };
    let store = store.clone();
    cx.spawn(async move |cx| {
        let result = work.await;
        let _ = cx.update(|cx| notify_repo_changed(&store, cx));
        match result {
            Ok(outcome) => {
                let applied = !outcome.summary.starts_with("Nothing");
                if op.is_destructive() && outcome.undo.is_some() {
                    if let Some(entry_id) = outcome.journal_id {
                        let summary = outcome.summary.clone();
                        let store = store.clone();
                        let _ = cx
                            .update(|cx| activity::show_undo_toast(&store, entry_id, summary, cx));
                    }
                }
                Ok(applied)
            }
            Err(CommandError::Failed(err))
                if !allow_without_backup && err.downcast_ref::<BackupUnavailable>().is_some() =>
            {
                let reason = err.to_string();
                let store = store.clone();
                let _ = cx.update(|cx| confirm_without_backup(&store, op, reason, cx));
                Ok(false)
            }
            Err(err) => Err(format!(
                "{} failed:\n{}",
                op.verb(),
                command_error_text(&err)
            )),
        }
    })
}

/// The one confirmation that remains for discards: the backup that would make them
/// undoable could not be taken.
fn confirm_without_backup(store: &Entity<GitStore>, op: GitOp, reason: String, cx: &mut App) {
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let Some(window) = cx.active_window() else {
        return;
    };
    let store = store.clone();
    let spec = ConfirmSpec {
        title: "Discard without a backup?".into(),
        message: vec![
            SharedString::from(reason),
            "Without a backup this cannot be undone.".into(),
        ],
        items: op.confirm_items(),
        confirm_label: "Discard".into(),
    };
    let _ = window.update(cx, move |_, window, cx| {
        let ws = ws.clone();
        confirm(&ws, spec, window, cx, move |_, cx| {
            let task = run_op_with(&store, op, true, cx);
            let store = store.clone();
            cx.spawn(async move |cx| {
                if let Err(message) = task.await {
                    let _ = cx.update(|cx| {
                        if let Some(ws) = store.read(cx).workspace.upgrade() {
                            ws.update(cx, |ws, cx| ws.show_toast(message, cx));
                        }
                    });
                }
            })
            .detach();
        });
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use lector_app::{AppCore, CoreOptions};
    use lector_ui::actions::workspace::UndoLastOperation;
    use lector_ui::views::Workspace;
    use lector_workspace::Project;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn commands_map_to_registry_ids_and_arguments() {
        let (id, args) = GitOp::Stage(vec!["a.rs".into()]).command(false);
        assert_eq!(id, "git.stage");
        assert_eq!(args, json!({ "paths": ["a.rs"] }));

        let (id, args) = GitOp::Discard(vec!["a.rs".into(), "b.rs".into()]).command(true);
        assert_eq!(id, "git.discard");
        assert_eq!(
            args,
            json!({ "paths": ["a.rs", "b.rs"], "allow_without_backup": true })
        );

        let (id, args) = GitOp::ResolveDelete("a.rs".into()).command(false);
        assert_eq!(id, "git.resolve_delete");
        assert_eq!(
            args,
            json!({ "path": "a.rs", "allow_without_backup": false })
        );

        assert_eq!(GitOp::StageAll.command(false).0, "git.stage_all");
        assert_eq!(GitOp::UnstageAll.command(false).0, "git.unstage_all");
        assert_eq!(
            GitOp::Continue(OperationKind::Rebase).command(false).0,
            "git.continue_operation"
        );
        assert_eq!(
            GitOp::Abort(OperationKind::Merge).command(false).0,
            "git.abort_operation"
        );
    }

    #[test]
    fn only_discards_are_destructive() {
        assert!(GitOp::Discard(vec![]).is_destructive());
        assert!(GitOp::ResolveDelete("a".into()).is_destructive());
        assert!(!GitOp::Stage(vec![]).is_destructive());
        assert!(!GitOp::UnstageAll.is_destructive());
        assert!(!GitOp::Abort(OperationKind::Merge).is_destructive());
    }

    #[gpui::test]
    fn discard_runs_immediately_shows_an_undo_toast_and_undo_restores_the_file(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let data = tempfile::tempdir().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nmy precious edit\n").unwrap();
        std::fs::write(root.join("new.txt"), "untracked work\n").unwrap();

        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        settings.session.restore = false;
        let core = AppCore::open(CoreOptions {
            data_dir: Some(data.path().to_path_buf()),
            ..CoreOptions::for_tests()
        });
        cx.update(|cx| {
            lector_ui::ThemeState::init(
                cx,
                lector_theme::ThemeRegistry::new(),
                lector_ui::ThemeMode::Dark,
                lector_theme::Appearance::Dark,
            );
            cx.set_global(lector_ui::code_view::EditorSettings::default());
            cx.set_global(lector_ui::settings::AppSettings {
                settings: std::sync::Arc::new(settings.clone()),
                warnings: Default::default(),
            });
            cx.set_global(lector_ui::commands_bridge::CoreGlobal(core));
            crate::init(cx);
        });
        let project = Project::open(&root, settings).unwrap();
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
        cx.run_until_parked();
        let store = cx
            .update(|_, cx| crate::store_for(ws.entity_id(), cx))
            .expect("git store for the workspace");

        // Nothing to undo yet: a toast says so; the Activity log opens as a tab.
        cx.update(|window, cx| window.focus(&ws.focus_handle(cx)));
        cx.run_until_parked();
        cx.dispatch_action(UndoLastOperation);
        cx.run_until_parked();
        let toast = ws.read_with(cx, |ws, _| ws.toast_message());
        assert_eq!(toast.as_ref().map(|t| t.as_ref()), Some("Nothing to undo"));
        cx.dispatch_action(lector_ui::actions::workspace::ShowActivity);
        cx.run_until_parked();
        let opened = ws.read_with(cx, |ws, cx| {
            ws.active_pane()
                .read(cx)
                .index_for_key(&SharedString::from("activity"), cx)
                .is_some()
        });
        assert!(opened, "Activity log tab");

        // No confirmation: the files are gone at once, an Undo toast is up.
        let task = cx.update(|_, cx| {
            run_op(
                &store,
                GitOp::Discard(vec!["a.txt".into(), "new.txt".into()]),
                cx,
            )
        });
        let applied = wait(cx, task);
        assert_eq!(applied, Ok(true));
        // git's checkout may convert line endings (core.autocrlf on Windows runners).
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "one\ntwo\n"
        );
        assert!(!root.join("new.txt").exists());
        let message = ws.read_with(cx, |ws, _| ws.action_toast_message());
        assert!(
            message.is_some_and(|m| m.contains("Discarded")),
            "undo toast shown"
        );

        // `workspace::UndoLastOperation` puts both back, byte for byte.
        cx.update(|window, cx| window.focus(&ws.focus_handle(cx)));
        cx.run_until_parked();
        cx.dispatch_action(UndoLastOperation);
        for _ in 0..50 {
            cx.run_until_parked();
            if root.join("new.txt").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nmy precious edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "untracked work\n"
        );
    }

    fn wait<T: 'static>(cx: &mut VisualTestContext, task: Task<T>) -> T {
        cx.executor().block_test(task)
    }
}
