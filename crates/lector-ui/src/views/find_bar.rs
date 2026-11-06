//! In-file find: a small modal that highlights matches in the active code
//! view. Enter jumps to the next match; matches stay highlighted after the
//! modal closes so `editor::FindNext` / `FindPrev` keep working.
//! Matching uses `lector_search::find_in_text` on a background thread over
//! the document snapshot (case / whole word / regex toggles).
//!
//! A second row replaces text (`editor::FindReplace`, `editor::Find` again or
//! the chevron). *Replace* swaps the match at or after the caret, *Replace
//! All* swaps every match as one undo step, both through
//! `DocumentModel::apply_edits`; with the regex toggle on, `$1` / `${name}`
//! in the replacement expand capture groups. Read-only tabs only search.

use std::ops::Range;

use crate::scale::px;
use gpui::{
    div, prelude::*, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Task, Window,
};
use lector_buffer::TextEdit;
use lector_search::{replacements_in_text, CaseMode, SearchQuery};

use crate::actions::editor::{ReplaceAll, ReplaceNext};
use crate::actions::picker::{Cancel, Confirm, SelectNext, SelectPrev};
use crate::code_view::edit::{offset_to_pos, pos_to_offset};
use crate::code_view::{CodeView, Pos, Selection};
use crate::components::popover::surface;
use crate::components::text_input::{TextInput, TextInputEvent};
use crate::components::{h_flex, metrics, Button, ButtonStyle, IconButton, Tooltip};
use crate::editor::DocumentModel;
use crate::icons::{Icon, IconName, IconSize};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt};

/// Case-insensitive (smart case: sensitive if the query has uppercase)
/// substring matches of `query` in every row of `view`'s source.
pub fn find_matches(view: &CodeView, query: &str, limit: usize) -> Vec<(u32, Range<usize>)> {
    if query.is_empty() {
        return Vec::new();
    }
    let sensitive = query.chars().any(char::is_uppercase);
    let needle = if sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let source = view.source();
    let mut out = Vec::new();
    for row in 0..source.line_count() {
        let line = source.line(row);
        let hay = if sensitive {
            line.to_string()
        } else {
            line.to_lowercase()
        };
        // Lowercasing can change byte lengths for some scripts; only use
        // matches whose offsets are valid in the original line.
        let mut start = 0;
        while let Some(ix) = hay[start..].find(&needle) {
            let s = start + ix;
            let e = s + needle.len();
            if line.is_char_boundary(s) && e <= line.len() && line.is_char_boundary(e) {
                out.push((row, s..e));
                if out.len() >= limit {
                    return out;
                }
            }
            start = e.max(s + 1);
            while start < hay.len() && !hay.is_char_boundary(start) {
                start += 1;
            }
            if start >= hay.len() {
                break;
            }
        }
    }
    out
}

/// What to do with the matches and their replacements once computed.
type FinishReplace =
    fn(&mut FindBar, Vec<(Range<usize>, String)>, &mut Window, &mut Context<FindBar>);

/// Maximum highlighted matches.
const MAX_MATCHES: usize = 100_000;

