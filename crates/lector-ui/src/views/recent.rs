//! The recent projects picker (`workspace::OpenRecent`): Enter opens the
//! project in this window, the secondary modifier (Ctrl/Cmd+Enter or
//! Ctrl/Cmd+click) opens it in a new window.

use std::path::PathBuf;
use std::rc::Rc;

use gpui::{prelude::*, px, AnyElement, App, DismissEvent, SharedString, Window};

use super::welcome::{recent_entries, RecentEntry};
use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::components::{h_flex, metrics};
use crate::fuzzy::fuzzy_match;
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Called with the chosen root and whether to open a new window.
pub type OpenRecentHandler = Rc<dyn Fn(PathBuf, bool, &mut Window, &mut App)>;

/// Picker delegate over [`lector_config::RecentProjects`].
pub struct RecentProjectsDelegate {
    entries: Vec<RecentEntry>,
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    on_open: OpenRecentHandler,
}

impl RecentProjectsDelegate {
    /// Recent projects except `current_root`.
    pub fn new(current_root: Option<PathBuf>, on_open: OpenRecentHandler) -> Self {
        let entries = recent_entries()
            .into_iter()
            .filter(|e| Some(&e.path) != current_root.as_ref())
            .collect();
        RecentProjectsDelegate {
            entries,
            matches: Vec::new(),
            selected: 0,
            on_open,
        }
    }

    /// Display paths of the current matches (tests).
    pub fn match_paths(&self) -> Vec<SharedString> {
        self.matches
            .iter()
            .map(|(ix, _)| self.entries[*ix].display.clone())
            .collect()
    }
}

impl PickerDelegate for RecentProjectsDelegate {
    fn placeholder(&self) -> SharedString {
        "Open a recent project…".into()
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

    fn update_matches(&mut self, query: &str, _: &mut Window, _: &mut Context<Picker<Self>>) {
        let query = query.trim();
        if query.is_empty() {
            self.matches = (0..self.entries.len()).map(|ix| (ix, Vec::new())).collect();
        } else {
            let mut scored: Vec<(i64, usize, Vec<usize>)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(ix, e)| {
                    let m = fuzzy_match(query, &e.display)?;
                    Some((m.score, ix, m.positions))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            self.matches = scored.into_iter().map(|(_, ix, p)| (ix, p)).collect();
        }
        self.selected = 0;
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some((ix, _)) = self.matches.get(self.selected) else {
            return;
        };
        let entry = &self.entries[*ix];
        if !entry.exists {
            return;
        }
        let new_window = window.modifiers().secondary();
        let path = entry.path.clone();
        let on_open = self.on_open.clone();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| on_open(path, new_window, window, cx));
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let (entry_ix, positions) = &self.matches[ix];
        let entry = &self.entries[*entry_ix];
        let ui = &cx.theme().ui;
        PickerItem::new(entry.name.clone(), selected)
            .icon(
                Icon::new(if entry.exists {
                    IconName::Folder
                } else {
                    IconName::FolderX
                })
                .size(IconSize::Small)
                .color(ui.text_muted.hsla()),
            )
            .when(!entry.exists, |item| item.label_color(ui.text_faint.hsla()))
            .detail(entry.display.clone(), positions.clone())
            .end_text((!entry.exists).then(|| "missing".into()))
            .into_any_element()
    }

    fn empty_message(&self) -> SharedString {
        if self.entries.is_empty() {
            "No recent projects yet".into()
        } else {
            "No matching projects".into()
        }
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let ui = &cx.theme().ui;
        let secondary = if cfg!(target_os = "macos") {
            "⌘↩"
        } else {
            "Ctrl+Enter"
        };
        Some(
            h_flex()
                .px(px(12.))
                .py(px(6.))
                .gap(px(14.))
                .text_size(metrics::TEXT_XS)
                .text_color(ui.text_faint.hsla())
                .child(format!(
                    "{} open in this window",
                    if cfg!(target_os = "macos") {
                        "↩"
                    } else {
                        "Enter"
                    }
                ))
                .child(format!("{secondary} open in a new window"))
                .into_any_element(),
        )
    }
}
