//! The public value types of the document store: ids, snapshots, events, options and errors.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lector_base::CancelToken;
use lector_buffer::save::Encoding;
use lector_buffer::{Buffer, BufferOptions, Selection, TextEdit, Transaction};
use lector_fs::FileStamp;
use lector_syntax::Syntax;
use lector_text::{Rope, Text};

use crate::{Document, LineHighlights};

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
    pub(super) fn of(buffer: &Buffer, seq: u64) -> BufferView {
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
