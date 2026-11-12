//! The integrated multi-tab terminal (v0.7): a bottom Terminal panel with tabs and splits,
//! terminals as editor-area tabs, "Open in Terminal" context-menu entries, the `terminal::*`
//! actions, a pass-through key context and session restore. Built on the UI-free core in
//! `lector-terminal`; registered into every workspace window by [`init`].
//!
//! Security rules (see `docs/ARCHITECTURE.md`, "lector-ui-terminal"): Lector never writes
//! to a terminal on its own (no auto-run; "Open in Terminal" only picks the folder); only
//! http/https/mailto links open, file links are resolved off the UI thread; multi-line pastes
//! without bracketed paste ask first; the shell program/args/env come from user settings
//! only; programs may copy to the clipboard (OSC 52) only while their terminal is focused and
//! can never read it.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod colors;
pub mod config;
pub mod element;
pub mod grid;
pub mod keys;
pub mod model;
pub mod panel;
pub mod routing;
pub mod session;
pub mod view;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use gpui::{
    App, AppContext as _, Context, Entity, EntityId, FocusHandle, Focusable, WeakEntity, Window,
};
use lector_terminal::{Direction, Terminal};
use lector_ui::actions::terminal as actions;
use lector_ui::views::pane::{PaneItem, TabIcon};
use lector_ui::views::{Workspace, WorkspaceRoot};

pub use config::CwdRequest;
pub use model::SplitAxis;
pub use panel::{TerminalPanel, TerminalPanelEvent};
pub use view::{TerminalView, TerminalViewEvent};

/// Live terminals and panels of the process.
#[derive(Default)]
struct Registry {
    next_id: u64,
    sessions: Vec<Weak<Terminal>>,
    views: HashMap<u64, WeakEntity<TerminalView>>,
    in_panel: HashSet<u64>,
    panels: HashMap<EntityId, WeakEntity<TerminalPanel>>,
}

impl gpui::Global for Registry {}

pub(crate) fn next_id(cx: &mut App) -> u64 {
    let r = cx.default_global::<Registry>();
    r.next_id += 1;
    r.next_id
}

pub(crate) fn registry_add(term: &Arc<Terminal>, cx: &mut App) {
    let r = cx.default_global::<Registry>();
    r.sessions.retain(|w| w.strong_count() > 0);
    r.sessions.push(Arc::downgrade(term));
}

pub(crate) fn set_in_panel(id: u64, in_panel: bool, cx: &mut App) {
    let r = cx.default_global::<Registry>();
    if in_panel {
        r.in_panel.insert(id);
    } else {
        r.in_panel.remove(&id);
    }
}

pub(crate) fn is_in_panel(id: u64, cx: &App) -> bool {
    cx.try_global::<Registry>()
        .is_some_and(|r| r.in_panel.contains(&id))
}

fn register_view(view: &Entity<TerminalView>, cx: &mut App) {
    let id = view.read(cx).id;
    let r = cx.default_global::<Registry>();
    r.views.retain(|_, w| w.upgrade().is_some());
    r.views.insert(id, view.downgrade());
}

/// The Terminal panel of a workspace window.
pub fn panel_for(workspace: EntityId, cx: &App) -> Option<Entity<TerminalPanel>> {
    cx.try_global::<Registry>()?
        .panels
        .get(&workspace)?
        .upgrade()
}

/// Kills every terminal of the process (app quit). Dropping views also kills, this makes it
/// deterministic before the process exits.
pub fn kill_all(cx: &mut App) {
    if let Some(r) = cx.try_global::<Registry>() {
        for term in r.sessions.iter().filter_map(Weak::upgrade) {
            term.kill();
        }
    }
}

/// A terminal shown as a tab in the editor area.
pub struct TerminalItem {
    view: Entity<TerminalView>,
}

impl TerminalItem {
    /// Wraps `view`.
    pub fn new(view: Entity<TerminalView>) -> Self {
        Self { view }
    }
}

