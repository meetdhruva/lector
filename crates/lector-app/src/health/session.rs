//! Session markers: how "sessions without a crash" are counted.
//!
//! Every GUI process owns one record, `<data dir>/health/sessions/<started ms>-<pid>-<n>.json`,
//! written when [`SessionHealth::begin`] runs (`state: running`) and rewritten by
//! [`SessionHealth::end_clean`] (`state: clean`). A process only ever writes its own record,
//! so any number of windows and processes can run at once without touching each other's data.
//!
//! A record that still says `running` while its process is gone (see [`super::process`]) is an
//! abnormal end: a crash, a kill, a power loss. The first process that notices (at its own
//! `begin`) freezes that verdict by creating `<id>.json.abnormal` next to the record, so a pid that
//! the OS reuses later cannot bring the dead session back to life. A `clean` record always wins
//! over an `.abnormal` marker. Records beyond the newest [`MAX_SESSIONS`] are deleted, which is
//! the "rolling" part of the log.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::process::{liveness, Liveness, ProcessIdentity};
use super::{unix_secs, HealthPaths};

/// Sessions kept in the rolling log.
pub const MAX_SESSIONS: usize = 500;

const RECORD_SCHEMA: u32 = 1;
const MARKER_SUFFIX: &str = ".abnormal";

/// Where a session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// Started and not finished (as written; see [`SessionRecord::resolved`]).
    Running,
    /// Ended through [`SessionHealth::end_clean`].
    Clean,
    /// Ended without it.
    Abnormal,
}

/// One session record, as stored in the JSON file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Format version of the record.
    #[serde(default)]
    pub schema: u32,
    /// `<started ms>-<pid>-<n>`; also the file name.
    pub id: String,
    /// Process id.
    pub pid: u32,
    /// Start-time token of the process (Linux), see [`ProcessIdentity`].
    #[serde(default)]
    pub start_token: Option<String>,
    /// Executable file name.
    #[serde(default)]
    pub exe: String,
    /// Lector version.
    pub version: String,
    /// Operating system.
    #[serde(default)]
    pub os: String,
    /// CPU architecture.
    #[serde(default)]
    pub arch: String,
    /// Start time, seconds since the Unix epoch.
    pub started: u64,
    /// As written by the process itself: `running` or `clean`.
    pub state: SessionState,
    /// End time of a clean session.
    #[serde(default)]
    pub ended: Option<u64>,
    /// Panics seen by the process (a panic does not necessarily end the session).
    #[serde(default)]
    pub panics: u32,
}

impl SessionRecord {
    /// The verdict for this record: `Clean` stays clean; `Running` is `Abnormal` when the
    /// freeze marker exists or its process is gone, and stays `Running` while it lives.
    pub fn resolved(&self, marker_exists: bool) -> SessionState {
        match self.state {
            SessionState::Running => {
                if marker_exists {
                    return SessionState::Abnormal;
                }
                match liveness(self.pid, self.start_token.as_deref(), &self.exe) {
                    Liveness::Dead => SessionState::Abnormal,
                    Liveness::Alive | Liveness::Unknown => SessionState::Running,
                }
            }
            other => other,
        }
    }
}

struct Active {
    path: PathBuf,
    record: SessionRecord,
}

static SEQ: AtomicU32 = AtomicU32::new(0);

/// The session of the real GUI process, for the panic hook.
static CURRENT: Mutex<Option<Arc<Mutex<Active>>>> = Mutex::new(None);

/// The id of the process's session, if one was begun.
pub(crate) fn current_session_id() -> Option<String> {
    let current = CURRENT.try_lock()?;
    let active = current.as_ref()?.try_lock()?;
    Some(active.record.id.clone())
}

/// Records `total` panics in the process's session record. Never blocks (the panic may have
/// happened while the session lock was held) and never fails.
pub(crate) fn note_panic(total: usize) {
    let Some(current) = CURRENT.try_lock() else {
        return;
    };
    let Some(shared) = current.as_ref() else {
        return;
    };
    let Some(mut active) = shared.try_lock() else {
        return;
    };
    active.record.panics = u32::try_from(total).unwrap_or(u32::MAX);
    let Active { path, record } = &*active;
    let _ = write_record(path, record);
}

/// A running session. Cheap to clone (all clones are the same session); there is deliberately no
/// `Drop`: unwinding out of a crashing program must not look like a clean exit.
#[derive(Clone)]
pub struct SessionHealth {
    shared: Arc<Mutex<Active>>,
    paths: HealthPaths,
}

