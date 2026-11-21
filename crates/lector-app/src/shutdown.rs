//! Orderly stop of background work when the process quits (ADR 0003).
//!
//! Order: every supervised child process tree is stopped first (`TERM`, `KILL` after the
//! grace period), which also ends the jobs and owner threads that wait on a child (git, agents,
//! language servers, their pipe readers); then the [`Scheduler`] cancels what is queued and
//! running and waits a bounded time for its threads. Owner threads made with
//! `lector_base::thread::spawn_named` end with the resource they own (a dropped watcher, a
//! killed process, a closed terminal); the ones still alive are reported.

use std::time::Duration;

use lector_base::thread::{threads, ThreadInfo};

use crate::scheduler::Scheduler;

/// What [`shutdown`] left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Whether every scheduler thread exited within the timeout.
    pub scheduler_stopped: bool,
    /// Owner threads still alive afterwards (diagnostics; the process exits anyway).
    pub owner_threads_alive: Vec<ThreadInfo>,
}

/// Stops child processes (waiting up to `process_grace` before killing them), then the
/// process-wide scheduler (waiting up to `scheduler_timeout` for its threads). Blocking; call
/// it once when the app quits. Later jobs are cancelled at once.
pub fn shutdown(process_grace: Duration, scheduler_timeout: Duration) -> ShutdownReport {
    shutdown_with(
        lector_process::Supervisor::global(),
        &Scheduler::global(),
        process_grace,
        scheduler_timeout,
    )
}

/// [`shutdown`] for an explicit supervisor and scheduler (tests).
pub fn shutdown_with(
    supervisor: &lector_process::Supervisor,
    scheduler: &Scheduler,
    process_grace: Duration,
    scheduler_timeout: Duration,
) -> ShutdownReport {
    supervisor.shutdown(process_grace);
    let scheduler_stopped = scheduler.shutdown(scheduler_timeout);
    let owner_threads_alive: Vec<ThreadInfo> = threads().into_iter().filter(|t| t.alive).collect();
    for t in &owner_threads_alive {
        tracing::debug!("at shutdown, owner thread still alive: {} {}", t.kind, t.name);
    }
    ShutdownReport {
        scheduler_stopped,
        owner_threads_alive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{JobKey, JobKind, Priority, Subject};
    use std::time::Instant;

    #[test]
    fn quits_promptly_with_jobs_queued_and_running() {
        let supervisor = lector_process::Supervisor::new();
        let scheduler = Scheduler::with_limits(1, 2);
        let (tx, rx) = std::sync::mpsc::channel();
        let running = scheduler.spawn(
            JobKey::new(JobKind::GitLog, Subject::App),
            Priority::Background,
            move |cancel| {
                tx.send(()).ok();
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            },
        );
        rx.recv_timeout(Duration::from_secs(5)).ok();
        let queued: Vec<_> = (0..50)
            .map(|i| {
                scheduler.spawn(
                    JobKey::new(JobKind::DocumentHighlight, Subject::Document(i)),
                    Priority::Idle,
                    |_| (),
                )
            })
            .collect();
        let started = Instant::now();
        let report = shutdown_with(
            &supervisor,
            &scheduler,
            Duration::from_millis(100),
            Duration::from_secs(5),
        );
        assert!(report.scheduler_stopped);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(running.wait().is_none());
        assert!(queued.into_iter().all(|h| h.wait().is_none()));
        assert!(supervisor.is_shutting_down());
    }
}
