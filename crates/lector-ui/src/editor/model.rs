//! [`DocumentModel`]: the GPUI adapter over a shared
//! [`lector_app::documents::DocHandle`], one per open path in the app and
//! shared by every tab showing it.
//!
//! The document itself — buffer, disk state (stamps, conflicts, deletion,
//! encoding), incremental reparsing, outline/folds, highlights, diff gutter,
//! LSP sync and saving — lives in the headless store (`lector-app`), where
//! agents, commands and other windows see the same text. This adapter only
//! turns the store's snapshots and events into what the views render:
//!
//! * after each edit (synchronously, for typing latency) a new `Document`
//!   snapshot and [`DocumentSource`] over the edited text, with the previous
//!   highlights kept as a stale layer until the store's reparse lands;
//! * highlight chunks requested by the views are computed by the store
//!   (newest requests first) and copied into the shared [`SourceState`];
//! * outline / folds / sticky headers (`settled` snapshot) and the diff
//!   gutter come from versioned store queries;
//! * GPUI before-save hooks (format on save) run before the store saves;
//! * the auto-save timer follows [`lector_app::documents::auto_save_decision`].

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Global, SharedString, Task, WeakEntity,
};
use lector_app::documents::{
    auto_save_decision, AutoSaveDecision, AutoSaveInputs, AutoSaveTrigger, DocError, DocEvent,
    DocHandle, DocumentStore, OpenOptions, QueryKind, SaveOutcome as StoreSaveOutcome, SaveRequest,
    Subscription,
};
use lector_buffer::save::Encoding;
use lector_buffer::{BufferOptions, TextEdit, Transaction};
use lector_text::Text;
use lector_workspace::{Document, Project};

pub use lector_app::documents::{is_editable_document, DiskConflict};

use super::doc_store::document_store;
use super::source::{convert_gutter, DocumentSource, SourceState};
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
    /// A "changed on disk" conflict appeared or was resolved, the file was
    /// deleted or came back, or the document was locked / unlocked.
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
/// See [`BeforeSaveHook`]. Headless hooks register on the store instead
/// (`DocumentStore::register_before_save`); both run.
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

/// Buffer options for `doc` from the settings.
pub fn buffer_options(doc: &Document, cx: &App) -> BufferOptions {
    lector_app::documents::buffer_options(doc, &AppSettings::get(cx))
}

/// The open, possibly edited state of one file (a view of a shared document).
pub struct DocumentModel {
    path: PathBuf,
    project: Project,
    store: DocumentStore,
    handle: Option<DocHandle>,
    loaded: bool,
    load_error: Option<SharedString>,
    /// Latest snapshot (its text is the buffer text; syntax may be pending).
    doc: Option<Document>,
    /// Latest snapshot whose outline and folds are computed.
    settled: Option<Document>,
    buffer: Option<Entity<EditBuffer>>,
    state: Rc<SourceState>,
    source: Option<Rc<DocumentSource>>,
    requests: mpsc::UnboundedSender<u32>,
    folds: Arc<Vec<Range<u32>>>,
    // Mirrored store state.
    encoding: Encoding,
    conflict: Option<DiskConflict>,
    deleted: bool,
    locked: bool,
    was_dirty: bool,
    save_count: u64,
    views: usize,
    // Tasks.
    autosave_task: Option<Task<()>>,
    load_task: Option<Task<()>>,
    reload_pending: bool,
    _highlight_task: Task<()>,
    _event_task: Option<Task<()>>,
    _doc_subscription: Option<Subscription>,
    _buffer_subscription: Option<gpui::Subscription>,
}

impl EventEmitter<DocumentEvent> for DocumentModel {}

impl DocumentModel {
    /// The model for `path`, shared with other tabs of the same file
    /// (loaded in the background when new; `target_row` gets highlighted
    /// first).
    pub fn open(path: PathBuf, project: Project, target_row: u32, cx: &mut App) -> Entity<Self> {
        if let Some(existing) = document_for_path(&path, cx) {
            return existing;
        }
        let store = document_store(cx);
        let model = cx.new(|cx| Self::new(path.clone(), project, store, target_row, cx));
        cx.default_global::<OpenDocuments>()
            .0
            .insert(path, model.downgrade());
        model
    }

