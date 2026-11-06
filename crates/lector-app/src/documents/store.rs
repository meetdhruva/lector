//! [`DocumentStore`] and [`DocHandle`]: one live document per canonical path.
//!
//! Locking: each document has one state mutex, held only for short
//! synchronous steps (a buffer operation, installing a result). Work that may
//! take longer (parsing, highlighting, outline, git, LSP, disk I/O for saves)
//! happens outside it. Events and background jobs produced while the lock is
//! held are collected in an [`Fx`] and dispatched after it is released, so
//! subscribers may call back into the handle and an inline executor cannot
//! deadlock.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use lector_buffer::save::{Encoding, LineEnding as SaveLineEnding, SaveOptions};
use lector_buffer::{Buffer, BufferOptions, EditDelta, Selection, TextEdit, Transaction};
use lector_diff::{diff_lines, DiffOptions, GutterMark};
use lector_fs::{FileKind, FileStamp};
use lector_git::BlameHunk;
use lector_lsp::DocumentDiagnostics;
use lector_syntax::{OutlineItem, Syntax, SyntaxEdit};
use lector_text::{Rope, Text};
use lector_workspace::{Document, LineEnding, LineHighlights, Project};
use parking_lot::{Mutex, RwLock};

use super::derive::{self, DocDiagnostic};
use super::exec::{DocExecutor, DocJob};
use super::policy;
use crate::scheduler::{CancelToken, JobKey, Priority};

/// Rows per highlight chunk (request and cache granularity).
pub const HIGHLIGHT_CHUNK: u32 = 128;
/// Transactions kept per document (diagnostics mapping, view catch-up).
pub const HISTORY_LEN: usize = 256;

/// Process-unique id of an open document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub u64);

/// Who made an edit. Front-ends allocate one per view/adapter with
/// [`EditOrigin::new`] and skip their own edits when catching up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EditOrigin(pub u64);

static NEXT_ORIGIN: AtomicU64 = AtomicU64::new(16);

impl EditOrigin {
    /// Programmatic edits through [`DocHandle::apply_edits`] & co.
    pub const EXTERNAL: EditOrigin = EditOrigin(0);
    /// Reloading from disk.
    pub const RELOAD: EditOrigin = EditOrigin(1);
    /// Save-time cleanup and before-save hook edits.
    pub const SAVE: EditOrigin = EditOrigin(2);

    /// A fresh, process-unique origin.
    #[allow(clippy::new_without_default)] // every call returns a different origin
    pub fn new() -> EditOrigin {
        EditOrigin(NEXT_ORIGIN.fetch_add(1, Ordering::Relaxed))
    }
}

/// Highlight lines of one chunk.
pub type ChunkHighlights = (u32, Arc<Vec<LineHighlights>>);

/// Errors of document operations.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// The file could not be loaded.
    #[error("cannot open {path}: {message}")]
    Load {
        /// The path.
        path: PathBuf,
        /// Why.
        message: String,
    },
    /// Edits are locked ([`DocHandle::set_read_only`]).
    #[error("the document is read-only")]
    ReadOnly,
    /// Binary, memory-mapped or lossily decoded files cannot be edited or saved.
    #[error("the document cannot be edited (binary, too large or not losslessly decoded)")]
    NotEditable,
    /// The text changed since the version the caller computed against.
    #[error("the document changed (version {actual}, expected {expected})")]
    VersionMismatch {
        /// What the caller expected.
        expected: u64,
        /// The current version.
        actual: u64,
    },
    /// The file changed on disk since it was loaded or saved; nothing was
    /// written. Front-ends ask the user, then save with `force`.
    #[error("{} changed on disk since it was loaded", path.display())]
    ChangedOnDisk {
        /// The path.
        path: PathBuf,
    },
    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A file change on disk that conflicts with unsaved edits.
#[derive(Clone)]
pub struct DiskConflict {
    /// The file as it is on disk now.
    pub disk: Document,
    /// Its stamp.
    pub stamp: Option<FileStamp>,
}

impl std::fmt::Debug for DiskConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskConflict")
            .field("stamp", &self.stamp)
            .finish_non_exhaustive()
    }
}

/// What Lector knows about the file on disk.
#[derive(Clone, Debug)]
pub struct DiskState {
    /// Stamp when loaded / last saved / last accepted (`None`: unknown or absent).
    pub stamp: Option<FileStamp>,
    /// Encoding used when saving.
    pub encoding: Encoding,
    /// The file is gone from disk while the document is open.
    pub deleted: bool,
    /// A dirty document whose file changed on disk.
    pub conflict: Option<DiskConflict>,
}

/// An immutable view of a document at one version.
#[derive(Clone)]
pub struct DocSnapshot {
    /// The document id.
    pub id: DocId,
    /// Its path.
    pub path: PathBuf,
    /// The current text as a `Document` (syntax may lag: see `syntax_pending`).
    pub document: Document,
    /// Text version (`Rope::version`; also the LSP sync tag).
    pub version: u64,
    /// Monotonic edit counter.
    pub seq: u64,
    /// Unsaved changes.
    pub dirty: bool,
    /// Has an edit buffer (text, not mapped, lossless).
    pub editable: bool,
    /// Edits are locked.
    pub read_only: bool,
    /// Disk state.
    pub disk: DiskState,
    /// Successful saves since open.
    pub save_count: u64,
    /// A reparse of the newest text is pending.
    pub syntax_pending: bool,
}

impl DocSnapshot {
    /// The text, if the document is text.
    pub fn text(&self) -> Option<&Text> {
        self.document.text()
    }

    /// The syntax tree of this version, if parsed.
    pub fn syntax(&self) -> Option<&Syntax> {
        self.document.syntax()
    }
}

/// The parts of the buffer a front-end mirrors (consistent at `seq`).
#[derive(Clone, Debug)]
pub struct BufferView {
    /// Text.
    pub text: Rope,
    /// Its version.
    pub version: u64,
    /// Edit counter it belongs to.
    pub seq: u64,
    /// The buffer's selections (as left by the last operation).
    pub selections: Vec<Selection>,
    /// Options.
    pub options: BufferOptions,
    /// Unsaved changes.
    pub dirty: bool,
    /// Undo available.
    pub can_undo: bool,
    /// Redo available.
    pub can_redo: bool,
}

impl BufferView {
    fn of(buffer: &Buffer, seq: u64) -> BufferView {
        BufferView {
            text: buffer.text().clone(),
            version: buffer.version(),
            seq,
            selections: buffer.selections().to_vec(),
            options: buffer.options().clone(),
            dirty: buffer.is_dirty(),
            can_undo: buffer.can_undo(),
            can_redo: buffer.can_redo(),
        }
    }
}

/// One recorded edit.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    /// Edit counter after it.
    pub seq: u64,
    /// Who made it.
    pub origin: EditOrigin,
    /// Text version after it.
    pub version: u64,
    /// Text after it.
    pub text: Rope,
    /// The edit.
    pub transaction: Arc<Transaction>,
}

/// Edits since some `seq`, for a front-end catching up.
#[derive(Clone, Debug)]
pub struct CatchUp {
    /// Entries after the given seq, oldest first (own edits included).
    pub entries: Vec<HistoryEntry>,
    /// History did not reach back far enough: `entries` is empty and the
    /// caller must resynchronize from `view`.
    pub gap: bool,
    /// The buffer now.
    pub view: BufferView,
}

/// Result of an edit through a handle.
#[derive(Debug)]
pub struct EditResult<R> {
    /// What the operation returned.
    pub value: R,
    /// The edit (empty when nothing changed).
    pub transaction: Arc<Transaction>,
    /// Foreign edits made since the caller's `since` seq (before this one).
    pub catch_up: Vec<HistoryEntry>,
    /// History gap before this edit (resync from `view`).
    pub gap: bool,
    /// The buffer after the edit.
    pub view: BufferView,
}

/// Kind of derived query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryKind {
    /// Outline, folds, scopes (sticky headers, breadcrumbs).
    Structure,
    /// Diff gutter vs HEAD.
    Gutter,
    /// Blame mapped onto the current text.
    Blame,
}

