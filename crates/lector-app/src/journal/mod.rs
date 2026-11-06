//! The activity journal (v0.4): an append-only, persisted record of every write Lector
//! made to the user's files or repository, each with an [`UndoRecord`] when it can be
//! undone. "Undo instead of confirm": destructive commands back up what they will lose
//! into the shadow repository (`lector_git::ShadowRepo`) *before* writing, and the
//! journal turns that backup into an Undo button.
//!
//! * [`Journal`] implements [`ActivitySink`], so [`crate::CommandRegistry::invoke`] records
//!   every `writes` command automatically.
//! * Entries live in the database's journal table ([`DbStore`]) and survive restarts;
//!   the table is bounded ([`Journal::MAX_ENTRIES`] by default).
//! * [`Journal::undo`] executes the record: restores files or index entries from the
//!   shadow, or invokes the inverse command. It refuses with [`UndoError::Changed`]
//!   when the files changed since (the UI offers "Restore anyway" =
//!   [`UndoMode::Force`]), and records the undo itself as a new, again undoable entry
//!   (redo). The files are backed up again before an undo overwrites them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::SystemTime;

use lector_git::ShadowSnapshot;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::commands::{ActivityRecord, ActivitySink, Source};
use crate::core::AppCore;
use crate::db::Db;

mod undo;

pub use undo::{UndoError, UndoMode, UndoStatus};

/// How to undo a recorded write.
///
/// Paths are repository-relative (`/`-separated). The `after` maps are
/// per-path fingerprints of the state the command left behind
/// ([`lector_git::ShadowRepo::fingerprints`] / `index_fingerprints`): before restoring,
/// the journal checks they still hold, so an undo never silently throws away edits made
/// since. An empty map skips the check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoRecord {
    /// Put working-tree files back from the shadow snapshot (discard, accept-all,
    /// merge resolution; also the redo of an undo).
    RestoreFiles {
        /// The backup taken before the write.
        shadow: ShadowSnapshot,
        /// Paths to restore.
        paths: Vec<String>,
        /// Fingerprints of the files right after the write.
        #[serde(default)]
        after: BTreeMap<String, String>,
    },
    /// Undo by invoking another command (stage <-> unstage).
    Invoke {
        /// Command id.
        command: String,
        /// Its arguments.
        args: Value,
    },
    /// Put index entries back from the shadow snapshot (staging that has no simple inverse).
    RestoreIndex {
        /// The index backup taken before the write.
        shadow: ShadowSnapshot,
        /// Paths whose index entries are restored.
        paths: Vec<String>,
        /// Fingerprints of the index entries right after the write.
        #[serde(default)]
        after: BTreeMap<String, String>,
    },
    /// Several records undone together, last first (a write that touched files and the index).
    Multi {
        /// The records, in the order the writes happened.
        records: Vec<UndoRecord>,
    },
}

impl UndoRecord {
    /// Every shadow snapshot this record needs.
    pub fn snapshots(&self) -> Vec<&ShadowSnapshot> {
        match self {
            UndoRecord::RestoreFiles { shadow, .. } | UndoRecord::RestoreIndex { shadow, .. } => {
                vec![shadow]
            }
            UndoRecord::Invoke { .. } => Vec::new(),
            UndoRecord::Multi { records } => {
                records.iter().flat_map(UndoRecord::snapshots).collect()
            }
        }
    }
}

/// One journal entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Ids count up from 1 and are never reused.
    pub id: u64,
    /// When the write happened.
    pub time: SystemTime,
    /// Command id ("git.discard"); `journal.undo` for the entry an undo records.
    pub command: String,
    /// One line ("Discarded 3 hunks in parser.rs").
    pub summary: String,
    /// The repository work directory, if any.
    pub repo: Option<PathBuf>,
    /// Absolute paths written.
    pub paths: Vec<PathBuf>,
    /// How to undo it (`None`: not undoable).
    pub undo: Option<UndoRecord>,
    /// Whether it was undone.
    pub undone: bool,
    /// Arguments the command was invoked with.
    #[serde(default)]
    pub args: Value,
    /// Who invoked it.
    #[serde(default)]
    pub source: Source,
    /// For an entry that is itself an undo: the entry it undid.
    #[serde(default)]
    pub undo_of: Option<u64>,
    /// The entry that undid this one.
    #[serde(default)]
    pub undone_by: Option<u64>,
}

impl ActivityEntry {
    /// Whether this entry records an undo (so "undo last operation" skips it).
    pub fn is_undo(&self) -> bool {
        self.undo_of.is_some()
    }
}

/// Persistence of the journal: raw rows by id. Implemented over the database
/// ([`DbStore`]) and in memory ([`MemoryStore`], tests).
pub trait JournalStore: Send + Sync {
    /// Appends a row and returns its id (1, 2, ... never reused).
    fn append(&self, bytes: &[u8]) -> anyhow::Result<u64>;
    /// One row.
    fn get(&self, id: u64) -> anyhow::Result<Option<Vec<u8>>>;
    /// Replaces a row.
    fn put(&self, id: u64, bytes: &[u8]) -> anyhow::Result<()>;
    /// Up to `limit` rows with id < `before` (all when `None`), newest first.
    fn list(&self, limit: usize, before: Option<u64>) -> anyhow::Result<Vec<(u64, Vec<u8>)>>;
    /// Drops the oldest rows beyond `keep`.
    fn trim(&self, keep: usize) -> anyhow::Result<usize>;
}

