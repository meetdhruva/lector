//! The one GPUI bridge for background work (ADR 0003, decision 2).
//!
//! UI code (this crate, `lector-ui-git`, `lector-ui-terminal`, `lector-ui-agent`) starts
//! background work only through [`background`]: the closure runs on the `lector-app`
//! [`Scheduler`] under a [`JobKey`] and a [`Priority`], and the result is delivered on the main
//! thread by the returned [`BackgroundTask`] (a future wrapping a GPUI [`Task`]):
//!
//! * **Dropping the task cancels the job at once** (like dropping a GPUI task cancels its
//!   future, but synchronously: GPUI only drops a cancelled task's future when its runnable is
//!   next polled): the job's [`CancelToken`] is set, a job that has not started never runs, a
//!   running one sees the token. Keep the task in a field to keep the work alive;
//!   [`BackgroundTask::detach`] lets the work finish without anyone waiting.
//! * **A result is delivered only if it is still wanted**: the task resolves to
//!   `Err(JobError::Cancelled)` when the job was replaced by a newer one with the same key, its
//!   subject was cancelled ([`cancel_subject`]) or the task was asked to stop, even if the job
//!   had already produced its value: the token is checked again on the main thread, where
//!   replacing and cancelling happen.
//! * A job that panicked resolves to `Err(JobError::Panicked(..))` instead of taking the
//!   worker down.
//!
//! Under GPUI's test dispatcher the bridge uses a [`Scheduler::with_runner`] that runs jobs on
//! GPUI's test executor (detected only when lector-ui is built with `cfg(test)` or its
//! `test-support` feature: every crate whose GPUI tests start background work must enable it in
//! its dev-dependencies, and its test setup should assert [`is_test_dispatcher`]), so `run_until_parked` and the fake clock still drive everything
//! deterministically; keys, subjects and cancellation are the scheduler's in both cases.
//! Timers (`background_executor().timer(..)`) and pure UI animation stay on GPUI.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use gpui::{App, AsyncApp, AsyncWindowContext, BackgroundExecutor, Context, EntityId};
use gpui::{ForegroundExecutor, Subscription, Task};
use lector_base::thread::TaskRunner;
use lector_base::CancelToken;

pub use lector_app::scheduler::{JobError, JobKey, JobKind, Priority, Scheduler, Subject};

/// The executors of a GPUI context; implemented for every context UI code holds.
pub trait BridgeContext {
    /// GPUI's background executor (only its dispatcher is used, to detect the test dispatcher).
    fn background_executor_handle(&self) -> BackgroundExecutor;
    /// The main-thread executor that delivers results.
    fn foreground_executor_handle(&self) -> ForegroundExecutor;
}

impl BridgeContext for App {
    fn background_executor_handle(&self) -> BackgroundExecutor {
        self.background_executor().clone()
    }
    fn foreground_executor_handle(&self) -> ForegroundExecutor {
        self.foreground_executor().clone()
    }
}

impl<V> BridgeContext for Context<'_, V> {
    fn background_executor_handle(&self) -> BackgroundExecutor {
        self.background_executor().clone()
    }
    fn foreground_executor_handle(&self) -> ForegroundExecutor {
        self.foreground_executor().clone()
    }
}

impl BridgeContext for AsyncApp {
    fn background_executor_handle(&self) -> BackgroundExecutor {
        self.background_executor().clone()
    }
    fn foreground_executor_handle(&self) -> ForegroundExecutor {
        self.foreground_executor().clone()
    }
}

impl BridgeContext for AsyncWindowContext {
    fn background_executor_handle(&self) -> BackgroundExecutor {
        self.background_executor().clone()
    }
    fn foreground_executor_handle(&self) -> ForegroundExecutor {
        self.foreground_executor().clone()
    }
}

/// The subject of one view or model: its jobs belong to it (see [`Subject::View`]).
pub fn view(id: EntityId) -> Subject {
    Subject::View(id.as_u64())
}

/// A key for `kind` work of the view or model `id`.
pub fn view_key(kind: JobKind, id: EntityId) -> JobKey {
    JobKey::new(kind, view(id))
}

