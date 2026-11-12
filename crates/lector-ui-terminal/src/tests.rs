//! GPUI tests of the terminal UI. Terminals here get no process (`without_processes`): the
//! core already tests PTYs; these cover key routing through GPUI's dispatch, the panel's
//! commands and model, editor-area moves and the "Open in Terminal" folders.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, prelude::*, App, Context, Entity, FocusHandle, Focusable, KeyDownEvent, TestAppContext,
    VisualTestContext, Window,
};
use lector_theme::{Appearance, ThemeRegistry};
use lector_ui::actions::terminal as actions;
use lector_ui::keymap::KeySpec;
use lector_ui::theme::{ThemeMode, ThemeState};
use lector_ui::views::{Workspace, WorkspaceRoot};

use crate::keys::{passthrough_bindings, NO_ACTION};
use crate::{panel_for, TerminalPanel};

fn settings_with(configure: impl FnOnce(&mut lector_config::Settings)) -> lector_config::Settings {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    settings.terminal.restore = false;
    configure(&mut settings);
    settings
}

fn init(cx: &mut TestAppContext, settings: lector_config::Settings) {
    cx.update(|cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(lector_ui::code_view::EditorSettings::default());
        cx.set_global(lector_ui::settings::AppSettings {
            settings: Arc::new(settings),
            warnings: Default::default(),
        });
        crate::init(cx);
    });
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src/deep")).expect("mkdir");
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("write");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    Fixture { _dir: dir, root }
}

fn open_workspace<'a>(
    cx: &'a mut TestAppContext,
    root: &Path,
) -> (
    Entity<Workspace>,
    Entity<TerminalPanel>,
    &'a mut VisualTestContext,
) {
    let project = lector_workspace::Project::open(root, settings_with(|_| {})).expect("project");
    let slot: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::default();
    let s = slot.clone();
    let (_root, cx) = cx.add_window_view(move |window, cx| {
        let workspace = cx.new(|cx| Workspace::new(project, window, cx));
        *s.borrow_mut() = Some(workspace.clone());
        WorkspaceRoot { workspace }
    });
    cx.run_until_parked();
    let ws = slot.borrow().clone().expect("workspace");
    let panel = cx
        .update(|_, cx| panel_for(ws.entity_id(), cx))
        .expect("panel");
    // Actions dispatch from the focused element: focus the workspace.
    cx.update(|window, cx| {
        let focus = ws.focus_handle(cx);
        window.focus(&focus);
    });
    panel.update(cx, |p, _| p.without_processes());
    (ws, panel, cx)
}

fn terminal_cwds(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) -> Vec<PathBuf> {
    cx.update(|_, cx| {
        panel
            .read(cx)
            .terminals()
            .iter()
            .map(|v| v.read(cx).cwd().to_path_buf())
            .collect()
    })
}

