//! The scheduler's state and threads: four lanes of queues (compute, with one worker reserved
//! for interactive jobs; the bounded blocking lane; the serial lane, one job per key at a time;
//! the guaranteed lane), the debounce timer, and (in [`quit`]) the quit drain and shutdown.
//!
//! **Lock discipline.** Everything here runs under one mutex, taken only through
//! [`Inner::lock`]. It is never held while user code runs: a job body, a runner, a token's
//! cancellation (which runs its `on_cancel` hooks), a waker (a job's result sender is part of
//! its body), or the drop of a body or closure (arbitrary destructors; a body also holds a
//! [`Registration`], whose drop takes the lock). Debug builds check it: [`Inner::lock`] counts
//! the locks this thread holds and [`assert_unlocked`] panics where user code is entered with
//! one held, instead of deadlocking.

mod quit;

pub use quit::{AbandonedWrite, DrainReport};

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use lector_base::thread::{spawn_named, ThreadKind};
use lector_base::CancelToken;
use parking_lot::{Condvar, Mutex, MutexGuard};

use super::key::{JobKey, JobKind, Lane, Subject};
use super::ticket::Ticket;
use super::Priority;

/// A job body handed to a [`Runner`]; it returns at once when its job was cancelled.
pub type Body = Box<dyn FnOnce() + Send + 'static>;

/// Runs jobs somewhere else than on the scheduler's own threads (GPUI's test executor):
/// `runner(delay, body)` must call `body` once, after `delay`.
pub type Runner = Arc<dyn Fn(Duration, Body) + Send + Sync + 'static>;

/// A thread of an on-demand lane with nothing to do for this long exits.
const ELASTIC_IDLE: Duration = Duration::from_secs(30);

thread_local! {
    /// Set on the scheduler's own threads: a job that waits for another job runs it inline
    /// when it has not started yet (see [`Inner::steal`]).
    static ON_WORKER: Cell<bool> = const { Cell::new(false) };
    /// How many scheduler locks this thread holds (debug check of the lock discipline).
    static LOCKS_HELD: Cell<u32> = const { Cell::new(0) };
}

/// Whether the current thread is a scheduler worker.
pub(super) fn on_worker() -> bool {
    ON_WORKER.with(Cell::get)
}

/// Debug builds: panics when this thread holds a scheduler lock. Called where user code is
/// entered (job bodies, runners, token cancellation) and before taking the lock (re-entry
/// would deadlock), so a violation fails a test instead of hanging the app.
pub(super) fn assert_unlocked(what: &str) {
    if cfg!(debug_assertions) {
        let held = LOCKS_HELD.with(Cell::get);
        assert!(
            held == 0,
            "scheduler: {what} while holding the scheduler lock"
        );
    }
}

/// The scheduler lock, held: counts itself in [`LOCKS_HELD`] for [`assert_unlocked`].
pub(super) struct Locked<'a> {
    guard: MutexGuard<'a, State>,
}

impl Deref for Locked<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.guard
    }
}

impl DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.guard
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        LOCKS_HELD.with(|n| n.set(n.get().saturating_sub(1)));
    }
}

/// Keeps a job registered under its key (so a newer job with the key, or cancelling its
/// subject, still reaches it) until its result has been delivered or nobody can take it any
/// more. Shared by the job's body (until it has run) and its handle (until the result is taken
/// or the handle dropped); the entry goes when the last holder drops it, or at delivery.
pub(super) struct Registration {
    inner: Weak<Inner>,
    key: JobKey,
    id: u64,
}

impl Registration {
    pub(super) fn new(inner: &Arc<Inner>, key: JobKey, id: u64) -> Arc<Registration> {
        Arc::new(Registration {
            inner: Arc::downgrade(inner),
            key,
            id,
        })
    }

    /// Whether a finished job's value may be delivered now: it is still registered and its
    /// token not cancelled. Checks and unregisters under the scheduler lock, so a replacement
    /// or subject cancellation either happened before (not delivered) or after (delivered).
    pub(super) fn deliver(&self, token: &CancelToken) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return !token.is_cancelled();
        };
        let mut st = inner.lock();
        let present = st
            .live
            .get(&self.key)
            .is_some_and(|e| e.iter().any(|(i, _)| *i == self.id));
        let removed = unregister(&mut st, &self.key, self.id);
        drop(st);
        drop(removed);
        present && !token.is_cancelled()
    }
}

