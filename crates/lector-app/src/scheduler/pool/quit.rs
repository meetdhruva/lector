//! Quitting: cancel everything that is not a write, drain the writes within their bounds, then
//! stop the threads (orchestrated by `lector_app::shutdown`).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lector_base::thread::ThreadKind;

use super::{assert_unlocked, Active, Debounced, Inner, Job, Role, Stop};
use crate::scheduler::key::{AtQuit, JobKind, Subject};
use crate::scheduler::ticket::Ticket;

/// A write quit gave up on: still queued or running at the drain bound, or a network write
/// past its grace. Enough for the next start to tell the user what may not have happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedWrite {
    /// The job's kind.
    pub kind: JobKind,
    /// What it belonged to: for a git write, the repository.
    pub subject: Subject,
    /// What the job did, when its caller said ("git.commit").
    pub what: Option<String>,
    /// `true`: it was running (it may still complete after the process exits, e.g. a commit
    /// whose hook is still running); `false`: it never started.
    pub running: bool,
}

impl AbandonedWrite {
    fn queued(job: &Job) -> AbandonedWrite {
        AbandonedWrite {
            kind: job.key.kind,
            subject: job.key.subject.clone(),
            what: job.what.as_deref().map(str::to_owned),
            running: false,
        }
    }

    fn running(active: &Active) -> AbandonedWrite {
        AbandonedWrite {
            kind: active.kind,
            subject: active.subject.clone(),
            what: active.what.as_deref().map(str::to_owned),
            running: true,
        }
    }
}

impl std::fmt::Display for AbandonedWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(what) = &self.what {
            write!(f, " `{what}`")?;
        }
        match &self.subject {
            Subject::Repo(p) => write!(f, " in repository {}", p.display())?,
            Subject::Path(p) => write!(f, " for {}", p.display())?,
            other => write!(f, " of {other:?}")?,
        }
        let state = if self.running {
            "still running"
        } else {
            "never started"
        };
        let bound = if self.kind.at_quit() == AtQuit::DrainBriefly {
            "network grace"
        } else {
            "drain bound"
        };
        write!(f, " ({state} at the {bound})")
    }
}

/// What [`Inner::drain`] could not finish within its bounds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Writes abandoned (one entry per job): still queued or running at the bound, or network
    /// writes past their grace.
    pub abandoned: Vec<AbandonedWrite>,
}

impl Inner {
    /// The first step of quitting: from now on only writes are accepted. Every other job is
    /// cancelled (queued ones dropped, their waiters told), debounces dropped, and a drain
    /// worker started so queued writes run even if every worker is busy. Returns how many jobs
    /// were cancelled.
    pub(in crate::scheduler) fn begin_quit(self: &Arc<Self>) -> usize {
        let mut st = self.lock();
        if st.quitting || st.shutdown {
            return 0;
        }
        st.quitting = true;
        let mut dropped: Vec<Job> = Vec::new();
        // Serial-lane jobs are all writes (a unit test in `key` keeps it so): kept.
        for q in st.all_queues() {
            let (cancel, keep): (VecDeque<Job>, VecDeque<Job>) =
                q.drain(..).partition(|j| !j.key.kind.is_write());
            *q = keep;
            dropped.extend(cancel);
        }
        let debounced: Vec<Debounced> = st.debounced.drain().map(|(_, d)| d).collect();
        drop(st);
        let cancelled = self.cancel_where(|k| !k.kind.is_write(), Stop::Abort);
        for d in &debounced {
            d.ticket.abort();
        }
        for job in dropped {
            self.run(job);
        }
        let dropped_debounces = debounced.len();
        drop(debounced);
        if self.runner.is_none() {
            let me = self.clone();
            self.start_thread(
                ThreadKind::SchedulerWorker,
                "quit-drain".into(),
                move || me.compute_loop(Role::Drain),
            );
        }
        self.drain_cv.notify_all();
        cancelled + dropped_debounces
    }