#[gpui::test]
fn panel_commands_tabs_splits_and_editor_area(cx: &mut TestAppContext) {
    init(cx, settings_with(|_| {}));
    let f = fixture();
    let (ws, panel, cx) = open_workspace(cx, &f.root);
    assert!(!ws.read_with(cx, |ws, _| ws.is_bottom_panel_visible()));

    // Ctrl+` with no terminal: shows the panel and opens one in the project root.
    cx.dispatch_action(actions::TogglePanel);
    cx.run_until_parked();
    assert!(ws.read_with(cx, |ws, _| ws.is_bottom_panel_visible()));
    assert_eq!(terminal_cwds(&panel, cx), vec![f.root.clone()]);
    let first = panel
        .read_with(cx, |p, _| p.active_terminal())
        .expect("terminal");
    assert!(cx.update(|window, cx| first.read(cx).focus_handle.is_focused(window)));
    // The panel is an F6 region while shown.
    let regions = ws.read_with(cx, |ws, cx| ws.regions(cx));
    assert!(regions.contains(&lector_ui::views::workspace::Region::BottomPanel));

    // New tab, split, next/previous tab, focus next split.
    cx.dispatch_action(actions::NewTerminal);
    cx.dispatch_action(actions::SplitTerminal);
    cx.run_until_parked();
    panel.read_with(cx, |p, _| {
        assert_eq!(p.model.tabs().len(), 2);
        assert_eq!(p.model.active_tab().map(|t| t.panes.len()), Some(2));
        assert_eq!(p.model.active_tab().map(|t| t.focused), Some(1));
    });
    cx.dispatch_action(actions::FocusNextSplit);
    panel.read_with(cx, |p, _| {
        assert_eq!(p.model.active_tab().map(|t| t.focused), Some(0))
    });
    cx.dispatch_action(actions::PrevTerminal);
    let active = panel.read_with(cx, |p, _| p.active_terminal());
    assert_eq!(active.as_ref(), Some(&first));
    cx.dispatch_action(actions::NextTerminal);
    assert_eq!(panel.read_with(cx, |p, _| p.model.active_index()), 1);

    // Move the focused terminal to the editor area and back.
    let moved = panel
        .read_with(cx, |p, _| p.active_terminal())
        .expect("terminal");
    cx.dispatch_action(actions::MoveToEditor);
    cx.run_until_parked();
    assert!(!panel.read_with(cx, |p, _| p.contains(&moved)));
    let key = ws.read_with(cx, |ws, cx| {
        ws.active_pane()
            .read(cx)
            .active_item()
            .and_then(|i| i.item_key(cx))
    });
    let want = format!("terminal:{}", moved.read_with(cx, |v, _| v.id));
    assert_eq!(key.map(|k| k.to_string()), Some(want));
    cx.dispatch_action(actions::MoveToPanel);
    cx.run_until_parked();
    assert!(panel.read_with(cx, |p, _| p.contains(&moved)));
    assert_eq!(
        ws.read_with(cx, |ws, cx| ws.active_pane().read(cx).item_count()),
        0
    );

    // Kill every terminal: the panel hides when the last one goes.
    for _ in 0..3 {
        cx.dispatch_action(actions::KillTerminal);
        cx.run_until_parked();
    }
    assert!(panel.read_with(cx, |p, _| p.terminals().is_empty()));
    assert!(!ws.read_with(cx, |ws, _| ws.is_bottom_panel_visible()));
}

