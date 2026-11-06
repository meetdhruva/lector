//! GPUI integration tests for keyboard-only operation: F6 region cycling, focus restore after
//! dialogs, Tab order in the find bar, the interface scale and reduced motion.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, Entity, Focusable, TestAppContext, VisualTestContext};
use lector_theme::{Appearance, ThemeRegistry};

use crate::actions::editor::{Find, GoToLine};
use crate::actions::workspace::{
    FocusNextControl, FocusNextRegion, FocusPrevControl, FocusPrevRegion,
};
use crate::editor::FileView;
use crate::theme::{ThemeMode, ThemeState};
use crate::views::workspace::Region;
use crate::views::Workspace;

fn init(cx: &mut TestAppContext, configure: impl FnOnce(&mut lector_config::Settings)) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    configure(&mut settings);
    cx.update(|cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(crate::code_view::EditorSettings::default());
        crate::scale::apply_from_settings(&settings);
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

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\nlet a = 1;\n").expect("write");
    let root = dir.path().canonicalize().expect("canonical");
    Fixture { _dir: dir, root }
}

fn open_workspace<'a>(
    cx: &'a mut TestAppContext,
    root: &std::path::Path,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let project = lector_workspace::Project::open(root, settings).expect("project");
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
    cx.run_until_parked();
    (ws, cx)
}

fn open_main(
    ws: &Entity<Workspace>,
    root: &std::path::Path,
    cx: &mut VisualTestContext,
) -> Entity<FileView> {
    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(root.join("main.rs"), Some(1), Some(1), window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    cx.update(|window, cx| {
        let focus = view.focus_handle(cx);
        window.focus(&focus);
    });
    cx.run_until_parked();
    view
}

fn region(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Region> {
    cx.update(|window, cx| ws.read(cx).focused_region(window, cx))
}

#[gpui::test]
fn f6_cycles_through_every_region_and_back(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.root);
    open_main(&ws, &fx.root, cx);
    assert_eq!(region(&ws, cx), Some(Region::Pane(0)));

    let regions = ws.read_with(cx, |ws, cx| ws.regions(cx));
    assert_eq!(
        regions,
        vec![
            Region::TitleBar,
            Region::ActivityBar,
            Region::Sidebar,
            Region::Pane(0),
            Region::StatusBar
        ]
    );

    // Forward: pane -> status bar -> title bar -> activity bar -> sidebar -> pane.
    let mut seen = Vec::new();
    for _ in 0..regions.len() {
        cx.dispatch_action(FocusNextRegion);
        cx.run_until_parked();
        seen.push(region(&ws, cx));
    }
    assert_eq!(
        seen,
        vec![
            Some(Region::StatusBar),
            Some(Region::TitleBar),
            Some(Region::ActivityBar),
            Some(Region::Sidebar),
            Some(Region::Pane(0)),
        ]
    );

    // Backward is the exact reverse.
    let mut back = Vec::new();
    for _ in 0..regions.len() {
        cx.dispatch_action(FocusPrevRegion);
        cx.run_until_parked();
        back.push(region(&ws, cx));
    }
    assert_eq!(
        back,
        vec![
            Some(Region::Sidebar),
            Some(Region::ActivityBar),
            Some(Region::TitleBar),
            Some(Region::StatusBar),
            Some(Region::Pane(0)),
        ]
    );
}

#[gpui::test]
fn hidden_sidebar_is_skipped_by_f6(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.root);
    open_main(&ws, &fx.root, cx);
    cx.dispatch_action(crate::actions::workspace::ToggleSidebar);
    cx.run_until_parked();
    let regions = ws.read_with(cx, |ws, cx| ws.regions(cx));
    assert!(!regions.contains(&Region::Sidebar) && !regions.contains(&Region::ActivityBar));
}

#[gpui::test]
fn closing_a_dialog_gives_the_focus_back(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_main(&ws, &fx.root, cx);
    let focus = cx.update(|_, cx| view.focus_handle(cx));
    assert!(cx.update(|window, _| focus.is_focused(window)));

    cx.dispatch_action(GoToLine);
    cx.run_until_parked();
    assert!(ws.read_with(cx, |ws, cx| ws.modal_layer().read(cx).has_active_modal()));
    assert!(
        !cx.update(|window, _| focus.is_focused(window)),
        "the dialog takes the focus"
    );

    cx.dispatch_action(crate::actions::picker::Cancel);
    cx.run_until_parked();
    assert!(!ws.read_with(cx, |ws, cx| ws.modal_layer().read(cx).has_active_modal()));
    assert!(
        cx.update(|window, _| focus.is_focused(window)),
        "Escape returns to the editor"
    );
}

