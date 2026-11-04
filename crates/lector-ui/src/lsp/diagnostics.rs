//! Diagnostics of open documents: stored as byte ranges in the *current*
//! buffer text and mapped through every edit until the server republishes;
//! rendered as squiggles, gutter icons and scrollbar marks in each file tab,
//! shown in hovers, counted in the status bar, navigated with F8.
//!
//! Published diagnostics refer to the text synced with a tag (the buffer
//! text version, see `LspManager::open_document_tagged`). The store keeps a
//! short history of `(version, text, transaction)` per document so a publish
//! for an older text is converted against that text and mapped forward.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, EntityId, Focusable, Global, Subscription, Task, WeakEntity,
    Window,
};
use lector_buffer::{EditDelta, Transaction};
use lector_lsp::{DiagnosticSeverity, DiagnosticTag, DocumentDiagnostics};
use lector_text::Rope;

use crate::actions::editor::{CodeActions, Hover, NextDiagnostic, PrevDiagnostic};
use crate::code_view::{EditBufferEvent, GutterIcon, InlineMarks, Underline};
use crate::editor::{DocumentEvent, DocumentModel, FileView, FileViewEvent, Reveal};
use crate::icons::IconName;
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::Workspace;

/// Texts kept per document to map late publishes forward.
const HISTORY: usize = 256;
/// Caret rest before the lightbulb asks for code actions.
pub const LIGHTBULB_DELAY_MS: u64 = 400;

/// One diagnostic in the current buffer text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticEntry {
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

impl DiagnosticEntry {
    /// `source(code)` / `source` / `code`, for display.
    pub fn origin(&self) -> Option<String> {
        match (&self.source, &self.code) {
            (Some(s), Some(c)) => Some(format!("{s}({c})")),
            (Some(s), None) => Some(s.clone()),
            (None, Some(c)) => Some(c.clone()),
            (None, None) => None,
        }
    }
}

struct Snapshot {
    version: u64,
    text: Rope,
    /// The edit that produced this text from the previous snapshot.
    deltas: Arc<Transaction>,
}

struct DocState {
    path: PathBuf,
    entries: Vec<DiagnosticEntry>,
    history: VecDeque<Snapshot>,
    buffer_attached: bool,
    _subscriptions: Vec<Subscription>,
}

struct ViewState {
    view: WeakEntity<FileView>,
    lightbulb: Option<u32>,
    lightbulb_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Diagnostics of all open documents (a global entity; observe it for
/// changes).
pub struct DiagnosticsStore {
    docs: HashMap<EntityId, DocState>,
    views: HashMap<EntityId, ViewState>,
}

struct GlobalStore(Entity<DiagnosticsStore>);

impl Global for GlobalStore {}

/// Creates the store and starts tracking documents and file tabs.
pub(crate) fn init(cx: &mut App) {
    let store = DiagnosticsStore::global(cx);
    let s = store.clone();
    cx.observe_new(
        move |_: &mut DocumentModel, _, cx: &mut Context<DocumentModel>| {
            let model = cx.entity();
            let s = s.clone();
            cx.defer(move |cx| s.update(cx, |s, cx| s.track_document(model, cx)));
        },
    )
    .detach();
    cx.observe_new(move |_: &mut FileView, _, cx: &mut Context<FileView>| {
        let view = cx.entity();
        let s = store.clone();
        cx.defer(move |cx| s.update(cx, |s, cx| s.track_view(view, cx)));
    })
    .detach();
}

/// Maps a byte offset through one edit. Starts move right with text
/// inserted at them; ends stay (a diagnostic does not grow when typing
/// right after it).
fn map_offset(off: usize, d: &EditDelta, is_end: bool) -> usize {
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

fn severity_color(severity: DiagnosticSeverity, cx: &App) -> gpui::Hsla {
    let ui = &cx.theme().ui;
    match severity {
        DiagnosticSeverity::Error => ui.error.hsla(),
        DiagnosticSeverity::Warning => ui.warning.hsla(),
        DiagnosticSeverity::Information => ui.info.hsla(),
        DiagnosticSeverity::Hint => ui.hint.hsla(),
    }
}

/// Icon for a severity.
pub fn severity_icon(severity: DiagnosticSeverity) -> IconName {
    match severity {
        DiagnosticSeverity::Error => IconName::CircleX,
        DiagnosticSeverity::Warning => IconName::AlertTriangle,
        DiagnosticSeverity::Information | DiagnosticSeverity::Hint => IconName::Info,
    }
}

/// Lower-case severity name.
pub fn severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Information => "info",
        DiagnosticSeverity::Hint => "hint",
    }
}

impl DiagnosticsStore {
    /// The global store (created on first use).
    pub fn global(cx: &mut App) -> Entity<DiagnosticsStore> {
        if let Some(g) = cx.try_global::<GlobalStore>() {
            return g.0.clone();
        }
        let store = cx.new(|_| DiagnosticsStore {
            docs: HashMap::new(),
            views: HashMap::new(),
        });
        cx.set_global(GlobalStore(store.clone()));
        store
    }