/// Runs `f` on the scheduler (lane by `key.kind`, `prio` as allowed for the kind) and
/// delivers its result on the main thread. Dropping the returned task cancels the job; a
/// replaced or cancelled job's result is never delivered (see the module docs).
pub fn background<C, T>(
    cx: &C,
    key: JobKey,
    prio: Priority,
    f: impl FnOnce(&CancelToken) -> T + Send + 'static,
) -> BackgroundTask<T>
where
    C: BridgeContext + ?Sized,
    T: Send + 'static,
{
    let handle = scheduler_for(&cx.background_executor_handle()).spawn(key, prio, f);
    let token = handle.token().clone();
    let task = cx.foreground_executor_handle().spawn(Delivery {
        token: token.clone(),
        outcome: Box::pin(handle.outcome()),
    });
    BackgroundTask {
        token,
        task: Box::pin(task),
        done: false,
    }
}

/// The main-thread end of a job started with [`background`]: await it for the result.
/// Dropping it before it resolved cancels the job immediately.
#[must_use = "dropping a BackgroundTask cancels its job; keep it or detach() it"]
pub struct BackgroundTask<T> {
    token: CancelToken,
    /// Delivers the outcome on the main thread. Dropping it does not cancel the job (a dropped
    /// handle never does); this task's own `Drop` does, unless detached.
    task: Pin<Box<Task<Result<T, JobError>>>>,
    done: bool,
}

impl<T> BackgroundTask<T> {
    /// Lets the job run to completion with nobody waiting for its result.
    pub fn detach(mut self) {
        self.done = true;
    }
}

impl<T> Future for BackgroundTask<T> {
    type Output = Result<T, JobError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        match self.task.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(outcome) => {
                self.done = true;
                Poll::Ready(outcome)
            }
        }
    }
}

impl<T> Drop for BackgroundTask<T> {
    fn drop(&mut self) {
        if !self.done {
            self.token.cancel();
        }
    }
}

impl<T> Unpin for BackgroundTask<T> {}

/// Runs `f` on the scheduler without delivering anything: fire and forget (it still is
/// replaced by a newer job with the same key and cancelled with its subject).
pub fn detach<C>(cx: &C, key: JobKey, prio: Priority, f: impl FnOnce(&CancelToken) + Send + 'static)
where
    C: BridgeContext + ?Sized,
{
    let _ = scheduler_for(&cx.background_executor_handle()).spawn(key, prio, f);
}

/// Cancels every job of `subject` (a view, document or repository that closes). Returns how
/// many jobs were cancelled.
pub fn cancel_subject<C: BridgeContext + ?Sized>(cx: &C, subject: &Subject) -> usize {
    scheduler_for(&cx.background_executor_handle()).cancel_subject(subject)
}

/// Cancels every job of the view or model `cx` belongs to ([`view`]) when it is released, detached
/// ones included. Keep the subscription for as long as the view lives (closing a window
/// releases its workspace).
pub fn cancel_on_release<V: 'static>(cx: &mut Context<V>) -> Subscription {
    let subject = view(cx.entity_id());
    cx.on_release(move |_, cx| {
        cancel_subject(cx, &subject);
    })
}

/// A [`TaskRunner`] for a crate below `lector-app` (which does not schedule): each task runs
/// as a job with `key` at `prio`.
pub fn task_runner<C: BridgeContext + ?Sized>(cx: &C, key: JobKey, prio: Priority) -> TaskRunner {
    scheduler_for(&cx.background_executor_handle()).task_runner(key, prio)
}

/// The scheduler UI work runs on (GPUI's test executor under the test dispatcher).
pub fn scheduler<C: BridgeContext + ?Sized>(cx: &C) -> Scheduler {
    scheduler_for(&cx.background_executor_handle())
}

/// Whether `cx` runs under GPUI's test dispatcher.
pub fn is_test_dispatcher<C: BridgeContext + ?Sized>(cx: &C) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if cx
        .background_executor_handle()
        .dispatcher
        .as_test()
        .is_some()
    {
        return true;
    }
    let _ = cx;
    false
}

fn scheduler_for(bg: &BackgroundExecutor) -> Scheduler {
    #[cfg(any(test, feature = "test-support"))]
    if bg.dispatcher.as_test().is_some() {
        return test_scheduler(bg);
    }
    let _ = bg;
    Scheduler::global()
}

