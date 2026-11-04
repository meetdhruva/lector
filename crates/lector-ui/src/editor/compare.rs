//! A read-only unified comparison of two texts (e.g. the file on disk vs the
//! unsaved buffer), built on `lector_diff` rows and code-view decorations.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{AnyView, App, AppContext, Entity, FocusHandle, Focusable, SharedString, Window};
use lector_diff::{diff_lines, unified_rows, DiffOptions, RowKind};

use crate::code_view::{CodeSource, CodeView, DecorationColor, LexicalHighlighter, RowDecoration};
use crate::code_view::{HighlightSpan, SpanStyle};
use crate::icons::IconName;
use crate::views::pane::{PaneItem, TabIcon};

/// One row of the comparison.
struct Row {
    text: SharedString,
    kind: RowKind,
    old: Option<u32>,
    new: Option<u32>,
}

/// Unified rows of `old` → `new` as a [`CodeSource`].
pub struct CompareSource {
    rows: Vec<Row>,
    version: u64,
    max_len: usize,
    highlighter: Option<LexicalHighlighter>,
    digits: usize,
}

impl CompareSource {
    /// Builds the comparison (`file_name` picks lexical highlighting).
    pub fn new(old: &str, new: &str, file_name: Option<&str>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let diff = diff_lines(old, new, &DiffOptions::default());
        let split = |t: &str| -> Vec<String> {
            t.split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
                .collect()
        };
        let (old_lines, new_lines) = (split(old), split(new));
        let rows: Vec<Row> = unified_rows(&diff)
            .into_iter()
            .map(|r| {
                let text = match (r.kind, r.old, r.new) {
                    (RowKind::Removed, Some(o), _) => old_lines.get(o as usize),
                    (_, _, Some(n)) => new_lines.get(n as usize),
                    (_, Some(o), None) => old_lines.get(o as usize),
                    _ => None,
                }
                .cloned()
                .unwrap_or_default();
                Row {
                    text: text.into(),
                    kind: r.kind,
                    old: r.old,
                    new: r.new,
                }
            })
            .collect();
        let max_len = rows.iter().map(|r| r.text.len()).max().unwrap_or(0);
        let digits = (old_lines.len().max(new_lines.len()).max(1) as f64)
            .log10()
            .floor() as usize
            + 1;
        CompareSource {
            rows,
            version: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            max_len,
            highlighter: file_name
                .and_then(LexicalHighlighter::for_file_name)
                .map(|(_, h)| h),
            digits,
        }
    }

    /// Number of changed (added or removed) rows.
    pub fn changed_rows(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| !matches!(r.kind, RowKind::Context))
            .count()
    }
}

impl CodeSource for CompareSource {
    fn line_count(&self) -> u32 {
        self.rows.len().max(1) as u32
    }

    fn line(&self, row: u32) -> SharedString {
        self.rows
            .get(row as usize)
            .map(|r| r.text.clone())
            .unwrap_or_default()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn max_line_len(&self) -> Option<usize> {
        Some(self.max_len)
    }

    fn highlights(&self, rows: Range<u32>) -> Vec<HighlightSpan> {
        let Some(h) = &self.highlighter else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for row in rows.start..rows.end.min(self.line_count()) {
            let mut in_block = false;
            let line = self.line(row);
            out.extend(h.highlight_line(&line, &mut in_block).into_iter().map(
                |(range, capture)| HighlightSpan {
                    row,
                    range,
                    style: SpanStyle::Capture(capture),
                },
            ));
        }
        out
    }

    fn line_number_label(&self, row: u32) -> Option<SharedString> {
        let r = self.rows.get(row as usize)?;
        let fmt = |n: Option<u32>| match n {
            Some(n) => format!("{:>w$}", n + 1, w = self.digits),
            None => " ".repeat(self.digits),
        };
        Some(format!("{} {}", fmt(r.old), fmt(r.new)).into())
    }

    fn line_number_chars(&self) -> Option<usize> {
        Some(self.digits * 2 + 1)
    }

    fn row_decorations(&self, rows: Range<u32>) -> Vec<RowDecoration> {
        (rows.start..rows.end.min(self.line_count()))
            .filter_map(|row| {
                let r = self.rows.get(row as usize)?;
                let background = match r.kind {
                    RowKind::Added => DecorationColor::AddedLine,
                    RowKind::Removed => DecorationColor::RemovedLine,
                    RowKind::Changed => DecorationColor::AddedLine,
                    RowKind::Context => return None,
                };
                Some(RowDecoration {
                    row,
                    background: Some(background),
                    ranges: Vec::new(),
                })
            })
            .collect()
    }
}

/// A pane item showing a [`CompareSource`].
pub struct CompareItem {
    view: Entity<CodeView>,
    title: SharedString,
    path: Option<PathBuf>,
}

impl CompareItem {
    /// A comparison tab of `old` (e.g. disk) → `new` (e.g. buffer).
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        title: impl Into<SharedString>,
        path: Option<PathBuf>,
        old: &str,
        new: &str,
        cx: &mut App,
    ) -> Box<dyn PaneItem> {
        let name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string());
        let source = Rc::new(CompareSource::new(old, new, name.as_deref()));
        let view = cx.new(|cx| {
            let mut v = CodeView::new(source, cx);
            v.set_show_minimap(false, cx);
            v
        });
        Box::new(CompareItem {
            view,
            title: title.into(),
            path,
        })
    }
}

impl PaneItem for CompareItem {
    fn title(&self, _: &App) -> SharedString {
        self.title.clone()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::Diff)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn code_view(&self) -> Option<Entity<CodeView>> {
        Some(self.view.clone())
    }

    fn tab_kind(&self, _: &App) -> Option<lector_config::TabKind> {
        None
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        self.path
            .as_ref()
            .map(|p| format!("compare-disk:{}", p.display()).into())
    }

    fn clone_item(&self, _window: &mut Window, _cx: &mut App) -> Option<Box<dyn PaneItem>> {
        None
    }
}

/// Opens `item` in the window's workspace (active pane).
pub fn open_compare_item(item: Box<dyn PaneItem>, window: &mut Window, cx: &mut App) {
    let Some(Some(root)) = window.root::<crate::views::WorkspaceRoot>() else {
        return;
    };
    let workspace = root.read(cx).workspace.clone();
    workspace.update(cx, |ws, cx| ws.add_item(item, false, window, cx));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_rows_mark_changes() {
        let s = CompareSource::new("a\nb\nc\n", "a\nB\nc\nd\n", Some("x.txt"));
        assert!(s.changed_rows() >= 3);
        let labels: Vec<String> = (0..s.line_count())
            .filter_map(|r| s.line_number_label(r).map(|l| l.to_string()))
            .collect();
        assert_eq!(labels[0], "1 1");
        let decos = s.row_decorations(0..s.line_count());
        assert!(decos
            .iter()
            .any(|d| d.background == Some(DecorationColor::RemovedLine)));
    }
}
