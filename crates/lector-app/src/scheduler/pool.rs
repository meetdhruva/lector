//! The scheduler's state and threads: two lanes of priority queues, the compute workers (one
//! of them reserved for interactive jobs), the elastic blocking lane, the debounce timer and
//! shutdown. Everything here runs under one mutex that is never held while a job runs, while a
//! thread is started or while a runner is called.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lector_base::thread::{spawn_named, ThreadKind};
use lector_base::CancelToken;
use parking_lot::{Condvar, Mutex, MutexGuard};

use super::key::{JobKey, Lane, Subject};
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

pub(super) struct Job {
    pub(super) id: u64,
    pub(super) key: JobKey,
    pub(super) body: Body,
}

struct Debounced {
    deadline: Instant,
    prio: Priority,
    token: CancelToken,
    body: Body,
}

type Queues = [VecDeque<Job>; 4];

#[derive(Default)]
struct State {
    compute: Queues,
    blocking: Queues,
    /// Every job queued or running, by key: (id, token).
    live: HashMap<JobKey, Vec<(u64, CancelToken)>>,
    debounced: HashMap<JobKey, Debounced>,
    /// Jobs running on the scheduler's threads, or handed to the runner and not finished.
    running: usize,
    blocking_threads: usize,
    blocking_idle: usize,
    /// Scheduler threads (workers and timer) that have not exited.
    threads_alive: usize,
    shutdown: bool,
}

impl State {
    fn queued(&self) -> usize {
        self.compute
            .iter()
            .chain(self.blocking.iter())
            .map(VecDeque::len)
            .sum()
    }

    fn idle(&self) -> bool {
        self.running == 0 && self.queued() == 0 && self.debounced.is_empty()
    }
}

/// Which compute jobs a worker takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Any priority, highest first.
    General,
    /// Only `Interactive` jobs: one keystroke-driven job always finds a free worker, even when
    /// every general worker is busy with long background work.
    Reserved,
}

pub(super) struct Inner {
    state: Mutex<State>,
    general_cv: Condvar,
    reserved_cv: Condvar,
    blocking_cv: Condvar,
    timer_cv: Condvar,
    idle_cv: Condvar,
    next_id: AtomicU64,
    pub(super) general: usize,
    pub(super) blocking_max: usize,
    runner: Option<Runner>,
}