impl PaneItem for TerminalItem {
    fn title(&self, cx: &App) -> gpui::SharedString {
        self.view.read(cx).display_title().into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(lector_ui::icons::IconName::Terminal)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn view(&self) -> gpui::AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn clone_item(&self, _: &mut Window, _: &mut App) -> Option<Box<dyn PaneItem>> {
        None
    }

    fn tab_kind(&self, _: &App) -> Option<lector_config::TabKind> {
        None
    }

    fn item_key(&self, cx: &App) -> Option<gpui::SharedString> {
        Some(format!("terminal:{}", self.view.read(cx).id).into())
    }
}

/// The workspace of `window`, if it shows one.
fn workspace_of(window: &mut Window, cx: &App) -> Option<Entity<Workspace>> {
    let root = window.root::<WorkspaceRoot>().flatten()?;
    Some(root.read(cx).workspace.clone())
}

/// Registers the terminal with the app: key pass-through, "Open in Terminal", quit handling
/// and the Terminal panel of every workspace window. Call after `lector_ui::init`.
pub fn init(cx: &mut App) {
    cx.default_global::<Registry>();
    lector_ui::keymap::add_keymap_extension(cx, keys::passthrough_bindings);
    lector_ui::settings::bind_effective_keymap(cx);
    lector_ui::terminal_hook::set_open_in_terminal(cx, |path, window, cx| {
        let Some(ws) = workspace_of(window, cx) else {
            return;
        };
        let Some(panel) = panel_for(ws.entity_id(), cx) else {
            return;
        };
        panel.update(cx, |p, cx| {
            p.new_terminal(CwdRequest::Explicit(path), None, None, window, cx);
        });
    });
    cx.on_app_quit(|cx| {
        save_all_sessions_now(cx);
        kill_all(cx);
        async {}
    })
    .detach();
    cx.observe_new(|ws: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace = cx.entity();
        let panel = cx.new(|cx| TerminalPanel::new(workspace.downgrade(), window, cx));
        cx.default_global::<Registry>()
            .panels
            .insert(workspace.entity_id(), panel.downgrade());
        let focus = panel.focus_handle(cx);
        ws.set_bottom_panel(panel.clone().into(), focus, cx);
        register_actions(ws, &panel);
        // Register new views and save the session when tabs change.
        let has_folder = ws.has_folder();
        let root = ws.root().to_path_buf();
        cx.subscribe(&panel, move |_, panel, _: &TerminalPanelEvent, cx| {
            let views = panel.read(cx).terminals();
            for v in &views {
                register_view(v, cx);
            }
            if has_folder && view::terminal_settings(cx).restore {
                let root = root.clone();
                panel.update(cx, |p, cx| p.schedule_save(root, cx));
            }
        })
        .detach();
        if has_folder && view::terminal_settings(cx).restore {
            let root = ws.root().to_path_buf();
            let panel = panel.downgrade();
            cx.spawn_in(window, async move |_, cx| {
                let load =
                    cx.background_spawn(
                        async move { session::load(&session::session_path(), &root) },
                    );
                let tabs = load.await;
                if tabs.is_empty() {
                    return;
                }
                let _ = panel.update_in(cx, |p, window, cx| p.restore(tabs, window, cx));
            })
            .detach();
        }
    })
    .detach();
}

fn save_all_sessions_now(cx: &mut App) {
    if !view::terminal_settings(cx).restore {
        return;
    }
    let Some(r) = cx.try_global::<Registry>() else {
        return;
    };
    let panels: Vec<Entity<TerminalPanel>> =
        r.panels.values().filter_map(WeakEntity::upgrade).collect();
    let path = session::session_path();
    for panel in panels {
        let p = panel.read(cx);
        let Some(ws) = p.workspace_handle().upgrade() else {
            continue;
        };
        let ws = ws.read(cx);
        if !ws.has_folder() {
            continue;
        }
        let tabs = p.saved_tabs(cx);
        if let Err(err) = session::store(&path, ws.root(), &tabs) {
            tracing::warn!("terminal session: {err}");
        }
    }
}

