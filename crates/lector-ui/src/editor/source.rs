//! [`DocumentSource`]: a [`CodeSource`] over a `lector_workspace::Document`.
//!
//! Highlights are computed off the main thread: `highlights()` returns what
//! is cached and queues missing row chunks on a channel that the owning
//! [`super::DocumentModel`] forwards to the document store (computed on its
//! executor), notifying the views when results arrive. Rows without results render as plain text.
//!
//! While a document is being edited, the syntax tree of the newest text may
//! still be parsing. Meanwhile rows are drawn with the previous highlights
//! mapped through the edits ([`StaleHighlights`]): unchanged rows shift with
//! the text, an edited row keeps the spans left and right of the edit, and
//! only text inside an edit renders plain — no frame ever waits for a parse.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use gpui::SharedString;
use lector_buffer::EditDelta;
use lector_syntax::HighlightId;
use lector_workspace::{Document, LineHighlights};

use crate::code_view::{CodeSource, GutterMark, GutterMarkKind, HighlightSpan, SpanStyle};

/// Rows per highlight chunk (requests and cache granularity; the store's).
pub const HIGHLIGHT_CHUNK: u32 = lector_app::documents::HIGHLIGHT_CHUNK;

/// How many stale layers may chain before older ones are flattened.
const MAX_STALE_DEPTH: usize = 6;
/// Rows around an edit that are flattened when the chain is cut.
const FLATTEN_MARGIN: u32 = 160;

type Spans = Arc<Vec<(Range<usize>, HighlightId)>>;

/// Highlights of a previous text version, viewed through later edits.
pub struct StaleHighlights {
    /// Fresh chunks of the previous version.
    prev_chunks: HashMap<u32, Arc<Vec<LineHighlights>>>,
    /// Flattened rows of older versions (previous-version coordinates).
    prev_rows: HashMap<u32, Spans>,
    /// The layer below (previous version's own stale highlights).
    prev: Option<Rc<StaleHighlights>>,
    /// Edits from the previous version to this one, in application order.
    deltas: Vec<EditDelta>,
    depth: usize,
    memo: RefCell<HashMap<u32, Option<Spans>>>,
}

/// A piece of a row: columns `cols` (in some version) that land at column
/// `dest` of the newest row.
#[derive(Clone, Debug, PartialEq)]
struct Piece {
    row: u32,
    cols: Range<usize>,
    dest: usize,
}

/// Maps pieces of the text after `d` back to the text before it.
fn unapply(d: &EditDelta, pieces: Vec<Piece>) -> Vec<Piece> {
    let (s_row, s_col) = (d.start.row, d.start.column as usize);
    let (o_row, o_col) = (d.old_end.row, d.old_end.column as usize);
    let (n_row, n_col) = (d.new_end.row, d.new_end.column as usize);
    let mut out = Vec::with_capacity(pieces.len() + 1);
    for p in pieces {
        if p.row < s_row {
            out.push(p);
            continue;
        }
        if p.row > n_row {
            out.push(Piece {
                row: p.row - n_row + o_row,
                ..p
            });
            continue;
        }
        // Prefix before the edit (on the start row).
        if p.row == s_row && p.cols.start < s_col {
            out.push(Piece {
                row: s_row,
                cols: p.cols.start..p.cols.end.min(s_col),
                dest: p.dest,
            });
        }
        // Suffix after the inserted text (on the new end row).
        if p.row == n_row && p.cols.end > n_col {
            let from = p.cols.start.max(n_col);
            let shift_back = |c: usize| c - n_col + o_col;
            out.push(Piece {
                row: o_row,
                cols: shift_back(from)..shift_back(p.cols.end).max(shift_back(from)),
                dest: p.dest + (from - p.cols.start),
            });
        }
    }
    out
}

fn spans_for_row(chunks: &HashMap<u32, Arc<Vec<LineHighlights>>>, row: u32) -> Option<Spans> {
    let lines = chunks.get(&(row / HIGHLIGHT_CHUNK))?;
    let line = lines.get((row % HIGHLIGHT_CHUNK) as usize)?;
    (line.row == row).then(|| Arc::new(line.spans.clone()))
}

