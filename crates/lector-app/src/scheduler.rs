//! One process-wide job scheduler: a fixed worker pool, four priority queues,
//! keyed "latest wins" cancellation and debouncing.
//!
//! * Jobs run on `num_cpus - 1` worker threads (at least 2); the highest
//!   non-empty [`Priority`] queue is served first, FIFO within a queue.
//! * A job spawned with a [`JobKey`] cancels any older, unfinished job with the
//!   same key ("latest wins"). Cancelled jobs that have not started are dropped
//!   without running; running jobs see [`CancelToken::is_cancelled`] and their
//!   result is discarded.
//! * [`Scheduler::debounce`] coalesces calls with the same key: only the last
//!   closure runs, `delay` after the last call.
//! * [`JobHandle`] exposes the result as a runtime-agnostic future (a
//!   `futures` oneshot), so GPUI, tests and the CLI can all await it.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::FutureExt;
use parking_lot::{Condvar, Mutex};

pub use lector_search::CancelToken;

/// Scheduling class of a job; earlier variants run first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Directly answers a user gesture (keystroke, click); must feel instant.
    Interactive = 0,
    /// Work for something currently on screen (highlights, visible diff).
    Visible = 1,
    /// Work the user will likely need soon (status refresh, indexing).
    Background = 2,
    /// Anything that can wait (cache trimming, prefetch).
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
}

/// Identifies a logical job: a newer job with the same key cancels older ones.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobKey(pub String);

impl JobKey {
    /// Creates a key.
    pub fn new(key: impl Into<String>) -> Self {
        JobKey(key.into())
    }
}

impl From<&str> for JobKey {
    fn from(s: &str) -> Self {
        JobKey(s.to_string())
    }
}

impl From<String> for JobKey {
    fn from(s: String) -> Self {
        JobKey(s)
    }
}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Job {
    token: CancelToken,
    key: Option<(String, u64)>,
    run: Task,
}

struct Debounced {
    deadline: Instant,
    prio: Priority,
    f: Box<dyn FnOnce(&CancelToken) + Send + 'static>,
}

#[derive(Default)]
struct State {
    queues: [VecDeque<Job>; 4],
    /// Newest job per key: (job id, its token).
    keyed: HashMap<String, (u64, CancelToken)>,
    debounced: HashMap<String, Debounced>,
    running: usize,
    shutdown: bool,
}

struct Inner {
    state: Mutex<State>,
    work: Condvar,
    timer: Condvar,
    idle: Condvar,
    next_id: AtomicU64,
    workers: usize,
}

/// Shuts the pool down when the last [`Scheduler`] clone is dropped.
struct Owner(Arc<Inner>);

impl Drop for Owner {
    fn drop(&mut self) {
        let mut st = self.0.state.lock();
        st.shutdown = true;
        for q in &mut st.queues {
            for job in q.drain(..) {
                job.token.cancel();
            }
        }
        st.debounced.clear();
        drop(st);
        self.0.work.notify_all();
        self.0.timer.notify_all();
    }
}

/// Handle to the worker pool. Cheap to clone.
#[derive(Clone)]
pub struct Scheduler {
    owner: Arc<Owner>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("workers", &self.owner.0.workers)
            .finish()
    }
}

/// Default number of workers: `num_cpus - 1`, at least 2.
pub fn default_workers() -> usize {
    num_cpus::get().saturating_sub(1).max(2)
}

