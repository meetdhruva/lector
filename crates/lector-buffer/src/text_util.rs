//! Text helpers on top of [`Rope`]: grapheme and word boundaries, line and
//! column arithmetic. Everything here is total: out-of-range inputs clamp.

use std::ops::Range;

use lector_text::{Bias, Point, Rope};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// Bytes of context read on each side of an offset for grapheme analysis.
const GRAPHEME_WINDOW: usize = 128;
/// Bytes of a line read on each side of an offset for word analysis.
const WORD_WINDOW: usize = 2048;

/// Line terminator style of a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Eol {
    Lf,
    CrLf,
}

impl Eol {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
        }
    }
}

/// The terminator of the first line break of `rope`, `None` without any.
pub(crate) fn detect_eol(rope: &Rope) -> Option<Eol> {
    let mut prev_cr = false;
    for chunk in rope.chunks() {
        if let Some(i) = chunk.find('\n') {
            let cr = if i == 0 {
                prev_cr
            } else {
                chunk.as_bytes()[i - 1] == b'\r'
            };
            return Some(if cr { Eol::CrLf } else { Eol::Lf });
        }
        prev_cr = chunk.ends_with('\r');
    }
    None
}

pub(crate) fn char_at(rope: &Rope, offset: usize) -> Option<char> {
    rope.chars_at(offset).next()
}

pub(crate) fn char_before(rope: &Rope, offset: usize) -> Option<char> {
    rope.reversed_chars_at(offset).next()
}

fn extend_window(rope: &Rope, start: usize, end: usize) -> Range<usize> {
    let len = rope.len();
    let mut start = start;
    let mut end = end;
    if start > 0
        && start < len
        && rope.byte_at(start) == Some(b'\n')
        && rope.byte_at(start - 1) == Some(b'\r')
    {
        start -= 1;
    }
    if end > 0
        && end < len
        && rope.byte_at(end) == Some(b'\n')
        && rope.byte_at(end - 1) == Some(b'\r')
    {
        end += 1;
    }
    start..end
}

fn grapheme_window(rope: &Rope, offset: usize) -> (usize, String) {
    let start = rope.clip_offset(offset.saturating_sub(GRAPHEME_WINDOW), Bias::Left);
    let end = rope.clip_offset((offset + GRAPHEME_WINDOW).min(rope.len()), Bias::Right);
    let w = extend_window(rope, start, end);
    (w.start, rope.text_for_range(w))
}

/// True when `offset` is at a grapheme cluster boundary (`\r\n` is one cluster).
pub(crate) fn is_grapheme_boundary(rope: &Rope, offset: usize) -> bool {
    let len = rope.len();
    if offset == 0 || offset >= len {
        return true;
    }
    if let (Some(p), Some(n)) = (rope.byte_at(offset - 1), rope.byte_at(offset)) {
        if p < 0x80 && n < 0x80 {
            return !(p == b'\r' && n == b'\n');
        }
        if (0x80..0xC0).contains(&n) {
            return false;
        }
    }
    let (base, s) = grapheme_window(rope, offset);
    let rel = offset - base;
    if !s.is_char_boundary(rel) {
        return false;
    }
    let mut gc = GraphemeCursor::new(rel, s.len(), true);
    gc.is_boundary(&s, 0).unwrap_or(true)
}

/// The grapheme boundary before `offset` (strictly smaller unless 0).
pub(crate) fn prev_grapheme(rope: &Rope, offset: usize) -> usize {
    let offset = rope.clip_offset(offset.min(rope.len()), Bias::Left);
    if offset == 0 {
        return 0;
    }
    let (base, s) = grapheme_window(rope, offset);
    let rel = offset - base;
    let mut gc = GraphemeCursor::new(rel, s.len(), true);
    match gc.prev_boundary(&s, 0) {
        Ok(Some(b)) => base + b,
        Ok(None) => base,
        Err(_) => rope.clip_offset(offset - 1, Bias::Left),
    }
}

/// The grapheme boundary after `offset` (strictly larger unless at the end).
pub(crate) fn next_grapheme(rope: &Rope, offset: usize) -> usize {
    let len = rope.len();
    let offset = rope.clip_offset(offset.min(len), Bias::Left);
    if offset >= len {
        return len;
    }
    let (base, s) = grapheme_window(rope, offset);
    let rel = offset - base;
    let mut gc = GraphemeCursor::new(rel, s.len(), true);
    match gc.next_boundary(&s, 0) {
        Ok(Some(b)) => base + b,
        Ok(None) => base + s.len(),
        Err(_) => rope.clip_offset(offset + 1, Bias::Right),
    }
}

/// Clips `offset` to a grapheme boundary.
pub(crate) fn clip_grapheme(rope: &Rope, offset: usize, bias: Bias) -> usize {
    let o = rope.clip_offset(offset.min(rope.len()), Bias::Left);
    if is_grapheme_boundary(rope, o) {
        return o;
    }
    match bias {
        Bias::Left => prev_grapheme(rope, o),
        Bias::Right => next_grapheme(rope, o),
    }
}

