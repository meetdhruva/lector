//! Lector's embedded database (redb, pure Rust): the storage under [`crate::store`].
//!
//! Tables: `meta` (schema version, flags), `review` (viewed marks), `comments`,
//! `sessions`, `recent`, `kv`, `cache` (bytes by key, LRU-trimmed) and `journal`
//! (append-only activity log, owned by the journal module). The typed, versioned
//! concerns of [`crate::store`] add their own tables (`store_meta`,
//! `terminal_sessions`, `notices`, `update_state`).
//!
//! **Opening.** [`Db::open`] opens a file strictly (an error when it cannot). The policy
//! for the user's database — sharing with another Lector process, read-only state when
//! the file is locked, copying a corrupt file aside — is [`crate::store::Store::open`].
//! One process opens the file once (the [`crate::AppCore`] holds it; every window shares
//! it). Processes open it in redb's `MultiWriter` mode where the platform supports
//! byte-range locks, so two Lector instances share one database.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{
    Builder, ConcurrencyMode, Database, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Current schema version.
pub const SCHEMA_VERSION: u64 = 1;
/// Cache budget in bytes before LRU trimming.
pub const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
pub(crate) const REVIEW: TableDefinition<&str, &[u8]> = TableDefinition::new("review");
pub(crate) const COMMENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("comments");
pub(crate) const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
pub(crate) const RECENT: TableDefinition<&str, u64> = TableDefinition::new("recent");
pub(crate) const KV: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");
const CACHE_NAME: &str = "cache";
pub(crate) const CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new(CACHE_NAME);
pub(crate) const JOURNAL: TableDefinition<u64, &[u8]> = TableDefinition::new("journal");

const KEY_VERSION: &str = "schema_version";
/// Set by 0.4 to 0.7.1 once the v0.3 JSON stores were copied into the database.
pub(crate) const KEY_IMPORTED_V03: &str = "imported_v03";
const KEY_COMMENT_SEQ: &str = "comment_seq";

/// Database errors.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// redb failure (storage, transaction, table).
    #[error("database: {0}")]
    Redb(String),
    /// File system error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// (De)serialization of a stored value.
    #[error("stored value: {0}")]
    Json(#[from] serde_json::Error),
    /// The file was written by a newer Lector.
    #[error("database schema {found} is newer than supported ({supported})")]
    TooNew {
        /// Schema version in the file.
        found: u64,
        /// [`SCHEMA_VERSION`].
        supported: u64,
    },
}

macro_rules! from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for DbError {
            fn from(e: $t) -> Self {
                DbError::Redb(e.to_string())
            }
        }
    )*};
}
from_redb!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError
);

/// `Result` with [`DbError`].
pub type DbResult<T> = Result<T, DbError>;

/// Where the database lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbMode {
    /// The shared file.
    File(PathBuf),
    /// The file could not be opened for writing (locked by another process that excludes
    /// this one, written by a newer Lector, not writable...): the data is held in memory
    /// for this process only, seeded from the file when it could at least be read. Nothing
    /// is ever written back to `path` or anywhere else.
    ReadOnly {
        /// The file that could not be opened for writing.
        path: PathBuf,
        /// Why, for the log and the UI.
        reason: String,
    },
    /// In memory only (tests, `--no-state`).
    Memory,
}

/// Called once, at the first state write of a process whose state is read-only.
type ReadOnlyWriteHook = Box<dyn FnOnce() + Send>;

struct Inner {
    db: Database,
    mode: DbMode,
    cache_puts: AtomicU32,
    read_only_write: parking_lot::Mutex<Option<ReadOnlyWriteHook>>,
}

/// Handle to the database. Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("mode", &self.inner.mode)
            .finish()
    }
}

