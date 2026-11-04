//! Caret movement, selection commands and multi-cursor operations.

use lector_text::Bias;

use crate::buffer::{Buffer, OccurrenceSession};
use crate::selection::Selection;
use crate::text_util::{
    clip_grapheme, col_chars, identifier_range_at, is_word_char, leading_ws_len, next_grapheme,
    offset_at_col, prev_grapheme, row_end_with_eol, row_of, row_start, word_left, word_range_at,
    word_right,
};
use crate::Motion;

impl Buffer {
    fn vertical(&self, head: usize, goal: Option<u32>, delta: i64) -> (usize, u32) {
        let rope = &self.text;
        let row = row_of(rope, head) as i64;
        let col = goal.unwrap_or_else(|| col_chars(rope, head));
        let max = rope.max_point().row as i64;
        let mut target = row + delta;
        if target < 0 {
            if row == 0 {
                return (0, col);
            }
            target = 0;
        } else if target > max {
            if row == max {
                return (rope.len(), col);
            }
            target = max;
        }
        (offset_at_col(rope, target as u32, col), col)
    }

    /// Moves every caret; with `extend` the anchors stay and selections grow.
    /// Without `extend`, plain Left/Right collapse a selection to its edge.
    pub fn move_carets(&mut self, motion: Motion, extend: bool) {
        self.history.break_coalescing();
        let rope = &self.text;
        let mut out = Vec::with_capacity(self.sels.len());
        for s in &self.sels {
            if !extend && !s.is_empty() && matches!(motion, Motion::Left | Motion::Right) {
                let r = s.range();
                let at = if motion == Motion::Left {
                    r.start
                } else {
                    r.end
                };
                out.push(Selection::caret(s.id, at));
                continue;
            }
            let head = s.head;
            let mut goal = None;
            let target = match motion {
                Motion::Left => prev_grapheme(rope, head),
                Motion::Right => next_grapheme(rope, head),
                Motion::WordLeft => word_left(rope, head),
                Motion::WordRight => word_right(rope, head),
                Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_) => {
                    let delta = match motion {
                        Motion::Up => -1,
                        Motion::Down => 1,
                        Motion::PageUp(n) => -i64::from(n.max(1)),
                        Motion::PageDown(n) => i64::from(n.max(1)),
                        _ => 0,
                    };
                    let (off, col) = self.vertical(head, s.goal, delta);
                    goal = Some(col);
                    off
                }
                Motion::LineStart => {
                    let lr = rope.line_range(row_of(rope, head));
                    let line = rope.text_for_range(lr.clone());
                    let first = lr.start + leading_ws_len(&line);
                    if head == first {
                        lr.start
                    } else {
                        first
                    }
                }
                Motion::LineEnd => rope.line_range(row_of(rope, head)).end,
                Motion::BufferStart => 0,
                Motion::BufferEnd => rope.len(),
            };
            out.push(Selection {
                id: s.id,
                anchor: if extend { s.anchor } else { target },
                head: target,
                goal,
            });
        }
        self.install_selections(out);
    }

    /// Selects the whole text with one selection.
    pub fn select_all(&mut self) {
        self.history.break_coalescing();
        let id = self.newest;
        let len = self.text.len();
        self.install_selections(vec![Selection {
            id,
            anchor: 0,
            head: len,
            goal: None,
        }]);
    }

    /// Selects the full lines of every selection; repeating extends each by
    /// one more line.
    pub fn select_line(&mut self) {
        self.history.break_coalescing();
        let rope = &self.text;
        let out = self
            .sels
            .iter()
            .map(|s| {
                let r = s.range();
                let first = row_of(rope, r.start);
                let start = row_start(rope, first);
                let last_row = rope.max_point().row;
                let mut last = row_of(rope, r.end);
                let whole = !r.is_empty()
                    && r.start == start
                    && (r.end == rope.len() || r.end == row_start(rope, last));
                let end = if whole {
                    // Already whole lines: take one more.
                    let end_row = if r.end == rope.len() { last_row } else { last };
                    if end_row >= last_row {
                        rope.len()
                    } else {
                        row_end_with_eol(rope, end_row)
                    }
                } else {
                    if r.end == row_start(rope, last) && last > first && !r.is_empty() {
                        last -= 1;
                    }
                    row_end_with_eol(rope, last)
                };
                Selection {
                    id: s.id,
                    anchor: start,
                    head: end,
                    goal: None,
                }
            })
            .collect();
        self.install_selections(out);
    }

    /// Replaces all selections with the word at `offset`.
    pub fn select_word_at(&mut self, offset: usize) {
        self.history.break_coalescing();
        let offset = self
            .text
            .clip_offset(offset.min(self.text.len()), Bias::Left);
        let r = word_range_at(&self.text, offset);
        let id = self.newest;
        self.install_selections(vec![Selection {
            id,
            anchor: r.start,
            head: r.end,
            goal: None,
        }]);
    }

    /// How the next occurrence of the newest selection is matched: a session
    /// started from a caret (word under the caret) keeps matching whole words,
    /// a selection that was made by hand matches as a plain substring.
    fn whole_word_session(&self, query: &str) -> bool {
        self.occurrence
            .as_ref()
            .is_some_and(|o| o.version == self.text.version() && o.query == query)
    }

    /// Ctrl+D. If a selection is empty, selects the word under each such
    /// caret. Otherwise adds the next occurrence (case-sensitive, wrapping,
    /// skipping ones that are already selected) of the newest selection's
    /// text: whole words only when the session started from a caret, any
    /// substring when it started from a selection. Returns whether the
    /// selections changed.
    pub fn select_next_occurrence(&mut self) -> bool {
        self.history.break_coalescing();
        if self.sels.iter().any(|s| s.is_empty()) {
            let rope = &self.text;
            let mut changed = false;
            let out: Vec<Selection> = self
                .sels
                .iter()
                .map(|s| {
                    if !s.is_empty() {
                        return *s;
                    }
                    let w = identifier_range_at(rope, s.head);
                    if w.is_empty() {
                        return *s;
                    }
                    changed = true;
                    Selection {
                        id: s.id,
                        anchor: w.start,
                        head: w.end,
                        goal: None,
                    }
                })
                .collect();
            if changed {
                self.install_selections(out);
                let newest = self.newest_selection();
                self.occurrence = Some(OccurrenceSession {
                    version: self.text.version(),
                    query: self.text.text_for_range(newest.range()),
                });
            }
            return changed;
        }
        let newest = self.newest_selection();
        let query = self.text.text_for_range(newest.range());
        if query.is_empty() {
            return false;
        }
        let whole_word = self.whole_word_session(&query);
        let hay = self.text.to_string();
        let from = newest.range().end;
        let taken = |a: usize, b: usize| {
            self.sels.iter().any(|s| {
                let r = s.range();
                a < r.end && r.start < b
            })
        };
        // After the newest selection first, then wrapping around to the start.
        let found = {
            let matches = matches_of(&hay, &query, whole_word);
            matches
                .clone()
                .filter(|&at| at >= from)
                .chain(matches.filter(|&at| at < from))
                .find(|&at| !taken(at, at + query.len()))
        };
        let Some(at) = found else {
            return false;
        };
        let id = self.new_id();
        let mut sels = self.sels.clone();
        sels.push(Selection {
            id,
            anchor: at,
            head: at + query.len(),
            goal: None,
        });
        self.newest = id;
        self.install_selections(sels);
        if whole_word {
            // The session continues with the same query.
            self.occurrence = Some(OccurrenceSession {
                version: self.text.version(),
                query,
            });
        }
        true
    }

    /// Selects every occurrence of the newest selection's text, or of the word
    /// under the caret when it is empty (whole words only in that case and
    /// when continuing a [`select_next_occurrence`](Buffer::select_next_occurrence)
    /// session that started from a caret; otherwise any substring).
    pub fn select_all_occurrences(&mut self) {
        self.history.break_coalescing();
        let newest = self.newest_selection();
        let (range, whole_word) = if newest.is_empty() {
            (identifier_range_at(&self.text, newest.head), true)
        } else {
            let query = self.text.text_for_range(newest.range());
            let whole_word = self.whole_word_session(&query);
            (newest.range(), whole_word)
        };
        if range.is_empty() {
            return;
        }
        let query = self.text.text_for_range(range.clone());
        let hay = self.text.to_string();
        let mut sels = Vec::new();
        let mut primary = None;
        for at in matches_of(&hay, &query, whole_word) {
            let id = if at == range.start {
                primary = Some(self.newest);
                self.newest
            } else {
                self.new_id()
            };
            sels.push(Selection {
                id,
                anchor: at,
                head: at + query.len(),
                goal: None,
            });
        }
        if sels.is_empty() {
            return;
        }
        if primary.is_none() {
            self.newest = sels.last().map_or(self.newest, |s| s.id);
        }
        self.install_selections(sels);
        if whole_word {
            self.occurrence = Some(OccurrenceSession {
                version: self.text.version(),
                query,
            });
        }
    }

    fn add_caret_vertical(&mut self, up: bool) {
        self.history.break_coalescing();
        let anchor_sel = if up {
            self.sels.first()
        } else {
            self.sels.last()
        };
        let Some(base) = anchor_sel.copied() else {
            return;
        };
        let head = base.head;
        let rope = &self.text;
        let row = row_of(rope, head);
        let target_row = if up {
            if row == 0 {
                return;
            }
            row - 1
        } else {
            if row >= rope.max_point().row {
                return;
            }
            row + 1
        };
        let col = base.goal.unwrap_or_else(|| col_chars(rope, head));
        let off = offset_at_col(rope, target_row, col);
        let id = self.new_id();
        let mut sels = self.sels.clone();
        sels.push(Selection {
            id,
            anchor: off,
            head: off,
            goal: Some(col),
        });
        self.newest = id;
        self.install_selections(sels);
    }

    /// Adds a caret on the line above the topmost selection (goal column kept).
    pub fn add_caret_above(&mut self) {
        self.add_caret_vertical(true);
    }

    /// Adds a caret on the line below the bottommost selection (goal column kept).
    pub fn add_caret_below(&mut self) {
        self.add_caret_vertical(false);
    }

    /// Adds a caret at `offset` (alt-click); it becomes the newest. When a
    /// caret already sits there it is removed instead (toggle), unless it is
    /// the only selection.
    pub fn add_caret(&mut self, offset: usize) {
        self.history.break_coalescing();
        let offset = clip_grapheme(&self.text, offset, Bias::Left);
        if self.sels.len() > 1 {
            if let Some(ix) = self
                .sels
                .iter()
                .position(|s| s.is_empty() && s.head == offset)
            {
                let mut sels = self.sels.clone();
                sels.remove(ix);
                self.install_selections(sels);
                return;
            }
        }
        let id = self.new_id();
        let mut sels = self.sels.clone();
        sels.push(Selection::caret(id, offset));
        self.newest = id;
        self.install_selections(sels);
    }

    /// Splits every multi-line selection into one caret per line end (the last
    /// caret sits at the selection's end).
    pub fn split_selection_into_lines(&mut self) {
        self.history.break_coalescing();
        let sels = self.sels.clone();
        let mut out: Vec<Selection> = Vec::with_capacity(sels.len());
        for s in sels {
            let rope = &self.text;
            let r = s.range();
            let first = row_of(rope, r.start);
            let end_row = row_of(rope, r.end);
            if r.is_empty() || first == end_row {
                out.push(s);
                continue;
            }
            let last = if r.end == row_start(rope, end_row) {
                end_row - 1
            } else {
                end_row
            };
            let positions: Vec<usize> = (first..=last)
                .map(|row| {
                    if row == end_row {
                        r.end
                    } else {
                        rope.line_range(row).end.max(r.start)
                    }
                })
                .collect();
            let mut last_id = s.id;
            for (k, pos) in positions.into_iter().enumerate() {
                let id = if k == 0 { s.id } else { self.new_id() };
                last_id = id;
                out.push(Selection::caret(id, pos));
            }
            if s.id == self.newest {
                self.newest = last_id;
            }
        }
        self.install_selections(out);
    }

    /// Escape: with several selections keep only the newest; with one
    /// non-empty selection collapse it to its caret.
    pub fn collapse_selections(&mut self) {
        self.history.break_coalescing();
        if self.sels.len() > 1 {
            let keep = self.newest_selection();
            self.install_selections(vec![keep]);
        } else if let Some(s) = self.sels.first().copied() {
            if !s.is_empty() {
                self.install_selections(vec![Selection::caret(s.id, s.head)]);
            }
        }
    }
}

/// Byte offsets of the non-overlapping matches of `query` in `hay`, in order;
/// with `whole_word` only those not touching a word character on either side.
fn matches_of<'a>(
    hay: &'a str,
    query: &'a str,
    whole_word: bool,
) -> impl Iterator<Item = usize> + Clone + 'a {
    hay.match_indices(query)
        .map(|(at, _)| at)
        .filter(move |&at| {
            !whole_word
                || (!hay[..at].chars().next_back().is_some_and(is_word_char)
                    && !hay[at + query.len()..]
                        .chars()
                        .next()
                        .is_some_and(is_word_char))
        })
}
