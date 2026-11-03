//! In-file find: a small modal that highlights matches in the active code
//! view. Enter jumps to the next match; matches stay highlighted after the
//! modal closes so `editor::FindNext` / `FindPrev` keep working.
//! Integration replaces the matcher with `lector_search::find_in_text`.

use std::ops::Range;

use gpui::{
    div, prelude::*, px, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, Window,
};

use crate::actions::picker::{Cancel, Confirm, SelectNext, SelectPrev};
use crate::code_view::{CodeView, Pos, Selection};
use crate::components::popover::surface;
use crate::components::text_input::{TextInput, TextInputEvent};
use crate::components::{h_flex, metrics};
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

/// The find modal.
pub struct FindBar {
    input: Entity<TextInput>,
    target: Entity<CodeView>,
    count: usize,
    active: Option<usize>,
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
    pub fn new(target: Entity<CodeView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Find in file", cx));
        let selected = target.read(cx).selected_text();
        let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            let TextInputEvent::Changed(query) = event;
            let query = query.to_string();
            this.update_matches(&query, cx);
        });
        let mut bar = FindBar {
            input,
            target,
            count: 0,
            active: None,
            _subscription: subscription,
        };
        if !selected.is_empty() && !selected.contains('\n') {
            bar.input.update(cx, |i, cx| {
                i.set_text(selected.clone(), cx);
                i.select_all_text(cx);
            });
            bar.update_matches(&selected, cx);
        }
        bar
    }

    fn update_matches(&mut self, query: &str, cx: &mut Context<Self>) {
        let matches = find_matches(self.target.read(cx), query, 100_000);
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
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let target = self.target.clone();
        self.active = target.update(cx, |v, cx| v.step_search_match(delta, false, cx));
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
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
            .w(px(480.))
            .child(
                h_flex()
                    .h(px(40.))
                    .px(px(12.))
                    .gap(px(8.))
                    .child(
                        Icon::new(IconName::Search)
                            .size(IconSize::Small)
                            .color(ui.text_faint.hsla()),
                    )
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
                            .text_size(metrics::TEXT_SM)
                            .text_color(if self.count == 0 && !status.is_empty() {
                                ui.error.hsla()
                            } else {
                                ui.text_muted.hsla()
                            })
                            .child(status),
                    ),
            )
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