/// The find modal.
pub struct FindBar {
    input: Entity<TextInput>,
    replace_input: Entity<TextInput>,
    target: Entity<CodeView>,
    /// The document behind `target`; `None` for views that cannot be edited.
    model: Option<Entity<DocumentModel>>,
    replace_open: bool,
    /// Select the active match once the next search finishes (after Replace).
    select_after_search: bool,
    /// Outcome of the last replace ("3 replaced"), shown instead of the count.
    note: Option<SharedString>,
    replace_task: Option<Task<()>>,
    count: usize,
    active: Option<usize>,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    error: Option<SharedString>,
    search_task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for FindBar {}

impl Focusable for FindBar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl FindBar {
    /// Find in `target`, prefilled with its selection (if single-line).
    /// `model` is the document behind it (enables replace); `replace_open`
    /// shows the replace row at once.
    pub fn new(
        target: Entity<CodeView>,
        model: Option<Entity<DocumentModel>>,
        replace_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("Find in file", cx));
        let replace_input = cx.new(|cx| TextInput::new("Replace", cx));
        let selected = target.read(cx).selected_text();
        let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            let TextInputEvent::Changed(query) = event;
            let query = query.to_string();
            this.note = None;
            this.update_matches(&query, cx);
        });
        let mut bar = FindBar {
            input,
            replace_input,
            target,
            model,
            replace_open: false,
            select_after_search: false,
            note: None,
            replace_task: None,
            count: 0,
            active: None,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            error: None,
            search_task: None,
            _subscription: subscription,
        };
        if !selected.is_empty() && !selected.contains('\n') {
            bar.input.update(cx, |i, cx| {
                i.set_text(selected.clone(), cx);
                i.select_all_text(cx);
            });
            bar.update_matches(&selected, cx);
        }
        bar.replace_open = replace_open && bar.can_replace(cx);
        bar
    }

    /// The query as typed.
    pub fn query_text(&self, cx: &App) -> String {
        self.input.read(cx).text().to_string()
    }

    /// Whether the replace row is shown.
    pub fn replace_open(&self) -> bool {
        self.replace_open
    }

    /// Replacing needs an editable document: read-only tabs only search.
    pub fn can_replace(&self, cx: &App) -> bool {
        self.model
            .as_ref()
            .is_some_and(|m| m.read(cx).is_editable())
            && self.target.read(cx).is_editable()
    }

    /// Shows the replace row and puts the caret in its field.
    pub fn open_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_replace(cx) {
            return;
        }
        self.replace_open = true;
        let focus = self.replace_input.focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    /// Shows or hides the replace row (`editor::Find` again, the chevron).
    pub fn toggle_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.replace_open {
            self.replace_open = false;
            let focus = self.input.focus_handle(cx);
            window.focus(&focus);
            cx.notify();
        } else {
            self.open_replace(window, cx);
        }
    }

    fn search(&self, cx: &App) -> SearchQuery {
        SearchQuery {
            pattern: self.input.read(cx).text().to_string(),
            regex: self.regex,
            case: if self.case_sensitive {
                CaseMode::Sensitive
            } else {
                CaseMode::Smart
            },
            whole_word: self.whole_word,
            ..Default::default()
        }
    }

    /// Computes every match of the query with its replacement over the
    /// buffer text, then hands them to `finish` (on the main thread) if the
    /// text did not change meanwhile; if it did, starts over.
    fn with_replacements(
        &mut self,
        finish: FinishReplace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_replace(cx) {
            return;
        }
        let search = self.search(cx);
        if search.pattern.is_empty() {
            return;
        }
        let replacement = self.replace_input.read(cx).text().to_string();
        let Some(buffer) = self
            .model
            .as_ref()
            .and_then(|m| m.read(cx).buffer().cloned())
        else {
            return;
        };
        let (text, version) = {
            let b = buffer.read(cx);
            (b.text().clone(), b.buffer().version())
        };
        self.replace_task = Some(cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(
                    async move { replacements_in_text(&text.to_string(), &search, &replacement) },
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.error = None;
                match found {
                    Err(err) => {
                        this.error = Some(err.to_string().into());
                        cx.notify();
                    }
                    Ok(found) => {
                        let unchanged = buffer.read(cx).buffer().version() == version;
                        if unchanged {
                            finish(this, found, window, cx);
                        } else {
                            this.with_replacements(finish, window, cx);
                        }
                    }
                }
            });
        }));
    }

    /// Replaces the selected match, or the first match after the caret, and
    /// moves on to the next one.
    pub fn replace_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_replacements(Self::finish_replace_next, window, cx);
    }

    fn finish_replace_next(
        &mut self,
        found: Vec<(Range<usize>, String)>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = self.model.clone() else {
            return;
        };
        let Some(buffer) = model.read(cx).buffer().cloned() else {
            return;
        };
        let selection = self.target.read(cx).selection();
        let (start, end) = {
            let rope = buffer.read(cx).text();
            let a = pos_to_offset(rope, selection.anchor);
            let b = pos_to_offset(rope, selection.head);
            (a.min(b), a.max(b))
        };
        let chosen = found
            .iter()
            .position(|(range, _)| range.start == start && range.end == end)
            .or_else(|| found.iter().position(|(range, _)| range.start >= start))
            .or(if found.is_empty() { None } else { Some(0) });
        let Some((range, new_text)) = chosen.and_then(|ix| found.into_iter().nth(ix)) else {
            self.note = Some("No results".into());
            cx.notify();
            return;
        };
        let caret = range.start + new_text.len();
        model.update(cx, |m, cx| {
            m.apply_edits(vec![TextEdit { range, new_text }], cx)
        });
        let pos = offset_to_pos(buffer.read(cx).text(), caret);
        self.target.update(cx, |v, cx| v.set_cursor(pos, cx));
        self.note = None;
        self.select_after_search = true;
        let query = self.input.read(cx).text().to_string();
        self.update_matches(&query, cx);
    }

    /// Replaces every match, as one undo step.
    pub fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_replacements(Self::finish_replace_all, window, cx);
    }

    fn finish_replace_all(
        &mut self,
        found: Vec<(Range<usize>, String)>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = self.model.clone() else {
            return;
        };
        let count = found.len();
        let edits = found
            .into_iter()
            .map(|(range, new_text)| TextEdit { range, new_text })
            .collect();
        if count > 0 {
            model.update(cx, |m, cx| m.apply_edits(edits, cx));
        }
        self.note = Some(match count {
            0 => "No results".into(),
            1 => "1 replaced".into(),
            n => format!("{n} replaced").into(),
        });
        let query = self.input.read(cx).text().to_string();
        self.update_matches(&query, cx);
    }

    fn replace_next_action(
        &mut self,
        _: &ReplaceNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_next(window, cx);
    }

    fn replace_all_action(&mut self, _: &ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_all(window, cx);
    }

    fn update_matches(&mut self, query: &str, cx: &mut Context<Self>) {
        self.error = None;
        if query.is_empty() {
            self.search_task = None;
            self.apply_matches(Vec::new(), cx);
            return;
        }
        let search = SearchQuery {
            pattern: query.to_string(),
            ..self.search(cx)
        };
        // The buffer is always current; a view's source may lag an edit.
        let live = self
            .model
            .as_ref()
            .and_then(|m| m.read(cx).buffer().cloned())
            .map(|b| lector_text::Text::from(b.read(cx).text().clone()));
        let text = live.or_else(|| self.target.read(cx).source().text_snapshot());
        let Some(text) = text else {
            // Sources without a snapshot (small in-memory text): match here.
            let matches = find_matches(self.target.read(cx), query, MAX_MATCHES);
            self.apply_matches(matches, cx);
            return;
        };
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let string = text.to_string();
                    lector_search::find_in_text(&string, &search).map(|ranges| {
                        ranges
                            .into_iter()
                            .take(MAX_MATCHES)
                            .map(|r| {
                                let start = text.offset_to_point(r.start);
                                let end = text.offset_to_point(r.end);
                                let end_col = if end.row == start.row {
                                    end.column as usize
                                } else {
                                    text.line_len(start.row) as usize
                                };
                                (start.row, start.column as usize..end_col)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(matches) => this.apply_matches(matches, cx),
                Err(err) => {
                    this.error = Some(err.to_string().into());
                    this.apply_matches(Vec::new(), cx);
                }
            });
        }));
    }

    fn apply_matches(&mut self, matches: Vec<(u32, Range<usize>)>, cx: &mut Context<Self>) {
        let cursor = self.target.read(cx).cursor();
        // Start at the first match at or after the cursor.
        let active = (!matches.is_empty()).then(|| {
            matches
                .iter()
                .position(|(row, r)| Pos::new(*row, r.start) >= cursor)
                .unwrap_or(0)
        });
        self.count = matches.len();
        self.active = active;
        self.target
            .update(cx, |v, cx| v.set_search_matches(matches, active, cx));
        if std::mem::take(&mut self.select_after_search) && active.is_some() {
            self.target.update(cx, |v, cx| {
                v.step_search_match(0, true, cx);
            });
        }
        cx.notify();
    }

    /// Flips the case (0), whole word (1) or regex (any other) option.
    pub fn toggle(&mut self, which: u8, cx: &mut Context<Self>) {
        self.note = None;
        match which {
            0 => self.case_sensitive = !self.case_sensitive,
            1 => self.whole_word = !self.whole_word,
            _ => self.regex = !self.regex,
        }
        let query = self.input.read(cx).text().to_string();
        self.update_matches(&query, cx);
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let target = self.target.clone();
        self.active = target.update(cx, |v, cx| v.step_search_match(delta, false, cx));
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.replace_open
            && self
                .replace_input
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            self.replace_next(window, cx);
        } else {
            self.step(1, cx);
        }
    }

    fn next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Leave the cursor on the active match.
        let target = self.target.clone();
        target.update(cx, |v, cx| {
            v.step_search_match(0, true, cx);
        });
        window.focus(&target.focus_handle(cx));
        cx.emit(DismissEvent);
    }
}

