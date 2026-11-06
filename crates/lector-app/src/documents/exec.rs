//! The executor seam of the document store.
//!
//! Every derived query and background step (reparse, highlights, outline,
//! gutter, blame mapping, LSP sync) runs through a [`DocExecutor`]. The
//! production implementation is the shared [`Scheduler`]; the GPUI front-end
//! plugs in its own deterministic executor in tests so `run_until_parked` and
//! fake clocks keep working.

use std::time::Duration;

use crate::scheduler::{CancelToken, JobKey, Priority, Scheduler};

/// A unit of background work. It receives a [`CancelToken`] that is set when
/// a newer job with the same key superseded it.
pub type DocJob = Box<dyn FnOnce(&CancelToken) + Send + 'static>;

/// Where the document store runs its background work.
pub trait DocExecutor: Send + Sync + 'static {
    /// Runs `job` soon. With a `key`, an older unfinished job with the same
    /// key is cancelled ("latest wins").
    fn spawn(&self, prio: Priority, key: Option<JobKey>, job: DocJob);
    /// Runs `job` once, `delay` after the last call with the same `key`
    /// (earlier jobs for that key are dropped).
    fn debounce(&self, key: JobKey, delay: Duration, prio: Priority, job: DocJob);
}

impl DocExecutor for Scheduler {
    fn spawn(&self, prio: Priority, key: Option<JobKey>, job: DocJob) {
        // The handle is dropped: results are delivered through the store.
        let _ = Scheduler::spawn(self, prio, key, job);
    }

    fn debounce(&self, key: JobKey, delay: Duration, prio: Priority, job: DocJob) {
        if delay.is_zero() {
            let _ = Scheduler::spawn(self, prio, Some(key), job);
        } else {
            Scheduler::debounce(self, key, delay, prio, job);
        }
    }
}

/// Runs every job immediately on the calling thread (debounces too, without
/// waiting). For tests and one-shot CLI use; never use it from a UI thread.
#[derive(Debug, Default, Clone, Copy)]
pub struct InlineExecutor;

impl DocExecutor for InlineExecutor {
    fn spawn(&self, _prio: Priority, _key: Option<JobKey>, job: DocJob) {
        job(&CancelToken::new());
    }

    fn debounce(&self, _key: JobKey, _delay: Duration, _prio: Priority, job: DocJob) {
        job(&CancelToken::new());
    }
}
