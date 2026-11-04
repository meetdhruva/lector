//! [`DocumentModel`]: the open, possibly edited state of one file, shared by
//! every tab showing it.
//!
//! It owns the [`EditBuffer`] (when the file is editable), the current
//! [`Document`] snapshot built from the buffer text, the highlight state and
//! everything derived from edits:
//!
//! * after each edit the snapshot and [`DocumentSource`] are rebuilt
//!   synchronously (cheap: rope clones), the previous highlights are kept as a
//!   stale layer, and the syntax tree is re-parsed incrementally in the
//!   background (`Syntax::edit_and_reparse`), coalescing edits made meanwhile;
//! * the settled snapshot (outline, folds, breadcrumbs, sticky headers) is
//!   recomputed 300 ms after typing stops, the diff gutter 300 ms after, and
//!   the language server gets the new text after 150 ms;
//! * saving follows the write policy: before-save hooks, optional trimming,
//!   encoding/BOM/line endings kept, a "changed on disk" check, then an atomic
//!   write;
//! * external changes reload a clean buffer in place and raise a conflict for
//!   a dirty one.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Global, SharedString, Task, WeakEntity,
};
use lector_buffer::save::{Encoding, LineEnding as SaveLineEnding, SaveOptions};
use lector_buffer::{
    Buffer, BufferOptions, CommentTokens, EditDelta, IndentStyle, TextEdit, Transaction,
};
use lector_fs::{FileKind, FileStamp};
use lector_syntax::{Syntax, SyntaxEdit};
use lector_text::Text;
use lector_workspace::{Document, Indentation, LineEnding, Project};

use super::source::{convert_gutter, DocumentSource, SourceState, HIGHLIGHT_CHUNK};
use crate::code_view::{EditBuffer, EditBufferEvent};
use crate::settings::AppSettings;

/// Debounce for outline / folds / breadcrumbs / sticky headers after edits.
pub const SETTLE_DELAY_MS: u64 = 300;
/// Debounce for the diff gutter after edits.
pub const GUTTER_DELAY_MS: u64 = 300;
/// Debounce for sending the new text to the language server.
pub const LSP_SYNC_DELAY_MS: u64 = 150;

/// Events emitted by a [`DocumentModel`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentEvent {
    /// First load finished (or failed: see [`DocumentModel::load_error`]).
    Loaded,
    /// The source changed (edit, reparse, reload): views take
    /// [`DocumentModel::source`].
    SourceChanged,
    /// Highlights or gutter marks arrived: views repaint.
    Repaint,
    /// The settled snapshot (outline, folds) was recomputed.
    Settled,
    /// The dirty flag flipped.
    DirtyChanged,
    /// The file was written.
    Saved,
    /// A "changed on disk" conflict appeared or was resolved.
    ConflictChanged,
    /// The document now lives at another path ("Save As").
    PathChanged,
}

/// Result of a save attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written to disk.
    Saved,
    /// Nothing to write (clean, or not editable).
    Unchanged,
    /// The file changed on disk since it was loaded; nothing was written.
    Conflict,
}

/// A file change on disk that conflicts with unsaved edits.
#[derive(Clone)]
pub struct DiskConflict {
    /// The file as it is on disk now.
    pub disk: Document,
    /// Its stamp.
    pub stamp: Option<FileStamp>,
}

/// A before-save hook: given the document about to be saved, returns edits
/// to apply first (e.g. LSP formatting), or `None` to skip. Edits are
/// offsets into the text at the time the hook was called; they are applied
/// only if the buffer did not change meanwhile.
pub type BeforeSaveHook =
    Rc<dyn Fn(&Entity<DocumentModel>, &mut App) -> Option<Task<anyhow::Result<Vec<TextEdit>>>>>;

/// Registered before-save hooks (run in registration order).
#[derive(Default, Clone)]
pub struct BeforeSaveHooks(pub Vec<BeforeSaveHook>);

impl Global for BeforeSaveHooks {}

/// Registers a hook that runs before every save (format-on-save, ...).
/// See [`BeforeSaveHook`].
pub fn register_before_save(
    cx: &mut App,
    hook: impl Fn(&Entity<DocumentModel>, &mut App) -> Option<Task<anyhow::Result<Vec<TextEdit>>>>
        + 'static,
) {
    cx.default_global::<BeforeSaveHooks>().0.push(Rc::new(hook));
}

/// Open documents by path (one model per file, shared by its tabs).
#[derive(Default)]
struct OpenDocuments(HashMap<PathBuf, WeakEntity<DocumentModel>>);

impl Global for OpenDocuments {}

/// All live document models (for "Save All" and quit prompts).
pub fn open_documents(cx: &App) -> Vec<Entity<DocumentModel>> {
    cx.try_global::<OpenDocuments>()
        .map(|o| o.0.values().filter_map(|w| w.upgrade()).collect())
        .unwrap_or_default()
}

