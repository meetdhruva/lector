//! Theme selector: a picker over registered themes with live preview.

use gpui::{prelude::*, AnyElement, DismissEvent, SharedString, Window};

use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ThemeMode, ThemeState};
use lector_base::fuzzy::{Matcher, Query};

/// Theme selector delegate.
pub struct ThemeSelectorDelegate {
    names: Vec<(SharedString, bool)>,
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    original_mode: ThemeMode,
    original_name: SharedString,
    confirmed: bool,
}

impl ThemeSelectorDelegate {
    /// Selector over all registered themes.
    pub fn new(cx: &gpui::App) -> Self {
        let state = cx.global::<ThemeState>();
        let names = state
            .registry()
            .themes()
            .iter()
            .map(|t| (SharedString::from(t.name.clone()), t.appearance.is_dark()))
            .collect();
        let original_name = SharedString::from(crate::ActiveTheme::theme(cx).name.clone());
        ThemeSelectorDelegate {
            names,
            matches: Vec::new(),
            selected: 0,
            original_mode: state.mode().clone(),
            original_name,
            confirmed: false,
        }
    }

    fn preview(&self, cx: &mut gpui::App) {
        if let Some((ix, _)) = self.matches.get(self.selected) {
            ThemeState::select_theme(cx, &self.names[*ix].0);
        }
    }
}

impl PickerDelegate for ThemeSelectorDelegate {
    fn placeholder(&self) -> SharedString {
        "Select a theme…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, cx: &mut Context<Picker<Self>>) {
        self.selected = ix;
        self.preview(cx);
    }

    fn update_matches(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.matches = Matcher::new()
            .rank_by(
                &Query::new(query),
                &self.names,
                |(n, _)| n.as_ref(),
                usize::MAX,
            )
            .into_iter()
            .map(|r| (r.index, r.positions))
            .collect();
        self.selected = self
            .matches
            .iter()
            .position(|(ix, _)| self.names[*ix].0 == self.original_name)
            .unwrap_or(0);
    }

    fn confirm(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.confirmed = true;
        self.preview(cx);
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if !self.confirmed {
            let mode = self.original_mode.clone();
            ThemeState::set_mode(cx, mode);
        }
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let (name_ix, positions) = &self.matches[ix];
        let (name, dark) = &self.names[*name_ix];
        PickerItem::new(name.clone(), selected)
            .icon(
                Icon::new(if *dark { IconName::Moon } else { IconName::Sun }).size(IconSize::Small),
            )
            .positions(positions.clone())
            .into_any_element()
    }
}