impl Render for FindBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let status = match (self.count, self.active) {
            _ if self.error.is_some() => "Invalid pattern".to_string(),
            _ if self.note.is_some() => self.note.clone().unwrap_or_default().to_string(),
            (0, _) if self.input.read(cx).text().is_empty() => String::new(),
            (0, _) => "No results".to_string(),
            (n, Some(a)) => format!("{} of {n}", a + 1),
            (n, None) => format!("{n} results"),
        };
        surface(cx)
            .key_context(contexts::PICKER)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::prev))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::replace_next_action))
            .on_action(cx.listener(Self::replace_all_action))
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                // Tab / Shift-Tab move between the find and replace fields.
                let k = &e.keystroke;
                if k.key != "tab" || k.modifiers.control || k.modifiers.alt || k.modifiers.platform
                {
                    return;
                }
                if this.focus_other_field(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .w(px(560.))
            .child(
                h_flex()
                    .h(px(40.))
                    .px(px(12.))
                    .gap(px(8.))
                    .child(self.render_chevron(cx))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .text_color(ui.text.hsla())
                            .child(self.input.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(metrics::text_sm())
                            .text_color(if self.count == 0 && !status.is_empty() {
                                ui.error.hsla()
                            } else {
                                ui.text_muted.hsla()
                            })
                            .child(status),
                    )
                    .child(self.render_toggles(cx)),
            )
            .when(self.replace_open && self.can_replace(cx), |el| {
                el.child(self.render_replace_row(cx))
            })
    }
}

