//! GPUI tests for the v0.4 review UX seams of the shell: the uncommitted-changes hint, the
//! one-time language-server install banner and the "why did this line change" query of a file
//! view.

use std::path::Path;
use std::sync::Arc;

use gpui::{App, Entity, TestAppContext, VisualTestContext};
use lector_app::{AppCore, CoreOptions};
use lector_theme::{Appearance, ThemeRegistry};

use crate::theme::{ThemeMode, ThemeState};
use crate::views::{ReviewHint, Workspace};

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .status();
    assert!(status.is_ok_and(|s| s.success()), "git {args:?}");
}

fn setup(cx: &mut TestAppContext, data: &Path) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let core = AppCore::open(CoreOptions {
        data_dir: Some(data.to_path_buf()),
        ..CoreOptions::for_tests()
    });
    cx.update(|cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(crate::code_view::EditorSettings::default());
        cx.set_global(crate::settings::AppSettings {
            settings: Arc::new(settings),
            warnings: Default::default(),
        });
        cx.set_global(crate::commands_bridge::CoreGlobal(core));
    });
}

fn open_workspace<'a>(
    cx: &'a mut TestAppContext,
    root: &Path,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let project = lector_workspace::Project::open(root, settings).expect("project");
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
    cx.run_until_parked();
    (ws, cx)
}

fn noop_review() -> Box<dyn gpui::Action> {
    Box::new(crate::actions::workspace::ShowChanges)
}

#[gpui::test]
fn review_hint_is_stored_and_cleared(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().expect("data dir");
    let dir = tempfile::tempdir().expect("dir");
    setup(cx, data.path());
    let (ws, cx) = open_workspace(cx, &dunce::canonicalize(dir.path()).expect("root"));
    assert!(ws.read_with(cx, |ws, _| ws.review_hint().is_none()));
    let hint = ReviewHint {
        uncommitted: 3,
        new_since_last: true,
        open_review: noop_review,
    };
    ws.update(cx, |ws, cx| ws.set_review_hint(Some(hint.clone()), cx));
    cx.run_until_parked();
    assert_eq!(
        ws.read_with(cx, |ws, _| ws.review_hint().cloned()),
        Some(hint)
    );
    // The welcome view of the window shows the same hint.
    ws.update_in(cx, |ws, window, cx| {
        let _ = ws.welcome_view(window, cx);
    });
    ws.update(cx, |ws, cx| ws.set_review_hint(None, cx));
    assert!(ws.read_with(cx, |ws, _| ws.review_hint().is_none()));
}

#[gpui::test]
fn install_banner_shows_once_per_server_and_can_be_turned_off(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().expect("data dir");
    let dir = tempfile::tempdir().expect("dir");
    setup(cx, data.path());
    let (ws, cx) = open_workspace(cx, &dunce::canonicalize(dir.path()).expect("root"));

    // Unknown servers have no install hint, so no banner.
    ws.update(cx, |ws, cx| {
        ws.offer_lsp_install_for_tests("not-a-server", cx)
    });
    assert!(ws.read_with(cx, |ws, _| ws.lsp_banner().is_none()));

    ws.update(cx, |ws, cx| {
        ws.offer_lsp_install_for_tests("rust-analyzer", cx)
    });
    let banner = ws.read_with(cx, |ws, _| ws.lsp_banner()).expect("banner");
    assert_eq!(banner.server, "rust-analyzer");
    assert_eq!(banner.command, "rustup component add rust-analyzer");

    // Dismissed without "Don't show again": gone, and never offered for this server again.
    ws.update(cx, |ws, cx| ws.dismiss_lsp_banner(false, cx));
    assert!(ws.read_with(cx, |ws, _| ws.lsp_banner().is_none()));
    ws.update(cx, |ws, cx| {
        ws.offer_lsp_install_for_tests("rust-analyzer", cx)
    });
    assert!(
        ws.read_with(cx, |ws, _| ws.lsp_banner().is_none()),
        "once per server"
    );

    // Another server is offered; "Don't show again" silences every server.
    ws.update(cx, |ws, cx| ws.offer_lsp_install_for_tests("gopls", cx));
    assert!(ws.read_with(cx, |ws, _| ws.lsp_banner().is_some()));
    ws.update(cx, |ws, cx| ws.dismiss_lsp_banner(true, cx));
    ws.update(cx, |ws, cx| ws.offer_lsp_install_for_tests("pyright", cx));
    assert!(
        ws.read_with(cx, |ws, _| ws.lsp_banner().is_none()),
        "turned off"
    );
}

#[gpui::test]
fn file_view_explains_changed_lines_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let data = tempfile::tempdir().expect("data dir");
    let dir = tempfile::tempdir().expect("dir");
    let root = dunce::canonicalize(dir.path()).expect("root");
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").expect("write");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "-q", "-m", "add a"]);
    std::fs::write(root.join("a.txt"), "one\ntwo\nTHREE\nfour\n").expect("write");
    setup(cx, data.path());
    let (ws, cx) = open_workspace(cx, &root);
    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(root.join("a.txt"), Some(1), Some(1), window, cx)
        })
        .expect("file view");
    let mut changed = None;
    for _ in 0..50 {
        cx.run_until_parked();
        changed = view.read_with(cx, |v, cx| v.provenance_query(2, cx));
        if changed.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let query = changed.expect("the modified line has a provenance query");
    assert_eq!(query.path, "a.txt");
    assert_eq!(query.line, 2);
    assert!(query.rev.is_none(), "working tree");
    assert!(
        view.read_with(cx, |v, cx| v.provenance_query(0, cx))
            .is_none(),
        "unchanged lines have nothing to explain"
    );
}