impl Scheduler {
    /// The process-wide scheduler (created on first use with [`default_workers`]).
    pub fn global() -> Scheduler {
        static GLOBAL: OnceLock<Scheduler> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Scheduler::new(default_workers()))
            .clone()
    }

    /// A new pool with `workers` threads (at least 1). The threads stop when
    /// the last clone of the returned scheduler is dropped.
    pub fn new(workers: usize) -> Scheduler {
        let workers = workers.max(1);
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            work: Condvar::new(),
            timer: Condvar::new(),
            idle: Condvar::new(),
            next_id: AtomicU64::new(1),
            workers,
        });
        for i in 0..workers {
            let inner = inner.clone();
            let spawned = thread::Builder::new()
                .name(format!("lector-worker-{i}"))
                .spawn(move || worker_loop(&inner));
            if let Err(err) = spawned {
                tracing::error!("cannot start scheduler worker: {err}");
            }
        }
        {
            let inner = inner.clone();
            let spawned = thread::Builder::new()
                .name("lector-debounce".into())
                .spawn(move || timer_loop(&inner));
            if let Err(err) = spawned {
                tracing::error!("cannot start scheduler timer: {err}");
            }
        }
        Scheduler {
            owner: Arc::new(Owner(inner)),
        }
    }

    fn inner(&self) -> &Arc<Inner> {
        &self.owner.0
    }

    /// Number of worker threads.
    pub fn workers(&self) -> usize {
        self.inner().workers
    }

    /// Runs `f` on a worker. With a `key`, any older unfinished job with the
    /// same key is cancelled (its handle resolves to `None`).
    pub fn spawn<T: Send + 'static>(
        &self,
        prio: Priority,
        key: Option<JobKey>,
        f: impl FnOnce(&CancelToken) -> T + Send + 'static,
    ) -> JobHandle<T> {
        let token = CancelToken::new();
        let (tx, rx) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let run: Task = {
            let token = token.clone();
            let finished = finished.clone();
            Box::new(move || {
                if !token.is_cancelled() {
                    match catch_unwind(AssertUnwindSafe(|| f(&token))) {
                        Ok(value) if !token.is_cancelled() => {
                            let _ = tx.send(value);
                        }
                        Ok(_) => {}
                        Err(_) => tracing::error!("scheduler job panicked"),
                    }
                }
                finished.store(true, Ordering::Release);
            })
        };
        self.submit(prio, key.map(|k| k.0), token.clone(), run);
        JobHandle {
            token,
            finished,
            rx: Some(rx),
        }
    }

    /// Runs `f` once, `delay` after the *last* call with the same `key`
    /// (earlier closures for that key are dropped without running). When it
    /// runs it is a keyed job, so it also cancels a still-running older run.
    pub fn debounce(
        &self,
        key: JobKey,
        delay: Duration,
        prio: Priority,
        f: impl FnOnce(&CancelToken) + Send + 'static,
    ) {
        let inner = self.inner();
        let mut st = inner.state.lock();
        if st.shutdown {
            return;
        }
        st.debounced.insert(
            key.0,
            Debounced {
                deadline: Instant::now() + delay,
                prio,
                f: Box::new(f),
            },
        );
        drop(st);
        inner.timer.notify_all();
    }

    /// Cancels the newest job with `key` and any pending debounce for it.
    pub fn cancel_key(&self, key: &JobKey) {
        let mut st = self.inner().state.lock();
        st.debounced.remove(&key.0);
        if let Some((_, token)) = st.keyed.remove(&key.0) {
            token.cancel();
        }
    }

    /// Jobs queued but not started (cancelled ones included until a worker drops them).
    pub fn pending(&self) -> usize {
        let st = self.inner().state.lock();
        st.queues.iter().map(VecDeque::len).sum::<usize>() + st.debounced.len()
    }

    /// Blocks until no job is queued, debounced or running, or `timeout` passes.
    /// Returns true when idle. Meant for tests and orderly shutdown.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let inner = self.inner();
        let mut st = inner.state.lock();
        loop {
            let busy = st.running > 0
                || st.queues.iter().any(|q| !q.is_empty())
                || !st.debounced.is_empty();
            if !busy {
                return true;
            }
            if inner.idle.wait_until(&mut st, deadline).timed_out() {
                return false;
            }
        }
    }

    fn submit(&self, prio: Priority, key: Option<String>, token: CancelToken, run: Task) {
        submit(self.inner(), prio, key, token, run);
    }
}

fn submit(inner: &Inner, prio: Priority, key: Option<String>, token: CancelToken, run: Task) {
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let mut st = inner.state.lock();
    if st.shutdown {
        token.cancel();
        drop(st);
        // Run the wrapper so the handle sees `finished` (it skips `f`).
        run();
        return;
    }
    let key = key.map(|k| {
        if let Some((_, old)) = st.keyed.insert(k.clone(), (id, token.clone())) {
            old.cancel();
        }
        (k, id)
    });
    st.queues[prio as usize].push_back(Job { token, key, run });
    drop(st);
    inner.work.notify_one();
}

fn worker_loop(inner: &Inner) {
    loop {
        let job = {
            let mut st = inner.state.lock();
            loop {
                if let Some(job) = st.queues.iter_mut().find_map(VecDeque::pop_front) {
                    st.running += 1;
                    break job;
                }
                if st.shutdown {
                    return;
                }
                inner.work.wait(&mut st);
            }
        };
        let Job { token: _, key, run } = job;
        run();
        let mut st = inner.state.lock();
        st.running -= 1;
        if let Some((k, id)) = key {
            if st.keyed.get(&k).is_some_and(|(newest, _)| *newest == id) {
                st.keyed.remove(&k);
            }
        }
        let idle =
            st.running == 0 && st.queues.iter().all(VecDeque::is_empty) && st.debounced.is_empty();
        drop(st);
        if idle {
            inner.idle.notify_all();
        }
    }
}