/// Takes the scheduler lock: never drop a job body (it holds a `Registration`) while holding
/// it ([`Inner::lock`] checks this in debug builds).
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut st = inner.lock();
            let removed = unregister(&mut st, &self.key, self.id);
            drop(st);
            drop(removed);
        }
    }
}

pub(super) struct Job {
    pub(super) id: u64,
    pub(super) key: JobKey,
    pub(super) prio: Priority,
    pub(super) body: Body,
    /// What the job does, for the abandoned-write report ("git.commit").
    pub(super) what: Option<Arc<str>>,
    queued_at: Instant,
    /// Already logged as waiting long for a blocking-lane slot.
    warned: bool,
}

impl Job {
    pub(super) fn new(id: u64, key: JobKey, prio: Priority, body: Body) -> Job {
        Job {
            id,
            key,
            prio,
            body,
            what: None,
            queued_at: Instant::now(),
            warned: false,
        }
    }

    /// With a description for diagnostics.
    pub(super) fn described(mut self, what: Option<Arc<str>>) -> Job {
        self.what = what;
        self
    }
}

/// A job running on a scheduler thread (or handed to the runner).
#[derive(Clone)]
pub(super) struct Active {
    pub(super) kind: JobKind,
    pub(super) subject: Subject,
    pub(super) what: Option<Arc<str>>,
}

impl Active {
    fn of(job: &Job) -> Active {
        Active {
            kind: job.key.kind,
            subject: job.key.subject.clone(),
            what: job.what.clone(),
        }
    }
}

struct Debounced {
    id: u64,
    deadline: Instant,
    prio: Priority,
    ticket: Ticket,
    body: Body,
}

type Queues = [VecDeque<Job>; 4];

/// A lane whose threads start on demand: the blocking lane (bounded), the serial lane and the
/// guaranteed lane (unbounded).
///
/// Capacity accounting (all under the scheduler lock): `waiters` workers sit in the lane's
/// condvar; each is either unclaimed (`idle`) or claimed by a submitted job (`wakeups`), so
/// `idle + wakeups == waiters` always. A submission claims an unclaimed waiter (`idle -= 1`,
/// `wakeups += 1`, notify) or starts a new thread (up to the bound), so every queued job has a
/// claimed waiter or a starting thread of its own, unless the bounded lane is at its bound.
/// A waiter that leaves the condvar (woken, spuriously or by timeout) consumes a claim if
/// there is one, else leaves `idle`; it then takes the next queued job, if any.
#[derive(Default)]
pub(super) struct Elastic {
    queues: Queues,
    threads: usize,
    /// Waiting workers not claimed by a job.
    idle: usize,
    /// Waiting workers claimed by a submitted job and notified, not yet left the condvar.
    wakeups: usize,
}

impl Elastic {
    /// Takes capacity for one more queued job: `Some(true)` = start a thread, `Some(false)` = a
    /// waiter was claimed (notify one), `None` = at the bound (the job waits for a worker).
    fn claim(&mut self, bound: Option<usize>) -> Option<bool> {
        if self.idle > 0 {
            self.idle -= 1;
            self.wakeups += 1;
            Some(false)
        } else if bound.is_none_or(|max| self.threads < max) {
            self.threads += 1;
            Some(true)
        } else {
            None
        }
    }

    /// A waiter left the condvar.
    fn leave_wait(&mut self) {
        if self.wakeups > 0 {
            self.wakeups -= 1;
        } else {
            self.idle -= 1;
        }
    }
}

/// Index of a lane in `State::elastic` / `Inner::elastic_cv`.
const BLOCKING: usize = 0;
const SERIAL: usize = 1;
const GUARANTEED: usize = 2;
const ELASTIC_LANES: usize = 3;