/// One runner scheduler per GPUI test dispatcher (per test), so keys and subjects are shared by
/// everything the test's app starts.
#[cfg(any(test, feature = "test-support"))]
fn test_scheduler(bg: &BackgroundExecutor) -> Scheduler {
    use std::cell::RefCell;
    use std::sync::Arc;
    thread_local! {
        static CURRENT: RefCell<Option<(usize, Scheduler)>> = const { RefCell::new(None) };
    }
    // The cached runner holds the executor, so the dispatcher cannot be freed (and its address
    // reused) while it is cached.
    let id = Arc::as_ptr(&bg.dispatcher) as *const () as usize;
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        if let Some((cached, scheduler)) = current.as_ref() {
            if *cached == id {
                return scheduler.clone();
            }
        }
        let exec = bg.clone();
        let scheduler = Scheduler::with_runner(Arc::new(move |delay, body| {
            let timer = exec.clone();
            exec.spawn(async move {
                if !delay.is_zero() {
                    timer.timer(delay).await;
                }
                body();
            })
            .detach();
        }));
        *current = Some((id, scheduler.clone()));
        scheduler
    })
}

/// Resolves to the job's outcome on the main thread, turning a value produced after the job
/// was replaced or cancelled into `Cancelled`.
struct Delivery<T> {
    token: CancelToken,
    outcome: Pin<Box<dyn Future<Output = Result<T, JobError>> + Send>>,
}

impl<T> Future for Delivery<T> {
    type Output = Result<T, JobError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let outcome = match self.outcome.as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(outcome) => outcome,
        };
        // Replaced or cancelled after the value was produced: the value is stale.
        Poll::Ready(match outcome {
            Ok(_) if self.token.is_cancelled() => Err(JobError::Cancelled),
            other => other,
        })
    }
}

