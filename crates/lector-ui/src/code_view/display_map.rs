//! Mapping between buffer rows and display rows (folds + soft wrap), and
//! tab expansion. Pure logic, independent of GPUI.

use std::ops::Range;

use smallvec::SmallVec;
use unicode_width::UnicodeWidthChar;

/// Display width of a char at display column `col`.
pub fn char_width(c: char, col: usize, tab_size: usize) -> usize {
    match c {
        '\t' => tab_size - col % tab_size,
        c => c.width().unwrap_or(0),
    }
}

/// Display column (tab-expanded width) of byte offset `byte` in `line`.
pub fn display_column(line: &str, byte: usize, tab_size: usize) -> usize {
    let mut col = 0;
    for (ix, c) in line.char_indices() {
        if ix >= byte {
            break;
        }
        col += char_width(c, col, tab_size);
    }
    col
}

/// Byte offset in `line` closest to display column `target` (never inside a char).
pub fn byte_for_display_column(line: &str, target: usize, tab_size: usize) -> usize {
    let mut col = 0;
    for (ix, c) in line.char_indices() {
        let w = char_width(c, col, tab_size);
        if col + w > target {
            // Round to the nearer edge.
            return if target - col > w / 2 && w > 1 {
                ix + c.len_utf8()
            } else {
                ix
            };
        }
        col += w;
    }
    line.len()
}

/// A line with tabs expanded to spaces, plus the mapping back to source bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandedLine {
    /// Display text (no tabs).
    pub text: String,
    /// `(source_byte_of_tab, display_byte_after_tab)` for each tab, sorted.
    tabs: Vec<(usize, usize, usize)>,
}

impl ExpandedLine {
    /// Expands tabs in `line`.
    pub fn new(line: &str, tab_size: usize) -> Self {
        if !line.contains('\t') {
            return ExpandedLine {
                text: line.to_string(),
                tabs: Vec::new(),
            };
        }
        let mut text = String::with_capacity(line.len() + 8);
        let mut tabs = Vec::new();
        let mut col = 0;
        for (ix, c) in line.char_indices() {
            if c == '\t' {
                let w = tab_size - col % tab_size;
                let disp_start = text.len();
                text.extend(std::iter::repeat_n(' ', w));
                tabs.push((ix, disp_start, w));
                col += w;
            } else {
                text.push(c);
                col += char_width(c, col, tab_size);
            }
        }
        ExpandedLine { text, tabs }
    }

    /// Display byte of source byte `src`.
    pub fn to_display(&self, src: usize) -> usize {
        let mut delta = 0usize;
        for &(tab_src, _, w) in &self.tabs {
            if tab_src < src {
                delta += w - 1;
            } else {
                break;
            }
        }
        src + delta
    }

    /// Source byte of display byte `disp` (positions inside an expanded tab
    /// snap to the nearer side).
    pub fn to_source(&self, disp: usize) -> usize {
        let mut delta = 0usize;
        for &(tab_src, tab_disp, w) in &self.tabs {
            if disp >= tab_disp + w {
                delta += w - 1;
            } else if disp > tab_disp {
                let inside = disp - tab_disp;
                return if inside * 2 >= w {
                    tab_src + 1
                } else {
                    tab_src
                };
            } else {
                break;
            }
        }
        disp.saturating_sub(delta)
    }
}

/// Indentation (display columns) of continuation rows when `line` wraps
/// at `cols`: the line's own indentation, capped at half the width.
pub fn wrap_indent(line: &str, cols: usize, tab_size: usize) -> usize {
    let mut width = 0;
    for c in line.chars() {
        match c {
            ' ' => width += 1,
            '\t' => width += tab_size - width % tab_size,
            _ => break,
        }
    }
    width.min(cols.max(8) / 2)
}

/// Start byte offsets of soft-wrapped segments of `line` (first is 0),
/// wrapping at `cols` display columns and preferring whitespace breaks.
/// Continuation segments are narrower by [`wrap_indent`] columns.
pub fn wrap_segments(line: &str, cols: usize, tab_size: usize) -> SmallVec<[usize; 4]> {
    let mut starts: SmallVec<[usize; 4]> = SmallVec::new();
    starts.push(0);
    let cols = cols.max(8);
    let indent = wrap_indent(line, cols, tab_size);
    let mut limit = cols;
    let mut col = 0;
    let mut seg_start = 0;
    let mut last_break: Option<(usize, usize)> = None; // (byte after whitespace, col there)
    let mut line_col = 0;
    for (ix, c) in line.char_indices() {
        let w = char_width(c, line_col, tab_size);
        if col + w > limit && ix > seg_start {
            let (brk, brk_col) = match last_break {
                Some((b, bc)) if b > seg_start && bc * 10 >= limit * 6 => (b, bc),
                _ => (ix, col),
            };
            starts.push(brk);
            seg_start = brk;
            col -= brk_col;
            last_break = None;
            limit = cols - indent;
        }
        col += w;
        line_col += w;
        if (c == ' ' || c == '\t') && line_col > indent {
            last_break = Some((ix + c.len_utf8(), col));
        }
    }
    starts
}