fn elastic_index(lane: Lane) -> Option<usize> {
    match lane {
        Lane::Compute => None,
        Lane::Blocking => Some(BLOCKING),
        Lane::Serial => Some(SERIAL),
        Lane::Guaranteed => Some(GUARANTEED),
    }
}

/// A blocking-lane job waiting longer than this for a slot is logged (once).
const SLOT_WAIT_WARN: Duration = Duration::from_secs(3);

/// More threads than this at once on an unbounded lane (serial, guaranteed) is logged.
const UNBOUNDED_WARN: usize = 64;

#[derive(Default)]
pub(super) struct State {
    compute: Queues,
    elastic: [Elastic; ELASTIC_LANES],
    /// Serial lane: keys with a job queued on the lane or running. Their later jobs wait in
    /// `serial_waiting` until it finishes ([`Inner::finish`] releases the next one).
    serial_busy: HashSet<JobKey>,
    serial_waiting: HashMap<JobKey, VecDeque<Job>>,
    /// Every job queued or running, by key: (id, ticket).
    live: HashMap<JobKey, Vec<(u64, Ticket)>>,
    debounced: HashMap<JobKey, Debounced>,
    /// Jobs running on the scheduler's threads (or handed to the runner), by id.
    active: HashMap<u64, Active>,
    /// Scheduler threads (workers and timer) that have not exited.
    threads_alive: usize,
    /// Quitting: only kinds that are drained at quit are accepted and run.
    quitting: bool,
    shutdown: bool,
}

impl State {
    /// Every queue a worker takes jobs from (not the serial lane's waiting jobs).
    fn all_queues(&mut self) -> impl Iterator<Item = &mut VecDeque<Job>> {
        let compute = self.compute.iter_mut();
        compute.chain(self.elastic.iter_mut().flat_map(|e| e.queues.iter_mut()))
    }

    fn queued(&self) -> usize {
        let elastic = self.elastic.iter().flat_map(|e| e.queues.iter());
        let ready: usize = self.compute.iter().chain(elastic).map(VecDeque::len).sum();
        ready
            + self
                .serial_waiting
                .values()
                .map(VecDeque::len)
                .sum::<usize>()
    }

    fn idle(&self) -> bool {
        self.active.is_empty() && self.queued() == 0 && self.debounced.is_empty()
    }

    /// Takes the first queued job (highest priority first, lanes in order) that is drained at
    /// quit. A serial job queued here is its key's head, so running it keeps the order.
    fn pop_drained(&mut self) -> Option<Job> {
        for q in self.all_queues() {
            if let Some(pos) = q.iter().position(|j| j.key.kind.is_write()) {
                return q.remove(pos);
            }
        }
        None
    }

    /// After a serial job of `key` finished: its next waiting job, or the key is free.
    fn next_serial(&mut self, key: &JobKey) -> Option<Job> {
        let next = self
            .serial_waiting
            .get_mut(key)
            .and_then(VecDeque::pop_front);
        if self.serial_waiting.get(key).is_some_and(VecDeque::is_empty) {
            self.serial_waiting.remove(key);
        }
        if next.is_none() {
            self.serial_busy.remove(key);
        }
        next
    }
}

/// Which jobs a worker takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Compute jobs of any priority, highest first.
    General,
    /// Only `Interactive` compute jobs: one keystroke-driven job always finds a free worker,
    /// even when every general worker is busy with long background work.
    Reserved,
    /// Started at quit: only jobs drained at quit, from any lane, so the drain never waits for
    /// a worker held by a cancelled job that ignores its token.
    Drain,
}

/// What a queued job needs once the lock is released ([`Inner::start_dispatched`]).
enum Dispatch {
    /// Waits behind its key's running job on the serial lane.
    Waiting,
    Compute {
        interactive: bool,
    },
    /// `claim` as in [`Elastic::claim`].
    Elastic {
        ix: usize,
        claim: Option<bool>,
    },
    /// Hand it to the runner.
    Runner(Job),
}

/// How [`Inner::cancel_where`] stops what it matches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    /// Explicit cancellation: a write that already started is left alone ([`Ticket::revoke`]).
    Revoke,
    /// Quit: everything matched is cancelled ([`Ticket::abort`]).
    Abort,
}

