//! Local crash log and session health. Nothing here ever leaves the machine.
//!
//! Two questions the 0.7 targets depend on: "does Lector crash?" and "how often?". The answers
//! are kept in Lector's own data directory:
//!
//! * [`install_panic_hook`]: a panic hook that writes a crash report to
//!   `<data dir>/crashes/` (newest 20 kept; home directory redacted; no file contents) and then
//!   calls the previous hook. Installed by the GUI and by the headless CLI/MCP.
//! * [`SessionHealth`]: `begin()` at GUI startup, `end_clean()` on orderly shutdown. A session
//!   that never reached `end_clean` (crash, kill, power loss) is counted as abnormal by the
//!   next start. Per-process marker files make any number of windows and processes safe; see
//!   the `session` module for the rules.
//! * [`health_summary`]: totals, the crash-free rate, per-version counts and the latest crash
//!   reports; `lector health [--json]` prints it.
//!
//! These files are Lector's own state (see "Write policy" in `docs/ARCHITECTURE.md`), written
//! only while the app starts, ends or panics, never as a side effect of reading.

mod crash;
mod process;
mod session;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub use crash::{
    install_panic_hook, install_panic_hook_with, panics_in_process, prune_reports,
    read_report_header, redact, render_report, report_files, sanitize_message, write_report,
    CrashFile, HookOptions, PanicDetails, ReportContext, MAX_MESSAGE_CHARS, MAX_REPORTS,
    MAX_REPORTS_PER_PROCESS, MAX_REPORT_BYTES,
};
pub use process::{liveness, Liveness, ProcessIdentity};
pub use session::{SessionHealth, SessionRecord, SessionState, MAX_SESSIONS};

/// Where the health files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthPaths {
    /// Crash reports: `<data dir>/crashes`.
    pub crashes: PathBuf,
    /// Session records: `<data dir>/health/sessions`.
    pub sessions: PathBuf,
}

impl HealthPaths {
    /// The paths below `data_dir`.
    pub fn in_dir(data_dir: &Path) -> HealthPaths {
        HealthPaths {
            crashes: data_dir.join("crashes"),
            sessions: data_dir.join("health").join("sessions"),
        }
    }
}

impl Default for HealthPaths {
    /// The paths below `lector_config::paths::data_dir()`.
    fn default() -> Self {
        HealthPaths::in_dir(&lector_config::paths::data_dir())
    }
}

/// Counts for one Lector version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionHealth {
    /// Version string.
    pub version: String,
    /// Sessions started.
    pub sessions: u32,
    /// Sessions that ended cleanly.
    pub clean: u32,
    /// Sessions that ended abnormally.
    pub abnormal: u32,
    /// Panics seen in these sessions.
    pub panics: u32,
}

/// The state of the rolling session log.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthSummary {
    /// Sessions in the log (at most [`MAX_SESSIONS`]), including those still running.
    pub sessions: u32,
    /// Sessions whose process is still running.
    pub running: u32,
    /// Sessions that ended cleanly.
    pub clean: u32,
    /// Sessions that ended abnormally (crash, kill, power loss).
    pub abnormal: u32,
    /// Panics seen in the logged sessions (a caught panic does not end a session).
    pub panics: u32,
    /// `clean / (clean + abnormal)`; `None` until a session has finished.
    pub crash_free_rate: Option<f64>,
    /// Counts per version, newest first.
    pub by_version: Vec<VersionHealth>,
    /// The latest crash reports, newest first (up to [`MAX_REPORTS`]).
    pub last_crashes: Vec<CrashFile>,
    /// Number of crash report files kept.
    pub crash_reports: usize,
    /// Directory of the crash reports.
    pub crash_dir: PathBuf,
}

/// Summarizes the user's health log. Read-only.
pub fn health_summary() -> HealthSummary {
    health_summary_in(&HealthPaths::default())
}

/// Summarizes the health log at `paths`. Read-only.
pub fn health_summary_in(paths: &HealthPaths) -> HealthSummary {
    let mut summary = HealthSummary {
        sessions: 0,
        running: 0,
        clean: 0,
        abnormal: 0,
        panics: 0,
        crash_free_rate: None,
        by_version: Vec::new(),
        last_crashes: Vec::new(),
        crash_reports: 0,
        crash_dir: paths.crashes.clone(),
    };
    let mut versions: BTreeMap<String, (u64, VersionHealth)> = BTreeMap::new();
    for (record, marker) in session::read_records(&paths.sessions) {
        let state = record.resolved(marker);
        summary.sessions += 1;
        summary.panics += record.panics;
        let entry = versions.entry(record.version.clone()).or_insert_with(|| {
            (
                0,
                VersionHealth {
                    version: record.version.clone(),
                    sessions: 0,
                    clean: 0,
                    abnormal: 0,
                    panics: 0,
                },
            )
        });
        entry.0 = entry.0.max(record.started);
        entry.1.sessions += 1;
        entry.1.panics += record.panics;
        match state {
            SessionState::Running => summary.running += 1,
            SessionState::Clean => {
                summary.clean += 1;
                entry.1.clean += 1;
            }
            SessionState::Abnormal => {
                summary.abnormal += 1;
                entry.1.abnormal += 1;
            }
        }
    }
    let finished = summary.clean + summary.abnormal;
    if finished > 0 {
        summary.crash_free_rate = Some(f64::from(summary.clean) / f64::from(finished));
    }
    let mut by_version: Vec<(u64, VersionHealth)> = versions.into_values().collect();
    by_version.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.version.cmp(&a.1.version)));
    summary.by_version = by_version.into_iter().map(|(_, v)| v).collect();
    let files = report_files(&paths.crashes);
    summary.crash_reports = files.len();
    summary.last_crashes = files
        .iter()
        .take(MAX_REPORTS)
        .filter_map(|p| read_report_header(p))
        .collect();
    summary
}

/// The id of this process's GUI session, once [`SessionHealth::begin`] ran.
pub fn current_session_id() -> Option<String> {
    session::current_session_id()
}

/// The session that ran just before the one named `current_id`, when it ended abnormally (crash,
/// kill, power loss). `None` when it ended cleanly, is still running (another window or process),
/// or there is none. Read-only: the GUI uses it for its one-time "didn't shut down cleanly" note.
pub fn previous_session_abnormal(paths: &HealthPaths, current_id: &str) -> Option<SessionRecord> {
    let (record, marker) = session::read_records(&paths.sessions)
        .into_iter()
        .rfind(|(r, _)| r.id != current_id && r.id.as_str() < current_id)?;
    (record.resolved(marker) == SessionState::Abnormal).then_some(record)
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `2026-11-13T10:22:33Z` for `secs` since the Unix epoch (UTC).
pub fn rfc3339(secs: u64) -> String {
    let secs = i64::try_from(secs).unwrap_or(i64::MAX / 4);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Days since 1970-01-01 to a proleptic Gregorian date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests;