impl<T> Unpin for Delivery<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn key(kind: JobKind, n: u64) -> JobKey {
        JobKey::new(kind, Subject::View(n))
    }

    #[gpui::test]
    fn a_newer_job_with_the_same_key_replaces_the_older_one(cx: &mut TestAppContext) {
        let ran = Arc::new(AtomicUsize::new(0));
        let (first, second) = cx.update(|cx| {
            let r1 = ran.clone();
            let r2 = ran.clone();
            let first = background(cx, key(JobKind::GitLog, 1), Priority::Visible, move |_| {
                r1.fetch_add(1, Ordering::SeqCst);
                1
            });
            let second = background(cx, key(JobKind::GitLog, 1), Priority::Visible, move |_| {
                r2.fetch_add(1, Ordering::SeqCst);
                2
            });
            (first, second)
        });
        cx.run_until_parked();
        assert_eq!(cx.executor().block_test(first), Err(JobError::Cancelled));
        assert_eq!(cx.executor().block_test(second), Ok(2));
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the replaced job never ran");
    }

    #[gpui::test]
    fn cancelling_a_subject_cancels_its_jobs_only(cx: &mut TestAppContext) {
        let (mine, other, write) = cx.update(|cx| {
            let mine = background(cx, key(JobKind::GitChangeset, 7), Priority::Visible, |_| 1);
            let other = background(cx, key(JobKind::GitChangeset, 8), Priority::Visible, |_| 2);
            let write = background(cx, key(JobKind::GitWrite, 7), Priority::Visible, |_| 3);
            assert_eq!(cancel_subject(cx, &Subject::View(7)), 1);
            (mine, other, write)
        });
        cx.run_until_parked();
        let fg = cx.executor();
        assert_eq!(fg.block_test(mine), Err(JobError::Cancelled));
        assert_eq!(fg.block_test(other), Ok(2));
        assert_eq!(
            fg.block_test(write),
            Ok(3),
            "a write survives its view closing"
        );
    }

    #[gpui::test]
    fn dropping_the_task_cancels_the_job(cx: &mut TestAppContext) {
        let ran = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| {
            let r = ran.clone();
            let task = background(cx, key(JobKind::GitLog, 1), Priority::Visible, move |_| {
                r.fetch_add(1, Ordering::SeqCst);
            });
            drop(task);
        });
        cx.run_until_parked();
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    struct Probe(Subscription);

    #[gpui::test]
    fn releasing_a_view_cancels_its_jobs(cx: &mut TestAppContext) {
        let ran = Arc::new(AtomicUsize::new(0));
        let probe = cx.update(|cx| cx.new(|cx| Probe(cancel_on_release(cx))));
        cx.update(|cx| {
            let r = ran.clone();
            let key = view_key(JobKind::Reindex, probe.entity_id());
            detach(cx, key, Priority::Background, move |_| {
                r.fetch_add(1, Ordering::SeqCst);
            });
            let _keep = &probe.read(cx).0;
        });
        // Released when the update's effects flush, before the test executor runs the job.
        cx.update(move |_| drop(probe));
        cx.run_until_parked();
        assert_eq!(ran.load(Ordering::SeqCst), 0, "the view was released first");
    }

    #[gpui::test]
    fn a_detached_task_lets_the_job_finish(cx: &mut TestAppContext) {
        let ran = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| {
            let r = ran.clone();
            background(cx, key(JobKind::GitLog, 1), Priority::Visible, move |_| {
                r.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        });
        cx.run_until_parked();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[gpui::test]
    fn a_panicking_job_resolves_to_an_error(cx: &mut TestAppContext) {
        let task = cx.update(|cx| {
            background(cx, key(JobKind::GitLog, 1), Priority::Visible, |_| -> u8 {
                panic!("expected test panic")
            })
        });
        cx.run_until_parked();
        assert!(matches!(
            cx.executor().block_test(task),
            Err(JobError::Panicked(_))
        ));
    }

    #[gpui::test]
    fn under_the_test_dispatcher_work_runs_on_the_test_executor(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(is_test_dispatcher(cx));
            assert!(!scheduler(cx).is_shut_down());
        });
        let task = cx.update(|cx| background(cx, key(JobKind::GitLog, 1), Priority::Idle, |_| 5));
        cx.run_until_parked();
        assert_eq!(cx.executor().block_test(task), Ok(5));
    }

    /// On the real scheduler (as in the app): job A has finished and sent its value, then,
    /// before the main thread delivers it, a newer job with the same key is submitted (the
    /// detached definitions lookup case): A's consumer sees `Cancelled`, never the value.
    #[test]
    fn a_result_replaced_after_it_finished_is_not_delivered() {
        let s = Scheduler::new(1);
        let a = s.spawn(key(JobKind::LspDefinitions, 1), Priority::Visible, |_| {
            "old"
        });
        wait_finished(&a);
        let b = s.spawn(key(JobKind::LspDefinitions, 1), Priority::Visible, |_| {
            "new"
        });
        assert_eq!(deliver(a), Err(JobError::Cancelled));
        assert_eq!(deliver(b), Ok("new"));
        assert_eq!(s.live_jobs(), 0);
    }

    #[test]
    fn a_result_whose_subject_closed_after_it_finished_is_not_delivered() {
        let s = Scheduler::new(1);
        let a = s.spawn(key(JobKind::GitLog, 3), Priority::Visible, |_| 1);
        wait_finished(&a);
        assert_eq!(s.cancel_subject(&Subject::View(3)), 1);
        assert_eq!(deliver(a), Err(JobError::Cancelled));
        assert_eq!(s.live_jobs(), 0);
    }

    #[test]
    fn delivered_dropped_and_detached_results_leave_no_registration() {
        let s = Scheduler::new(1);
        let a = s.spawn(key(JobKind::GitLog, 1), Priority::Visible, |_| 1);
        assert_eq!(deliver(a), Ok(1));
        let b = s.spawn(key(JobKind::GitLog, 2), Priority::Visible, |_| 2);
        wait_finished(&b);
        // A delivery that is dropped before it ran (the view went away).
        drop(Delivery {
            token: b.token().clone(),
            outcome: Box::pin(b.outcome()),
        });
        // A detached job: nobody holds its handle.
        drop(s.spawn(key(JobKind::Reindex, 3), Priority::Visible, |_| ()));
        assert!(s.wait_idle(Duration::from_secs(5)));
        assert_eq!(s.live_jobs(), 0);
    }

    fn wait_finished<T>(h: &lector_app::scheduler::JobHandle<T>) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !h.is_finished() {
            assert!(std::time::Instant::now() < end);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// What the bridge's foreground delivery resolves to.
    fn deliver<T: Send + 'static>(h: lector_app::scheduler::JobHandle<T>) -> Result<T, JobError> {
        futures::executor::block_on(Delivery {
            token: h.token().clone(),
            outcome: Box::pin(h.outcome()),
        })
    }
}