    fn new(
        path: PathBuf,
        project: Project,
        store: DocumentStore,
        target_row: u32,
        cx: &mut Context<Self>,
    ) -> Self {
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
        })
        .detach();
        let mut model = DocumentModel {
            path,
            project,
            store,
            handle: None,
            loaded: false,
            load_error: None,
            doc: None,
            settled: None,
            buffer: None,
            state: Rc::new(SourceState::default()),
            source: None,
            requests: tx,
            folds: Arc::new(Vec::new()),
            encoding: Encoding::Utf8,
            conflict: None,
            deleted: false,
            locked: false,
            was_dirty: false,
            save_count: 0,
            views: 0,
            autosave_task: None,
            load_task: None,
            reload_pending: false,
            _highlight_task: highlight_task,
            _event_task: None,
            _doc_subscription: None,
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

    /// The shared document (once loaded).
    pub fn handle(&self) -> Option<&DocHandle> {
        self.handle.as_ref()
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
        self.buffer.is_some()
    }

    /// Whether the shared document is locked (`document.set_read_only`).
    pub fn is_locked(&self) -> bool {
        self.locked
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

    /// Whether the file was deleted (or moved away) on disk while this
    /// document is open. Saving writes it again.
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// The encoding used when saving.
    pub fn save_encoding(&self) -> Encoding {
        self.encoding
    }

    /// Removes this document from the open-documents registry (its last tab
    /// closed): the next open of the path loads it fresh (unless another
    /// window or an agent still holds the shared document).
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
        if let Some(handle) = &self.handle {
            // Our handle and the buffer mirror's: nobody else (another
            // window, an agent) holds the document, so forget it.
            let ours = 1 + usize::from(self.buffer.is_some());
            if handle.holders() <= ours {
                self.store.forget(handle);
            }
        }
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
        let store = self.store.clone();
        let warm = target_row.saturating_sub(40)..target_row + 120;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move {
                    store.open_with(
                        &path,
                        &project,
                        OpenOptions {
                            buffer_options: None,
                            warm_rows: Some(warm),
                        },
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                match opened {
                    Ok(handle) => this.attach(handle, cx),
                    Err(err) => {
                        this.loaded = true;
                        this.load_error = Some(err.to_string().into());
                        cx.emit(DocumentEvent::Loaded);
                    }
                }
                if std::mem::take(&mut this.reload_pending) {
                    this.file_changed_on_disk(cx);
                }
            });
        }));
    }

    /// Connects to the shared document: event bridge, then the first snapshot.
    fn attach(&mut self, handle: DocHandle, cx: &mut Context<Self>) {
        let (tx, mut rx) = mpsc::unbounded::<DocEvent>();
        self._doc_subscription = Some(handle.subscribe(move |ev| {
            let _ = tx.unbounded_send(ev.clone());
        }));
        self._event_task = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(ev) = rx.try_recv() {
                    batch.push(ev);
                }
                if this
                    .update(cx, |this, cx| {
                        for ev in batch {
                            this.doc_event(ev, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        self.handle = Some(handle);
        self.install_snapshot(cx);
    }

    /// (Re)builds everything from the store's current snapshot.
    fn install_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let snap = handle.snapshot();
        let doc = snap.document.clone();
        self.encoding = snap.disk.encoding;
        self.conflict = snap.disk.conflict.clone();
        self.deleted = snap.disk.deleted;
        self.locked = snap.read_only;
        self.save_count = snap.save_count;
        let (version, chunks) = handle.cached_highlights();
        self.state.reset(version);
        for (chunk, lines) in chunks {
            self.state.insert_chunk(chunk, lines);
        }
        let had_buffer = self.buffer.is_some();
        if snap.editable {
            if !had_buffer {
                if let Some(eb) = EditBuffer::shared(handle.clone()) {
                    let buffer = cx.new(|_| eb);
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
            handle.set_options(buffer_options(&doc, cx));
            if let Some(buffer) = &self.buffer {
                buffer.update(cx, |b, cx| b.catch_up(cx));
            }
        } else {
            self.buffer = None;
            self._buffer_subscription = None;
        }
        let settled = handle.settled_document();
        self.folds = handle
            .structure()
            .value
            .map(|s| s.folds)
            .unwrap_or_else(|| settled.fold_ranges());
        self.settled = Some(settled);
        self.doc = Some(doc);
        self.was_dirty = snap.dirty;
        self.loaded = true;
        self.rebuild_source();
        self.refresh_gutter(cx);
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
                let ok = this.update(cx, |this, _| {
                    for chunk in batch.drain(batch.len().min(4)..) {
                        this.state.cancel_chunk(chunk);
                    }
                    let requested = this
                        .handle
                        .as_ref()
                        .is_some_and(|h| h.request_highlights(&batch));
                    if !requested {
                        // No syntax yet: asked again after the reparse.
                        for chunk in &batch {
                            this.state.cancel_chunk(*chunk);
                        }
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
    }

    // Store events.

    fn doc_event(&mut self, event: DocEvent, cx: &mut Context<Self>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        match event {
            DocEvent::Edited { .. } | DocEvent::DirtyChanged { .. } => {
                if let Some(buffer) = &self.buffer {
                    buffer.update(cx, |b, cx| b.catch_up(cx));
                }
                self.check_dirty(cx);
            }
            DocEvent::Reparsed { version } => self.adopt_live(version, cx),
            DocEvent::QueryReady {
                kind: QueryKind::Structure,
                ..
            } => {
                if let Some(s) = handle.structure().value {
                    if s.document.syntax().is_some() {
                        self.folds = s.folds.clone();
                    }
                    self.settled = Some(s.document);
                    self.rebuild_source();
                    cx.emit(DocumentEvent::SourceChanged);
                    cx.emit(DocumentEvent::Settled);
                }
            }
            DocEvent::QueryReady {
                kind: QueryKind::Gutter,
                version,
            } => {
                if version == self.current_version() && AppSettings::get(cx).git.show_gutter {
                    if let Some(marks) = handle.gutter().value {
                        self.state.set_gutter(convert_gutter(&marks));
                        cx.emit(DocumentEvent::Repaint);
                    }
                }
            }
            DocEvent::QueryReady { .. } | DocEvent::DiagnosticsChanged => {}
            DocEvent::HighlightsReady { version, chunks } => {
                for (chunk, lines) in chunks {
                    self.state.insert_chunk_for(version, chunk, lines);
                }
                cx.emit(DocumentEvent::Repaint);
            }
            DocEvent::Saved { .. }
            | DocEvent::DiskStateChanged
            | DocEvent::ReadOnlyChanged { .. } => {
                self.sync_from_store(cx);
            }
            DocEvent::Reloaded { version } => {
                if let Some(buffer) = &self.buffer {
                    buffer.update(cx, |b, cx| b.catch_up(cx));
                }
                if version == self.current_version() {
                    self.state.reset(version);
                    let s = handle.structure();
                    if let Some(s) = s.value.filter(|_| s.current) {
                        self.folds = s.folds;
                        self.settled = Some(s.document);
                    }
                    self.adopt_live(version, cx);
                }
                self.sync_from_store(cx);
                self.refresh_gutter(cx);
            }
            DocEvent::Replaced => {
                self.install_snapshot(cx);
                self.sync_from_store(cx);
            }
            DocEvent::PathChanged { .. } => self.sync_from_store(cx),
        }
    }

    /// Takes the store's live document (with its syntax tree) when it is the
    /// version shown.
    fn adopt_live(&mut self, version: u64, cx: &mut Context<Self>) {
        let Some(handle) = &self.handle else { return };
        if self.current_version() != version {
            return;
        }
        let live = handle.live_document();
        if live.text().map(|t| t.version()) == Some(version) && live.syntax().is_some() {
            self.doc = Some(live);
            self.rebuild_source();
            cx.emit(DocumentEvent::SourceChanged);
        }
    }

    /// Mirrors disk state, lock, path and save count from the store.
    fn sync_from_store(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        if let Some(buffer) = &self.buffer {
            buffer.update(cx, |b, cx| b.catch_up(cx));
        }
        let snap = handle.snapshot();
        if snap.path != self.path {
            self.set_path(snap.path.clone(), cx);
        }
        let had_conflict = self.conflict.is_some();
        let state_changed = had_conflict != snap.disk.conflict.is_some()
            || self.deleted != snap.disk.deleted
            || self.locked != snap.read_only;
        self.conflict = snap.disk.conflict.clone();
        self.deleted = snap.disk.deleted;
        self.locked = snap.read_only;
        self.encoding = snap.disk.encoding;
        if state_changed {
            cx.emit(DocumentEvent::ConflictChanged);
        }
        if snap.save_count > self.save_count {
            self.save_count = snap.save_count;
            self.refresh_gutter(cx);
            cx.emit(DocumentEvent::Saved);
        }
        self.check_dirty(cx);
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
        self.state.begin_edit(version, deltas);
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
        self.rebuild_source();
        cx.emit(DocumentEvent::SourceChanged);
        if let Some(handle) = &self.handle {
            // Versioned queries: scheduled (debounced) for this version.
            let _ = handle.structure();
            if AppSettings::get(cx).git.show_gutter {
                let _ = handle.gutter();
            }
        }
        self.check_dirty(cx);
        self.schedule_autosave(AutoSaveTrigger::Edited, cx);
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

    /// Recomputes the diff gutter vs HEAD (re-reading HEAD) in the background.
    pub fn refresh_gutter(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = &self.handle else { return };
        let enabled = AppSettings::get(cx).git.show_gutter
            && self.doc.as_ref().is_some_and(|d| d.text().is_some());
        handle.set_gutter_enabled(enabled);
        if !enabled {
            self.state.set_gutter(Vec::new());
            return;
        }
        handle.refresh_gutter();
        // Already computed (inline executors): install now.
        let r = handle.gutter();
        if r.current && r.version == self.current_version() {
            if let Some(marks) = r.value {
                self.state.set_gutter(convert_gutter(&marks));
                cx.emit(DocumentEvent::Repaint);
            }
        }
    }

    fn autosave_inputs(&self, cx: &App) -> AutoSaveInputs {
        AutoSaveInputs {
            dirty: self.is_dirty(cx),
            editable: self.is_editable(),
            conflict: self.conflict.is_some(),
            deleted: self.deleted,
            read_only: self.locked,
        }
    }

    fn schedule_autosave(&mut self, trigger: AutoSaveTrigger, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx);
        let decision = auto_save_decision(
            settings.editor.auto_save,
            settings.editor.auto_save_delay_ms,
            trigger,
            self.autosave_inputs(cx),
        );
        match decision {
            AutoSaveDecision::Never => self.autosave_task = None,
            AutoSaveDecision::SaveNow => self.save(false, cx).detach(),
            AutoSaveDecision::SaveAfter(delay) => {
                self.autosave_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(delay).await;
                    let Ok(task) = this.update(cx, |this, cx| {
                        let settings = AppSettings::get(cx);
                        let decision = auto_save_decision(
                            settings.editor.auto_save,
                            settings.editor.auto_save_delay_ms,
                            AutoSaveTrigger::TimerFired,
                            this.autosave_inputs(cx),
                        );
                        (decision == AutoSaveDecision::SaveNow).then(|| this.save(false, cx))
                    }) else {
                        return;
                    };
                    if let Some(task) = task {
                        let _ = task.await;
                    }
                }));
            }
        }
    }

    /// The tab or window lost focus (`editor.auto_save = "on_focus_change"`).
    pub fn focus_lost(&mut self, cx: &mut Context<Self>) {
        self.schedule_autosave(AutoSaveTrigger::FocusLost, cx);
    }

    // Saving.

    /// Saves the buffer: before-save hooks, cleanup, conflict check (unless
    /// `force`), atomic write. Never prompts — callers handle
    /// [`SaveOutcome::Conflict`].
    pub fn save(
        &mut self,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<SaveOutcome>> {
        self.save_to(None, force, cx)
    }

    fn save_to(
        &mut self,
        path: Option<PathBuf>,
        force: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<SaveOutcome>> {
        let (Some(buffer), Some(handle)) = (self.buffer.clone(), self.handle.clone()) else {
            return Task::ready(Ok(SaveOutcome::Unchanged));
        };
        let hooks = cx
            .try_global::<BeforeSaveHooks>()
            .cloned()
            .unwrap_or_default();
        let this_entity = cx.entity();
        let editor = AppSettings::get(cx).editor.clone();
        cx.spawn(async move |this, cx| {
            // 1. GPUI before-save hooks (format on save, ...).
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
            // 2. The store: its hooks, cleanup edits, stamp check, atomic write.
            let req = SaveRequest {
                force: force || path.is_some(),
                path,
                ..SaveRequest::from_settings(&editor)
            };
            let result = cx
                .background_executor()
                .spawn(async move { handle.save(req) })
                .await;
            let outcome = match result {
                Ok(StoreSaveOutcome::Saved) => SaveOutcome::Saved,
                Ok(StoreSaveOutcome::Unchanged) => SaveOutcome::Unchanged,
                Err(DocError::ChangedOnDisk { .. }) => SaveOutcome::Conflict,
                Err(DocError::NotEditable) => SaveOutcome::Unchanged,
                Err(err) => return Err(anyhow::Error::new(err)),
            };
            this.update(cx, |this, cx| {
                this.sync_from_store(cx);
                cx.notify();
            })?;
            Ok(outcome)
        })
    }

    /// Writes the buffer to `path` and makes it this document's path.
    pub fn save_as(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<SaveOutcome>> {
        self.save_to(Some(path), true, cx)
    }

    fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.path, path.clone());
        let weak = cx.entity().downgrade();
        let docs = cx.default_global::<OpenDocuments>();
        docs.0.remove(&old);
        docs.0.insert(path, weak);
        if let Some(handle) = &self.handle {
            let doc = handle.live_document();
            self.settled = Some(doc.clone());
            self.folds = doc.fold_ranges();
            self.state.reset(self.current_version());
            if let Some(buffer) = &self.buffer {
                let _ = buffer;
                handle.set_options(buffer_options(&doc, cx));
            }
            self.doc = Some(doc);
            self.rebuild_source();
        }
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
        if let Some(handle) = &self.handle {
            handle.keep_mine();
        }
        if self.conflict.take().is_some() {
            cx.emit(DocumentEvent::ConflictChanged);
            cx.notify();
        }
    }

    fn reload_from_disk(&mut self, discard: bool, cx: &mut Context<Self>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if discard {
                        handle.reload()
                    } else {
                        handle.check_disk()
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.load_task = None;
                if let Err(err) = result {
                    tracing::debug!("reload {}: {err:#}", this.path.display());
                }
                this.sync_from_store(cx);
                if std::mem::take(&mut this.reload_pending) {
                    this.reload_from_disk(false, cx);
                }
            });
        }));
    }

    /// Re-applies settings (indentation, auto-close, ...) to the buffer.
    pub fn settings_changed(&mut self, cx: &mut Context<Self>) {
        if let (Some(handle), Some(doc)) = (&self.handle, self.doc.clone()) {
            if self.buffer.is_some() {
                handle.set_options(buffer_options(&doc, cx));
                if let Some(buffer) = &self.buffer {
                    buffer.update(cx, |b, cx| b.catch_up(cx));
                }
            }
        }
        self.refresh_gutter(cx);
    }
}