impl StaleHighlights {
    fn lookup_prev(&self, row: u32) -> Option<Spans> {
        if let Some(s) = spans_for_row(&self.prev_chunks, row) {
            return Some(s);
        }
        if let Some(s) = self.prev_rows.get(&row) {
            return Some(s.clone());
        }
        self.prev.as_ref()?.lookup(row)
    }

    /// Spans for `row` of this layer's version, if any are known.
    pub fn lookup(&self, row: u32) -> Option<Spans> {
        if let Some(hit) = self.memo.borrow().get(&row) {
            return hit.clone();
        }
        let mut pieces = vec![Piece {
            row,
            cols: 0..usize::MAX / 2,
            dest: 0,
        }];
        for d in self.deltas.iter().rev() {
            pieces = unapply(d, pieces);
            if pieces.is_empty() {
                break;
            }
        }
        let mut out: Vec<(Range<usize>, HighlightId)> = Vec::new();
        let mut any = false;
        for p in &pieces {
            let Some(spans) = self.lookup_prev(p.row) else {
                continue;
            };
            any = true;
            for (range, id) in spans.iter() {
                let s = range.start.max(p.cols.start);
                let e = range.end.min(p.cols.end);
                if e > s {
                    let at = p.dest + (s - p.cols.start);
                    out.push((at..at + (e - s), *id));
                }
            }
        }
        out.sort_by_key(|(r, _)| r.start);
        let result = any.then(|| Arc::new(out));
        self.memo.borrow_mut().insert(row, result.clone());
        result
    }
}

/// Mutable state shared between a [`DocumentSource`] and its owner.
#[derive(Default)]
pub struct SourceState {
    highlights: RefCell<HashMap<u32, Arc<Vec<LineHighlights>>>>,
    pending: RefCell<HashSet<u32>>,
    stale: RefCell<Option<Rc<StaleHighlights>>>,
    gutter: RefCell<Rc<Vec<GutterMark>>>,
    sticky_cache: RefCell<Option<(u32, Vec<u32>)>>,
    /// Text version the fresh highlight chunks belong to.
    version: Cell<u64>,
}

impl SourceState {
    /// Stores computed highlights for `chunk`.
    pub fn insert_chunk(&self, chunk: u32, lines: Arc<Vec<LineHighlights>>) {
        self.pending.borrow_mut().remove(&chunk);
        self.highlights.borrow_mut().insert(chunk, lines);
    }

    /// Stores `chunk` only if it was computed for the current text version.
    pub fn insert_chunk_for(&self, version: u64, chunk: u32, lines: Arc<Vec<LineHighlights>>) {
        if self.version.get() == version {
            self.insert_chunk(chunk, lines);
        } else {
            self.pending.borrow_mut().remove(&chunk);
        }
    }

    /// Forgets that `chunk` was requested (so it is requested again).
    pub fn cancel_chunk(&self, chunk: u32) {
        self.pending.borrow_mut().remove(&chunk);
    }

    /// Whether `chunk` has results.
    pub fn has_chunk(&self, chunk: u32) -> bool {
        self.highlights.borrow().contains_key(&chunk)
    }

    /// Text version of the fresh highlights.
    pub fn version(&self) -> u64 {
        self.version.get()
    }

    /// Sets the text version (on load).
    pub fn set_version(&self, version: u64) {
        self.version.set(version);
    }

    /// Replaces the diff gutter marks.
    pub fn set_gutter(&self, marks: Vec<GutterMark>) {
        *self.gutter.borrow_mut() = Rc::new(marks);
    }

