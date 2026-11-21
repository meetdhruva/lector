//! Opening the user's database: sharing, locks, corruption.
//!
//! * **Shared.** Processes open the file in redb's `MultiWriter` mode (byte-range locks:
//!   Linux, macOS, Windows), so two Lector processes (0.7.1 included, which uses the same
//!   redb and mode) share one database read-write. Write transactions are serialized by
//!   redb's writer lock across processes; a [`super::Keyspace::update`] is atomic across
//!   them.
//! * **Locked.** When the file cannot be opened because another process holds it in a way
//!   that excludes this one (redb `DatabaseAlreadyOpen`: a writer in `ExclusiveWriter` mode,
//!   which is what redb uses on platforms without byte-range locks and what redb before 4.3
//!   did; `LockProtocolMismatch`: a redb with another locking protocol; on Windows also a
//!   sharing or lock violation from the OS), the open is retried for about a second, then
//!   the process runs with **read-only state**: an in-memory database, seeded from the file
//!   when the file can be opened read-only (it cannot while an exclusive writer holds it),
//!   that is never written back anywhere. [`super::Store::mode`] says so and the log says
//!   why. No private copy of the file is made (0.7.1 did, and its changes diverged).
//! * **Not writable for other reasons** (written by a newer Lector, read-only file system,
//!   permissions, a redb file format that needs an upgrade): read-only state too, seeded
//!   from the file when it can be read. The file is never modified.
//! * **Corrupt.** A file that redb reports as corrupt (bad magic number, damaged header or
//!   pages, truncated) is copied aside to `<name>.corrupt-<unix seconds>` (owner-only, 0600
//!   on Unix), removed, and a fresh database is created; [`super::Store::recovered_from`]
//!   names the copy for the UI. If the copy fails, the file is left alone and the process
//!   runs with read-only state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use redb::{ConcurrencyMode, Database, ReadableDatabase, ReadableTable, TableDefinition};

use super::keyspace::STORE_META;
use super::{concerns, Store, StoreInfo};
use crate::db::{
    builder, open_file, Db, DbError, DbMode, COMMENTS, JOURNAL, KV, META, RECENT, REVIEW, SESSIONS,
};

/// Why the file could not be opened for writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenFailure {
    /// Another live process holds it and excludes this one.
    Locked(String),
    /// redb reports the file as damaged.
    Corrupt(String),
    /// Anything else (permissions, a newer schema, an old file format...): left untouched.
    Unavailable(String),
}

/// Sorts a redb open error. Locks are taken before redb reads anything, so a corrupt file
/// held by another process is reported as locked, never as corrupt.
pub(crate) fn classify(err: &redb::DatabaseError) -> OpenFailure {
    use redb::{DatabaseError as D, StorageError as S};
    let text = err.to_string();
    match err {
        D::DatabaseAlreadyOpen | D::LockProtocolMismatch => OpenFailure::Locked(text),
        D::Storage(S::Corrupted(_)) => OpenFailure::Corrupt(text),
        D::Storage(S::Io(io)) if is_lock_violation(io) => OpenFailure::Locked(text),
        D::Storage(S::Io(io))
            if matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
            ) =>
        {
            OpenFailure::Corrupt(text)
        }
        _ => OpenFailure::Unavailable(text),
    }
}

/// Windows reports a file another process locked as ERROR_SHARING_VIOLATION (32) or
/// ERROR_LOCK_VIOLATION (33).
fn is_lock_violation(err: &std::io::Error) -> bool {
    cfg!(windows) && matches!(err.raw_os_error(), Some(32) | Some(33))
}

/// Delays between attempts while the file is locked (about one second in all).
const LOCK_RETRY: &[u64] = &[50, 100, 150, 200, 250, 250];

