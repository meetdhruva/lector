//! Character-level editing: typing, newline, deletion.

use std::ops::Range;

use crate::buffer::Buffer;
use crate::engine::{Change, EditKind, NewSel, Plan, Pos, Side};
use crate::text_util::{
    char_at, char_before, leading_ws_len, matching_opener, next_grapheme, prev_grapheme, row_of,
    row_start, visual_col, word_left, word_right,
};
use crate::{IndentStyle, Transaction};

fn closer_for(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '`')
}

fn opener_for(close: char) -> Option<char> {
    match close {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

fn is_closer(c: char) -> bool {
    matches!(c, ')' | ']' | '}')
}

/// Whether a line ending in `c` is followed by a deeper indented one.
fn opens_block(c: char, colon: bool) -> bool {
    matches!(c, '{' | '(' | '[') || (colon && c == ':')
}

/// Characters after which an opener is auto-closed.
fn closes_before(next: Option<char>) -> bool {
    match next {
        None => true,
        Some(c) => c.is_whitespace() || ")]};:.,=>".contains(c),
    }
}

impl Buffer {
    /// The text of one indentation level.
    pub(crate) fn indent_unit(&self) -> String {
        match self.options.indent {
            IndentStyle::Tabs => "\t".to_string(),
            IndentStyle::Spaces(n) => " ".repeat(n.max(1) as usize),
        }
    }

    /// Width of one indentation level in columns.
    pub(crate) fn indent_width(&self) -> u32 {
        match self.options.indent {
            IndentStyle::Tabs => self.options.tab_size.max(1),
            IndentStyle::Spaces(n) => n.max(1),
        }
    }

    fn caret_spec(id: u64, pos: Pos) -> NewSel {
        NewSel {
            id: Some(id),
            anchor: pos,
            head: pos,
            goal: None,
        }
    }

    /// Typed text at every caret, replacing selections. Single typed
    /// characters get bracket/quote handling: surrounding a selection,
    /// auto-closing, and typing over a closer.
    pub fn insert(&mut self, text: &str) -> Transaction {
        if text.is_empty() {
            return Transaction::default();
        }
        if text == "\n" || text == "\r\n" {
            return self.newline();
        }
        let mut chars = text.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        };
        let auto = self.options.auto_close_brackets;
        let kind = if text.contains('\n') {
            EditKind::Other
        } else {
            EditKind::Typing
        };
        let rope = &self.text;
        // Rows touched by more than one selection: electric outdent rewrites the
        // whole indentation of a row, which would overlap the other carets' edits.
        let mut shared_rows = std::collections::HashSet::new();
        if single.is_some_and(is_closer) && self.options.auto_indent && self.sels.len() > 1 {
            let mut seen = std::collections::HashSet::new();
            for s in &self.sels {
                let r = s.range();
                let (first, last) = (row_of(rope, r.start), row_of(rope, r.end));
                for row in first..=last {
                    if !seen.insert(row) {
                        shared_rows.insert(row);
                    }
                }
            }
        }
        let mut changes: Vec<Change> = Vec::with_capacity(self.sels.len());
        let mut specs: Vec<NewSel> = Vec::with_capacity(self.sels.len());
        let mut closers: Vec<(Pos, char)> = Vec::new();
        for s in &self.sels {
            let r = s.range();
            let idx = changes.len();
            let mut handled = false;
            let special =
                single.filter(|c| is_quote(*c) || is_closer(*c) || closer_for(*c).is_some());
            if let (Some(ch), true) = (special, auto) {
                let next = char_at(rope, r.end);
                let prev = char_before(rope, r.start);
                if !r.is_empty() {
                    let closer = closer_for(ch).or(is_quote(ch).then_some(ch));
                    if let Some(closer) = closer {
                        let inner = rope.text_for_range(r.clone());
                        let wrapped = format!("{ch}{inner}{closer}");
                        let (a, h) = (1, 1 + inner.len());
                        let (a, h) = if s.reversed() { (h, a) } else { (a, h) };
                        specs.push(NewSel {
                            id: Some(s.id),
                            anchor: Pos::In(idx, a),
                            head: Pos::In(idx, h),
                            goal: None,
                        });
                        changes.push(Change::replace(r.start, r.end, wrapped));
                        handled = true;
                    }
                } else if (is_closer(ch) || is_quote(ch))
                    && next == Some(ch)
                    && self.has_auto_closer(r.end, ch)
                {
                    let to = r.end + ch.len_utf8();
                    specs.push(Self::caret_spec(s.id, Pos::Old(to, Side::Before)));
                    handled = true;
                } else if let Some(closer) = closer_for(ch) {
                    if closes_before(next) {
                        changes.push(Change::insert(r.start, format!("{ch}{closer}")));
                        specs.push(Self::caret_spec(s.id, Pos::In(idx, ch.len_utf8())));
                        closers.push((Pos::In(idx, ch.len_utf8()), closer));
                        handled = true;
                    }
                } else if is_quote(ch) {
                    let word_before =
                        prev.is_some_and(|p| p.is_alphanumeric() || p == '_' || p == '\\');
                    if !word_before && closes_before(next) && prev != Some(ch) && {
                        let row = row_of(rope, r.start);
                        let before = rope.text_for_range(row_start(rope, row)..r.start);
                        before.matches(ch).count() % 2 == 0
                    } {
                        changes.push(Change::insert(r.start, format!("{ch}{ch}")));
                        specs.push(Self::caret_spec(s.id, Pos::In(idx, ch.len_utf8())));
                        closers.push((Pos::In(idx, ch.len_utf8()), ch));
                        handled = true;
                    }
                }
            }
            if !handled
                && r.is_empty()
                && self.options.auto_indent
                && !shared_rows.contains(&row_of(rope, r.start))
            {
                if let Some((line_start, indent)) = single
                    .filter(|c| is_closer(*c))
                    .and_then(|c| self.electric_outdent(r.start, c))
                {
                    let typed = format!("{indent}{text}");
                    let caret = typed.len();
                    changes.push(Change::replace(line_start, r.start, typed));
                    specs.push(Self::caret_spec(s.id, Pos::In(idx, caret)));
                    handled = true;
                }
            }
            if !handled {
                changes.push(Change::replace(r.start, r.end, text));
                specs.push(Self::caret_spec(s.id, Pos::In(idx, text.len())));
            }
        }
        self.apply_plan_closing(
            Plan {
                changes,
                sels: specs,
                kind,
            },
            closers,
        )
    }

    /// Typing the closer `close` at the end of a line that holds only
    /// whitespace: the line start and the indentation the line should get so
    /// the closer lines up with the line of its opener (one indent unit less
    /// than now when no opener is found). `None` when nothing changes.
    fn electric_outdent(&self, caret: usize, close: char) -> Option<(usize, String)> {
        let rope = &self.text;
        let row = row_of(rope, caret);
        let line_range = rope.line_range(row);
        if caret != line_range.end {
            return None;
        }
        let current = rope.text_for_range(line_range.clone());
        if !current.chars().all(|c| c == ' ' || c == '\t') {
            return None;
        }
        let target = match opener_for(close)
            .and_then(|open| matching_opener(rope, line_range.start, open, close))
        {
            Some(opener) => {
                let opener_line = rope.text_for_range(rope.line_range(row_of(rope, opener)));
                opener_line[..leading_ws_len(&opener_line)].to_string()
            }
            None => {
                let width = self.indent_width() as usize;
                let drop = if current.ends_with('\t') {
                    1
                } else {
                    current
                        .chars()
                        .rev()
                        .take_while(|c| *c == ' ')
                        .count()
                        .min(width)
                };
                current[..current.len() - drop].to_string()
            }
        };
        (target != current).then_some((line_range.start, target))
    }

    /// Newline with auto-indent at every caret.
    pub fn newline(&mut self) -> Transaction {
        let eol = self.eol().as_str();
        let unit = self.indent_unit();
        let auto = self.options.auto_indent;
        let colon = self.options.indent_after_colon;
        let rope = &self.text;
        let mut changes = Vec::with_capacity(self.sels.len());
        let mut specs = Vec::with_capacity(self.sels.len());
        for s in &self.sels {
            let r = s.range();
            let idx = changes.len();
            let (text, caret) = if auto {
                let row = row_of(rope, r.start);
                let ls = row_start(rope, row);
                let before = rope.text_for_range(ls..r.start);
                let indent = &before[..leading_ws_len(&before)];
                let last = before.trim_end().chars().next_back();
                let prev = char_before(rope, r.start);
                let next = char_at(rope, r.end);
                let between =
                    matches!((prev, next), (Some(p), Some(n)) if closer_for(p) == Some(n));
                if between {
                    let first = format!("{eol}{indent}{unit}");
                    let caret = first.len();
                    (format!("{first}{eol}{indent}"), caret)
                } else if last.is_some_and(|c| opens_block(c, colon)) {
                    let t = format!("{eol}{indent}{unit}");
                    let c = t.len();
                    (t, c)
                } else {
                    let t = format!("{eol}{indent}");
                    let c = t.len();
                    (t, c)
                }
            } else {
                (eol.to_string(), eol.len())
            };
            changes.push(Change::replace(r.start, r.end, text));
            specs.push(Self::caret_spec(s.id, Pos::In(idx, caret)));
        }
        self.apply_plan(Plan {
            changes,
            sels: specs,
            kind: EditKind::Other,
        })
    }

    /// Opens a new line below each caret's line and moves the caret there.
    pub fn newline_below(&mut self) -> Transaction {
        self.open_line(false)
    }

    /// Opens a new line above each caret's line and moves the caret there.
    pub fn newline_above(&mut self) -> Transaction {
        self.open_line(true)
    }

    fn open_line(&mut self, above: bool) -> Transaction {
        let eol = self.eol().as_str();
        let unit = self.indent_unit();
        let auto = self.options.auto_indent;
        let colon = self.options.indent_after_colon;
        let rope = &self.text;
        let mut rows: Vec<u32> = Vec::new();
        let mut owners: Vec<Vec<u64>> = Vec::new();
        for s in &self.sels {
            let row = row_of(rope, s.head);
            if rows.last() == Some(&row) {
                if let Some(o) = owners.last_mut() {
                    o.push(s.id);
                }
            } else {
                rows.push(row);
                owners.push(vec![s.id]);
            }
        }
        let mut changes = Vec::new();
        let mut specs = Vec::new();
        for (row, ids) in rows.iter().zip(&owners) {
            let lr = rope.line_range(*row);
            let line = rope.text_for_range(lr.clone());
            let indent_len = leading_ws_len(&line);
            let mut indent = if auto {
                line[..indent_len].to_string()
            } else {
                String::new()
            };
            let idx = changes.len();
            if above {
                let caret = indent.len();
                changes.push(Change::insert(lr.start, format!("{indent}{eol}")));
                for id in ids {
                    specs.push(Self::caret_spec(*id, Pos::In(idx, caret)));
                }
            } else {
                if auto
                    && line
                        .trim_end()
                        .chars()
                        .next_back()
                        .is_some_and(|c| opens_block(c, colon))
                {
                    indent.push_str(&unit);
                }
                let t = format!("{eol}{indent}");
                let caret = t.len();
                changes.push(Change::insert(lr.end, t));
                for id in ids {
                    specs.push(Self::caret_spec(*id, Pos::In(idx, caret)));
                }
            }
        }
        self.apply_plan(Plan {
            changes,
            sels: specs,
            kind: EditKind::Other,
        })
    }

    /// Deletes one range per selection (merging overlaps); every selection
    /// collapses to the start of its deleted range. Empty ranges delete nothing.
    pub(crate) fn delete_ranges(
        &mut self,
        ranges: Vec<Range<usize>>,
        kind: EditKind,
    ) -> Transaction {
        let mut merged: Vec<Range<usize>> =
            ranges.iter().filter(|r| !r.is_empty()).cloned().collect();
        merged.sort_by_key(|r| (r.start, r.end));
        let mut out: Vec<Range<usize>> = Vec::with_capacity(merged.len());
        for r in merged {
            match out.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => out.push(r),
            }
        }
        let specs: Vec<NewSel> = self
            .sels
            .iter()
            .zip(&ranges)
            .map(|(s, r)| {
                let pos = if r.is_empty() {
                    Pos::Old(r.start, Side::After)
                } else {
                    let idx = out.partition_point(|m| m.end <= r.start);
                    Pos::In(idx, 0)
                };
                Self::caret_spec(s.id, pos)
            })
            .collect();
        let changes = out
            .into_iter()
            .map(|r| Change::delete(r.start, r.end))
            .collect();
        self.apply_plan(Plan {
            changes,
            sels: specs,
            kind,
        })
    }

    /// Deletes the selections, or the grapheme before each caret. In leading
    /// whitespace it removes one indent unit; between `()` `[]` `{}` or a
    /// pair of quotes it removes both.
    pub fn backspace(&mut self) -> Transaction {
        let rope = &self.text;
        let tab = self.options.tab_size;
        let width = self.indent_width();
        let spaces = matches!(self.options.indent, IndentStyle::Spaces(_));
        let pairs = self.options.auto_close_brackets;
        let mut ranges = Vec::with_capacity(self.sels.len());
        for s in &self.sels {
            let r = s.range();
            if !r.is_empty() {
                ranges.push(r);
                continue;
            }
            let off = r.start;
            if off == 0 {
                ranges.push(off..off);
                continue;
            }
            let prev = char_before(rope, off);
            let next = char_at(rope, off);
            if pairs {
                if let (Some(p), Some(n)) = (prev, next) {
                    if closer_for(p) == Some(n) || (is_quote(p) && p == n) {
                        ranges.push(off - p.len_utf8()..off + n.len_utf8());
                        continue;
                    }
                }
            }
            let row = row_of(rope, off);
            let ls = row_start(rope, row);
            let before = rope.text_for_range(ls..off);
            if spaces && prev == Some(' ') && before.chars().all(|c| c == ' ' || c == '\t') {
                let vcol = visual_col(&before, tab);
                let want = if vcol % width == 0 {
                    width
                } else {
                    vcol % width
                } as usize;
                let run = before.chars().rev().take_while(|c| *c == ' ').count();
                let n = want.min(run);
                ranges.push(off - n..off);
                continue;
            }
            ranges.push(prev_grapheme(rope, off)..off);
        }
        self.delete_ranges(ranges, EditKind::Backspace)
    }

    /// Deletes the selections, or the grapheme after each caret.
    pub fn delete(&mut self) -> Transaction {
        let rope = &self.text;
        let ranges = self
            .sels
            .iter()
            .map(|s| {
                let r = s.range();
                if r.is_empty() {
                    r.start..next_grapheme(rope, r.start)
                } else {
                    r
                }
            })
            .collect();
        self.delete_ranges(ranges, EditKind::DeleteForward)
    }

    /// Deletes the selections, or back to the previous word start.
    pub fn delete_word_left(&mut self) -> Transaction {
        let rope = &self.text;
        let ranges = self
            .sels
            .iter()
            .map(|s| {
                let r = s.range();
                if r.is_empty() {
                    word_left(rope, r.start)..r.start
                } else {
                    r
                }
            })
            .collect();
        self.delete_ranges(ranges, EditKind::Other)
    }

    /// Deletes the selections, or forward to the end of the next word.
    pub fn delete_word_right(&mut self) -> Transaction {
        let rope = &self.text;
        let ranges = self
            .sels
            .iter()
            .map(|s| {
                let r = s.range();
                if r.is_empty() {
                    r.start..word_right(rope, r.start)
                } else {
                    r
                }
            })
            .collect();
        self.delete_ranges(ranges, EditKind::Other)
    }

    /// Deletes the selections, or from each caret back to the start of its
    /// line (at column 0: the preceding line break).
    pub fn delete_to_line_start(&mut self) -> Transaction {
        let rope = &self.text;
        let ranges = self
            .sels
            .iter()
            .map(|s| {
                let r = s.range();
                if !r.is_empty() {
                    return r;
                }
                let ls = row_start(rope, row_of(rope, r.start));
                if ls < r.start {
                    ls..r.start
                } else {
                    prev_grapheme(rope, r.start)..r.start
                }
            })
            .collect();
        self.delete_ranges(ranges, EditKind::Other)
    }
}