/// The live model of `path`, if open.
pub fn document_for_path(path: &Path, cx: &App) -> Option<Entity<DocumentModel>> {
    cx.try_global::<OpenDocuments>()?
        .0
        .get(path)
        .and_then(|w| w.upgrade())
}

/// The open, possibly edited state of one file.
pub struct DocumentModel {
    path: PathBuf,
    project: Project,
    loaded: bool,
    load_error: Option<SharedString>,
    /// Latest snapshot (its text is the buffer text; syntax may be pending).
    doc: Option<Document>,
    /// Latest snapshot whose outline and folds are computed.
    settled: Option<Document>,
    buffer: Option<Entity<EditBuffer>>,
    editable: bool,
    state: Rc<SourceState>,
    source: Option<Rc<DocumentSource>>,
    requests: mpsc::UnboundedSender<u32>,
    folds: Arc<Vec<Range<u32>>>,
    // Incremental parsing.
    parsed: Option<(Syntax, u64)>,
    pending_deltas: Vec<EditDelta>,
    parse_task: Option<Task<()>>,
    adopt: Option<(Document, Option<FileStamp>)>,
    // Debounced work.
    settle_task: Option<Task<()>>,
    gutter_task: Option<Task<()>>,
    gutter_base: Option<Arc<Option<String>>>,
    lsp_task: Option<Task<()>>,
    autosave_task: Option<Task<()>>,
    _highlight_task: Task<()>,
    load_task: Option<Task<()>>,
    reload_pending: bool,
    // Disk state.
    stamp: Option<FileStamp>,
    encoding: Encoding,
    conflict: Option<DiskConflict>,
    was_dirty: bool,
    save_count: u64,
    views: usize,
    _buffer_subscription: Option<gpui::Subscription>,
}

impl EventEmitter<DocumentEvent> for DocumentModel {}

/// What the background load produced.
struct Loaded {
    doc: Document,
    chunks: Vec<(u32, Arc<Vec<lector_workspace::LineHighlights>>)>,
    stamp: Option<FileStamp>,
    bom: bool,
}

fn load_blocking(project: &Project, path: &Path, target_row: u32) -> anyhow::Result<Loaded> {
    // Stamp before reading: if the file changes in between we would rather
    // report a (false) conflict than silently overwrite newer content.
    let stamp = lector_fs::stamp(path).ok();
    let doc = project.load_document(path)?;
    let _ = doc.outline();
    let _ = doc.fold_ranges();
    let mut chunks = Vec::new();
    if doc.syntax().is_some() {
        let first = target_row.saturating_sub(40) / HIGHLIGHT_CHUNK;
        let last = (target_row + 120) / HIGHLIGHT_CHUNK;
        for chunk in first..=last {
            let rows = chunk * HIGHLIGHT_CHUNK..(chunk + 1) * HIGHLIGHT_CHUNK;
            if rows.start >= doc.line_count() {
                break;
            }
            chunks.push((chunk, Arc::new(doc.line_highlights(rows))));
        }
    }
    let bom = doc.encoding() == lector_fs::UTF_8 && {
        use std::io::Read;
        let mut head = [0u8; 3];
        std::fs::File::open(path)
            .and_then(|mut f| f.read_exact(&mut head))
            .is_ok()
            && head == [0xEF, 0xBB, 0xBF]
    };
    Ok(Loaded {
        doc,
        chunks,
        stamp,
        bom,
    })
}

fn syntax_edits(deltas: &[EditDelta]) -> Vec<SyntaxEdit> {
    deltas
        .iter()
        .map(|d| SyntaxEdit {
            start_byte: d.start_byte,
            old_end_byte: d.old_end_byte,
            new_end_byte: d.new_end_byte,
            start: d.start,
            old_end: d.old_end,
            new_end: d.new_end,
        })
        .collect()
}

/// Buffer options for `doc` from the settings.
pub fn buffer_options(doc: &Document, cx: &App) -> BufferOptions {
    let settings = AppSettings::get(cx);
    let tab_size = settings.tab_size.max(1);
    let detected = match doc.indentation() {
        Indentation::Tabs => IndentStyle::Tabs,
        Indentation::Spaces(n) => IndentStyle::Spaces(n.max(1)),
    };
    let indent = match settings.editor.insert_spaces {
        None => detected,
        Some(true) => match detected {
            IndentStyle::Spaces(n) => IndentStyle::Spaces(n),
            IndentStyle::Tabs => IndentStyle::Spaces(tab_size),
        },
        Some(false) => IndentStyle::Tabs,
    };
    let comment = doc
        .language()
        .map(|l| CommentTokens {
            line: l.line_comment.clone(),
            block: l.block_comment.clone(),
        })
        .unwrap_or_default();
    BufferOptions {
        tab_size,
        indent,
        auto_indent: settings.editor.auto_indent,
        auto_close_brackets: settings.editor.auto_close_brackets,
        comment,
    }
}