/// A review comment (anchored to a line of a file in a changeset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    /// Unique id (assigned by [`Db::add_comment`]).
    pub id: u64,
    /// Repository work directory.
    pub repo: String,
    /// Changeset key (`lector_review::review::changeset_key`).
    pub changeset: String,
    /// Repo-relative path.
    pub path: String,
    /// 0-based line in the new side.
    pub line: u32,
    /// Text of the anchored line (for re-anchoring across revisions).
    #[serde(default)]
    pub anchor_text: String,
    /// Markdown body.
    pub body: String,
    /// Unix seconds.
    pub created: i64,
    /// Resolved / hidden.
    #[serde(default)]
    pub resolved: bool,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub(crate) fn builder(mode: ConcurrencyMode) -> Builder {
    let mut b = Builder::new();
    b.set_cache_size(16 * 1024 * 1024);
    b.set_concurrency_mode(mode);
    b
}

/// Opens (or creates) `path` in `MultiWriter` mode, or `ExclusiveWriter` where the platform
/// has no byte-range locks. The raw redb error is returned so that the caller can tell a
/// lock from corruption ([`crate::store::Store::open`]).
pub(crate) fn open_file(path: &Path) -> Result<Database, redb::DatabaseError> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    create_private(path)?;
    if !cfg!(any(target_os = "linux", target_os = "macos", windows)) {
        return builder(ConcurrencyMode::ExclusiveWriter).create(path);
    }
    match builder(ConcurrencyMode::MultiWriter).create(path) {
        Err(redb::DatabaseError::Storage(redb::StorageError::Unsupported)) => {
            tracing::debug!(
                "no byte-range locks for {}; opening exclusively",
                path.display()
            );
            builder(ConcurrencyMode::ExclusiveWriter).create(path)
        }
        other => other,
    }
}

