//! Go-to-line modal: `LINE[:COL]`, previewing the target as you type.

use crate::scale::px;
use gpui::{
    div, prelude::*, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Window,
};

use crate::actions::picker::{Cancel, Confirm};
use crate::code_view::{CodeView, Pos};
use crate::components::popover::surface;
use crate::components::text_input::{TextInput, TextInputEvent};
use crate::components::{h_flex, metrics, v_flex};
use crate::icons::{Icon, IconName, IconSize};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt};

/// Parses `"12"`, `"12:4"` or `":12"` into 1-based `(line, column)`.
pub fn parse_line_col(text: &str) -> Option<(u32, Option<u32>)> {
    let text = text.trim().trim_start_matches(':');
    let mut parts = text.splitn(2, [':', ',']);
    let line = parts.next()?.trim().parse::<u32>().ok()?;
    let col = parts.next().and_then(|c| c.trim().parse::<u32>().ok());
    Some((line, col))
}

/// The modal.
pub struct GoToLine {
    input: Entity<TextInput>,
    target: Entity<CodeView>,
    original: (Pos, gpui::Point<gpui::Pixels>),
    line_count: u32,
    current_line: u32,
    confirmed: bool,
    _subscription: Subscription,
}

impl EventEmitter<DismissEvent> for GoToLine {}

impl Focusable for GoToLine {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl GoToLine {
    /// Modal targeting `view`.
    pub fn new(target: Entity<CodeView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = target.read(cx);
        let line_count = view.source().line_count();
        let cursor = view.cursor();
        let original = (cursor, view.scroll_offset());
        let input = cx.new(|cx| {
            TextInput::new(
                format!("Line number (1–{line_count}), optionally :column"),
                cx,
            )
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            let TextInputEvent::Changed(text) = event;
            if let Some((line, _)) = parse_line_col(text) {
                let row = line
                    .saturating_sub(1)
                    .min(this.line_count.saturating_sub(1));
                this.target
                    .update(cx, |v, cx| v.scroll_to_row(row, true, cx));
            }
            cx.notify();
        });
        GoToLine {
            input,
            target,
            original,
            line_count,
            current_line: cursor.row + 1,
            confirmed: false,
            _subscription: subscription,
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().clone();
        if let Some((line, col)) = parse_line_col(&text) {
            let row = line
                .saturating_sub(1)
                .min(self.line_count.saturating_sub(1));
            let target = self.target.clone();
            self.confirmed = true;
            target.update(cx, |v, cx| {
                let line_text = v.line_text(row);
                let col = col
                    .map(|c| {
                        crate::code_view::display_map::byte_for_display_column(
                            &line_text,
                            c.saturating_sub(1) as usize,
                            4,
                        )
                    })
                    .unwrap_or(0);
                v.set_cursor(Pos::new(row, col), cx);
                v.scroll_to_row(row, true, cx);
            });
            window.focus(&target.focus_handle(cx));
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        let (pos, scroll) = self.original;
        self.target.update(cx, |v, cx| {
            v.set_scroll_offset(scroll, cx);
            let _ = pos;
        });
        cx.emit(DismissEvent);
    }
}

impl Render for GoToLine {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let text = self.input.read(cx).text().clone();
        let hint = match parse_line_col(&text) {
            Some((line, Some(col))) => format!("Go to line {line}, column {col}"),
            Some((line, None)) => format!("Go to line {line}"),
            None => format!("Current line {} of {}", self.current_line, self.line_count),
        };
        surface(cx)
            .key_context(contexts::PICKER)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(px(440.))
            .child(
                v_flex()
                    .child(
                        h_flex()
                            .h(px(42.))
                            .px(px(12.))
                            .gap(px(8.))
                            .border_b_1()
                            .border_color(ui.border_variant.hsla())
                            .child(
                                Icon::new(IconName::Hash)
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
                            ),
                    )
                    .child(
                        div()
                            .px(px(14.))
                            .py(px(9.))
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_muted.hsla())
                            .child(hint),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::parse_line_col;

    #[test]
    fn parses() {
        assert_eq!(parse_line_col("12"), Some((12, None)));
        assert_eq!(parse_line_col(" 12:4 "), Some((12, Some(4))));
        assert_eq!(parse_line_col(":7"), Some((7, None)));
        assert_eq!(parse_line_col("x"), None);
    }
}