impl FindBar {
    /// Moves the caret between the find and replace fields (when the replace
    /// row is shown). Returns whether focus moved.
    fn focus_other_field(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !(self.replace_open && self.can_replace(cx)) {
            return false;
        }
        let find = self.input.focus_handle(cx);
        let target = if find.contains_focused(window, cx) {
            self.replace_input.focus_handle(cx)
        } else {
            find
        };
        window.focus(&target);
        cx.notify();
        true
    }

    /// Chevron (or search icon when replacing is unavailable) at the row start.
    fn render_chevron(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        if !self.can_replace(cx) {
            return div()
                .flex_none()
                .child(
                    Icon::new(IconName::Search)
                        .size(IconSize::Small)
                        .color(ui.text_faint.hsla()),
                )
                .into_any_element();
        }
        let weak = cx.entity().downgrade();
        IconButton::new(
            "find-replace-toggle",
            if self.replace_open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            },
        )
        .icon_size(IconSize::Small)
        .tooltip(Tooltip::text("Toggle Replace"))
        .on_click(move |_, window, cx| {
            let _ = weak.update(cx, |bar, cx| bar.toggle_replace(window, cx));
        })
        .into_any_element()
    }

    fn render_replace_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let weak = cx.entity().downgrade();
        let next = weak.clone();
        h_flex()
            .h(px(36.))
            .px(px(12.))
            .gap(px(8.))
            .border_t_1()
            .border_color(ui.border.hsla())
            .child(
                Icon::new(IconName::Replace)
                    .size(IconSize::Small)
                    .color(ui.text_faint.hsla()),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .text_color(ui.text.hsla())
                    .child(self.replace_input.clone()),
            )
            .child(
                Button::new("replace-next", "Replace")
                    .style(ButtonStyle::Subtle)
                    .compact()
                    .tooltip(Tooltip::for_action(
                        "Replace Next",
                        &crate::actions::editor::ReplaceNext,
                    ))
                    .on_click(move |_, window, cx| {
                        let _ = next.update(cx, |bar, cx| bar.replace_next(window, cx));
                    }),
            )
            .child(
                Button::new("replace-all", "All")
                    .style(ButtonStyle::Subtle)
                    .compact()
                    .tooltip(Tooltip::for_action(
                        "Replace All",
                        &crate::actions::editor::ReplaceAll,
                    ))
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |bar, cx| bar.replace_all(window, cx));
                    }),
            )
    }

    fn render_toggles(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let toggle = |id: &'static str, icon: IconName, on: bool, which: u8, tip: &'static str| {
            let weak = weak.clone();
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .selected(on)
                .tooltip(Tooltip::text(tip))
                .on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |bar, cx| bar.toggle(which, cx));
                })
        };
        let step = |id: &'static str, icon: IconName, delta: isize, tip: &'static str| {
            let weak = weak.clone();
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(tip))
                .on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |bar, cx| bar.step(delta, cx));
                })
        };
        h_flex()
            .flex_none()
            .gap(px(1.))
            .child(toggle(
                "find-case",
                IconName::CaseSensitive,
                self.case_sensitive,
                0,
                "Match Case",
            ))
            .child(toggle(
                "find-word",
                IconName::WholeWord,
                self.whole_word,
                1,
                "Match Whole Word",
            ))
            .child(toggle(
                "find-regex",
                IconName::Regex,
                self.regex,
                2,
                "Use Regular Expression",
            ))
            .child(div().w(px(6.)))
            .child(step("find-prev", IconName::ArrowUp, -1, "Previous Match"))
            .child(step("find-next", IconName::ArrowDown, 1, "Next Match"))
    }
}