    /// The global store, if created.
    pub fn try_global(cx: &App) -> Option<Entity<DiagnosticsStore>> {
        cx.try_global::<GlobalStore>().map(|g| g.0.clone())
    }

    fn doc_for_path(&self, path: &Path) -> Option<(&EntityId, &DocState)> {
        self.docs.iter().find(|(_, d)| d.path == path)
    }

    /// Diagnostics of `path` (byte ranges in its current buffer text),
    /// sorted by position.
    pub fn entries(&self, path: &Path) -> &[DiagnosticEntry] {
        self.doc_for_path(path)
            .map(|(_, d)| d.entries.as_slice())
            .unwrap_or_default()
    }

    /// Paths with diagnostics and their entries, sorted by path.
    pub fn all(&self) -> Vec<(PathBuf, Vec<DiagnosticEntry>)> {
        let mut out: Vec<_> = self
            .docs
            .values()
            .filter(|d| !d.entries.is_empty())
            .map(|d| (d.path.clone(), d.entries.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// (errors, warnings) over all open documents.
    pub fn counts(&self) -> (usize, usize) {
        let mut counts = (0, 0);
        for e in self.docs.values().flat_map(|d| &d.entries) {
            match e.severity {
                DiagnosticSeverity::Error => counts.0 += 1,
                DiagnosticSeverity::Warning => counts.1 += 1,
                _ => {}
            }
        }
        counts
    }

    /// Row the lightbulb is shown on in `view`.
    pub fn lightbulb_row(&self, view: &Entity<FileView>) -> Option<u32> {
        self.views.get(&view.entity_id()).and_then(|v| v.lightbulb)
    }

    // Tracking documents.

    fn track_document(&mut self, model: Entity<DocumentModel>, cx: &mut Context<Self>) {
        let id = model.entity_id();
        if self.docs.contains_key(&id) {
            return;
        }
        let path = model.read(cx).path().to_path_buf();
        let subs = vec![
            cx.subscribe(
                &model,
                |this, model, event: &DocumentEvent, cx| match event {
                    DocumentEvent::Loaded => this.attach_buffer(&model, cx),
                    DocumentEvent::PathChanged => {
                        if let Some(doc) = this.docs.get_mut(&model.entity_id()) {
                            let old =
                                std::mem::replace(&mut doc.path, model.read(cx).path().into());
                            doc.entries.clear();
                            this.refresh_views(&old, cx);
                            cx.notify();
                        }
                    }
                    _ => {}
                },
            ),
            cx.observe_release(&model, move |this, _, cx| {
                if let Some(doc) = this.docs.remove(&id) {
                    if !doc.entries.is_empty() {
                        cx.notify();
                    }
                }
            }),
        ];
        self.docs.insert(
            id,
            DocState {
                path,
                entries: Vec::new(),
                history: VecDeque::new(),
                buffer_attached: false,
                _subscriptions: subs,
            },
        );
        if model.read(cx).is_loaded() {
            self.attach_buffer(&model, cx);
        }
    }

    fn attach_buffer(&mut self, model: &Entity<DocumentModel>, cx: &mut Context<Self>) {
        let id = model.entity_id();
        let Some(buffer) = model.read(cx).buffer().cloned() else {
            return;
        };
        let Some(doc) = self.docs.get_mut(&id) else {
            return;
        };
        if doc.buffer_attached {
            return;
        }
        doc.buffer_attached = true;
        let text = buffer.read(cx).text().clone();
        doc.history.push_back(Snapshot {
            version: text.version(),
            text,
            deltas: Arc::new(Transaction::default()),
        });
        doc._subscriptions.push(cx.subscribe(
            &buffer,
            move |this, buffer, event: &EditBufferEvent, cx| {
                let EditBufferEvent::Edited { transaction, .. } = event;
                let text = buffer.read(cx).text().clone();
                this.edited(id, text, transaction.clone(), cx);
            },
        ));
        // Diagnostics may have been published before the buffer existed.
        let path = doc.path.clone();
        if let Some(lsp) = model.read(cx).project().lsp() {
            let published = lsp.diagnostics(&path);
            self.published(&path, published, cx);
        }
    }

    fn edited(&mut self, id: EntityId, text: Rope, tx: Arc<Transaction>, cx: &mut Context<Self>) {
        let Some(doc) = self.docs.get_mut(&id) else {
            return;
        };
        if tx.deltas.is_empty() {
            return;
        }
        let had_entries = !doc.entries.is_empty();
        for e in &mut doc.entries {
            e.range = map_range(&e.range, &tx.deltas);
        }
        doc.history.push_back(Snapshot {
            version: text.version(),
            text,
            deltas: tx,
        });
        while doc.history.len() > HISTORY {
            doc.history.pop_front();
        }
        let path = doc.path.clone();
        if had_entries {
            self.refresh_views(&path, cx);
            cx.notify();
        }
    }

    /// The server published (or cleared, `None`) diagnostics for `path`.
    /// Ignored for documents that are not open, and for publishes that refer
    /// to a text older than the history (the next publish replaces them).
    pub fn published(
        &mut self,
        path: &Path,
        published: Option<DocumentDiagnostics>,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.doc_for_path(path).map(|(id, _)| *id) else {
            return;
        };
        let Some(doc) = self.docs.get_mut(&id) else {
            return;
        };
        let entries = match published {
            None => Vec::new(),
            Some(published) => {
                let Some(tag) = published.tag else {
                    // Computed for a text we no longer know: keep the mapped ones.
                    return;
                };
                let Some(ix) = doc.history.iter().rposition(|s| s.version == tag) else {
                    return;
                };
                let text = doc.history[ix].text.clone();
                let later: Vec<Arc<Transaction>> = doc
                    .history
                    .iter()
                    .skip(ix + 1)
                    .map(|s| s.deltas.clone())
                    .collect();
                let mut entries: Vec<DiagnosticEntry> = published
                    .diagnostics
                    .into_iter()
                    .map(|d| {
                        let mut range = super::range_for_lsp(&text, &d.range);
                        for tx in &later {
                            range = map_range(&range, &tx.deltas);
                        }
                        DiagnosticEntry {
                            range,
                            severity: d.severity,
                            message: d.message,
                            source: d.source,
                            code: d.code,
                            tags: d.tags,
                        }
                    })
                    .collect();
                entries.sort_by_key(|e| (e.range.start, e.severity));
                entries
            }
        };
        if doc.entries == entries {
            return;
        }
        doc.entries = entries;
        let path = doc.path.clone();
        self.refresh_views(&path, cx);
        cx.notify();
    }

    // File tabs.

    fn track_view(&mut self, view: Entity<FileView>, cx: &mut Context<Self>) {
        let id = view.entity_id();
        let code_view = view.read(cx).code_view().clone();
        let weak_cv = code_view.downgrade();
        code_view.update(cx, |v, _| {
            v.on_gutter_icon_click(move |_row, window, cx| {
                if let Some(cv) = weak_cv.upgrade() {
                    let focus = cv.focus_handle(cx);
                    window.focus(&focus);
                }
                window.dispatch_action(Box::new(CodeActions), cx);
            });
        });
        let subs = vec![
            cx.subscribe(&view, |this, view, event: &FileViewEvent, cx| match event {
                FileViewEvent::Loaded | FileViewEvent::PathChanged => {
                    this.update_view(&view, cx);
                }
                FileViewEvent::CursorMoved => this.schedule_lightbulb(&view, cx),
                _ => {}
            }),
            cx.observe_release(&view, move |this, _, _| {
                this.views.remove(&id);
            }),
        ];
        self.views.insert(
            id,
            ViewState {
                view: view.downgrade(),
                lightbulb: None,
                lightbulb_task: None,
                _subscriptions: subs,
            },
        );
        self.update_view(&view, cx);
    }

    fn refresh_views(&mut self, path: &Path, cx: &mut Context<Self>) {
        let views: Vec<Entity<FileView>> = self
            .views
            .values()
            .filter_map(|v| v.view.upgrade())
            .filter(|v| v.read(cx).path() == path)
            .collect();
        for view in views {
            self.update_view(&view, cx);
        }
    }

    /// Recomputes the squiggles / icons / lightbulb of one tab.
    fn update_view(&mut self, view: &Entity<FileView>, cx: &mut Context<Self>) {
        let marks = self.marks_for(view, cx);
        let code_view = view.read(cx).code_view().clone();
        code_view.update(cx, |v, cx| v.set_inline_marks(marks, cx));
    }

    fn marks_for(&self, view: &Entity<FileView>, cx: &App) -> InlineMarks {
        let v = view.read(cx);
        let model = v.model().read(cx);
        let Some(text) = model.buffer().map(|b| b.read(cx).text().clone()) else {
            return InlineMarks::default();
        };
        let entries = self.entries(v.path());
        let mut marks = InlineMarks::default();
        let mut row_severity: HashMap<u32, DiagnosticSeverity> = HashMap::new();
        for e in entries {
            let start = text.offset_to_point(e.range.start.min(text.len()));
            let end = text.offset_to_point(e.range.end.min(text.len()));
            let color = severity_color(e.severity, cx);
            let wavy = matches!(
                e.severity,
                DiagnosticSeverity::Error | DiagnosticSeverity::Warning
            );
            // Hints (often "unused" code) stay subtle.
            let color = if e.severity == DiagnosticSeverity::Hint {
                color.opacity(0.45)
            } else {
                color
            };
            for row in start.row..=end.row.min(start.row + 200) {
                let from = if row == start.row {
                    start.column as usize
                } else {
                    0
                };
                let to = if row == end.row {
                    end.column as usize
                } else {
                    text.line_len(row) as usize
                };
                if row != start.row && to == 0 {
                    continue;
                }
                marks.underlines.push(Underline {
                    row,
                    range: from..to.max(from),
                    color,
                    wavy,
                });
            }
            if e.severity != DiagnosticSeverity::Hint {
                let s = row_severity.entry(start.row).or_insert(e.severity);
                *s = (*s).min(e.severity);
                if e.severity <= DiagnosticSeverity::Warning {
                    marks.scrollbar.push((start.row, color));
                }
            }
        }
        marks.underlines.sort_by_key(|u| u.row);
        let mut icons: Vec<GutterIcon> = row_severity
            .into_iter()
            .map(|(row, s)| GutterIcon {
                row,
                icon: severity_icon(s),
                color: severity_color(s, cx),
                clickable: false,
            })
            .collect();
        icons.sort_by_key(|i| i.row);
        if let Some(row) = self.views.get(&view.entity_id()).and_then(|s| s.lightbulb) {
            icons.push(GutterIcon {
                row,
                icon: IconName::Lightbulb,
                color: cx.theme().ui.warning.hsla(),
                clickable: true,
            });
        }
        marks.gutter_icons = icons;
        marks
    }

    // Lightbulb.

    fn schedule_lightbulb(&mut self, view: &Entity<FileView>, cx: &mut Context<Self>) {
        let id = view.entity_id();
        let Some(state) = self.views.get_mut(&id) else {
            return;
        };
        let caret_row = view.read(cx).code_view().read(cx).cursor().row;
        if state.lightbulb.is_some_and(|r| r != caret_row) {
            state.lightbulb = None;
            self.update_view(view, cx);
        }
        let Some(state) = self.views.get_mut(&id) else {
            return;
        };
        let weak = view.downgrade();
        state.lightbulb_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(LIGHTBULB_DELAY_MS))
                .await;
            let Ok(Some((request, row, version))) = cx.update(|cx| {
                let view = weak.upgrade()?;
                super::code_actions::lightbulb_request(&view, cx)
            }) else {
                return;
            };
            let has_actions = request
                .await
                .is_ok_and(|actions| actions.iter().any(|a| a.disabled_reason.is_none()));
            let _ = this.update(cx, |this, cx| {
                let Some(view) = weak.upgrade() else { return };
                let v = view.read(cx);
                let still = v.code_view().read(cx).cursor().row == row
                    && v.model()
                        .read(cx)
                        .buffer()
                        .is_some_and(|b| b.read(cx).text().version() == version);
                let row = (has_actions && still).then_some(row);
                if let Some(state) = this.views.get_mut(&view.entity_id()) {
                    if state.lightbulb != row {
                        state.lightbulb = row;
                        this.update_view(&view, cx);
                    }
                }
            });
        }));
    }
}

