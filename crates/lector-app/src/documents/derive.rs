//! Pure helpers for derived document data: mapping ranges through edits
//! (diagnostics) and remapping blame through a line diff.

use std::ops::Range;

use lector_buffer::EditDelta;
use lector_diff::LineDiff;
use lector_git::{BlameHunk, Oid};
use lector_lsp::{DiagnosticSeverity, DiagnosticTag};
use lector_text::{PointUtf16, Rope};

/// One diagnostic in the current text of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocDiagnostic {
    /// Byte range in the current text (may be empty).
    pub range: Range<usize>,
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Message.
    pub message: String,
    /// Producer (`rustc`, `pyflakes`, ...).
    pub source: Option<String>,
    /// Code.
    pub code: Option<String>,
    /// Unnecessary / deprecated.
    pub tags: Vec<DiagnosticTag>,
}

/// Maps a byte offset through one edit. Starts move right with text inserted
/// at them; ends stay (a range does not grow when typing right after it).
pub fn map_offset(off: usize, d: &EditDelta, is_end: bool) -> usize {
    if off < d.start_byte || (is_end && off == d.start_byte) {
        off
    } else if off >= d.old_end_byte {
        off - d.old_end_byte + d.new_end_byte
    } else if is_end {
        d.new_end_byte
    } else {
        d.start_byte
    }
}

/// Maps a byte range through a transaction's deltas (in their listed order).
pub fn map_range(range: &Range<usize>, deltas: &[EditDelta]) -> Range<usize> {
    let mut r = range.clone();
    for d in deltas {
        let start = map_offset(r.start, d, false);
        let end = map_offset(r.end, d, true).max(start);
        r = start..end;
    }
    r
}

/// Maps a row through edits (rows inside a replaced region move to its start).
pub fn map_row(row: u32, deltas: &[EditDelta]) -> u32 {
    let mut row = row;
    for d in deltas {
        if row > d.old_end.row {
            row = row - d.old_end.row + d.new_end.row;
        } else if row > d.start.row {
            row = d.start.row.max(row.min(d.new_end.row));
        }
    }
    row
}

/// Byte offset of a UTF-16 LSP position, clamped to the line and to a char
/// boundary.
pub fn offset_for_utf16(text: &Rope, point: PointUtf16) -> usize {
    let rows = text.line_count();
    if point.row >= rows {
        return text.len();
    }
    let range = text.line_range(point.row);
    let line = text.text_for_range(range.clone());
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if units >= point.column {
            return range.start + i;
        }
        units += c.len_utf16() as u32;
        if units > point.column {
            // Inside a surrogate pair: stay before the char.
            return range.start + i;
        }
    }
    range.end
}

/// Converts published LSP diagnostics against `text` (the text they were
/// computed for).
pub fn convert_diagnostics(text: &Rope, diags: Vec<lector_lsp::Diagnostic>) -> Vec<DocDiagnostic> {
    diags
        .into_iter()
        .map(|d| {
            let start = offset_for_utf16(text, d.range.start);
            let end = offset_for_utf16(text, d.range.end).max(start);
            DocDiagnostic {
                range: start..end,
                severity: d.severity,
                message: d.message,
                source: d.source,
                code: d.code,
                tags: d.tags,
            }
        })
        .collect()
}

/// Remaps blame `hunks` (lines of the blamed text) through `diff` (blamed
/// text → current text): unchanged lines keep their commit, added or edited
/// lines become "not committed yet" (`uncommitted`, zero oid).
pub fn remap_blame(hunks: &[BlameHunk], diff: &LineDiff) -> Vec<BlameHunk> {
    let edited = Oid::from_bytes(&[0u8; 20]);
    let mut out: Vec<BlameHunk> = Vec::with_capacity(hunks.len() + diff.hunks.len());
    let copy = |out: &mut Vec<BlameHunk>, old: Range<u32>, new_start: u32| {
        let first = hunks.partition_point(|h| h.lines.end <= old.start);
        for h in hunks[first..]
            .iter()
            .take_while(|h| h.lines.start < old.end)
        {
            let start = h.lines.start.max(old.start);
            let end = h.lines.end.min(old.end);
            if start >= end {
                continue;
            }
            let mut moved = h.clone();
            moved.lines = start - old.start + new_start..end - old.start + new_start;
            moved.orig_start = h.orig_start + (start - h.lines.start);
            out.push(moved);
        }
    };
    let (mut old_pos, mut new_pos) = (0u32, 0u32);
    for hunk in &diff.hunks {
        copy(&mut out, old_pos..hunk.old.start, new_pos);
        if let (Some(oid), false) = (edited, hunk.new.is_empty()) {
            out.push(BlameHunk {
                oid,
                lines: hunk.new.clone(),
                orig_start: 0,
                orig_path: String::new(),
                author: String::new(),
                time: 0,
                summary: String::new(),
                uncommitted: true,
            });
        }
        old_pos = hunk.old.end;
        new_pos = hunk.new.end;
    }
    copy(&mut out, old_pos..diff.old_lines, new_pos);
    out
}

/// Line endings do not matter to line mapping.
pub fn unix_lines(text: String) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_text::Point;

    fn delta(start: usize, old_end: usize, new_end: usize) -> EditDelta {
        EditDelta {
            start_byte: start,
            old_end_byte: old_end,
            new_end_byte: new_end,
            start: Point::new(0, start as u32),
            old_end: Point::new(0, old_end as u32),
            new_end: Point::new(0, new_end as u32),
        }
    }

    #[test]
    fn ranges_follow_edits_without_growing() {
        // Insert 2 bytes at 5: a range 5..8 moves to 7..10.
        assert_eq!(map_range(&(5..8), &[delta(5, 5, 7)]), 7..10);
        // Typing right after the end does not grow it.
        assert_eq!(map_range(&(2..5), &[delta(5, 5, 6)]), 2..5);
        // Deleting the whole range collapses it.
        assert_eq!(map_range(&(2..5), &[delta(1, 6, 1)]), 1..1);
    }

    #[test]
    fn utf16_offsets_clamp() {
        let text = Rope::from_str("aé😀b\nxy");
        assert_eq!(offset_for_utf16(&text, PointUtf16 { row: 0, column: 2 }), 3);
        assert_eq!(offset_for_utf16(&text, PointUtf16 { row: 0, column: 3 }), 3);
        assert_eq!(
            offset_for_utf16(&text, PointUtf16 { row: 0, column: 99 }),
            8
        );
        assert_eq!(
            offset_for_utf16(&text, PointUtf16 { row: 7, column: 0 }),
            text.len()
        );
    }
}
