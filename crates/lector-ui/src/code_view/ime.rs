//! Text input for an editable [`CodeView`]: the IME helpers and the
//! `EntityInputHandler` implementation (composition, marked text, replacement).

use std::ops::Range;

use gpui::{
    prelude::*, px, App, Bounds, EntityInputHandler, Pixels, Point, UTF16Selection, Window,
};
use lector_text::Rope;

use super::{edit, CodeView};

impl EntityInputHandler for CodeView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.buffer_text(cx)?;
        let range = edit::range_from_utf16(&text, &range_utf16);
        adjusted_range.replace(edit::range_to_utf16(&text, &range));
        Some(text.text_for_range(range))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let text = self.buffer_text(cx)?;
        let sel = self.selection();
        let (start, end) = sel.range();
        let range = edit::pos_to_offset(&text, start)..edit::pos_to_offset(&text, end);
        Some(UTF16Selection {
            range: edit::range_to_utf16(&text, &range),
            reversed: sel.head < sel.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let marked = self.marked.clone()?;
        let text = self.buffer_text(cx)?;
        Some(edit::range_to_utf16(&text, &marked))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        self.end_ime_transaction(cx);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_editable() {
            return;
        }
        let Some(rope) = self.buffer_text(cx) else {
            return;
        };
        let explicit = range_utf16.map(|r| edit::range_from_utf16(&rope, &r));
        match (explicit, self.marked.clone()) {
            (Some(range), _) | (None, Some(range)) => {
                // Committing a composition, or the platform replacing a range
                // (accent menu, character palette): one exact replacement.
                self.replace_range_raw(range, text, None, cx);
            }
            (None, None) => {
                // Plain typing: every caret, with auto-close/type-over.
                // An owner (completion: commit characters) may take the text over.
                if let Some(hook) = self.popups.interceptor() {
                    if hook(text, window, cx) {
                        return;
                    }
                }
                self.popups.set_typed(text);
                let text = text.to_string();
                self.edit_buffer(cx, move |b| b.insert(&text));
            }
        }
        self.marked = None;
        self.end_ime_transaction(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_editable() {
            return;
        }
        if self.marked.is_none() {
            // A composition starts: what an owner holds back goes in first.
            self.run_command_hook(cx);
        }
        let Some(rope) = self.buffer_text(cx) else {
            return;
        };
        let range = range_utf16
            .map(|r| edit::range_from_utf16(&rope, &r))
            .or(self.marked.clone())
            .unwrap_or_else(|| {
                let (s, e) = self.selection().range();
                edit::pos_to_offset(&rope, s)..edit::pos_to_offset(&rope, e)
            });
        if !self.ime_transaction {
            if let Some(buffer) = &self.buffer {
                buffer.update(cx, |b, cx| b.update_buffer(|b| b.begin_transaction(), cx));
                self.ime_transaction = true;
            }
        }
        let start = range.start;
        let caret = new_selected_range_utf16.map(|sel| {
            // Relative to the new text, in UTF-16.
            let to_bytes = |u: usize| {
                let mut units = 0;
                let mut bytes = 0;
                for c in new_text.chars() {
                    if units >= u {
                        break;
                    }
                    units += c.len_utf16();
                    bytes += c.len_utf8();
                }
                bytes
            };
            start + to_bytes(sel.start)..start + to_bytes(sel.end)
        });
        let inserted = self.replace_range_raw(range, new_text, caret, cx);
        self.marked = inserted.filter(|r| !r.is_empty());
        if self.marked.is_none() {
            self.end_ime_transaction(cx);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let text = self.buffer_text(cx)?;
        let range = edit::range_from_utf16(&text, &range_utf16);
        let start = edit::offset_to_pos(&text, range.start);
        let origin = self.point_for_pos(start)?;
        let line_height = self.line_height()?;
        let end = edit::offset_to_pos(&text, range.end);
        let width = match self.point_for_pos(end) {
            Some(p) if end.row == start.row && p.x > origin.x => p.x - origin.x,
            _ => px(1.),
        };
        Some(Bounds::new(origin, gpui::size(width, line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let pos = self.pos_for_point(point)?;
        let text = self.buffer_text(cx)?;
        Some(edit::offset_to_utf16(
            &text,
            edit::pos_to_offset(&text, pos),
        ))
    }
}

impl CodeView {
    // IME helpers.

    fn buffer_text(&self, cx: &App) -> Option<Rope> {
        self.buffer.as_ref().map(|b| b.read(cx).text().clone())
    }

    /// Replaces `range` (bytes) with `text` as an external edit (no
    /// auto-close), leaving the caret at `caret` (bytes, absolute) or after
    /// the inserted text.
    fn replace_range_raw(
        &mut self,
        range: Range<usize>,
        text: &str,
        caret: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let text_owned = text.to_string();
        let inserted = self.edit(cx, move |b| {
            let tx = b.apply_edits(vec![lector_buffer::TextEdit {
                range: range.clone(),
                new_text: text_owned.clone(),
            }]);
            let end = range.start + text_owned.len();
            let caret = caret.unwrap_or(end..end);
            let id = b.selections().iter().map(|s| s.id).max().unwrap_or(0) + 1;
            b.set_selections(vec![lector_buffer::Selection {
                id,
                anchor: caret.start,
                head: caret.end,
                goal: None,
            }]);
            (range.start..end, tx)
        })?;
        Some(inserted)
    }

    fn end_ime_transaction(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.ime_transaction) {
            if let Some(buffer) = &self.buffer {
                buffer.update(cx, |b, cx| b.update_buffer(|b| b.end_transaction(), cx));
            }
        }
    }
}
