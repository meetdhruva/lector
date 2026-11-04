//! Line-level editing: indent, comments, duplicate, move, delete, clipboard.

use lector_text::Rope;

use crate::buffer::{side_pair, Buffer};
use crate::engine::{Change, EditKind, NewSel, Plan, Pos, Side};
use crate::text_util::{
    col_chars, leading_ws_len, row_end_with_eol, row_of, row_start, visual_col, Eol,
};
use crate::{IndentStyle, Transaction};

/// Rows touched by one or more selections.
#[derive(Clone, Debug)]
pub(crate) struct RowGroup {
    pub first: u32,
    pub last: u32,
    /// Indices into the selection list.
    pub sels: Vec<usize>,
}

/// Last row a selection covers: a non-empty multi-row selection ending at
/// column 0 does not include its final row.
fn covered_rows(rope: &Rope, range: std::ops::Range<usize>) -> (u32, u32) {
    let first = row_of(rope, range.start);
    let mut last = row_of(rope, range.end);
    if range.start != range.end && last > first && range.end == row_start(rope, last) {
        last -= 1;
    }
    (first, last)
}

impl Buffer {
    /// Groups selections by the rows they cover. Groups overlap-merge; with
    /// `merge_adjacent` neighbouring groups merge too.
    pub(crate) fn row_groups(&self, merge_adjacent: bool) -> Vec<RowGroup> {
        let mut out: Vec<RowGroup> = Vec::new();
        for (i, s) in self.sels.iter().enumerate() {
            let (first, last) = covered_rows(&self.text, s.range());
            if let Some(g) = out.last_mut() {
                if first <= g.last + u32::from(merge_adjacent) {
                    g.last = g.last.max(last);
                    g.sels.push(i);
                    continue;
                }
            }
            out.push(RowGroup {
                first,
                last,
                sels: vec![i],
            });
        }
        out
    }

    fn line_text(&self, row: u32) -> String {
        self.text.text_for_range(self.text.line_range(row))
    }

    /// Specs mapping every selection through the changes with sticky sides.
    fn sticky_specs(&self) -> Vec<NewSel> {
        self.identity_sels()
    }

    // ---- indentation -----------------------------------------------------------

    /// Indent text to add to a line whose leading whitespace is `lead`.
    fn indent_insert(&self, lead: &str) -> String {
        match self.options.indent {
            IndentStyle::Tabs => "\t".into(),
            IndentStyle::Spaces(n) => {
                let n = n.max(1);
                if lead.chars().all(|c| c == ' ') {
                    let k = lead.len() as u32 % n;
                    " ".repeat((n - k) as usize)
                } else {
                    " ".repeat(n as usize)
                }
            }
        }
    }

    /// Tab key: inserts an indent unit at carets (up to the next tab stop for
    /// spaces); with any non-empty selection indents the selected lines.
    pub fn tab(&mut self) -> Transaction {
        if self.sels.iter().any(|s| !s.is_empty()) {
            return self.indent();
        }
        let width = self.indent_width();
        let tab = self.options.tab_size;
        let tabs = matches!(self.options.indent, IndentStyle::Tabs);
        let rope = &self.text;
        let mut changes = Vec::with_capacity(self.sels.len());
        let mut specs = Vec::with_capacity(self.sels.len());
        for s in &self.sels {
            let off = s.head;
            let idx = changes.len();
            let text = if tabs {
                "\t".to_string()
            } else {
                let ls = row_start(rope, row_of(rope, off));
                let before = rope.text_for_range(ls..off);
                let col = visual_col(&before, tab);
                " ".repeat((width - col % width) as usize)
            };
            let len = text.len();
            changes.push(Change::insert(off, text));
            specs.push(NewSel {
                id: Some(s.id),
                anchor: Pos::In(idx, len),
                head: Pos::In(idx, len),
                goal: None,
            });
        }
        self.apply_plan(Plan {
            changes,
            sels: specs,
            kind: EditKind::Other,
        })
    }

