//! Editing actions of the [`CodeView`]: buffer-backed edits, undo/redo,
//! clipboard, line operations and multi-caret selection commands.

use std::collections::BTreeSet;
use std::rc::Rc;

use gpui::{prelude::*, px, ClipboardItem, Entity, Window};
use lector_buffer::{Buffer, Transaction};
use lector_text::Rope;

use super::edit::{EditBuffer, RopeSource};
use super::selection::{Pos, Selection};
use super::{edit, selection, CodeView, CodeViewEvent};
use crate::actions::editor::*;

impl CodeView {
    // Editing.

    /// Runs a mutating buffer operation with this view's selections, then
    /// takes the buffer's resulting selections. `None` when the view is not
    /// editable.
    pub fn edit<R>(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Buffer) -> (R, Transaction),
    ) -> Option<R> {
        if self.is_read_only() {
            return None;
        }
        let buffer = self.buffer.clone()?;
        let origin = cx.entity_id();
        let sels = self.selections.clone();
        let primary = self.primary;
        let started = std::time::Instant::now();
        let (result, new_sels, changed, rope) = buffer.update(cx, |b, cx| {
            let text = b.text().clone();
            let before = text.version();
            let bsels = edit::to_buffer_selections(&text, &sels, primary);
            let r = b.edit(Some(origin), Some(bsels), f, cx)?;
            let rope = b.text().clone();
            let changed = rope.version() != before;
            let sels = edit::from_buffer_selections(&rope, b.buffer().selections());
            Some((r, sels, changed, rope))
        })?;
        if changed && self.manage_source {
            self.use_rope_source(rope);
        }
        let (sels, primary) = selection::normalize(new_sels.0, new_sels.1);
        self.selections = sels;
        self.primary = primary;
        if changed {
            self.drag = None;
            cx.emit(CodeViewEvent::Edited);
        }
        self.selections_changed(true, cx);
        tracing::trace!(target: "lector_ui::perf", elapsed_us = started.elapsed().as_micros() as u64, "code view edit");
        Some(result)
    }

    /// Shows `rope` as the plain source.
    fn use_rope_source(&mut self, rope: Rope) {
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string());
        self.source = Rc::new(RopeSource::new(rope, name.as_deref()));
        self.display_map_key = None;
        self.fold_ranges_version = None;
    }

    /// Makes the source reflect the buffer right now. A view whose owner manages the source
    /// only learns of an edit when the owner's change event arrives, after the current
    /// update; code that edits and then keeps going in the same update (an owner finishing
    /// held-back input before a command) calls this so the command measures the new text.
    /// The owner's own source replaces it when its event arrives, before anything is drawn.
    pub(crate) fn sync_source_with_buffer(&mut self, cx: &mut Context<Self>) {
        if let Some(rope) = self.buffer.as_ref().map(|b| b.read(cx).text().clone()) {
            self.use_rope_source(rope);
        }
    }

    /// [`CodeView::edit`] for operations returning only a transaction.
    pub fn edit_buffer(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Buffer) -> Transaction,
    ) -> bool {
        self.edit(cx, |b| ((), f(b))).is_some()
    }

    /// Runs a selection-only buffer operation (works while read-only).
    fn select_with(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Buffer)) -> bool {
        let Some(buffer) = self.buffer.clone() else {
            return false;
        };
        let sels = self.selections.clone();
        let primary = self.primary;
        let new_sels = buffer.update(cx, |b, cx| {
            b.catch_up(cx);
            let text = b.text().clone();
            let bsels = edit::to_buffer_selections(&text, &sels, primary);
            b.update_buffer(
                |buf| {
                    buf.set_selections(bsels);
                    f(buf);
                    edit::from_buffer_selections(&text, buf.selections())
                },
                cx,
            )
        });
        let (sels, primary) = selection::normalize(new_sels.0, new_sels.1);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
        true
    }

    /// Another view (or the owner) changed the shared buffer: map carets,
    /// folds and search matches through the edit.
    pub(super) fn buffer_edited(
        &mut self,
        origin: Option<gpui::EntityId>,
        transaction: &Transaction,
        buffer: &Entity<EditBuffer>,
        cx: &mut Context<Self>,
    ) {
        let deltas = &transaction.deltas;
        let version = buffer.read(cx).text().version();
        self.splice_wrap_counts(deltas, version);
        if !self.folded.is_empty() {
            let folded: BTreeSet<u32> = self
                .folded
                .iter()
                .filter(|row| {
                    // Drop folds whose header row was replaced.
                    !deltas.iter().any(|d| {
                        d.start.row < **row && **row <= d.old_end.row
                            || (d.start.row == **row && d.old_end.row > **row)
                    })
                })
                .map(|row| edit::map_row_through(*row, deltas))
                .collect();
            self.folded = folded;
            self.fold_generation += 1;
            self.display_map_key = None;
        }
        if !self.search_matches.is_empty() {
            let edited: Vec<(u32, u32)> = deltas
                .iter()
                .map(|d| (d.start.row, d.new_end.row))
                .collect();
            self.search_matches = self
                .search_matches
                .drain(..)
                .filter_map(|(row, range)| {
                    let start = edit::map_pos_through(Pos::new(row, range.start), deltas);
                    let end = edit::map_pos_through(Pos::new(row, range.end), deltas);
                    let touched = edited
                        .iter()
                        .any(|(s, e)| start.row >= *s && start.row <= *e);
                    (!touched && start.row == end.row).then_some((start.row, start.col..end.col))
                })
                .collect();
            self.active_match = None;
        }
        if origin == Some(cx.entity_id()) {
            return;
        }
        let mapped: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| Selection {
                anchor: edit::map_pos_through(s.anchor, deltas),
                head: edit::map_pos_through(s.head, deltas),
                goal_column: s.goal_column,
            })
            .collect();
        let (sels, primary) = selection::normalize(mapped, self.primary);
        self.selections = sels;
        self.primary = primary;
        self.marked = None;
        if self.manage_source {
            let rope = buffer.read(cx).text().clone();
            self.source = Rc::new(RopeSource::new(rope, None));
            self.display_map_key = None;
        }
        cx.emit(CodeViewEvent::SelectionChanged);
        cx.notify();
    }

    pub(super) fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.undo()) {
            cx.propagate();
        }
    }
    pub(super) fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.redo()) {
            cx.propagate();
        }
    }
    pub(super) fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_read_only() {
            // Read-only: behave like copy.
            self.copy(cx);
            return;
        }
        if let Some(text) = self.edit(cx, |b| b.cut()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
    pub(super) fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        if !self.edit_buffer(cx, |b| b.paste(&text)) {
            cx.propagate();
        }
    }
    pub(super) fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.newline()) {
            cx.propagate();
        }
    }
    pub(super) fn newline_below(
        &mut self,
        _: &NewlineBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.newline_below()) {
            cx.propagate();
        }
    }
    pub(super) fn newline_above(
        &mut self,
        _: &NewlineAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.newline_above()) {
            cx.propagate();
        }
    }
    pub(super) fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.backspace()) {
            cx.propagate();
        }
    }
    pub(super) fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.delete()) {
            cx.propagate();
        }
    }
    pub(super) fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.delete_word_left()) {
            cx.propagate();
        }
    }
    pub(super) fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.delete_word_right()) {
            cx.propagate();
        }
    }
    pub(super) fn delete_to_line_start(
        &mut self,
        _: &DeleteToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.delete_to_line_start()) {
            cx.propagate();
        }
    }
    pub(super) fn delete_line(&mut self, _: &DeleteLine, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.delete_line()) {
            cx.propagate();
        }
    }
    pub(super) fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.tab()) {
            cx.propagate();
        }
    }
    pub(super) fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.indent()) {
            cx.propagate();
        }
    }
    pub(super) fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.outdent()) {
            cx.propagate();
        }
    }
    pub(super) fn toggle_comment(
        &mut self,
        _: &ToggleComment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.toggle_comment()) {
            cx.propagate();
        }
    }
    pub(super) fn duplicate_line(
        &mut self,
        _: &DuplicateLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.duplicate_line()) {
            cx.propagate();
        }
    }
    pub(super) fn move_line_up(&mut self, _: &MoveLineUp, _: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_buffer(cx, |b| b.move_lines(true)) {
            cx.propagate();
        }
    }
    pub(super) fn move_line_down(
        &mut self,
        _: &MoveLineDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_buffer(cx, |b| b.move_lines(false)) {
            cx.propagate();
        }
    }

    // Selection operations (buffer-backed when available).

    pub(super) fn select_line(&mut self, _: &SelectLine, _: &mut Window, cx: &mut Context<Self>) {
        if self.select_with(cx, |b| b.select_line()) {
            return;
        }
        let moved: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| {
                let (start, end) = s.range();
                let (a, _) = self.line_range(start.row);
                let end_row = if end.col == 0 && end.row > start.row && !s.is_empty() {
                    end.row
                } else {
                    end.row + 1
                };
                let b = if end_row < self.source.line_count() {
                    Pos::new(end_row, 0)
                } else {
                    self.end_pos()
                };
                Selection {
                    anchor: a,
                    head: b,
                    goal_column: None,
                }
            })
            .collect();
        self.set_selections_keep_primary(moved, cx);
    }

    fn set_selections_keep_primary(&mut self, sels: Vec<Selection>, cx: &mut Context<Self>) {
        let (sels, primary) = selection::normalize(sels, self.primary);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
    }

    pub(super) fn select_next_occurrence(
        &mut self,
        _: &SelectNextOccurrence,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.select_with(cx, |b| {
            b.select_next_occurrence();
        }) {
            return;
        }
        self.select_occurrences_fallback(false, cx);
    }

    pub(super) fn select_all_occurrences(
        &mut self,
        _: &SelectAllOccurrences,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.select_with(cx, |b| b.select_all_occurrences()) {
            return;
        }
        self.select_occurrences_fallback(true, cx);
    }

    /// Occurrence selection for views without a buffer (diffs, large files).
    fn select_occurrences_fallback(&mut self, all: bool, cx: &mut Context<Self>) {
        let primary = self.selection();
        if primary.is_empty() {
            if let Some((s, e, _)) = selection::word_at(self.source.as_ref(), primary.head) {
                self.set_selection(
                    Selection {
                        anchor: s,
                        head: e,
                        goal_column: None,
                    },
                    cx,
                );
            }
            if !all {
                return;
            }
        }
        let needle = self.selected_text();
        if needle.is_empty() || needle.contains('\n') {
            return;
        }
        let mut found: Vec<Selection> = Vec::new();
        let n = self.source.line_count();
        for row in 0..n {
            let line = self.source.line(row);
            for (ix, _) in line.match_indices(needle.as_str()) {
                found.push(Selection {
                    anchor: Pos::new(row, ix),
                    head: Pos::new(row, ix + needle.len()),
                    goal_column: None,
                });
            }
            if found.len() > 10_000 {
                break;
            }
        }
        if all {
            if !found.is_empty() {
                self.set_selections(found, cx);
            }
            return;
        }
        let last_end = self
            .selections
            .iter()
            .map(|s| s.range().1)
            .max()
            .unwrap_or_default();
        let next = found
            .iter()
            .find(|f| f.anchor >= last_end)
            .or_else(|| found.first())
            .copied();
        if let Some(next) = next {
            let mut sels = self.selections.clone();
            sels.push(next);
            let primary = sels.len() - 1;
            let (sels, primary) = selection::normalize(sels, primary);
            self.selections = sels;
            self.primary = primary;
            self.selections_changed(true, cx);
        }
    }

    fn add_cursor_vertical(&mut self, delta: i64, cx: &mut Context<Self>) {
        // Add a caret above the topmost / below the bottommost caret, keeping
        // the display column (soft wrap and folds aware).
        let tab = Self::tab_size(cx);
        let edge = if delta < 0 {
            self.selections.first().copied()
        } else {
            self.selections.last().copied()
        };
        let Some(edge) = edge else { return };
        let (head, goal) = selection::vertical(
            self.source.as_ref(),
            &self.display_map,
            edge.head,
            edge.goal_column,
            delta,
            tab,
        );
        if head.row == edge.head.row {
            return;
        }
        let mut sels = self.selections.clone();
        sels.push(Selection {
            anchor: head,
            head,
            goal_column: Some(goal),
        });
        let primary = sels.len() - 1;
        let (sels, primary) = selection::normalize(sels, primary);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
    }

    pub(super) fn add_cursor_above(
        &mut self,
        _: &AddCursorAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_vertical(-1, cx);
    }
    pub(super) fn add_cursor_below(
        &mut self,
        _: &AddCursorBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_vertical(1, cx);
    }

    pub(super) fn split_selection_into_lines(
        &mut self,
        _: &SplitSelectionIntoLines,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.select_with(cx, |b| b.split_selection_into_lines()) {
            return;
        }
        let mut out = Vec::new();
        for s in &self.selections {
            let (start, end) = s.range();
            if start.row == end.row {
                out.push(*s);
                continue;
            }
            for row in start.row..=end.row {
                let len = self.source.line(row).len();
                let a = if row == start.row { start.col } else { 0 };
                let b = if row == end.row { end.col } else { len };
                if row == end.row && b == 0 {
                    continue;
                }
                out.push(Selection {
                    anchor: Pos::new(row, a),
                    head: Pos::new(row, b),
                    goal_column: None,
                });
            }
        }
        if !out.is_empty() {
            self.set_selections(out, cx);
        }
    }

    /// Shift-F10 / the Menu key: the context menu at the caret (below its line).
    pub(super) fn show_context_menu(
        &mut self,
        _: &crate::actions::workspace::ShowContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.selection().head;
        let below = self.last_layout.as_ref().map(|l| l.line_height);
        let anchor = self
            .point_for_pos(head)
            .map(|p| gpui::point(p.x, p.y + below.unwrap_or(px(20.))))
            .unwrap_or_else(|| gpui::point(px(120.), px(120.)));
        self.deploy_context_menu(anchor, window, cx);
    }

    pub(super) fn toggle_read_only_action(
        &mut self,
        _: &ToggleReadOnly,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.buffer.is_none() {
            cx.propagate();
            return;
        }
        self.toggle_read_only(cx);
    }
}
