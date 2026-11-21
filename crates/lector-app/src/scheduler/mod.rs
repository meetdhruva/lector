//! One process-wide job scheduler for short background work (ADR 0003).
//!
//! * Every job has a [`JobKey`] (a [`JobKind`] from the closed list in [`key`] plus a
//!   [`Subject`]) and a [`Priority`]. A newer job with the same key cancels older ones for kinds
//!   that [replace](JobKind::replaces); [`Scheduler::cancel_subject`] cancels everything of a
//!   document, repository, view... that closes. A cancelled job that has not started never runs;
//!   a running one sees its [`CancelToken`]; either way its result is never delivered.
//! * **Compute lane**: `default_workers()` general workers serve the highest non-empty
//!   priority first (FIFO within a priority), plus **one worker reserved for `Interactive`
//!   jobs**, so a keystroke-driven job runs at once even when every general worker is busy
//!   with long background work. Only kinds marked [`JobKind::may_be_interactive`] get
//!   `Interactive`; others asking for it run at `Visible`.
//! * **Blocking lane**: kinds that wait on somebody else (network, language servers, agents,
//!   the user, external programs) run on their own threads, started on demand up to
//!   [`BLOCKING_MAX`] and stopped after 30 s idle, so they never occupy the compute workers.
//! * A job that panics is caught: the worker survives, the panic is logged (and written to the
//!   crash log by the process's panic hook, see `health`) and the waiter gets
//!   [`JobError::Panicked`].
//! * A job that waits for another job ([`JobHandle::wait`]) on a scheduler thread runs that job
//!   inline if it has not started, so waiting on the same pool cannot deadlock it.
//! * [`Scheduler::shutdown`] cancels everything and waits a bounded time for the threads.
//! * [`Scheduler::debounce`] coalesces calls with the same key: only the last closure runs,
//!   `delay` after the last call.
//! * [`Scheduler::with_runner`] hands every job to a runner instead of threads (the GPUI
//!   bridge uses GPUI's test executor under its test dispatcher, so tests stay deterministic);
//!   keys, subjects and cancellation work the same.
//!
//! [`JobHandle`] exposes the result as a runtime-agnostic future (a `futures` oneshot), so GPUI,
//! tests and the CLI can all await it.

pub mod key;
mod pool;

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use futures::channel::oneshot;
use lector_base::CancelToken;

pub use key::{JobKey, JobKind, Lane, Subject};
pub use pool::{Body, Runner};

use pool::{Inner, Job};

/// Most threads the blocking lane runs at once; further blocking jobs queue by priority.
pub const BLOCKING_MAX: usize = 16;

/// Scheduling class of a job; earlier variants run first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Directly answers a keystroke and is short (reparse, picker ranking, find as you type).
    /// Only for kinds that [may be interactive](JobKind::may_be_interactive).
    Interactive = 0,
    /// Work for something currently on screen (highlights, the visible diff, an opened file).
    Visible = 1,
    /// Work the user will likely need soon (status refresh, gutter, blame).
    Background = 2,
    /// Anything that can wait (indexing, prefetch).
    Idle = 3,
}

impl Priority {
    /// All priorities, highest first.
    pub const ALL: [Priority; 4] = [
        Priority::Interactive,
        Priority::Visible,
        Priority::Background,
        Priority::Idle,
    ];

    /// The priority a job of `kind` actually runs at.
    pub fn for_kind(self, kind: JobKind) -> Priority {
        if self == Priority::Interactive && !kind.may_be_interactive() {
            tracing::debug!("{kind:?} asked for Interactive; runs at Visible");
            Priority::Visible
        } else {
            self
        }
    }
}

/// Why a job produced no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    /// Replaced by a newer job, cancelled (by its handle, its subject or shutdown).
    Cancelled,
    /// The job panicked; the message of the panic.
    Panicked(String),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Cancelled => f.write_str("the background job was cancelled"),
            JobError::Panicked(msg) => write!(f, "the background job panicked: {msg}"),
        }
    }
}

impl std::error::Error for JobError {}

/// Shuts the pool down (without waiting) when the last [`Scheduler`] clone is dropped.
struct Owner(Arc<Inner>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.shutdown(Duration::ZERO);
    }
}

/// Handle to the scheduler. Cheap to clone.
#[derive(Clone)]
pub struct Scheduler {
    owner: Arc<Owner>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("workers", &self.inner().general)
            .field("runner", &self.inner().has_runner())
            .finish()
    }
}

/// Default number of general compute workers: `num_cpus - 1`, at least 2 (the reserved
/// interactive worker comes on top).
pub fn default_workers() -> usize {
    num_cpus::get().saturating_sub(1).max(2)
}