// ---- lines -------------------------------------------------------------

pub(crate) fn row_of(rope: &Rope, offset: usize) -> u32 {
    rope.offset_to_point(offset).row
}

pub(crate) fn row_start(rope: &Rope, row: u32) -> usize {
    rope.point_to_offset(Point::new(row, 0))
}

/// Start of the row after `row` (after its terminator), or the text length.
pub(crate) fn row_end_with_eol(rope: &Rope, row: u32) -> usize {
    if row + 1 < rope.line_count() {
        row_start(rope, row + 1)
    } else {
        rope.len()
    }
}

/// Number of chars between the start of the row of `offset` and `offset`.
pub(crate) fn col_chars(rope: &Rope, offset: usize) -> u32 {
    let row = row_of(rope, offset);
    let ls = row_start(rope, row);
    rope.text_summary_for_range(ls..offset.max(ls)).chars as u32
}

/// Offset of char column `col` on `row`, clamped to the end of the row text.
pub(crate) fn offset_at_col(rope: &Rope, row: u32, col: u32) -> usize {
    let lr = rope.line_range(row);
    let mut off = lr.start;
    for ch in rope.chars_at(lr.start).take(col as usize) {
        if off >= lr.end {
            break;
        }
        off += ch.len_utf8();
    }
    off.min(lr.end)
}

/// Length in bytes of the leading blanks (spaces and tabs) of `text`.
pub(crate) fn leading_ws_len(text: &str) -> usize {
    text.len() - text.trim_start_matches([' ', '\t']).len()
}

/// Visual column after `text` where tabs jump to the next multiple of `tab_size`.
pub(crate) fn visual_col(text: &str, tab_size: u32) -> u32 {
    let ts = tab_size.max(1);
    let mut col = 0u32;
    for ch in text.chars() {
        if ch == '\t' {
            col = (col / ts + 1) * ts;
        } else {
            col += 1;
        }
    }
    col
}

pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---- words ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    Space,
    Word,
    Punct,
}

