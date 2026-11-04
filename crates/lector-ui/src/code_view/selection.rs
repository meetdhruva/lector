//! Cursor and selection model plus movement logic (GPUI-independent).

use std::cmp::Ordering;

use unicode_segmentation::UnicodeSegmentation;

use super::display_map::{
    byte_for_display_column, display_column, wrap_indent, wrap_segments, DisplayMap,
};

/// A position in the buffer: row and byte column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Pos {
    /// 0-based row.
    pub row: u32,
    /// Byte offset within the row.
    pub col: usize,
}

impl Pos {
    /// New position.
    pub const fn new(row: u32, col: usize) -> Self {
        Pos { row, col }
    }
}

impl PartialOrd for Pos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pos {
    fn cmp(&self, other: &Self) -> Ordering {
        self.row.cmp(&other.row).then(self.col.cmp(&other.col))
    }
}

/// A selection: `anchor` stays put, `head` moves (the cursor).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Fixed end.
    pub anchor: Pos,
    /// Moving end (cursor).
    pub head: Pos,
    /// Remembered display column for vertical movement.
    pub goal_column: Option<usize>,
}

impl Selection {
    /// Collapsed selection at `pos`.
    pub fn cursor(pos: Pos) -> Self {
        Selection {
            anchor: pos,
            head: pos,
            goal_column: None,
        }
    }

