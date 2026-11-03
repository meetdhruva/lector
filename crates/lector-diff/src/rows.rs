//! Row layouts (split / unified), context folding and gutter marks.

use std::ops::Range;

use crate::LineDiff;

/// Kind of a diff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKind {
    /// Unchanged line (both line numbers set).
    Context,
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Removed,
    /// Split view only: a removed line paired with an added line.
    Changed,
}

/// A side-by-side row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SplitRow {
    /// Row kind.
    pub kind: RowKind,
    /// Old line (0-based), if any.
    pub old: Option<u32>,
    /// New line (0-based), if any.
    pub new: Option<u32>,
    /// Index into `LineDiff::hunks` for non-context rows.
    pub hunk: Option<usize>,
}

/// A unified row (`Changed` is never produced).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnifiedRow {
    /// Row kind.
    pub kind: RowKind,
    /// Old line (0-based), if any.
    pub old: Option<u32>,
    /// New line (0-based), if any.
    pub new: Option<u32>,
    /// Index into `LineDiff::hunks` for non-context rows.
    pub hunk: Option<usize>,
}

/// Implemented by row types so [`fold_context`] works on both layouts.
pub trait HasKind {
    /// The row's kind.
    fn kind(&self) -> RowKind;
}

impl HasKind for SplitRow {
    fn kind(&self) -> RowKind {
        self.kind
    }
}

impl HasKind for UnifiedRow {
    fn kind(&self) -> RowKind {
        self.kind
    }
}

fn context_run(old: &mut u32, new: &mut u32, until_old: u32, mut push: impl FnMut(u32, u32)) {
    while *old < until_old {
        push(*old, *new);
        *old += 1;
        *new += 1;
    }
}

/// Side-by-side rows: per hunk, removed/added lines are paired as `Changed` up to the
/// shorter side, the remainder is `Removed` or `Added`. Includes context rows.
pub fn split_rows(diff: &LineDiff) -> Vec<SplitRow> {
    let mut rows = Vec::new();
    let (mut o, mut n) = (0u32, 0u32);
    let ctx = |rows: &mut Vec<SplitRow>, old, new| {
        rows.push(SplitRow {
            kind: RowKind::Context,
            old: Some(old),
            new: Some(new),
            hunk: None,
        })
    };
    for (i, h) in diff.hunks.iter().enumerate() {
        context_run(&mut o, &mut n, h.old.start, |a, b| ctx(&mut rows, a, b));
        let (ol, nl) = (h.old.end - h.old.start, h.new.end - h.new.start);
        for k in 0..ol.max(nl) {
            let old = (k < ol).then_some(h.old.start + k);
            let new = (k < nl).then_some(h.new.start + k);
            let kind = match (old, new) {
                (Some(_), Some(_)) => RowKind::Changed,
                (Some(_), None) => RowKind::Removed,
                _ => RowKind::Added,
            };
            rows.push(SplitRow {
                kind,
                old,
                new,
                hunk: Some(i),
            });
        }
        o = h.old.end;
        n = h.new.end;
    }
    context_run(&mut o, &mut n, diff.old_lines, |a, b| ctx(&mut rows, a, b));
    rows
}

/// Unified rows: per hunk all removed lines, then all added lines. Includes context rows.
pub fn unified_rows(diff: &LineDiff) -> Vec<UnifiedRow> {
    let mut rows = Vec::new();
    let (mut o, mut n) = (0u32, 0u32);
    let ctx = |rows: &mut Vec<UnifiedRow>, old, new| {
        rows.push(UnifiedRow {
            kind: RowKind::Context,
            old: Some(old),
            new: Some(new),
            hunk: None,
        })
    };
    for (i, h) in diff.hunks.iter().enumerate() {
        context_run(&mut o, &mut n, h.old.start, |a, b| ctx(&mut rows, a, b));
        for l in h.old.clone() {
            rows.push(UnifiedRow {
                kind: RowKind::Removed,
                old: Some(l),
                new: None,
                hunk: Some(i),
            });
        }
        for l in h.new.clone() {
            rows.push(UnifiedRow {
                kind: RowKind::Added,
                old: None,
                new: Some(l),
                hunk: Some(i),
            });
        }
        o = h.old.end;
        n = h.new.end;
    }
    context_run(&mut o, &mut n, diff.old_lines, |a, b| ctx(&mut rows, a, b));
    rows
}

/// A run of rows to show, or a folded run of unchanged rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Rows to display (indices into the rows slice).
    Rows(Range<usize>),
    /// Hidden context rows (indices into the rows slice).
    Folded {
        /// The hidden rows.
        rows: Range<usize>,
    },
}

/// Folds long runs of `Context` rows, keeping `context` rows next to each change.
///
/// Runs at the very start keep only the trailing `context` rows, runs at the very end
/// only the leading ones; a file without changes folds entirely. A fold hides at least
/// two rows (a one-row fold would be no smaller than the row it replaces).
pub fn fold_context<R: HasKind>(rows: &[R], context: u32) -> Vec<Segment> {
    let n = context as usize;
    let mut out: Vec<Segment> = Vec::new();
    let push_rows = |out: &mut Vec<Segment>, r: Range<usize>| {
        if r.is_empty() {
            return;
        }
        if let Some(Segment::Rows(prev)) = out.last_mut() {
            if prev.end == r.start {
                prev.end = r.end;
                return;
            }
        }
        out.push(Segment::Rows(r));
    };
    let mut i = 0;
    while i < rows.len() {
        let start = i;
        let is_ctx = rows[i].kind() == RowKind::Context;
        while i < rows.len() && (rows[i].kind() == RowKind::Context) == is_ctx {
            i += 1;
        }
        if !is_ctx {
            push_rows(&mut out, start..i);
            continue;
        }
        let keep_head = if start == 0 { 0 } else { n };
        let keep_tail = if i == rows.len() { 0 } else { n };
        let len = i - start;
        if len < keep_head + keep_tail + 2 {
            push_rows(&mut out, start..i);
        } else {
            push_rows(&mut out, start..start + keep_head);
            out.push(Segment::Folded {
                rows: start + keep_head..i - keep_tail,
            });
            push_rows(&mut out, i - keep_tail..i);
        }
    }
    out
}

/// Kind of an in-editor gutter mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GutterKind {
    /// Pure insertion.
    Added,
    /// Lines replaced.
    Modified,
    /// Pure deletion (empty range).
    Deleted,
}

/// A gutter mark on the new text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GutterMark {
    /// Rows in the NEW text; for `Deleted` an empty range at the row after the removal.
    pub rows: Range<u32>,
    /// Mark kind.
    pub kind: GutterKind,
}

/// Gutter marks for the diff of HEAD (old) against the buffer (new).
pub fn gutter_marks(diff: &LineDiff) -> Vec<GutterMark> {
    diff.hunks
        .iter()
        .map(|h| {
            if h.old.is_empty() {
                GutterMark {
                    rows: h.new.clone(),
                    kind: GutterKind::Added,
                }
            } else if h.new.is_empty() {
                GutterMark {
                    rows: h.new.start..h.new.start,
                    kind: GutterKind::Deleted,
                }
            } else {
                GutterMark {
                    rows: h.new.clone(),
                    kind: GutterKind::Modified,
                }
            }
        })
        .collect()
}