    /// Waits until every write ([`AtQuit::Drain`] / [`AtQuit::DrainBriefly`]) queued or running
    /// has finished, at most until `deadline`; [`AtQuit::DrainBriefly`] jobs (network writes)
    /// only until `brief_deadline`, after which they are cancelled. Call after
    /// [`Inner::begin_quit`]. Reports what was left.
    pub(in crate::scheduler) fn drain(
        self: &Arc<Self>,
        deadline: Instant,
        brief_deadline: Instant,
    ) -> DrainReport {
        let mut report = DrainReport::default();
        let mut given_up: HashSet<u64> = HashSet::new();
        let mut st = self.lock();
        loop {
            let now = Instant::now();
            if now >= brief_deadline {
                // Network writes past their grace: cancel and stop waiting for them. None runs
                // on the serial lane (a unit test in `key`), so its waiting jobs stay.
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
                let running: Vec<(u64, Active)> = st
                    .active
                    .iter()
                    .filter(|(id, a)| {
                        a.kind.at_quit() == AtQuit::DrainBriefly && !given_up.contains(id)
                    })
                    .map(|(id, a)| (*id, a.clone()))
                    .collect();
                drop(st);
                for job in dropped {
                    report.abandoned.push(AbandonedWrite::queued(&job));
                    self.abort_id(&job.key, job.id);
                    self.run(job);
                }
                if !running.is_empty() {
                    self.cancel_where(|k| k.kind.at_quit() == AtQuit::DrainBriefly, Stop::Abort);
                }
                for (id, active) in running {
                    given_up.insert(id);
                    report.abandoned.push(AbandonedWrite::running(&active));
                }
                st = self.lock();
            }
            let pending_writes = st
                .all_queues()
                .flat_map(|q| q.iter())
                .any(|j| j.key.kind.is_write())
                || !st.serial_waiting.is_empty()
                || st
                    .active
                    .iter()
                    .any(|(id, a)| a.kind.is_write() && !given_up.contains(id));
            if !pending_writes {
                break;
            }
            if now >= deadline {
                let mut left: Vec<AbandonedWrite> = Vec::new();
                for q in st.all_queues() {
                    let writes = q.iter().filter(|j| j.key.kind.is_write());
                    left.extend(writes.map(AbandonedWrite::queued));
                }
                let waiting = st.serial_waiting.values().flat_map(|q| q.iter());
                left.extend(waiting.map(AbandonedWrite::queued));
                left.extend(
                    st.active
                        .iter()
                        .filter(|(id, a)| a.kind.is_write() && !given_up.contains(id))
                        .map(|(_, a)| AbandonedWrite::running(a)),
                );
                report.abandoned.extend(left);
                break;
            }
            let wake = if brief_deadline > now {
                deadline.min(brief_deadline)
            } else {
                deadline
            };
            self.progress_cv.wait_until(&mut st.guard, wake);
        }
        report
    }

    /// Stops the scheduler: no new job runs, every queued job is dropped (its waiter gets
    /// `Cancelled`), every running job is cancelled, then waits up to `timeout` for the
    /// scheduler's threads to exit. Returns whether they all did.
    pub(in crate::scheduler) fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.lock();
        let st = &mut *guard;
        st.shutdown = true;
        let mut dropped: Vec<Job> = Vec::new();
        for q in st.all_queues() {
            dropped.extend(q.drain(..));
        }
        for (_, q) in st.serial_waiting.drain() {
            dropped.extend(q);
        }
        st.serial_busy.clear();
        // Cancel what is queued or running. A job that already finished keeps its ticket and
        // registration until its waiter takes the result (a write that completed during the
        // quit drain still reports success).
        let unfinished: HashSet<u64> = dropped
            .iter()
            .map(|j| j.id)
            .chain(st.active.keys().copied())
            .collect();
        let mut tickets: Vec<Ticket> = Vec::new();
        st.live.retain(|_, entries| {
            entries.retain(|(id, t)| {
                let cancel = unfinished.contains(id);
                if cancel {
                    tickets.push(t.clone());
                }
                !cancel
            });
            !entries.is_empty()
        });
        let debounced: Vec<Debounced> = st.debounced.drain().map(|(_, d)| d).collect();
        drop(guard);
        tickets.extend(debounced.iter().map(|d| d.ticket.clone()));
        for t in &tickets {
            t.abort();
        }
        drop(debounced);
        // Their tickets are aborted: each body only tells its waiter.
        assert_unlocked("dropping queued jobs");
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
        let mut st = self.lock();
        while st.threads_alive > 0 {
            if self
                .progress_cv
                .wait_until(&mut st.guard, deadline)
                .timed_out()
            {
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
}
