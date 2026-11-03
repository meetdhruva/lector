//! Command palette: a picker over every registered user action, showing
//! key bindings; confirming dispatches the action to the previously
//! focused element.

use gpui::{
    prelude::*, px, Action, AnyElement, App, DismissEvent, FocusHandle, SharedString, Window,
};

use crate::actions::{humanize_action_name, is_palette_hidden};
use crate::components::binding_text_for_action;
use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::fuzzy::match_and_sort;

/// One palette entry.
pub struct Command {
    /// Action name (`workspace::ToggleSidebar`).
    pub name: &'static str,
    /// Human label (`Workspace: Toggle Sidebar`).
    pub label: SharedString,
    /// The action.
    pub action: Box<dyn Action>,
    /// Display key binding.
    pub binding: Option<SharedString>,
}

/// Palette delegate.
pub struct CommandPaletteDelegate {
    commands: Vec<Command>,
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    previous_focus: Option<FocusHandle>,
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
                    action,
                    binding,
                })
            })
            .collect();
        commands.sort_by(|a, b| a.label.cmp(&b.label));
        CommandPaletteDelegate {
            commands,
            matches: Vec::new(),
            selected: 0,
            previous_focus,
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
            match_and_sort(query, self.commands.iter().map(|c| c.label.as_ref()))
                .into_iter()
                .map(|(ix, m)| (ix, m.positions))
                .collect()
        };
        self.selected = 0;
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some((ix, _)) = self.matches.get(self.selected) else {
            return;
        };
        let action = self.commands[*ix].action.boxed_clone();
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
        _cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let (cmd_ix, positions) = &self.matches[ix];
        let cmd = &self.commands[*cmd_ix];
        PickerItem::new(cmd.label.clone(), selected)
            .positions(positions.clone())
            .end_text(cmd.binding.clone())
            .into_any_element()
    }

    fn width(&self) -> gpui::Pixels {
        px(640.)
    }
}
