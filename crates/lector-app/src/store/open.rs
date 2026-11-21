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
//!   pages, truncated) is believed only when it stays so for about half a second (a file
//!   another process is still initialising looks the same for a moment). Recovery then runs
//!   under a cross-process lock (`<name>.recovery-lock`, held with an OS file lock through redb's
//!   exclusive mode, so a process that dies releases it): the first thing done under the lock
//!   is to open the file again; if it opens now (another process recovered it), it is used and
//!   nothing is recovered. Otherwise the file is moved aside with a rename in the same
//!   directory onto a name reserved with `create_new` (`<name>.corrupt-<unix seconds>[-n]`,
//!   created 0600; the damaged file is made 0600 before the rename, so it is never readable by
//!   others), falling back to copying into the reserved file and removing the original. A
//!   fresh database is created before the lock is released, so the other processes find it.
//!   [`super::Store::recovered_from`] names the copy for the UI. If anything fails, the file is
//!   left alone and the process runs with read-only state.
//! * **New files** are created owner-only (0600 on Unix) whatever the umask; an existing file
//!   keeps its mode.

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

/// Delays between the re-opens that confirm a file is damaged (about half a second in all).
const CORRUPT_CONFIRM: &[u64] = &[100, 150, 250];

/// [`open_with_retry`], believing "corrupt" only when it stays so for [`CORRUPT_CONFIRM`].
fn open_confirmed(path: &Path) -> Result<Database, OpenFailure> {
    let mut attempt = open_with_retry(path);
    for delay in CORRUPT_CONFIRM {
        if !matches!(attempt, Err(OpenFailure::Corrupt(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(*delay));
        attempt = open_with_retry(path);
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

/// What [`super::Store::probe`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// No database yet (the next start creates one).
    Missing,
    /// It opens: the next start shares it read-write.
    Healthy,
    /// The next start would run with read-only state, for this reason.
    ReadOnly(String),
    /// It is damaged: the next start sets it aside and starts a fresh one.
    Damaged(String),
}

impl Probe {
    /// The line `lector health` prints (`None` when healthy or missing).
    pub fn line(&self) -> Option<String> {
        match self {
            Probe::Missing | Probe::Healthy => None,
            Probe::ReadOnly(reason) => super::StoreMode::ReadOnly {
                reason: reason.clone(),
            }
            .indicator()
            .map(|i| i.detail),
            Probe::Damaged(reason) => Some(format!(
                "Lector's state database is damaged ({reason}); the next start keeps it aside and starts a new one"
            )),
        }
    }
}

/// Looks at the file through a read-only handle (opened in the writers' mode, so it coexists
/// with running Lector processes) and answers what [`open`] would do, without creating,
/// writing or recovering anything:
///
/// * missing: [`Probe::Missing`] when the file can be created (its nearest existing folder is
///   writable), else read-only (`open` cannot create it);
/// * newer schema, locked by an excluding process: read-only;
/// * opens read-only, or fails only with `DatabaseError::RepairAborted` (a database a
///   writable open repairs: unclean shutdown, unflushed primary): healthy when the file can
///   be opened for writing (checked by opening it for writing and writing nothing, which
///   changes neither contents nor modification time), else read-only (read-only file,
///   permissions, read-only file system);
/// * damaged: [`Probe::Damaged`] when its folder is writable (`open` moves it aside), else
///   read-only;
/// * any other error (permission denied, other I/O, a file format that needs an upgrade...):
///   read-only, with the reason.
pub(crate) fn probe(path: &Path) -> Probe {
    if std::fs::symlink_metadata(path).is_err() {
        return match creatable(path) {
            Ok(()) => Probe::Missing,
            Err(e) => Probe::ReadOnly(format!("cannot create the database: {e}")),
        };
    }
    if let Some(found) = peek_schema(path).filter(|v| *v > crate::db::SCHEMA_VERSION) {
        return Probe::ReadOnly(format!(
            "the database was written by a newer Lector (schema {found}, this one knows {})",
            crate::db::SCHEMA_VERSION
        ));
    }
    let opened = builder(ConcurrencyMode::MultiWriter).open_read_only(path);
    match opened {
        Ok(_) | Err(redb::DatabaseError::RepairAborted) => match writable_file(path) {
            Ok(()) => Probe::Healthy,
            Err(e) => Probe::ReadOnly(format!("the database cannot be written: {e}")),
        },
        Err(e) => match classify(&e) {
            OpenFailure::Locked(r) => Probe::ReadOnly(format!(
                "the database is in use by another process that excludes this one ({r})"
            )),
            OpenFailure::Corrupt(r) => match writable_dir(path.parent()) {
                Ok(()) => Probe::Damaged(r),
                Err(e) => Probe::ReadOnly(format!(
                    "the database is damaged ({r}) and could not be moved aside ({e})"
                )),
            },
            OpenFailure::Unavailable(r) => Probe::ReadOnly(r),
        },
    }
}

/// Whether `path` (an existing file) can be opened for writing. Writes nothing.
fn writable_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new().write(true).open(path).map(drop)
}

/// Whether a file can be created in `dir` (`None`: the current folder). Creates nothing.
fn writable_dir(dir: Option<&Path>) -> std::io::Result<()> {
    let dir = dir
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    {
        rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).map_err(std::io::Error::from)
    }
    #[cfg(not(unix))]
    {
        // Windows: a folder's read-only attribute does not stop creating files in it; ACLs
        // are only known by trying. Report what the metadata says.
        std::fs::metadata(dir).map(drop)
    }
}

/// Whether `path` (missing) could be created: its nearest existing ancestor is writable.
fn creatable(path: &Path) -> std::io::Result<()> {
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        if std::fs::symlink_metadata(d).is_ok() {
            return writable_dir(Some(d));
        }
        dir = d.parent();
    }
    writable_dir(None)
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
    match open_confirmed(path) {
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

/// The cross-process lock under which a damaged database is recovered: a small redb file
/// next to it, opened in `ExclusiveWriter` mode (an OS file lock that the OS releases when the
/// process ends, however it ends). Released on drop.
struct RecoveryLock {
    _db: Database,
}

impl RecoveryLock {
    /// Waits (up to about ten seconds) for the lock of `path`.
    fn acquire(path: &Path) -> Result<RecoveryLock, String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".recovery-lock");
        let lock = PathBuf::from(name);
        let mut last = String::new();
        for _ in 0..500 {
            crate::db::create_private(&lock).map_err(|e| e.to_string())?;
            match builder(ConcurrencyMode::ExclusiveWriter).create(&lock) {
                Ok(db) => return Ok(RecoveryLock { _db: db }),
                Err(e) => match classify(&e) {
                    OpenFailure::Locked(r) => last = r,
                    // A lock file left half-written by a process that died while creating it
                    // (it held no lock then, or this would say "locked"): start it again.
                    OpenFailure::Corrupt(r) => {
                        last = r;
                        let _ = std::fs::remove_file(&lock);
                    }
                    OpenFailure::Unavailable(r) => return Err(r),
                },
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!("timed out waiting for the recovery lock ({last})"))
    }
}

/// Recovers a damaged database (see the module doc).
fn recover(path: &Path, reason: &str) -> Store {
    let _lock = match RecoveryLock::acquire(path) {
        Ok(lock) => lock,
        Err(e) => {
            return read_only(
                path,
                format!(
                    "the database is damaged ({reason}) and the recovery lock is unavailable ({e})"
                ),
            )
        }
    };
    // Another process may have recovered it while this one waited.
    let reason = match open_confirmed(path) {
        Ok(database) => return writable(database, path, None),
        Err(OpenFailure::Corrupt(reason)) => reason,
        Err(OpenFailure::Locked(r)) => {
            return read_only(
                path,
                format!("the database is in use by another process that excludes this one ({r})"),
            )
        }
        Err(OpenFailure::Unavailable(r)) => return read_only(path, r),
    };
    let aside = match move_aside(path) {
        Ok(aside) => aside,
        Err(e) => {
            return read_only(
                path,
                format!("the database is damaged ({reason}) and could not be moved aside ({e})"),
            )
        }
    };
    tracing::warn!(
        "the database {} was damaged ({reason}); it was moved to {} and a new database was started",
        path.display(),
        aside.display()
    );
    // Created while the lock is held: the processes waiting for it find this database.
    match open_file(path) {
        Ok(database) => writable(database, path, Some(aside)),
        Err(e) => read_only(path, format!("cannot create a new database: {e}")),
    }
}

/// Reserves `<path>.corrupt-<unix seconds>[-n]` with `create_new` (owner-only from the start).
fn reserve_aside(path: &Path) -> std::io::Result<PathBuf> {
    let secs = crate::db::now_nanos() / 1_000_000_000;
    let mut base = path.as_os_str().to_os_string();
    base.push(format!(".corrupt-{secs}"));
    let mut last = None;
    for n in 1..1000 {
        let mut name = base.clone();
        if n > 1 {
            name.push(format!("-{n}"));
        }
        let candidate = PathBuf::from(name);
        match lector_base::private::create_new(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free name")))
}

/// Moves the damaged file onto a reserved owner-only name: a rename (atomic, same directory)
/// after making the file owner-only, else a copy into the reserved file and a removal.
fn move_aside(path: &Path) -> std::io::Result<PathBuf> {
    let target = reserve_aside(path)?;
    if restrict_to_owner(path).is_ok() && std::fs::rename(path, &target).is_ok() {
        return Ok(target);
    }
    let copied = std::fs::File::open(path).and_then(|mut source| {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&target)?;
        std::io::copy(&mut source, &mut out)?;
        out.sync_all()
    });
    match copied.and_then(|_| std::fs::remove_file(path)) {
        Ok(()) => Ok(target),
        Err(e) => {
            let _ = std::fs::remove_file(&target);
            Err(e)
        }
    }
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
