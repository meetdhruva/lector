//! A multi-line text input (commit messages and other prose): IME via
//! `EntityInputHandler` like [`super::text_input::TextInput`], soft wrap,
//! vertical scrolling, mouse and keyboard selection, clipboard, undo/redo for
//! typing, a placeholder and an optional ruler column.
//!
//! Key context: `TextArea TextInput` (the single-line `text_input::*` bindings
//! apply too: Backspace, Delete, Left, Right, Home, End, clipboard, Select All);
//! `text_area::*` adds the multi-line ones (Newline, MoveUp/Down, Undo/Redo, ...).
//! Wrap it in an element with its own key context (e.g. `CommitEditor`) to bind
//! actions such as "commit" to keys in it.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, App, Bounds, ClipboardItem,
    ContentMask, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, Hsla, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollWheelEvent,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::actions::text_input::{
    Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft,
    SelectRight, ShowCharacterPalette,
};
use crate::theme::{ActiveTheme, ColorExt};

actions!(
    text_area,
    [
        /// Inserts a line break.
        Newline,
        /// Moves the caret one visual line up.
        MoveUp,
        /// Moves the caret one visual line down.
        MoveDown,
        /// Extends the selection one visual line up.
        SelectUp,
        /// Extends the selection one visual line down.
        SelectDown,
        /// Moves the caret to the start of the text.
        MoveToStart,
        /// Moves the caret to the end of the text.
        MoveToEnd,
        /// Extends the selection to the start of the text.
        SelectToStart,
        /// Extends the selection to the end of the text.
        SelectToEnd,
        /// Extends the selection to the start of the line.
        SelectToLineStart,
        /// Extends the selection to the end of the line.
        SelectToLineEnd,
        /// Moves the caret one word left.
        WordLeft,
        /// Moves the caret one word right.
        WordRight,
        /// Extends the selection one word left.
        SelectWordLeft,
        /// Extends the selection one word right.
        SelectWordRight,
        /// Deletes the word before the caret.
        DeleteWordLeft,
        /// Undoes the last edit.
        Undo,
        /// Redoes the last undone edit.
        Redo,
        /// Moves the caret one page up.
        PageUp,
        /// Moves the caret one page down.
        PageDown,
    ]
);

/// Key context identifiers of a [`TextArea`].
pub const KEY_CONTEXT: &str = "TextArea TextInput";

/// Events emitted by [`TextArea`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextAreaEvent {
    /// The text changed.
    Changed,
}

/// Typing within this interval coalesces into one undo step.
const UNDO_COALESCE: Duration = Duration::from_millis(1000);
const MAX_UNDO: usize = 500;

#[derive(Clone)]
struct Snapshot {
    text: String,
    selection: Range<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Typing,
    Other,
}

/// Layout of the last paint, used for hit testing and caret movement.
struct LayoutCache {
    lines: Vec<WrappedLine>,
    /// Byte offset of each logical line.
    starts: Vec<usize>,
    /// Content y (unscrolled) of each logical line.
    tops: Vec<Pixels>,
    line_height: Pixels,
    bounds: Bounds<Pixels>,
}

impl LayoutCache {
    fn line_of(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|s| *s <= offset)
            .saturating_sub(1)
    }

    /// Content position (unscrolled, relative to the text origin) of `offset`.
    fn position(&self, offset: usize) -> Point<Pixels> {
        let ix = self.line_of(offset);
        let Some(line) = self.lines.get(ix) else {
            return point(px(0.), px(0.));
        };
        let local = offset - self.starts[ix];
        let p = line
            .position_for_index(local, self.line_height)
            .unwrap_or_else(|| point(line.width(), px(0.)));
        point(p.x, self.tops[ix] + p.y)
    }

    /// Offset closest to a content position (unscrolled, relative to the text origin).
    fn offset_at(&self, p: Point<Pixels>, text_len: usize) -> usize {
        if self.lines.is_empty() || p.y < px(0.) {
            return 0;
        }
        let ix = self.tops.partition_point(|t| *t <= p.y).saturating_sub(1);
        let line = &self.lines[ix];
        let local = point(p.x.max(px(0.)), p.y - self.tops[ix]);
        let height = line.size(self.line_height).height;
        if local.y >= height {
            // Below the last line.
            return if ix + 1 == self.lines.len() {
                text_len
            } else {
                self.starts[ix] + line.len()
            };
        }
        let i = match line.closest_index_for_position(local, self.line_height) {
            Ok(i) | Err(i) => i,
        };
        (self.starts[ix] + i).min(text_len)
    }

    fn content_height(&self) -> Pixels {
        match (self.tops.last(), self.lines.last()) {
            (Some(top), Some(line)) => *top + line.size(self.line_height).height,
            _ => self.line_height,
        }
    }
}