impl SessionHealth {
    /// Starts the session of this process in the user's data directory: freezes the verdict of
    /// earlier sessions that ended abnormally, trims the log and writes the marker. Never
    /// fails; when the data directory is unwritable the session is simply not recorded.
    pub fn begin() -> SessionHealth {
        let session = Self::begin_in(
            HealthPaths::default(),
            ProcessIdentity::current(),
            env!("CARGO_PKG_VERSION"),
        );
        *CURRENT.lock() = Some(session.shared.clone());
        session
    }

    /// Like [`SessionHealth::begin`] for explicit paths, identity and version (tests, tools).
    /// Does not register for the panic hook.
    pub fn begin_in(paths: HealthPaths, who: ProcessIdentity, version: &str) -> SessionHealth {
        freeze_abnormal_sessions(&paths.sessions);
        let started_secs = unix_secs();
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let record = SessionRecord {
            schema: RECORD_SCHEMA,
            id: format!(
                "{started_ms:015}-{}-{}",
                who.pid,
                SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            pid: who.pid,
            start_token: who.start_token,
            exe: who.exe,
            version: version.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            started: started_secs,
            state: SessionState::Running,
            ended: None,
            panics: u32::try_from(super::crash::panics_in_process()).unwrap_or(u32::MAX),
        };
        let path = paths.sessions.join(format!("{}.json", record.id));
        if let Err(e) =
            fs::create_dir_all(&paths.sessions).and_then(|()| write_record(&path, &record))
        {
            tracing::warn!("session health: cannot write {}: {e}", path.display());
        }
        trim_log(&paths.sessions, MAX_SESSIONS);
        SessionHealth {
            shared: Arc::new(Mutex::new(Active { path, record })),
            paths,
        }
    }

    /// The session id.
    pub fn id(&self) -> String {
        self.shared.lock().record.id.clone()
    }

    /// The paths this session writes to.
    pub fn paths(&self) -> &HealthPaths {
        &self.paths
    }

    /// Marks the session as ended cleanly. Call it on every orderly shutdown; calling it again
    /// does nothing.
    pub fn end_clean(&self) {
        let mut active = self.shared.lock();
        if active.record.state == SessionState::Clean {
            return;
        }
        active.record.state = SessionState::Clean;
        active.record.ended = Some(unix_secs());
        let panics = super::crash::panics_in_process();
        if panics > 0 {
            active.record.panics = u32::try_from(panics).unwrap_or(u32::MAX);
        }
        let Active { path, record } = &*active;
        if let Err(e) = write_record(path, record) {
            tracing::warn!("session health: cannot update {}: {e}", path.display());
        }
    }
}

/// Writes `record` to `path` through a temporary file in the same directory, so readers never
/// see a half-written record.
pub(crate) fn write_record(path: &Path, record: &SessionRecord) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, json)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The record files of `dir`, oldest first.
fn record_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    files.sort();
    files
}

fn marker_path(record_path: &Path) -> PathBuf {
    let mut name = record_path.as_os_str().to_owned();
    name.push(MARKER_SUFFIX);
    PathBuf::from(name)
}

/// Reads every readable record of `dir` with whether its freeze marker exists, oldest first.
pub(crate) fn read_records(dir: &Path) -> Vec<(SessionRecord, bool)> {
    record_files(dir)
        .into_iter()
        .filter_map(|path| {
            let record: SessionRecord = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            Some((record, marker_path(&path).exists()))
        })
        .collect()
}

/// Creates the `.abnormal` marker next to every `running` record whose process is gone.
fn freeze_abnormal_sessions(dir: &Path) {
    for path in record_files(dir) {
        let Some(record) = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionRecord>(&b).ok())
        else {
            continue;
        };
        if record.state != SessionState::Running {
            continue;
        }
        let marker = marker_path(&path);
        if marker.exists() {
            continue;
        }
        if record.resolved(false) == SessionState::Abnormal {
            // `create_new`: several processes starting at once agree, nobody overwrites.
            let _ = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker);
        }
    }
}

/// Deletes the oldest finished records (and their markers) beyond `keep`. A record whose
/// process is still running is never deleted.
fn trim_log(dir: &Path, keep: usize) {
    let files = record_files(dir);
    let excess = files.len().saturating_sub(keep);
    for path in files.into_iter().take(excess) {
        let marker = marker_path(&path);
        let running = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionRecord>(&b).ok())
            .is_some_and(|r| r.resolved(marker.exists()) == SessionState::Running);
        if !running {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(&marker);
        }
    }
}

#[cfg(test)]
pub(crate) fn trim_log_for_tests(dir: &Path, keep: usize) {
    trim_log(dir, keep);
}