/// The terminal an action applies to: the focused one (panel or editor area), else the
/// active editor tab when it is a terminal, else the panel's active terminal.
fn target_terminal(
    ws: &Workspace,
    panel: &Entity<TerminalPanel>,
    window: &Window,
    cx: &App,
) -> Option<Entity<TerminalView>> {
    if let Some(r) = cx.try_global::<Registry>() {
        for view in r.views.values().filter_map(WeakEntity::upgrade) {
            let v = view.read(cx);
            let find_focused = v
                .find
                .as_ref()
                .is_some_and(|f| f.input.focus_handle(cx).contains_focused(window, cx));
            if v.focus_handle.contains_focused(window, cx) || find_focused {
                return Some(view);
            }
        }
    }
    if let Some(view) = active_editor_terminal(ws, cx) {
        return Some(view);
    }
    panel.read(cx).active_terminal()
}

fn active_editor_terminal(ws: &Workspace, cx: &App) -> Option<Entity<TerminalView>> {
    let key = ws.active_pane().read(cx).active_item()?.item_key(cx)?;
    let id: u64 = key.strip_prefix("terminal:")?.parse().ok()?;
    cx.try_global::<Registry>()?.views.get(&id)?.upgrade()
}

/// Runs `f` on the target terminal after the workspace's action handler returns (views and
/// the panel read and update the workspace).
fn with_target(
    ws: &mut Workspace,
    panel: &Entity<TerminalPanel>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut TerminalView, &mut Window, &mut Context<TerminalView>) + 'static,
) {
    if let Some(view) = target_terminal(ws, panel, window, cx) {
        window.defer(cx, move |window, cx| {
            view.update(cx, |v, cx| f(v, window, cx))
        });
    }
}

/// Runs `f` on the panel after the workspace's action handler returns.
fn defer_panel(
    panel: &Entity<TerminalPanel>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut TerminalPanel, &mut Window, &mut Context<TerminalPanel>) + 'static,
) {
    let panel = panel.clone();
    window.defer(cx, move |window, cx| {
        panel.update(cx, |p, cx| f(p, window, cx))
    });
}