    /// The text changed from the current version to `new_version` through
    /// `deltas`: fresh chunks entirely above the first edited row stay valid;
    /// everything else becomes a stale layer drawn until new results arrive.
    /// Gutter marks shift with the text.
    pub fn begin_edit(&self, new_version: u64, deltas: &[EditDelta]) {
        let first_row = deltas.iter().map(|d| d.start.row).min().unwrap_or(0);
        let old_chunks = std::mem::take(&mut *self.highlights.borrow_mut());
        let keep: HashMap<u32, Arc<Vec<LineHighlights>>> = old_chunks
            .iter()
            .filter(|(chunk, _)| (**chunk + 1) * HIGHLIGHT_CHUNK <= first_row)
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let prev = self.stale.borrow_mut().take();
        let depth = prev.as_ref().map_or(0, |p| p.depth + 1);
        let (prev, prev_rows) = match prev {
            Some(p) if depth > MAX_STALE_DEPTH => {
                // Flatten rows near the edit; drop the older chain.
                let lo = first_row.saturating_sub(FLATTEN_MARGIN);
                let last = deltas.iter().map(|d| d.old_end.row).max().unwrap_or(0);
                let mut rows = HashMap::new();
                for row in lo..last + FLATTEN_MARGIN {
                    if let Some(s) = p.lookup(row) {
                        rows.insert(row, s);
                    }
                }
                (None, rows)
            }
            other => (other, HashMap::new()),
        };
        let layer = StaleHighlights {
            prev_chunks: old_chunks,
            prev_rows,
            prev,
            deltas: deltas.to_vec(),
            depth: if depth > MAX_STALE_DEPTH { 0 } else { depth },
            memo: RefCell::new(HashMap::new()),
        };
        *self.highlights.borrow_mut() = keep;
        self.pending.borrow_mut().clear();
        *self.stale.borrow_mut() = Some(Rc::new(layer));
        self.version.set(new_version);
        *self.sticky_cache.borrow_mut() = None;
        let gutter = self.gutter.borrow().clone();
        if !gutter.is_empty() {
            let mapped = gutter
                .iter()
                .map(|m| GutterMark {
                    rows: crate::code_view::edit::map_row_through(m.rows.start, deltas)
                        ..crate::code_view::edit::map_row_through(m.rows.end, deltas),
                    kind: m.kind,
                })
                .collect();
            *self.gutter.borrow_mut() = Rc::new(mapped);
        }
    }

    /// Replaces all highlight state (reload with a new parse).
    pub fn reset(&self, version: u64) {
        self.highlights.borrow_mut().clear();
        self.pending.borrow_mut().clear();
        *self.stale.borrow_mut() = None;
        *self.sticky_cache.borrow_mut() = None;
        self.version.set(version);
    }

    /// Drops the stale layer once every chunk in `rows` is fresh.
    fn stale_spans(&self, row: u32) -> Option<Spans> {
        self.stale.borrow().as_ref()?.lookup(row)
    }
}

/// Converts `lector_diff` gutter marks to code-view marks.
pub fn convert_gutter(marks: &[lector_diff::GutterMark]) -> Vec<GutterMark> {
    marks
        .iter()
        .map(|m| GutterMark {
            rows: m.rows.clone(),
            kind: match m.kind {
                lector_diff::GutterKind::Added => GutterMarkKind::Added,
                lector_diff::GutterKind::Modified => GutterMarkKind::Modified,
                lector_diff::GutterKind::Deleted => GutterMarkKind::Deleted,
            },
        })
        .collect()
}

/// A code source over a document snapshot.
pub struct DocumentSource {
    doc: Document,
    /// Settled snapshot (outline computed) for sticky headers.
    settled: Option<Document>,
    line_count: u32,
    max_len: Option<usize>,
    language: Option<SharedString>,
    folds: Arc<Vec<Range<u32>>>,
    state: Rc<SourceState>,
    requests: Option<UnboundedSender<u32>>,
}

impl DocumentSource {
    /// Source over `doc` (text documents). `requests` receives chunk indices
    /// whose highlights are missing; `None` disables highlighting.
    pub fn new(
        doc: Document,
        state: Rc<SourceState>,
        requests: Option<UnboundedSender<u32>>,
    ) -> Self {
        let folds = doc.fold_ranges();
        let settled = Some(doc.clone());
        Self::with_parts(doc, settled, folds, state, requests)
    }

