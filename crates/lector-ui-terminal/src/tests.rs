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
    let root = dunce::canonicalize(dir.path()).expect("canonical");
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

/// "Open in Terminal" from a context menu leaves the focus in the new terminal: the menu
/// restores the previous focus (the file tree) after its entry ran, which must not win.
#[gpui::test]
fn open_in_terminal_from_a_context_menu_focuses_the_terminal(cx: &mut TestAppContext) {
    use lector_ui::components::context_menu::ContextMenu;
    use lector_ui::components::popover::Popover;

    init(cx, settings_with(|_| {}));
    let f = fixture();
    let (ws, panel, cx) = open_workspace(cx, &f.root);
    let root = f.root.clone();
    let before = cx.update(|window, cx| window.focused(cx));
    let menu = cx.update(|window, cx| {
        ContextMenu::build(window, cx, move |menu, _, _| {
            menu.entry("Open in Terminal", move |window, cx| {
                lector_ui::terminal_hook::open_in_terminal(root.clone(), window, cx)
            })
        })
    });
    let mut popover: Popover<ContextMenu> = Popover::new();
    ws.update_in(cx, |_, window, cx| {
        popover.show(
            menu.clone(),
            gpui::point(gpui::px(10.), gpui::px(10.)),
            gpui::Corner::TopLeft,
            window,
            cx,
            |_, _, _| {},
        )
    });
    menu.update_in(cx, |m, window, cx| m.activate(0, window, cx));
    cx.run_until_parked();
    let terminal = panel
        .read_with(cx, |p, _| p.active_terminal())
        .expect("terminal opened");
    assert!(
        cx.update(|window, cx| terminal.read(cx).focus_handle.is_focused(window)),
        "the menu's focus restore won (before: {})",
        before.is_some()
    );
    drop(popover);
}

fn pending_flags(panel: &Entity<TerminalPanel>, cx: &mut VisualTestContext) -> Vec<bool> {
    cx.update(|_, cx| {
        panel
            .read(cx)
            .terminals()
            .iter()
            .map(|v| v.read(cx).is_pending())
            .collect()
    })
}

fn redraw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

/// Restored tabs are placeholders: nothing starts while the panel is hidden, and only the
/// tab that is shown starts its shell.
#[gpui::test]
fn restore_is_lazy_until_a_tab_is_shown(cx: &mut TestAppContext) {
    use crate::session::SavedTab;

    init(cx, settings_with(|_| {}));
    let f = fixture();
    let (ws, panel, cx) = open_workspace(cx, &f.root);
    let tabs = vec![
        SavedTab {
            cwds: vec![f.root.clone(), f.root.join("src")],
            title: Some("build".into()),
            vertical: false,
        },
        SavedTab {
            cwds: vec![f.root.join("src/deep")],
            ..SavedTab::default()
        },
    ];
    panel.update_in(cx, |p, window, cx| p.restore(tabs, window, cx));
    redraw(cx);
    assert_eq!(pending_flags(&panel, cx), vec![true, true, true]);
    assert!(!ws.read_with(cx, |ws, _| ws.is_bottom_panel_visible()));
    // Titles and folders are known without a shell.
    panel.read_with(cx, |p, _| {
        assert_eq!(p.model.tabs().len(), 2);
    });
    // Showing the panel starts the active tab (both splits) only.
    cx.dispatch_action(actions::TogglePanel);
    redraw(cx);
    assert!(ws.read_with(cx, |ws, _| ws.is_bottom_panel_visible()));
    let (active, flags) = (
        panel.read_with(cx, |p, _| p.model.active_index()),
        pending_flags(&panel, cx),
    );
    let per_tab: Vec<Vec<bool>> = {
        let mut i = 0;
        panel.read_with(cx, |p, _| {
            p.model
                .tabs()
                .iter()
                .map(|t| {
                    let n = t.panes.len();
                    i += n;
                    flags[i - n..i].to_vec()
                })
                .collect()
        })
    };
    assert!(per_tab[active].iter().all(|pending| !pending));
    assert!(per_tab
        .iter()
        .enumerate()
        .filter(|(ix, _)| *ix != active)
        .all(|(_, tab)| tab.iter().all(|pending| *pending)));
    // Selecting the other tab starts it.
    panel.update_in(cx, |p, window, cx| p.activate_relative(1, window, cx));
    redraw(cx);
    assert!(pending_flags(&panel, cx).iter().all(|pending| !pending));
}

/// Two tabs in the same folder are told apart ("repo", "repo (2)").
#[gpui::test]
fn tabs_with_the_same_title_are_numbered(cx: &mut TestAppContext) {
    init(cx, settings_with(|_| {}));
    let f = fixture();
    let (_ws, panel, cx) = open_workspace(cx, &f.root);
    for _ in 0..3 {
        cx.dispatch_action(actions::NewTerminal);
        cx.run_until_parked();
    }
    let titles: Vec<String> = cx.update(|_, cx| {
        let panel = panel.read(cx);
        (0..3).map(|ix| panel.unique_title(ix, cx)).collect()
    });
    let name = f.root.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        titles,
        vec![name.clone(), format!("{name} (2)"), format!("{name} (3)")]
    );
}