/// Events of one document. Delivered in order, outside the document lock.
#[derive(Clone, Debug)]
pub enum DocEvent {
    /// The text changed.
    Edited {
        /// Who edited.
        origin: EditOrigin,
        /// Edit counter after it.
        seq: u64,
        /// Text version after it.
        version: u64,
        /// The edit.
        transaction: Arc<Transaction>,
    },
    /// The syntax tree of `version` is installed.
    Reparsed {
        /// The version.
        version: u64,
    },
    /// A derived query has a result for `version`.
    QueryReady {
        /// Which.
        kind: QueryKind,
        /// The version.
        version: u64,
    },
    /// Highlight chunks of `version` were computed (stale versions too, so
    /// requesters can forget their pending marks).
    HighlightsReady {
        /// The version they belong to.
        version: u64,
        /// `(chunk, lines)`.
        chunks: Vec<(u32, Arc<Vec<LineHighlights>>)>,
    },
    /// Mapped diagnostics changed.
    DiagnosticsChanged,
    /// The dirty flag flipped.
    DirtyChanged {
        /// Now dirty.
        dirty: bool,
    },
    /// The file was written.
    Saved {
        /// Version written.
        version: u64,
    },
    /// Conflict / deleted / stamp changed.
    DiskStateChanged,
    /// The text was replaced from disk (follows its `Edited`); the snapshot
    /// carries a full parse.
    Reloaded {
        /// The version.
        version: u64,
    },
    /// Not editable (or editability changed): the whole snapshot was replaced.
    Replaced,
    /// Save As moved the document.
    PathChanged {
        /// Old path.
        old: PathBuf,
        /// New path.
        new: PathBuf,
    },
    /// [`DocHandle::set_read_only`].
    ReadOnlyChanged {
        /// Now read-only.
        read_only: bool,
    },
}

/// Delays of debounced work (`Duration::ZERO` = run as soon as possible).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryDelays {
    /// Outline / folds / scopes after an edit.
    pub structure: Duration,
    /// Diff gutter after an edit.
    pub gutter: Duration,
    /// LSP text sync after an edit.
    pub lsp: Duration,
    /// Blame remapping after an edit.
    pub blame: Duration,
}

impl Default for QueryDelays {
    fn default() -> Self {
        QueryDelays {
            structure: Duration::from_millis(300),
            gutter: Duration::from_millis(300),
            lsp: Duration::from_millis(150),
            blame: Duration::from_millis(120),
        }
    }
}

impl QueryDelays {
    /// Everything immediately (tests, CLI).
    pub const IMMEDIATE: QueryDelays = QueryDelays {
        structure: Duration::ZERO,
        gutter: Duration::ZERO,
        lsp: Duration::ZERO,
        blame: Duration::ZERO,
    };
}

/// How to open a document.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Buffer options (default: from the project's settings).
    pub buffer_options: Option<BufferOptions>,
    /// Rows whose highlights are computed while loading (first paint).
    pub warm_rows: Option<Range<u32>>,
}

/// How to save.
#[derive(Debug, Clone, Default)]
pub struct SaveRequest {
    /// Write even if the file changed on disk (or nothing changed).
    pub force: bool,
    /// Strip trailing spaces/tabs first (as buffer edits).
    pub trim_trailing_whitespace: bool,
    /// End with exactly one newline (as buffer edits).
    pub ensure_final_newline: bool,
    /// Run the store's before-save hooks.
    pub run_hooks: bool,
    /// Save As: write here and move the document.
    pub path: Option<PathBuf>,
}

impl SaveRequest {
    /// From editor settings (hooks on; not forced).
    pub fn from_settings(editor: &lector_config::EditorSettings) -> SaveRequest {
        SaveRequest {
            force: false,
            trim_trailing_whitespace: editor.trim_trailing_whitespace,
            ensure_final_newline: editor.ensure_final_newline,
            run_hooks: true,
            path: None,
        }
    }
}

/// Result of a successful save call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written.
    Saved,
    /// Nothing to write (clean and unchanged on disk).
    Unchanged,
}

/// What [`DocHandle::check_disk`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskChange {
    /// Same stamp (e.g. our own save).
    Unchanged,
    /// Clean document reloaded in place.
    Reloaded,
    /// Dirty document: a conflict is now pending.
    Conflict,
    /// The file is gone.
    Deleted,
}

/// A before-save hook (e.g. format on save), run on the saving thread.
/// Returns edits against `doc`'s text; they are applied only if the text did
/// not change meanwhile. Errors are logged and the save goes on.
pub trait BeforeSaveHook: Send + Sync {
    /// Edits to apply before saving `doc`.
    fn before_save(&self, doc: &DocSnapshot, cancel: &CancelToken)
        -> anyhow::Result<Vec<TextEdit>>;
}

impl<F> BeforeSaveHook for F
where
    F: Fn(&DocSnapshot, &CancelToken) -> anyhow::Result<Vec<TextEdit>> + Send + Sync,
{
    fn before_save(
        &self,
        doc: &DocSnapshot,
        cancel: &CancelToken,
    ) -> anyhow::Result<Vec<TextEdit>> {
        self(doc, cancel)
    }
}

type Callback = Arc<dyn Fn(&DocEvent) + Send + Sync>;

/// Keeps a subscription alive; dropping it unsubscribes.
#[must_use = "dropping a Subscription unsubscribes"]
pub struct Subscription {
    doc: Weak<DocShared>,
    id: u64,
}

impl Subscription {
    /// Keeps the callback registered for the document's lifetime.
    pub fn detach(mut self) {
        self.doc = Weak::new();
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(doc) = self.doc.upgrade() {
            doc.subs.list.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

#[derive(Default)]
struct Subscribers {
    list: Mutex<Vec<(u64, Callback)>>,
    queue: Mutex<VecDeque<DocEvent>>,
    dispatching: Mutex<()>,
    next: AtomicU64,
}

impl Subscribers {
    /// Queues events and delivers them in order (whoever holds the dispatch
    /// lock delivers; re-entrant emits are picked up by the outer loop).
    fn emit(&self, events: Vec<DocEvent>) {
        if events.is_empty() {
            return;
        }
        self.queue.lock().extend(events);
        loop {
            let Some(_guard) = self.dispatching.try_lock() else {
                return;
            };
            loop {
                let Some(event) = self.queue.lock().pop_front() else {
                    break;
                };
                let subs: Vec<Callback> = self.list.lock().iter().map(|(_, c)| c.clone()).collect();
                for cb in subs {
                    cb(&event);
                }
            }
            drop(_guard);
            if self.queue.lock().is_empty() {
                return;
            }
        }
    }
}

/// Effects collected under the document lock, run after it is released.
#[derive(Default)]
struct Fx {
    events: Vec<DocEvent>,
    jobs: Vec<(Priority, Option<JobKey>, Option<Duration>, DocJob)>,
}

impl Fx {
    fn spawn(&mut self, prio: Priority, key: Option<JobKey>, job: DocJob) {
        self.jobs.push((prio, key, None, job));
    }
    fn debounce(&mut self, key: JobKey, delay: Duration, prio: Priority, job: DocJob) {
        self.jobs.push((prio, Some(key), Some(delay), job));
    }
}

/// A derived value cached per version.
struct Slot<T> {
    value: Option<(u64, T)>,
    scheduled: Option<u64>,
    waiting_syntax: bool,
    computes: u64,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            value: None,
            scheduled: None,
            waiting_syntax: false,
            computes: 0,
        }
    }
}

/// Outline, folds and the document they were computed from.
#[derive(Clone)]
pub struct Structure {
    /// The document (syntax present when the language has a grammar).
    pub document: Document,
    /// Outline.
    pub outline: Arc<Vec<OutlineItem>>,
    /// Fold ranges.
    pub folds: Arc<Vec<Range<u32>>>,
}

/// A query answer: the newest value known and whether it is current.
#[derive(Clone, Debug)]
pub struct QueryResult<T> {
    /// The newest value (possibly for an older version).
    pub value: Option<T>,
    /// Version of `value`.
    pub version: u64,
    /// `value` belongs to the current version.
    pub current: bool,
}

struct DocState {
    path: PathBuf,
    key: PathBuf,
    project: Project,
    buffer: Option<Buffer>,
    live: Document,
    seq: u64,
    history: VecDeque<HistoryEntry>,
    disk: DiskState,
    read_only: bool,
    save_count: u64,
    was_dirty: bool,
    // Syntax.
    parsed: Option<(Syntax, u64)>,
    pending_deltas: Vec<EditDelta>,
    parse_running: bool,
    parse_gen: u64,
    // Queries.
    structure: Slot<Structure>,
    gutter: Slot<Arc<Vec<GutterMark>>>,
    gutter_base: Option<Arc<Option<String>>>,
    gutter_enabled: bool,
    blame: Slot<Arc<Vec<BlameHunk>>>,
    highlights: (u64, HashMap<u32, Arc<Vec<LineHighlights>>>),
    highlight_computes: u64,
    diagnostics: Arc<Vec<DocDiagnostic>>,
    lsp_synced: Option<u64>,
}

impl DocState {
    fn version(&self) -> u64 {
        match &self.buffer {
            Some(b) => b.version(),
            None => self.live.text().map_or(0, |t| t.version()),
        }
    }

    fn dirty(&self) -> bool {
        self.buffer.as_ref().is_some_and(|b| b.is_dirty())
    }

    fn syntax_pending(&self) -> bool {
        self.parse_running || !self.pending_deltas.is_empty()
    }

