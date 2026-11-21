//! Caret movement and view actions of the [`CodeView`]: moving and
//! extending every selection, paging, find next/previous, copy, folding,
//! wrap and minimap toggles, and change navigation.

use gpui::{prelude::*, px, App, ClipboardItem, Pixels, Window};

use super::selection::{Pos, Selection};
use super::{edit, selection, ActionListener, CodeView, EditorSettings, ScrollRequest};
use crate::actions::editor::*;

impl CodeView {
    /// Applies `f` to every selection: it returns the new head (and goal);
    /// with `extend` the anchor stays, otherwise the selection collapses.
    fn move_selections(
        &mut self,
        extend: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&Self, &Selection) -> (Pos, Option<usize>),
    ) {
        let moved: Vec<Selection> = self
            .selections
            .iter()
            .map(|sel| {
                let (head, goal) = f(self, sel);
                Selection {
                    anchor: if extend { sel.anchor } else { head },
                    head,
                    goal_column: goal,
                }
            })
            .collect();
        let (sels, primary) = selection::normalize(moved, self.primary);
        self.selections = sels;
        self.primary = primary;
        self.selections_changed(true, cx);
    }

    fn move_head(&mut self, extend: bool, cx: &mut Context<Self>, f: impl Fn(&Self, Pos) -> Pos) {
        self.move_selections(extend, cx, |this, sel| (f(this, sel.head), None));
    }

    fn move_vertical(&mut self, delta: i64, extend: bool, cx: &mut Context<Self>) {
        let tab = Self::tab_size(cx);
        self.move_selections(extend, cx, |this, sel| {
            // Collapse to the selection edge in the direction of travel.
            let from = if !extend && !sel.is_empty() {
                let (s, e) = sel.range();
                if delta < 0 {
                    s
                } else {
                    e
                }
            } else {
                sel.head
            };
            let (head, goal) = selection::vertical(
                this.source.as_ref(),
                &this.display_map,
                from,
                sel.goal_column,
                delta,
                tab,
            );
            (head, Some(goal))
        });
    }

    pub(super) fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, false, cx);
    }
    pub(super) fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, false, cx);
    }
    pub(super) fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, true, cx);
    }
    pub(super) fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, true, cx);
    }
    pub(super) fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.page_rows();
        self.scroll.y = (self.scroll.y - self.line_height_px(cx) * n as f32).max(px(0.));
        self.move_vertical(-n, false, cx);
    }
    pub(super) fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.page_rows();
        self.scroll.y += self.line_height_px(cx) * n as f32;
        self.move_vertical(n, false, cx);
    }
    pub(super) fn select_page_up(
        &mut self,
        _: &SelectPageUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = self.page_rows();
        self.move_vertical(-n, true, cx);
    }
    pub(super) fn select_page_down(
        &mut self,
        _: &SelectPageDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = self.page_rows();
        self.move_vertical(n, true, cx);
    }

    fn line_height_px(&self, cx: &App) -> Pixels {
        self.last_layout
            .as_ref()
            .map(|l| l.line_height)
            .unwrap_or_else(|| {
                let s = EditorSettings::get(cx);
                (s.font_size * s.line_height).round()
            })
    }

    /// Registers `command` as an action listener that first lets an owner holding input
    /// back apply it (see [`CodeView::run_command_hook`]). Every action of the view goes
    /// through here.
    pub(super) fn command<A: 'static>(
        cx: &Context<Self>,
        command: fn(&mut Self, &A, &mut Window, &mut Context<Self>),
    ) -> ActionListener<A> {
        Box::new(cx.listener(move |this, action, window, cx| {
            this.run_command_hook(cx);
            command(this, action, window, cx);
        }))
    }

    pub(super) fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, sel| {
            if sel.is_empty() {
                (selection::left(this.source.as_ref(), sel.head), None)
            } else {
                (sel.range().0, None)
            }
        });
    }
    pub(super) fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, sel| {
            if sel.is_empty() {
                (selection::right(this.source.as_ref(), sel.head), None)
            } else {
                (sel.range().1, None)
            }
        });
    }
    pub(super) fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |this, p| selection::left(this.source.as_ref(), p));
    }
    pub(super) fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_head(true, cx, |this, p| {
            selection::right(this.source.as_ref(), p)
        });
    }
    pub(super) fn move_word_left(
        &mut self,
        _: &MoveWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(false, cx, |this, p| {
            selection::word_left(this.source.as_ref(), p)
        });
    }
    pub(super) fn move_word_right(
        &mut self,
        _: &MoveWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(false, cx, |this, p| {
            selection::word_right(this.source.as_ref(), p)
        });
    }
    pub(super) fn select_word_left(
        &mut self,
        _: &SelectWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(true, cx, |this, p| {
            selection::word_left(this.source.as_ref(), p)
        });
    }
    pub(super) fn select_word_right(
        &mut self,
        _: &SelectWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(true, cx, |this, p| {
            selection::word_right(this.source.as_ref(), p)
        });
    }

    fn line_start(this: &Self, p: Pos) -> Pos {
        // Smart home: first non-whitespace, then column 0.
        let line = this.source.line(p.row);
        let first = line.len() - line.trim_start().len();
        Pos::new(p.row, if p.col == first { 0 } else { first })
    }

    pub(super) fn move_to_line_start(
        &mut self,
        _: &MoveToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(false, cx, Self::line_start);
    }
    pub(super) fn move_to_line_end(
        &mut self,
        _: &MoveToLineEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(false, cx, |this, p| {
            Pos::new(p.row, this.source.line(p.row).len())
        });
    }
    pub(super) fn select_to_line_start(
        &mut self,
        _: &SelectToLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(true, cx, Self::line_start);
    }
    pub(super) fn select_to_line_end(
        &mut self,
        _: &SelectToLineEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_head(true, cx, |this, p| {
            Pos::new(p.row, this.source.line(p.row).len())
        });
    }

    pub(super) fn end_pos(&self) -> Pos {
        let last = self.source.line_count().saturating_sub(1);
        Pos::new(last, self.source.line(last).len())
    }

    pub(super) fn scroll_to_top(
        &mut self,
        _: &ScrollToTop,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_cursor(Pos::new(0, 0), cx);
    }
    pub(super) fn scroll_to_bottom(
        &mut self,
        _: &ScrollToBottom,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = self.end_pos();
        self.set_cursor(end, cx);
    }
    pub(super) fn select_to_top(
        &mut self,
        _: &SelectToTop,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sel = self.selection();
        self.set_selection(
            Selection {
                anchor: sel.anchor,
                head: Pos::new(0, 0),
                goal_column: None,
            },
            cx,
        );
    }
    pub(super) fn select_to_bottom(
        &mut self,
        _: &SelectToBottom,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = self.end_pos();
        let sel = self.selection();
        self.set_selection(
            Selection {
                anchor: sel.anchor,
                head: end,
                goal_column: None,
            },
            cx,
        );
    }

    /// Selects the whole text.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let end = self.end_pos();
        self.selections = vec![Selection {
            anchor: Pos::new(0, 0),
            head: end,
            goal_column: None,
        }];
        self.primary = 0;
        self.selections_changed(false, cx);
    }

    pub(super) fn select_all_action(
        &mut self,
        _: &SelectAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_all(cx);
    }

    pub(super) fn clear_selection(
        &mut self,
        _: &ClearSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selections.len() > 1 {
            let keep = self.selection();
            self.selections = vec![keep];
            self.primary = 0;
            self.selections_changed(true, cx);
        } else if !self.selection().is_empty() {
            let head = self.selection().head;
            self.set_cursor(head, cx);
        } else if !self.search_matches.is_empty() {
            self.search_matches.clear();
            self.active_match = None;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    pub(super) fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.step_search_match(1, false, cx).is_none() {
            cx.propagate();
        }
    }

    pub(super) fn find_prev(&mut self, _: &FindPrev, _: &mut Window, cx: &mut Context<Self>) {
        if self.step_search_match(-1, false, cx).is_none() {
            cx.propagate();
        }
    }

    /// Selected text of the primary selection (empty if it is empty).
    pub fn selected_text(&self) -> String {
        selection::selected_text(self.source.as_ref(), &self.selection())
    }

    /// Text of every selection (empty selections give the empty string).
    pub fn selected_texts(&self) -> Vec<String> {
        self.selections
            .iter()
            .map(|s| selection::selected_text(self.source.as_ref(), s))
            .collect()
    }

    /// Text copied for the current selections: selected text joined by
    /// newlines; empty selections copy their whole line.
    fn copy_text(&mut self, cx: &mut Context<Self>) -> String {
        if let Some(buffer) = self.buffer.clone() {
            let sels = self.selections.clone();
            let primary = self.primary;
            return buffer.update(cx, |b, cx| {
                let text = b.text().clone();
                let bsels = edit::to_buffer_selections(&text, &sels, primary);
                b.update_buffer(
                    |buf| {
                        buf.set_selections(bsels);
                        buf.copy()
                    },
                    cx,
                )
            });
        }
        if self.selections.iter().all(|s| s.is_empty()) {
            let mut rows: Vec<u32> = self.selections.iter().map(|s| s.head.row).collect();
            rows.dedup();
            return rows
                .iter()
                .map(|row| format!("{}\n", self.source.line(*row)))
                .collect();
        }
        self.selected_texts()
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Copies the selections (or the current lines when empty) to the clipboard.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self.copy_text(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    pub(super) fn copy_action(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy(cx);
    }

    pub(super) fn toggle_soft_wrap(
        &mut self,
        _: &ToggleSoftWrap,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let wrap = !self.soft_wrap(cx);
        self.set_soft_wrap(wrap, cx);
    }

    pub(super) fn toggle_minimap(
        &mut self,
        _: &ToggleMinimap,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let show = !self.show_minimap(cx);
        self.set_show_minimap(show, cx);
    }

    pub(super) fn toggle_fold(&mut self, _: &ToggleFold, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_fold_at_cursor(cx);
    }

    pub(super) fn fold_all(&mut self, _: &FoldAll, _: &mut Window, cx: &mut Context<Self>) {
        let ranges = self.fold_ranges();
        // Fold top-level regions only (nested ones are hidden anyway).
        let mut end = 0;
        for r in ranges.iter() {
            if r.start >= end {
                self.folded.insert(r.start);
                end = r.end;
            }
        }
        self.fold_generation += 1;
        self.display_map_key = None;
        let head = self.selection().head;
        if self.display_map_hides(head.row) {
            let start = ranges
                .iter()
                .filter(|r| r.start < head.row && head.row < r.end)
                .map(|r| r.start)
                .min()
                .unwrap_or(0);
            self.selections = vec![Selection::cursor(Pos::new(start, 0))];
            self.primary = 0;
        }
        self.pending_scroll = Some(ScrollRequest::FitCursor);
        cx.notify();
    }

    pub(super) fn unfold_all(&mut self, _: &UnfoldAll, _: &mut Window, cx: &mut Context<Self>) {
        self.folded.clear();
        self.fold_generation += 1;
        self.display_map_key = None;
        self.pending_scroll = Some(ScrollRequest::FitCursor);
        cx.notify();
    }

    fn jump_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let marks = self.source.gutter_marks(0..self.source.line_count());
        let row = self.selection().head.row;
        let target = if forward {
            marks
                .iter()
                .map(|m| m.rows.start)
                .filter(|r| *r > row)
                .min()
        } else {
            marks
                .iter()
                .map(|m| m.rows.start)
                .filter(|r| *r < row)
                .max()
        };
        if let Some(target) = target {
            self.set_cursor(Pos::new(target, 0), cx);
            self.pending_scroll = Some(ScrollRequest::Row {
                row: target,
                center: true,
            });
        }
    }

    pub(super) fn next_change(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_change(true, cx);
    }
    pub(super) fn prev_change(&mut self, _: &PrevChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_change(false, cx);
    }
}