/// Ctrl+Shift+W on an empty panel hides it, and right after the last terminal went it does not
/// close the window (the shortcut means Close Window once the editor has the focus).
#[gpui::test]
fn kill_terminal_never_closes_the_window(cx: &mut TestAppContext) {
    use lector_ui::actions::workspace::CloseWindow;

    init(cx, settings_with(|_| {}));
    let f = fixture();
    let (ws, panel, cx) = open_workspace(cx, &f.root);
    cx.dispatch_action(actions::TogglePanel);
    cx.run_until_parked();
    assert_eq!(panel.read_with(cx, |p, _| p.terminals().len()), 1);
    cx.dispatch_action(actions::KillTerminal);
    cx.run_until_parked();
    assert!(panel.read_with(cx, |p, _| p.terminals().is_empty()));
    assert!(!ws.read_with(cx, |ws, _| ws.is_bottom_panel_visible()));
    // The second press arrives with the editor focused: swallowed once, with a hint.
    cx.update(|window, cx| {
        let focus = ws.focus_handle(cx);
        window.focus(&focus);
    });
    cx.dispatch_action(CloseWindow);
    cx.run_until_parked();
    let toast = ws.read_with(cx, |ws, _| ws.toast_message());
    assert!(
        toast.is_some_and(|t| t.contains("stays open")),
        "the window must not close"
    );
    // An empty panel that is shown and focused hides on the shortcut.
    ws.update_in(cx, |ws, window, cx| {
        ws.set_bottom_panel_visible(true, window, cx)
    });
    cx.update(|window, cx| {
        let focus = panel.read(cx).focus_handle(cx);
        window.focus(&focus);
    });
    cx.dispatch_action(actions::KillTerminal);
    cx.run_until_parked();
    assert!(!ws.read_with(cx, |ws, _| ws.is_bottom_panel_visible()));
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
            .on_action(cx.listener(|h, _: &actions::Clear, _, _| h.actions.push("clear")))
            .on_action(
                cx.listener(|h, _: &lector_ui::actions::git::Push, _, _| h.actions.push("push")),
            )
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

/// The shipped defaults of `platform`, as lector-app binds them, plus the terminal pass-through.
fn bind_defaults(cx: &mut App, platform: lector_config::Platform) {
    let mut specs: Vec<KeySpec> = lector_config::default_keymap(platform)
        .into_iter()
        .map(|k| KeySpec {
            keys: k.keys,
            action: k.action,
            context: k.context,
        })
        .collect();
    let extra = passthrough_bindings(&specs);
    specs.extend(extra);
    lector_ui::keymap::bind_keymap(cx, &specs);
}

/// macOS: Cmd+K clears the focused terminal at once. The `cmd-k` chords of the workspace
/// (push, pull, ...) neither delay it nor can be reached from a terminal.
#[gpui::test]
fn cmd_k_clears_a_terminal_immediately_and_sync_chords_stay_out(cx: &mut TestAppContext) {
    let (harness, cx) = cx.add_window_view(|_, cx| Harness {
        terminal: cx.focus_handle(),
        keys: Vec::new(),
        actions: Vec::new(),
    });
    cx.update(|window, cx| {
        bind_defaults(cx, lector_config::Platform::MacOS);
        let focus = harness.read(cx).terminal.clone();
        window.focus(&focus);
    });
    // No timer is advanced: a chord prefix would still be waiting for its second key.
    cx.simulate_keystrokes("cmd-k");
    harness.read_with(cx, |h, _| {
        assert_eq!(h.actions, vec!["clear"], "keys: {:?}", h.keys)
    });
    cx.simulate_keystrokes("cmd-k p");
    harness.read_with(cx, |h, _| {
        assert_eq!(h.actions, vec!["clear", "clear"], "push must not run");
        assert_eq!(h.keys.last().map(String::as_str), Some("p"));
    });
}

/// Linux / Windows: Ctrl+K is the shell's, and `ctrl-k p` does not push from a terminal.
#[gpui::test]
fn ctrl_k_reaches_the_shell_and_sync_chords_stay_out(cx: &mut TestAppContext) {
    let (harness, cx) = cx.add_window_view(|_, cx| Harness {
        terminal: cx.focus_handle(),
        keys: Vec::new(),
        actions: Vec::new(),
    });
    cx.update(|window, cx| {
        bind_defaults(cx, lector_config::Platform::Linux);
        let focus = harness.read(cx).terminal.clone();
        window.focus(&focus);
    });
    cx.simulate_keystrokes("ctrl-k p");
    harness.read_with(cx, |h, _| {
        assert!(h.actions.is_empty(), "nothing ran: {:?}", h.actions);
        assert_eq!(h.keys, vec!["ctrl-k", "p"]);
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
