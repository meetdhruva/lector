//! One process-wide job scheduler for short background work (ADR 0003).
//!
//! * Every job has a [`JobKey`] (a [`JobKind`] from the closed list in [`key`] plus a
//!   [`Subject`]) and a [`Priority`]. A newer job with the same key cancels older ones for kinds
//!   that [replace](JobKind::replaces); [`Scheduler::cancel_subject`] cancels everything of a
//!   document, repository, view... that closes. A cancelled job that has not started never runs;
//!   a running one sees its [`CancelToken`]; either way its result is never delivered.
//! * **Writes the user asked for** ([`JobKind::is_write`]) are never replaced, not cancelled with
//!   their subject, and once queued they run even if their token was cancelled before they
//!   started (the bridge's task was dropped because its view closed); their result is then
//!   dropped. Only [`Scheduler::cancel_key`] keeps a queued write from starting; a write that
//!   started runs to completion.
//! * **Compute lane**: `default_workers()` general workers serve the highest non-empty
//!   priority first (FIFO within a priority), plus **one worker reserved for `Interactive`
//!   jobs**, so a keystroke-driven job runs at once even when every general worker is busy
//!   with long background work. Only kinds marked [`JobKind::may_be_interactive`] get
//!   `Interactive`; others asking for it run at `Visible`.
//! * **Blocking lane**: kinds that wait on somebody else (network, language servers, agents,
//!   the user, external programs) run on their own threads, started on demand up to
//!   [`BLOCKING_MAX`] and stopped after 30 s idle, so they never occupy the compute workers.
//! * **Serial lane**: git writes (hooks, signing, pinentry: other people's programs with no
//!   timeout) run one per key at a time, in submission order, each key's jobs on a thread of
//!   its own (started on demand, stopped after 30 s idle). The key's subject is the repository,
//!   so one repository has one git write at a time (git requires it) while repositories do not
//!   wait for each other; the lane takes neither compute workers nor blocking-lane slots.
//! * A job that panics is caught: the worker survives, the panic is logged (and written to the
//!   crash log by the process's panic hook, see `health`) and the waiter gets
//!   [`JobError::Panicked`].
//! * A job that waits for a compute job ([`JobHandle::wait`]) on a scheduler thread runs that job
//!   inline if it has not started, so waiting on the compute pool cannot deadlock it; jobs of the
//!   other lanes are waited for (never run inline on a compute worker).
//! * **Guaranteed lane**: kinds that other jobs or a person wait on (credential prompts, agent
//!   requests and turns, cancel/undo of a turn, terminal kill escalation) always start at once, so a
//!   blocking lane full of waiters cannot keep them from starting.
//! * Quitting: [`Scheduler::begin_quit`] cancels everything but writes, [`Scheduler::drain`]
//!   lets writes finish within a bound (queued serial writes keep their order), and reports
//!   the ones it gave up on by kind, subject and description ([`AbandonedWrite`]);
//!   [`Scheduler::shutdown`] cancels the rest and waits a bounded time for the threads
//!   (orchestrated by `lector_app::shutdown`).
//! * The scheduler's lock is never held while user code runs (job bodies, runners, cancel
//!   hooks, destructors of dropped bodies); debug builds assert it.
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
mod ticket;

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use futures::channel::oneshot;
use lector_base::CancelToken;

pub use key::{AtQuit, JobKey, JobKind, Lane, Subject, Writes};
pub use pool::{AbandonedWrite, Body, DrainReport, Runner};