/// [`JournalStore`] over the database's journal table.
pub struct DbStore(pub Db);

impl JournalStore for DbStore {
    fn append(&self, bytes: &[u8]) -> anyhow::Result<u64> {
        Ok(self.0.append_journal(bytes)?)
    }
    fn get(&self, id: u64) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.0.journal_get(id)?)
    }
    fn put(&self, id: u64, bytes: &[u8]) -> anyhow::Result<()> {
        Ok(self.0.journal_put(id, bytes)?)
    }
    fn list(&self, limit: usize, before: Option<u64>) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        Ok(self.0.journal_list(limit, before)?)
    }
    fn trim(&self, keep: usize) -> anyhow::Result<usize> {
        Ok(self.0.journal_trim(keep)?)
    }
}

/// [`JournalStore`] in memory.
#[derive(Default)]
pub struct MemoryStore {
    rows: Mutex<(u64, BTreeMap<u64, Vec<u8>>)>,
}

impl JournalStore for MemoryStore {
    fn append(&self, bytes: &[u8]) -> anyhow::Result<u64> {
        let mut rows = self.rows.lock();
        rows.0 += 1;
        let id = rows.0;
        rows.1.insert(id, bytes.to_vec());
        Ok(id)
    }
    fn get(&self, id: u64) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.rows.lock().1.get(&id).cloned())
    }
    fn put(&self, id: u64, bytes: &[u8]) -> anyhow::Result<()> {
        self.rows.lock().1.insert(id, bytes.to_vec());
        Ok(())
    }
    fn list(&self, limit: usize, before: Option<u64>) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        let rows = self.rows.lock();
        Ok(rows
            .1
            .range(..before.unwrap_or(u64::MAX))
            .rev()
            .take(limit)
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }
    fn trim(&self, keep: usize) -> anyhow::Result<usize> {
        let mut rows = self.rows.lock();
        let mut removed = 0;
        while rows.1.len() > keep {
            rows.1.pop_first();
            removed += 1;
        }
        Ok(removed)
    }
}

/// What changed in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalEvent {
    /// A new entry was appended.
    Recorded(u64),
    /// An entry changed (marked undone, ...).
    Updated(u64),
}

type Callback = Arc<dyn Fn(&JournalEvent) + Send + Sync>;

struct Inner {
    store: Arc<dyn JournalStore>,
    data_dir: PathBuf,
    max_entries: usize,
    /// Serializes read-modify-write of rows.
    write_lock: Mutex<()>,
    /// Serializes undos.
    undo_lock: Mutex<()>,
    subscribers: Mutex<Vec<(u64, Callback)>>,
    next_subscription: AtomicU64,
}

/// The activity journal. Cheap to clone (shared).
#[derive(Clone)]
pub struct Journal {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("data_dir", &self.inner.data_dir)
            .finish()
    }
}

/// Keeps a [`Journal::subscribe`] callback alive; dropping it unsubscribes.
#[must_use = "dropping a Subscription unsubscribes"]
pub struct Subscription {
    id: u64,
    inner: Weak<Inner>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.subscribers.lock().retain(|(id, _)| *id != self.id);
        }
    }
}

impl Journal {
    /// Entries kept by default; older ones are dropped as new ones arrive.
    pub const MAX_ENTRIES: usize = 2000;

    /// A journal over `store`. `data_dir` is where the shadow repositories live
    /// (`lector_config::paths::data_dir()`).
    pub fn new(store: Arc<dyn JournalStore>, data_dir: PathBuf) -> Journal {
        Journal::with_limit(store, data_dir, Self::MAX_ENTRIES)
    }

    /// Like [`Journal::new`] with an explicit entry limit.
    pub fn with_limit(store: Arc<dyn JournalStore>, data_dir: PathBuf, max: usize) -> Journal {
        Journal {
            inner: Arc::new(Inner {
                store,
                data_dir,
                max_entries: max.max(1),
                write_lock: Mutex::new(()),
                undo_lock: Mutex::new(()),
                subscribers: Mutex::new(Vec::new()),
                next_subscription: AtomicU64::new(1),
            }),
        }
    }

    /// The core's journal, created on first use over its database and installed as the
    /// activity sink (so every write command is recorded). Idempotent.
    pub fn install(core: &AppCore) -> Journal {
        let journal = core.extension_or_init(|| {
            Arc::new(Journal::new(
                Arc::new(DbStore(core.db().clone())),
                lector_config::paths::data_dir(),
            ))
        });
        if core.activity_sink().is_none() {
            core.set_activity_sink(Arc::new(journal.as_ref().clone()));
        }
        journal.as_ref().clone()
    }