#[gpui::test]
fn tab_and_f6_stay_inside_a_dialog(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.root);
    open_main(&ws, &fx.root, cx);
    cx.dispatch_action(GoToLine);
    cx.run_until_parked();
    let container = ws
        .read_with(cx, |ws, cx| {
            ws.modal_layer().read(cx).modal_container().cloned()
        })
        .expect("modal container");
    for _ in 0..6 {
        cx.dispatch_action(FocusNextControl);
        cx.run_until_parked();
        assert!(cx.update(|window, cx| container.contains_focused(window, cx)));
    }
    for _ in 0..3 {
        cx.dispatch_action(FocusPrevControl);
        cx.run_until_parked();
        assert!(cx.update(|window, cx| container.contains_focused(window, cx)));
    }
    cx.dispatch_action(FocusNextRegion);
    cx.run_until_parked();
    assert!(cx.update(|window, cx| container.contains_focused(window, cx)));
}

#[gpui::test]
fn tab_order_in_the_find_bar_follows_the_visual_order(cx: &mut TestAppContext) {
    init(cx, |_| {});
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.root);
    open_main(&ws, &fx.root, cx);
    cx.dispatch_action(Find);
    cx.run_until_parked();
    cx.simulate_input("a");
    cx.run_until_parked();
    let bar = ws
        .read_with(cx, |ws, cx| ws.find_bar(cx))
        .expect("find bar");
    let input = cx.update(|_, cx| bar.focus_handle(cx));
    assert!(cx.update(|window, _| input.is_focused(window)));

    // Tab leaves the input for the next control and Shift-Tab comes back.
    cx.dispatch_action(FocusNextControl);
    cx.run_until_parked();
    assert!(!cx.update(|window, _| input.is_focused(window)));
    cx.dispatch_action(FocusPrevControl);
    cx.run_until_parked();
    assert!(cx.update(|window, _| input.is_focused(window)));
}

#[gpui::test]
fn the_scale_resizes_the_ui_and_resets(cx: &mut TestAppContext) {
    init(cx, |s| s.ui.scale = 1.5);
    assert_eq!(crate::scale::scale(), 1.5);
    assert_eq!(f32::from(crate::scale::px(10.0)), 15.0);
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.root);
    open_main(&ws, &fx.root, cx);
    cx.update(|window, _| assert_eq!(f32::from(window.rem_size()), 16.0 * 1.5));

    cx.dispatch_action(crate::actions::workspace::UiZoomIn);
    cx.run_until_parked();
    assert!(crate::scale::scale() > 1.5);
    cx.dispatch_action(crate::actions::workspace::UiZoomReset);
    cx.run_until_parked();
    assert_eq!(crate::scale::scale(), 1.0);
    cx.update(|window, _| assert_eq!(f32::from(window.rem_size()), 16.0));
    // The runtime scale is also the one the settings global reports.
    cx.update(|_, cx| {
        assert_eq!(crate::settings::AppSettings::get(cx).ui.scale, 1.0);
    });
    drop(ws);
}

#[gpui::test]
fn reduce_motion_stops_the_caret_blinking(cx: &mut TestAppContext) {
    init(cx, |s| {
        s.cursor_blink = true;
        s.ui.reduce_motion = Some(true);
    });
    let fx = fixture();
    let (ws, cx) = open_workspace(cx, &fx.root);
    let view = open_main(&ws, &fx.root, cx);
    let blinking = |cx: &mut VisualTestContext| {
        view.read_with(cx, |v, cx| v.code_view().read(cx).is_blinking())
    };
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(!blinking(cx), "no blink timer with reduced motion");

    // Switching it off lets the caret blink again.
    cx.dispatch_action(crate::actions::workspace::ToggleReduceMotion);
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(!cx.update(|_, cx| crate::a11y::reduce_motion(cx)));
    assert!(blinking(cx), "the caret blinks without reduced motion");
}
