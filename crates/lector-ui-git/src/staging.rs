//! Git write operations run from the UI: stage / unstage / discard files or
//! hunks, off the main thread, followed by an immediate refresh of every git view.
//!
//! Every function here is the direct result of an explicit user action (button,
//! menu item, key binding); nothing writes on its own. Since v0.4 each operation is a
//! registry command (`lector_app::commands::git_write`), so it is journaled in the
//! activity log. Destructive ones (discard) run immediately after Lector backed up what
//! will be lost, and show an "Undo" toast instead of asking first; only when the backup
//! cannot be taken does a confirmation come up.

use gpui::{App, Entity, SharedString, Task};
use lector_app::commands::git_write::BackupUnavailable;
use lector_app::commands::{CommandContext, CommandError, Source};
use lector_app::journal::Journal;
use lector_git::{Changeset, FileChange, OperationKind};
use lector_review::review::{self, HunkAction, LineSelection};
use lector_ui::background::{background_described, view, JobKey, JobKind, Priority, Subject};
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
        /// [`lector_review::review::FileDiff::content_id`] of the diff the
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

/// [`git_error_text`] through a review-analysis error.
pub fn review_error_text(e: &lector_review::Error) -> String {
    match e {
        lector_review::Error::Git(g) => git_error_text(g),
        other => other.to_string(),
    }
}

/// [`git_error_text`] through a project error.
pub fn project_error_text(e: &lector_app::project::ProjectError) -> String {
    match e {
        lector_app::project::ProjectError::Git(g) => git_error_text(g),
        other => other.to_string(),
    }
}

/// The text a failed command shows inline: git's stderr verbatim when git failed.
pub fn command_error_text(e: &CommandError) -> String {
    match e {
        CommandError::Failed(err) => {
            if let Some(g) = err.downcast_ref::<lector_git::Error>() {
                git_error_text(g)
            } else if let Some(w) = err.downcast_ref::<lector_review::Error>() {
                review_error_text(w)
            } else if let Some(w) = err.downcast_ref::<lector_app::project::ProjectError>() {
                project_error_text(w)
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
    // A write the user asked for: never replaced, survives the panel closing (dropping its task
    // detaches it); one git write per repository at a time, in order.
    let key = git_write_key(store, cx);
    let work = {
        let core = core.clone();
        background_described(cx, key, Priority::Visible, id, move |_| {
            // The journal records every write command; idempotent.
            Journal::install(&core);
            let ctx = CommandContext::new(core.clone(), Some(project), Source::Ui);
            core.registry().invoke(id, args, &ctx)
        })
    };
    let store = store.clone();
    cx.spawn(async move |cx| {
        let result = match work.await {
            Ok(result) => result,
            Err(e) => return Err(format!("{} failed:\n{e}", op.verb())),
        };
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

/// The key of a git write on `store`'s repository: the serial lane runs one per repository at a
/// time, in submission order, whichever view started it.
pub fn git_write_key(store: &Entity<GitStore>, cx: &App) -> JobKey {
    let workdir = store
        .read(cx)
        .project()
        .and_then(|p| p.repo().map(|r| r.workdir().to_path_buf()));
    let subject = match workdir {
        Some(dir) => Subject::repo(dir),
        None => view(store.entity_id()),
    };
    JobKey::new(JobKind::GitWrite, subject)
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
    use lector_app::Project;
    use lector_app::{AppCore, CoreOptions};
    use lector_ui::actions::workspace::UndoLastOperation;
    use lector_ui::views::Workspace;

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

    fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A workspace window on a fresh repository whose `a.txt` has an unstaged change; the core
    /// keeps its journal in `data`.
    fn workspace_on_repo<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
        data: &std::path::Path,
    ) -> (
        Entity<Workspace>,
        &'a mut VisualTestContext,
        Entity<GitStore>,
        AppCore,
    ) {
        cx.executor().allow_parking();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "Ada Lovelace"]);
        git(root, &["config", "user.email", "ada@example.com"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-q", "-m", "initial"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        settings.session.restore = false;
        let core = AppCore::open(CoreOptions {
            data_dir: Some(data.to_path_buf()),
            ..CoreOptions::for_tests()
        });
        let global = core.clone();
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
            cx.set_global(lector_ui::commands_bridge::CoreGlobal(global));
            crate::init(cx);
        });
        let project = Project::open(root, settings).unwrap();
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
        cx.run_until_parked();
        let store = cx
            .update(|_, cx| crate::store_for(ws.entity_id(), cx))
            .expect("git store for the workspace");
        (ws, cx, store, core)
    }

    /// ADR 0010 item 4: closing the window (which drops the panel's `op_task`) right after
    /// "Stage", before the queued write ran, still stages; a read started the same way is
    /// cancelled.
    #[gpui::test]
    fn closing_the_window_with_a_queued_stage_still_stages(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let (_ws, cx, store, _core) = workspace_on_repo(cx, &root, data.path());
        let read_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        cx.update(|window, cx| {
            let stage = run_op(&store, GitOp::Stage(vec!["a.txt".into()]), cx);
            let r = read_ran.clone();
            let key = lector_ui::background::view_key(JobKind::GitChangeset, store.entity_id());
            let read = lector_ui::background::background(cx, key, Priority::Visible, move |_| {
                r.store(true, std::sync::atomic::Ordering::SeqCst)
            });
            // What closing the Changes panel or the window does to its tasks.
            drop(stage);
            drop(read);
            window.remove_window();
        });
        for _ in 0..50 {
            cx.run_until_parked();
            if git_out(&root, &["diff", "--cached", "--name-only"]).contains("a.txt") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            git_out(&root, &["diff", "--cached", "--name-only"]).trim(),
            "a.txt",
            "the stage happened after the window closed"
        );
        assert!(
            !read_ran.load(std::sync::atomic::Ordering::SeqCst),
            "the read was cancelled"
        );
    }

    /// A commit still running (its pre-commit hook sleeps) when its view closes completes,
    /// and the journal records it (real scheduler threads, the composer's job).
    #[cfg(unix)]
    #[gpui::test]
    fn closing_the_window_during_a_commit_completes_it_and_journals_it(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let (_ws, cx, store, core) = workspace_on_repo(cx, &root, data.path());
        git(&root, &["add", "a.txt"]);
        let marker = root.join(".git/hook-started");
        let hook = root.join(".git/hooks/pre-commit");
        let script = format!("#!/bin/sh\ntouch \"{}\"\nsleep 1\n", marker.display());
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let scheduler = lector_ui::background::Scheduler::new(2);
        let task = cx.update(|_, cx| {
            let project = store.read(cx).project().cloned();
            let core = lector_ui::commands_bridge::core(cx);
            let key = git_write_key(&store, cx);
            lector_ui::background::background_on(
                &scheduler,
                cx,
                key,
                Priority::Visible,
                move |_| {
                    Journal::install(&core);
                    let ctx = CommandContext::new(core.clone(), project, Source::Ui);
                    core.registry()
                        .invoke("git.commit", json!({ "message": "closing commit" }), &ctx)
                        .map(|_| ())
                },
            )
        });
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() {
            assert!(std::time::Instant::now() < end, "the hook started");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cx.update(|window, _| {
            drop(task);
            window.remove_window();
        });
        cx.run_until_parked();
        assert!(scheduler.wait_idle(std::time::Duration::from_secs(10)));
        assert!(git_out(&root, &["log", "--format=%s"]).contains("closing commit"));
        let journal = Journal::of(&core).expect("journal installed");
        assert!(
            journal.list(10).iter().any(|e| e.command == "git.commit"),
            "the commit is journaled"
        );
    }
}