    fn line_ending(&self) -> LineEnding {
        self.live.line_ending()
    }
}

pub(crate) struct DocShared {
    id: DocId,
    me: Weak<DocShared>,
    store: Weak<StoreInner>,
    executor: Arc<dyn DocExecutor>,
    state: Mutex<DocState>,
    subs: Subscribers,
    save_lock: Mutex<()>,
}

impl Drop for DocShared {
    fn drop(&mut self) {
        let st = self.state.get_mut();
        let mut replaced = false;
        if let Some(store) = self.store.upgrade() {
            let mut docs = store.docs.lock();
            match docs.get(&st.key) {
                Some(w) if w.strong_count() == 0 => {
                    docs.remove(&st.key);
                }
                Some(_) => replaced = true,
                None => {}
            }
        }
        // A newer document of the same path keeps the server's copy open.
        if let (Some(lsp), false) = (st.project.lsp(), replaced) {
            if st.lsp_synced.is_some() {
                lsp.close_document(&st.path);
            }
        }
    }
}

struct StoreInner {
    executor: Arc<dyn DocExecutor>,
    docs: Mutex<HashMap<PathBuf, Weak<DocShared>>>,
    hooks: RwLock<Vec<(u64, Arc<dyn BeforeSaveHook>)>>,
    delays: RwLock<QueryDelays>,
    next_id: AtomicU64,
    pinned: Mutex<HashMap<DocId, DocHandle>>,
}

/// All open documents of the process (one per canonical path). Cheap to clone.
#[derive(Clone)]
pub struct DocumentStore {
    inner: Arc<StoreInner>,
}

impl std::fmt::Debug for DocumentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentStore")
            .field("open", &self.inner.docs.lock().len())
            .finish()
    }
}

/// Registration of a store-level before-save hook; dropping it does not
/// unregister (use [`DocumentStore::unregister_before_save`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookId(pub u64);