/// Whether a loaded document can be edited and saved losslessly.
pub fn is_editable_document(doc: &Document) -> bool {
    matches!(doc.kind(), FileKind::Text)
        && doc.text().is_some_and(|t| !t.is_mapped())
        && !doc.lossy()
        && matches!(doc.encoding(), "UTF-8" | "UTF-16LE" | "UTF-16BE")
}

impl DocumentModel {
    /// The model for `path`, shared with other tabs of the same file
    /// (loaded in the background when new; `target_row` gets highlighted
    /// first).
    pub fn open(path: PathBuf, project: Project, target_row: u32, cx: &mut App) -> Entity<Self> {
        if let Some(existing) = document_for_path(&path, cx) {
            return existing;
        }
        let model = cx.new(|cx| Self::new(path.clone(), project, target_row, cx));
        cx.default_global::<OpenDocuments>()
            .0
            .insert(path, model.downgrade());
        model
    }

    fn new(path: PathBuf, project: Project, target_row: u32, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = mpsc::unbounded();
        let highlight_task = Self::spawn_highlighter(rx, cx);
        cx.on_release(|this: &mut Self, cx| {
            if let Some(docs) = cx.try_global::<OpenDocuments>() {
                let stale = docs
                    .0
                    .get(&this.path)
                    .is_some_and(|w| w.upgrade().is_none());
                if stale {
                    cx.global_mut::<OpenDocuments>().0.remove(&this.path);
                }
            }
            if let Some(lsp) = this.project.lsp() {
                lsp.close_document(&this.path);
            }
        })
        .detach();
        let mut model = DocumentModel {
            path,
            project,
            loaded: false,
            load_error: None,
            doc: None,
            settled: None,
            buffer: None,
            editable: false,
            state: Rc::new(SourceState::default()),
            source: None,
            requests: tx,
            folds: Arc::new(Vec::new()),
            parsed: None,
            pending_deltas: Vec::new(),
            parse_task: None,
            adopt: None,
            settle_task: None,
            gutter_task: None,
            gutter_base: None,
            lsp_task: None,
            autosave_task: None,
            _highlight_task: highlight_task,
            load_task: None,
            reload_pending: false,
            stamp: None,
            encoding: Encoding::Utf8,
            conflict: None,
            was_dirty: false,
            save_count: 0,
            views: 0,
            _buffer_subscription: None,
        };
        model.load(target_row, cx);
        model
    }

    // Accessors.

    /// Absolute path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The project.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Whether the first load finished.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Why loading failed, if it did.
    pub fn load_error(&self) -> Option<&SharedString> {
        self.load_error.as_ref()
    }

    /// The latest snapshot (text always current; syntax may be pending).
    pub fn live_document(&self) -> Option<&Document> {
        self.doc.as_ref()
    }

    /// The settled snapshot: outline and folds computed, at most a few
    /// hundred milliseconds behind typing.
    pub fn settled_document(&self) -> Option<&Document> {
        self.settled.as_ref().or(self.doc.as_ref())
    }

    /// The editing buffer (editable text files only).
    pub fn buffer(&self) -> Option<&Entity<EditBuffer>> {
        self.buffer.as_ref()
    }

    /// Whether the file can be edited (text, not memory-mapped, lossless encoding).
    pub fn is_editable(&self) -> bool {
        self.editable && self.buffer.is_some()
    }