impl Inner {
    /// A pool with `general` compute workers, one reserved interactive worker, a blocking
    /// lane of at most `blocking_max` threads and the debounce timer.
    pub(super) fn start(general: usize, blocking_max: usize) -> Arc<Inner> {
        let inner = Arc::new(Inner::new(general.max(1), blocking_max.max(1), None));
        let mut roles = vec![Role::Reserved];
        roles.extend(std::iter::repeat_n(Role::General, inner.general));
        for (i, role) in roles.into_iter().enumerate() {
            let worker = inner.clone();
            let name = match role {
                Role::Reserved => "interactive".to_string(),
                Role::General => format!("compute-{i}"),
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
            blocking_cv: Condvar::new(),
            timer_cv: Condvar::new(),
            idle_cv: Condvar::new(),
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
    pub(super) fn submit(self: &Arc<Self>, prio: Priority, token: CancelToken, job: Job) {
        let mut st = self.state.lock();
        if st.shutdown {
            drop(st);
            token.cancel();
            // The body sees the cancelled token and only reports `Cancelled` to its waiter.
            (job.body)();
            return;
        }
        register(&mut st, &job.key, job.id, &token);
        if let Some(runner) = self.runner.clone() {
            st.running += 1;
            drop(st);
            let me = self.clone();
            let Job { id, key, body } = job;
            runner(
                Duration::ZERO,
                Box::new(move || {
                    body();
                    me.finish(id, &key);
                }),
            );
            return;
        }
        match job.key.kind.lane() {
            Lane::Compute => {
                st.compute[prio as usize].push_back(job);
                drop(st);
                self.general_cv.notify_one();
                if prio == Priority::Interactive {
                    self.reserved_cv.notify_one();
                }
            }
            Lane::Blocking => {
                st.blocking[prio as usize].push_back(job);
                let grow = st.blocking_idle == 0 && st.blocking_threads < self.blocking_max;
                if grow {
                    st.blocking_threads += 1;
                }
                drop(st);
                if grow {
                    self.grow_blocking();
                } else {
                    self.blocking_cv.notify_one();
                }
            }
        }
    }

    fn grow_blocking(self: &Arc<Self>) {
        let me = self.clone();
        let n = self.next_id();
        let started = self.start_thread(ThreadKind::SchedulerWorker, format!("blocking-{n}"), {
            move || me.blocking_loop()
        });
        if !started {
            let mut st = self.state.lock();
            st.blocking_threads -= 1;
            if st.blocking_threads == 0 {
                tracing::error!("no blocking-lane thread could start; blocking jobs wait");
            }
        }
    }

    /// Runs `body` once, `delay` after the last call with `key` (keyed like a job when it runs).
    pub(super) fn debounce(
        self: &Arc<Self>,
        key: JobKey,
        delay: Duration,
        prio: Priority,
        token: CancelToken,
        body: Body,
    ) {
        if let Some(runner) = self.runner.clone() {
            // Under a runner the delay is the runner's (GPUI's fake clock in tests): the job is
            // registered now, so a newer call cancels this one while it waits.
            let id = self.next_id();
            let mut st = self.state.lock();
            if st.shutdown {
                return;
            }
            register(&mut st, &key, id, &token);
            st.running += 1;
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
        if st.shutdown {
            return;
        }
        if let Some(old) = st.debounced.insert(
            key,
            Debounced {
                deadline: Instant::now() + delay,
                prio,
                token,
                body,
            },
        ) {
            old.token.cancel();
        }
        drop(st);
        self.timer_cv.notify_all();
    }

    /// Bookkeeping after a job ran (or was handed back by the runner).
    fn finish(&self, id: u64, key: &JobKey) {
        let mut st = self.state.lock();
        st.running = st.running.saturating_sub(1);
        unregister(&mut st, key, id);
        let idle = st.idle();
        drop(st);
        if idle {
            self.idle_cv.notify_all();
        }
    }

    fn run(&self, job: Job) {
        let Job { id, key, body } = job;
        body();
        self.finish(id, &key);
    }

    fn compute_loop(&self, role: Role) {
        let cv = match role {
            Role::General => &self.general_cv,
            Role::Reserved => &self.reserved_cv,
        };
        loop {
            let job = {
                let mut st = self.state.lock();
                loop {
                    let next = match role {
                        Role::General => st.compute.iter_mut().find_map(VecDeque::pop_front),
                        Role::Reserved => st.compute[Priority::Interactive as usize].pop_front(),
                    };
                    if let Some(job) = next {
                        st.running += 1;
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

    fn blocking_loop(&self) {
        loop {
            let job = {
                let mut st = self.state.lock();
                loop {
                    if let Some(job) = st.blocking.iter_mut().find_map(VecDeque::pop_front) {
                        st.running += 1;
                        break job;
                    }
                    if st.shutdown {
                        st.blocking_threads -= 1;
                        return;
                    }
                    st.blocking_idle += 1;
                    let timed_out = self.blocking_cv.wait_for(&mut st, BLOCKING_IDLE).timed_out();
                    st.blocking_idle -= 1;
                    let empty = st.blocking.iter().all(VecDeque::is_empty);
                    if timed_out && empty {
                        st.blocking_threads -= 1;
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
                    let id = self.next_id();
                    self.submit(
                        d.prio,
                        d.token,
                        Job {
                            id,
                            key,
                            body: d.body,
                        },
                    );
                }
                st = self.state.lock();
                continue;
            }
            match st.debounced.values().map(|d| d.deadline).min() {
                Some(deadline) => {
                    self.timer_cv.wait_until(&mut st, deadline);
                }
                None => self.timer_cv.wait(&mut st),
            }
        }
    }

    /// Takes job `id` out of its queue if it has not started (a worker that waits for it runs
    /// it inline instead of blocking: a job waiting for another job can never deadlock the pool).
    pub(super) fn steal(&self, id: u64) -> Option<Job> {
        let mut st = self.state.lock();
        let st = &mut *st;
        for q in st.compute.iter_mut().chain(st.blocking.iter_mut()) {
            if let Some(pos) = q.iter().position(|j| j.id == id) {
                let job = q.remove(pos)?;
                st.running += 1;
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

    /// Cancels every job of `subject` whose kind is cancelled with its subject; returns how
    /// many jobs (queued, running or debounced) were cancelled.
    pub(super) fn cancel_subject(&self, subject: &Subject) -> usize {
        let hit = |k: &JobKey| &k.subject == subject && k.kind.cancelled_with_subject();
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
        for k in keys {
            if let Some(d) = st.debounced.remove(&k) {
                tokens.push(d.token);
            }
        }
        drop(st);
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
            if self.idle_cv.wait_until(&mut st, deadline).timed_out() {
                return st.idle();
            }
        }
        true
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
        for q in st.compute.iter_mut().chain(st.blocking.iter_mut()) {
            dropped.extend(q.drain(..));
        }
        let mut tokens: Vec<CancelToken> = st
            .live
            .drain()
            .flat_map(|(_, e)| e.into_iter().map(|(_, t)| t))
            .collect();
        tokens.extend(st.debounced.drain().map(|(_, d)| d.token));
        drop(guard);
        for t in &tokens {
            t.cancel();
        }
        // Their tokens are cancelled: each body only tells its waiter.
        for job in dropped {
            (job.body)();
        }
        self.general_cv.notify_all();
        self.reserved_cv.notify_all();
        self.blocking_cv.notify_all();
        self.timer_cv.notify_all();
        let mut st = self.state.lock();
        while st.threads_alive > 0 {
            if self.idle_cv.wait_until(&mut st, deadline).timed_out() {
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
        self.state.lock().blocking_threads
    }

    #[cfg(test)]
    pub(super) fn threads_alive(&self) -> usize {
        self.state.lock().threads_alive
    }
}

/// Decrements `threads_alive` when a scheduler thread's loop returns (or unwinds).
struct AliveGuard(Arc<Inner>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        let mut st: MutexGuard<'_, State> = self.0.state.lock();
        st.threads_alive = st.threads_alive.saturating_sub(1);
        drop(st);
        self.0.idle_cv.notify_all();
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