/// Markdown for the diagnostics at `offset` of `path` (hover), most severe
/// first.
pub fn hover_markdown(path: &Path, offset: usize, cx: &App) -> Option<String> {
    let store = DiagnosticsStore::try_global(cx)?;
    let store = store.read(cx);
    let mut hits: Vec<&DiagnosticEntry> = store
        .entries(path)
        .iter()
        .filter(|e| e.range.start <= offset && offset <= e.range.end.max(e.range.start + 1))
        .collect();
    if hits.is_empty() {
        return None;
    }
    hits.sort_by_key(|e| e.severity);
    let parts: Vec<String> = hits
        .iter()
        .map(|e| {
            let origin = e.origin().map(|o| format!(" `{o}`")).unwrap_or_default();
            let message = e.message.trim().replace('\n', "  \n");
            format!("**{}**{origin}: {message}", severity_label(e.severity))
        })
        .collect();
    Some(parts.join("\n\n"))
}

// Navigation (F8 / shift-F8).

pub(crate) fn next_diagnostic(
    ws: &mut Workspace,
    _: &NextDiagnostic,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    jump(ws, true, window, cx);
}

pub(crate) fn prev_diagnostic(
    ws: &mut Workspace,
    _: &PrevDiagnostic,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    jump(ws, false, window, cx);
}

