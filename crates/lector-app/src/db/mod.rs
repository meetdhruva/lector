//! Lector's embedded database (redb, pure Rust) at `data_dir()/lector.redb`.
//!
//! Tables: `meta` (schema version, flags), `review` (viewed marks), `comments`,
//! `sessions`, `recent`, `kv`, `cache` (bytes by key, LRU-trimmed) and `journal`
//! (append-only activity log, owned by the journal module).
//!
//! **Multiple windows / processes.** One process opens the file once (the
//! [`crate::AppCore`] holds it; every window shares it). Processes open it in
//! redb's `MultiWriter` mode where the platform supports byte-range locks, so two
//! Lector instances share one database. If the file cannot be opened (another
//! process holds an exclusive lock, an incompatible redb, corruption), the
//! database falls back to a per-process temporary copy and finally to memory —
//! it never crashes the app; [`Db::mode`] says which.
//!
//! On first open the v0.3 JSON stores (`review.json`, `sessions/*.json`,
//! `recent.json`) are imported once ([`Db::import_v03`]); the files are left in
//! place.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{
    Builder, ConcurrencyMode, Database, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

mod stores;
pub use stores::{DbRecentProjects, DbReviewBackend, DbSessionStore};

/// Current schema version.
pub const SCHEMA_VERSION: u64 = 1;
/// Cache budget in bytes before LRU trimming.
pub const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const REVIEW: TableDefinition<&str, &[u8]> = TableDefinition::new("review");
const COMMENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("comments");
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
const RECENT: TableDefinition<&str, u64> = TableDefinition::new("recent");
const KV: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");
const CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new("cache");
const JOURNAL: TableDefinition<u64, &[u8]> = TableDefinition::new("journal");

const KEY_VERSION: &str = "schema_version";
const KEY_IMPORTED: &str = "imported_v03";
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
    /// A private copy (the shared file could not be opened); changes are not shared.
    TempCopy {
        /// The file that could not be opened.
        original: PathBuf,
        /// The copy in use.
        copy: PathBuf,
    },
    /// In memory only (tests, or nothing else worked).
    Memory,
}

struct Inner {
    db: Database,
    mode: DbMode,
    cache_puts: AtomicU32,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let DbMode::TempCopy { copy, .. } = &self.mode {
            let _ = std::fs::remove_file(copy);
        }
    }
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
    /// Changeset key (`lector_workspace::review::changeset_key`).
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

fn builder(mode: ConcurrencyMode) -> Builder {
    let mut b = Builder::new();
    b.set_cache_size(16 * 1024 * 1024);
    b.set_concurrency_mode(mode);
    b
}

fn open_file(path: &Path) -> DbResult<Database> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let multi = if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
        builder(ConcurrencyMode::MultiWriter).create(path)
    } else {
        builder(ConcurrencyMode::ExclusiveWriter).create(path)
    };
    match multi {
        Ok(db) => Ok(db),
        Err(err) => {
            tracing::debug!("multi-writer open failed ({err}); trying exclusive");
            Ok(builder(ConcurrencyMode::ExclusiveWriter).create(path)?)
        }
    }
}

impl Db {
    /// Opens (or creates) the database file and migrates it.
    pub fn open(path: &Path) -> DbResult<Db> {
        let db = open_file(path)?;
        Db::init(db, DbMode::File(path.to_path_buf()))
    }

