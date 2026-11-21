//! Orderly stop of background work when the process quits (ADR 0003).
//!
//! Order:
//! 1. [`Scheduler::begin_quit`]: everything that is not a write is cancelled at once (reads,
//!    computations, debounces), so the writes get the workers.
//! 2. [`Scheduler::drain`]: writes ([`AtQuit::Drain`]: saves, git writes, writing commands,
//!    session saves, terminal kill escalation) finish, queued ones run, up to
//!    [`QuitBounds::drain`]; network writes ([`AtQuit::DrainBriefly`]: push and other sync
//!    commands, forge comments, agent cancel/undo) only up to [`QuitBounds::network_grace`],
//!    since they can hang. Child processes are still alive here, so a `git commit` is not
//!    killed halfway.
//! 3. Every supervised child process tree is stopped (`TERM`, `KILL` after
//!    [`QuitBounds::process_grace`]).
//! 4. [`Scheduler::shutdown`] cancels what is left and waits [`QuitBounds::scheduler`] for its
//!    threads.
//!
//! Writes given up on are logged and written to the crash log (`lector health`). Owner threads
//! made with `lector_base::thread::spawn_named` end with the resource they own; the ones still
//! alive are reported.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lector_base::thread::{threads, ThreadInfo};

use crate::scheduler::{AtQuit, JobKind, Scheduler};

/// How long each step of [`shutdown`] may take. The worst case is the sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuitBounds {
    /// Local writes still running or queued at quit (10 s).
    pub drain: Duration,
    /// Network writes (2 s, within `drain`).
    pub network_grace: Duration,
    /// Child process trees: `TERM`, then `KILL` after this (1.5 s).
    pub process_grace: Duration,
    /// The scheduler's threads after the final cancellation (0.5 s).
    pub scheduler: Duration,
}

impl Default for QuitBounds {
    fn default() -> Self {
        QuitBounds {
            drain: Duration::from_secs(10),
            network_grace: Duration::from_secs(2),
            process_grace: Duration::from_millis(1500),
            scheduler: Duration::from_millis(500),
        }
    }
}

/// What [`shutdown`] did and left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Jobs cancelled at once (not writes).
    pub cancelled: usize,
    /// Writes given up on (one entry per job): still running or queued at the drain bound, or
    /// network writes past their grace.
    pub abandoned: Vec<JobKind>,
    /// Whether every scheduler thread exited within [`QuitBounds::scheduler`].
    pub scheduler_stopped: bool,
    /// Owner threads still alive afterwards (diagnostics; the process exits anyway).
    pub owner_threads_alive: Vec<ThreadInfo>,
}

/// Quits the process-wide supervisor and scheduler (see the module docs). Blocking; call it
/// once when the app quits. Abandoned writes go to the user's crash log.
pub fn shutdown(bounds: QuitBounds) -> ShutdownReport {
    let crashes = crate::health::HealthPaths::default().crashes;
    shutdown_with(
        lector_process::Supervisor::global(),
        &Scheduler::global(),
        bounds,
        Some(&crashes),
    )
}

/// [`shutdown`] for an explicit supervisor and scheduler; abandoned writes are written to
/// `crash_dir` when given.
pub fn shutdown_with(
    supervisor: &lector_process::Supervisor,
    scheduler: &Scheduler,
    bounds: QuitBounds,
    crash_dir: Option<&Path>,
) -> ShutdownReport {
    let cancelled = scheduler.begin_quit();
    let drained = scheduler.drain(bounds.drain, bounds.network_grace);
    supervisor.shutdown(bounds.process_grace);
    let scheduler_stopped = scheduler.shutdown(bounds.scheduler);
    let owner_threads_alive: Vec<ThreadInfo> = threads().into_iter().filter(|t| t.alive).collect();
    for t in &owner_threads_alive {
        tracing::debug!(
            "at shutdown, owner thread still alive: {} {}",
            t.kind,
            t.name
        );
    }
    if !drained.abandoned.is_empty() {
        report_abandoned(&drained.abandoned, crash_dir);
    }
    ShutdownReport {
        cancelled,
        abandoned: drained.abandoned,
        scheduler_stopped,
        owner_threads_alive,
    }
}

/// Logs the writes given up on at quit and records them in the crash log.
fn report_abandoned(kinds: &[JobKind], crash_dir: Option<&Path>) {
    let list = kinds
        .iter()
        .map(|k| {
            let how = if k.at_quit() == AtQuit::DrainBriefly {
                "network grace"
            } else {
                "drain bound"
            };
            format!("{k:?} ({how})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!("quit gave up on {} write(s): {list}", kinds.len());
    tracing::error!("{message}");
    let Some(dir) = crash_dir else {
        return;
    };
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let details = crate::health::PanicDetails {
        message,
        location: None,
        thread: Some("quit".into()),
        backtrace: String::new(),
    };
    let cx = crate::health::ReportContext {
        version: env!("CARGO_PKG_VERSION").to_string(),
        time,
        session: crate::health::current_session_id(),
        home: None,
    };
    let text = crate::health::render_report(&details, &cx);
    if let Err(err) = crate::health::write_report(dir, time, &text, crate::health::MAX_REPORTS) {
        tracing::warn!("cannot record the abandoned writes in the crash log: {err}");
    }
}

#[cfg(test)]
mod tests;