impl CodeView {
    /// Moves the active search match by `delta` (wrapping), selects it and
    /// scrolls to it. With `select_only`, just selects the current match.
    /// Returns the new active index.
    pub fn step_search_match(
        &mut self,
        delta: isize,
        select_only: bool,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let (matches, active) = self.search_matches();
        if matches.is_empty() {
            return None;
        }
        let n = matches.len() as isize;
        let next = if select_only {
            active.unwrap_or(0)
        } else {
            match active {
                Some(a) => (a as isize + delta).rem_euclid(n) as usize,
                None => 0,
            }
        };
        let (row, range) = matches[next].clone();
        let all = matches.to_vec();
        self.set_search_matches(all, Some(next), cx);
        self.set_selection(
            Selection {
                anchor: Pos::new(row, range.start),
                head: Pos::new(row, range.end),
                goal_column: None,
            },
            cx,
        );
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_view::InMemorySource;
    use std::rc::Rc;

    #[gpui::test]
    fn finds_smart_case(cx: &mut gpui::TestAppContext) {
        let view = cx.new(|cx| CodeView::new(Rc::new(InMemorySource::new("Foo foo\nbar FOO")), cx));
        view.read_with(cx, |v, _| {
            assert_eq!(find_matches(v, "foo", 100).len(), 3);
            assert_eq!(find_matches(v, "Foo", 100), vec![(0, 0..3)]);
            assert!(find_matches(v, "", 100).is_empty());
        });
    }
}