/// Multi-line text input state.
pub struct TextArea {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    goal_x: Option<Pixels>,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    last_edit: Option<(Instant, EditKind)>,
    ruler: Option<u32>,
    font_family: Option<SharedString>,
    min_lines: u32,
    max_lines: u32,
    visual_rows: u32,
    scroll_y: Pixels,
    autoscroll: bool,
    layout: Option<LayoutCache>,
    is_selecting: bool,
}

impl EventEmitter<TextAreaEvent> for TextArea {}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextArea {
    /// An empty text area with a placeholder (3 to 12 visible lines).
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        TextArea {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            goal_x: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit: None,
            ruler: None,
            font_family: None,
            min_lines: 3,
            max_lines: 12,
            visual_rows: 3,
            scroll_y: px(0.),
            autoscroll: false,
            layout: None,
            is_selecting: false,
        }
    }

    /// The text.
    pub fn text(&self) -> &str {
        &self.content
    }

    /// True when the text is empty.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Replaces the whole text (one undo step) and puts the caret at the end.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        if text == self.content {
            return;
        }
        self.push_undo(EditKind::Other);
        self.content = text;
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_x = None;
        self.autoscroll = true;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    /// Sets the placeholder shown while empty.
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Draws a vertical guide after `column` characters (`None` or 0 = no ruler).
    pub fn set_ruler(&mut self, column: Option<u32>, cx: &mut Context<Self>) {
        self.ruler = column.filter(|c| *c > 0);
        cx.notify();
    }

    /// Uses `family` instead of the inherited font (e.g. the buffer font).
    pub fn set_font_family(&mut self, family: Option<SharedString>, cx: &mut Context<Self>) {
        self.font_family = family;
        cx.notify();
    }

    /// The area grows with its content between `min` and `max` visible lines.
    pub fn set_line_limits(&mut self, min: u32, max: u32, cx: &mut Context<Self>) {
        self.min_lines = min.max(1);
        self.max_lines = max.max(self.min_lines);
        cx.notify();
    }

    /// Selects the whole text.
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    /// Byte range of the selection.
    pub fn selection(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    /// Focuses the text area.
    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    // Editing primitives.

    fn push_undo(&mut self, kind: EditKind) {
        let now = Instant::now();
        let coalesce = kind == EditKind::Typing
            && self
                .last_edit
                .is_some_and(|(t, k)| k == EditKind::Typing && now - t < UNDO_COALESCE);
        if !coalesce {
            self.undo_stack.push(Snapshot {
                text: self.content.clone(),
                selection: self.selected_range.clone(),
            });
            if self.undo_stack.len() > MAX_UNDO {
                self.undo_stack.remove(0);
            }
        }
        self.redo_stack.clear();
        self.last_edit = Some((now, kind));
    }

    fn replace(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let start = self.clip(range.start.min(self.content.len()));
        let end = self.clip(range.end.min(self.content.len())).max(start);
        let typing = new_text.chars().count() == 1
            && !new_text.contains(char::is_whitespace)
            && start == end;
        self.push_undo(if typing {
            EditKind::Typing
        } else {
            EditKind::Other
        });
        self.content.replace_range(start..end, new_text);
        self.goal_x = None;
        self.autoscroll = true;
        cx.emit(TextAreaEvent::Changed);
    }

    fn clip(&self, offset: usize) -> usize {
        let mut o = offset.min(self.content.len());
        while !self.content.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    fn restore(&mut self, snap: Snapshot, cx: &mut Context<Self>) {
        self.content = snap.text;
        let end = self.content.len();
        self.selected_range = snap.selection.start.min(end)..snap.selection.end.min(end);
        self.selection_reversed = false;
        self.marked_range = None;
        self.goal_x = None;
        self.autoscroll = true;
        self.last_edit = None;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clip(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.autoscroll = true;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clip(offset);
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.autoscroll = true;
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn previous_word(&self, offset: usize) -> usize {
        self.content
            .unicode_word_indices()
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_word(&self, offset: usize) -> usize {
        self.content
            .unicode_word_indices()
            .find_map(|(idx, w)| (idx + w.len() > offset).then_some(idx + w.len()))
            .unwrap_or(self.content.len())
    }

    fn line_start(&self, offset: usize) -> usize {
        self.content[..offset].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.content[offset..]
            .find('\n')
            .map_or(self.content.len(), |i| offset + i)
    }

    /// Offset one visual line above (`dir < 0`) or below the caret, keeping the goal x.
    fn vertical(&mut self, rows: i32) -> usize {
        let cursor = self.cursor_offset();
        let Some(layout) = &self.layout else {
            return if rows < 0 { 0 } else { self.content.len() };
        };
        let p = layout.position(cursor);
        let x = *self.goal_x.get_or_insert(p.x);
        let y = p.y + layout.line_height * (rows as f32) + layout.line_height / 2.;
        if y < px(0.) {
            return 0;
        }
        if y >= layout.content_height() {
            return self.content.len();
        }
        layout.offset_at(point(x, y), self.content.len())
    }

    fn page_rows(&self) -> i32 {
        self.visual_rows.min(self.max_lines).max(1) as i32
    }

    // Actions.

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.previous_word(self.cursor_offset()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.next_word(self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.previous_word(self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.next_word(self.cursor_offset()), cx);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(-1);
        let goal = self.goal_x;
        self.move_to(target, cx);
        self.goal_x = goal;
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(1);
        let goal = self.goal_x;
        self.move_to(target, cx);
        self.goal_x = goal;
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(-1);
        self.select_to(target, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(1);
        self.select_to(target, cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(-self.page_rows());
        let goal = self.goal_x;
        self.move_to(target, cx);
        self.goal_x = goal;
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(self.page_rows());
        let goal = self.goal_x;
        self.move_to(target, cx);
        self.goal_x = goal;
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.line_start(self.cursor_offset()), cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.line_end(self.cursor_offset()), cx);
    }

    fn select_to_line_start(
        &mut self,
        _: &SelectToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goal_x = None;
        self.select_to(self.line_start(self.cursor_offset()), cx);
    }

    fn select_to_line_end(&mut self, _: &SelectToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.line_end(self.cursor_offset()), cx);
    }

    fn move_to_start(&mut self, _: &MoveToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(0, cx);
    }

    fn move_to_end(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.content.len(), cx);
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.content.len(), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_word(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snap) = self.undo_stack.pop() {
            self.redo_stack.push(Snapshot {
                text: self.content.clone(),
                selection: self.selected_range.clone(),
            });
            self.restore(snap, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snap) = self.redo_stack.pop() {
            self.undo_stack.push(Snapshot {
                text: self.content.clone(),
                selection: self.selected_range.clone(),
            });
            self.restore(snap, cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n");
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    // Mouse.

    fn offset_for_mouse(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = &self.layout else {
            return self.content.len();
        };
        let local = point(
            position.x - layout.bounds.left(),
            position.y - layout.bounds.top() + self.scroll_y,
        );
        layout.offset_at(local, self.content.len())
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.goal_x = None;
        let offset = self.offset_for_mouse(event.position);
        match event.click_count {
            2 => {
                let start = self.previous_word(self.next_boundary(offset));
                let end = self.next_word(start);
                self.selected_range = start..end.max(start);
                self.selection_reversed = false;
                cx.notify();
            }
            n if n >= 3 => {
                self.selected_range = self.line_start(offset)..self.line_end(offset);
                self.selection_reversed = false;
                cx.notify();
            }
            _ => {
                self.is_selecting = true;
                if event.modifiers.shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, cx)
                }
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.offset_for_mouse(event.position);
            self.select_to(offset, cx);
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = &self.layout else {
            return;
        };
        let max = (layout.content_height() - layout.bounds.size.height).max(px(0.));
        if max <= px(0.) {
            return;
        }
        let delta = event.delta.pixel_delta(window.line_height());
        let new = (self.scroll_y - delta.y).clamp(px(0.), max);
        if new != self.scroll_y {
            self.scroll_y = new;
            cx.stop_propagation();
            cx.notify();
        }
    }

    // UTF-16 conversions for the platform input handler.

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

impl EntityInputHandler for TextArea {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        self.content.get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = new_text.replace("\r\n", "\n").replace('\r', "\n");
        self.replace(range.clone(), &new_text, cx);
        let caret = self.clip(range.start + new_text.len());
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range.take();
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
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.replace(range.clone(), new_text, cx);
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.position(range.start);
        let end = layout.position(range.end);
        let origin = layout.bounds.origin - point(px(0.), self.scroll_y);
        Some(Bounds::from_corners(
            origin + start,
            origin + point(end.x.max(start.x), start.y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_for_mouse(point);
        Some(self.offset_to_utf16(offset))
    }
}

struct TextAreaElement {
    area: Entity<TextArea>,
}

struct Prepaint {
    lines: Vec<WrappedLine>,
    tops: Vec<Pixels>,
    line_height: Pixels,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
    ruler: Option<PaintQuad>,
}

impl IntoElement for TextAreaElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn shape(
    text: &str,
    color: Hsla,
    marked: Option<&Range<usize>>,
    wrap: Pixels,
    window: &mut Window,
) -> (Vec<WrappedLine>, Vec<usize>) {
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs: Vec<TextRun> = match marked {
        Some(m) if m.end <= text.len() => vec![
            TextRun {
                len: m.start,
                ..run.clone()
            },
            TextRun {
                len: m.end - m.start,
                underline: Some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: text.len() - m.end,
                ..run
            },
        ],
        _ => vec![run],
    };
    let lines: Vec<WrappedLine> = window
        .text_system()
        .shape_text(
            SharedString::from(text.to_string()),
            font_size,
            &runs,
            Some(wrap),
            None,
        )
        .map(|l| l.into_iter().collect())
        .unwrap_or_default();
    let mut starts = Vec::with_capacity(lines.len());
    let mut at = 0usize;
    for line in &lines {
        starts.push(at);
        at += line.len() + 1;
    }
    (lines, starts)
}

impl Element for TextAreaElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let area = self.area.read(cx);
        let rows = area.visual_rows.clamp(area.min_lines, area.max_lines);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let ui = cx.theme().ui.clone();
        let area = self.area.read(cx);
        let line_height = window.line_height();
        let wrap = (bounds.size.width - px(2.)).max(px(20.));
        let placeholder = area.content.is_empty();
        let (lines, starts) = if placeholder {
            let text = area.placeholder.to_string();
            shape(&text, ui.text_faint.hsla(), None, wrap, window)
        } else {
            let text = area.content.clone();
            let marked = area.marked_range.clone();
            shape(
                &text,
                window.text_style().color,
                marked.as_ref(),
                wrap,
                window,
            )
        };
        let mut tops = Vec::with_capacity(lines.len());
        let mut y = px(0.);
        for line in &lines {
            tops.push(y);
            y += line.size(line_height).height;
        }
        let content_height = y.max(line_height);
        let visual_rows = (content_height / line_height).round().max(1.) as u32;

        let ruler_col = area.ruler;
        let selected = area.selected_range.clone();
        let cursor_offset = area.cursor_offset();
        let mut scroll_y = area.scroll_y;
        let autoscroll = area.autoscroll;
        let cache = LayoutCache {
            lines: if placeholder {
                Vec::new()
            } else {
                lines.clone()
            },
            starts: if placeholder { vec![0] } else { starts },
            tops: if placeholder {
                vec![px(0.)]
            } else {
                tops.clone()
            },
            line_height,
            bounds,
        };
        let cache_lines_empty = cache.lines.is_empty();
        let caret = if cache_lines_empty {
            point(px(0.), px(0.))
        } else {
            cache.position(cursor_offset)
        };
        // Keep the caret visible after edits and keyboard moves.
        let max_scroll = (content_height - bounds.size.height).max(px(0.));
        if autoscroll {
            if caret.y < scroll_y {
                scroll_y = caret.y;
            } else if caret.y + line_height > scroll_y + bounds.size.height {
                scroll_y = caret.y + line_height - bounds.size.height;
            }
        }
        scroll_y = scroll_y.clamp(px(0.), max_scroll);
        let origin = point(bounds.left(), bounds.top() - scroll_y);

        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let cursor_height = (font_size * 1.3).min(line_height);
        let cursor_inset = (line_height - cursor_height) / 2.;
        let mut selections = Vec::new();
        let cursor = if selected.is_empty() {
            Some(fill(
                Bounds::new(
                    origin + point(caret.x, caret.y + cursor_inset),
                    size(px(1.5), cursor_height),
                ),
                ui.cursor.hsla(),
            ))
        } else {
            let a = cache.position(selected.start);
            let b = cache.position(selected.end);
            let right = bounds.size.width;
            let color = ui.selection.hsla();
            if a.y == b.y {
                selections.push(fill(
                    Bounds::from_corners(origin + a, origin + point(b.x, b.y + line_height)),
                    color,
                ));
            } else {
                selections.push(fill(
                    Bounds::from_corners(origin + a, origin + point(right, a.y + line_height)),
                    color,
                ));
                if b.y > a.y + line_height {
                    selections.push(fill(
                        Bounds::from_corners(
                            origin + point(px(0.), a.y + line_height),
                            origin + point(right, b.y),
                        ),
                        color,
                    ));
                }
                selections.push(fill(
                    Bounds::from_corners(
                        origin + point(px(0.), b.y),
                        origin + point(b.x, b.y + line_height),
                    ),
                    color,
                ));
            }
            None
        };
        let ruler = ruler_col.and_then(|col| {
            let sample = "0".repeat(10);
            let run = TextRun {
                len: sample.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let w = window
                .text_system()
                .shape_line(sample.into(), font_size, &[run], None)
                .width
                / 10.;
            let x = w * col as f32;
            (x < bounds.size.width).then(|| {
                fill(
                    Bounds::new(
                        point(bounds.left() + x, bounds.top()),
                        size(px(1.), bounds.size.height),
                    ),
                    ui.border_variant.hsla(),
                )
            })
        });

        let changed_rows = visual_rows != area.visual_rows;
        self.area.update(cx, |area, cx| {
            area.layout = Some(cache);
            area.visual_rows = visual_rows;
            area.scroll_y = scroll_y;
            area.autoscroll = false;
            if changed_rows {
                // Grow/shrink on the next frame.
                cx.notify();
            }
        });
        Prepaint {
            lines,
            tops,
            line_height,
            cursor,
            selections,
            ruler,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus_handle, scroll_y) = {
            let a = self.area.read(cx);
            (a.focus_handle.clone(), a.scroll_y)
        };
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.area.clone()),
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(ruler) = prepaint.ruler.take() {
                window.paint_quad(ruler);
            }
            for quad in prepaint.selections.drain(..) {
                window.paint_quad(quad);
            }
            for (line, top) in prepaint.lines.iter().zip(&prepaint.tops) {
                let y = bounds.top() + *top - scroll_y;
                let h = line.size(prepaint.line_height).height;
                if y + h < bounds.top() || y > bounds.bottom() {
                    continue;
                }
                if let Err(err) = line.paint(
                    point(bounds.left(), y),
                    prepaint.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                ) {
                    tracing::debug!("text area paint failed: {err:#}");
                }
            }
            if focus_handle.is_focused(window) {
                if let Some(cursor) = prepaint.cursor.take() {
                    window.paint_quad(cursor);
                }
            }
        });
    }
}

impl Render for TextArea {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .w_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .when_some(self.font_family.clone(), |el, f| el.font_family(f))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_to_line_start))
            .on_action(cx.listener(Self::select_to_line_end))
            .on_action(cx.listener(Self::move_to_start))
            .on_action(cx.listener(Self::move_to_end))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TextAreaElement { area: cx.entity() })
    }
}

/// Length in characters of the first line (the commit subject) of `text`.
pub fn first_line_chars(text: &str) -> usize {
    text.lines().next().map_or(0, |l| l.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_length() {
        assert_eq!(first_line_chars(""), 0);
        assert_eq!(first_line_chars("Fix the thing\n\nbody"), 13);
        assert_eq!(first_line_chars("héllo"), 5);
    }
}
