//! The keyboard shortcuts viewer (`workspace::ShowKeybindings`): a searchable
//! table of the live key map (command, keys, context). Filter by typing a
//! command name or keys, or press "Record Keys" and type the shortcut itself.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::scale::px;
use gpui::{
    div, prelude::*, AnyElement, AnyView, App, Entity, FocusHandle, Focusable, FontWeight,
    KeyBinding, Keystroke, SharedString, Subscription, Window,
};

use super::pane::{PaneItem, TabIcon};
use super::welcome::keycap;
use crate::actions::{humanize_action_name, is_palette_hidden, workspace::OpenKeymap};
use crate::components::list::{virtual_list, VirtualListState};
use crate::components::text_input::{TextInput, TextInputEvent};
use crate::components::{
    format_binding, format_keystroke, h_flex, metrics, v_flex, Button, ButtonStyle,
};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Pane item key of the viewer tab.
pub const KEYBINDINGS_ITEM_KEY: &str = "lector:keybindings";

/// One row of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRow {
    /// "Workspace: Toggle Sidebar".
    pub label: SharedString,
    /// "workspace::ToggleSidebar".
    pub action: SharedString,
    /// "Ctrl+B" (chords space separated).
    pub keys: SharedString,
    /// Key context predicate, or "" for global bindings.
    pub context: SharedString,
    /// Keystrokes as (key, modifiers) for matching recorded input.
    strokes: Vec<(String, gpui::Modifiers)>,
    /// Lowercased haystack for filtering.
    haystack: String,
    /// Whether the action is a user command (palette-visible).
    command: bool,
}

/// Rows of the live key map: later bindings take precedence, so a binding
/// shadowed by a later one with the same keys and context is dropped.
pub fn binding_rows(cx: &App) -> Vec<BindingRow> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let bindings: Vec<&KeyBinding> = keymap.bindings().collect();
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for binding in bindings.iter().rev() {
        let action = binding.action().name();
        if action == "zed::NoAction" || action.ends_with("::NoAction") {
            continue;
        }
        let keys = format_binding(binding);
        let context: SharedString = binding
            .predicate()
            .map(|p| p.to_string().into())
            .unwrap_or_default();
        if !seen.insert((keys.clone(), context.clone())) {
            continue;
        }
        let label: SharedString = humanize_action_name(action).into();
        let strokes = binding
            .keystrokes()
            .iter()
            .map(|k| (k.key().to_string(), *k.modifiers()))
            .collect();
        let haystack = format!("{label} {action} {keys} {context}").to_lowercase();
        rows.push(BindingRow {
            label,
            action: action.into(),
            keys,
            context,
            strokes,
            haystack,
            command: !is_palette_hidden(action),
        });
    }
    // Every command is listed, also the ones without a key: they show as "Unbound" and can be
    // found by typing "unbound".
    let bound: std::collections::HashSet<SharedString> =
        rows.iter().map(|r: &BindingRow| r.action.clone()).collect();
    for name in cx.all_action_names() {
        if is_palette_hidden(name) || name.ends_with("::NoAction") || bound.contains(*name) {
            continue;
        }
        let label: SharedString = humanize_action_name(name).into();
        rows.push(BindingRow {
            haystack: format!("{label} {name} unbound").to_lowercase(),
            label,
            action: (*name).into(),
            keys: SharedString::default(),
            context: SharedString::default(),
            strokes: Vec::new(),
            command: true,
        });
    }
    // User commands first, then UI plumbing (pickers, text inputs), by label.
    rows.sort_by(|a, b| {
        b.command
            .cmp(&a.command)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.keys.cmp(&b.keys))
    });
    rows
}

/// Whether `row` matches every whitespace-separated term of `query`.
pub fn row_matches(row: &BindingRow, query: &str) -> bool {
    query
        .to_lowercase()
        .split_whitespace()
        .all(|term| row.haystack.contains(term))
}