pub(super) struct Inner {
    state: Mutex<State>,
    general_cv: Condvar,
    reserved_cv: Condvar,
    drain_cv: Condvar,
    elastic_cv: [Condvar; ELASTIC_LANES],
    timer_cv: Condvar,
    /// Notified whenever a job finishes or the scheduler's thread count changes.
    progress_cv: Condvar,
    next_id: AtomicU64,
    pub(super) general: usize,
    pub(super) blocking_max: usize,
    /// How long an idle thread of an on-demand lane waits for work before it exits.
    elastic_idle: Duration,
    runner: Option<Runner>,
}

impl Inner {
    /// A pool with `general` compute workers, one reserved interactive worker, a blocking
    /// lane of at most `blocking_max` threads, the serial and guaranteed lanes and the
    /// debounce timer.
    pub(super) fn start(general: usize, blocking_max: usize) -> Arc<Inner> {
        Inner::start_with_idle(general, blocking_max, ELASTIC_IDLE)
    }

    /// [`Inner::start`] with the idle timeout of on-demand threads.
    pub(super) fn start_with_idle(
        general: usize,
        blocking_max: usize,
        elastic_idle: Duration,
    ) -> Arc<Inner> {
        let mut inner = Inner::new(general.max(1), blocking_max.max(1), None);
        inner.elastic_idle = elastic_idle;
        let inner = Arc::new(inner);
        let mut roles = vec![Role::Reserved];
        roles.extend(std::iter::repeat_n(Role::General, inner.general));
        for (i, role) in roles.into_iter().enumerate() {
            let worker = inner.clone();
            let name = match role {
                Role::Reserved => "interactive".to_string(),
                _ => format!("compute-{i}"),
            };
            inner.start_thread(ThreadKind::SchedulerWorker, name, move || {
                worker.compute_loop(role);
            });
        }
        let timer = inner.clone();
        inner.start_thread(ThreadKind::SchedulerTimer, "debounce".into(), move || {
            timer.timer_loop();
        });
        inner
    }

    /// A scheduler without threads: every job is handed to `runner`.
    pub(super) fn with_runner(runner: Runner) -> Arc<Inner> {
        Arc::new(Inner::new(1, 1, Some(runner)))
    }

    fn new(general: usize, blocking_max: usize, runner: Option<Runner>) -> Inner {
        Inner {
            state: Mutex::new(State::default()),
            general_cv: Condvar::new(),
            reserved_cv: Condvar::new(),
            drain_cv: Condvar::new(),
            elastic_cv: [Condvar::new(), Condvar::new(), Condvar::new()],
            timer_cv: Condvar::new(),
            progress_cv: Condvar::new(),
            next_id: AtomicU64::new(1),
            general,
            blocking_max,
            elastic_idle: ELASTIC_IDLE,
            runner,
        }
    }

