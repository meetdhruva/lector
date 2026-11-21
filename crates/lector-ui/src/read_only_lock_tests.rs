//! Tabs locked read-only (a file opened from a link that points outside the project): the lock
//! is part of the open, so finishing the load, reloading, changes on disk, the toggle and a
//! restored session never make the tab editable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, Entity, Focusable, TestAppContext, VisualTestContext};
use lector_theme::{Appearance, ThemeRegistry};

use crate::actions::editor::ToggleReadOnly;
use crate::editor::FileView;
use crate::theme::{ThemeMode, ThemeState};
use crate::views::pane::PaneItem;
use crate::views::Workspace;

fn init(cx: &mut TestAppContext) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    cx.update(|cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(crate::code_view::EditorSettings::default());
        cx.set_global(crate::settings::AppSettings {
            settings: Arc::new(settings),
            warnings: Default::default(),
        });
    });
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn fixture(files: &[(&str, &str)]) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).expect("write");
    }
    let root = dunce::canonicalize(dir.path()).expect("canonical");
    Fixture { _dir: dir, root }
}

fn open_workspace<'a>(
    cx: &'a mut TestAppContext,
    root: &Path,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let project = lector_app::project::Project::open(root, settings).expect("project");
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
    cx.run_until_parked();
    (ws, cx)
}

fn focus(view: &Entity<FileView>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let focus = view.focus_handle(cx);
        window.focus(&focus);
    });
    cx.run_until_parked();
}

fn text(view: &Entity<FileView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |v, cx| {
        v.model()
            .read(cx)
            .buffer()
            .map(|b| b.read(cx).text().to_string())
            .unwrap_or_default()
    })
}

fn read_only(view: &Entity<FileView>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |v, cx| v.is_read_only(cx))
}

#[gpui::test]
fn a_linked_outside_file_stays_read_only_through_load_reload_and_the_toggle(
    cx: &mut TestAppContext,
) {
    init(cx);
    let project = fixture(&[("a.txt", "inside\n")]);
    let outside = fixture(&[("notes.txt", "outside\n")]);
    let (ws, cx) = open_workspace(cx, &project.root);
    let path = outside.root.join("notes.txt");

    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_linked_path_read_only(path.clone(), Some(1), Some(1), window, cx)
        })
        .expect("file view");
    // Before the document has loaded the tab is already locked...
    assert!(view.read_with(cx, |v, _| v.is_loading()));
    assert!(read_only(&view, cx));
    // ...and finishing the load (what used to overwrite the flag) does not lift it.
    cx.run_until_parked();
    assert!(!view.read_with(cx, |v, _| v.is_loading()));
    assert_eq!(text(&view, cx), "outside\n");
    assert!(read_only(&view, cx), "Loaded made the tab editable");
    focus(&view, cx);
    cx.simulate_input("x");
    assert_eq!(text(&view, cx), "outside\n", "typing edited a locked tab");
    let meta = view.read_with(cx, |v, cx| v.meta(cx)).expect("meta");
    assert!(meta.read_only && meta.editable);
    assert!(meta.read_only_reason.is_some());

    // The file changing on disk and a reload: still locked.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, "changed outside\n").expect("write");
    view.update(cx, |v, cx| v.reload(cx));
    cx.run_until_parked();
    assert_eq!(text(&view, cx), "changed outside\n");
    assert!(read_only(&view, cx), "a reload made the tab editable");

    // "Toggle read-only" (keyboard, menu) explains instead of unlocking.
    cx.dispatch_action(ToggleReadOnly);
    cx.run_until_parked();
    assert!(read_only(&view, cx));
    let toast = ws.read_with(cx, |ws, _| ws.toast_message().map(|m| m.to_string()));
    assert!(
        toast
            .as_deref()
            .is_some_and(|t| t.contains("outside the project")),
        "{toast:?}"
    );
    // The status bar's toggle goes the same way.
    view.update(cx, |v, cx| v.toggle_read_only(cx));
    cx.run_until_parked();
    assert!(read_only(&view, cx));
    cx.simulate_input("y");
    assert_eq!(text(&view, cx), "changed outside\n");
    // The tab's indicator is the existing read-only one.
    let indicated = ws.read_with(cx, |ws, cx| {
        ws.active_pane()
            .read(cx)
            .items()
            .any(|i| i.path(cx).as_deref() == Some(path.as_path()) && i.is_read_only(cx))
    });
    assert!(indicated);
}

