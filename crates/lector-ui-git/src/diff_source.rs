//! [`CodeSource`] implementations over a [`FileDiff`]: the old or new column
//! of a side-by-side diff (with filler rows), or a unified diff. Unchanged
//! regions can be folded into "⋯ N unchanged lines" rows.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gpui::SharedString;
use lector_diff::{RowKind, Segment};
use lector_ui::code_view::{CodeSource, DecorationColor, HighlightSpan, RowDecoration, SpanStyle};
use lector_workspace::review::{FileDiff, LineSpan};

static NEXT_VERSION: AtomicU64 = AtomicU64::new(1 << 40);

/// Which column a [`DiffSource`] shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiffColumn {
    /// Left column of a split diff (old side).
    Old,
    /// Right column of a split diff (new side).
    New,
    /// Single-column unified diff.
    Unified,
}

/// A row as displayed: a diff row or a folded run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayRow {
    /// Index into `FileDiff::split` (Old/New columns) or `FileDiff::unified`.
    Row(usize),
    /// Folded run of unchanged rows (indices into the same vector).
    Fold(Range<usize>),
}

/// Builds display rows from folded segments; folds whose start is in
/// `expanded` (or all, with `expand_all`) are shown in full.
pub fn display_rows(
    segments: &[Segment],
    expanded: &HashSet<usize>,
    expand_all: bool,
) -> Vec<DisplayRow> {
    let mut out = Vec::new();
    for seg in segments {
        match seg {
            Segment::Rows(r) => out.extend(r.clone().map(DisplayRow::Row)),
            Segment::Folded { rows } => {
                if expand_all || expanded.contains(&rows.start) {
                    out.extend(rows.clone().map(DisplayRow::Row));
                } else {
                    out.push(DisplayRow::Fold(rows.clone()));
                }
            }
        }
    }
    out
}

/// Index of the first display row of every hunk.
pub fn hunk_display_starts(diff: &FileDiff, rows: &[DisplayRow], unified: bool) -> Vec<u32> {
    let mut out = Vec::new();
    let mut last = None;
    for (i, r) in rows.iter().enumerate() {
        let hunk = match r {
            DisplayRow::Row(ix) => {
                if unified {
                    diff.unified.get(*ix).and_then(|r| r.hunk)
                } else {
                    diff.split.get(*ix).and_then(|r| r.hunk)
                }
            }
            DisplayRow::Fold(_) => None,
        };
        if hunk.is_some() && hunk != last {
            out.push(i as u32);
        }
        last = hunk;
    }
    out
}

/// Precomputed per-diff values shared by all columns.
pub struct DiffMeta {
    /// Longest old line (bytes).
    pub old_max: usize,
    /// Longest new line (bytes).
    pub new_max: usize,
}