fn timer_loop(inner: &Inner) {
    let mut st = inner.state.lock();
    loop {
        if st.shutdown {
            return;
        }
        let now = Instant::now();
        let due: Vec<String> = st
            .debounced
            .iter()
            .filter(|(_, d)| d.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        if !due.is_empty() {
            let jobs: Vec<(String, Debounced)> = due
                .into_iter()
                .filter_map(|k| st.debounced.remove(&k).map(|d| (k, d)))
                .collect();
            drop(st);
            for (key, d) in jobs {
                let token = CancelToken::new();
                let f = d.f;
                let run: Task = {
                    let token = token.clone();
                    Box::new(move || {
                        if !token.is_cancelled()
                            && catch_unwind(AssertUnwindSafe(|| f(&token))).is_err()
                        {
                            tracing::error!("debounced job panicked");
                        }
                    })
                };
                submit(inner, d.prio, Some(key), token, run);
            }
            st = inner.state.lock();
            continue;
        }
        match st.debounced.values().map(|d| d.deadline).min() {
            Some(deadline) => {
                inner.timer.wait_until(&mut st, deadline);
            }
            None => inner.timer.wait(&mut st),
        }
    }
}

/// Result handle of a spawned job. Dropping it does **not** cancel the job.
pub struct JobHandle<T> {
    token: CancelToken,
    finished: Arc<AtomicBool>,
    rx: Option<oneshot::Receiver<T>>,
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
    /// Requests cancellation; the handle then resolves to `None`.
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
            Ok(Some(v)) => {
                self.rx = None;
                Some(v)
            }
            Ok(None) => None,
            Err(_) => {
                self.rx = None;
                None
            }
        }
    }
}

