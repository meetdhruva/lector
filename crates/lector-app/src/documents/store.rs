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

use crate::{Document, LineEnding, LineHighlights, Project};
use lector_buffer::save::Encoding;
use lector_buffer::{Buffer, BufferOptions, EditDelta, Transaction};
use lector_diff::GutterMark;
use lector_fs::{FileKind, FileStamp};
use lector_git::BlameHunk;
use lector_syntax::{OutlineItem, Syntax, SyntaxEdit};
use lector_text::Rope;
use parking_lot::{Mutex, RwLock};

use super::derive::DocDiagnostic;
use super::exec::{DocExecutor, DocJob};
use super::policy;
use crate::scheduler::{JobKey, JobKind, Priority, Subject};

mod disk;
mod edit;
mod queries;
mod types;

pub use types::*;

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
    jobs: Vec<(Priority, JobKey, Option<Duration>, DocJob)>,
}

impl Fx {
    fn spawn(&mut self, prio: Priority, key: JobKey, job: DocJob) {
        self.jobs.push((prio, key, None, job));
    }
    fn debounce(&mut self, key: JobKey, delay: Duration, prio: Priority, job: DocJob) {
        self.jobs.push((prio, key, Some(delay), job));
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
        self.executor.cancel_subject(&Subject::Document(self.id.0));
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

    fn delays(&self) -> QueryDelays {
        self.shared
            .store
            .upgrade()
            .map(|s| *s.delays.read())
            .unwrap_or_default()
    }

    fn key(&self, kind: JobKind) -> JobKey {
        JobKey::new(kind, Subject::Document(self.shared.id.0))
    }

    fn run(&self, fx: Fx) {
        for (prio, key, delay, job) in fx.jobs {
            match delay {
                Some(delay) => self.shared.executor.debounce(key, delay, prio, job),
                None => self.shared.executor.spawn(prio, key, job),
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
