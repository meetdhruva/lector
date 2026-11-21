//! The scheduler's state and threads: three lanes of priority queues (compute, with one worker
//! reserved for interactive jobs; the bounded blocking lane; the guaranteed lane), the debounce
//! timer, the quit drain and shutdown. Everything here runs under one mutex that is never held while a job runs, while a
//! thread is started or while a runner is called.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use lector_base::thread::{spawn_named, ThreadKind};
use lector_base::CancelToken;
use parking_lot::{Condvar, Mutex, MutexGuard};

use super::key::{AtQuit, JobKey, JobKind, Lane, Subject};
use super::Priority;

/// A job body handed to a [`Runner`]; it returns at once when its job was cancelled.
pub type Body = Box<dyn FnOnce() + Send + 'static>;

/// Runs jobs somewhere else than on the scheduler's own threads (GPUI's test executor):
/// `runner(delay, body)` must call `body` once, after `delay`.
pub type Runner = Arc<dyn Fn(Duration, Body) + Send + Sync + 'static>;

/// A blocking-lane thread with nothing to do for this long exits.
const BLOCKING_IDLE: Duration = Duration::from_secs(30);

thread_local! {
    /// Set on the scheduler's own threads: a job that waits for another job runs it inline
    /// when it has not started yet (see [`Inner::steal`]).
    static ON_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread is a scheduler worker.
pub(super) fn on_worker() -> bool {
    ON_WORKER.with(Cell::get)
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
        let mut st = inner.state.lock();
        let present = st
            .live
            .get(&self.key)
            .is_some_and(|e| e.iter().any(|(i, _)| *i == self.id));
        unregister(&mut st, &self.key, self.id);
        present && !token.is_cancelled()
    }
}

/// Never drop a job body (it holds a `Registration`) while holding the scheduler lock: this
/// takes it.
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut st = inner.state.lock();
            unregister(&mut st, &self.key, self.id);
        }
    }
}

pub(super) struct Job {
    pub(super) id: u64,
    pub(super) key: JobKey,
    pub(super) body: Body,
    queued_at: Instant,
    /// Already logged as waiting long for a blocking-lane slot.
    warned: bool,
}

impl Job {
    pub(super) fn new(id: u64, key: JobKey, body: Body) -> Job {
        Job {
            id,
            key,
            body,
            queued_at: Instant::now(),
            warned: false,
        }
    }
}

struct Debounced {
    id: u64,
    deadline: Instant,
    prio: Priority,
    token: CancelToken,
    body: Body,
}

type Queues = [VecDeque<Job>; 4];

/// A lane whose threads start on demand: the blocking lane (bounded) and the guaranteed lane.
///
/// Capacity accounting (all under the scheduler lock): `waiters` workers sit in the lane's
/// condvar; each is either unclaimed (`idle`) or claimed by a submitted job (`wakeups`), so
/// `idle + wakeups == waiters` always. A submission claims an unclaimed waiter (`idle -= 1`,
/// `wakeups += 1`, notify) or starts a new thread (up to the bound), so every queued job has a
/// claimed waiter or a starting thread of its own, unless the bounded lane is at its bound.
/// A waiter that leaves the condvar (woken, spuriously or by timeout) consumes a claim if
/// there is one, else leaves `idle`; it then takes the next queued job, if any.
#[derive(Default)]
struct Elastic {
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
const GUARANTEED: usize = 1;

/// A blocking-lane job waiting longer than this for a slot is logged (once).
const SLOT_WAIT_WARN: Duration = Duration::from_secs(3);

/// More guaranteed-lane threads than this at once is logged (it has no bound).
const GUARANTEED_WARN: usize = 64;

#[derive(Default)]
struct State {
    compute: Queues,
    elastic: [Elastic; 2],
    /// Every job queued or running, by key: (id, token).
    live: HashMap<JobKey, Vec<(u64, CancelToken)>>,
    debounced: HashMap<JobKey, Debounced>,
    /// Jobs running on the scheduler's threads (or handed to the runner), by id.
    active: HashMap<u64, JobKind>,
    /// Scheduler threads (workers and timer) that have not exited.
    threads_alive: usize,
    /// Quitting: only kinds that are drained at quit are accepted and run.
    quitting: bool,
    shutdown: bool,
}

impl State {
    fn all_queues(&mut self) -> impl Iterator<Item = &mut VecDeque<Job>> {
        let [blocking, guaranteed] = &mut self.elastic;
        self.compute
            .iter_mut()
            .chain(blocking.queues.iter_mut())
            .chain(guaranteed.queues.iter_mut())
    }

