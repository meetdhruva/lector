//! Command palette: a picker over every registered user action, showing
//! key bindings; confirming dispatches the action to the previously
//! focused element. Registry commands (`lector_app::commands`) are listed too:
//! those bound to an action annotate its row with their availability (greyed,
//! reason as tooltip), argument-free ones without an action get their own row
//! and run through `commands_bridge::invoke`.

use gpui::{
    div, prelude::*, px, Action, AnyElement, App, DismissEvent, FocusHandle, SharedString,
    WeakEntity, Window,
};

use crate::actions::{humanize_action_name, is_palette_hidden};
use crate::commands_bridge::PaletteEntry;
use crate::components::binding_text_for_action;
use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::components::tooltip::Tooltip;
use crate::fuzzy::NucleoList;
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::Workspace;

/// One palette entry.
pub struct Command {
    /// Action name (`workspace::ToggleSidebar`).
    pub name: &'static str,
    /// Human label (`Workspace: Toggle Sidebar`).
    pub label: SharedString,
    /// The action (None for registry-only commands).
    pub action: Option<Box<dyn Action>>,
    /// Registry command id run instead of dispatching an action.
    pub command_id: Option<&'static str>,
    /// Display key binding.
    pub binding: Option<SharedString>,
    /// Why the command cannot run now (greyed row, tooltip).
    pub unavailable: Option<SharedString>,
}

/// Palette delegate.
pub struct CommandPaletteDelegate {
    commands: Vec<Command>,
    list: NucleoList<usize>,
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    previous_focus: Option<FocusHandle>,
    workspace: Option<WeakEntity<Workspace>>,
}

impl CommandPaletteDelegate {
    /// Collects commands from all registered actions.
    pub fn new(previous_focus: Option<FocusHandle>, cx: &App) -> Self {
        let mut commands: Vec<Command> = cx
            .all_action_names()
            .iter()
            .filter(|name| !is_palette_hidden(name))
            .filter_map(|name| {
                let action = cx.build_action(name, None).ok()?;
                let binding = binding_text_for_action(action.as_ref(), cx);
                Some(Command {
                    name,
                    label: humanize_action_name(name).into(),
                    action: Some(action),
                    command_id: None,
                    binding,
                    unavailable: None,
                })
            })
            .collect();
        commands.sort_by(|a, b| a.label.cmp(&b.label));
        Self::from_commands(commands, previous_focus, None)
    }

    /// All actions plus the registry commands of a workspace window.
    pub fn with_registry(
        previous_focus: Option<FocusHandle>,
        entries: Vec<PaletteEntry>,
        workspace: WeakEntity<Workspace>,
        cx: &App,
    ) -> Self {
        let mut this = Self::new(previous_focus, cx);
        let mut commands = std::mem::take(&mut this.commands);
        for entry in entries {
            let reason = entry.unavailable.map(SharedString::from);
            match entry.ui_action {
                Some(action) => {
                    if let Some(c) = commands.iter_mut().find(|c| c.name == action) {
                        c.unavailable = reason;
                    }
                }
                None => commands.push(Command {
                    name: entry.id,
                    label: entry.label.into(),
                    action: None,
                    command_id: Some(entry.id),
                    binding: None,
                    unavailable: reason,
                }),
            }
        }
        commands.sort_by(|a, b| a.label.cmp(&b.label));
        Self::from_commands(commands, this.previous_focus.take(), Some(workspace))
    }

    fn from_commands(
        commands: Vec<Command>,
        previous_focus: Option<FocusHandle>,
        workspace: Option<WeakEntity<Workspace>>,
    ) -> Self {
        let list = NucleoList::new(false);
        for (ix, c) in commands.iter().enumerate() {
            list.push(ix, &c.label);
        }
        CommandPaletteDelegate {
            commands,
            list,
            matches: Vec::new(),
            selected: 0,
            previous_focus,
            workspace,
        }
    }

    /// Labels of current matches (tests).
    pub fn match_labels(&self) -> Vec<SharedString> {
        self.matches
            .iter()
            .map(|(ix, _)| self.commands[*ix].label.clone())
            .collect()
    }
}

impl PickerDelegate for CommandPaletteDelegate {
    fn placeholder(&self) -> SharedString {
        "Execute a command…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.matches = if query.trim().is_empty() {
            (0..self.commands.len())
                .map(|ix| (ix, Vec::new()))
                .collect()
        } else {
            self.list.set_query(query.trim());
            self.list.settle(200);
            self.list
                .top(500)
                .into_iter()
                .map(|m| (m.item, m.positions))
                .collect()
        };
        self.selected = 0;
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some((ix, _)) = self.matches.get(self.selected) else {
            return;
        };
        let command = &self.commands[*ix];
        if command.unavailable.is_some() {
            return;
        }
        let Some(action) = command.action.as_ref().map(|a| a.boxed_clone()) else {
            let (Some(id), Some(ws)) = (command.command_id, self.workspace.clone()) else {
                return;
            };
            cx.emit(DismissEvent);
            if let Some(focus) = self.previous_focus.clone() {
                window.focus(&focus);
            }
            window.defer(cx, move |window, cx| {
                if let Some(ws) = ws.upgrade() {
                    ws.update(cx, |ws, cx| {
                        crate::commands_bridge::invoke(
                            ws,
                            id,
                            serde_json::Value::Null,
                            true,
                            window,
                            cx,
                        )
                    });
                }
            });
            return;
        };
        cx.emit(DismissEvent);
        match self.previous_focus.clone() {
            Some(focus) => {
                window.focus(&focus);
                let action = action.boxed_clone();
                window.defer(cx, move |window, cx| {
                    focus.dispatch_action(action.as_ref(), window, cx)
                });
            }
            None => window.dispatch_action(action, cx),
        }
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let (cmd_ix, positions) = &self.matches[ix];
        let cmd = &self.commands[*cmd_ix];
        let item = PickerItem::new(cmd.label.clone(), selected)
            .positions(positions.clone())
            .end_text(cmd.binding.clone());
        match &cmd.unavailable {
            None => item.into_any_element(),
            Some(why) => div()
                .id(("palette-unavailable", ix))
                .tooltip(Tooltip::text(why.clone()))
                .child(item.label_color(cx.theme().ui.text_faint.hsla()))
                .into_any_element(),
        }
    }

    fn width(&self) -> gpui::Pixels {
        px(640.)
    }
}
