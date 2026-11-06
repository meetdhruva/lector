//! The headless document store (v0.4): one live document per canonical path,
//! shared by every view, window, agent, CLI call and test.
//!
//! * [`DocumentStore`] opens files ([`DocumentStore::open`]) and hands out
//!   [`DocHandle`]s; every handle of a path is the same document.
//! * Edits go through a `lector_buffer::Buffer` and produce `Transaction`s
//!   ([`DocHandle::edit_from`], [`DocHandle::apply_edits`]); each edit gets a
//!   monotonic `seq` and is kept in a short history so views (and LSP
//!   diagnostics) can follow it.
//! * Disk state: saves are atomic and keep encoding, line endings and
//!   permissions; a file changed on disk since load is a typed error
//!   ([`DocError::ChangedOnDisk`]); external changes reload a clean document,
//!   turn a dirty one into a [`DiskConflict`], and track deletion.
//! * Derived data are **versioned queries** computed on the executor
//!   (the shared scheduler in the app): syntax (incremental reparse),
//!   highlights per row chunk, outline/folds/scopes, diff gutter vs HEAD,
//!   blame mapped onto the buffer, diagnostics mapped through edits. Each is
//!   cached by text version, never blocks, and the newest request wins.
//! * Front-ends subscribe ([`DocHandle::subscribe`]) and render snapshots.
//!
//! See `docs/ARCHITECTURE.md`, "lector-app — documents".

mod derive;
mod exec;
mod policy;
mod store;
#[cfg(test)]
mod tests;

pub use derive::{map_offset, map_range, map_row, remap_blame, DocDiagnostic};
pub use exec::{DocExecutor, DocJob, InlineExecutor};
pub use policy::{
    auto_save_decision, buffer_options, cleanup_edits, AutoSaveDecision, AutoSaveInputs,
    AutoSaveTrigger, MIN_AUTO_SAVE_DELAY,
};
pub use store::{
    canonical_key, is_editable_document, BeforeSaveHook, BufferView, CatchUp, ChunkHighlights,
    DiskChange, DiskConflict, DiskState, DocError, DocEvent, DocHandle, DocId, DocSnapshot,
    DocumentStore, EditOrigin, EditResult, HistoryEntry, HookId, OpenOptions, QueryCounts,
    QueryDelays, QueryKind, QueryResult, SaveOutcome, SaveRequest, Structure, Subscription,
    HIGHLIGHT_CHUNK, HISTORY_LEN,
};