    /// Source over an edited snapshot: `folds` and `settled` (for sticky
    /// headers) may lag behind `doc` until the owner recomputes them.
    pub fn with_parts(
        doc: Document,
        settled: Option<Document>,
        folds: Arc<Vec<Range<u32>>>,
        state: Rc<SourceState>,
        requests: Option<UnboundedSender<u32>>,
    ) -> Self {
        let (line_count, max_len) = match doc.text() {
            Some(text) => {
                let max_len = text.as_rope().map(|rope| {
                    let s = rope.summary();
                    rope.line_len(s.longest_row) as usize
                });
                (text.line_count().max(1), max_len)
            }
            None => (1, Some(0)),
        };
        let language = doc
            .language()
            .map(|l| SharedString::from(l.name.clone()))
            .or_else(|| Some("Plain Text".into()));
        let requests = if doc.syntax().is_some() {
            requests
        } else {
            None
        };
        DocumentSource {
            doc,
            settled,
            line_count,
            max_len,
            language,
            folds,
            state,
            requests,
        }
    }

    /// The document.
    pub fn document(&self) -> &Document {
        &self.doc
    }

    /// Shared state (highlight cache, gutter).
    pub fn state(&self) -> &Rc<SourceState> {
        &self.state
    }
}

impl CodeSource for DocumentSource {
    fn line_count(&self) -> u32 {
        self.line_count
    }

    fn line(&self, row: u32) -> SharedString {
        match self.doc.text() {
            Some(text) if row < text.line_count() => text.line(row).into_owned().into(),
            _ => SharedString::default(),
        }
    }

    fn version(&self) -> u64 {
        self.doc.text().map_or(0, |t| t.version())
    }