fn classify(seg: &str) -> Class {
    if seg.chars().all(char::is_whitespace) {
        Class::Space
    } else if seg.chars().any(is_word_char) {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Word segments of `line` (UAX #29), with runs of whitespace and runs of
/// punctuation merged into one segment each. UAX #29 keeps `a.b` and `can't`
/// together; for code those split at the punctuation, so mixed segments are
/// split between word characters and the rest (by grapheme cluster, so
/// combining marks stay attached).
fn segments(line: &str) -> Vec<(Range<usize>, Class)> {
    let mut out: Vec<(Range<usize>, Class)> = Vec::new();
    let mut push = |range: Range<usize>, class: Class| {
        if let Some((last, lclass)) = out.last_mut() {
            if *lclass == class && class != Class::Word {
                last.end = range.end;
                return;
            }
        }
        out.push((range, class));
    };
    for (i, seg) in line.split_word_bound_indices() {
        let class = classify(seg);
        if class != Class::Word || seg.chars().all(is_word_char) {
            push(i..i + seg.len(), class);
            continue;
        }
        let mut run_start = 0;
        let mut run_word = None;
        for (gi, g) in seg.grapheme_indices(true) {
            let word = g.chars().next().is_some_and(is_word_char);
            match run_word {
                Some(w) if w == word => {}
                Some(w) => {
                    push(
                        i + run_start..i + gi,
                        if w { Class::Word } else { Class::Punct },
                    );
                    run_start = gi;
                    run_word = Some(word);
                }
                None => run_word = Some(word),
            }
        }
        if let Some(w) = run_word {
            push(
                i + run_start..i + seg.len(),
                if w { Class::Word } else { Class::Punct },
            );
        }
    }
    out
}

/// The row text around `offset`: `(base offset, text, row range start..end)`.
fn line_window(rope: &Rope, offset: usize) -> (usize, String, Range<usize>) {
    let row = row_of(rope, offset);
    let lr = rope.line_range(row);
    let start = rope.clip_offset(lr.start.max(offset.saturating_sub(WORD_WINDOW)), Bias::Left);
    let end = rope.clip_offset(lr.end.min(offset + WORD_WINDOW), Bias::Right);
    (start, rope.text_for_range(start..end.max(start)), lr)
}

/// Where Ctrl+Left lands: skips blanks, then one word or punctuation run;
/// at the start of a line moves to the end of the previous one.
pub(crate) fn word_left(rope: &Rope, offset: usize) -> usize {
    let offset = rope.clip_offset(offset.min(rope.len()), Bias::Left);
    let (base, text, lr) = line_window(rope, offset);
    if offset <= lr.start {
        let row = row_of(rope, offset);
        return if row == 0 {
            0
        } else {
            rope.line_range(row - 1).end
        };
    }
    let rel = offset - base;
    let segs = segments(&text);
    let Some(mut i) = segs.iter().rposition(|(r, _)| r.start < rel) else {
        return base;
    };
    if segs[i].1 == Class::Space && i > 0 {
        i -= 1;
    }
    base + segs[i].0.start
}

/// Where Ctrl+Right lands: skips blanks, then one word or punctuation run;
/// at the end of a line moves to the start of the next one.
pub(crate) fn word_right(rope: &Rope, offset: usize) -> usize {
    let len = rope.len();
    let offset = rope.clip_offset(offset.min(len), Bias::Left);
    let (base, text, lr) = line_window(rope, offset);
    if offset >= lr.end {
        let row = row_of(rope, offset);
        return row_end_with_eol(rope, row).max(offset);
    }
    let rel = offset - base;
    let segs = segments(&text);
    let Some(mut i) = segs.iter().position(|(r, _)| r.end > rel) else {
        return lr.end;
    };
    if segs[i].1 == Class::Space && i + 1 < segs.len() {
        i += 1;
    }
    base + segs[i].0.end
}

/// The word (or whitespace / punctuation run) at `offset`; empty on empty lines.
pub(crate) fn word_range_at(rope: &Rope, offset: usize) -> Range<usize> {
    word_range_impl(rope, offset, false)
}

/// Like [`word_range_at`] but empty unless the caret touches a real word.
pub(crate) fn identifier_range_at(rope: &Rope, offset: usize) -> Range<usize> {
    word_range_impl(rope, offset, true)
}

fn word_range_impl(rope: &Rope, offset: usize, only_words: bool) -> Range<usize> {
    let offset = rope.clip_offset(offset.min(rope.len()), Bias::Left);
    let (base, text, lr) = line_window(rope, offset);
    if lr.is_empty() {
        return offset..offset;
    }
    let rel = offset - base;
    let segs = segments(&text);
    let after = segs.iter().find(|(r, _)| r.start <= rel && rel < r.end);
    let before = segs.iter().find(|(r, _)| r.start < rel && rel <= r.end);
    let pick = match (after, before) {
        (Some(a), _) if a.1 == Class::Word => Some(a),
        (_, Some(b)) if b.1 == Class::Word => Some(b),
        (Some(a), _) => Some(a),
        (None, b) => b,
    };
    match pick {
        Some((r, class)) if !only_words || *class == Class::Word => base + r.start..base + r.end,
        _ => offset..offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn graphemes() {
        let r = rope("a\u{301}b👨‍👩‍👧x\r\ny");
        // "á" = 'a' + U+0301 (3 bytes)
        assert_eq!(next_grapheme(&r, 0), 3);
        assert_eq!(prev_grapheme(&r, 3), 0);
        assert!(!is_grapheme_boundary(&r, 1));
        let family_end = 4 + "👨‍👩‍👧".len();
        assert_eq!(next_grapheme(&r, 4), family_end);
        assert_eq!(prev_grapheme(&r, family_end), 4);
        // \r\n is a unit
        let nl = family_end + 1;
        assert_eq!(next_grapheme(&r, nl), nl + 2);
        assert_eq!(prev_grapheme(&r, nl + 2), nl);
        assert!(!is_grapheme_boundary(&r, nl + 1));
        assert_eq!(clip_grapheme(&r, nl + 1, Bias::Left), nl);
        assert_eq!(clip_grapheme(&r, nl + 1, Bias::Right), nl + 2);
    }

    #[test]
    fn words() {
        let r = rope("foo bar_baz.qux  ++ x");
        assert_eq!(word_right(&r, 0), 3);
        assert_eq!(word_right(&r, 3), 11); // skips blank, whole word incl. underscore
        assert_eq!(word_right(&r, 11), 12);
        assert_eq!(word_left(&r, 11), 4);
        assert_eq!(word_left(&r, 12), 11);
        assert_eq!(word_left(&r, 4), 0);
        assert_eq!(word_left(&r, 3), 0);
        let e = "foo bar_baz.qux  ".len();
        assert_eq!(word_left(&r, e + 2), e); // punctuation run
        assert_eq!(word_range_at(&r, 5), 4..11);
        assert_eq!(word_range_at(&r, 3), 0..3);
    }

    #[test]
    fn words_cross_lines() {
        let r = rope("ab\r\ncd");
        assert_eq!(word_right(&r, 2), 4);
        assert_eq!(word_left(&r, 4), 2);
        assert_eq!(word_left(&r, 0), 0);
        assert_eq!(word_right(&r, 6), 6);
    }

    #[test]
    fn eol_detection() {
        assert_eq!(detect_eol(&rope("a")), None);
        assert_eq!(detect_eol(&rope("a\nb\r\n")), Some(Eol::Lf));
        assert_eq!(detect_eol(&rope("a\r\nb\n")), Some(Eol::CrLf));
    }
}