/// Opens `path` for writing, retrying while it is locked.
fn open_with_retry(path: &Path) -> Result<Database, OpenFailure> {
    let mut attempt = open_file(path).map_err(|e| classify(&e));
    for delay in LOCK_RETRY {
        if !matches!(attempt, Err(OpenFailure::Locked(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(*delay));
        attempt = open_file(path).map_err(|e| classify(&e));
    }
    attempt
}

/// The schema version of an existing file, read through a read-only handle (opened in the
/// writers' mode, so it coexists with them). `None` when it cannot be read that way.
fn peek_schema(path: &Path) -> Option<u64> {
    if !path.is_file() {
        return None;
    }
    let db = builder(ConcurrencyMode::MultiWriter)
        .open_read_only(path)
        .ok()?;
    let tx = db.begin_read().ok()?;
    let meta = tx.open_table(META).ok()?;
    let version = meta.get("schema_version").ok()?.map(|g| g.value());
    version
}

/// The store over the file at `path`, by the policy in the module doc. Never fails.
pub(crate) fn open(path: &Path) -> Store {
    // A file of a newer Lector is never opened for writing at all (opening for writing lets
    // redb update its header even when no transaction commits).
    if let Some(found) = peek_schema(path).filter(|v| *v > crate::db::SCHEMA_VERSION) {
        return read_only(
            path,
            format!(
                "the database was written by a newer Lector (schema {found}, this one knows {})",
                crate::db::SCHEMA_VERSION
            ),
        );
    }
    match open_with_retry(path) {
        Ok(database) => writable(database, path, None),
        Err(OpenFailure::Corrupt(reason)) => recover(path, &reason),
        Err(OpenFailure::Locked(reason)) => read_only(
            path,
            format!("the database is in use by another process that excludes this one ({reason})"),
        ),
        Err(OpenFailure::Unavailable(reason)) => read_only(path, reason),
    }
}

fn writable(database: Database, path: &Path, recovered: Option<PathBuf>) -> Store {
    match Db::from_database(database, DbMode::File(path.to_path_buf())) {
        Ok(db) => {
            let store = Store::with_info(db, StoreInfo { recovered_from: recovered });
            if let Err(e) = store.create_tables() {
                tracing::warn!("cannot create the state tables: {e}");
            }
            store
        }
        // The database handle is dropped here; nothing was written (the migration that
        // found the newer schema aborted its transaction).
        Err(DbError::TooNew { found, supported }) => read_only(
            path,
            format!("the database was written by a newer Lector (schema {found}, this one knows {supported})"),
        ),
        Err(e) => read_only(path, format!("cannot prepare the database: {e}")),
    }
}

/// Copies a corrupt file aside and starts a fresh database.
fn recover(path: &Path, reason: &str) -> Store {
    let aside = aside_name(path);
    let copied = std::fs::copy(path, &aside).and_then(|_| restrict_to_owner(&aside));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&aside);
        return read_only(
            path,
            format!("the database is damaged ({reason}) and could not be copied aside ({e})"),
        );
    }
    if let Err(e) = std::fs::remove_file(path) {
        return read_only(
            path,
            format!(
                "the database is damaged ({reason}); kept a copy at {} but could not replace it ({e})",
                aside.display()
            ),
        );
    }
    tracing::warn!(
        "the database {} was damaged ({reason}); a copy is kept at {} and a new database was started",
        path.display(),
        aside.display()
    );
    match open_file(path) {
        Ok(database) => writable(database, path, Some(aside)),
        Err(e) => read_only(path, format!("cannot create a new database: {e}")),
    }
}

/// `<path>.corrupt-<unix seconds>` (with a counter if that exists).
fn aside_name(path: &Path) -> PathBuf {
    let secs = crate::db::now_nanos() / 1_000_000_000;
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".corrupt-{secs}"));
    let first = PathBuf::from(&name);
    if !first.exists() {
        return first;
    }
    (2..1000)
        .map(|n| {
            let mut n2 = name.clone();
            n2.push(format!("-{n}"));
            PathBuf::from(n2)
        })
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Read-only state: an in-memory database seeded from the file when it can be opened
/// read-only, never written back.
fn read_only(path: &Path, reason: String) -> Store {
    tracing::warn!(
        "state is read-only in this process: {reason}. Changes made here are kept in memory and lost on exit ({})",
        path.display()
    );
    let mode = DbMode::ReadOnly {
        path: path.to_path_buf(),
        reason,
    };
    let memory =
        match redb::Builder::new().create_with_backend(redb::backends::InMemoryBackend::new()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("cannot create an in-memory database: {e}");
                return Store::from_db(Db::in_memory());
            }
        };
    if path.is_file() {
        if let Err(e) = seed(path, &memory) {
            tracing::info!("read-only state starts empty: the file cannot be read ({e})");
        }
    }
    let store = Store::from_db(Db::from_snapshot(memory, mode));
    if let Err(e) = store.create_tables() {
        tracing::warn!("cannot create the state tables: {e}");
    }
    store
}

/// Copies every table Lector knows (but the cache) from a read-only handle on `path`.
fn seed(path: &Path, into: &Database) -> Result<(), String> {
    let mut last = String::new();
    for mode in [
        ConcurrencyMode::MultiWriter,
        ConcurrencyMode::ExclusiveWriter,
    ] {
        match builder(mode).open_read_only(path) {
            Ok(source) => return copy_tables(&source, into).map_err(|e| e.to_string()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

fn copy_tables(source: &redb::ReadOnlyDatabase, into: &Database) -> Result<(), DbError> {
    let rtx = source.begin_read()?;
    let wtx = into.begin_write()?;
    {
        let str_u64: [TableDefinition<&str, u64>; 3] = [META, RECENT, STORE_META];
        for def in str_u64 {
            let Ok(src) = rtx.open_table(def) else {
                continue;
            };
            let mut dst = wtx.open_table(def)?;
            for item in src.iter()? {
                let (k, v) = item?;
                dst.insert(k.value(), v.value())?;
            }
        }
        let mut bytes: Vec<TableDefinition<&str, &[u8]>> = vec![REVIEW, COMMENTS, SESSIONS, KV];
        bytes.extend(concerns::BYTE_CONCERNS.iter().map(|c| c.table()));
        for def in bytes {
            let Ok(src) = rtx.open_table(def) else {
                continue;
            };
            let mut dst = wtx.open_table(def)?;
            for item in src.iter()? {
                let (k, v) = item?;
                dst.insert(k.value(), v.value())?;
            }
        }
        if let Ok(src) = rtx.open_table(JOURNAL) {
            let mut dst = wtx.open_table(JOURNAL)?;
            for item in src.iter()? {
                let (k, v) = item?;
                dst.insert(k.value(), v.value())?;
            }
        }
    }
    wtx.commit()?;
    Ok(())
}

/// Makes `path` readable and writable by its owner only (no-op where not applicable).
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