/// The canonical registry key for `path`.
pub fn canonical_key(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

struct Loaded {
    doc: Document,
    stamp: Option<FileStamp>,
    encoding: Encoding,
    chunks: Vec<(u32, Arc<Vec<LineHighlights>>)>,
}

fn load_from_disk(
    project: &Project,
    path: &Path,
    warm: Option<Range<u32>>,
) -> Result<Loaded, DocError> {
    // Stamp before reading: if the file changes in between we would rather
    // report a (false) conflict than silently overwrite newer content.
    let stamp = lector_fs::stamp(path).ok();
    let doc = project.load_document(path).map_err(|e| DocError::Load {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let _ = doc.outline();
    let _ = doc.fold_ranges();
    let mut chunks = Vec::new();
    if let (Some(rows), Some(_)) = (warm, doc.syntax()) {
        let first = rows.start / HIGHLIGHT_CHUNK;
        let last = rows.end.saturating_sub(1).max(rows.start) / HIGHLIGHT_CHUNK;
        for chunk in first..=last {
            let range = chunk * HIGHLIGHT_CHUNK..(chunk + 1) * HIGHLIGHT_CHUNK;
            if range.start >= doc.line_count() {
                break;
            }
            chunks.push((chunk, Arc::new(doc.line_highlights(range))));
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
    let encoding = match doc.encoding() {
        "UTF-16LE" => Encoding::Utf16Le,
        "UTF-16BE" => Encoding::Utf16Be,
        _ if bom => Encoding::Utf8Bom,
        _ => Encoding::Utf8,
    };
    Ok(Loaded {
        doc,
        stamp,
        encoding,
        chunks,
    })
}

/// Whether a loaded document can be edited and saved losslessly.
pub fn is_editable_document(doc: &Document) -> bool {
    matches!(doc.kind(), FileKind::Text)
        && doc.text().is_some_and(|t| !t.is_mapped())
        && !doc.lossy()
        && matches!(doc.encoding(), "UTF-8" | "UTF-16LE" | "UTF-16BE")
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

impl DocumentStore {
    /// A store running its background work on `executor`.
    pub fn new(executor: Arc<dyn DocExecutor>) -> DocumentStore {
        DocumentStore {
            inner: Arc::new(StoreInner {
                executor,
                docs: Mutex::new(HashMap::new()),
                hooks: RwLock::new(Vec::new()),
                delays: RwLock::new(QueryDelays::default()),
                next_id: AtomicU64::new(1),
                pinned: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The store of an [`crate::AppCore`] (created on first use, on its scheduler).
    pub fn for_core(core: &crate::AppCore) -> Arc<DocumentStore> {
        let scheduler = core.scheduler().clone();
        core.extension_or_init(|| Arc::new(DocumentStore::new(Arc::new(scheduler))))
    }

    /// Sets the debounce delays for documents (existing and new).
    pub fn set_delays(&self, delays: QueryDelays) {
        *self.inner.delays.write() = delays;
    }

    /// Current delays.
    pub fn delays(&self) -> QueryDelays {
        *self.inner.delays.read()
    }

    /// The open document for `path`, if any.
    pub fn get(&self, path: &Path) -> Option<DocHandle> {
        let key = canonical_key(path);
        let docs = self.inner.docs.lock();
        docs.get(&key)
            .or_else(|| docs.get(path))
            .and_then(|w| w.upgrade())
            .map(|shared| DocHandle { shared })
    }

    /// Opens `path` (blocking: reads and parses the file) or returns the
    /// document already open for it.
    pub fn open(&self, path: &Path, project: &Project) -> Result<DocHandle, DocError> {
        self.open_with(path, project, OpenOptions::default())
    }

    /// [`DocumentStore::open`] with options (ignored when already open,
    /// except that `warm_rows` are computed on the scheduler).
    pub fn open_with(
        &self,
        path: &Path,
        project: &Project,
        opts: OpenOptions,
    ) -> Result<DocHandle, DocError> {
        if let Some(existing) = self.get(path) {
            if let Some(rows) = opts.warm_rows {
                let chunks: Vec<u32> = (rows.start / HIGHLIGHT_CHUNK
                    ..=rows.end.saturating_sub(1) / HIGHLIGHT_CHUNK)
                    .collect();
                existing.request_highlights(&chunks);
            }
            return Ok(existing);
        }
        let key = canonical_key(path);
        let loaded = load_from_disk(project, path, opts.warm_rows.clone())?;
        let mut docs = self.inner.docs.lock();
        if let Some(shared) = docs.get(&key).and_then(|w| w.upgrade()) {
            // Someone else opened it meanwhile.
            return Ok(DocHandle { shared });
        }
        let id = DocId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let options = opts
            .buffer_options
            .unwrap_or_else(|| policy::buffer_options(&loaded.doc, &project.settings()));
        let state = new_state(path, key.clone(), project.clone(), loaded, options);
        let shared = Arc::new_cyclic(|me| DocShared {
            id,
            me: me.clone(),
            store: Arc::downgrade(&self.inner),
            executor: self.inner.executor.clone(),
            state: Mutex::new(state),
            subs: Subscribers::default(),
            save_lock: Mutex::new(()),
        });
        docs.insert(key, Arc::downgrade(&shared));
        drop(docs);
        let handle = DocHandle { shared };
        handle.schedule_lsp_sync(Duration::ZERO);
        Ok(handle)
    }

    /// Every open document.
    pub fn documents(&self) -> Vec<DocHandle> {
        let mut docs = self.inner.docs.lock();
        docs.retain(|_, w| w.strong_count() > 0);
        let mut out: Vec<DocHandle> = docs
            .values()
            .filter_map(|w| w.upgrade())
            .map(|shared| DocHandle { shared })
            .collect();
        out.sort_by_key(|h| h.id());
        out
    }

    /// Open documents with unsaved changes.
    pub fn dirty_documents(&self) -> Vec<DocHandle> {
        self.documents()
            .into_iter()
            .filter(|d| d.is_dirty())
            .collect()
    }

    /// Forgets `handle` when nobody else holds it (its last view closed): the
    /// next open of the path loads it fresh from disk.
    pub fn release(&self, handle: DocHandle) {
        // The registry holds a weak reference; dropping the last handle does
        // the rest (background jobs only hold weak references too).
        drop(handle);
    }

    /// Keeps `handle` open without a view (documents opened by commands and
    /// agents stay live, with their unsaved edits, until [`DocumentStore::unpin`]).
    pub fn pin(&self, handle: &DocHandle) {
        self.inner.pinned.lock().insert(handle.id(), handle.clone());
    }

    /// Releases a [`DocumentStore::pin`]; true if it was pinned.
    pub fn unpin(&self, handle: &DocHandle) -> bool {
        self.inner.pinned.lock().remove(&handle.id()).is_some()
    }

    /// Whether `handle` is pinned.
    pub fn is_pinned(&self, handle: &DocHandle) -> bool {
        self.inner.pinned.lock().contains_key(&handle.id())
    }

    /// Removes `handle` from the registry (whoever still holds it keeps a
    /// detached document): the next open of its path loads from disk. Used
    /// when the last view of a document is closed without saving.
    pub fn forget(&self, handle: &DocHandle) {
        let key = handle.shared.state.lock().key.clone();
        let mut docs = self.inner.docs.lock();
        if docs
            .get(&key)
            .is_some_and(|w| w.ptr_eq(&Arc::downgrade(&handle.shared)))
        {
            docs.remove(&key);
        }
    }

    /// Registers a before-save hook (run in registration order on every save
    /// with `run_hooks`).
    pub fn register_before_save(&self, hook: Arc<dyn BeforeSaveHook>) -> HookId {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.hooks.write().push((id, hook));
        HookId(id)
    }

    /// Removes a hook.
    pub fn unregister_before_save(&self, id: HookId) {
        self.inner.hooks.write().retain(|(i, _)| *i != id.0);
    }
}

fn new_state(
    path: &Path,
    key: PathBuf,
    project: Project,
    loaded: Loaded,
    options: BufferOptions,
) -> DocState {
    let Loaded {
        doc,
        stamp,
        encoding,
        chunks,
    } = loaded;
    let buffer = is_editable_document(&doc)
        .then(|| doc.text().and_then(|t| t.as_rope()).cloned())
        .flatten()
        .map(|rope| Buffer::new(rope, options));
    let version = match &buffer {
        Some(b) => b.version(),
        None => doc.text().map_or(0, |t| t.version()),
    };
    let structure = Slot {
        value: Some((
            version,
            Structure {
                document: doc.clone(),
                outline: doc.outline(),
                folds: doc.fold_ranges(),
            },
        )),
        ..Slot::default()
    };
    // The loaded text is history too: diagnostics may refer to it.
    let base = buffer.as_ref().map(|b| HistoryEntry {
        seq: 0,
        origin: EditOrigin::RELOAD,
        version: b.version(),
        text: b.text().clone(),
        transaction: Arc::new(Transaction::default()),
    });
    DocState {
        path: path.to_path_buf(),
        key,
        project,
        parsed: doc.syntax().cloned().map(|s| (s, version)),
        buffer,
        live: doc,
        seq: 0,
        history: base.into_iter().collect(),
        disk: DiskState {
            stamp,
            encoding,
            deleted: false,
            conflict: None,
        },
        read_only: false,
        save_count: 0,
        was_dirty: false,
        pending_deltas: Vec::new(),
        parse_running: false,
        parse_gen: 0,
        structure,
        gutter: Slot::default(),
        gutter_base: None,
        gutter_enabled: true,
        blame: Slot::default(),
        highlights: (version, chunks.into_iter().collect()),
        highlight_computes: 0,
        diagnostics: Arc::new(Vec::new()),
        lsp_synced: None,
    }
}

/// A live document. Cheap to clone; every clone is the same document.
#[derive(Clone)]
pub struct DocHandle {
    shared: Arc<DocShared>,
}

impl PartialEq for DocHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl Eq for DocHandle {}

impl std::fmt::Debug for DocHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocHandle")
            .field("id", &self.shared.id)
            .field("path", &self.path())
            .finish()
    }
}

impl DocHandle {
    // ----- accessors ---------------------------------------------------

    /// The document id.
    pub fn id(&self) -> DocId {
        self.shared.id
    }

    /// Current path.
    pub fn path(&self) -> PathBuf {
        self.shared.state.lock().path.clone()
    }

    /// How many handles to this document exist (views, agents, commands).
    pub fn holders(&self) -> usize {
        Arc::strong_count(&self.shared)
    }

    /// The project it was opened in.
    pub fn project(&self) -> Project {
        self.shared.state.lock().project.clone()
    }

    /// Current text version.
    pub fn version(&self) -> u64 {
        self.shared.state.lock().version()
    }

    /// Edit counter.
    pub fn seq(&self) -> u64 {
        self.shared.state.lock().seq
    }

    /// Unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.shared.state.lock().dirty()
    }

    /// Has an edit buffer.
    pub fn is_editable(&self) -> bool {
        self.shared.state.lock().buffer.is_some()
    }

    /// Edits are locked.
    pub fn is_read_only(&self) -> bool {
        self.shared.state.lock().read_only
    }

    /// Current text (a cheap rope clone).
    pub fn text(&self) -> Option<Rope> {
        let st = self.shared.state.lock();
        match &st.buffer {
            Some(b) => Some(b.text().clone()),
            None => st.live.text().and_then(|t| t.as_rope()).cloned(),
        }
    }

    /// The current document (text exact; syntax may lag behind edits).
    pub fn live_document(&self) -> Document {
        self.shared.state.lock().live.clone()
    }

    /// A consistent snapshot.
    pub fn snapshot(&self) -> DocSnapshot {
        let st = self.shared.state.lock();
        DocSnapshot {
            id: self.shared.id,
            path: st.path.clone(),
            document: st.live.clone(),
            version: st.version(),
            seq: st.seq,
            dirty: st.dirty(),
            editable: st.buffer.is_some(),
            read_only: st.read_only,
            disk: st.disk.clone(),
            save_count: st.save_count,
            syntax_pending: st.syntax_pending(),
        }
    }

    /// The disk state.
    pub fn disk_state(&self) -> DiskState {
        self.shared.state.lock().disk.clone()
    }

    /// The buffer as a front-end mirrors it (`None`: not editable).
    pub fn buffer_view(&self) -> Option<BufferView> {
        let st = self.shared.state.lock();
        st.buffer.as_ref().map(|b| BufferView::of(b, st.seq))
    }

    /// Recorded edits after `since` (see [`CatchUp`]); `None` when not editable.
    pub fn catch_up(&self, since: u64) -> Option<CatchUp> {
        let st = self.shared.state.lock();
        let buffer = st.buffer.as_ref()?;
        let (entries, gap) = history_since(&st, since);
        Some(CatchUp {
            entries,
            gap,
            view: BufferView::of(buffer, st.seq),
        })
    }

    /// Subscribes to events. Callbacks run on the thread that caused the
    /// event, outside the document lock, in order.
    pub fn subscribe(&self, cb: impl Fn(&DocEvent) + Send + Sync + 'static) -> Subscription {
        let id = self.shared.subs.next.fetch_add(1, Ordering::Relaxed);
        self.shared.subs.list.lock().push((id, Arc::new(cb)));
        Subscription {
            doc: Arc::downgrade(&self.shared),
            id,
        }
    }

    // ----- editing -----------------------------------------------------

    /// Runs a buffer operation as `origin`. `since` is the caller's last seen
    /// seq: edits made by others after it are returned in
    /// [`EditResult::catch_up`] (use `u64::MAX` to skip). Fails when read-only
    /// or not editable.
    pub fn edit_from<R>(
        &self,
        origin: EditOrigin,
        since: u64,
        f: impl FnOnce(&mut Buffer) -> (R, Transaction),
    ) -> Result<EditResult<R>, DocError> {
        let mut fx = Fx::default();
        let result = {
            let mut st = self.shared.state.lock();
            if st.read_only {
                return Err(DocError::ReadOnly);
            }
            let (catch_up, gap) = if since == u64::MAX {
                (Vec::new(), false)
            } else {
                history_since(&st, since)
            };
            let Some(buffer) = st.buffer.as_mut() else {
                return Err(DocError::NotEditable);
            };
            let (value, tx) = f(buffer);
            let tx = Arc::new(tx);
            if !tx.deltas.is_empty() {
                self.record_edit(&mut st, origin, tx.clone(), true, &mut fx);
            }
            self.check_dirty(&mut st, &mut fx);
            let view = st
                .buffer
                .as_ref()
                .map(|b| BufferView::of(b, st.seq))
                .ok_or(DocError::NotEditable)?;
            EditResult {
                value,
                transaction: tx,
                catch_up,
                gap,
                view,
            }
        };
        self.run(fx);
        Ok(result)
    }

    /// [`DocHandle::edit_from`] for programmatic edits.
    pub fn edit<R>(
        &self,
        f: impl FnOnce(&mut Buffer) -> (R, Transaction),
    ) -> Result<(R, Arc<Transaction>), DocError> {
        self.edit_from(EditOrigin::EXTERNAL, u64::MAX, f)
            .map(|r| (r.value, r.transaction))
    }

    /// Runs a buffer operation that does not change the text (selections,
    /// options, transactions grouping, copy). Works while read-only.
    pub fn with_buffer_mut<R>(&self, f: impl FnOnce(&mut Buffer) -> R) -> Option<(R, BufferView)> {
        let mut fx = Fx::default();
        let out = {
            let mut st = self.shared.state.lock();
            let seq = st.seq;
            let buffer = st.buffer.as_mut()?;
            let before = buffer.version();
            let r = f(buffer);
            debug_assert_eq!(before, buffer.version(), "with_buffer_mut changed the text");
            let view = BufferView::of(buffer, seq);
            self.check_dirty(&mut st, &mut fx);
            (r, view)
        };
        self.run(fx);
        Some(out)
    }

    /// Applies edits (offsets into the current text) as one undo step.
    pub fn apply_edits(&self, edits: Vec<TextEdit>) -> Result<Arc<Transaction>, DocError> {
        self.apply_edits_as(EditOrigin::EXTERNAL, None, edits)
    }

    /// Applies edits only if the text is still at `version`.
    pub fn apply_edits_at(
        &self,
        version: u64,
        edits: Vec<TextEdit>,
    ) -> Result<Arc<Transaction>, DocError> {
        self.apply_edits_as(EditOrigin::EXTERNAL, Some(version), edits)
    }

    /// Applies edits as `origin`, optionally only at `expected` version.
    pub fn apply_edits_as(
        &self,
        origin: EditOrigin,
        expected: Option<u64>,
        edits: Vec<TextEdit>,
    ) -> Result<Arc<Transaction>, DocError> {
        let r = self.edit_from(origin, u64::MAX, move |b| {
            if let Some(expected) = expected {
                if b.version() != expected {
                    return (Err(b.version()), Transaction::default());
                }
            }
            (Ok(()), b.apply_edits(edits))
        })?;
        match r.value {
            Ok(()) => Ok(r.transaction),
            Err(actual) => Err(DocError::VersionMismatch {
                expected: expected.unwrap_or_default(),
                actual,
            }),
        }
    }

    /// Replaces the whole text (one undo step; carets kept near their place).
    pub fn replace_all(&self, text: &str) -> Result<Arc<Transaction>, DocError> {
        self.edit(|b| ((), b.replace_all(text))).map(|(_, tx)| tx)
    }

    /// Undo.
    pub fn undo(&self) -> Result<Arc<Transaction>, DocError> {
        self.edit(|b| ((), b.undo())).map(|(_, tx)| tx)
    }

    /// Redo.
    pub fn redo(&self) -> Result<Arc<Transaction>, DocError> {
        self.edit(|b| ((), b.redo())).map(|(_, tx)| tx)
    }

    /// Replaces the buffer options.
    pub fn set_options(&self, options: BufferOptions) {
        let _ = self.with_buffer_mut(|b| b.set_options(options));
    }

    /// Locks or unlocks edits.
    pub fn set_read_only(&self, read_only: bool) {
        let changed = {
            let mut st = self.shared.state.lock();
            std::mem::replace(&mut st.read_only, read_only) != read_only
        };
        if changed {
            self.shared
                .subs
                .emit(vec![DocEvent::ReadOnlyChanged { read_only }]);
        }
    }

    // ----- internals: edit bookkeeping ---------------------------------

    fn record_edit(
        &self,
        st: &mut DocState,
        origin: EditOrigin,
        tx: Arc<Transaction>,
        reparse: bool,
        fx: &mut Fx,
    ) {
        let Some(buffer) = &st.buffer else { return };
        let rope = buffer.text().clone();
        let version = buffer.version();
        st.seq += 1;
        let seq = st.seq;
        st.history.push_back(HistoryEntry {
            seq,
            origin,
            version,
            text: rope.clone(),
            transaction: tx.clone(),
        });
        while st.history.len() > HISTORY_LEN {
            st.history.pop_front();
        }
        st.live = st.live.with_text(Text::from(rope), None);
        st.highlights = (version, HashMap::new());
        if !st.diagnostics.is_empty() {
            st.diagnostics = Arc::new(
                st.diagnostics
                    .iter()
                    .map(|d| DocDiagnostic {
                        range: derive::map_range(&d.range, &tx.deltas),
                        ..d.clone()
                    })
                    .collect(),
            );
            fx.events.push(DocEvent::DiagnosticsChanged);
        }
        if reparse {
            st.pending_deltas.extend_from_slice(&tx.deltas);
            self.schedule_reparse(st, fx);
        }
        fx.events.push(DocEvent::Edited {
            origin,
            seq,
            version,
            transaction: tx,
        });
        let delays = self.delays();
        self.lsp_job(st, delays.lsp, fx);
    }

    fn check_dirty(&self, st: &mut DocState, fx: &mut Fx) {
        let dirty = st.dirty();
        if dirty != st.was_dirty {
            st.was_dirty = dirty;
            fx.events.push(DocEvent::DirtyChanged { dirty });
        }
    }

    fn delays(&self) -> QueryDelays {
        self.shared
            .store
            .upgrade()
            .map(|s| *s.delays.read())
            .unwrap_or_default()
    }

    fn key(&self, what: &str) -> JobKey {
        JobKey(format!("doc:{}:{what}", self.shared.id.0))
    }

    fn run(&self, fx: Fx) {
        for (prio, key, delay, job) in fx.jobs {
            match (delay, key) {
                (Some(delay), Some(key)) => self.shared.executor.debounce(key, delay, prio, job),
                (_, key) => self.shared.executor.spawn(prio, key, job),
            }
        }
        self.shared.subs.emit(fx.events);
    }

    /// Runs `f` with the shared state of a still-open document (jobs hold
    /// only weak references).
    fn with_weak(weak: &Weak<DocShared>, f: impl FnOnce(&DocHandle)) {
        if let Some(shared) = weak.upgrade() {
            f(&DocHandle { shared });
        }
    }

    // ----- syntax --------------------------------------------------------

    fn schedule_reparse(&self, st: &mut DocState, fx: &mut Fx) {
        if st.parse_running || st.pending_deltas.is_empty() {
            return;
        }
        let Some((old, _)) = st.parsed.clone() else {
            st.pending_deltas.clear();
            return;
        };
        let Some(text) = st.live.text().cloned() else {
            return;
        };
        let version = text.version();
        let edits = syntax_edits(&std::mem::take(&mut st.pending_deltas));
        st.parse_running = true;
        let gen = st.parse_gen;
        let weak = self.shared.me.clone();
        fx.spawn(
            Priority::Interactive,
            None,
            Box::new(move |_| {
                let started = std::time::Instant::now();
                let parsed = old
                    .edit_and_reparse(&text, &edits)
                    .or_else(|_| Syntax::parse(old.language(), &text));
                tracing::trace!(target: "lector_app::perf", elapsed_us = started.elapsed().as_micros() as u64, "incremental reparse");
                Self::with_weak(&weak, |h| h.parsed(gen, version, parsed.ok()));
            }),
        );
    }

    fn parsed(&self, gen: u64, version: u64, syntax: Option<Syntax>) {
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            if st.parse_gen != gen {
                return;
            }
            st.parse_running = false;
            let Some(syntax) = syntax else {
                // Too large now: no more syntax for this buffer.
                st.parsed = None;
                st.pending_deltas.clear();
                self.syntax_settled(&mut st, &mut fx);
                drop(st);
                self.run(fx);
                return;
            };
            st.parsed = Some((syntax.clone(), version));
            if st.version() == version {
                if let Some(text) = st.live.text().cloned() {
                    st.live = st.live.with_text(text, Some(syntax));
                    fx.events.push(DocEvent::Reparsed { version });
                }
                self.syntax_settled(&mut st, &mut fx);
            } else {
                self.schedule_reparse(&mut st, &mut fx);
            }
        }
        self.run(fx);
    }

    /// The current version's syntax is final: run queries that waited for it.
    fn syntax_settled(&self, st: &mut DocState, fx: &mut Fx) {
        if std::mem::take(&mut st.structure.waiting_syntax) {
            st.structure.scheduled = None;
            self.structure_job(st, Duration::ZERO, fx);
        }
    }

    // ----- queries -------------------------------------------------------

    /// Outline + folds of the current version (scheduled when missing; the
    /// newest older result meanwhile). [`DocEvent::QueryReady`] announces it.
    pub fn structure(&self) -> QueryResult<Structure> {
        let mut fx = Fx::default();
        let r = {
            let mut st = self.shared.state.lock();
            let version = st.version();
            let delay = self.delays().structure;
            let current = st
                .structure
                .value
                .as_ref()
                .is_some_and(|(v, _)| *v == version);
            if !current {
                self.structure_job(&mut st, delay, &mut fx);
            }
            slot_result(&st.structure, version)
        };
        self.run(fx);
        r
    }

    /// Outline of the current version (see [`DocHandle::structure`]).
    pub fn outline(&self) -> QueryResult<Arc<Vec<OutlineItem>>> {
        map_result(self.structure(), |s| s.outline)
    }

    /// Fold ranges of the current version (see [`DocHandle::structure`]).
    pub fn folds(&self) -> QueryResult<Arc<Vec<Range<u32>>>> {
        map_result(self.structure(), |s| s.folds)
    }

    /// The newest document whose outline is computed (sticky headers,
    /// breadcrumbs): at most a few hundred milliseconds behind typing.
    pub fn settled_document(&self) -> Document {
        let st = self.shared.state.lock();
        st.structure
            .value
            .as_ref()
            .map(|(_, s)| s.document.clone())
            .unwrap_or_else(|| st.live.clone())
    }

    /// Enclosing scopes at `row` from the settled document (sticky headers,
    /// breadcrumbs), outermost first.
    pub fn scopes_at_row(&self, row: u32) -> Vec<OutlineItem> {
        self.settled_document().scopes_at_row(row)
    }

    fn structure_job(&self, st: &mut DocState, delay: Duration, fx: &mut Fx) {
        let version = st.version();
        if st.structure.scheduled == Some(version) {
            return;
        }
        st.structure.scheduled = Some(version);
        let weak = self.shared.me.clone();
        fx.debounce(
            self.key("structure"),
            delay,
            Priority::Visible,
            Box::new(move |cancel| {
                Self::with_weak(&weak, |h| h.compute_structure(version, cancel));
            }),
        );
    }

    fn compute_structure(&self, version: u64, cancel: &CancelToken) {
        let doc = {
            let mut st = self.shared.state.lock();
            if st.version() != version || cancel.is_cancelled() {
                if st.structure.scheduled == Some(version) {
                    st.structure.scheduled = None;
                }
                return;
            }
            if st.syntax_pending() {
                st.structure.waiting_syntax = true;
                return;
            }
            st.live.clone()
        };
        let outline = doc.outline();
        let folds = doc.fold_ranges();
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.structure.computes += 1;
            if st.structure.scheduled == Some(version) {
                st.structure.scheduled = None;
            }
            let newer = st
                .structure
                .value
                .as_ref()
                .is_some_and(|(v, _)| *v == st.version());
            if !newer {
                st.structure.value = Some((
                    version,
                    Structure {
                        document: doc,
                        outline,
                        folds,
                    },
                ));
                fx.events.push(DocEvent::QueryReady {
                    kind: QueryKind::Structure,
                    version,
                });
            }
        }
        self.run(fx);
    }

    /// Diff gutter vs HEAD for the current version (scheduled when missing).
    pub fn gutter(&self) -> QueryResult<Arc<Vec<GutterMark>>> {
        let mut fx = Fx::default();
        let r = {
            let mut st = self.shared.state.lock();
            let version = st.version();
            let current = st.gutter.value.as_ref().is_some_and(|(v, _)| *v == version);
            if !current && st.gutter_enabled {
                let delay = self.delays().gutter;
                self.gutter_job(&mut st, delay, &mut fx);
            }
            slot_result(&st.gutter, version)
        };
        self.run(fx);
        r
    }

    /// Re-reads HEAD and recomputes the gutter now (git state changed).
    pub fn refresh_gutter(&self) {
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.gutter_base = None;
            st.gutter.scheduled = None;
            if st.gutter_enabled {
                self.gutter_job(&mut st, Duration::ZERO, &mut fx);
            }
        }
        self.run(fx);
    }

    /// Turns the gutter query on or off (off: empty marks).
    pub fn set_gutter_enabled(&self, enabled: bool) {
        let mut st = self.shared.state.lock();
        st.gutter_enabled = enabled;
        if !enabled {
            let version = st.version();
            st.gutter.value = Some((version, Arc::new(Vec::new())));
        } else {
            st.gutter.value = None;
        }
    }

    fn gutter_job(&self, st: &mut DocState, delay: Duration, fx: &mut Fx) {
        let version = st.version();
        if st.gutter.scheduled == Some(version) {
            return;
        }
        st.gutter.scheduled = Some(version);
        let weak = self.shared.me.clone();
        fx.debounce(
            self.key("gutter"),
            delay,
            Priority::Background,
            Box::new(move |cancel| {
                Self::with_weak(&weak, |h| h.compute_gutter(version, cancel));
            }),
        );
    }

    fn compute_gutter(&self, version: u64, cancel: &CancelToken) {
        let (doc, base, project) = {
            let mut st = self.shared.state.lock();
            if st.version() != version || cancel.is_cancelled() {
                if st.gutter.scheduled == Some(version) {
                    st.gutter.scheduled = None;
                }
                return;
            }
            (st.live.clone(), st.gutter_base.clone(), st.project.clone())
        };
        let base =
            base.unwrap_or_else(|| Arc::new(doc.text().and_then(|_| doc.gutter_base(&project))));
        let marks = match base.as_ref() {
            Some(b) => doc.gutter_against(b),
            None => Vec::new(),
        };
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.gutter.computes += 1;
            st.gutter_base = Some(base);
            if st.gutter.scheduled == Some(version) {
                st.gutter.scheduled = None;
            }
            if st.version() == version {
                st.gutter.value = Some((version, Arc::new(marks)));
                fx.events.push(DocEvent::QueryReady {
                    kind: QueryKind::Gutter,
                    version,
                });
            }
        }
        self.run(fx);
    }

    /// Blame of the file on disk mapped onto the current text (edited lines
    /// are `uncommitted`). `None` value outside a repository.
    pub fn blame_map(&self) -> QueryResult<Arc<Vec<BlameHunk>>> {
        let mut fx = Fx::default();
        let r = {
            let mut st = self.shared.state.lock();
            let version = st.version();
            let current = st.blame.value.as_ref().is_some_and(|(v, _)| *v == version);
            if !current && st.blame.scheduled != Some(version) {
                st.blame.scheduled = Some(version);
                let weak = self.shared.me.clone();
                fx.debounce(
                    self.key("blame"),
                    self.delays().blame,
                    Priority::Background,
                    Box::new(move |cancel| {
                        Self::with_weak(&weak, |h| h.compute_blame(version, cancel));
                    }),
                );
            }
            slot_result(&st.blame, version)
        };
        self.run(fx);
        r
    }

    fn compute_blame(&self, version: u64, cancel: &CancelToken) {
        let (project, path, text) = {
            let mut st = self.shared.state.lock();
            if st.version() != version || cancel.is_cancelled() {
                if st.blame.scheduled == Some(version) {
                    st.blame.scheduled = None;
                }
                return;
            }
            let text = match &st.buffer {
                Some(b) => b.text().to_string(),
                None => st.live.text().map(|t| t.to_string()).unwrap_or_default(),
            };
            (st.project.clone(), st.path.clone(), text)
        };
        let mapped = project.relative_path(&path).and_then(|rel| {
            let hunks = project.blame(&rel).ok()?;
            let disk = std::fs::read(&path).ok()?;
            let disk = derive::unix_lines(String::from_utf8_lossy(&disk).into_owned());
            let current = derive::unix_lines(text);
            if disk == current {
                return Some(hunks);
            }
            let diff = diff_lines(&disk, &current, &DiffOptions::default());
            Some(Arc::new(derive::remap_blame(&hunks, &diff)))
        });
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.blame.computes += 1;
            if st.blame.scheduled == Some(version) {
                st.blame.scheduled = None;
            }
            if st.version() == version {
                st.blame.value = Some((version, mapped.unwrap_or_default()));
                fx.events.push(DocEvent::QueryReady {
                    kind: QueryKind::Blame,
                    version,
                });
            }
        }
        self.run(fx);
    }

    /// Highlights of `chunk` (rows `chunk * HIGHLIGHT_CHUNK ..`) for the
    /// current version, if computed.
    pub fn highlight_chunk(&self, chunk: u32) -> Option<Arc<Vec<LineHighlights>>> {
        let st = self.shared.state.lock();
        (st.highlights.0 == st.version())
            .then(|| st.highlights.1.get(&chunk).cloned())
            .flatten()
    }

    /// Every computed chunk of the current version.
    pub fn cached_highlights(&self) -> (u64, Vec<ChunkHighlights>) {
        let st = self.shared.state.lock();
        let version = st.version();
        if st.highlights.0 != version {
            return (version, Vec::new());
        }
        (
            version,
            st.highlights
                .1
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
        )
    }

    /// Highlights of `rows` if every chunk is computed; otherwise schedules
    /// the missing chunks ([`DocEvent::HighlightsReady`]) and returns `None`.
    pub fn highlights(&self, rows: Range<u32>) -> Option<Vec<LineHighlights>> {
        if rows.is_empty() {
            return Some(Vec::new());
        }
        let chunks: Vec<u32> =
            (rows.start / HIGHLIGHT_CHUNK..=(rows.end - 1) / HIGHLIGHT_CHUNK).collect();
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for &chunk in &chunks {
            match self.highlight_chunk(chunk) {
                Some(lines) => out.extend(lines.iter().filter(|l| rows.contains(&l.row)).cloned()),
                None => missing.push(chunk),
            }
        }
        if missing.is_empty() {
            return Some(out);
        }
        self.request_highlights(&missing);
        None
    }

    /// Computes `chunks` (in the given order: put the visible ones first) for
    /// the current version. Returns false when the document has no syntax
    /// (yet): request again after [`DocEvent::Reparsed`].
    pub fn request_highlights(&self, chunks: &[u32]) -> bool {
        let mut fx = Fx::default();
        let ok = {
            let st = self.shared.state.lock();
            let doc = st.live.clone();
            if doc.syntax().is_none() {
                false
            } else {
                let version = st.version();
                let work: Vec<u32> = chunks
                    .iter()
                    .copied()
                    .filter(|c| st.highlights.0 != version || !st.highlights.1.contains_key(c))
                    .collect();
                if !work.is_empty() {
                    let weak = self.shared.me.clone();
                    fx.spawn(
                        Priority::Visible,
                        None,
                        Box::new(move |_| {
                            let computed: Vec<(u32, Arc<Vec<LineHighlights>>)> = work
                                .into_iter()
                                .map(|chunk| {
                                    let rows =
                                        chunk * HIGHLIGHT_CHUNK..(chunk + 1) * HIGHLIGHT_CHUNK;
                                    (chunk, Arc::new(doc.line_highlights(rows)))
                                })
                                .collect();
                            Self::with_weak(&weak, |h| h.highlights_done(version, computed));
                        }),
                    );
                }
                true
            }
        };
        self.run(fx);
        ok
    }

    fn highlights_done(&self, version: u64, chunks: Vec<(u32, Arc<Vec<LineHighlights>>)>) {
        {
            let mut st = self.shared.state.lock();
            st.highlight_computes += chunks.len() as u64;
            if st.version() == version {
                if st.highlights.0 != version {
                    st.highlights = (version, HashMap::new());
                }
                for (chunk, lines) in &chunks {
                    st.highlights.1.insert(*chunk, lines.clone());
                }
            }
        }
        self.shared
            .subs
            .emit(vec![DocEvent::HighlightsReady { version, chunks }]);
    }

    /// How many times each query was computed (tests, diagnostics).
    pub fn compute_counts(&self) -> QueryCounts {
        let st = self.shared.state.lock();
        QueryCounts {
            structure: st.structure.computes,
            gutter: st.gutter.computes,
            blame: st.blame.computes,
            highlight_chunks: st.highlight_computes,
        }
    }

    // ----- diagnostics -----------------------------------------------------

    /// Installs published diagnostics: converted against the text they were
    /// computed for (its version is the LSP sync tag) and mapped through
    /// every later edit. `None` clears. Publishes for texts older than the
    /// history are ignored (the next publish replaces them).
    pub fn set_diagnostics(&self, published: Option<DocumentDiagnostics>) {
        let changed = {
            let mut st = self.shared.state.lock();
            let entries = match published {
                None => Vec::new(),
                Some(p) => {
                    let Some(tag) = p.tag else { return };
                    let (text, later): (Rope, Vec<Arc<Transaction>>) = if st.version() == tag {
                        let Some(text) = self.current_rope(&st) else {
                            return;
                        };
                        (text, Vec::new())
                    } else {
                        let Some(ix) = st.history.iter().rposition(|e| e.version == tag) else {
                            return;
                        };
                        (
                            st.history[ix].text.clone(),
                            st.history
                                .iter()
                                .skip(ix + 1)
                                .map(|e| e.transaction.clone())
                                .collect(),
                        )
                    };
                    let mut entries = derive::convert_diagnostics(&text, p.diagnostics);
                    for e in &mut entries {
                        for tx in &later {
                            e.range = derive::map_range(&e.range, &tx.deltas);
                        }
                    }
                    entries.sort_by_key(|e| (e.range.start, e.severity));
                    entries
                }
            };
            if *st.diagnostics == entries {
                false
            } else {
                st.diagnostics = Arc::new(entries);
                true
            }
        };
        if changed {
            self.shared.subs.emit(vec![DocEvent::DiagnosticsChanged]);
        }
    }

    /// Diagnostics in the current text.
    pub fn diagnostics(&self) -> Arc<Vec<DocDiagnostic>> {
        self.shared.state.lock().diagnostics.clone()
    }

    fn current_rope(&self, st: &DocState) -> Option<Rope> {
        match &st.buffer {
            Some(b) => Some(b.text().clone()),
            None => st.live.text().and_then(|t| t.as_rope()).cloned(),
        }
    }

    // ----- LSP -------------------------------------------------------------

    fn lsp_job(&self, st: &mut DocState, delay: Duration, fx: &mut Fx) {
        if st.project.lsp().is_none() {
            return;
        }
        let weak = self.shared.me.clone();
        fx.debounce(
            self.key("lsp"),
            delay,
            Priority::Background,
            Box::new(move |_| {
                Self::with_weak(&weak, |h| {
                    h.sync_lsp_now();
                })
            }),
        );
    }

    fn schedule_lsp_sync(&self, delay: Duration) {
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            self.lsp_job(&mut st, delay, &mut fx);
        }
        self.run(fx);
    }

    /// Sends the current text to the language server now (blocking-free:
    /// the manager queues it), tagged with the text version. Returns the
    /// version synced.
    pub fn sync_lsp_now(&self) -> Option<u64> {
        let (lsp, path, lang, text, version) = {
            let st = self.shared.state.lock();
            let lsp = st.project.lsp()?.clone();
            let lang = st.live.language()?.name.clone();
            let text = st.live.text()?;
            if text.is_mapped() || st.live.is_large() {
                return None;
            }
            if st.lsp_synced == Some(st.version()) {
                return st.lsp_synced;
            }
            (
                lsp,
                st.path.clone(),
                lang,
                self.current_rope(&st)?,
                st.version(),
            )
        };
        lsp.open_document_tagged(&path, &lang, &text.to_string(), version);
        self.shared.state.lock().lsp_synced = Some(version);
        Some(version)
    }

    // ----- disk ------------------------------------------------------------

    /// Saves (blocking): before-save hooks, cleanup edits, encoding/line
    /// endings kept, "changed on disk" check (unless `force`), atomic write.
    pub fn save(&self, req: SaveRequest) -> Result<SaveOutcome, DocError> {
        let _serial = self.shared.save_lock.lock();
        let (same_path, target) = {
            let st = self.shared.state.lock();
            if st.buffer.is_none() {
                return Err(DocError::NotEditable);
            }
            match &req.path {
                Some(p) if *p != st.path => (false, p.clone()),
                _ => (true, st.path.clone()),
            }
        };
        // 1. Hooks (format on save, ...).
        if req.run_hooks {
            let hooks: Vec<Arc<dyn BeforeSaveHook>> = self
                .shared
                .store
                .upgrade()
                .map(|s| s.hooks.read().iter().map(|(_, h)| h.clone()).collect())
                .unwrap_or_default();
            for hook in hooks {
                let snap = self.snapshot();
                match hook.before_save(&snap, &CancelToken::new()) {
                    Ok(edits) if !edits.is_empty() => {
                        match self.apply_edits_as(EditOrigin::SAVE, Some(snap.version), edits) {
                            Ok(_) | Err(DocError::VersionMismatch { .. }) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!("before-save hook failed: {err:#}"),
                }
            }
        }
        // 2. Cleanup edits, encode.
        let mut fx = Fx::default();
        let (bytes, version, expected, dirty, deleted) = {
            let mut st = self.shared.state.lock();
            let line_ending = st.line_ending();
            let encoding = st.disk.encoding;
            let Some(buffer) = st.buffer.as_mut() else {
                return Err(DocError::NotEditable);
            };
            let edits = policy::cleanup_edits(
                buffer.text(),
                line_ending,
                req.trim_trailing_whitespace,
                req.ensure_final_newline,
            );
            if !edits.is_empty() {
                let tx = Arc::new(buffer.apply_edits(edits));
                if !tx.deltas.is_empty() {
                    self.record_edit(&mut st, EditOrigin::SAVE, tx, true, &mut fx);
                }
            }
            let opts = SaveOptions {
                encoding,
                line_ending: match line_ending {
                    LineEnding::CrLf => Some(SaveLineEnding::CrLf),
                    LineEnding::Lf => Some(SaveLineEnding::Lf),
                    _ => None,
                },
                trim_trailing_whitespace: false,
                ensure_final_newline: false,
            };
            let Some(buffer) = st.buffer.as_ref() else {
                return Err(DocError::NotEditable);
            };
            let bytes = lector_buffer::save::encode(buffer.text(), &opts);
            (
                bytes,
                buffer.version(),
                st.disk.stamp,
                st.dirty(),
                st.disk.deleted,
            )
        };
        self.run(fx);
        // A clean buffer of a deleted file is still written: Save recreates it.
        if same_path && !dirty && !req.force && !deleted && expected.is_some() {
            return Ok(SaveOutcome::Unchanged);
        }
        // 3. Conflict check + atomic write.
        if same_path && !req.force {
            if let Some(expected) = expected {
                if let Ok(now) = lector_fs::stamp(&target) {
                    if now != expected {
                        return Err(DocError::ChangedOnDisk { path: target });
                    }
                }
            }
        }
        lector_fs::write_atomic(&target, &bytes)?;
        let new_stamp = lector_fs::stamp(&target).ok();
        // 4. Bookkeeping.
        let mut fx = Fx::default();
        {
            let mut st = self.shared.state.lock();
            st.disk.stamp = new_stamp;
            st.disk.conflict = None;
            st.disk.deleted = false;
            st.save_count += 1;
            if let Some(b) = st.buffer.as_mut() {
                if b.version() == version {
                    b.mark_saved();
                }
            }
            st.gutter_base = None;
            st.gutter.value = None;
            st.gutter.scheduled = None;
            st.blame.value = None;
            st.blame.scheduled = None;
            self.check_dirty(&mut st, &mut fx);
            if !same_path {
                self.move_to(&mut st, target, &mut fx);
            }
            fx.events.push(DocEvent::Saved { version });
            fx.events.push(DocEvent::DiskStateChanged);
        }
        self.run(fx);
        Ok(SaveOutcome::Saved)
    }

    fn move_to(&self, st: &mut DocState, path: PathBuf, fx: &mut Fx) {
        let old = std::mem::replace(&mut st.path, path.clone());
        if let Some(lsp) = st.project.lsp() {
            lsp.close_document(&old);
        }
        st.lsp_synced = None;
        let new_key = canonical_key(&path);
        if let Some(store) = self.shared.store.upgrade() {
            let mut docs = store.docs.lock();
            if docs.get(&st.key).is_some_and(|w| w.ptr_eq(&self.shared.me)) {
                docs.remove(&st.key);
            }
            docs.insert(new_key.clone(), self.shared.me.clone());
        }
        st.key = new_key;
        let rel = st.project.relative_path(&path);
        let languages = st.project.languages().clone();
        let doc = st.live.with_path(path.clone(), rel, &languages);
        let version = st.version();
        st.parse_gen += 1;
        st.parse_running = false;
        st.pending_deltas.clear();
        st.parsed = doc.syntax().cloned().map(|s| (s, version));
        st.structure = Slot {
            value: Some((
                version,
                Structure {
                    document: doc.clone(),
                    outline: doc.outline(),
                    folds: doc.fold_ranges(),
                },
            )),
            ..Slot::default()
        };
        st.live = doc;
        st.highlights = (version, HashMap::new());
        st.disk.stamp = lector_fs::stamp(&path).ok();
        self.lsp_job(st, Duration::ZERO, fx);
        fx.events.push(DocEvent::PathChanged { old, new: path });
    }

    /// The file changed on disk (watcher): same stamp → nothing (our own
    /// save); gone → deleted state; clean → reloaded in place (carets kept);
    /// dirty → a pending [`DiskConflict`]. Blocking (reads the file).
    pub fn check_disk(&self) -> Result<DiskChange, DocError> {
        self.reload_inner(false)
    }

    /// Discards unsaved changes and reloads from disk (Revert / "Reload" in
    /// a conflict). Blocking.
    pub fn reload(&self) -> Result<DiskChange, DocError> {
        self.reload_inner(true)
    }

    /// Keeps the buffer after an external change: the next save overwrites.
    pub fn keep_mine(&self) {
        let changed = {
            let mut st = self.shared.state.lock();
            match st.disk.conflict.take() {
                Some(conflict) => {
                    st.disk.stamp = conflict.stamp;
                    true
                }
                None => false,
            }
        };
        if changed {
            self.shared.subs.emit(vec![DocEvent::DiskStateChanged]);
        }
    }

    fn set_deleted(&self, deleted: bool) {
        let changed = {
            let mut st = self.shared.state.lock();
            std::mem::replace(&mut st.disk.deleted, deleted) != deleted
        };
        if changed {
            self.shared.subs.emit(vec![DocEvent::DiskStateChanged]);
        }
    }

    fn reload_inner(&self, discard: bool) -> Result<DiskChange, DocError> {
        let (project, path, known) = {
            let st = self.shared.state.lock();
            (st.project.clone(), st.path.clone(), st.disk.stamp)
        };
        match lector_fs::stamp(&path) {
            Ok(now) if !discard && known == Some(now) => {
                self.set_deleted(false);
                return Ok(DiskChange::Unchanged);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.set_deleted(true);
                return Ok(DiskChange::Deleted);
            }
            _ => {}
        }
        let loaded = load_from_disk(&project, &path, None)?;
        let mut fx = Fx::default();
        let change = {
            let mut st = self.shared.state.lock();
            if st.disk.deleted {
                st.disk.deleted = false;
                fx.events.push(DocEvent::DiskStateChanged);
            }
            self.apply_disk_version(&mut st, loaded, discard, &mut fx)
        };
        self.run(fx);
        Ok(change)
    }

    fn apply_disk_version(
        &self,
        st: &mut DocState,
        loaded: Loaded,
        discard: bool,
        fx: &mut Fx,
    ) -> DiskChange {
        let editable_now = is_editable_document(&loaded.doc);
        if st.buffer.is_none() || !editable_now {
            // Not editable (or became binary / huge): replace the snapshot.
            if st.dirty() && !discard {
                st.disk.conflict = Some(DiskConflict {
                    disk: loaded.doc,
                    stamp: loaded.stamp,
                });
                fx.events.push(DocEvent::DiskStateChanged);
                return DiskChange::Conflict;
            }
            let options = st
                .buffer
                .as_ref()
                .map(|b| b.options().clone())
                .unwrap_or_else(|| policy::buffer_options(&loaded.doc, &st.project.settings()));
            let path = st.path.clone();
            let key = st.key.clone();
            let project = st.project.clone();
            let mut fresh = new_state(&path, key, project, loaded, options);
            fresh.read_only = st.read_only;
            fresh.seq = st.seq + 1;
            // No history across a replacement: followers resynchronize.
            fresh.history.clear();
            fresh.parse_gen = st.parse_gen + 1;
            fresh.gutter_enabled = st.gutter_enabled;
            *st = fresh;
            fx.events.push(DocEvent::Replaced);
            fx.events.push(DocEvent::DiskStateChanged);
            return DiskChange::Reloaded;
        }
        let dirty = st.dirty();
        if dirty && !discard {
            st.disk.conflict = Some(DiskConflict {
                disk: loaded.doc,
                stamp: loaded.stamp,
            });
            fx.events.push(DocEvent::DiskStateChanged);
            return DiskChange::Conflict;
        }
        let Some(new_text) = loaded.doc.text().map(|t| t.to_string()) else {
            return DiskChange::Unchanged;
        };
        st.disk.conflict = None;
        st.disk.stamp = loaded.stamp;
        st.disk.encoding = loaded.encoding;
        let Some(buffer) = st.buffer.as_mut() else {
            return DiskChange::Unchanged;
        };
        let same = buffer.text().len() == new_text.len() && buffer.text().to_string() == new_text;
        if !same {
            let tx = Arc::new(buffer.replace_all(&new_text));
            if !tx.deltas.is_empty() {
                self.record_edit(st, EditOrigin::RELOAD, tx, false, fx);
            }
            // Adopt the full parse of the disk version.
            let version = st.version();
            st.parse_gen += 1;
            st.parse_running = false;
            st.pending_deltas.clear();
            st.parsed = loaded.doc.syntax().cloned().map(|s| (s, version));
            if let Some(text) = st.live.text().cloned() {
                st.live = loaded.doc.with_text(text, loaded.doc.syntax().cloned());
            }
            st.structure.value = Some((
                version,
                Structure {
                    document: loaded.doc.clone(),
                    outline: loaded.doc.outline(),
                    folds: loaded.doc.fold_ranges(),
                },
            ));
            st.structure.scheduled = None;
            st.structure.waiting_syntax = false;
            fx.events.push(DocEvent::Reloaded { version });
        }
        if let Some(b) = st.buffer.as_mut() {
            b.mark_saved();
        }
        st.gutter_base = None;
        st.blame.value = None;
        self.check_dirty(st, fx);
        fx.events.push(DocEvent::DiskStateChanged);
        DiskChange::Reloaded
    }
}

fn history_since(st: &DocState, since: u64) -> (Vec<HistoryEntry>, bool) {
    if since >= st.seq {
        return (Vec::new(), false);
    }
    match st.history.front() {
        Some(first) if first.seq <= since + 1 => (
            st.history
                .iter()
                .filter(|e| e.seq > since)
                .cloned()
                .collect(),
            false,
        ),
        _ => (Vec::new(), true),
    }
}

fn slot_result<T: Clone>(slot: &Slot<T>, version: u64) -> QueryResult<T> {
    match &slot.value {
        Some((v, value)) => QueryResult {
            value: Some(value.clone()),
            version: *v,
            current: *v == version,
        },
        None => QueryResult {
            value: None,
            version: 0,
            current: false,
        },
    }
}

fn map_result<T, U>(r: QueryResult<T>, f: impl FnOnce(T) -> U) -> QueryResult<U> {
    QueryResult {
        value: r.value.map(f),
        version: r.version,
        current: r.current,
    }
}

/// How many times each query ran (see [`DocHandle::compute_counts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueryCounts {
    /// Outline/folds computations.
    pub structure: u64,
    /// Gutter computations.
    pub gutter: u64,
    /// Blame mappings.
    pub blame: u64,
    /// Highlight chunks computed.
    pub highlight_chunks: u64,
}