    /// Whether anchor == head.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Ordered `(start, end)`.
    pub fn range(&self) -> (Pos, Pos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// Sorts selections by start and merges overlapping ones (carets at the same
/// position, or ranges that overlap; touching non-empty ranges stay apart).
/// Returns the merged list and the new index of `primary`.
pub fn normalize(mut selections: Vec<Selection>, primary: usize) -> (Vec<Selection>, usize) {
    if selections.is_empty() {
        return (vec![Selection::default()], 0);
    }
    let mut indexed: Vec<(usize, Selection)> = selections.drain(..).enumerate().collect();
    indexed.sort_by_key(|(_, s)| s.range());
    let mut out: Vec<Selection> = Vec::with_capacity(indexed.len());
    let mut primary_out = 0;
    let primary_ix = primary.min(indexed.len() - 1);
    for (ix, sel) in indexed {
        if let Some(last) = out.last_mut() {
            let (ls, le) = last.range();
            let (s, e) = sel.range();
            // Overlapping ranges, or a caret touching a range edge / another caret.
            let overlaps = s < le || (s == le && (s == e || ls == le));
            if overlaps {
                let start = ls.min(s);
                let end = le.max(e);
                let reversed = last.head < last.anchor;
                *last = if reversed {
                    Selection {
                        anchor: end,
                        head: start,
                        goal_column: None,
                    }
                } else {
                    Selection {
                        anchor: start,
                        head: end,
                        goal_column: last.goal_column,
                    }
                };
                if ix == primary_ix {
                    primary_out = out.len() - 1;
                }
                continue;
            }
        }
        if ix == primary_ix {
            primary_out = out.len();
        }
        out.push(sel);
    }
    (out, primary_out)
}

/// Read access to lines for movement.
pub trait LineAccess {
    /// Number of rows.
    fn line_count(&self) -> u32;
    /// Row text.
    fn line_text(&self, row: u32) -> gpui::SharedString;
}

impl<T: super::CodeSource + ?Sized> LineAccess for T {
    fn line_count(&self) -> u32 {
        super::CodeSource::line_count(self)
    }
    fn line_text(&self, row: u32) -> gpui::SharedString {
        self.line(row)
    }
}

/// Clamps `pos` to the text (row in range, column on a char boundary).
pub fn clip(text: &(impl LineAccess + ?Sized), pos: Pos) -> Pos {
    let last = text.line_count().saturating_sub(1);
    let row = pos.row.min(last);
    let line = text.line_text(row);
    let mut col = pos.col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    Pos::new(row, col)
}

/// One grapheme left (wrapping to the previous line end).
pub fn left(text: &(impl LineAccess + ?Sized), pos: Pos) -> Pos {
    if pos.col == 0 {
        if pos.row == 0 {
            return pos;
        }
        let row = pos.row - 1;
        return Pos::new(row, text.line_text(row).len());
    }
    let line = text.line_text(pos.row);
    let col = line[..pos.col.min(line.len())]
        .grapheme_indices(true)
        .next_back()
        .map(|(ix, _)| ix)
        .unwrap_or(0);
    Pos::new(pos.row, col)
}

/// One grapheme right (wrapping to the next line start).
pub fn right(text: &(impl LineAccess + ?Sized), pos: Pos) -> Pos {
    let line = text.line_text(pos.row);
    if pos.col >= line.len() {
        if pos.row + 1 >= text.line_count() {
            return Pos::new(pos.row, line.len());
        }
        return Pos::new(pos.row + 1, 0);
    }
    let col = line[pos.col..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(ix, _)| pos.col + ix)
        .unwrap_or(line.len());
    Pos::new(pos.row, col)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Start of the previous word.
pub fn word_left(text: &(impl LineAccess + ?Sized), pos: Pos) -> Pos {
    if pos.col == 0 {
        return left(text, pos);
    }
    let line = text.line_text(pos.row);
    let before: Vec<(usize, char)> = line[..pos.col.min(line.len())].char_indices().collect();
    let mut i = before.len();
    while i > 0 && class(before[i - 1].1) == CharClass::Space {
        i -= 1;
    }
    if i == 0 {
        return Pos::new(pos.row, 0);
    }
    let cls = class(before[i - 1].1);
    while i > 0 && class(before[i - 1].1) == cls {
        i -= 1;
    }
    Pos::new(pos.row, before.get(i).map(|(ix, _)| *ix).unwrap_or(0))
}

/// End of the next word.
pub fn word_right(text: &(impl LineAccess + ?Sized), pos: Pos) -> Pos {
    let line = text.line_text(pos.row);
    if pos.col >= line.len() {
        return right(text, pos);
    }
    let after: Vec<(usize, char)> = line[pos.col..].char_indices().collect();
    let mut i = 0;
    while i < after.len() && class(after[i].1) == CharClass::Space {
        i += 1;
    }
    if i < after.len() {
        let cls = class(after[i].1);
        while i < after.len() && class(after[i].1) == cls {
            i += 1;
        }
    }
    Pos::new(
        pos.row,
        after
            .get(i)
            .map(|(ix, _)| pos.col + ix)
            .unwrap_or(line.len()),
    )
}

/// The word (or run of punctuation / whitespace) containing `pos`.
pub fn word_range(text: &(impl LineAccess + ?Sized), pos: Pos) -> (Pos, Pos) {
    let line = text.line_text(pos.row);
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if chars.is_empty() {
        return (Pos::new(pos.row, 0), Pos::new(pos.row, 0));
    }
    let mut ix = chars
        .partition_point(|(b, _)| *b < pos.col)
        .min(chars.len() - 1);
    // Prefer the word to the left when the cursor sits right after it.
    if ix > 0
        && (pos.col >= line.len() || class(chars[ix].1) != CharClass::Word)
        && class(chars[ix - 1].1) == CharClass::Word
    {
        ix -= 1;
    }
    let cls = class(chars[ix].1);
    let mut start = ix;
    while start > 0 && class(chars[start - 1].1) == cls {
        start -= 1;
    }
    let mut end = ix + 1;
    while end < chars.len() && class(chars[end].1) == cls {
        end += 1;
    }
    let end_byte = chars.get(end).map(|(b, _)| *b).unwrap_or(line.len());
    (
        Pos::new(pos.row, chars[start].0),
        Pos::new(pos.row, end_byte),
    )
}

/// The identifier at `pos`, if any (for word highlights / go to definition).
pub fn word_at(text: &(impl LineAccess + ?Sized), pos: Pos) -> Option<(Pos, Pos, String)> {
    let (s, e) = word_range(text, pos);
    let line = text.line_text(pos.row);
    let word = line.get(s.col..e.col)?;
    word.chars()
        .next()
        .filter(|c| c.is_alphanumeric() || *c == '_')?;
    Some((s, e, word.to_string()))
}

/// Segment of `pos` within its wrapped row: `(segment index, segment start byte)`.
fn segment_of(line: &str, col: usize, wrap: Option<usize>, tab: usize) -> (usize, usize) {
    match wrap {
        None => (0, 0),
        Some(cols) => {
            let segs = wrap_segments(line, cols, tab);
            let ix = segs.partition_point(|&s| s <= col).saturating_sub(1);
            (ix, segs[ix])
        }
    }
}

/// Moves `pos` by `delta` display rows keeping `goal` display column
/// (relative to the wrap segment). Returns the new position and goal.
pub fn vertical(
    text: &(impl LineAccess + ?Sized),
    map: &DisplayMap,
    pos: Pos,
    goal: Option<usize>,
    delta: i64,
    tab: usize,
) -> (Pos, usize) {
    let wrap = map.wrap_cols();
    let line = text.line_text(pos.row);
    let (seg_ix, seg_start) = segment_of(&line, pos.col, wrap, tab);
    // Goal is a visual column: continuation rows are shifted by their indent.
    let offset = |line: &str, seg_ix: usize| match wrap {
        Some(cols) if seg_ix > 0 => wrap_indent(line, cols, tab),
        _ => 0,
    };
    let goal = goal.unwrap_or_else(|| {
        display_column(&line, pos.col, tab) - display_column(&line, seg_start, tab)
            + offset(&line, seg_ix)
    });
    let display_row = map.row_to_display(pos.row) as i64 + seg_ix as i64;
    let target = display_row + delta;
    let count = map.display_row_count() as i64;
    if target < 0 {
        return (Pos::new(0, 0), goal);
    }
    if target >= count {
        let last = text.line_count().saturating_sub(1);
        return (Pos::new(last, text.line_text(last).len()), goal);
    }
    let (row, seg) = map.display_to_row(target as u32);
    let line = text.line_text(row);
    let (seg_start, seg_end) = match wrap {
        None => (0, line.len()),
        Some(cols) => {
            let segs = wrap_segments(&line, cols, tab);
            let start = segs.get(seg).copied().unwrap_or(0);
            let end = segs.get(seg + 1).copied().unwrap_or(line.len());
            (start, end)
        }
    };
    let base = display_column(&line, seg_start, tab);
    let visual = goal.saturating_sub(offset(&line, seg));
    let mut col = byte_for_display_column(&line, base + visual, tab).max(seg_start);
    if seg_end < line.len() && col >= seg_end {
        // Stay on this wrapped display row rather than its continuation.
        col = line[..seg_end]
            .char_indices()
            .next_back()
            .map(|(ix, _)| ix)
            .unwrap_or(seg_start)
            .max(seg_start);
    } else {
        col = col.min(seg_end);
    }
    (Pos::new(row, col), goal)
}

/// Text covered by a selection, joining rows with `\n`.
pub fn selected_text(text: &(impl LineAccess + ?Sized), sel: &Selection) -> String {
    let (start, end) = sel.range();
    let mut out = String::new();
    for row in start.row..=end.row.min(text.line_count().saturating_sub(1)) {
        let line = text.line_text(row);
        let from = if row == start.row {
            start.col.min(line.len())
        } else {
            0
        };
        let to = if row == end.row {
            end.col.min(line.len())
        } else {
            line.len()
        };
        if from <= to {
            out.push_str(&line[from..to]);
        }
        if row != end.row {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_view::InMemorySource;

    fn src(text: &str) -> InMemorySource {
        InMemorySource::new(text)
    }

    #[test]
    fn left_right_wrap_lines() {
        let s = src("ab\nc");
        assert_eq!(right(&s, Pos::new(0, 2)), Pos::new(1, 0));
        assert_eq!(left(&s, Pos::new(1, 0)), Pos::new(0, 2));
        assert_eq!(left(&s, Pos::new(0, 0)), Pos::new(0, 0));
        assert_eq!(right(&s, Pos::new(1, 1)), Pos::new(1, 1));
        let s = src("é😀x");
        assert_eq!(right(&s, Pos::new(0, 0)), Pos::new(0, 2));
        assert_eq!(right(&s, Pos::new(0, 2)), Pos::new(0, 6));
        assert_eq!(left(&s, Pos::new(0, 6)), Pos::new(0, 2));
    }

    #[test]
    fn words() {
        let s = src("let foo_bar = baz(1);");
        assert_eq!(word_right(&s, Pos::new(0, 0)), Pos::new(0, 3));
        assert_eq!(word_right(&s, Pos::new(0, 3)), Pos::new(0, 11));
        assert_eq!(word_left(&s, Pos::new(0, 11)), Pos::new(0, 4));
        assert_eq!(
            word_range(&s, Pos::new(0, 6)),
            (Pos::new(0, 4), Pos::new(0, 11))
        );
        assert_eq!(
            word_at(&s, Pos::new(0, 16)).map(|w| w.2),
            Some("baz".to_string())
        );
        assert_eq!(word_at(&s, Pos::new(0, 12)), None);
    }

    #[test]
    fn vertical_keeps_goal_column() {
        let s = src("abcdef\nx\nabcdef");
        let map = DisplayMap::new(3);
        let (p, goal) = vertical(&s, &map, Pos::new(0, 4), None, 1, 4);
        assert_eq!(p, Pos::new(1, 1));
        let (p, _) = vertical(&s, &map, p, Some(goal), 1, 4);
        assert_eq!(p, Pos::new(2, 4));
        let (p, _) = vertical(&s, &map, Pos::new(0, 2), None, -1, 4);
        assert_eq!(p, Pos::new(0, 0));
        let (p, _) = vertical(&s, &map, Pos::new(2, 2), None, 5, 4);
        assert_eq!(p, Pos::new(2, 6));
    }

    #[test]
    fn vertical_skips_folds_and_walks_wraps() {
        let s = src("a\nb\nc\nd");
        let mut map = DisplayMap::new(4);
        map.set_folds(std::iter::once(0..3));
        let (p, _) = vertical(&s, &map, Pos::new(0, 0), None, 1, 4);
        assert_eq!(p.row, 3);

        let long = "0123456789abcdefghij";
        let s = src(long);
        let mut map = DisplayMap::new(1);
        map.set_wrap(Some(10), 4, |_| long.to_string());
        let (p, _) = vertical(&s, &map, Pos::new(0, 3), None, 1, 4);
        assert_eq!(p, Pos::new(0, 13));
    }

    #[test]
    fn selected_text_spans_rows() {
        let s = src("hello\nworld\n!");
        let sel = Selection {
            anchor: Pos::new(1, 2),
            head: Pos::new(0, 3),
            goal_column: None,
        };
        assert_eq!(selected_text(&s, &sel), "lo\nwo");
        let all = Selection {
            anchor: Pos::new(0, 0),
            head: Pos::new(2, 1),
            goal_column: None,
        };
        assert_eq!(selected_text(&s, &all), "hello\nworld\n!");
    }
}