    fn queued(&self) -> usize {
        let elastic = self.elastic.iter().flat_map(|e| e.queues.iter());
        self.compute.iter().chain(elastic).map(VecDeque::len).sum()
    }

    fn idle(&self) -> bool {
        self.active.is_empty() && self.queued() == 0 && self.debounced.is_empty()
    }

    /// Takes the first queued job (highest priority first, lanes in order) that is drained at
    /// quit.
    fn pop_drained(&mut self) -> Option<Job> {
        for q in self.all_queues() {
            if let Some(pos) = q
                .iter()
                .position(|j| j.key.kind.at_quit() != AtQuit::Cancel)
            {
                return q.remove(pos);
            }
        }
        None
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

/// What [`Inner::drain`] could not finish within its bounds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Kinds of writes abandoned (one entry per job): still queued or running at the bound,
    /// or network writes past their grace.
    pub abandoned: Vec<JobKind>,
}

pub(super) struct Inner {
    state: Mutex<State>,
    general_cv: Condvar,
    reserved_cv: Condvar,
    drain_cv: Condvar,
    elastic_cv: [Condvar; 2],
    timer_cv: Condvar,
    /// Notified whenever a job finishes or the scheduler's thread count changes.
    progress_cv: Condvar,
    next_id: AtomicU64,
    pub(super) general: usize,
    pub(super) blocking_max: usize,
    runner: Option<Runner>,
}

impl Inner {
    /// A pool with `general` compute workers, one reserved interactive worker, a blocking
    /// lane of at most `blocking_max` threads, the guaranteed lane and the debounce timer.
    pub(super) fn start(general: usize, blocking_max: usize) -> Arc<Inner> {
        let inner = Arc::new(Inner::new(general.max(1), blocking_max.max(1), None));
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
            elastic_cv: [Condvar::new(), Condvar::new()],
            timer_cv: Condvar::new(),
            progress_cv: Condvar::new(),
            next_id: AtomicU64::new(1),
            general,
            blocking_max,
            runner,
        }
    }