    /// Takes the scheduler lock (see the module docs for what may not happen while held).
    pub(super) fn lock(&self) -> Locked<'_> {
        assert_unlocked("taking the scheduler lock again");
        let guard = self.state.lock();
        LOCKS_HELD.with(|n| n.set(n.get() + 1));
        Locked { guard }
    }

    /// Starts a scheduler thread; counted in `threads_alive` until its loop returns.
    fn start_thread(
        self: &Arc<Self>,
        kind: ThreadKind,
        name: String,
        body: impl FnOnce() + Send + 'static,
    ) -> bool {
        self.lock().threads_alive += 1;
        let me = self.clone();
        let started = spawn_named(kind, name, move || {
            ON_WORKER.with(|w| w.set(true));
            // Leaves the count even if a job's bookkeeping ever panicked.
            let _alive = AliveGuard(me);
            body();
        });
        match started {
            Ok(_) => true,
            Err(err) => {
                tracing::error!("cannot start a scheduler thread ({kind}): {err}");
                let mut st = self.lock();
                st.threads_alive -= 1;
                false
            }
        }
    }

    pub(super) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers `job` under its key (cancelling what it replaces) and queues it on its lane
    /// (on the serial lane behind its key's running job). Returns `false` when it was refused
    /// (after shutdown, or a non-write while quitting): its body has then run with its ticket
    /// aborted.
    pub(super) fn submit(self: &Arc<Self>, ticket: Ticket, job: Job) -> bool {
        let mut st = self.lock();
        let refused = st.shutdown || (st.quitting && !job.key.kind.is_write());
        if refused {
            drop(st);
            ticket.abort();
            // The body sees the aborted ticket and only reports `Cancelled` to its waiter.
            (job.body)();
            return false;
        }
        let replaced = register(&mut st, &job.key, job.id, &ticket);
        let waits = job.key.kind.lane() == Lane::Serial && !st.serial_busy.insert(job.key.clone());
        let dispatch = if waits {
            let key = job.key.clone();
            st.serial_waiting.entry(key).or_default().push_back(job);
            Dispatch::Waiting
        } else {
            self.dispatch(&mut st, job)
        };
        let quitting = st.quitting;
        drop(st);
        // Replaced jobs are cancelled after the lock: their hooks may use the scheduler.
        for old in replaced {
            old.revoke();
        }
        self.start_dispatched(dispatch, quitting);
        true
    }

    /// Queues `job` on its lane (under the lock); [`Inner::start_dispatched`] wakes or starts
    /// a worker once the lock is released.
    fn dispatch(&self, st: &mut State, job: Job) -> Dispatch {
        if self.runner.is_some() {
            st.active.insert(job.id, Active::of(&job));
            return Dispatch::Runner(job);
        }
        let prio = job.prio;
        let Some(ix) = elastic_index(job.key.kind.lane()) else {
            st.compute[prio as usize].push_back(job);
            return Dispatch::Compute {
                interactive: prio == Priority::Interactive,
            };
        };
        let bound = (ix == BLOCKING).then_some(self.blocking_max);
        let e = &mut st.elastic[ix];
        e.queues[prio as usize].push_back(job);
        let claim = e.claim(bound);
        if bound.is_none() && e.threads > UNBOUNDED_WARN && claim == Some(true) {
            let lane = if ix == SERIAL { "serial" } else { "guaranteed" };
            tracing::warn!("{} {lane}-lane threads at once", e.threads);
        }
        Dispatch::Elastic { ix, claim }
    }

    /// The part of queuing a job that happens without the lock: notify a worker, start a
    /// thread, or call the runner.
    fn start_dispatched(self: &Arc<Self>, dispatch: Dispatch, quitting: bool) {
        assert_unlocked("starting a job");
        match dispatch {
            Dispatch::Waiting => {}
            Dispatch::Compute { interactive } => {
                self.general_cv.notify_one();
                if interactive {
                    self.reserved_cv.notify_one();
                }
            }
            Dispatch::Elastic { ix, claim } => match claim {
                Some(true) => self.grow(ix),
                Some(false) => {
                    self.elastic_cv[ix].notify_one();
                }
                // A full blocking lane: the timer logs jobs that wait long for a slot.
                None => {
                    self.timer_cv.notify_all();
                }
            },
            Dispatch::Runner(job) => {
                if let Some(runner) = self.runner.clone() {
                    let me = self.clone();
                    runner(Duration::ZERO, Box::new(move || me.run(job)));
                }
            }
        }
        if quitting {
            self.drain_cv.notify_all();
        }
    }

    fn grow(self: &Arc<Self>, ix: usize) {
        let me = self.clone();
        let n = self.next_id();
        let name = match ix {
            BLOCKING => format!("blocking-{n}"),
            SERIAL => format!("serial-{n}"),
            _ => format!("guaranteed-{n}"),
        };
        let started = self.start_thread(ThreadKind::SchedulerWorker, name, move || {
            me.elastic_loop(ix)
        });
        if !started {
            let mut st = self.lock();
            st.elastic[ix].threads -= 1;
            if st.elastic[ix].threads == 0 {
                tracing::error!("no thread could start for a lane of the scheduler; its jobs wait");
            }
        }
    }

    /// Runs `body` once, `delay` after the last call with `key` (keyed like a job when it runs).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn debounce(
        self: &Arc<Self>,
        id: u64,
        key: JobKey,
        delay: Duration,
        prio: Priority,
        ticket: Ticket,
        body: Body,
    ) {
        let mut st = self.lock();
        if st.shutdown || st.quitting {
            drop(st);
            // Dropped without the lock (it holds a `Registration`).
            drop(body);
            return;
        }
        if let Some(runner) = self.runner.clone() {
            // Under a runner the delay is the runner's (GPUI's fake clock in tests): the job is
            // registered now, so a newer call cancels this one while it waits.
            let replaced = register(&mut st, &key, id, &ticket);
            let job = Job::new(id, key, prio, body);
            st.active.insert(id, Active::of(&job));
            drop(st);
            for old in replaced {
                old.revoke();
            }
            let me = self.clone();
            assert_unlocked("calling the runner");
            runner(delay, Box::new(move || me.run(job)));
            return;
        }
        let deadline = Instant::now() + delay;
        let old = st.debounced.insert(
            key,
            Debounced {
                id,
                deadline,
                prio,
                ticket,
                body,
            },
        );
        drop(st);
        // Cancelled and dropped after the lock: a body holds a `Registration`, whose drop takes
        // the lock, and a token's hooks may use the scheduler.
        if let Some(old) = old {
            old.ticket.revoke();
        }
        self.timer_cv.notify_all();
    }

    /// Bookkeeping after a job ran (or was dropped unrun after it was queued); on the serial
    /// lane, releases its key's next job. The job's registration under its key ends with its
    /// [`Registration`], not here.
    fn finish(self: &Arc<Self>, id: u64, key: &JobKey) {
        let mut st = self.lock();
        st.active.remove(&id);
        let next = if key.kind.lane() == Lane::Serial {
            st.next_serial(key)
        } else {
            None
        };
        let quitting = st.quitting;
        let dispatch = next.map(|job| self.dispatch(&mut st, job));
        drop(st);
        if let Some(dispatch) = dispatch {
            self.start_dispatched(dispatch, quitting);
        }
        self.progress_cv.notify_all();
    }

    fn run(self: &Arc<Self>, job: Job) {
        let Job { id, key, body, .. } = job;
        assert_unlocked("running a job");
        body();
        self.finish(id, &key);
    }

    fn compute_loop(self: &Arc<Self>, role: Role) {
        let cv = match role {
            Role::General => &self.general_cv,
            Role::Reserved => &self.reserved_cv,
            Role::Drain => &self.drain_cv,
        };
        loop {
            let job = {
                let mut st = self.lock();
                loop {
                    let next = match role {
                        Role::General => st.compute.iter_mut().find_map(VecDeque::pop_front),
                        Role::Reserved => st.compute[Priority::Interactive as usize].pop_front(),
                        Role::Drain => st.pop_drained(),
                    };
                    if let Some(job) = next {
                        st.active.insert(job.id, Active::of(&job));
                        break job;
                    }
                    if st.shutdown {
                        return;
                    }
                    cv.wait(&mut st.guard);
                }
            };
            self.run(job);
        }
    }

    fn elastic_loop(self: &Arc<Self>, ix: usize) {
        loop {
            let job = {
                let mut st = self.lock();
                loop {
                    let e = &mut st.elastic[ix];
                    if let Some(job) = e.queues.iter_mut().find_map(VecDeque::pop_front) {
                        st.active.insert(job.id, Active::of(&job));
                        break job;
                    }
                    if st.shutdown {
                        st.elastic[ix].threads -= 1;
                        return;
                    }
                    st.elastic[ix].idle += 1;
                    let timed_out = self.elastic_cv[ix]
                        .wait_for(&mut st.guard, self.elastic_idle)
                        .timed_out();
                    let e = &mut st.elastic[ix];
                    e.leave_wait();
                    if timed_out && e.queues.iter().all(VecDeque::is_empty) {
                        e.threads -= 1;
                        return;
                    }
                }
            };
            self.run(job);
        }
    }

    fn timer_loop(self: &Arc<Self>) {
        let mut st = self.lock();
        loop {
            if st.shutdown {
                return;
            }
            let now = Instant::now();
            let due: Vec<JobKey> = st
                .debounced
                .iter()
                .filter(|(_, d)| d.deadline <= now)
                .map(|(k, _)| k.clone())
                .collect();
            if !due.is_empty() {
                let jobs: Vec<(JobKey, Debounced)> = due
                    .into_iter()
                    .filter_map(|k| st.debounced.remove(&k).map(|d| (k, d)))
                    .collect();
                drop(st);
                for (key, d) in jobs {
                    let _ = self.submit(d.ticket, Job::new(d.id, key, d.prio, d.body));
                }
                st = self.lock();
                continue;
            }
            let next_warn = warn_slow_slots(&mut st, now);
            let next = st
                .debounced
                .values()
                .map(|d| d.deadline)
                .chain(next_warn)
                .min();
            match next {
                Some(deadline) => {
                    self.timer_cv.wait_until(&mut st.guard, deadline);
                }
                None => self.timer_cv.wait(&mut st.guard),
            }
        }
    }

    /// Takes compute job `id` out of its queue if it has not started: a compute worker that
    /// waits for it runs it inline instead of blocking, so a job waiting for another job can
    /// never deadlock the compute pool. Jobs of the other lanes are never run inline (they may
    /// wait without bound and would park a compute worker); their lanes guarantee progress.
    pub(super) fn steal(&self, id: u64) -> Option<Job> {
        let mut st = self.lock();
        let st = &mut *st;
        for q in st.compute.iter_mut() {
            if let Some(pos) = q.iter().position(|j| j.id == id) {
                let job = q.remove(pos)?;
                st.active.insert(job.id, Active::of(&job));
                return Some(job);
            }
        }
        None
    }

    /// Runs a job taken with [`Inner::steal`] on the calling thread.
    pub(super) fn run_stolen(self: &Arc<Self>, job: Job) {
        self.run(job);
    }

    /// Cancels every job with `key` and a pending debounce for it. A write that already started
    /// is not cancelled and stays registered (it reports its result).
    pub(super) fn cancel_key(&self, key: &JobKey) {
        let mut st = self.lock();
        let mut taken = Vec::new();
        if let Some(entries) = st.live.get_mut(key) {
            entries.retain(|(id, t)| {
                let take = t.revocable();
                if take {
                    taken.push((*id, t.clone()));
                }
                !take
            });
            if entries.is_empty() {
                st.live.remove(key);
            }
        }
        let debounced = st.debounced.remove(key);
        drop(st);
        let started: Vec<(u64, Ticket)> = taken.into_iter().filter(|(_, t)| !t.revoke()).collect();
        if !started.is_empty() {
            // A write started between the check and the revoke: it stays registered.
            self.lock()
                .live
                .entry(key.clone())
                .or_default()
                .extend(started);
        }
        if let Some(d) = debounced {
            d.ticket.revoke();
        }
    }

    /// Cancels the one job `id` of `key` at quit (it then reports `Cancelled` when its body
    /// runs).
    fn abort_id(&self, key: &JobKey, id: u64) {
        let ticket = {
            let mut st = self.lock();
            let entries = st.live.get_mut(key);
            entries.and_then(|e| e.iter().position(|(i, _)| *i == id).map(|p| e.remove(p).1))
        };
        if let Some(t) = ticket {
            t.abort();
        }
    }

    /// Cancels every job of `subject` whose kind is cancelled with its subject; returns how
    /// many jobs (queued, running or debounced) were cancelled.
    pub(super) fn cancel_subject(&self, subject: &Subject) -> usize {
        self.cancel_where(
            |k| &k.subject == subject && k.kind.cancelled_with_subject(),
            Stop::Revoke,
        )
    }

    /// Cancels every queued, running or debounced job whose key matches; returns how many.
    /// Tokens are cancelled and bodies dropped after the lock.
    pub(super) fn cancel_where(&self, hit: impl Fn(&JobKey) -> bool, stop: Stop) -> usize {
        let mut tickets = Vec::new();
        let mut st = self.lock();
        st.live.retain(|k, entries| {
            if hit(k) {
                entries.retain(|(_, t)| {
                    let take = stop == Stop::Abort || t.revocable();
                    if take {
                        tickets.push(t.clone());
                    }
                    !take
                });
            }
            !entries.is_empty()
        });
        let keys: Vec<JobKey> = st.debounced.keys().filter(|k| hit(k)).cloned().collect();
        let removed: Vec<Debounced> = keys.iter().filter_map(|k| st.debounced.remove(k)).collect();
        drop(st);
        tickets.extend(removed.iter().map(|d| d.ticket.clone()));
        let mut cancelled = 0;
        for t in &tickets {
            match stop {
                Stop::Abort => {
                    t.abort();
                    cancelled += 1;
                }
                Stop::Revoke => cancelled += usize::from(t.revoke()),
            }
        }
        drop(removed);
        cancelled
    }

    pub(super) fn pending(&self) -> usize {
        let st = self.lock();
        st.queued() + st.debounced.len()
    }

    pub(super) fn live_jobs(&self) -> usize {
        self.lock().live.values().map(Vec::len).sum()
    }

    pub(super) fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        while !st.idle() {
            if self
                .progress_cv
                .wait_until(&mut st.guard, deadline)
                .timed_out()
            {
                return st.idle();
            }
        }
        true
    }

    pub(super) fn is_shut_down(&self) -> bool {
        self.lock().shutdown
    }

    pub(super) fn has_runner(&self) -> bool {
        self.runner.is_some()
    }

    #[cfg(test)]
    pub(super) fn blocking_threads(&self) -> usize {
        self.lock().elastic[BLOCKING].threads
    }

    #[cfg(test)]
    pub(super) fn threads_alive(&self) -> usize {
        self.lock().threads_alive
    }
}

