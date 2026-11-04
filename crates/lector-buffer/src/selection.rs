//! Selections and their normalization.

use std::ops::Range;

use lector_text::{Bias, Rope};

use crate::text_util::clip_grapheme;

/// A selection (or caret when empty) in byte offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Stable identifier; survives edits, merges keep the surviving selection's id.
    pub id: u64,
    /// The fixed end.
    pub anchor: usize,
    /// The moving end (the caret).
    pub head: usize,
    /// Preferred column in chars for vertical movement.
    pub goal: Option<u32>,
}

impl Selection {
    /// An empty selection at `offset`.
    pub fn caret(id: u64, offset: usize) -> Selection {
        Selection {
            id,
            anchor: offset,
            head: offset,
            goal: None,
        }
    }

    /// The covered byte range, ordered.
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    /// True for a bare caret.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// True when the head is before the anchor.
    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }

    pub(crate) fn with_range(&self, range: Range<usize>, reversed: bool) -> Selection {
        let (anchor, head) = if reversed {
            (range.end, range.start)
        } else {
            (range.start, range.end)
        };
        Selection {
            anchor,
            head,
            ..*self
        }
    }
}

/// Clips every endpoint to a grapheme boundary, sorts and merges.
///
/// Two selections merge when they overlap, or touch while at least one is
/// empty. The merged selection keeps the id/direction/goal of the newest
/// selection when involved, else of the first one. Never returns an empty
/// list for a non-empty input.
pub(crate) fn normalize(rope: &Rope, mut sels: Vec<Selection>, newest: u64) -> Vec<Selection> {
    for s in &mut sels {
        s.anchor = clip_grapheme(rope, s.anchor, Bias::Left);
        s.head = if s.head == s.anchor {
            s.anchor
        } else {
            clip_grapheme(rope, s.head, Bias::Left)
        };
    }
    let sorted = sels.windows(2).all(|w| {
        let (a, b) = (w[0].range(), w[1].range());
        (a.start, a.end) <= (b.start, b.end)
    });
    if !sorted {
        sels.sort_by_key(|s| {
            let r = s.range();
            (r.start, r.end)
        });
    }
    let mut out: Vec<Selection> = Vec::with_capacity(sels.len());
    for s in sels {
        if let Some(last) = out.last_mut() {
            let (lr, sr) = (last.range(), s.range());
            let overlap =
                sr.start < lr.end || (sr.start == lr.end && (lr.is_empty() || sr.is_empty()));
            if overlap {
                let merged = lr.start..lr.end.max(sr.end);
                let winner = if s.id == newest { &s } else { &*last };
                let reversed = winner.reversed();
                let mut m = winner.with_range(merged, reversed);
                if m.is_empty() {
                    m.goal = winner.goal;
                }
                *last = m;
                continue;
            }
        }
        out.push(s);
    }
    out
}