    /// Starts a scheduler thread; counted in `threads_alive` until its loop returns.
    fn start_thread(
        self: &Arc<Self>,
        kind: ThreadKind,
        name: String,
        body: impl FnOnce() + Send + 'static,
    ) -> bool {
        self.state.lock().threads_alive += 1;
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
                let mut st = self.state.lock();
                st.threads_alive -= 1;
                false
            }
        }
    }

    pub(super) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers `job` under its key (cancelling what it replaces) and queues it on its lane.
    /// Returns `false` when it was refused (after shutdown, or a non-write while quitting): its
    /// body has then run with a cancelled token.
    pub(super) fn submit(self: &Arc<Self>, prio: Priority, token: CancelToken, job: Job) -> bool {
        let mut st = self.state.lock();
        let refused = st.shutdown || (st.quitting && job.key.kind.at_quit() == AtQuit::Cancel);
        if refused {
            drop(st);
            token.cancel();
            // The body sees the cancelled token and only reports `Cancelled` to its waiter.
            (job.body)();
            return false;
        }
        register(&mut st, &job.key, job.id, &token);
        if let Some(runner) = self.runner.clone() {
            st.active.insert(job.id, job.key.kind);
            drop(st);
            let me = self.clone();
            let Job { id, key, body, .. } = job;
            runner(
                Duration::ZERO,
                Box::new(move || {
                    body();
                    me.finish(id, &key);
                }),
            );
            return true;
        }
        let quitting = st.quitting;
        match job.key.kind.lane() {
            Lane::Compute => {
                st.compute[prio as usize].push_back(job);
                drop(st);
                self.general_cv.notify_one();
                if prio == Priority::Interactive {
                    self.reserved_cv.notify_one();
                }
            }
            lane => {
                let ix = if lane == Lane::Blocking {
                    BLOCKING
                } else {
                    GUARANTEED
                };
                let bound = (ix == BLOCKING).then_some(self.blocking_max);
                let e = &mut st.elastic[ix];
                e.queues[prio as usize].push_back(job);
                let claim = e.claim(bound);
                if ix == GUARANTEED && e.threads > GUARANTEED_WARN && claim == Some(true) {
                    tracing::warn!("{} guaranteed-lane threads at once", e.threads);
                }
                drop(st);
                match claim {
                    Some(true) => self.grow(ix),
                    Some(false) => {
                        self.elastic_cv[ix].notify_one();
                    }
                    // A full blocking lane: the timer logs jobs that wait long for a slot.
                    None => {
                        self.timer_cv.notify_all();
                    }
                }
            }
        }
        if quitting {
            self.drain_cv.notify_all();
        }
        true
    }

    fn grow(self: &Arc<Self>, ix: usize) {
        let me = self.clone();
        let n = self.next_id();
        let name = if ix == BLOCKING {
            format!("blocking-{n}")
        } else {
            format!("guaranteed-{n}")
        };
        let started = self.start_thread(ThreadKind::SchedulerWorker, name, move || {
            me.elastic_loop(ix)
        });
        if !started {
            let mut st = self.state.lock();
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
        token: CancelToken,
        body: Body,
    ) {
        if let Some(runner) = self.runner.clone() {
            // Under a runner the delay is the runner's (GPUI's fake clock in tests): the job is
            // registered now, so a newer call cancels this one while it waits.
            let mut st = self.state.lock();
            if st.shutdown || st.quitting {
                return;
            }
            register(&mut st, &key, id, &token);
            st.active.insert(id, key.kind);
            drop(st);
            let me = self.clone();
            runner(
                delay,
                Box::new(move || {
                    body();
                    me.finish(id, &key);
                }),
            );
            return;
        }
        let mut st = self.state.lock();
        if st.shutdown || st.quitting {
            return;
        }
        let old = st.debounced.insert(
            key,
            Debounced {
                id,
                deadline: Instant::now() + delay,
                prio,
                token,
                body,
            },
        );
        // Dropped after the lock: a body holds a `Registration`, whose drop takes the lock.
        drop(st);
        if let Some(old) = old {
            old.token.cancel();
        }
        self.timer_cv.notify_all();
    }

    /// Bookkeeping after a job ran (or was handed back by the runner).
    /// The job's registration under its key ends with its [`Registration`], not here.
    fn finish(&self, id: u64, _key: &JobKey) {
        let mut st = self.state.lock();
        st.active.remove(&id);
        drop(st);
        self.progress_cv.notify_all();
    }

    fn run(&self, job: Job) {
        let Job { id, key, body, .. } = job;
        body();
        self.finish(id, &key);
    }

    fn compute_loop(&self, role: Role) {
        let cv = match role {
            Role::General => &self.general_cv,
            Role::Reserved => &self.reserved_cv,
            Role::Drain => &self.drain_cv,
        };
        loop {
            let job = {
                let mut st = self.state.lock();
                loop {
                    let next = match role {
                        Role::General => st.compute.iter_mut().find_map(VecDeque::pop_front),
                        Role::Reserved => st.compute[Priority::Interactive as usize].pop_front(),
                        Role::Drain => st.pop_drained(),
                    };
                    if let Some(job) = next {
                        st.active.insert(job.id, job.key.kind);
                        break job;
                    }
                    if st.shutdown {
                        return;
                    }
                    cv.wait(&mut st);
                }
            };
            self.run(job);
        }
    }

    fn elastic_loop(&self, ix: usize) {
        loop {
            let job = {
                let mut st = self.state.lock();
                loop {
                    let e = &mut st.elastic[ix];
                    if let Some(job) = e.queues.iter_mut().find_map(VecDeque::pop_front) {
                        st.active.insert(job.id, job.key.kind);
                        break job;
                    }
                    if st.shutdown {
                        st.elastic[ix].threads -= 1;
                        return;
                    }
                    st.elastic[ix].idle += 1;
                    let timed_out = self.elastic_cv[ix]
                        .wait_for(&mut st, BLOCKING_IDLE)
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
        let mut st = self.state.lock();
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
                    let _ = self.submit(d.prio, d.token, Job::new(d.id, key, d.body));
                }
                st = self.state.lock();
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
                    self.timer_cv.wait_until(&mut st, deadline);
                }
                None => self.timer_cv.wait(&mut st),
            }
        }
    }

    /// Takes compute job `id` out of its queue if it has not started: a compute worker that
    /// waits for it runs it inline instead of blocking, so a job waiting for another job can
    /// never deadlock the compute pool. Jobs of the other lanes are never run inline (they may
    /// wait without bound and would park a compute worker); their lanes guarantee progress.
    pub(super) fn steal(&self, id: u64) -> Option<Job> {
        let mut st = self.state.lock();
        let st = &mut *st;
        for q in st.compute.iter_mut() {
            if let Some(pos) = q.iter().position(|j| j.id == id) {
                let job = q.remove(pos)?;
                st.active.insert(job.id, job.key.kind);
                return Some(job);
            }
        }
        None
    }

    /// Runs a job taken with [`Inner::steal`] on the calling thread.
    pub(super) fn run_stolen(&self, job: Job) {
        self.run(job);
    }

    /// Cancels every job with `key` and a pending debounce for it.
    pub(super) fn cancel_key(&self, key: &JobKey) {
        let mut st = self.state.lock();
        let entries = st.live.remove(key).unwrap_or_default();
        let debounced = st.debounced.remove(key);
        drop(st);
        for (_, token) in entries {
            token.cancel();
        }
        if let Some(d) = debounced {
            d.token.cancel();
        }
    }

    /// Cancels the one job `id` of `key` (it then reports `Cancelled` when its body runs).
    fn cancel_id(&self, key: &JobKey, id: u64) {
        let token = {
            let mut st = self.state.lock();
            let entries = st.live.get_mut(key);
            entries.and_then(|e| e.iter().position(|(i, _)| *i == id).map(|p| e.remove(p).1))
        };
        if let Some(t) = token {
            t.cancel();
        }
    }

    /// Cancels every job of `subject` whose kind is cancelled with its subject; returns how
    /// many jobs (queued, running or debounced) were cancelled.
    pub(super) fn cancel_subject(&self, subject: &Subject) -> usize {
        self.cancel_where(|k| &k.subject == subject && k.kind.cancelled_with_subject())
    }

    /// Cancels every queued, running or debounced job whose key matches; returns how many.
    fn cancel_where(&self, hit: impl Fn(&JobKey) -> bool) -> usize {
        let mut tokens = Vec::new();
        let mut st = self.state.lock();
        st.live.retain(|k, entries| {
            if hit(k) {
                tokens.extend(entries.drain(..).map(|(_, t)| t));
                false
            } else {
                true
            }
        });
        let keys: Vec<JobKey> = st.debounced.keys().filter(|k| hit(k)).cloned().collect();
        let removed: Vec<Debounced> = keys.iter().filter_map(|k| st.debounced.remove(k)).collect();
        // Bodies are dropped after the lock (their `Registration` takes it).
        drop(st);
        tokens.extend(removed.into_iter().map(|d| d.token));
        for t in &tokens {
            t.cancel();
        }
        tokens.len()
    }

    pub(super) fn pending(&self) -> usize {
        let st = self.state.lock();
        st.queued() + st.debounced.len()
    }

    pub(super) fn live_jobs(&self) -> usize {
        self.state.lock().live.values().map(Vec::len).sum()
    }

    pub(super) fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.state.lock();
        while !st.idle() {
            if self.progress_cv.wait_until(&mut st, deadline).timed_out() {
                return st.idle();
            }
        }
        true
    }

    /// The first step of quitting: from now on only kinds drained at quit are accepted. Every
    /// other job is cancelled (queued ones dropped, their waiters told), debounces dropped, and a
    /// drain worker started so queued writes run even if every worker is busy. Returns how many
    /// jobs were cancelled.
    pub(super) fn begin_quit(self: &Arc<Self>) -> usize {
        let mut st = self.state.lock();
        if st.quitting || st.shutdown {
            return 0;
        }
        st.quitting = true;
        let mut dropped: Vec<Job> = Vec::new();
        for q in st.all_queues() {
            let (cancel, keep): (VecDeque<Job>, VecDeque<Job>) = q
                .drain(..)
                .partition(|j| j.key.kind.at_quit() == AtQuit::Cancel);
            *q = keep;
            dropped.extend(cancel);
        }
        drop(st);
        let cancelled = self.cancel_where(|k| k.kind.at_quit() == AtQuit::Cancel);
        let debounced: Vec<Debounced> = self
            .state
            .lock()
            .debounced
            .drain()
            .map(|(_, d)| d)
            .collect();
        for d in &debounced {
            d.token.cancel();
        }
        for job in dropped {
            let Job { id, key, body, .. } = job;
            body();
            self.finish(id, &key);
        }
        if self.runner.is_none() {
            let me = self.clone();
            self.start_thread(
                ThreadKind::SchedulerWorker,
                "quit-drain".into(),
                move || me.compute_loop(Role::Drain),
            );
        }
        self.drain_cv.notify_all();
        cancelled + debounced.len()
    }

    /// Waits until every write ([`AtQuit::Drain`] / [`AtQuit::DrainBriefly`]) queued or running
    /// has finished, at most until `deadline`; [`AtQuit::DrainBriefly`] jobs (network writes)
    /// only until `brief_deadline`, after which they are cancelled. Call after
    /// [`Inner::begin_quit`]. Reports what was left.
    pub(super) fn drain(&self, deadline: Instant, brief_deadline: Instant) -> DrainReport {
        let mut report = DrainReport::default();
        let mut given_up: HashMap<u64, JobKind> = HashMap::new();
        let mut st = self.state.lock();
        loop {
            let now = Instant::now();
            if now >= brief_deadline {
                // Network writes past their grace: cancel and stop waiting for them.
                let mut dropped = Vec::new();
                for q in st.all_queues() {
                    while let Some(pos) = q
                        .iter()
                        .position(|j| j.key.kind.at_quit() == AtQuit::DrainBriefly)
                    {
                        if let Some(job) = q.remove(pos) {
                            dropped.push(job);
                        }
                    }
                }
                let running: Vec<(u64, JobKind)> = st
                    .active
                    .iter()
                    .filter(|(id, k)| {
                        k.at_quit() == AtQuit::DrainBriefly && !given_up.contains_key(id)
                    })
                    .map(|(id, k)| (*id, *k))
                    .collect();
                drop(st);
                for job in dropped {
                    report.abandoned.push(job.key.kind);
                    let Job { id, key, body, .. } = job;
                    self.cancel_id(&key, id);
                    body();
                    self.finish(id, &key);
                }
                if !running.is_empty() {
                    self.cancel_where(|k| k.kind.at_quit() == AtQuit::DrainBriefly);
                }
                for (id, kind) in running {
                    given_up.insert(id, kind);
                    report.abandoned.push(kind);
                }
                st = self.state.lock();
            }
            let pending_writes = st
                .all_queues()
                .flat_map(|q| q.iter())
                .any(|j| j.key.kind.at_quit() != AtQuit::Cancel)
                || st
                    .active
                    .iter()
                    .any(|(id, k)| k.at_quit() != AtQuit::Cancel && !given_up.contains_key(id));
            if !pending_writes {
                break;
            }
            if now >= deadline {
                for q in st.all_queues() {
                    for j in q.iter() {
                        if j.key.kind.at_quit() != AtQuit::Cancel {
                            report.abandoned.push(j.key.kind);
                        }
                    }
                }
                let left: Vec<JobKind> = st
                    .active
                    .iter()
                    .filter(|(id, k)| k.at_quit() != AtQuit::Cancel && !given_up.contains_key(id))
                    .map(|(_, k)| *k)
                    .collect();
                report.abandoned.extend(left);
                break;
            }
            let wake = if brief_deadline > now {
                deadline.min(brief_deadline)
            } else {
                deadline
            };
            self.progress_cv.wait_until(&mut st, wake);
        }
        report
    }

    /// Stops the scheduler: no new job runs, every queued job is dropped (its waiter gets
    /// `Cancelled`), every running job is cancelled, then waits up to `timeout` for the
    /// scheduler's threads to exit. Returns whether they all did.
    pub(super) fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock();
        let st = &mut *guard;
        st.shutdown = true;
        let mut dropped: Vec<Job> = Vec::new();
        for q in st.all_queues() {
            dropped.extend(q.drain(..));
        }
        // Cancel what is queued or running. A job that already finished keeps its token and
        // registration until its waiter takes the result (a write that completed during the
        // quit drain still reports success).
        let unfinished: std::collections::HashSet<u64> = dropped
            .iter()
            .map(|j| j.id)
            .chain(st.active.keys().copied())
            .collect();
        let mut tokens: Vec<CancelToken> = Vec::new();
        st.live.retain(|_, entries| {
            entries.retain(|(id, t)| {
                let cancel = unfinished.contains(id);
                if cancel {
                    tokens.push(t.clone());
                }
                !cancel
            });
            !entries.is_empty()
        });
        let debounced: Vec<Debounced> = st.debounced.drain().map(|(_, d)| d).collect();
        drop(guard);
        tokens.extend(debounced.into_iter().map(|d| d.token));
        for t in &tokens {
            t.cancel();
        }
        // Their tokens are cancelled: each body only tells its waiter.
        for job in dropped {
            (job.body)();
        }
        self.general_cv.notify_all();
        self.reserved_cv.notify_all();
        self.drain_cv.notify_all();
        for cv in &self.elastic_cv {
            cv.notify_all();
        }
        self.timer_cv.notify_all();
        let mut st = self.state.lock();
        while st.threads_alive > 0 {
            if self.progress_cv.wait_until(&mut st, deadline).timed_out() {
                break;
            }
        }
        let stopped = st.threads_alive == 0;
        if !stopped {
            tracing::warn!(
                "scheduler: {} thread(s) still running a job after {timeout:?}",
                st.threads_alive
            );
        }
        stopped
    }

    pub(super) fn is_shut_down(&self) -> bool {
        self.state.lock().shutdown
    }

    pub(super) fn has_runner(&self) -> bool {
        self.runner.is_some()
    }

    #[cfg(test)]
    pub(super) fn blocking_threads(&self) -> usize {
        self.state.lock().elastic[BLOCKING].threads
    }

    #[cfg(test)]
    pub(super) fn threads_alive(&self) -> usize {
        self.state.lock().threads_alive
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
        let mut st: MutexGuard<'_, State> = self.0.state.lock();
        st.threads_alive = st.threads_alive.saturating_sub(1);
        drop(st);
        self.0.progress_cv.notify_all();
    }
}

fn register(st: &mut State, key: &JobKey, id: u64, token: &CancelToken) {
    let entries = st.live.entry(key.clone()).or_default();
    if key.kind.replaces() {
        for (_, old) in entries.drain(..) {
            old.cancel();
        }
    }
    entries.push((id, token.clone()));
}

fn unregister(st: &mut State, key: &JobKey, id: u64) {
    if let Some(entries) = st.live.get_mut(key) {
        entries.retain(|(i, _)| *i != id);
        if entries.is_empty() {
            st.live.remove(key);
        }
    }
}