/// Whether `row`'s keystrokes start with `recorded`.
pub fn row_starts_with(row: &BindingRow, recorded: &[Keystroke]) -> bool {
    !recorded.is_empty()
        && row.strokes.len() >= recorded.len()
        && row
            .strokes
            .iter()
            .zip(recorded)
            .all(|((key, mods), ks)| key.eq_ignore_ascii_case(&ks.key) && *mods == ks.modifiers)
}

/// The viewer.
pub struct KeybindingsView {
    rows: Vec<BindingRow>,
    matches: Rc<RefCell<Vec<usize>>>,
    query: Entity<TextInput>,
    recording: bool,
    recorded: Vec<Keystroke>,
    list: VirtualListState,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for KeybindingsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.recording {
            self.focus_handle.clone()
        } else {
            self.query.focus_handle(cx)
        }
    }
}

impl KeybindingsView {
    /// A viewer over the current key map.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| TextInput::new("Search commands, keys or contexts…", cx));
        let mut subscriptions =
            vec![
                cx.subscribe_in(
                    &query,
                    window,
                    |this, _, event: &TextInputEvent, _, cx| match event {
                        TextInputEvent::Changed(_) => this.update_matches(cx),
                    },
                ),
            ];
        // Record mode: swallow keystrokes before any binding runs.
        let weak = cx.entity().downgrade();
        subscriptions.push(cx.intercept_keystrokes(move |event, window, cx| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            let (recording, focused) = {
                let v = view.read(cx);
                (v.recording, v.focus_handle.is_focused(window))
            };
            if !recording || !focused {
                return;
            }
            let ks = event.keystroke.clone();
            view.update(cx, |v, cx| v.record(ks, cx));
            cx.stop_propagation();
        }));
        let mut view = KeybindingsView {
            rows: binding_rows(cx),
            matches: Rc::new(RefCell::new(Vec::new())),
            query,
            recording: false,
            recorded: Vec::new(),
            list: VirtualListState::new(),
            focus_handle: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        };
        view.update_matches(cx);
        view
    }

    /// Re-reads the key map (after the user edited keymap.toml).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.rows = binding_rows(cx);
        self.update_matches(cx);
    }

    /// Number of rows shown (tests).
    pub fn match_count(&self) -> usize {
        self.matches.borrow().len()
    }

    /// Sets the text filter (tests, palette).
    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.query
            .update(cx, |q, cx| q.set_text(query.to_string(), cx));
        self.update_matches(cx);
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text().to_string();
        let matches: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                if self.recording || !self.recorded.is_empty() {
                    row_starts_with(row, &self.recorded)
                } else {
                    row_matches(row, &query)
                }
            })
            .map(|(ix, _)| ix)
            .collect();
        *self.matches.borrow_mut() = matches;
        self.list.reveal(0);
        cx.notify();
    }

    fn record(&mut self, ks: Keystroke, cx: &mut Context<Self>) {
        if ks.key == "escape" && !ks.modifiers.modified() {
            self.recording = false;
            cx.notify();
            return;
        }
        if self.recorded.len() >= 2 {
            self.recorded.clear();
        }
        self.recorded.push(ks);
        self.update_matches(cx);
    }

    fn toggle_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recording = !self.recording;
        self.recorded.clear();
        if self.recording {
            window.focus(&self.focus_handle);
        } else {
            let focus = self.query.focus_handle(cx);
            window.focus(&focus);
        }
        self.update_matches(cx);
    }

    fn recorded_text(&self) -> String {
        self.recorded
            .iter()
            .map(|k| format_keystroke(&k.modifiers, &k.key))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn render_row(row: &BindingRow, ix: usize, cx: &App) -> AnyElement {
        let ui = &cx.theme().ui;
        let keys: Vec<AnyElement> = if row.keys.is_empty() {
            vec![div()
                .text_size(metrics::text_sm())
                .text_color(ui.text_faint.hsla())
                .child("Unbound")
                .into_any_element()]
        } else {
            row.keys
                .split(' ')
                .map(|k| keycap(SharedString::from(k.to_string()), cx).into_any_element())
                .collect()
        };
        h_flex()
            .id(("binding-row", ix))
            .w_full()
            .h(px(34.))
            .px(px(16.))
            .gap(px(12.))
            .border_b_1()
            .border_color(ui.border_variant.hsla().opacity(0.6))
            .hover(|s| s.bg(ui.hover.hsla()))
            .child(
                v_flex().flex_1().min_w_0().child(
                    div()
                        .text_size(metrics::text())
                        .text_color(if row.command {
                            ui.text.hsla()
                        } else {
                            ui.text_muted.hsla()
                        })
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(row.label.clone()),
                ),
            )
            .child(h_flex().w(px(220.)).flex_none().gap(px(4.)).children(keys))
            .child(
                div()
                    .w(px(200.))
                    .flex_none()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_faint.hsla())
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(if row.context.is_empty() {
                        SharedString::from("Global")
                    } else {
                        row.context.clone()
                    }),
            )
            .child(
                div()
                    .w(px(240.))
                    .flex_none()
                    .text_size(metrics::text_xs())
                    .font_family(crate::assets::MONO_FONT)
                    .text_color(ui.text_faint.hsla())
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(row.action.clone()),
            )
            .into_any_element()
    }
}