    /// Like [`Journal::install`], with `data_dir` for the shadow repositories (tests).
    pub fn install_in(core: &AppCore, data_dir: PathBuf) -> Journal {
        let journal = Journal::new(Arc::new(DbStore(core.db().clone())), data_dir);
        core.set_extension(Arc::new(journal.clone()));
        core.set_activity_sink(Arc::new(journal.clone()));
        journal
    }

    /// The journal installed in `core`, if any.
    pub fn of(core: &AppCore) -> Option<Journal> {
        core.extension::<Journal>().map(|j| j.as_ref().clone())
    }

    /// Where shadow repositories live.
    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    fn decode(id: u64, bytes: &[u8]) -> Option<ActivityEntry> {
        match serde_json::from_slice::<ActivityEntry>(bytes) {
            Ok(mut entry) => {
                entry.id = id;
                Some(entry)
            }
            Err(err) => {
                tracing::warn!("unreadable journal entry {id}: {err}");
                None
            }
        }
    }

    /// The newest `limit` entries, newest first.
    pub fn list(&self, limit: usize) -> Vec<ActivityEntry> {
        self.list_before(limit, None)
    }

    /// Up to `limit` entries older than `before`, newest first (paging).
    pub fn list_before(&self, limit: usize, before: Option<u64>) -> Vec<ActivityEntry> {
        match self.inner.store.list(limit, before) {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(id, bytes)| Self::decode(id, &bytes))
                .collect(),
            Err(err) => {
                tracing::warn!("cannot read the activity journal: {err}");
                Vec::new()
            }
        }
    }

    /// One entry.
    pub fn get(&self, id: u64) -> Option<ActivityEntry> {
        let bytes = self.inner.store.get(id).ok()??;
        Self::decode(id, &bytes)
    }

    /// Appends an entry (the `id` field is ignored) and returns its id.
    pub fn append(&self, mut entry: ActivityEntry) -> Option<u64> {
        entry.id = 0;
        let bytes = match serde_json::to_vec(&entry) {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!("cannot encode a journal entry: {err}");
                return None;
            }
        };
        let id = {
            let _guard = self.inner.write_lock.lock();
            let id = match self.inner.store.append(&bytes) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!("cannot write the activity journal: {err}");
                    return None;
                }
            };
            if let Err(err) = self.inner.store.trim(self.inner.max_entries) {
                tracing::warn!("cannot trim the activity journal: {err}");
            }
            id
        };
        self.notify(JournalEvent::Recorded(id));
        Some(id)
    }

    /// Read-modify-write of one entry.
    pub(crate) fn update(&self, id: u64, f: impl FnOnce(&mut ActivityEntry)) -> bool {
        let changed = {
            let _guard = self.inner.write_lock.lock();
            let Some(mut entry) = self.get(id) else {
                return false;
            };
            f(&mut entry);
            entry.id = 0;
            match serde_json::to_vec(&entry) {
                Ok(bytes) => self.inner.store.put(id, &bytes).is_ok(),
                Err(_) => false,
            }
        };
        if changed {
            self.notify(JournalEvent::Updated(id));
        }
        changed
    }

    /// Calls `callback` for every new or changed entry (from the thread that wrote it)
    /// until the returned [`Subscription`] is dropped.
    pub fn subscribe(
        &self,
        callback: impl Fn(&JournalEvent) + Send + Sync + 'static,
    ) -> Subscription {
        let id = self.inner.next_subscription.fetch_add(1, Ordering::Relaxed);
        self.inner.subscribers.lock().push((id, Arc::new(callback)));
        Subscription {
            id,
            inner: Arc::downgrade(&self.inner),
        }
    }

    fn notify(&self, event: JournalEvent) {
        let callbacks: Vec<Callback> = self
            .inner
            .subscribers
            .lock()
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in callbacks {
            cb(&event);
        }
    }

    /// The newest entry "Undo last operation" would undo: undoable, not undone, not
    /// itself an undo; restricted to `repo` when given.
    pub fn last_undoable(&self, repo: Option<&Path>) -> Option<ActivityEntry> {
        let mut before = None;
        // Bounded walk: do not scan the whole journal for a repository with nothing to undo.
        for _ in 0..40 {
            let page = self.list_before(50, before);
            let last = page.last().map(|e| e.id)?;
            let hit = page.into_iter().find(|e| {
                e.undo.is_some()
                    && !e.undone
                    && !e.is_undo()
                    && repo.is_none_or(|r| e.repo.as_deref() == Some(r))
            });
            if hit.is_some() {
                return hit;
            }
            before = Some(last);
        }
        None
    }
}

impl ActivitySink for Journal {
    fn record(&self, record: ActivityRecord) -> Option<u64> {
        self.append(ActivityEntry {
            id: 0,
            time: SystemTime::now(),
            command: record.command,
            summary: record.summary,
            repo: record.repo,
            paths: record.paths,
            undo: record.undo,
            undone: false,
            args: record.args,
            source: record.source,
            undo_of: None,
            undone_by: None,
        })
    }
}

#[cfg(test)]
mod tests;