fn register_actions(ws: &mut Workspace, panel: &Entity<TerminalPanel>) {
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::TogglePanel, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| p.toggle(window, cx));
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::NewTerminal, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.new_terminal(CwdRequest::Default, None, None, window, cx);
        });
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::NewTerminalWithProfile, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.choose_profile(window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::OpenInTerminal, window, cx| {
        let request = ws
            .active_path(cx)
            .map(CwdRequest::Explicit)
            .unwrap_or(CwdRequest::Default);
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.new_terminal(request, None, None, window, cx);
        });
    });
    for axis in [SplitAxis::Horizontal, SplitAxis::Vertical] {
        let p = panel.clone();
        let handler = move |ws: &mut Workspace,
                            window: &mut Window,
                            cx: &mut Context<Workspace>| {
            let cwd = target_terminal(ws, &p, window, cx).map(|v| v.read(cx).cwd().to_path_buf());
            defer_panel(&p, window, cx, move |p, window, cx| {
                // Splitting a terminal of the editor area splits in the panel, from its folder.
                let request = cwd.map(CwdRequest::Explicit).unwrap_or(CwdRequest::Default);
                p.new_terminal(request, None, Some(axis), window, cx);
            });
        };
        match axis {
            SplitAxis::Horizontal => {
                ws.register_action(move |ws, _: &actions::SplitTerminal, w, cx| handler(ws, w, cx));
            }
            SplitAxis::Vertical => {
                ws.register_action(move |ws, _: &actions::SplitTerminalDown, w, cx| {
                    handler(ws, w, cx)
                });
            }
        }
    }
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::FocusNextSplit, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.focus_split_relative(1, window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::FocusPrevSplit, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.focus_split_relative(-1, window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::NextTerminal, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.activate_relative(1, window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::PrevTerminal, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.activate_relative(-1, window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::KillTerminal, window, cx| {
        let Some(view) = target_terminal(ws, &p, window, cx) else {
            return;
        };
        if p.read(cx).contains(&view) {
            defer_panel(&p, window, cx, move |p, window, cx| {
                p.kill(view, window, cx)
            });
        } else {
            // An editor-area terminal: closing its tab drops (and kills) it.
            close_editor_terminal(ws, &view, window, cx);
        }
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &actions::Rename, window, cx| {
        defer_panel(&p, window, cx, move |p, window, cx| {
            p.start_rename(window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::Clear, window, cx| {
        with_target(ws, &p, window, cx, |v, _, cx| v.clear(cx));
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::Copy, window, cx| {
        with_target(ws, &p, window, cx, |v, _, cx| {
            v.copy(cx);
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::Paste, window, cx| {
        with_target(ws, &p, window, cx, |v, window, cx| v.paste(window, cx));
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::SelectAll, window, cx| {
        with_target(ws, &p, window, cx, |v, _, cx| v.select_all(cx));
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::Find, window, cx| {
        ensure_panel_shown_for_target(ws, &p, window, cx);
        with_target(ws, &p, window, cx, |v, window, cx| v.open_find(window, cx));
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::FindNext, window, cx| {
        with_target(ws, &p, window, cx, |v, window, cx| {
            v.find_next(Direction::Next, window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::FindPrev, window, cx| {
        with_target(ws, &p, window, cx, |v, window, cx| {
            v.find_next(Direction::Previous, window, cx)
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::CloseFind, window, cx| {
        with_target(ws, &p, window, cx, |v, window, cx| v.close_find(window, cx));
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::ScrollToPrevCommand, window, cx| {
        with_target(ws, &p, window, cx, |v, _, cx| {
            v.scroll_to_command(Direction::Previous, cx);
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::ScrollToNextCommand, window, cx| {
        with_target(ws, &p, window, cx, |v, _, cx| {
            v.scroll_to_command(Direction::Next, cx);
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::MoveToEditor, window, cx| {
        let Some(view) = target_terminal(ws, &p, window, cx) else {
            return;
        };
        if !p.update(cx, |p, cx| p.release(&view, cx)) {
            return;
        }
        if p.read(cx).terminals().is_empty() {
            ws.set_bottom_panel_visible(false, window, cx);
        }
        ws.add_item(Box::new(TerminalItem::new(view.clone())), false, window, cx);
        let handle = view.read(cx).focus_handle.clone();
        window.focus(&handle);
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &actions::MoveToPanel, window, cx| {
        let Some(view) = target_terminal(ws, &p, window, cx) else {
            return;
        };
        if p.read(cx).contains(&view) {
            return;
        }
        // Take it into the panel first so closing the editor tab does not drop it.
        p.update(cx, |p, cx| p.adopt(view.clone(), None, cx));
        remove_editor_terminal(ws, &view, window, cx);
        ws.set_bottom_panel_visible(true, window, cx);
        let handle = view.read(cx).focus_handle.clone();
        window.focus(&handle);
    });
}

fn ensure_panel_shown_for_target(
    ws: &mut Workspace,
    panel: &Entity<TerminalPanel>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(view) = target_terminal(ws, panel, window, cx) {
        if panel.read(cx).contains(&view) && !ws.is_bottom_panel_visible() {
            ws.set_bottom_panel_visible(true, window, cx);
        }
    }
}

/// Removes an editor-area terminal's tab (the view lives on if someone else holds it).
fn remove_editor_terminal(
    ws: &mut Workspace,
    view: &Entity<TerminalView>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let key = format!("terminal:{}", view.read(cx).id);
    for pane in ws.panes() {
        let ix = pane.read(cx).index_for_key(&key, cx);
        if let Some(ix) = ix {
            pane.update(cx, |pane, cx| pane.close_without_prompt(ix, window, cx));
        }
    }
}

fn close_editor_terminal(
    ws: &mut Workspace,
    view: &Entity<TerminalView>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    view.update(cx, |v, _| v.kill());
    remove_editor_terminal(ws, view, window, cx);
}