impl Render for KeybindingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let count = self.matches.borrow().len();
        let rows = Rc::new(self.rows.clone());
        let matches = self.matches.clone();
        let recording = self.recording;
        let recorded = self.recorded_text();
        let header_cell = |text: &'static str, w: Option<f32>| {
            div()
                .when_some(w, |el, w| el.w(px(w)).flex_none())
                .when(w.is_none(), |el| el.flex_1().min_w_0())
                .text_size(metrics::text_xs())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui.text_faint.hsla())
                .child(text.to_uppercase())
        };
        let search_box = h_flex()
            .flex_1()
            .h(px(32.))
            .px(px(10.))
            .gap(px(8.))
            .rounded(metrics::radius())
            .border_1()
            .border_color(if recording {
                ui.accent.hsla()
            } else {
                ui.border.hsla()
            })
            .bg(ui.input_bg.hsla())
            .child(
                Icon::new(if recording {
                    IconName::Keyboard
                } else {
                    IconName::Search
                })
                .size(IconSize::Small)
                .color(ui.text_faint.hsla()),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(metrics::text())
                    .line_height(px(20.))
                    .map(|el| {
                        if recording {
                            el.text_color(if recorded.is_empty() {
                                ui.text_faint.hsla()
                            } else {
                                ui.text.hsla()
                            })
                            .child(if recorded.is_empty() {
                                "Press a shortcut… (Esc to stop)".to_string()
                            } else {
                                recorded.clone()
                            })
                        } else {
                            el.child(self.query.clone())
                        }
                    }),
            );
        v_flex()
            .id("keybindings")
            .key_context("KeybindingsView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(ui.background.hsla())
            .child(
                v_flex()
                    .flex_none()
                    .px(px(16.))
                    .pt(px(16.))
                    .pb(px(12.))
                    .gap(px(12.))
                    .border_b_1()
                    .border_color(ui.border.hsla())
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .child(
                                Icon::new(IconName::Keyboard)
                                    .size(IconSize::Large)
                                    .color(ui.text_accent.hsla()),
                            )
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Keyboard Shortcuts"),
                            )
                            .child(
                                div()
                                    .text_size(metrics::text_sm())
                                    .text_color(ui.text_faint.hsla())
                                    .child(format!("{count} of {} bindings", self.rows.len())),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("keybindings-edit", "Edit keymap.toml")
                                    .icon(IconName::FileCode)
                                    .style(ButtonStyle::Outlined)
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(OpenKeymap), cx)
                                    }),
                            ),
                    )
                    .child(
                        h_flex().gap(px(8.)).child(search_box).child(
                            Button::new(
                                "keybindings-record",
                                if recording {
                                    "Stop Recording"
                                } else {
                                    "Record Keys"
                                },
                            )
                            .icon(IconName::Keyboard)
                            .style(if recording {
                                ButtonStyle::Accent
                            } else {
                                ButtonStyle::Outlined
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.toggle_recording(window, cx)
                                }),
                            ),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .h(px(28.))
                    .px(px(16.))
                    .gap(px(12.))
                    .bg(ui.surface.hsla())
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .child(header_cell("Command", None))
                    .child(header_cell("Keys", Some(220.)))
                    .child(header_cell("Context", Some(200.)))
                    .child(header_cell("Action", Some(240.))),
            )
            .child(div().flex_1().min_h_0().map(|el| {
                if count == 0 {
                    el.child(
                        v_flex()
                            .pt(px(48.))
                            .items_center()
                            .gap(px(6.))
                            .text_color(ui.text_faint.hsla())
                            .child(
                                Icon::new(IconName::Keyboard)
                                    .size(IconSize::Custom(px(28.)))
                                    .color(ui.text_faint.hsla()),
                            )
                            .child(
                                div()
                                    .text_size(metrics::text())
                                    .child("No matching key bindings"),
                            ),
                    )
                } else {
                    el.child(virtual_list(
                        "keybindings-list",
                        count,
                        &self.list,
                        cx,
                        move |range, _, cx| {
                            let matches = matches.borrow();
                            range
                                .filter_map(|i| {
                                    let row = rows.get(*matches.get(i)?)?;
                                    Some(KeybindingsView::render_row(row, i, cx))
                                })
                                .collect()
                        },
                    ))
                }
            }))
    }
}