use pool::{Inner, Job, Registration};
use ticket::Ticket;

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
        self.try_spawn(key, prio, None, f).0
    }

    /// [`Scheduler::spawn`] with a description of what the job does (a command id such as
    /// `"git.commit"`): a write abandoned at quit is reported with it ([`AbandonedWrite`]).
    pub fn spawn_described<T: Send + 'static>(
        &self,
        key: JobKey,
        prio: Priority,
        what: impl Into<String>,
        f: impl FnOnce(&CancelToken) -> T + Send + 'static,
    ) -> JobHandle<T> {
        let what: Arc<str> = Arc::from(what.into());
        self.try_spawn(key, prio, Some(what), f).0
    }

    /// [`Scheduler::spawn`], also telling whether the job was accepted (`false`: refused after
    /// shutdown, or a non-write while quitting; the handle then resolves to `Cancelled`).
    fn try_spawn<T: Send + 'static>(
        &self,
        key: JobKey,
        prio: Priority,
        what: Option<Arc<str>>,
        f: impl FnOnce(&CancelToken) -> T + Send + 'static,
    ) -> (JobHandle<T>, bool) {
        let prio = prio.for_kind(key.kind);
        let ticket = Ticket::new(key.kind.runs_once_queued());
        let token = ticket.token().clone();
        let (tx, rx) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let id = self.inner().next_id();
        let reg = Registration::new(self.inner(), key.clone(), id);
        let body = guarded(
            key.kind,
            ticket.clone(),
            finished.clone(),
            f,
            tx,
            reg.clone(),
        );
        let job = Job::new(id, key, prio, body).described(what);
        let accepted = self.inner().submit(ticket, job);
        let handle = JobHandle {
            id,
            token,
            finished,
            rx: Some(rx),
            reg: Some(reg),
            inner: Arc::downgrade(self.inner()),
        };
        (handle, accepted)
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
        let ticket = Ticket::new(key.kind.runs_once_queued());
        let (tx, _rx) = oneshot::channel();
        let id = self.inner().next_id();
        let reg = Registration::new(self.inner(), key.clone(), id);
        let finished = Arc::new(AtomicBool::new(false));
        let body = guarded(key.kind, ticket.clone(), finished, f, tx, reg);
        self.inner().debounce(id, key, delay, prio, ticket, body);
    }

    /// A [`lector_base::thread::TaskRunner`] that runs each task as a job with `key` at `prio`:
    /// how crates below `lector-app` (which do not schedule, ADR 0003) get their short tasks
    /// run here. It honours the runner contract: a task the scheduler refuses (after shutdown,
    /// or a non-write kind while quitting) is handed back for the caller to run. A task taken
    /// and then cancelled before it started (its subject, the quit) is dropped, except for kinds
    /// drained at quit ([`AtQuit::Drain`], e.g. a terminal's kill escalation): cleanup that must
    /// happen runs wherever the job is dropped.
    pub fn task_runner(&self, key: JobKey, prio: Priority) -> lector_base::thread::TaskRunner {
        let scheduler = self.clone();
        let must_run = key.kind.at_quit() == AtQuit::Drain;
        Arc::new(move |task| {
            let pending = Arc::new(PendingTask {
                task: parking_lot::Mutex::new(Some(task)),
                must_run,
            });
            let job = pending.clone();
            let (_, accepted) = scheduler.try_spawn(key.clone(), prio, None, move |_| job.run());
            if accepted {
                return Ok(());
            }
            let handed_back = pending.task.lock().take();
            match handed_back {
                Some(task) => Err(task),
                None => Ok(()),
            }
        })
    }

    /// Cancels every job with `key` and any pending debounce for it. This is the one explicit
    /// way to cancel a [write](JobKind::is_write), and it only keeps a queued write from
    /// starting: a write that already started runs to completion and reports its result.
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

    /// First step of quitting: every job whose kind is [`AtQuit::Cancel`] is cancelled (queued
    /// ones dropped, debounces dropped) and later ones are refused; writes keep running, and a
    /// dedicated drain worker runs queued writes even if every worker is held. Returns how many
    /// jobs were cancelled.
    pub fn begin_quit(&self) -> usize {
        self.inner().begin_quit()
    }

    /// Waits for the writes queued or running at quit ([`Scheduler::begin_quit`] first): local
    /// writes up to `bound`, network writes ([`AtQuit::DrainBriefly`]) up to `network_grace`,
    /// after which they are cancelled. Reports the writes it gave up on.
    pub fn drain(&self, bound: Duration, network_grace: Duration) -> DrainReport {
        let now = std::time::Instant::now();
        self.inner()
            .drain(now + bound, now + network_grace.min(bound))
    }

    /// A scheduler like [`Scheduler::with_limits`] whose on-demand threads exit after `idle`
    /// without work (30 s otherwise).
    #[cfg(test)]
    pub(crate) fn with_idle_timeout(
        workers: usize,
        blocking_max: usize,
        idle: Duration,
    ) -> Scheduler {
        Scheduler {
            owner: Arc::new(Owner(Inner::start_with_idle(workers, blocking_max, idle))),
        }
    }

    /// Whether [`Scheduler::shutdown`] was called.
    pub fn is_shut_down(&self) -> bool {
        self.inner().is_shut_down()
    }
}