    /// Whether there are unsaved changes.
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.buffer.as_ref().is_some_and(|b| b.read(cx).is_dirty())
    }

    /// The current code source for views.
    pub fn source(&self) -> Option<Rc<DocumentSource>> {
        self.source.clone()
    }

    /// Shared highlight / gutter state.
    pub fn state(&self) -> &Rc<SourceState> {
        &self.state
    }

    /// A pending "changed on disk" conflict.
    pub fn conflict(&self) -> Option<&DiskConflict> {
        self.conflict.as_ref()
    }

    /// The encoding used when saving.
    pub fn save_encoding(&self) -> Encoding {
        self.encoding
    }

    /// Removes this document from the open-documents registry (its last tab
    /// closed): the next open of the path loads it fresh.
    pub fn unregister(&mut self, cx: &mut Context<Self>) {
        let me = cx.entity().downgrade();
        if let Some(docs) = cx.try_global::<OpenDocuments>() {
            if docs
                .0
                .get(&self.path)
                .is_some_and(|w| w.entity_id() == me.entity_id())
            {
                cx.global_mut::<OpenDocuments>().0.remove(&self.path);
            }
        }
        self.autosave_task = None;
    }

    /// Number of tabs showing this document (maintained by panes).
    pub fn view_count(&self) -> usize {
        self.views
    }

    /// A tab started showing this document.
    pub fn add_view(&mut self) {
        self.views += 1;
    }

    /// A tab stopped showing this document.
    pub fn remove_view(&mut self) {
        self.views = self.views.saturating_sub(1);
    }

    /// Number of successful saves (tests).
    pub fn save_count(&self) -> u64 {
        self.save_count
    }

    /// Applies external edits (LSP format, rename, code action) to the
    /// buffer as one undo step. Offsets refer to the current text.
    pub fn apply_edits(&mut self, edits: Vec<TextEdit>, cx: &mut Context<Self>) -> bool {
        let Some(buffer) = self.buffer.clone() else {
            return false;
        };
        let tx = buffer.update(cx, |b, cx| b.apply_edits(edits, cx));
        !tx.deltas.is_empty()
    }

    // Loading.

    fn load(&mut self, target_row: u32, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let path = self.path.clone();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { load_blocking(&project, &path, target_row) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match loaded {
                    Ok(loaded) => this.loaded(loaded, cx),
                    Err(err) => {
                        this.loaded = true;
                        this.load_error = Some(err.to_string().into());
                        cx.emit(DocumentEvent::Loaded);
                    }
                }
            });
        }));
    }

    fn loaded(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        let Loaded {
            doc,
            chunks,
            stamp,
            bom,
        } = loaded;
        self.stamp = stamp;
        self.encoding = match doc.encoding() {
            "UTF-16LE" => Encoding::Utf16Le,
            "UTF-16BE" => Encoding::Utf16Be,
            _ if bom => Encoding::Utf8Bom,
            _ => Encoding::Utf8,
        };
        let version = doc.text().map_or(0, |t| t.version());
        self.state.reset(version);
        for (chunk, lines) in chunks {
            self.state.insert_chunk(chunk, lines);
        }
        self.editable = is_editable_document(&doc);
        if self.editable {
            if let Some(rope) = doc.text().and_then(|t| t.as_rope()).cloned() {
                let options = buffer_options(&doc, cx);
                let buffer = cx.new(|_| EditBuffer::new(Buffer::new(rope, options)));
                self._buffer_subscription = Some(cx.subscribe(
                    &buffer,
                    |this, _, event: &EditBufferEvent, cx| {
                        let EditBufferEvent::Edited { transaction, .. } = event;
                        this.buffer_edited(transaction, cx);
                    },
                ));
                self.buffer = Some(buffer);
            }
        }
        self.parsed = doc.syntax().cloned().map(|s| (s, version));
        self.folds = doc.fold_ranges();
        self.settled = Some(doc.clone());
        self.doc = Some(doc);
        self.loaded = true;
        self.rebuild_source();
        self.refresh_gutter(cx);
        self.sync_lsp(cx, Duration::ZERO);
        cx.emit(DocumentEvent::Loaded);
        cx.emit(DocumentEvent::SourceChanged);
        cx.notify();
    }

    fn rebuild_source(&mut self) {
        let Some(doc) = self.doc.clone() else { return };
        self.source = Some(Rc::new(DocumentSource::with_parts(
            doc,
            self.settled.clone(),
            self.folds.clone(),
            self.state.clone(),
            Some(self.requests.clone()),
        )));
    }

    fn spawn_highlighter(mut rx: mpsc::UnboundedReceiver<u32>, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(chunk) = rx.try_recv() {
                    batch.push(chunk);
                }
                // Newest requests are what is visible now.
                batch.reverse();
                let Ok((doc, state)) = this.update(cx, |this, _| {
                    for chunk in batch.drain(batch.len().min(4)..) {
                        this.state.cancel_chunk(chunk);
                    }
                    (this.doc.clone(), this.state.clone())
                }) else {
                    break;
                };
                let Some(doc) = doc.filter(|d| d.syntax().is_some()) else {
                    for chunk in batch {
                        state.cancel_chunk(chunk);
                    }
                    continue;
                };
                let version = doc.text().map_or(0, |t| t.version());
                let work = batch.clone();
                let results = cx
                    .background_executor()
                    .spawn(async move {
                        work.into_iter()
                            .map(|chunk| {
                                let rows = chunk * HIGHLIGHT_CHUNK..(chunk + 1) * HIGHLIGHT_CHUNK;
                                (chunk, Arc::new(doc.line_highlights(rows)))
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
                for (chunk, lines) in results {
                    state.insert_chunk_for(version, chunk, lines);
                }
                if this
                    .update(cx, |_, cx| cx.emit(DocumentEvent::Repaint))
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    // Edits.

    fn buffer_edited(&mut self, transaction: &Transaction, cx: &mut Context<Self>) {
        let started = std::time::Instant::now();
        let Some(buffer) = self.buffer.clone() else {
            return;
        };
        let Some(old_doc) = self.doc.clone() else {
            return;
        };
        let rope = buffer.read(cx).text().clone();
        let text = Text::from(rope);
        let version = text.version();
        let deltas = &transaction.deltas;
        if let Some((disk, stamp)) = self.adopt.take() {
            // Reload: the disk version comes with a full parse.
            self.state.reset(version);
            self.parsed = disk.syntax().cloned().map(|s| (s, version));
            self.pending_deltas.clear();
            self.parse_task = None;
            let doc = disk.with_text(text, disk.syntax().cloned());
            self.folds = disk.fold_ranges();
            self.settled = Some(disk);
            self.doc = Some(doc);
            self.stamp = stamp;
            buffer.update(cx, |b, _| b.buffer_mut().mark_saved());
        } else {
            self.state.begin_edit(version, deltas);
            self.pending_deltas.extend_from_slice(deltas);
            self.folds = Arc::new(
                self.folds
                    .iter()
                    .filter_map(|r| {
                        let start = crate::code_view::edit::map_row_through(r.start, deltas);
                        let end = crate::code_view::edit::map_row_through(r.end, deltas);
                        (end > start + 1).then_some(start..end)
                    })
                    .collect(),
            );
            self.doc = Some(old_doc.with_text(text, None));
            self.schedule_reparse(cx);
        }
        self.rebuild_source();
        cx.emit(DocumentEvent::SourceChanged);
        self.schedule_settle(cx);
        self.schedule_gutter(cx);
        self.sync_lsp(cx, Duration::from_millis(LSP_SYNC_DELAY_MS));
        self.check_dirty(cx);
        self.schedule_autosave(cx);
        tracing::trace!(target: "lector_ui::perf", elapsed_us = started.elapsed().as_micros() as u64, "document edit sync");
    }

    fn check_dirty(&mut self, cx: &mut Context<Self>) {
        let dirty = self.is_dirty(cx);
        if dirty != self.was_dirty {
            self.was_dirty = dirty;
            cx.emit(DocumentEvent::DirtyChanged);
        }
        cx.notify();
    }

    fn current_version(&self) -> u64 {
        self.doc
            .as_ref()
            .and_then(|d| d.text())
            .map_or(0, |t| t.version())
    }

    fn schedule_reparse(&mut self, cx: &mut Context<Self>) {
        if self.parse_task.is_some() || self.pending_deltas.is_empty() {
            return;
        }
        let Some((old, _)) = self.parsed.clone() else {
            self.pending_deltas.clear();
            return;
        };
        let Some(text) = self.doc.as_ref().and_then(|d| d.text()).cloned() else {
            return;
        };
        let version = text.version();
        let edits = syntax_edits(&std::mem::take(&mut self.pending_deltas));
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let parsed = cx
                .background_executor()
                .spawn(async move {
                    let started = std::time::Instant::now();
                    let result = old
                        .edit_and_reparse(&text, &edits)
                        .or_else(|_| Syntax::parse(old.language(), &text));
                    tracing::trace!(target: "lector_ui::perf", elapsed_us = started.elapsed().as_micros() as u64, "incremental reparse");
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.parse_task = None;
                let Ok(syntax) = parsed else {
                    // Too large now: no more syntax for this buffer.
                    this.parsed = None;
                    this.pending_deltas.clear();
                    return;
                };
                this.parsed = Some((syntax.clone(), version));
                if this.current_version() == version {
                    if let Some(doc) = &this.doc {
                        let text = doc.text().cloned();
                        if let Some(text) = text {
                            this.doc = Some(doc.with_text(text, Some(syntax)));
                            this.rebuild_source();
                            cx.emit(DocumentEvent::SourceChanged);
                        }
                    }
                } else {
                    this.schedule_reparse(cx);
                }
            });
        }));
    }

    fn schedule_settle(&mut self, cx: &mut Context<Self>) {
        self.settle_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SETTLE_DELAY_MS))
                .await;
            // Wait for the reparse of the newest text.
            loop {
                let Ok(ready) = this.update(cx, |this, _| {
                    this.parse_task.is_none() && this.pending_deltas.is_empty()
                }) else {
                    return;
                };
                if ready {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(20))
                    .await;
            }
            let Ok(Some(doc)) = this.update(cx, |this, _| this.doc.clone()) else {
                return;
            };
            let computed = doc.clone();
            cx.background_executor()
                .spawn(async move {
                    let _ = computed.outline();
                    let _ = computed.fold_ranges();
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Install even if the text moved on meanwhile: the settled
                // snapshot may lag; the next settle catches up.
                if doc.syntax().is_some() {
                    this.folds = doc.fold_ranges();
                }
                this.settled = Some(doc);
                this.rebuild_source();
                cx.emit(DocumentEvent::SourceChanged);
                cx.emit(DocumentEvent::Settled);
            });
        }));
    }

    fn schedule_gutter(&mut self, cx: &mut Context<Self>) {
        if !AppSettings::get(cx).git.show_gutter {
            return;
        }
        self.gutter_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(GUTTER_DELAY_MS))
                .await;
            let _ = this.update(cx, |this, cx| this.compute_gutter(cx));
        }));
    }

    /// Recomputes the diff gutter vs HEAD (re-reading HEAD) in the background.
    pub fn refresh_gutter(&mut self, cx: &mut Context<Self>) {
        self.gutter_base = None;
        self.compute_gutter(cx);
    }

    fn compute_gutter(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.clone() else { return };
        if !AppSettings::get(cx).git.show_gutter || doc.text().is_none() {
            self.state.set_gutter(Vec::new());
            return;
        }
        let project = self.project.clone();
        let base = self.gutter_base.clone();
        let state = self.state.clone();
        let version = self.current_version();
        self.gutter_task = Some(cx.spawn(async move |this, cx| {
            let (base, marks) = cx
                .background_executor()
                .spawn(async move {
                    let base = base.unwrap_or_else(|| Arc::new(doc.gutter_base(&project)));
                    let marks = match base.as_ref() {
                        Some(b) => convert_gutter(&doc.gutter_against(b)),
                        None => Vec::new(),
                    };
                    (base, marks)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.gutter_base = Some(base);
                if this.current_version() == version {
                    state.set_gutter(marks);
                    cx.emit(DocumentEvent::Repaint);
                }
            });
        }));
    }

    fn sync_lsp(&mut self, cx: &mut Context<Self>, delay: Duration) {
        let Some(lsp) = self.project.lsp().cloned() else {
            return;
        };
        let Some(doc) = self.doc.clone() else { return };
        let (Some(lang), Some(text)) = (doc.language().cloned(), doc.text().cloned()) else {
            return;
        };
        if text.is_mapped() || doc.is_large() {
            return;
        }
        let path = self.path.clone();
        self.lsp_task = Some(cx.spawn(async move |_, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            cx.background_executor()
                .spawn(async move { lsp.open_document(&path, &lang.name, &text.to_string()) })
                .await;
        }));
    }

    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx);
        if settings.editor.auto_save != lector_config::AutoSave::AfterDelay {
            self.autosave_task = None;
            return;
        }
        let delay = Duration::from_millis(settings.editor.auto_save_delay_ms.max(100));
        self.autosave_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let Ok(task) = this.update(cx, |this, cx| {
                // Never auto-save over a file that changed on disk.
                (this.is_dirty(cx) && this.conflict.is_none()).then(|| this.save(false, cx))
            }) else {
                return;
            };
            if let Some(task) = task {
                let _ = task.await;
            }
        }));
    }

    // Saving.

    fn save_options(&self, cx: &App) -> SaveOptions {
        let line_ending = match self.doc.as_ref().map(|d| d.line_ending()) {
            Some(LineEnding::CrLf) => Some(SaveLineEnding::CrLf),
            Some(LineEnding::Lf) => Some(SaveLineEnding::Lf),
            _ => None,
        };
        let _ = cx;
        SaveOptions {
            encoding: self.encoding,
            line_ending,
            // Applied to the buffer before encoding (so buffer == disk).
            trim_trailing_whitespace: false,
            ensure_final_newline: false,
        }
    }

    /// Edits applying `trim_trailing_whitespace` / `ensure_final_newline`.
    fn cleanup_edits(&self, cx: &App) -> Vec<TextEdit> {
        let settings = AppSettings::get(cx);
        let Some(buffer) = &self.buffer else {
            return Vec::new();
        };
        let text = buffer.read(cx).text().clone();
        let mut edits = Vec::new();
        if settings.editor.trim_trailing_whitespace {
            for row in 0..text.line_count() {
                let range = text.line_range(row);
                let line = text.text_for_range(range.clone());
                let trimmed = line.trim_end_matches([' ', '\t']).len();
                if trimmed < line.len() {
                    edits.push(TextEdit {
                        range: range.start + trimmed..range.end,
                        new_text: String::new(),
                    });
                }
            }
        }
        if settings.editor.ensure_final_newline && !text.is_empty() {
            let len = text.len();
            let last_line = text.line_range(text.line_count() - 1);
            let ends_with_newline = last_line.is_empty() && text.line_count() > 1;
            if !ends_with_newline {
                let nl = match self.doc.as_ref().map(|d| d.line_ending()) {
                    Some(LineEnding::CrLf) => "\r\n",
                    _ => "\n",
                };
                edits.push(TextEdit {
                    range: len..len,
                    new_text: nl.to_string(),
                });
            } else {
                // Collapse extra trailing blank lines into one newline.
                let mut row = text.line_count() - 1;
                while row > 0 && text.line_range(row - 1).is_empty() && row > 1 {
                    row -= 1;
                }
                let keep_end = text.line_range(row.saturating_sub(1)).end;
                let nl_len = if text.byte_at(keep_end) == Some(b'\r') {
                    2
                } else {
                    1
                };
                let from = keep_end + nl_len;
                if from < len && row + 1 < text.line_count() {
                    edits.push(TextEdit {
                        range: from..len,
                        new_text: String::new(),
                    });
                }
            }
        }
        edits
    }

    /// Saves the buffer: before-save hooks, cleanup, conflict check (unless
    /// `force`), atomic write. Never prompts — callers handle
    /// [`SaveOutcome::Conflict`].
    pub fn save(
        &mut self,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<SaveOutcome>> {
        let path = self.path.clone();
        self.save_to(path, force, cx)
    }

    fn save_to(
        &mut self,
        path: PathBuf,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<SaveOutcome>> {
        let Some(buffer) = self.buffer.clone() else {
            return Task::ready(Ok(SaveOutcome::Unchanged));
        };
        let same_path = path == self.path;
        let hooks = cx
            .try_global::<BeforeSaveHooks>()
            .cloned()
            .unwrap_or_default();
        let this_entity = cx.entity();
        cx.spawn(async move |this, cx| {
            // 1. Before-save hooks (format on save, ...).
            for hook in hooks.0.iter() {
                let Ok((task, version)) = cx.update(|cx| {
                    let version = buffer.read(cx).buffer().version();
                    (hook(&this_entity, cx), version)
                }) else {
                    return Ok(SaveOutcome::Unchanged);
                };
                let Some(task) = task else { continue };
                match task.await {
                    Ok(edits) if !edits.is_empty() => {
                        this.update(cx, |this, cx| {
                            if buffer.read(cx).buffer().version() == version {
                                this.apply_edits(edits, cx);
                            }
                        })?;
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!("before-save hook failed: {err:#}"),
                }
            }
            // 2. Cleanup edits, encode.
            let (bytes, version, expected, opts_path) = this.update(cx, |this, cx| {
                let edits = this.cleanup_edits(cx);
                if !edits.is_empty() {
                    this.apply_edits(edits, cx);
                }
                let opts = this.save_options(cx);
                let b = buffer.read(cx).buffer();
                let bytes = lector_buffer::save::encode(b.text(), &opts);
                (bytes, b.version(), this.stamp, path.clone())
            })?;
            let dirty = this.update(cx, |this, cx| this.is_dirty(cx))?;
            if same_path && !dirty && !force && expected.is_some() {
                return Ok(SaveOutcome::Unchanged);
            }
            // 3. Conflict check + atomic write, off the main thread.
            let result = cx
                .background_executor()
                .spawn(async move {
                    if same_path && !force {
                        if let Some(expected) = expected {
                            match lector_fs::stamp(&opts_path) {
                                Ok(now) if now != expected => return Ok(None),
                                _ => {}
                            }
                        }
                    }
                    lector_fs::write_atomic(&opts_path, &bytes)?;
                    anyhow::Ok(Some(lector_fs::stamp(&opts_path).ok()))
                })
                .await?;
            let Some(new_stamp) = result else {
                return Ok(SaveOutcome::Conflict);
            };
            this.update(cx, |this, cx| {
                this.stamp = new_stamp;
                this.conflict = None;
                this.save_count += 1;
                buffer.update(cx, |b, _| {
                    if b.buffer().version() == version {
                        b.buffer_mut().mark_saved();
                    }
                });
                if !same_path {
                    this.set_path(path.clone(), cx);
                }
                this.refresh_gutter(cx);
                this.check_dirty(cx);
                cx.emit(DocumentEvent::Saved);
                cx.notify();
            })?;
            Ok(SaveOutcome::Saved)
        })
    }

    /// Writes the buffer to `path` and makes it this document's path.
    pub fn save_as(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<SaveOutcome>> {
        self.save_to(path, true, cx)
    }

    fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.path, path.clone());
        if let Some(lsp) = self.project.lsp() {
            lsp.close_document(&old);
        }
        let weak = cx.entity().downgrade();
        let docs = cx.default_global::<OpenDocuments>();
        docs.0.remove(&old);
        docs.0.insert(path.clone(), weak);
        let rel = self.project.relative_path(&path);
        let languages = self.project.languages().clone();
        if let Some(doc) = &self.doc {
            let doc = doc.with_path(path.clone(), rel.clone(), &languages);
            self.parsed = doc.syntax().cloned().map(|s| (s, self.current_version()));
            self.settled = Some(doc.clone());
            self.folds = doc.fold_ranges();
            self.doc = Some(doc);
            self.state.reset(self.current_version());
            self.rebuild_source();
        }
        if let (Some(buffer), Some(doc)) = (&self.buffer, self.doc.clone()) {
            let options = buffer_options(&doc, cx);
            buffer.update(cx, |b, _| b.buffer_mut().set_options(options));
        }
        self.stamp = lector_fs::stamp(&path).ok();
        self.sync_lsp(cx, Duration::ZERO);
        cx.emit(DocumentEvent::PathChanged);
        cx.emit(DocumentEvent::SourceChanged);
    }

    // External changes.

    /// The file changed on disk (watcher): reload a clean buffer in place,
    /// keeping carets; raise a conflict for a dirty one. Our own saves are
    /// recognized by their stamp and ignored.
    pub fn file_changed_on_disk(&mut self, cx: &mut Context<Self>) {
        if self.load_task.is_some() {
            self.reload_pending = true;
            return;
        }
        self.reload_from_disk(false, cx);
    }

    /// Discards unsaved changes and reloads from disk ("Revert File",
    /// "Reload" in the conflict banner).
    pub fn revert(&mut self, cx: &mut Context<Self>) {
        self.reload_from_disk(true, cx);
    }

    /// Keeps the buffer after an external change; the next save overwrites
    /// the file on disk.
    pub fn keep_mine(&mut self, cx: &mut Context<Self>) {
        if let Some(conflict) = self.conflict.take() {
            self.stamp = conflict.stamp;
            cx.emit(DocumentEvent::ConflictChanged);
            cx.notify();
        }
    }

    fn reload_from_disk(&mut self, discard: bool, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let path = self.path.clone();
        let known = self.stamp;
        let top = 0;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    if !discard {
                        if let (Some(known), Ok(now)) = (known, lector_fs::stamp(&path)) {
                            if now == known {
                                return Ok(None);
                            }
                        }
                    }
                    load_blocking(&project, &path, top).map(Some)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match loaded {
                    Ok(Some(loaded)) => this.apply_disk_version(loaded, discard, cx),
                    Ok(None) => {}
                    Err(err) => tracing::debug!("reload {}: {err:#}", this.path.display()),
                }
                if std::mem::take(&mut this.reload_pending) {
                    this.reload_from_disk(false, cx);
                }
            });
        }));
    }

    fn apply_disk_version(&mut self, loaded: Loaded, discard: bool, cx: &mut Context<Self>) {
        let Some(buffer) = self.buffer.clone() else {
            // Not editable: replace the snapshot.
            self.doc = None;
            self.loaded(loaded, cx);
            return;
        };
        let dirty = buffer.read(cx).is_dirty();
        if dirty && !discard {
            self.conflict = Some(DiskConflict {
                disk: loaded.doc,
                stamp: loaded.stamp,
            });
            cx.emit(DocumentEvent::ConflictChanged);
            cx.notify();
            return;
        }
        if !is_editable_document(&loaded.doc) {
            // Became binary / huge: reload as a fresh (read-only) snapshot.
            self.buffer = None;
            self._buffer_subscription = None;
            self.doc = None;
            self.loaded(loaded, cx);
            return;
        }
        let Some(new_text) = loaded.doc.text().map(|t| t.to_string()) else {
            return;
        };
        self.conflict = None;
        let stamp = loaded.stamp;
        let same = buffer.read(cx).text().len() == new_text.len()
            && buffer.read(cx).text().to_string() == new_text;
        if same {
            self.stamp = stamp;
            buffer.update(cx, |b, _| b.buffer_mut().mark_saved());
            self.check_dirty(cx);
            cx.emit(DocumentEvent::ConflictChanged);
            return;
        }
        self.adopt = Some((loaded.doc, stamp));
        buffer.update(cx, |b, cx| {
            b.edit(None, None, |b| ((), b.replace_all(&new_text)), cx)
        });
        cx.emit(DocumentEvent::ConflictChanged);
    }

    /// Re-applies settings (indentation, auto-close, ...) to the buffer.
    pub fn settings_changed(&mut self, cx: &mut Context<Self>) {
        if let (Some(buffer), Some(doc)) = (&self.buffer, self.doc.clone()) {
            let options = buffer_options(&doc, cx);
            buffer.update(cx, |b, _| b.buffer_mut().set_options(options));
        }
        self.refresh_gutter(cx);
    }
}
