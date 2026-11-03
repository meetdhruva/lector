//! [`DocumentSource`]: a [`CodeSource`] over a `lector_workspace::Document`.
//!
//! Highlights are computed off the main thread: `highlights()` returns what
//! is cached and queues missing row chunks on a channel that the owning
//! [`super::FileView`] drains on a background executor, notifying the code
//! view when results arrive. Rows without results render as plain text.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use gpui::SharedString;
use lector_workspace::{Document, LineHighlights};

use crate::code_view::{CodeSource, GutterMark, GutterMarkKind, HighlightSpan, SpanStyle};

/// Rows per highlight chunk (requests and cache granularity).
pub const HIGHLIGHT_CHUNK: u32 = 128;

/// Mutable state shared between a [`DocumentSource`] and its owner.
#[derive(Default)]
pub struct SourceState {
    highlights: RefCell<HashMap<u32, Arc<Vec<LineHighlights>>>>,
    pending: RefCell<HashSet<u32>>,
    gutter: RefCell<Rc<Vec<GutterMark>>>,
    sticky_cache: RefCell<Option<(u32, Vec<u32>)>>,
}

impl SourceState {
    /// Stores computed highlights for `chunk`.
    pub fn insert_chunk(&self, chunk: u32, lines: Arc<Vec<LineHighlights>>) {
        self.pending.borrow_mut().remove(&chunk);
        self.highlights.borrow_mut().insert(chunk, lines);
    }

    /// Forgets that `chunk` was requested (so it is requested again).
    pub fn cancel_chunk(&self, chunk: u32) {
        self.pending.borrow_mut().remove(&chunk);
    }

    /// Whether `chunk` has results.
    pub fn has_chunk(&self, chunk: u32) -> bool {
        self.highlights.borrow().contains_key(&chunk)
    }

    /// Replaces the diff gutter marks.
    pub fn set_gutter(&self, marks: Vec<GutterMark>) {
        *self.gutter.borrow_mut() = Rc::new(marks);
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
        let folds = doc.fold_ranges();
        let requests = if doc.syntax().is_some() {
            requests
        } else {
            None
        };
        DocumentSource {
            doc,
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
        let Some(requests) = &self.requests else {
            return Vec::new();
        };
        let end = rows.end.min(self.line_count);
        if rows.start >= end {
            return Vec::new();
        }
        let mut out = Vec::new();
        let cache = self.state.highlights.borrow();
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
                    if self.state.pending.borrow_mut().insert(chunk) {
                        let _ = requests.unbounded_send(chunk);
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
        let rows: Vec<u32> = match self.doc.text() {
            Some(text) => {
                let mut rows: Vec<u32> = self
                    .doc
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