/// Creates `path` empty and readable by its owner only (0600 on Unix, whatever the umask: the
/// database holds comments, the journal and project paths) unless it exists; an existing file
/// keeps the mode its owner gave it. redb initializes an empty file as a new database.
pub(crate) fn create_private(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

impl Db {
    /// Opens (or creates) the database file and migrates it. Strict: any failure is an
    /// error (the user's database is opened with [`crate::store::Store::open`]).
    pub fn open(path: &Path) -> DbResult<Db> {
        let db = open_file(path)?;
        Db::init(db, DbMode::File(path.to_path_buf()))
    }

    /// A handle over an already opened database, migrated.
    pub(crate) fn from_database(db: Database, mode: DbMode) -> DbResult<Db> {
        Db::init(db, mode)
    }

    /// A handle over an in-memory snapshot, without the schema check (the snapshot may come
    /// from a newer file; it is read-only state for this process).
    pub(crate) fn from_snapshot(db: Database, mode: DbMode) -> Db {
        Db {
            inner: Arc::new(Inner {
                db,
                mode,
                cache_puts: AtomicU32::new(0),
                read_only_write: parking_lot::Mutex::new(None),
            }),
        }
    }

    /// The redb handle (for [`crate::store`]).
    pub(crate) fn raw(&self) -> &Database {
        &self.inner.db
    }

    /// A write transaction for a change of Lector's state. In read-only state (the change is
    /// kept in memory only) the first one runs the hook set with
    /// [`Db::on_read_only_write`]. Cache writes and internal setup use [`Db::raw`].
    pub(crate) fn begin_write(&self) -> Result<redb::WriteTransaction, redb::TransactionError> {
        if matches!(self.inner.mode, DbMode::ReadOnly { .. }) {
            let hook = self.inner.read_only_write.lock().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        self.inner.db.begin_write()
    }

    /// Sets what runs at the first state write while the state is read-only (the CLI prints a
    /// warning on stderr). Replaces an earlier hook; never runs in read-write state.
    pub fn on_read_only_write(&self, hook: impl FnOnce() + Send + 'static) {
        *self.inner.read_only_write.lock() = Some(Box::new(hook));
    }

    /// An in-memory database (tests; last-resort fallback).
    pub fn in_memory() -> Db {
        let attempt = Builder::new()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(DbError::from)
            .and_then(|db| Db::init(db, DbMode::Memory));
        match attempt {
            Ok(db) => db,
            // Allocation failure of an empty in-memory database: nothing sensible remains.
            Err(err) => panic!("cannot create an in-memory database: {err}"),
        }
    }

    fn init(db: Database, mode: DbMode) -> DbResult<Db> {
        let this = Db {
            inner: Arc::new(Inner {
                db,
                mode,
                cache_puts: AtomicU32::new(0),
                read_only_write: parking_lot::Mutex::new(None),
            }),
        };
        this.migrate()?;
        Ok(this)
    }

    /// Where the data lives.
    pub fn mode(&self) -> &DbMode {
        &self.inner.mode
    }

    /// Schema version stored in the file.
    pub fn schema_version(&self) -> DbResult<u64> {
        let tx = self.inner.db.begin_read()?;
        let t = tx.open_table(META)?;
        Ok(t.get(KEY_VERSION)?.map(|g| g.value()).unwrap_or(0))
    }

    fn migrate(&self) -> DbResult<()> {
        let tx = self.inner.db.begin_write()?; // setup, not a state change
        {
            let mut meta = tx.open_table(META)?;
            let version = meta.get(KEY_VERSION)?.map(|g| g.value()).unwrap_or(0);
            if version > SCHEMA_VERSION {
                return Err(DbError::TooNew {
                    found: version,
                    supported: SCHEMA_VERSION,
                });
            }
            // v0 -> v1: create every table. Later migrations go here, each step
            // `if version < N { ...; }`, always inside this one transaction.
            if version < 1 {
                tx.open_table(REVIEW)?;
                tx.open_table(COMMENTS)?;
                tx.open_table(SESSIONS)?;
                tx.open_table(RECENT)?;
                tx.open_table(KV)?;
                tx.open_table(CACHE)?;
                tx.open_table(JOURNAL)?;
            }
            if version != SCHEMA_VERSION {
                meta.insert(KEY_VERSION, SCHEMA_VERSION)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ----- generic helpers -------------------------------------------------

    pub(crate) fn get_bytes(
        &self,
        def: TableDefinition<&str, &[u8]>,
        key: &str,
    ) -> DbResult<Option<Vec<u8>>> {
        let tx = self.inner.db.begin_read()?;
        let t = tx.open_table(def)?;
        Ok(t.get(key)?.map(|g| g.value().to_vec()))
    }

    pub(crate) fn put_bytes(
        &self,
        def: TableDefinition<&str, &[u8]>,
        key: &str,
        value: Option<&[u8]>,
    ) -> DbResult<()> {
        let tx = if redb::TableHandle::name(&def) == CACHE_NAME {
            self.inner.db.begin_write()?
        } else {
            self.begin_write()?
        };
        {
            let mut t = tx.open_table(def)?;
            match value {
                Some(v) => {
                    t.insert(key, v)?;
                }
                None => {
                    t.remove(key)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn scan_prefix(
        &self,
        def: TableDefinition<&str, &[u8]>,
        prefix: &str,
    ) -> DbResult<Vec<(String, Vec<u8>)>> {
        let tx = self.inner.db.begin_read()?;
        let t = tx.open_table(def)?;
        let mut out = Vec::new();
        for item in t.range::<&str>(prefix..)? {
            let (k, v) = item?;
            let k = k.value();
            if !k.starts_with(prefix) {
                break;
            }
            out.push((k.to_string(), v.value().to_vec()));
        }
        Ok(out)
    }

    // ----- kv ----------------------------------------------------------------

    /// Raw value of a `kv` key.
    pub fn kv_get(&self, key: &str) -> DbResult<Option<Vec<u8>>> {
        self.get_bytes(KV, key)
    }

    /// Sets (`Some`) or removes (`None`) a `kv` key.
    pub fn kv_put(&self, key: &str, value: Option<&[u8]>) -> DbResult<()> {
        self.put_bytes(KV, key, value)
    }

    /// A JSON value from `kv` (None when missing or unreadable).
    pub fn kv_get_json<T: DeserializeOwned>(&self, key: &str) -> DbResult<Option<T>> {
        Ok(match self.kv_get(key)? {
            Some(b) => Some(serde_json::from_slice(&b)?),
            None => None,
        })
    }

    /// Stores a JSON value in `kv`.
    pub fn kv_put_json<T: Serialize>(&self, key: &str, value: &T) -> DbResult<()> {
        let bytes = serde_json::to_vec(value)?;
        self.kv_put(key, Some(&bytes))
    }

    /// All `kv` entries whose key starts with `prefix`, sorted by key.
    pub fn kv_scan(&self, prefix: &str) -> DbResult<Vec<(String, Vec<u8>)>> {
        self.scan_prefix(KV, prefix)
    }

    // ----- cache ---------------------------------------------------------------

    /// Cached bytes for `key` (content-addressed caches: syntax, diffs, ...).
    pub fn cache_get(&self, key: &str) -> DbResult<Option<Vec<u8>>> {
        Ok(self
            .get_bytes(CACHE, key)?
            .and_then(|v| (v.len() >= 8).then(|| v[8..].to_vec())))
    }

    /// Stores cache bytes; the cache is trimmed (oldest first) beyond [`CACHE_MAX_BYTES`].
    pub fn cache_put(&self, key: &str, value: &[u8]) -> DbResult<()> {
        let mut stored = Vec::with_capacity(value.len() + 8);
        stored.extend_from_slice(&now_nanos().to_le_bytes());
        stored.extend_from_slice(value);
        self.put_bytes(CACHE, key, Some(&stored))?;
        if self.inner.cache_puts.fetch_add(1, Ordering::Relaxed) % 64 == 63 {
            self.trim_cache(CACHE_MAX_BYTES)?;
        }
        Ok(())
    }

    /// Removes the least recently written entries until the cache holds at most
    /// `max_bytes`. Returns how many were removed.
    pub fn trim_cache(&self, max_bytes: u64) -> DbResult<usize> {
        let tx = self.inner.db.begin_write()?; // the cache is not state
        let removed;
        {
            let mut t = tx.open_table(CACHE)?;
            let mut entries = Vec::new();
            let mut total = 0u64;
            for item in t.iter()? {
                let (k, v) = item?;
                let v = v.value();
                let at = v
                    .get(..8)
                    .and_then(|b| b.try_into().ok())
                    .map(u64::from_le_bytes)
                    .unwrap_or(0);
                total += v.len() as u64;
                entries.push((at, v.len() as u64, k.value().to_string()));
            }
            entries.sort();
            let mut n = 0;
            for (_, len, key) in entries {
                if total <= max_bytes {
                    break;
                }
                t.remove(key.as_str())?;
                total -= len;
                n += 1;
            }
            removed = n;
        }
        tx.commit()?;
        Ok(removed)
    }

    // ----- journal -------------------------------------------------------------

    /// Appends an entry to the journal; returns its id (1, 2, ...).
    pub fn append_journal(&self, bytes: &[u8]) -> DbResult<u64> {
        let tx = self.begin_write()?;
        let id;
        {
            let mut t = tx.open_table(JOURNAL)?;
            let last = t.last()?.map(|(k, _)| k.value()).unwrap_or(0);
            id = last + 1;
            t.insert(id, bytes)?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// One journal entry.
    pub fn journal_get(&self, id: u64) -> DbResult<Option<Vec<u8>>> {
        let tx = self.inner.db.begin_read()?;
        let t = tx.open_table(JOURNAL)?;
        Ok(t.get(id)?.map(|g| g.value().to_vec()))
    }

    /// Replaces a journal entry (e.g. to mark it undone).
    pub fn journal_put(&self, id: u64, bytes: &[u8]) -> DbResult<()> {
        let tx = self.begin_write()?;
        {
            let mut t = tx.open_table(JOURNAL)?;
            t.insert(id, bytes)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Up to `limit` entries with id < `before` (all when `None`), newest first.
    pub fn journal_list(&self, limit: usize, before: Option<u64>) -> DbResult<Vec<(u64, Vec<u8>)>> {
        let tx = self.inner.db.begin_read()?;
        let t = tx.open_table(JOURNAL)?;
        let mut out = Vec::new();
        let range = t.range(..before.unwrap_or(u64::MAX))?;
        for item in range.rev() {
            if out.len() >= limit {
                break;
            }
            let (k, v) = item?;
            out.push((k.value(), v.value().to_vec()));
        }
        Ok(out)
    }

    /// Deletes the oldest journal entries beyond `keep`; returns how many went.
    pub fn journal_trim(&self, keep: usize) -> DbResult<usize> {
        let tx = self.begin_write()?;
        let mut removed = 0;
        {
            let mut t = tx.open_table(JOURNAL)?;
            while t.len()? as usize > keep {
                if t.pop_first()?.is_none() {
                    break;
                }
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    // ----- comments ------------------------------------------------------------

    fn comment_key(repo: &str, id: u64) -> String {
        format!("{repo}\0{id:020}")
    }

    /// Adds a comment, assigning its id.
    pub fn add_comment(&self, mut comment: Comment) -> DbResult<Comment> {
        let tx = self.begin_write()?;
        {
            let mut meta = tx.open_table(META)?;
            let id = meta.get(KEY_COMMENT_SEQ)?.map(|g| g.value()).unwrap_or(0) + 1;
            meta.insert(KEY_COMMENT_SEQ, id)?;
            comment.id = id;
            if comment.created == 0 {
                comment.created = now_secs();
            }
            let bytes = serde_json::to_vec(&comment)?;
            let mut t = tx.open_table(COMMENTS)?;
            t.insert(
                Self::comment_key(&comment.repo, id).as_str(),
                bytes.as_slice(),
            )?;
        }
        tx.commit()?;
        Ok(comment)
    }

    /// Replaces a comment (same repo and id).
    pub fn update_comment(&self, comment: &Comment) -> DbResult<()> {
        let bytes = serde_json::to_vec(comment)?;
        self.put_bytes(
            COMMENTS,
            &Self::comment_key(&comment.repo, comment.id),
            Some(&bytes),
        )
    }

    /// Deletes a comment.
    pub fn delete_comment(&self, repo: &str, id: u64) -> DbResult<()> {
        self.put_bytes(COMMENTS, &Self::comment_key(repo, id), None)
    }

    /// Comments of a repository, optionally of one changeset, oldest first.
    pub fn comments(&self, repo: &str, changeset: Option<&str>) -> DbResult<Vec<Comment>> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan_prefix(COMMENTS, &format!("{repo}\0"))? {
            match serde_json::from_slice::<Comment>(&bytes) {
                Ok(c) if changeset.is_none_or(|cs| c.changeset == cs) => out.push(c),
                Ok(_) => {}
                Err(err) => tracing::warn!("skipping unreadable comment: {err}"),
            }
        }
        Ok(out)
    }

    // ----- comment threads (v0.6, `lector_app::comments`) ----------------------
    // Stored in the `comments` table under `t\0<repo>\0<id>`; legacy [`Comment`]
    // rows use `<repo>\0<id>` (repos are absolute paths, so prefixes never collide).

    fn thread_key(repo: &str, id: u64) -> String {
        format!("t\0{repo}\0{id:020}")
    }

    /// Next id for a comment thread or comment (shared sequence with legacy comments).
    pub fn next_comment_id(&self) -> DbResult<u64> {
        let tx = self.begin_write()?;
        let id;
        {
            let mut meta = tx.open_table(META)?;
            id = meta.get(KEY_COMMENT_SEQ)?.map(|g| g.value()).unwrap_or(0) + 1;
            meta.insert(KEY_COMMENT_SEQ, id)?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Stores (or with `None` deletes) a serialized comment thread.
    pub fn put_comment_thread(&self, repo: &str, id: u64, value: Option<&[u8]>) -> DbResult<()> {
        self.put_bytes(COMMENTS, &Self::thread_key(repo, id), value)
    }

    /// One serialized comment thread.
    pub fn comment_thread(&self, repo: &str, id: u64) -> DbResult<Option<Vec<u8>>> {
        self.get_bytes(COMMENTS, &Self::thread_key(repo, id))
    }

    /// Every serialized comment thread of a repository, oldest first.
    pub fn comment_threads(&self, repo: &str) -> DbResult<Vec<Vec<u8>>> {
        Ok(self
            .scan_prefix(COMMENTS, &format!("t\0{repo}\0"))?
            .into_iter()
            .map(|(_, v)| v)
            .collect())
    }
}

#[cfg(test)]
mod tests;