impl DiffMeta {
    /// Computes the metadata (call off the main thread for big files).
    pub fn compute(diff: &FileDiff) -> DiffMeta {
        let max = |side: &lector_workspace::review::DiffSide| {
            let n = side.line_count();
            (0..n)
                .map(|r| {
                    side.text
                        .as_ref()
                        .map(|t| t.line_len(r) as usize)
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
        };
        DiffMeta {
            old_max: max(&diff.old),
            new_max: max(&diff.new),
        }
    }
}

const CHUNK: u32 = 64;

/// Highlight spans per (old side?, row chunk).
type HighlightCache = HashMap<(bool, u32), Rc<Vec<LineSpan>>>;

/// One column of a diff as a [`CodeSource`].
pub struct DiffSource {
    diff: Arc<FileDiff>,
    meta: Arc<DiffMeta>,
    rows: Rc<Vec<DisplayRow>>,
    column: DiffColumn,
    word_diff: bool,
    version: u64,
    number_width: usize,
    highlight_cache: RefCell<HighlightCache>,
}

impl DiffSource {
    /// A column over `rows` (from [`display_rows`]).
    pub fn new(
        diff: Arc<FileDiff>,
        meta: Arc<DiffMeta>,
        rows: Rc<Vec<DisplayRow>>,
        column: DiffColumn,
        word_diff: bool,
    ) -> Self {
        let digits = |n: u32| (n.max(1) as f64).log10().floor() as usize + 1;
        let number_width = digits(diff.old.line_count())
            .max(digits(diff.new.line_count()))
            .max(3);
        DiffSource {
            diff,
            meta,
            rows,
            column,
            word_diff,
            version: NEXT_VERSION.fetch_add(1, Ordering::Relaxed),
            number_width,
            highlight_cache: RefCell::new(HashMap::new()),
        }
    }

    /// The display rows.
    pub fn rows(&self) -> &Rc<Vec<DisplayRow>> {
        &self.rows
    }

    /// (old line, new line, kind) of display row `row`; `None` for folds.
    pub fn lines_at(&self, row: u32) -> Option<(Option<u32>, Option<u32>, RowKind)> {
        match self.rows.get(row as usize)? {
            DisplayRow::Row(ix) => match self.column {
                DiffColumn::Unified => {
                    let r = self.diff.unified.get(*ix)?;
                    Some((r.old, r.new, r.kind))
                }
                _ => {
                    let r = self.diff.split.get(*ix)?;
                    Some((r.old, r.new, r.kind))
                }
            },
            DisplayRow::Fold(_) => None,
        }
    }

    /// The fold at display row `row`, if it is one.
    pub fn fold_at(&self, row: u32) -> Option<Range<usize>> {
        match self.rows.get(row as usize)? {
            DisplayRow::Fold(r) => Some(r.clone()),
            DisplayRow::Row(_) => None,
        }
    }

    /// (is_old_side, line) shown on display row `row` of this column.
    fn side_line(&self, row: u32) -> Option<(bool, u32)> {
        let (old, new, kind) = self.lines_at(row)?;
        match self.column {
            DiffColumn::Old => old.map(|l| (true, l)),
            DiffColumn::New => new.map(|l| (false, l)),
            DiffColumn::Unified => match kind {
                RowKind::Removed => old.map(|l| (true, l)),
                _ => new.map(|l| (false, l)),
            },
        }
    }

    fn spans_for(&self, old: bool, line: u32) -> Vec<LineSpan> {
        let chunk = line / CHUNK;
        let spans = {
            let cache = self.highlight_cache.borrow();
            cache.get(&(old, chunk)).cloned()
        };
        let spans = match spans {
            Some(s) => s,
            None => {
                let side = if old { &self.diff.old } else { &self.diff.new };
                let s = Rc::new(side.line_highlights(chunk * CHUNK..(chunk + 1) * CHUNK));
                let mut cache = self.highlight_cache.borrow_mut();
                if cache.len() > 256 {
                    cache.clear();
                }
                cache.insert((old, chunk), s.clone());
                s
            }
        };
        spans.iter().filter(|s| s.row == line).cloned().collect()
    }

    fn fold_label(rows: &Range<usize>) -> SharedString {
        let n = rows.len();
        format!("  ⋯  {n} unchanged line{}", if n == 1 { "" } else { "s" }).into()
    }
}

impl CodeSource for DiffSource {
    fn line_count(&self) -> u32 {
        (self.rows.len() as u32).max(1)
    }

    fn line(&self, row: u32) -> SharedString {
        if let Some(fold) = self.fold_at(row) {
            return Self::fold_label(&fold);
        }
        match self.side_line(row) {
            Some((true, l)) => self.diff.old.line(l).into(),
            Some((false, l)) => self.diff.new.line(l).into(),
            None => SharedString::default(),
        }
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn max_line_len(&self) -> Option<usize> {
        Some(match self.column {
            DiffColumn::Old => self.meta.old_max,
            DiffColumn::New => self.meta.new_max,
            DiffColumn::Unified => self.meta.old_max.max(self.meta.new_max),
        })
    }

    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan> {
        let mut out = Vec::new();
        for row in rows.start..rows.end.min(self.rows.len() as u32) {
            if self.fold_at(row).is_some() {
                let len = self.line(row).len();
                out.push(HighlightSpan {
                    row,
                    range: 0..len,
                    style: SpanStyle::Capture("comment"),
                });
                continue;
            }
            if let Some((old, line)) = self.side_line(row) {
                out.extend(
                    self.spans_for(old, line)
                        .into_iter()
                        .map(|s| HighlightSpan {
                            row,
                            range: s.range,
                            style: SpanStyle::Capture(s.id.name()),
                        }),
                );
            }
        }
        out
    }

    fn line_number(&self, row: u32) -> Option<u32> {
        self.side_line(row).map(|(_, l)| l)
    }

    fn line_number_label(&self, row: u32) -> Option<SharedString> {
        if self.column != DiffColumn::Unified {
            return None;
        }
        let (old, new, _) = self.lines_at(row)?;
        let w = self.number_width;
        let fmt = |n: Option<u32>| match n {
            Some(n) => format!("{:>w$}", n + 1),
            None => " ".repeat(w),
        };
        Some(format!("{} {}", fmt(old), fmt(new)).into())
    }

    fn line_number_chars(&self) -> Option<usize> {
        (self.column == DiffColumn::Unified).then_some(self.number_width * 2 + 1)
    }

    fn row_decorations(&self, rows: Range<u32>) -> Vec<RowDecoration> {
        let mut out = Vec::new();
        for row in rows.start..rows.end.min(self.rows.len() as u32) {
            if self.fold_at(row).is_some() {
                out.push(RowDecoration {
                    row,
                    background: Some(DecorationColor::HunkHeader),
                    ranges: Vec::new(),
                });
                continue;
            }
            let Some((old, new, kind)) = self.lines_at(row) else {
                continue;
            };
            let side = self.side_line(row);
            let (background, is_old) = match (self.column, kind, side) {
                (_, RowKind::Context, _) => continue,
                (DiffColumn::Old | DiffColumn::New, _, None) => (DecorationColor::Filler, false),
                (DiffColumn::Old, _, Some(_)) => (DecorationColor::RemovedLine, true),
                (DiffColumn::New, _, Some(_)) => (DecorationColor::AddedLine, false),
                (DiffColumn::Unified, RowKind::Removed, _) => (DecorationColor::RemovedLine, true),
                (DiffColumn::Unified, _, _) => (DecorationColor::AddedLine, false),
            };
            let mut ranges = Vec::new();
            if self.word_diff && background != DecorationColor::Filler {
                let inline = if is_old {
                    old.and_then(|l| self.diff.inline_for_old(l))
                } else {
                    new.and_then(|l| self.diff.inline_for_new(l))
                };
                if let Some(inline) = inline {
                    let (list, color) = if is_old {
                        (&inline.old, DecorationColor::RemovedWord)
                    } else {
                        (&inline.new, DecorationColor::AddedWord)
                    };
                    let line_len = self.line(row).len();
                    // A whole-line "refinement" adds nothing over the line color.
                    let whole = list.len() == 1 && list[0].start == 0 && list[0].end >= line_len;
                    if !whole {
                        ranges.extend(
                            list.iter()
                                .filter(|r| r.start < r.end)
                                .map(|r| (r.start.min(line_len)..r.end.min(line_len), color)),
                        );
                    }
                }
            }
            out.push(RowDecoration {
                row,
                background: Some(background),
                ranges,
            });
        }
        out
    }

    fn language_name(&self) -> Option<SharedString> {
        let lang = self
            .diff
            .new
            .language
            .as_ref()
            .or(self.diff.old.language.as_ref())?;
        Some(lang.name.clone().into())
    }
}

/// A read-only file at some revision (one side of a diff), with syntax
/// highlighting.
pub struct RevisionSource {
    side: lector_workspace::review::DiffSide,
    version: u64,
    max_len: usize,
}

impl RevisionSource {
    /// Source over `side` (text + syntax).
    pub fn new(side: lector_workspace::review::DiffSide) -> Self {
        let n = side.text.as_ref().map(|t| t.line_count()).unwrap_or(0);
        let max_len = (0..n)
            .map(|r| {
                side.text
                    .as_ref()
                    .map(|t| t.line_len(r) as usize)
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        RevisionSource {
            side,
            version: NEXT_VERSION.fetch_add(1, Ordering::Relaxed),
            max_len,
        }
    }
}

impl CodeSource for RevisionSource {
    fn line_count(&self) -> u32 {
        self.side
            .text
            .as_ref()
            .map(|t| t.line_count())
            .unwrap_or(1)
            .max(1)
    }

    fn line(&self, row: u32) -> SharedString {
        self.side.line(row).into()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn max_line_len(&self) -> Option<usize> {
        Some(self.max_len)
    }

    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan> {
        self.side
            .line_highlights(rows)
            .into_iter()
            .map(|s| HighlightSpan {
                row: s.row,
                range: s.range,
                style: SpanStyle::Capture(s.id.name()),
            })
            .collect()
    }

    fn language_name(&self) -> Option<SharedString> {
        self.side.language.as_ref().map(|l| l.name.clone().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_diff::DiffOptions;
    use lector_git::{ChangeKind, FileChange, Side};
    use lector_syntax::LanguageRegistry;
    use lector_workspace::review::diff_bytes;

    fn diff(old: &str, new: &str) -> Arc<FileDiff> {
        let change = FileChange {
            old_path: Some("a.rs".into()),
            new_path: Some("a.rs".into()),
            kind: ChangeKind::Modified,
            old: Side::None,
            new: Side::None,
            binary: None,
        };
        Arc::new(diff_bytes(
            change,
            Some(old.as_bytes().to_vec()),
            Some(new.as_bytes().to_vec()),
            &LanguageRegistry::new(),
            &DiffOptions::default(),
        ))
    }

    #[test]
    fn split_columns_fillers_folds_and_words() {
        let old: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 10\n", "line ten\nextra\n");
        let d = diff(&old, &new);
        let meta = Arc::new(DiffMeta::compute(&d));
        let rows = Rc::new(display_rows(&d.split_segments(2), &HashSet::new(), false));
        let left = DiffSource::new(d.clone(), meta.clone(), rows.clone(), DiffColumn::Old, true);
        let right = DiffSource::new(d.clone(), meta, rows.clone(), DiffColumn::New, true);
        // A leading fold, 2 context, changed + added, 2 context, trailing fold.
        assert!(left.fold_at(0).is_some());
        assert!(left.line(0).contains("unchanged lines"));
        let changed = (0..left.line_count())
            .find(|&r| left.line(r) == "line 10")
            .unwrap();
        assert_eq!(right.line(changed), "line ten");
        assert_eq!(right.line(changed + 1), "extra");
        // The old side has a filler row next to the added line.
        assert_eq!(left.line_number(changed + 1), None);
        let decos = left.row_decorations(changed..changed + 2);
        assert_eq!(decos[0].background, Some(DecorationColor::RemovedLine));
        assert!(!decos[0].ranges.is_empty(), "word-level ranges");
        assert_eq!(decos[1].background, Some(DecorationColor::Filler));
        let starts = hunk_display_starts(&d, &rows, false);
        assert_eq!(starts, vec![changed]);
        // Expanding everything shows all rows.
        let all = display_rows(&d.split_segments(2), &HashSet::new(), true);
        assert!(all.iter().all(|r| matches!(r, DisplayRow::Row(_))));
    }

    #[test]
    fn unified_labels() {
        let d = diff("a\nb\nc\n", "a\nB\nc\n");
        let meta = Arc::new(DiffMeta::compute(&d));
        let rows = Rc::new(display_rows(&d.unified_segments(3), &HashSet::new(), false));
        let src = DiffSource::new(d, meta, rows, DiffColumn::Unified, true);
        assert_eq!(src.line_number_chars(), Some(7));
        let labels: Vec<String> = (0..src.line_count())
            .map(|r| src.line_number_label(r).unwrap().to_string())
            .collect();
        assert_eq!(labels[0], "  1   1");
        assert_eq!(labels[1], "  2    ");
        assert_eq!(labels[2], "      2");
        assert_eq!(src.line(1), "b");
        assert_eq!(src.line(2), "B");
    }
}