impl Scheduler {
    /// The process-wide scheduler (created on first use with [`default_workers`]); the GUI,
    /// the CLI and the MCP server all use it.
    pub fn global() -> Scheduler {
        static GLOBAL: OnceLock<Scheduler> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Scheduler::new(default_workers()))
            .clone()
    }

    /// A new pool with `workers` general compute workers (at least 1), one reserved
    /// interactive worker and a blocking lane of up to [`BLOCKING_MAX`] threads. The threads
    /// stop when the last clone is dropped or at [`Scheduler::shutdown`].
    pub fn new(workers: usize) -> Scheduler {
        Scheduler::with_limits(workers, BLOCKING_MAX)
    }

    /// Like [`Scheduler::new`] with an explicit bound for the blocking lane.
    pub fn with_limits(workers: usize, blocking_max: usize) -> Scheduler {
        Scheduler {
            owner: Arc::new(Owner(Inner::start(workers, blocking_max))),
        }
    }

    /// A scheduler that starts no thread and hands every job to `runner` (with the delay of a
    /// debounce, zero otherwise). Keys, subjects, cancellation and results work as usual;
    /// priorities and lanes are the runner's business.
    pub fn with_runner(runner: Runner) -> Scheduler {
        Scheduler {
            owner: Arc::new(Owner(Inner::with_runner(runner))),
        }
    }

    fn inner(&self) -> &Arc<Inner> {
        &self.owner.0
    }

    /// Number of general compute workers (the reserved interactive worker not counted).
    pub fn workers(&self) -> usize {
        self.inner().general
    }

    /// The bound of the blocking lane.
    pub fn blocking_max(&self) -> usize {
        self.inner().blocking_max
    }

    /// Runs `f` on the lane of `key.kind` at `prio` (see [`Priority::for_kind`]). Older jobs
    /// with the same key are cancelled when the kind [replaces](JobKind::replaces).
    pub fn spawn<T: Send + 'static>(
        &self,
        key: JobKey,
        prio: Priority,
        f: impl FnOnce(&CancelToken) -> T + Send + 'static,
    ) -> JobHandle<T> {
        let prio = prio.for_kind(key.kind);
        let token = CancelToken::new();
        let (tx, rx) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let body = guarded(key.kind, token.clone(), finished.clone(), f, tx);
        let id = self.inner().next_id();
        self.inner()
            .submit(prio, token.clone(), Job { id, key, body });
        JobHandle {
            id,
            token,
            finished,
            rx: Some(rx),
            inner: Arc::downgrade(self.inner()),
        }
    }

    /// Runs `f` once, `delay` after the *last* call with the same `key` (earlier closures for
    /// that key are dropped without running). When it runs it is a keyed job, so it also
    /// cancels a still-running older run.
    pub fn debounce(
        &self,
        key: JobKey,
        delay: Duration,
        prio: Priority,
        f: impl FnOnce(&CancelToken) + Send + 'static,
    ) {
        let prio = prio.for_kind(key.kind);
        let token = CancelToken::new();
        let (tx, _rx) = oneshot::channel();
        let body = guarded(
            key.kind,
            token.clone(),
            Arc::new(AtomicBool::new(false)),
            f,
            tx,
        );
        self.inner().debounce(key, delay, prio, token, body);
    }

    /// A [`lector_base::thread::TaskRunner`] that runs each task as a job with `key` at `prio`:
    /// how crates below `lector-app` (which do not schedule, ADR 0003) get their short tasks
    /// run here. A task that has not started when its job is cancelled (shutdown, its subject)
    /// is dropped without running.
    pub fn task_runner(&self, key: JobKey, prio: Priority) -> lector_base::thread::TaskRunner {
        let scheduler = self.clone();
        Arc::new(move |task| {
            let _ = scheduler.spawn(key.clone(), prio, move |_| task());
        })
    }

    /// Cancels every job with `key` and any pending debounce for it.
    pub fn cancel_key(&self, key: &JobKey) {
        self.inner().cancel_key(key);
    }

    /// Cancels every queued, running or debounced job of `subject` (except kinds that are not
    /// [cancelled with their subject](JobKind::cancelled_with_subject)). Call it when the
    /// subject closes. Returns how many jobs were cancelled.
    pub fn cancel_subject(&self, subject: &Subject) -> usize {
        self.inner().cancel_subject(subject)
    }

    /// Jobs queued but not started (cancelled ones included until a worker drops them), plus
    /// pending debounces.
    pub fn pending(&self) -> usize {
        self.inner().pending()
    }

    /// Jobs queued or running that are still registered under their key.
    pub fn live_jobs(&self) -> usize {
        self.inner().live_jobs()
    }

    /// Blocks until no job is queued, debounced or running, or `timeout` passes. Returns true
    /// when idle. Meant for tests and orderly shutdown.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        self.inner().wait_idle(timeout)
    }

    /// Stops the scheduler for good: queued jobs are dropped (their waiters get
    /// [`JobError::Cancelled`]), running jobs are cancelled, later jobs are cancelled at once,
    /// and the call waits up to `timeout` for the scheduler's threads to exit. Returns whether
    /// they all did (a job that ignores its token can keep one busy; the process may exit
    /// anyway).
    pub fn shutdown(&self, timeout: Duration) -> bool {
        self.inner().shutdown(timeout)
    }

    /// Whether [`Scheduler::shutdown`] was called.
    pub fn is_shut_down(&self) -> bool {
        self.inner().is_shut_down()
    }
}