#[gpui::test]
fn open_in_terminal_folders(cx: &mut TestAppContext) {
    init(cx, settings_with(|_| {}));
    let f = fixture();
    let (ws, panel, cx) = open_workspace(cx, &f.root);
    let main = f.root.join("src/main.rs");
    // File tree: a folder, a file (its folder), the project root (empty area).
    for (path, want) in [
        (f.root.join("src/deep"), f.root.join("src/deep")),
        (main.clone(), f.root.join("src")),
        (f.root.clone(), f.root.clone()),
    ] {
        cx.update(|window, cx| lector_ui::terminal_hook::open_in_terminal(path, window, cx));
        cx.run_until_parked();
        assert_eq!(terminal_cwds(&panel, cx).last(), Some(&want));
    }
    // Editor tab / active file: Open in Terminal uses the file's folder.
    ws.update_in(cx, |ws, window, cx| {
        ws.open_path_at(main.clone(), Some(1), Some(1), window, cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(actions::OpenInTerminal);
    cx.run_until_parked();
    assert_eq!(terminal_cwds(&panel, cx).last(), Some(&f.root.join("src")));
    // New Terminal without context: the project root (default setting).
    cx.dispatch_action(actions::NewTerminal);
    cx.run_until_parked();
    assert_eq!(terminal_cwds(&panel, cx).last(), Some(&f.root));
    assert!(cx.update(|_, cx| lector_ui::terminal_hook::has_terminal(cx)));
}

#[gpui::test]
fn new_terminal_in_active_file_folder_when_configured(cx: &mut TestAppContext) {
    init(
        cx,
        settings_with(|s| s.terminal.new_terminal_cwd = lector_config::NewTerminalCwd::ActiveFile),
    );
    let f = fixture();
    let (ws, panel, cx) = open_workspace(cx, &f.root);
    cx.dispatch_action(actions::NewTerminal);
    cx.run_until_parked();
    assert_eq!(
        terminal_cwds(&panel, cx),
        vec![f.root.clone()],
        "no file open"
    );
    ws.update_in(cx, |ws, window, cx| {
        ws.open_path_at(f.root.join("src/main.rs"), Some(1), Some(1), window, cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(actions::NewTerminal);
    cx.run_until_parked();
    assert_eq!(terminal_cwds(&panel, cx).last(), Some(&f.root.join("src")));
}

// ---------------------------------------------------------------- key routing

gpui::actions!(test_terminal, [WorkspaceThing, Palette]);

/// A stand-in workspace (`Workspace` context) holding a terminal-like child (`Terminal`
/// context) that records the key-down events reaching it.
struct Harness {
    terminal: FocusHandle,
    keys: Vec<String>,
    actions: Vec<&'static str>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Workspace")
            .size_full()
            .on_action(cx.listener(|h, _: &WorkspaceThing, _, _| h.actions.push("workspace")))
            .on_action(cx.listener(|h, _: &Palette, _, _| h.actions.push("palette")))
            .on_action(cx.listener(|h, _: &actions::Copy, _, _| h.actions.push("copy")))
            .child(
                div()
                    .key_context("Terminal")
                    .track_focus(&self.terminal)
                    .size_full()
                    .on_key_down(
                        cx.listener(|h, e: &KeyDownEvent, _, _| h.keys.push(e.keystroke.unparse())),
                    ),
            )
    }
}

impl Focusable for Harness {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.terminal.clone()
    }
}

fn spec(keys: &str, action: &str, context: Option<&str>) -> KeySpec {
    KeySpec {
        keys: keys.into(),
        action: action.into(),
        context: context.map(Into::into),
    }
}

#[gpui::test]
fn reserved_keys_route_through_key_contexts(cx: &mut TestAppContext) {
    let (harness, cx) = cx.add_window_view(|_, cx| Harness {
        terminal: cx.focus_handle(),
        keys: Vec::new(),
        actions: Vec::new(),
    });
    cx.update(|window, cx| {
        let mut specs = vec![
            spec("ctrl-w", "test_terminal::WorkspaceThing", Some("Workspace")),
            spec("ctrl-r", "test_terminal::WorkspaceThing", None),
            spec(
                "ctrl-k ctrl-c",
                "test_terminal::WorkspaceThing",
                Some("Workspace"),
            ),
            spec(
                "ctrl-shift-p",
                "workspace::ToggleCommandPalette",
                Some("Workspace"),
            ),
            spec("ctrl-shift-x", "test_terminal::Palette", Some("Workspace")),
            spec("ctrl-shift-c", "terminal::Copy", Some("Terminal")),
        ];
        // `ToggleCommandPalette` is reserved: bind the test's palette action to its keys.
        specs.push(spec(
            "ctrl-shift-p",
            "test_terminal::Palette",
            Some("Workspace"),
        ));
        let extra = passthrough_bindings(&specs);
        assert!(extra.iter().all(|s| s.action == NO_ACTION));
        specs.extend(extra);
        // Unknown actions (workspace::*) are skipped by bind_keymap; that is fine here.
        lector_ui::keymap::bind_keymap(cx, &specs);
        let focus = harness.read(cx).terminal.clone();
        window.focus(&focus);
    });
    cx.simulate_keystrokes("ctrl-w ctrl-r ctrl-k ctrl-c ctrl-shift-p ctrl-shift-c ctrl-shift-x");
    harness.read_with(cx, |h, _| {
        // Shell keys reach the terminal; the reserved palette key and the terminal's own
        // copy binding stay with Lector; a non-reserved Workspace binding is passed through.
        assert_eq!(h.actions, vec!["palette", "copy"], "keys: {:?}", h.keys);
        for k in ["ctrl-w", "ctrl-r", "ctrl-k", "ctrl-c", "ctrl-shift-x"] {
            assert!(
                h.keys.iter().any(|x| x == k),
                "{k} missing from {:?}",
                h.keys
            );
        }
    });
}

#[test]
fn default_keymap_keeps_shell_keys_in_the_terminal() {
    let mut specs: Vec<KeySpec> = lector_ui::keymap::default_keymap();
    specs.extend(
        lector_config::default_keymap(lector_config::Platform::Linux)
            .into_iter()
            .map(|k| KeySpec {
                keys: k.keys,
                action: k.action,
                context: k.context,
            }),
    );
    let blocked: Vec<String> = passthrough_bindings(&specs)
        .into_iter()
        .map(|s| s.keys)
        .collect();
    // Bound in Workspace by default, but shells need them.
    for k in [
        "ctrl-w", "ctrl-p", "ctrl-b", "ctrl-o", "ctrl-t", "ctrl-g", "tab", "alt-left", "ctrl-q",
    ] {
        assert!(blocked.iter().any(|b| b == k), "{k} must reach the shell");
    }
    // Reserved for Lector.
    for k in [
        "ctrl-shift-p",
        "ctrl-`",
        "ctrl-shift-`",
        "f6",
        "shift-f6",
        "ctrl-=",
        "ctrl--",
        "shift-f10",
    ] {
        assert!(!blocked.iter().any(|b| b == k), "{k} must stay with Lector");
    }
}