    /// Opens `path`; if that fails, a private temporary copy of it; if that
    /// fails too, an in-memory database. Never fails.
    pub fn open_or_fallback(path: &Path) -> Db {
        match Db::open(path) {
            Ok(db) => return db,
            Err(err) => tracing::warn!(
                "cannot open {} ({err}); using a private copy",
                path.display()
            ),
        }
        let copy = std::env::temp_dir().join(format!(
            "lector-{}-{}.redb",
            std::process::id(),
            now_nanos()
        ));
        let copied = if path.exists() {
            std::fs::copy(path, &copy).map(|_| ())
        } else {
            Ok(())
        };
        if copied.is_ok() {
            let attempt = builder(ConcurrencyMode::ExclusiveWriter)
                .create(&copy)
                .map_err(DbError::from)
                .and_then(|db| {
                    Db::init(
                        db,
                        DbMode::TempCopy {
                            original: path.to_path_buf(),
                            copy: copy.clone(),
                        },
                    )
                });
            match attempt {
                Ok(db) => return db,
                Err(err) => {
                    tracing::warn!("cannot open a private copy ({err}); using memory");
                    let _ = std::fs::remove_file(&copy);
                }
            }
        }
        Db::in_memory()
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
        let tx = self.inner.db.begin_write()?;
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

    fn get_bytes(&self, def: TableDefinition<&str, &[u8]>, key: &str) -> DbResult<Option<Vec<u8>>> {
        let tx = self.inner.db.begin_read()?;
        let t = tx.open_table(def)?;
        Ok(t.get(key)?.map(|g| g.value().to_vec()))
    }

    fn put_bytes(
        &self,
        def: TableDefinition<&str, &[u8]>,
        key: &str,
        value: Option<&[u8]>,
    ) -> DbResult<()> {
        let tx = self.inner.db.begin_write()?;
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

    fn scan_prefix(
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

    fn table_len(&self, def: TableDefinition<&str, &[u8]>) -> DbResult<u64> {
        let tx = self.inner.db.begin_read()?;
        let t = tx.open_table(def)?;
        Ok(t.len()?)
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
        let tx = self.inner.db.begin_write()?;
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
        let tx = self.inner.db.begin_write()?;
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
        let tx = self.inner.db.begin_write()?;
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
        let tx = self.inner.db.begin_write()?;
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
        let tx = self.inner.db.begin_write()?;
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

    // ----- stores ----------------------------------------------------------------

    /// Viewed marks backed by this database.
    pub fn review_store(&self) -> lector_config::ReviewStore {
        lector_config::ReviewStore::with_backend(Arc::new(DbReviewBackend::new(self.clone())))
    }

    /// Sessions backed by this database.
    pub fn session_store(&self) -> DbSessionStore {
        DbSessionStore::new(self.clone())
    }

    /// Recent projects backed by this database.
    pub fn recent_projects(&self) -> DbRecentProjects {
        DbRecentProjects::new(self.clone())
    }

    // ----- v0.3 import -------------------------------------------------------------

    /// Imports `review.json`, `sessions/*.json` and `recent.json` from a v0.3
    /// data directory, once (a flag in `meta` records it). Returns the number
    /// of imported records (0 when already imported).
    pub fn import_v03(&self, data_dir: &Path) -> DbResult<usize> {
        {
            let tx = self.inner.db.begin_read()?;
            let meta = tx.open_table(META)?;
            if meta.get(KEY_IMPORTED)?.is_some() {
                return Ok(0);
            }
        }
        #[derive(Deserialize)]
        struct ReviewEntry {
            repo: String,
            changeset: String,
            path: String,
            hash: String,
            #[serde(default)]
            at: u64,
        }
        #[derive(Deserialize, Default)]
        struct ReviewFile {
            #[serde(default)]
            entries: Vec<ReviewEntry>,
        }
        #[derive(Deserialize, Default)]
        struct RecentFile {
            #[serde(default)]
            projects: Vec<PathBuf>,
        }
        let review: ReviewFile = std::fs::read(data_dir.join("review.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let recent: RecentFile = std::fs::read(data_dir.join("recent.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut sessions = Vec::new();
        if let Ok(rd) = std::fs::read_dir(data_dir.join("sessions")) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                if serde_json::from_slice::<lector_config::Session>(&bytes).is_ok() {
                    sessions.push((stem.to_string(), bytes));
                }
            }
        }
        let mut n = 0;
        let tx = self.inner.db.begin_write()?;
        {
            let mut t = tx.open_table(REVIEW)?;
            for e in review.entries {
                let key = stores::review_key(&e.repo, &e.changeset, &e.path);
                if t.get(key.as_str())?.is_none() {
                    let v = serde_json::to_vec(&stores::ReviewMark {
                        hash: e.hash,
                        at: e.at,
                    })?;
                    t.insert(key.as_str(), v.as_slice())?;
                    n += 1;
                }
            }
            let mut t = tx.open_table(SESSIONS)?;
            for (key, bytes) in sessions {
                if t.get(key.as_str())?.is_none() {
                    t.insert(key.as_str(), bytes.as_slice())?;
                    n += 1;
                }
            }
            let mut t = tx.open_table(RECENT)?;
            let base = now_nanos().saturating_sub(1_000_000_000);
            for (i, p) in recent.projects.iter().enumerate() {
                let key = p.to_string_lossy();
                if t.get(key.as_ref())?.is_none() {
                    t.insert(key.as_ref(), base - i as u64)?;
                    n += 1;
                }
            }
            tx.open_table(META)?.insert(KEY_IMPORTED, 1)?;
        }
        tx.commit()?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