/// Wraps `f`: skipped when cancelled before it starts, panics caught and reported, the result
/// sent only if the token is still not cancelled once `f` returns.
fn guarded<T: Send + 'static>(
    kind: JobKind,
    token: CancelToken,
    finished: Arc<AtomicBool>,
    f: impl FnOnce(&CancelToken) -> T + Send + 'static,
    tx: oneshot::Sender<Result<T, JobError>>,
) -> Body {
    Box::new(move || {
        let outcome = if token.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            match catch_unwind(AssertUnwindSafe(|| f(&token))) {
                Ok(_) if token.is_cancelled() => Err(JobError::Cancelled),
                Ok(value) => Ok(value),
                Err(payload) => {
                    let msg = panic_message(payload.as_ref());
                    tracing::error!(kind = ?kind, "background job panicked: {msg}");
                    Err(JobError::Panicked(msg))
                }
            }
        };
        let _ = tx.send(outcome);
        finished.store(true, Ordering::Release);
    })
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a non-string panic payload".to_string()
    }
}

/// Result handle of a spawned job. Dropping it does **not** cancel the job (the GPUI bridge's
/// task does; see `lector_ui::background`).
pub struct JobHandle<T> {
    id: u64,
    token: CancelToken,
    finished: Arc<AtomicBool>,
    rx: Option<oneshot::Receiver<Result<T, JobError>>>,
    inner: Weak<Inner>,
}

impl<T> std::fmt::Debug for JobHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("cancelled", &self.token.is_cancelled())
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl<T> JobHandle<T> {
    /// Requests cancellation; the handle then resolves to an error.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// The job's cancellation token.
    pub fn token(&self) -> &CancelToken {
        &self.token
    }

    /// True once the job ran (or was skipped because it was cancelled).
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// The result if the job finished successfully and it was not taken yet.
    pub fn try_take(&mut self) -> Option<T> {
        let rx = self.rx.as_mut()?;
        match rx.try_recv() {
            Ok(Some(outcome)) => {
                self.rx = None;
                outcome.ok()
            }
            Ok(None) => None,
            Err(_) => {
                self.rx = None;
                None
            }
        }
    }

    /// On a scheduler thread, runs the job here if it has not started yet (so a job waiting
    /// for another one cannot deadlock the pool).
    fn help(&self) {
        if !pool::on_worker() {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            if let Some(job) = inner.steal(self.id) {
                inner.run_stolen(job);
            }
        }
    }
}

impl<T: Send + 'static> JobHandle<T> {
    /// Resolves to the result, or the reason there is none.
    pub fn outcome(mut self) -> impl Future<Output = Result<T, JobError>> + Send + 'static {
        let rx = self.rx.take();
        async move {
            match rx {
                Some(rx) => rx.await.unwrap_or(Err(JobError::Cancelled)),
                None => Err(JobError::Cancelled),
            }
        }
    }

    /// Resolves to the result, or `None` if the job was cancelled or panicked.
    pub fn into_future(self) -> impl Future<Output = Option<T>> + Send + 'static {
        let outcome = self.outcome();
        async move { outcome.await.ok() }
    }

    /// Blocks the calling thread until the job resolves (headless callers, tests, or a job
    /// that needs another job's result: on a scheduler thread a job that has not started yet
    /// runs inline).
    pub fn wait_outcome(self) -> Result<T, JobError> {
        self.help();
        futures::executor::block_on(self.outcome())
    }

    /// [`JobHandle::wait_outcome`] without the reason.
    pub fn wait(self) -> Option<T> {
        self.wait_outcome().ok()
    }
}

#[cfg(test)]
mod tests;