    fn max_line_len(&self) -> Option<usize> {
        self.max_len
    }

    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan> {
        let end = rows.end.min(self.line_count);
        if rows.start >= end {
            return Vec::new();
        }
        let mut out = Vec::new();
        let cache = self.state.highlights.borrow();
        let has_stale = self.state.stale.borrow().is_some();
        if self.requests.is_none() && cache.is_empty() && !has_stale {
            return out;
        }
        for chunk in rows.start / HIGHLIGHT_CHUNK..=(end - 1) / HIGHLIGHT_CHUNK {
            match cache.get(&chunk) {
                Some(lines) => {
                    for line in lines.iter() {
                        if line.row < rows.start || line.row >= end {
                            continue;
                        }
                        out.extend(line.spans.iter().map(|(range, id)| HighlightSpan {
                            row: line.row,
                            range: range.clone(),
                            style: SpanStyle::Capture(id.name()),
                        }));
                    }
                }
                None => {
                    if let Some(requests) = &self.requests {
                        if self.state.pending.borrow_mut().insert(chunk) {
                            let _ = requests.unbounded_send(chunk);
                        }
                    }
                    if has_stale {
                        let lo = (chunk * HIGHLIGHT_CHUNK).max(rows.start);
                        let hi = ((chunk + 1) * HIGHLIGHT_CHUNK).min(end);
                        for row in lo..hi {
                            if let Some(spans) = self.state.stale_spans(row) {
                                out.extend(spans.iter().map(|(range, id)| HighlightSpan {
                                    row,
                                    range: range.clone(),
                                    style: SpanStyle::Capture(id.name()),
                                }));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    fn gutter_marks(&self, rows: Range<u32>) -> Vec<GutterMark> {
        let marks = self.state.gutter.borrow().clone();
        marks
            .iter()
            .filter(|m| m.rows.start <= rows.end && m.rows.end >= rows.start)
            .cloned()
            .collect()
    }

    fn fold_ranges(&self) -> Vec<Range<u32>> {
        self.folds.as_ref().clone()
    }

    fn sticky_rows(&self, top_row: u32) -> Vec<u32> {
        if let Some((row, rows)) = self.state.sticky_cache.borrow().as_ref() {
            if *row == top_row {
                return rows.clone();
            }
        }
        // Sticky headers come from the settled snapshot while edits are
        // being reparsed (they may lag a few hundred milliseconds).
        let doc = if self.doc.syntax().is_some() {
            &self.doc
        } else {
            match &self.settled {
                Some(d) => d,
                None => return Vec::new(),
            }
        };
        let rows: Vec<u32> = match doc.text() {
            Some(text) => {
                let mut rows: Vec<u32> = doc
                    .scopes_at_row(top_row)
                    .iter()
                    .map(|s| text.offset_to_point(s.name_range.start).row)
                    .filter(|r| *r < top_row)
                    .collect();
                rows.dedup();
                rows
            }
            None => Vec::new(),
        };
        *self.state.sticky_cache.borrow_mut() = Some((top_row, rows.clone()));
        rows
    }

    fn language_name(&self) -> Option<SharedString> {
        self.language.clone()
    }

    fn text_snapshot(&self) -> Option<lector_text::Text> {
        self.doc.text().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_text::Point;

    fn delta(start: (u32, u32), old_end: (u32, u32), new_end: (u32, u32)) -> EditDelta {
        EditDelta {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: 0,
            start: Point::new(start.0, start.1),
            old_end: Point::new(old_end.0, old_end.1),
            new_end: Point::new(new_end.0, new_end.1),
        }
    }

    type Rows = Vec<(u32, Vec<(Range<usize>, u16)>)>;

    fn layer(rows: Rows, deltas: Vec<EditDelta>) -> StaleHighlights {
        let mut prev_rows = HashMap::new();
        for (row, spans) in rows {
            prev_rows.insert(
                row,
                Arc::new(
                    spans
                        .into_iter()
                        .map(|(r, id)| (r, HighlightId(id)))
                        .collect::<Vec<_>>(),
                ),
            );
        }
        StaleHighlights {
            prev_chunks: HashMap::new(),
            prev_rows,
            prev: None,
            deltas,
            depth: 0,
            memo: RefCell::new(HashMap::new()),
        }
    }

    fn ids(spans: Option<Spans>) -> Vec<(Range<usize>, u16)> {
        spans
            .map(|s| s.iter().map(|(r, id)| (r.clone(), id.0)).collect())
            .unwrap_or_default()
    }

    #[test]
    fn typing_shifts_spans_after_the_caret() {
        // "let x = 1;" — type "yy" at column 4.
        let l = layer(
            vec![(0, vec![(0..3, 1), (8..9, 2)])],
            vec![delta((0, 4), (0, 4), (0, 6))],
        );
        assert_eq!(ids(l.lookup(0)), vec![(0..3, 1), (10..11, 2)]);
    }

    #[test]
    fn newline_splits_a_row_and_shifts_rows_below() {
        // Row 0 "fn a() {}" split at column 8 with 4 spaces of indent.
        let l = layer(
            vec![(0, vec![(0..2, 1), (8..9, 3)]), (1, vec![(0..3, 4)])],
            vec![delta((0, 8), (0, 8), (1, 4))],
        );
        assert_eq!(ids(l.lookup(0)), vec![(0..2, 1)]);
        assert_eq!(ids(l.lookup(1)), vec![(4..5, 3)]);
        assert_eq!(ids(l.lookup(2)), vec![(0..3, 4)]);
    }

    #[test]
    fn deleted_lines_disappear() {
        // Delete rows 1..3 entirely (from (1,0) to (3,0)).
        let l = layer(
            vec![(0, vec![(0..1, 1)]), (3, vec![(0..2, 5)])],
            vec![delta((1, 0), (3, 0), (1, 0))],
        );
        assert_eq!(ids(l.lookup(0)), vec![(0..1, 1)]);
        assert_eq!(ids(l.lookup(1)), vec![(0..2, 5)]);
    }

    #[test]
    fn multi_cursor_edits_compose() {
        // Back-to-front deltas: insert 1 byte at (2,0) then at (0,0).
        let l = layer(
            vec![(0, vec![(0..2, 1)]), (2, vec![(0..2, 2)])],
            vec![delta((2, 0), (2, 0), (2, 1)), delta((0, 0), (0, 0), (0, 1))],
        );
        assert_eq!(ids(l.lookup(0)), vec![(1..3, 1)]);
        assert_eq!(ids(l.lookup(2)), vec![(1..3, 2)]);
    }
}