/// Logs (once per job) blocking-lane jobs that have waited [`SLOT_WAIT_WARN`] for a slot;
/// returns when the next one would be due.
fn warn_slow_slots(st: &mut State, now: Instant) -> Option<Instant> {
    let mut next: Option<Instant> = None;
    for q in st.elastic[BLOCKING].queues.iter_mut() {
        for job in q.iter_mut().filter(|j| !j.warned) {
            let due = job.queued_at + SLOT_WAIT_WARN;
            if due <= now {
                job.warned = true;
                tracing::warn!(
                    kind = ?job.key.kind,
                    "a job has waited {:?} for a blocking-lane slot (all busy)",
                    now - job.queued_at
                );
            } else {
                next = Some(next.map_or(due, |n| n.min(due)));
            }
        }
    }
    next
}

/// Decrements `threads_alive` when a scheduler thread's loop returns (or unwinds).
struct AliveGuard(Arc<Inner>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.threads_alive = st.threads_alive.saturating_sub(1);
        drop(st);
        self.0.progress_cv.notify_all();
    }
}

/// Registers job `id` under `key`; returns the tickets it replaces, to be revoked once the
/// lock is released (revoking runs `on_cancel` hooks).
fn register(st: &mut State, key: &JobKey, id: u64, ticket: &Ticket) -> Vec<Ticket> {
    let entries = st.live.entry(key.clone()).or_default();
    let replaced = if key.kind.replaces() {
        entries.drain(..).map(|(_, t)| t).collect()
    } else {
        Vec::new()
    };
    entries.push((id, ticket.clone()));
    replaced
}

/// Removes job `id` from `key`'s entries; returns what was removed, to be dropped after the
/// lock (a ticket can hold the last reference to a token's hooks).
fn unregister(st: &mut State, key: &JobKey, id: u64) -> Vec<(u64, Ticket)> {
    let mut removed = Vec::new();
    if let Some(entries) = st.live.get_mut(key) {
        let mut i = 0;
        while i < entries.len() {
            if entries[i].0 == id {
                removed.push(entries.remove(i));
            } else {
                i += 1;
            }
        }
        if entries.is_empty() {
            st.live.remove(key);
        }
    }
    removed
}