impl<T: Send + 'static> JobHandle<T> {
    /// Resolves to the result, or `None` if the job was cancelled or panicked.
    pub fn into_future(mut self) -> impl Future<Output = Option<T>> + Send + 'static {
        let rx = self.rx.take();
        async move {
            match rx {
                Some(rx) => rx.map(Result::ok).await,
                None => None,
            }
        }
    }

    /// Blocks the calling thread until the job resolves (headless callers, tests).
    /// Never call this from a scheduler worker on a job queued behind it.
    pub fn wait(self) -> Option<T> {
        futures::executor::block_on(self.into_future())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Blocks every worker until `release` is sent, so queued jobs pile up.
    fn block_workers(s: &Scheduler) -> (Vec<mpsc::Sender<()>>, Vec<JobHandle<()>>) {
        let mut senders = Vec::new();
        let mut handles = Vec::new();
        let (started_tx, started_rx) = mpsc::channel();
        for _ in 0..s.workers() {
            let (tx, rx) = mpsc::channel::<()>();
            let started = started_tx.clone();
            handles.push(s.spawn(Priority::Interactive, None, move |_| {
                let _ = started.send(());
                let _ = rx.recv();
            }));
            senders.push(tx);
        }
        for _ in 0..s.workers() {
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        (senders, handles)
    }

    #[test]
    fn runs_and_returns() {
        let s = Scheduler::new(2);
        let h = s.spawn(Priority::Visible, None, |_| 21 * 2);
        assert_eq!(h.wait(), Some(42));
    }

    #[test]
    fn priority_order() {
        let s = Scheduler::new(1);
        let (release, _h) = block_workers(&s);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for (i, p) in [
            Priority::Idle,
            Priority::Background,
            Priority::Visible,
            Priority::Interactive,
            Priority::Idle,
            Priority::Interactive,
        ]
        .into_iter()
        .enumerate()
        {
            let order = order.clone();
            handles.push(s.spawn(p, None, move |_| order.lock().push((p, i))));
        }
        for r in release {
            r.send(()).unwrap();
        }
        for h in handles {
            assert!(h.wait().is_some());
        }
        let got = order.lock().clone();
        assert_eq!(
            got,
            vec![
                (Priority::Interactive, 3),
                (Priority::Interactive, 5),
                (Priority::Visible, 2),
                (Priority::Background, 1),
                (Priority::Idle, 0),
                (Priority::Idle, 4),
            ]
        );
    }

    #[test]
    fn latest_wins_cancels_older() {
        let s = Scheduler::new(2);
        let (release, _h) = block_workers(&s);
        let ran = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for i in 0..5u64 {
            let ran = ran.clone();
            handles.push(
                s.spawn(Priority::Visible, Some(JobKey::new("k")), move |_| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    i
                }),
            );
        }
        for r in release {
            r.send(()).unwrap();
        }
        let results: Vec<_> = handles.into_iter().map(JobHandle::wait).collect();
        assert_eq!(results, vec![None, None, None, None, Some(4)]);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn running_job_sees_cancellation() {
        let s = Scheduler::new(2);
        let (tx, rx) = mpsc::channel();
        let first = s.spawn(Priority::Visible, Some("k".into()), move |tok| {
            tx.send(()).unwrap();
            while !tok.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            1
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = s.spawn(Priority::Visible, Some("k".into()), |_| 2);
        assert_eq!(first.wait(), None);
        assert_eq!(second.wait(), Some(2));
    }

    #[test]
    fn explicit_cancel_and_try_take() {
        let s = Scheduler::new(1);
        let (release, _h) = block_workers(&s);
        let h = s.spawn(Priority::Idle, None, |_| 1);
        h.cancel();
        let mut h2 = s.spawn(Priority::Idle, None, |_| 2);
        assert_eq!(h2.try_take(), None);
        for r in release {
            r.send(()).unwrap();
        }
        assert_eq!(h.wait(), None);
        assert!(s.wait_idle(Duration::from_secs(5)));
        assert!(h2.is_finished());
        assert_eq!(h2.try_take(), Some(2));
        assert_eq!(h2.try_take(), None);
    }

    #[test]
    fn debounce_coalesces() {
        let s = Scheduler::new(2);
        let runs = Arc::new(Mutex::new(Vec::new()));
        for i in 0..10 {
            let runs = runs.clone();
            s.debounce(
                JobKey::new("d"),
                Duration::from_millis(60),
                Priority::Background,
                move |_| runs.lock().push(i),
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(s.wait_idle(Duration::from_secs(5)));
        assert_eq!(*runs.lock(), vec![9]);
        // A later call after the quiet period runs again.
        let r = runs.clone();
        s.debounce(
            JobKey::new("d"),
            Duration::from_millis(10),
            Priority::Background,
            move |_| r.lock().push(100),
        );
        assert!(s.wait_idle(Duration::from_secs(5)));
        assert_eq!(*runs.lock(), vec![9, 100]);
    }

    #[test]
    fn debounce_waits_for_quiet_period() {
        let s = Scheduler::new(2);
        let start = Instant::now();
        let (tx, rx) = mpsc::channel();
        s.debounce(
            JobKey::new("q"),
            Duration::from_millis(80),
            Priority::Visible,
            move |_| tx.send(Instant::now()).unwrap(),
        );
        let at = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(at.duration_since(start) >= Duration::from_millis(80));
    }

    #[test]
    fn no_lost_or_duplicated_jobs_under_contention() {
        let s = Scheduler::new(4);
        let counts = Arc::new(Mutex::new(HashMap::<u64, u32>::new()));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let s = s.clone();
                let counts = counts.clone();
                thread::spawn(move || {
                    let mut handles = Vec::new();
                    for i in 0..250u64 {
                        let id = t * 1000 + i;
                        let counts = counts.clone();
                        let prio = Priority::ALL[(id % 4) as usize];
                        handles.push(s.spawn(prio, None, move |_| {
                            *counts.lock().entry(id).or_default() += 1;
                            id
                        }));
                    }
                    handles.into_iter().map(|h| h.wait()).collect::<Vec<_>>()
                })
            })
            .collect();
        let mut total = 0;
        for (t, th) in threads.into_iter().enumerate() {
            let results = th.join().unwrap();
            for (i, r) in results.into_iter().enumerate() {
                assert_eq!(r, Some(t as u64 * 1000 + i as u64));
                total += 1;
            }
        }
        assert_eq!(total, 2000);
        let counts = counts.lock();
        assert_eq!(counts.len(), 2000);
        assert!(counts.values().all(|&c| c == 1));
    }

    #[test]
    fn keyed_contention_runs_newest_at_most_once_each() {
        let s = Scheduler::new(3);
        let ran = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for i in 0..500u64 {
            let ran = ran.clone();
            let key = JobKey::new(format!("k{}", i % 7));
            handles.push(s.spawn(Priority::Visible, Some(key), move |_| {
                ran.fetch_add(1, Ordering::SeqCst);
                i
            }));
        }
        let results: Vec<_> = handles.into_iter().map(JobHandle::wait).collect();
        // The newest job of each key always completes.
        for k in 0..7u64 {
            let last = (0..500u64).rev().find(|i| i % 7 == k).unwrap();
            assert_eq!(results[last as usize], Some(last));
        }
        let delivered = results.iter().filter(|r| r.is_some()).count() as u64;
        assert!(delivered <= ran.load(Ordering::SeqCst));
    }

    #[test]
    fn panicking_job_does_not_kill_worker() {
        let s = Scheduler::new(1);
        let h = s.spawn(Priority::Visible, None, |_| -> u32 { panic!("boom") });
        assert_eq!(h.wait(), None);
        assert_eq!(s.spawn(Priority::Visible, None, |_| 7).wait(), Some(7));
    }

    #[test]
    fn default_workers_at_least_two() {
        assert!(default_workers() >= 2);
    }
}