/// Folds and soft wrap for one source snapshot.
#[derive(Clone, Debug, Default)]
pub struct DisplayMap {
    line_count: u32,
    /// Hidden row ranges, sorted and disjoint.
    hidden: Vec<Range<u32>>,
    /// `hidden_before[i]`: hidden rows in `hidden[..i]`.
    hidden_before: Vec<u32>,
    wrap: Option<WrapIndex>,
}

#[derive(Clone, Debug)]
struct WrapIndex {
    cols: usize,
    /// `extra_before[v]`: extra display rows from wraps of visible lines `< v`.
    extra_before: Vec<u32>,
}

impl DisplayMap {
    /// Identity map for `line_count` rows.
    pub fn new(line_count: u32) -> Self {
        DisplayMap {
            line_count: line_count.max(1),
            ..Default::default()
        }
    }

    /// Applies folds (each hides `start + 1 .. end`). Overlapping folds merge.
    pub fn set_folds(&mut self, folds: impl IntoIterator<Item = Range<u32>>) {
        let mut hidden: Vec<Range<u32>> = folds
            .into_iter()
            .map(|f| (f.start + 1)..f.end.min(self.line_count))
            .filter(|r| r.start < r.end)
            .collect();
        hidden.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<u32>> = Vec::with_capacity(hidden.len());
        for r in hidden {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        let mut acc = 0;
        self.hidden_before = merged
            .iter()
            .map(|r| {
                let before = acc;
                acc += r.end - r.start;
                before
            })
            .collect();
        self.hidden = merged;
        self.wrap = None;
    }

    /// Enables soft wrap at `cols` columns. `line_text` returns row text.
    pub fn set_wrap(
        &mut self,
        cols: Option<usize>,
        tab_size: usize,
        line_text: impl Fn(u32) -> String,
    ) {
        let Some(cols) = cols else {
            self.wrap = None;
            return;
        };
        let visible = self.visible_count();
        let mut extra_before = Vec::with_capacity(visible as usize + 1);
        let mut acc = 0u32;
        for v in 0..visible {
            extra_before.push(acc);
            let text = line_text(self.visible_to_row(v));
            acc += wrap_segments(&text, cols, tab_size).len() as u32 - 1;
        }
        extra_before.push(acc);
        self.wrap = Some(WrapIndex { cols, extra_before });
    }

    /// Wrap width in columns, if wrapping.
    pub fn wrap_cols(&self) -> Option<usize> {
        self.wrap.as_ref().map(|w| w.cols)
    }

    /// Number of buffer rows.
    pub fn line_count(&self) -> u32 {
        self.line_count
    }

    fn total_hidden(&self) -> u32 {
        self.hidden
            .last()
            .zip(self.hidden_before.last())
            .map(|(r, b)| b + (r.end - r.start))
            .unwrap_or(0)
    }

    /// Number of visible (unfolded) buffer rows.
    pub fn visible_count(&self) -> u32 {
        self.line_count - self.total_hidden()
    }

    /// Total display rows.
    pub fn display_row_count(&self) -> u32 {
        let visible = self.visible_count();
        match &self.wrap {
            Some(w) => visible + w.extra_before.last().copied().unwrap_or(0),
            None => visible,
        }
    }

    /// Whether `row` is hidden by a fold.
    pub fn is_hidden(&self, row: u32) -> bool {
        let ix = self.hidden.partition_point(|r| r.end <= row);
        self.hidden.get(ix).is_some_and(|r| r.start <= row)
    }

    /// Visible index of `row`; hidden rows map to their fold's header row.
    pub fn row_to_visible(&self, row: u32) -> u32 {
        let row = row.min(self.line_count - 1);
        let ix = self.hidden.partition_point(|r| r.end <= row);
        match self.hidden.get(ix) {
            Some(r) if r.start <= row => (r.start - 1) - self.hidden_before[ix],
            _ => {
                let before = if ix == 0 {
                    0
                } else {
                    self.hidden_before[ix - 1]
                        + (self.hidden[ix - 1].end - self.hidden[ix - 1].start)
                };
                row - before
            }
        }
    }

    /// Buffer row of visible index `v`.
    pub fn visible_to_row(&self, v: u32) -> u32 {
        // Number of folds whose hidden range starts at or before visible index v.
        let mut lo = 0;
        let mut hi = self.hidden.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.hidden[mid].start - self.hidden_before[mid] <= v {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let k = lo;
        let hidden = if k == 0 {
            0
        } else {
            self.hidden_before[k - 1] + (self.hidden[k - 1].end - self.hidden[k - 1].start)
        };
        (v + hidden).min(self.line_count - 1)
    }

    /// First display row of buffer `row`.
    pub fn row_to_display(&self, row: u32) -> u32 {
        let v = self.row_to_visible(row);
        match &self.wrap {
            Some(w) => v + w.extra_before[v as usize],
            None => v,
        }
    }

    /// `(buffer row, wrap segment index)` of a display row (clamped).
    pub fn display_to_row(&self, display_row: u32) -> (u32, usize) {
        let d = display_row.min(self.display_row_count().saturating_sub(1));
        match &self.wrap {
            None => (self.visible_to_row(d), 0),
            Some(w) => {
                // Largest v with v + extra_before[v] <= d.
                let visible = self.visible_count() as usize;
                let mut lo = 0usize;
                let mut hi = visible;
                while lo + 1 < hi {
                    let mid = (lo + hi) / 2;
                    if mid as u32 + w.extra_before[mid] <= d {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                let seg = d - (lo as u32 + w.extra_before[lo]);
                (self.visible_to_row(lo as u32), seg as usize)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_expansion_roundtrip() {
        let e = ExpandedLine::new("\ta\tbc", 4);
        assert_eq!(e.text, "    a   bc");
        assert_eq!(e.to_display(0), 0);
        assert_eq!(e.to_display(1), 4);
        assert_eq!(e.to_display(2), 5);
        assert_eq!(e.to_display(3), 8);
        assert_eq!(e.to_source(4), 1);
        assert_eq!(e.to_source(8), 3);
        assert_eq!(e.to_source(9), 4);
        assert_eq!(e.to_source(1), 0);
        assert_eq!(e.to_source(3), 1);
        assert_eq!(display_column("\tab", 2, 4), 5);
        assert_eq!(byte_for_display_column("\tab", 5, 4), 2);
    }

    #[test]
    fn wrapping_prefers_spaces() {
        let segs = wrap_segments("aaaa bbbb cccc dddd", 10, 4);
        assert_eq!(segs.as_slice(), &[0, 10]);
        let segs = wrap_segments(&"x".repeat(25), 10, 4);
        assert_eq!(segs.as_slice(), &[0, 10, 20]);
        // Continuation rows are narrower by the line's indentation.
        let segs = wrap_segments("    abcdefghijklmnop", 10, 4);
        assert_eq!(segs.as_slice(), &[0, 10, 16]);
        assert_eq!(wrap_indent("    x", 10, 4), 4);
        assert_eq!(wrap_indent("\t\t\tx", 10, 4), 5);
        assert_eq!(wrap_segments("short", 10, 4).as_slice(), &[0]);
    }

    #[test]
    fn folds_map_rows() {
        let mut m = DisplayMap::new(10);
        m.set_folds([2..5, 7..9]); // hides 3,4 and 8
        assert_eq!(m.visible_count(), 7);
        let rows: Vec<u32> = (0..7).map(|v| m.visible_to_row(v)).collect();
        assert_eq!(rows, vec![0, 1, 2, 5, 6, 7, 9]);
        for (v, row) in rows.iter().enumerate() {
            assert_eq!(m.row_to_visible(*row), v as u32);
        }
        assert!(m.is_hidden(3) && m.is_hidden(8) && !m.is_hidden(5));
        assert_eq!(m.row_to_visible(4), 2);
        assert_eq!(m.row_to_display(9), 6);
    }

    #[test]
    fn nested_folds_merge() {
        let mut m = DisplayMap::new(10);
        m.set_folds([1..8, 2..4]);
        assert_eq!(m.visible_count(), 4); // 0,1,8,9
        assert_eq!(m.visible_to_row(2), 8);
    }

    #[test]
    fn wrap_maps_display_rows() {
        let lines = ["a".repeat(25), "b".to_string(), "c".repeat(12)];
        let mut m = DisplayMap::new(3);
        m.set_wrap(Some(10), 4, |r| lines[r as usize].clone());
        assert_eq!(m.display_row_count(), 3 + 1 + 2);
        assert_eq!(m.display_to_row(0), (0, 0));
        assert_eq!(m.display_to_row(2), (0, 2));
        assert_eq!(m.display_to_row(3), (1, 0));
        assert_eq!(m.display_to_row(5), (2, 1));
        assert_eq!(m.row_to_display(2), 4);
        assert_eq!(m.display_to_row(99), (2, 1));
    }
}