#[gpui::test]
fn explicit_opens_stay_editable_and_a_link_leaves_an_open_tab_alone(cx: &mut TestAppContext) {
    init(cx);
    let project = fixture(&[("a.txt", "inside\n")]);
    let outside = fixture(&[("notes.txt", "outside\n")]);
    let (ws, cx) = open_workspace(cx, &project.root);
    let path = outside.root.join("notes.txt");

    // File > Open of a file outside the project: editable, as before.
    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(path.clone(), Some(1), Some(1), window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    focus(&view, cx);
    assert!(!read_only(&view, cx));
    cx.simulate_input("x");
    assert_eq!(text(&view, cx), "xoutside\n");
    // A link to the file the user already has open does not take editing away from that tab.
    let again = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_linked_path_read_only(path.clone(), None, None, window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    assert_eq!(again, view);
    assert!(!read_only(&view, cx));
    assert!(view.read_with(cx, |v, _| v.read_only_lock().is_none()));
}

#[gpui::test]
fn the_lock_follows_split_copies_and_the_restored_session(cx: &mut TestAppContext) {
    init(cx);
    let project = fixture(&[("a.txt", "inside\n")]);
    let outside = fixture(&[("notes.txt", "outside\n")]);
    let (ws, cx) = open_workspace(cx, &project.root);
    let path = outside.root.join("notes.txt");
    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_linked_path_read_only(path.clone(), None, None, window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    // Also open an ordinary tab: only the locked one is flagged in the saved session.
    ws.update_in(cx, |ws, window, cx| {
        ws.open_path_at(project.root.join("a.txt"), None, None, window, cx);
    });
    cx.run_until_parked();

    // A copy of the tab (split) is locked too.
    let item = ws.read_with(cx, |ws, cx| {
        ws.active_pane()
            .read(cx)
            .items()
            .position(|i| i.path(cx).as_deref() == Some(path.as_path()))
    });
    assert_eq!(item, Some(0));
    let item = cx.update(|_, cx| crate::editor::FileItem::new(view.clone(), cx));
    let copy = cx.update(|window, cx| item.clone_item(window, cx).and_then(|i| i.file_view()));
    let copy = copy.expect("copy");
    cx.run_until_parked();
    assert!(read_only(&copy, cx));
    assert!(view.read_with(cx, |v, _| v.read_only_lock().is_some()));

    // The session records the lock and the restored tab is locked before it loads.
    let session = ws.read_with(cx, |ws, cx| ws.session(cx));
    let flags: Vec<(String, bool)> = session
        .panes
        .iter()
        .flat_map(|p| p.tabs.iter())
        .filter_map(|t| match &t.kind {
            lector_config::TabKind::File { path } => Some((
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                t.read_only_locked,
            )),
            _ => None,
        })
        .collect();
    assert!(
        flags.contains(&("notes.txt".to_string(), true)),
        "{flags:?}"
    );
    assert!(flags.contains(&("a.txt".to_string(), false)), "{flags:?}");

    let (ws2, cx) = open_workspace(cx, &project.root);
    let restored = ws2.update_in(cx, |ws, window, cx| {
        ws.restore_session_from(session, window, cx)
    });
    assert!(restored);
    let locked_view = ws2
        .read_with(cx, |ws, cx| {
            ws.all_file_views(cx)
                .into_iter()
                .find(|v| v.read(cx).path() == path.as_path())
        })
        .expect("restored tab");
    assert!(
        read_only(&locked_view, cx),
        "locked before the load finished"
    );
    cx.run_until_parked();
    assert_eq!(text(&locked_view, cx), "outside\n");
    assert!(read_only(&locked_view, cx), "restored tab became editable");
    let ordinary = ws2
        .read_with(cx, |ws, cx| {
            ws.all_file_views(cx)
                .into_iter()
                .find(|v| v.read(cx).path() == project.root.join("a.txt"))
        })
        .expect("ordinary tab");
    assert!(!read_only(&ordinary, cx));
}