    /// Indents every selected line by one level (blank lines are skipped
    /// unless the selection is a lone caret on that line).
    pub fn indent(&mut self) -> Transaction {
        let groups = self.row_groups(false);
        let mut changes = Vec::new();
        for g in &groups {
            for row in g.first..=g.last {
                let lr = self.text.line_range(row);
                if lr.is_empty() && g.first != g.last {
                    continue;
                }
                let line = self.line_text(row);
                let lead = &line[..leading_ws_len(&line)];
                changes.push(Change::insert(lr.start, self.indent_insert(lead)));
            }
        }
        let sels = self.sticky_specs();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    /// Removes one indentation level from every selected line.
    pub fn outdent(&mut self) -> Transaction {
        let groups = self.row_groups(false);
        let width = self.indent_width() as usize;
        let mut changes = Vec::new();
        for g in &groups {
            for row in g.first..=g.last {
                let line = self.line_text(row);
                let start = self.text.line_range(row).start;
                let remove = if line.starts_with('\t') {
                    1
                } else {
                    let spaces = line.chars().take_while(|c| *c == ' ').count();
                    if spaces == 0 {
                        0
                    } else {
                        spaces.min((spaces - 1) % width + 1)
                    }
                };
                if remove > 0 {
                    changes.push(Change::delete(start, start + remove));
                }
            }
        }
        let sels = self.sticky_specs();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    // ---- comments --------------------------------------------------------------

    /// Toggles comments on the selected lines. With a line token: all lines
    /// are commented unless every non-blank line already is (then they are
    /// uncommented); the token is inserted at the shallowest indentation.
    /// Without a line token the block delimiters wrap each selection's lines.
    pub fn toggle_comment(&mut self) -> Transaction {
        if let Some(tok) = self.options.comment.line.clone().filter(|t| !t.is_empty()) {
            return self.toggle_line_comment(&tok);
        }
        if let Some((open, close)) = self.options.comment.block.clone() {
            return self.toggle_block_comment(&open, &close);
        }
        Transaction::default()
    }

    fn toggle_line_comment(&mut self, tok: &str) -> Transaction {
        let mut rows: Vec<u32> = Vec::new();
        for g in self.row_groups(false) {
            rows.extend(g.first..=g.last);
        }
        rows.dedup();
        struct Info {
            start: usize,
            line: String,
            ws: usize,
            blank: bool,
        }
        let infos: Vec<Info> = rows
            .iter()
            .map(|&row| {
                let line = self.line_text(row);
                let ws = leading_ws_len(&line);
                Info {
                    start: self.text.line_range(row).start,
                    blank: ws == line.len(),
                    line,
                    ws,
                }
            })
            .collect();
        let non_blank: Vec<&Info> = infos.iter().filter(|i| !i.blank).collect();
        let all_commented =
            !non_blank.is_empty() && non_blank.iter().all(|i| i.line[i.ws..].starts_with(tok));
        let mut changes = Vec::new();
        if all_commented {
            for i in &non_blank {
                let at = i.start + i.ws;
                let after = &i.line[i.ws + tok.len()..];
                let extra = usize::from(after.starts_with(' '));
                changes.push(Change::delete(at, at + tok.len() + extra));
            }
        } else if non_blank.is_empty() {
            for i in &infos {
                changes.push(Change::insert(i.start + i.ws, format!("{tok} ")));
            }
        } else {
            let col = non_blank.iter().map(|i| i.ws).min().unwrap_or(0);
            for i in &non_blank {
                changes.push(Change::insert(i.start + col, format!("{tok} ")));
            }
        }
        let sels = self.sticky_specs();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    fn toggle_block_comment(&mut self, open: &str, close: &str) -> Transaction {
        let mut changes = Vec::new();
        for g in self.row_groups(false) {
            let lines: Vec<(u32, String)> =
                (g.first..=g.last).map(|r| (r, self.line_text(r))).collect();
            let Some((fr, ftext)) = lines.iter().find(|(_, t)| !t.trim().is_empty()) else {
                continue;
            };
            let Some((lr, ltext)) = lines.iter().rev().find(|(_, t)| !t.trim().is_empty()) else {
                continue;
            };
            let span_start = self.text.line_range(*fr).start + leading_ws_len(ftext);
            let span_end = self.text.line_range(*lr).start + ltext.trim_end().len();
            if span_end < span_start {
                continue;
            }
            let span = self.text.text_for_range(span_start..span_end);
            if span.len() >= open.len() + close.len()
                && span.starts_with(open)
                && span.ends_with(close)
            {
                let inner = &span[open.len()..span.len() - close.len()];
                let lead = usize::from(inner.starts_with(' '));
                let trail = usize::from(inner.len() > lead && inner.ends_with(' '));
                changes.push(Change::delete(span_start, span_start + open.len() + lead));
                changes.push(Change::delete(span_end - close.len() - trail, span_end));
            } else {
                changes.push(Change::insert(span_start, format!("{open} ")));
                changes.push(Change::insert(span_end, format!(" {close}")));
            }
        }
        let sels = self.sticky_specs();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    // ---- duplicate / move / delete ---------------------------------------------

    /// Copies the selected lines below themselves; selections move to the copy.
    pub fn duplicate_line(&mut self) -> Transaction {
        let eol = self.eol().as_str();
        let rope = &self.text;
        let mut changes = Vec::new();
        let mut specs: Vec<Option<NewSel>> = vec![None; self.sels.len()];
        for g in self.row_groups(false) {
            let block_start = row_start(rope, g.first);
            let text_end = rope.line_range(g.last).end;
            let term_end = row_end_with_eol(rope, g.last);
            let idx = changes.len();
            let (at, text, shift) = if term_end > text_end {
                (term_end, rope.text_for_range(block_start..term_end), 0)
            } else {
                let body = rope.text_for_range(block_start..text_end);
                (text_end, format!("{eol}{body}"), eol.len())
            };
            changes.push(Change::insert(at, text));
            for &i in &g.sels {
                let s = &self.sels[i];
                specs[i] = Some(NewSel {
                    id: Some(s.id),
                    anchor: Pos::In(idx, shift + s.anchor - block_start),
                    head: Pos::In(idx, shift + s.head - block_start),
                    goal: s.goal,
                });
            }
        }
        let sels = specs.into_iter().flatten().collect();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    /// Moves the selected lines one row up or down, keeping the selections on them.
    pub fn move_lines(&mut self, up: bool) -> Transaction {
        let rope = &self.text;
        let last_row = rope.max_point().row;
        let mut changes = Vec::new();
        let mut specs: Vec<Option<NewSel>> = vec![None; self.sels.len()];
        for g in self.row_groups(true) {
            let (r0, r1) = if up {
                if g.first == 0 {
                    continue;
                }
                (g.first - 1, g.last)
            } else {
                if g.last >= last_row {
                    continue;
                }
                (g.first, g.last + 1)
            };
            // Rows r0..=r1 with their terminators kept in place.
            let rows: Vec<u32> = (r0..=r1).collect();
            let texts: Vec<String> = rows.iter().map(|&r| self.line_text(r)).collect();
            let terms: Vec<String> = rows[..rows.len() - 1]
                .iter()
                .map(|&r| {
                    let lr = rope.line_range(r);
                    rope.text_for_range(lr.end..row_start(rope, r + 1))
                })
                .collect();
            let moving = if up {
                1..texts.len()
            } else {
                0..texts.len() - 1
            };
            let other = if up { 0 } else { texts.len() - 1 };
            let order: Vec<usize> = if up {
                moving.clone().chain(std::iter::once(other)).collect()
            } else {
                std::iter::once(other).chain(moving.clone()).collect()
            };
            let mut new_text = String::new();
            for (k, &src) in order.iter().enumerate() {
                new_text.push_str(&texts[src]);
                if k < terms.len() {
                    new_text.push_str(&terms[k]);
                }
            }
            let region_start = rope.line_range(r0).start;
            let region_end = rope.line_range(r1).end;
            let group_start = row_start(rope, g.first);
            // Offset of the group's first line inside the new region text.
            let group_in_new = if up {
                0
            } else {
                texts[other].len() + terms.first().map_or(0, String::len)
            };
            let idx = changes.len();
            changes.push(Change::replace(region_start, region_end, new_text));
            for &i in &g.sels {
                let s = &self.sels[i];
                let group_text_end = rope.line_range(g.last).end;
                let map = |p: usize| {
                    if p <= group_text_end || up {
                        Pos::In(idx, group_in_new + p.saturating_sub(group_start))
                    } else {
                        // Start of the row after the block: stays after it.
                        Pos::Old(row_end_with_eol(rope, r1), Side::Before)
                    }
                };
                specs[i] = Some(NewSel {
                    id: Some(s.id),
                    anchor: map(s.anchor),
                    head: map(s.head),
                    goal: s.goal,
                });
            }
        }
        let sels = self
            .sels
            .iter()
            .zip(specs)
            .map(|(s, spec)| {
                spec.unwrap_or_else(|| {
                    let (a, h) = side_pair(s);
                    NewSel {
                        id: Some(s.id),
                        anchor: Pos::Old(s.anchor, a),
                        head: Pos::Old(s.head, h),
                        goal: s.goal,
                    }
                })
            })
            .collect();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    /// Deletes the lines covered by the selections, caret to the same column.
    pub fn delete_line(&mut self) -> Transaction {
        let rope = &self.text;
        let groups = self.row_groups(true);
        let mut changes = Vec::with_capacity(groups.len());
        let mut specs: Vec<NewSel> = Vec::with_capacity(self.sels.len());
        for g in &groups {
            let mut start = row_start(rope, g.first);
            let end = row_end_with_eol(rope, g.last);
            if g.last == rope.max_point().row && g.first > 0 {
                start = rope.line_range(g.first - 1).end;
            }
            let idx = changes.len();
            changes.push(Change::delete(start, end));
            let head = self.sels[g.sels[0]].head;
            let col = col_chars(rope, head);
            for &i in &g.sels {
                specs.push(NewSel {
                    id: Some(self.sels[i].id),
                    anchor: Pos::LineCol(idx, col),
                    head: Pos::LineCol(idx, col),
                    goal: None,
                });
            }
        }
        self.apply_plan(Plan {
            changes,
            sels: specs,
            kind: EditKind::Other,
        })
    }

    // ---- clipboard -------------------------------------------------------------

    fn all_empty(&self) -> bool {
        self.sels.iter().all(|s| s.is_empty())
    }

    /// Selected text (joined with the line ending); with only empty selections
    /// the whole line of each caret (with its terminator).
    pub fn copy(&self) -> String {
        let eol = self.eol().as_str();
        let rope = &self.text;
        if self.all_empty() {
            let mut out = String::new();
            let mut last_row = None;
            for s in &self.sels {
                let row = row_of(rope, s.head);
                if last_row == Some(row) {
                    continue;
                }
                last_row = Some(row);
                let start = row_start(rope, row);
                let end = row_end_with_eol(rope, row);
                out.push_str(&rope.text_for_range(start..end));
                if rope.line_range(row).end == end {
                    out.push_str(eol);
                }
            }
            self.set_line_clip(Some(out.clone()));
            out
        } else {
            self.set_line_clip(None);
            self.selected_text()
        }
    }

    /// Copies like [`Buffer::copy`] and removes what was copied (whole lines
    /// for empty selections).
    pub fn cut(&mut self) -> (String, Transaction) {
        let text = self.copy();
        let tx = if self.all_empty() {
            self.delete_line()
        } else {
            let ranges = self.sels.iter().map(|s| s.range()).collect();
            self.delete_ranges(ranges, EditKind::Other)
        };
        (text, tx)
    }

    /// Pastes `text` at every caret. A clip that came from copying an empty
    /// selection goes above the caret's line; N lines onto N carets are
    /// distributed one per caret. Line endings follow the document.
    pub fn paste(&mut self, text: &str) -> Transaction {
        if text.is_empty() {
            return Transaction::default();
        }
        let doc_eol = self.eol_detected();
        let norm = |t: &str| normalize_eol(t, doc_eol);
        let is_line_clip = self.line_clip().as_deref() == Some(text) && self.all_empty();
        if is_line_clip {
            return self.paste_lines_above(&norm(text));
        }
        let body = norm(text);
        let n = self.sels.len();
        let parts: Vec<&str> = {
            let trimmed = body
                .strip_suffix("\r\n")
                .or_else(|| body.strip_suffix('\n'))
                .unwrap_or(&body);
            let all: Vec<&str> = split_lines(&body);
            let trimmed_parts: Vec<&str> = split_lines(trimmed);
            if n > 1 && all.len() == n {
                all
            } else if n > 1 && trimmed_parts.len() == n {
                trimmed_parts
            } else {
                Vec::new()
            }
        };
        let mut changes = Vec::with_capacity(n);
        let mut specs = Vec::with_capacity(n);
        for (i, s) in self.sels.iter().enumerate() {
            let r = s.range();
            let piece: &str = parts.get(i).copied().unwrap_or(&body);
            changes.push(Change::replace(r.start, r.end, piece));
            specs.push(NewSel {
                id: Some(s.id),
                anchor: Pos::In(i, piece.len()),
                head: Pos::In(i, piece.len()),
                goal: None,
            });
        }
        self.apply_plan(Plan {
            changes,
            sels: specs,
            kind: EditKind::Other,
        })
    }

    fn paste_lines_above(&mut self, body: &str) -> Transaction {
        let eol = self.eol().as_str();
        let rope = &self.text;
        let mut rows: Vec<u32> = Vec::new();
        for s in &self.sels {
            let row = row_of(rope, s.head);
            if rows.last() != Some(&row) {
                rows.push(row);
            }
        }
        let ensure_eol = |t: &str| -> String {
            if t.ends_with('\n') {
                t.to_string()
            } else {
                format!("{t}{eol}")
            }
        };
        let pieces: Vec<String> = {
            let lines: Vec<&str> = body.split_inclusive('\n').collect();
            if lines.len() == rows.len() && rows.len() > 1 {
                lines.iter().map(|l| ensure_eol(l)).collect()
            } else {
                vec![ensure_eol(body); rows.len()]
            }
        };
        let changes: Vec<Change> = rows
            .iter()
            .zip(&pieces)
            .map(|(&row, p)| Change::insert(row_start(rope, row), p.clone()))
            .collect();
        let sels = self.identity_sels_after();
        self.apply_plan(Plan {
            changes,
            sels,
            kind: EditKind::Other,
        })
    }

    /// Carets keep their text: insertions at their own offset push them along.
    fn identity_sels_after(&self) -> Vec<NewSel> {
        self.sels
            .iter()
            .map(|s| NewSel {
                id: Some(s.id),
                anchor: Pos::Old(s.anchor, Side::After),
                head: Pos::Old(s.head, Side::After),
                goal: s.goal,
            })
            .collect()
    }
}

fn split_lines(s: &str) -> Vec<&str> {
    s.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// Converts the line breaks of `text` to the document's (when it has one).
fn normalize_eol(text: &str, doc: Option<Eol>) -> String {
    let Some(doc) = doc else {
        return text.to_string();
    };
    if !text.contains(['\r', '\n']) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push_str(doc.as_str());
            }
            '\n' => out.push_str(doc.as_str()),
            c => out.push(c),
        }
    }
    out
}