/// The viewer as a tab.
pub struct KeybindingsItem {
    view: Entity<KeybindingsView>,
}

impl KeybindingsItem {
    /// Wraps `view`.
    pub fn new(view: Entity<KeybindingsView>) -> Self {
        KeybindingsItem { view }
    }
}

impl PaneItem for KeybindingsItem {
    fn title(&self, _: &App) -> SharedString {
        "Keyboard Shortcuts".into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::Keyboard)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn tab_kind(&self, _: &App) -> Option<lector_config::TabKind> {
        None
    }

    fn clone_item(&self, _window: &mut Window, _cx: &mut App) -> Option<Box<dyn PaneItem>> {
        Some(Box::new(KeybindingsItem {
            view: self.view.clone(),
        }))
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(KEYBINDINGS_ITEM_KEY.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        label: &str,
        action: &str,
        keys: &str,
        strokes: Vec<(&str, gpui::Modifiers)>,
    ) -> BindingRow {
        BindingRow {
            label: label.to_string().into(),
            action: action.to_string().into(),
            keys: keys.to_string().into(),
            context: "Workspace".into(),
            strokes: strokes
                .into_iter()
                .map(|(k, m)| (k.to_string(), m))
                .collect(),
            haystack: format!("{label} {action} {keys} Workspace").to_lowercase(),
            command: true,
        }
    }

    #[test]
    fn filters_by_terms_and_keystrokes() {
        let ctrl = gpui::Modifiers::control();
        let r = row(
            "Workspace: Toggle Sidebar",
            "workspace::ToggleSidebar",
            "Ctrl+B",
            vec![("b", ctrl)],
        );
        assert!(row_matches(&r, "toggle side"));
        assert!(row_matches(&r, "ctrl+b"));
        assert!(!row_matches(&r, "palette"));
        let ks = Keystroke::parse("ctrl-b").unwrap();
        assert!(row_starts_with(&r, &[ks]));
        let other = Keystroke::parse("ctrl-p").unwrap();
        assert!(!row_starts_with(&r, &[other]));
        assert!(!row_starts_with(&r, &[]));
    }
}