/// Index of the diagnostic to jump to from `caret` (wrapping).
pub fn target_index(entries: &[DiagnosticEntry], caret: usize, forward: bool) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    if forward {
        Some(
            entries
                .iter()
                .position(|e| e.range.start > caret)
                .unwrap_or(0),
        )
    } else {
        Some(
            entries
                .iter()
                .rposition(|e| e.range.start < caret)
                .unwrap_or(entries.len() - 1),
        )
    }
}

fn jump(ws: &mut Workspace, forward: bool, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(view) = ws.active_file_view(cx) else {
        cx.propagate();
        return;
    };
    let path = view.read(cx).path().to_path_buf();
    let store = DiagnosticsStore::global(cx);
    let entries = store.read(cx).entries(&path).to_vec();
    let Some(caret) = view.read(cx).cursor_offset(cx) else {
        return;
    };
    let Some(ix) = target_index(&entries, caret, forward) else {
        ws.show_toast("No problems in this file", cx);
        return;
    };
    let Some(text) = view
        .read(cx)
        .model()
        .read(cx)
        .buffer()
        .map(|b| b.read(cx).text().clone())
    else {
        return;
    };
    let p = text.offset_to_point(entries[ix].range.start.min(text.len()));
    view.update(cx, |v, cx| {
        v.reveal(
            Reveal {
                row: p.row,
                cols: p.column as usize..p.column as usize,
                center: true,
            },
            cx,
        )
    });
    let focus = view.read(cx).code_view().focus_handle(cx);
    window.focus(&focus);
    // Show the message once the view has laid out at the new position.
    cx.spawn_in(window, async move |_, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(60))
            .await;
        let _ = cx.update(|window, cx| window.dispatch_action(Box::new(Hover), cx));
    })
    .detach();
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
    fn ranges_move_with_edits() {
        // Insert 2 bytes before the range.
        assert_eq!(map_range(&(5..8), &[delta(1, 1, 3)]), 7..10);
        // Insert at the start: the range moves; at the end: it does not grow.
        assert_eq!(map_range(&(5..8), &[delta(5, 5, 6)]), 6..9);
        assert_eq!(map_range(&(5..8), &[delta(8, 8, 10)]), 5..8);
        // Typing inside grows the range.
        assert_eq!(map_range(&(5..8), &[delta(6, 6, 7)]), 5..9);
        // Deleting the whole range collapses it at the edit.
        assert_eq!(map_range(&(5..8), &[delta(4, 9, 4)]), 4..4);
        // Edits after are ignored; several deltas apply in order.
        assert_eq!(
            map_range(&(5..8), &[delta(20, 22, 20), delta(0, 2, 0)]),
            3..6
        );
    }

    fn entry(start: usize) -> DiagnosticEntry {
        DiagnosticEntry {
            range: start..start + 1,
            severity: DiagnosticSeverity::Error,
            message: String::new(),
            source: None,
            code: None,
            tags: vec![],
        }
    }

    #[test]
    fn navigation_wraps() {
        let e = vec![entry(2), entry(10), entry(20)];
        assert_eq!(target_index(&e, 0, true), Some(0));
        assert_eq!(target_index(&e, 2, true), Some(1));
        assert_eq!(target_index(&e, 25, true), Some(0));
        assert_eq!(target_index(&e, 10, false), Some(0));
        assert_eq!(target_index(&e, 1, false), Some(2));
        assert_eq!(target_index(&[], 1, false), None);
    }
}
