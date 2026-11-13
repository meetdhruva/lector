//! The Agent panel (v0.5): drive a coding agent over ACP from Lector and review each turn's
//! changes as a changeset. Registered into every workspace window by [`init`], like
//! `lector-ui-git`: a sidebar panel (activity-bar icon), the `agent::*` actions, and the
//! `agent.*` commands in the core's registry (palette, CLI, MCP).
#![forbid(unsafe_code)]

pub mod feedback;
pub mod health_model;
pub mod inline_edit;
pub mod notices;
pub mod panel;
mod render;
mod summary_ui;
pub mod update_model;
mod update_ui;

use gpui::{App, AppContext, Entity, Window};
use lector_agent::AgentManager;
use lector_ui::actions::agent as agent_actions;
use lector_ui::views::sidebar::SidebarPanel;
use lector_ui::views::Workspace;

pub use panel::AgentPanel;

/// The agent panel of each workspace window.
#[derive(Default)]
struct PanelsByWorkspace(std::collections::HashMap<gpui::EntityId, gpui::WeakEntity<AgentPanel>>);

impl gpui::Global for PanelsByWorkspace {}

/// The agent panel of a workspace window.
pub fn panel_for(workspace: gpui::EntityId, cx: &App) -> Option<Entity<AgentPanel>> {
    cx.try_global::<PanelsByWorkspace>()?
        .0
        .get(&workspace)?
        .upgrade()
}

/// Registers the agent commands and the Agent panel with every new workspace window.
/// Call after `lector_ui::init` and `lector_ui_git::init`.
pub fn init(cx: &mut App) {
    let core = lector_ui::commands_bridge::core(cx);
    lector_agent::commands::register(&core);
    cx.set_global(PanelsByWorkspace::default());
    update_ui::init(cx);
    cx.on_app_quit(move |_| {
        AgentManager::of(&core).stop_all();
        async {}
    })
    .detach();
    cx.observe_new(|ws: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !ws.has_folder() {
            return;
        }
        let workspace = cx.entity();
        let panel = cx.new(|cx| AgentPanel::new(&workspace, ws.project().clone(), window, cx));
        cx.default_global::<PanelsByWorkspace>()
            .0
            .insert(workspace.entity_id(), panel.downgrade());
        ws.sidebar().update(cx, |sb, cx| {
            sb.set_panel_view(SidebarPanel::Agent, panel.clone().into(), cx)
        });
        register_actions(ws, &panel);
        let inline = cx.new(|cx| {
            inline_edit::InlineEdits::new(
                cx.weak_entity(),
                workspace.downgrade(),
                panel.downgrade(),
            )
        });
        inline_edit::register_actions(ws, &inline);
    })
    .detach();
}

fn register_actions(ws: &mut Workspace, panel: &Entity<AgentPanel>) {
    ws.register_action(|ws, _: &agent_actions::TogglePanel, window, cx| {
        ws.show_panel(SidebarPanel::Agent, window, cx);
        ws.ensure_sidebar_width(PANEL_WIDTH, cx);
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &agent_actions::NewSession, window, cx| {
        reveal(ws, window, cx);
        let p = p.clone();
        window.defer(cx, move |window, cx| {
            p.update(cx, |p, cx| p.new_session(window, cx));
        });
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &agent_actions::Cancel, _, cx| {
        p.update(cx, |p, cx| p.cancel(cx));
    });
    let p = panel.clone();
    ws.register_action(move |_, _: &agent_actions::ReviewLastTurn, window, cx| {
        let p = p.clone();
        window.defer(cx, move |window, cx| {
            let last = p.read(cx).last_turn_id();
            match last {
                Some(id) => p.update(cx, |p, cx| p.review_turn(id, window, cx)),
                None => p.update(cx, |p, cx| p.toast("The agent has no turns yet.", cx)),
            }
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &agent_actions::UndoLastTurn, window, cx| {
        reveal(ws, window, cx);
        let p = p.clone();
        window.defer(cx, move |_, cx| {
            let last = p.read(cx).last_turn_id();
            match last {
                Some(id) => p.update(cx, |p, cx| p.undo_turn(id, false, cx)),
                None => p.update(cx, |p, cx| p.toast("The agent has no turns yet.", cx)),
            }
        });
    });
    let p = panel.clone();
    ws.register_action(move |ws, _: &agent_actions::SendFeedback, window, cx| {
        let chip = feedback::selection_feedback(ws, cx);
        reveal(ws, window, cx);
        let p = p.clone();
        window.defer(cx, move |window, cx| {
            p.update(cx, |p, cx| p.add_feedback(chip, window, cx));
        });
    });
}

/// Shows the Agent panel (without toggling it away when already visible).
fn reveal(ws: &mut Workspace, window: &mut Window, cx: &mut gpui::Context<Workspace>) {
    let visible =
        ws.is_sidebar_visible() && ws.sidebar().read(cx).active_panel() == SidebarPanel::Agent;
    if !visible {
        ws.show_panel(SidebarPanel::Agent, window, cx);
    }
    ws.ensure_sidebar_width(PANEL_WIDTH, cx);
}

/// Width the sidebar gets when the Agent panel is shown (a conversation needs room).
const PANEL_WIDTH: gpui::Pixels = gpui::px(400.);

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, WeakEntity};
    use lector_app::commands::{CommandContext, Source};
    use lector_app::{AppCore, CoreOptions};
    use lector_theme::{Appearance, ThemeRegistry};
    use lector_ui::components::picker::Picker;
    use lector_ui::theme::{ThemeMode, ThemeState};
    use lector_ui::views::command_palette::CommandPaletteDelegate;

    /// With every command of the app registered (agent commands included), the palette lists each
    /// title once, never doubles a namespace ("Agent: Agent: ...") and runs no destructive
    /// command without a UI flow or a confirmation.
    #[gpui::test]
    fn palette_over_the_full_registry_is_unique_and_guarded(cx: &mut TestAppContext) {
        cx.update(|cx: &mut gpui::App| {
            ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        });
        let core = AppCore::open(CoreOptions::for_tests());
        lector_agent::commands::register(&core);
        let ctx = CommandContext::new(core.clone(), None, Source::Ui);
        let entries = lector_ui::commands_bridge::palette_entries(&ctx);
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let delegate =
                CommandPaletteDelegate::with_registry(None, entries, WeakEntity::new_invalid(), cx);
            Picker::new(delegate, window, cx)
        });
        let (labels, rows) = picker.read_with(cx, |p, _| {
            (p.delegate.all_labels(), p.delegate.registry_rows())
        });
        let mut seen = std::collections::HashSet::new();
        for label in &labels {
            assert!(seen.insert(label.clone()), "`{label}` is listed twice");
            assert!(!label.contains("Agent: Agent:"), "doubled prefix: {label}");
        }
        assert!(labels.iter().any(|l| l.as_ref() == "Agent: List Agents"));
        for command in core.registry().list() {
            let spec = command.spec();
            if spec.destructive && spec.ui_action.is_none() && spec.ui_flow.is_none() {
                if let Some((_, confirm)) = rows.iter().find(|(id, _)| *id == spec.id) {
                    assert!(confirm, "`{}` runs from the palette unconfirmed", spec.id);
                }
            }
        }
        // The agent commands that have a panel flow do not get a second row.
        for id in ["agent.start", "agent.undo_turn", "agent.send_comments"] {
            assert!(!rows.iter().any(|(r, _)| *r == id), "{id} duplicated");
        }
    }
}
