//! The edit engine: applies a batch of non-overlapping changes to a rope,
//! computes exact deltas and maps selections through the batch.

use lector_text::{Point, Rope, TextSummary};

use crate::text_util::{offset_at_col, row_of};
use crate::EditDelta;

/// Replacement of `start..end` (offsets in the pre-edit text) by `text`.
#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Change {
    pub(crate) fn insert(at: usize, text: impl Into<String>) -> Change {
        Change {
            start: at,
            end: at,
            text: text.into(),
        }
    }

    pub(crate) fn delete(start: usize, end: usize) -> Change {
        Change {
            start,
            end,
            text: String::new(),
        }
    }

    pub(crate) fn replace(start: usize, end: usize, text: impl Into<String>) -> Change {
        Change {
            start,
            end,
            text: text.into(),
        }
    }

    fn delta(&self) -> isize {
        self.text.len() as isize - (self.end - self.start) as isize
    }
}

/// Which way an offset sticks when text is inserted exactly at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    /// Stays before the inserted text.
    Before,
    /// Moves after the inserted text.
    After,
}

/// A position in the text after a batch of changes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pos {
    /// An offset in the old text, mapped through the changes.
    Old(usize, Side),
    /// `rel` bytes into the text inserted by change number `idx`.
    In(usize, usize),
    /// Char column `col` of the line containing the start of change `idx` (new text).
    LineCol(usize, u32),
    /// Char column `col` of `row` in the new text.
    RowCol(u32, u32),
}

/// A selection after the batch; `id: None` allocates a fresh id.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NewSel {
    pub id: Option<u64>,
    pub anchor: Pos,
    pub head: Pos,
    pub goal: Option<u32>,
}

/// What kind of edit this is, for undo coalescing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKind {
    Typing,
    Backspace,
    DeleteForward,
    Other,
}

impl EditKind {
    pub(crate) fn coalescible(self) -> bool {
        self != EditKind::Other
    }
}

pub(crate) struct Plan {
    pub changes: Vec<Change>,
    pub sels: Vec<NewSel>,
    pub kind: EditKind,
}

/// The new text for a sorted, non-overlapping change list.
pub(crate) fn build_rope(old: &Rope, changes: &[Change]) -> Rope {
    if let [c] = changes {
        let mut r = old.clone();
        r.replace(c.start..c.end, &c.text);
        return r;
    }
    // Short gaps between changes are copied into a pending buffer that is pushed
    // in large blocks (far cheaper than a tree operation per caret); long gaps
    // share structure with `old`.
    const COPY_GAP: usize = 512;
    const FLUSH_AT: usize = 16 * 1024;
    let mut out = Rope::new();
    let mut pending = String::new();
    let mut pos = 0;
    let keep = |out: &mut Rope, pending: &mut String, from: usize, to: usize| {
        if to <= from {
            return;
        }
        if to - from <= COPY_GAP {
            for chunk in old.chunks_in_range(from..to) {
                pending.push_str(chunk);
            }
            if pending.len() >= FLUSH_AT {
                out.push(pending);
                pending.clear();
            }
        } else {
            out.push(pending);
            pending.clear();
            out.append(old.slice(from..to));
        }
    };
    for c in changes {
        keep(&mut out, &mut pending, pos, c.start);
        pending.push_str(&c.text);
        pos = c.end;
    }
    keep(&mut out, &mut pending, pos, old.len());
    out.push(&pending);
    out
}

/// Exact deltas, back to front.
pub(crate) fn build_deltas(old: &Rope, changes: &[Change]) -> Vec<EditDelta> {
    let mut deltas = Vec::with_capacity(changes.len());
    for c in changes.iter().rev() {
        let start = old.offset_to_point(c.start);
        let old_end = if c.end == c.start {
            start
        } else {
            old.offset_to_point(c.end)
        };
        let extent: Point = if c.text.is_empty() {
            Point::ZERO
        } else {
            TextSummary::from_text(&c.text).lines
        };
        deltas.push(EditDelta {
            start_byte: c.start,
            old_end_byte: c.end,
            new_end_byte: c.start + c.text.len(),
            start,
            old_end,
            new_end: start + extent,
        });
    }
    deltas
}

/// Maps selection positions through a change list.
pub(crate) struct Mapper<'a> {
    changes: &'a [Change],
    /// `cum[i]` = net length delta of `changes[..i]`.
    cum: Vec<isize>,
    new: &'a Rope,
}

impl<'a> Mapper<'a> {
    pub(crate) fn new(changes: &'a [Change], new: &'a Rope) -> Mapper<'a> {
        let mut cum = Vec::with_capacity(changes.len() + 1);
        let mut acc = 0isize;
        cum.push(0);
        for c in changes {
            acc += c.delta();
            cum.push(acc);
        }
        Mapper { changes, cum, new }
    }

    fn new_start(&self, idx: usize) -> usize {
        match self.changes.get(idx) {
            Some(c) => (c.start as isize + self.cum[idx]).max(0) as usize,
            None => self.new.len(),
        }
    }

    fn map_old(&self, off: usize, side: Side) -> usize {
        let k = self.changes.len();
        let i = self.changes.partition_point(|c| c.end < off);
        let mut shift = self.cum[i];
        let mut j = i;
        while j < k && self.changes[j].start <= off {
            let c = &self.changes[j];
            if c.start == c.end {
                match side {
                    Side::After => {
                        shift += c.text.len() as isize;
                        j += 1;
                        continue;
                    }
                    Side::Before => break,
                }
            }
            if off == c.end {
                shift += c.delta();
            } else {
                let new_start = c.start as isize + shift;
                let r = match side {
                    Side::Before => new_start,
                    Side::After => new_start + c.text.len() as isize,
                };
                return r.max(0) as usize;
            }
            break;
        }
        (off as isize + shift).max(0) as usize
    }

    pub(crate) fn resolve(&self, pos: Pos) -> usize {
        let len = self.new.len();
        let off = match pos {
            Pos::Old(off, side) => self.map_old(off, side),
            Pos::In(idx, rel) => self.new_start(idx) + rel,
            Pos::LineCol(idx, col) => {
                let base = self.new_start(idx).min(len);
                let row = row_of(self.new, base);
                offset_at_col(self.new, row, col)
            }
            Pos::RowCol(row, col) => {
                let row = row.min(self.new.max_point().row);
                offset_at_col(self.new, row, col)
            }
        };
        off.min(len)
    }
}