/// A task handed to [`Scheduler::task_runner`], shared by the job and the runner until one of
/// them takes it.
struct PendingTask {
    task: parking_lot::Mutex<Option<lector_base::thread::RunnerTask>>,
    /// Cleanup that must happen: run when dropped unrun (the job was cancelled before it ran).
    must_run: bool,
}

impl PendingTask {
    fn run(&self) {
        let task = self.task.lock().take();
        if let Some(task) = task {
            task();
        }
    }
}

impl Drop for PendingTask {
    fn drop(&mut self) {
        if self.must_run {
            if let Some(task) = self.task.get_mut().take() {
                task();
            }
        }
    }
}

/// Wraps `f`: skipped when its ticket does not let it start (a read whose token was cancelled,
/// a write the scheduler revoked), panics caught and reported, the result sent only if the
/// token is still not cancelled once `f` returns.
fn guarded<T: Send + 'static>(
    kind: JobKind,
    ticket: Ticket,
    finished: Arc<AtomicBool>,
    f: impl FnOnce(&CancelToken) -> T + Send + 'static,
    tx: oneshot::Sender<Result<T, JobError>>,
    reg: Arc<Registration>,
) -> Body {
    Box::new(move || {
        // Held until the body returns; the handle keeps the job registered after that.
        let _reg = reg;
        let token = ticket.token();
        let outcome = if !ticket.start() {
            Err(JobError::Cancelled)
        } else {
            match catch_unwind(AssertUnwindSafe(|| f(token))) {
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

/// Whether a finished job's value may be handed to its waiter (see [`Registration::deliver`]).
fn deliverable(reg: Option<&Registration>, token: &CancelToken) -> bool {
    match reg {
        Some(reg) => reg.deliver(token),
        None => !token.is_cancelled(),
    }
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
    /// Keeps the finished job registered until its value is taken (or this handle dropped).
    reg: Option<Arc<Registration>>,
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
    /// Requests cancellation; the handle then resolves to an error. A job that has not started
    /// is skipped, except a [write](JobKind::is_write), which still runs (only its result is
    /// dropped; see [`Scheduler::cancel_key`]).
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
                let reg = self.reg.take();
                outcome
                    .ok()
                    .filter(|_| deliverable(reg.as_deref(), &self.token))
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
    ///
    /// A value produced by a job that was replaced or cancelled before it is taken here (even
    /// after the job finished) resolves to [`JobError::Cancelled`]: the job stays registered
    /// under its key and subject until this point.
    pub fn outcome(mut self) -> impl Future<Output = Result<T, JobError>> + Send + 'static {
        let rx = self.rx.take();
        let reg = self.reg.take();
        let token = self.token.clone();
        async move {
            let outcome = match rx {
                Some(rx) => rx.await.unwrap_or(Err(JobError::Cancelled)),
                None => Err(JobError::Cancelled),
            };
            match outcome {
                Ok(value) if deliverable(reg.as_deref(), &token) => Ok(value),
                Ok(_) => Err(JobError::Cancelled),
                Err(e) => Err(e),
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
#[cfg(test)]
mod write_tests;
