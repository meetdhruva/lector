//! Cooperative cancellation: a shared flag with child tokens and cancel hooks.

use std::any::Any;
use std::fmt;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

type Hook = Box<dyn FnOnce() + Send>;

/// The error of a cancelled operation. Each crate's error type implements `From<Cancelled>`
/// (or maps it to its own `Cancelled` variant) so [`CancelToken::check`] works with `?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[derive(Default)]
struct State {
    hooks: Vec<Hook>,
    children: Vec<Weak<Inner>>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    state: Mutex<State>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means a panic elsewhere while holding it; the data is a list
        // of hooks and children and stays consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        // The flag is already visible: `on_cancel` and `child` check it under the lock, so each
        // of them either sees it or leaves something here for us to take.
        let (hooks, children) = {
            let mut state = self.lock();
            (
                std::mem::take(&mut state.hooks),
                std::mem::take(&mut state.children),
            )
        };
        // The lock is released: hooks and children may cancel other tokens (or this one) freely.
        let mut first_panic: Option<Box<dyn Any + Send>> = None;
        for child in children.into_iter().filter_map(|c| c.upgrade()) {
            // A panic in a child's hook must not stop the siblings or this token's hooks.
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| child.cancel())) {
                first_panic.get_or_insert(payload);
            }
        }
        for hook in hooks {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(hook)) {
                first_panic.get_or_insert(payload);
            }
        }
        if let Some(payload) = first_panic {
            resume_unwind(payload);
        }
    }
}

/// A cooperative cancellation flag. Clones share the flag.
///
/// * [`cancel`](Self::cancel) is idempotent and visible to every clone and every child.
/// * [`child`](Self::child) makes a token that is cancelled when its parent is and can also be
///   cancelled alone (a request superseded by a newer one cancels itself, not the caller's token).
/// * [`on_cancel`](Self::on_cancel) registers a hook that runs exactly once.
///
/// ```
/// use lector_base::CancelToken;
/// let user = CancelToken::new();
/// let request = user.child();
/// assert!(!request.is_cancelled());
/// user.cancel();
/// assert!(request.is_cancelled());
/// assert!(request.check().is_err());
/// ```
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent; visible to every clone and child.
    ///
    /// Runs the hooks registered with [`on_cancel`](Self::on_cancel) on the calling thread,
    /// after cancelling the children, without holding any internal lock.
    ///
    /// Completing the cancellation comes first: if a hook (of this token or of any descendant)
    /// panics, every other child and hook still runs, the token stays cancelled, and only then
    /// is the **first** panic resumed on the caller (later ones are dropped). Callers on shutdown
    /// paths that must not unwind wrap the call in `catch_unwind`; a repeated `cancel()` is a
    /// no-op and does not panic again.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// True once [`cancel`](Self::cancel) was called on this token, a clone, or a parent.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// `Err(Cancelled)` once cancelled; for `token.check()?` at loop heads.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// A token cancelled when `self` is, and cancellable alone (that never cancels `self`).
    /// A child of a cancelled token starts cancelled.
    ///
    /// The parent holds the child weakly: dropping every handle of a child frees it, so a
    /// long-lived parent does not accumulate dead children.
    pub fn child(&self) -> CancelToken {
        let child = CancelToken::new();
        {
            let mut state = self.inner.lock();
            if !self.is_cancelled() {
                // Prune on growth (amortised O(1)): handles of finished children are gone.
                if state.children.len() == state.children.capacity() {
                    state.children.retain(|c| c.strong_count() > 0);
                }
                state.children.push(Arc::downgrade(&child.inner));
                return child;
            }
        }
        child.inner.cancelled.store(true, Ordering::SeqCst);
        child
    }

    /// Runs `f` once when the token is cancelled, immediately (on the calling thread) if it
    /// already is.
    ///
    /// Threading: `f` runs on the thread that calls [`cancel`](Self::cancel) (on a clone, or on
    /// a parent), so it must be quick and must not block on the thread that cancels. Hooks run
    /// outside the internal lock, so `f` may cancel other tokens (even cyclically) or register
    /// more hooks. Each hook runs at most once; a token dropped without being cancelled drops
    /// its hooks unrun. Hooks of one token run in registration order, after its children.
    pub fn on_cancel(&self, f: impl FnOnce() + Send + 'static) {
        {
            let mut state = self.inner.lock();
            // Checked under the lock: `cancel` sets the flag before it takes the hooks.
            if !self.is_cancelled() {
                state.hooks.push(Box::new(f));
                return;
            }
        }
        f();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::time::Duration;

    fn counter() -> (Arc<AtomicU32>, impl Fn(u32) -> Box<dyn FnOnce() + Send>) {
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        (count, move |n| {
            let c = c.clone();
            Box::new(move || {
                c.fetch_add(n, Ordering::SeqCst);
            })
        })
    }

    #[test]
    fn clones_share_state() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        assert_eq!(b.check(), Ok(()));
        a.cancel();
        assert!(b.is_cancelled());
        assert_eq!(b.check(), Err(Cancelled));
        a.cancel();
        assert!(a.is_cancelled());
        assert_eq!(Cancelled.to_string(), "cancelled");
        assert!(format!("{a:?}").contains("true"));
    }

    #[test]
    fn independent_tokens_are_independent() {
        let (a, b) = (CancelToken::new(), CancelToken::default());
        a.cancel();
        assert!(!b.is_cancelled());
    }

    #[test]
    fn child_follows_parent_but_not_the_other_way() {
        let parent = CancelToken::new();
        let (c1, c2) = (parent.child(), parent.child());
        let grandchild = c1.child();
        c2.cancel();
        assert!(c2.is_cancelled());
        assert!(!parent.is_cancelled() && !c1.is_cancelled() && !grandchild.is_cancelled());
        parent.cancel();
        assert!(c1.is_cancelled() && grandchild.is_cancelled());
    }

    #[test]
    fn child_of_a_cancelled_token_starts_cancelled() {
        let parent = CancelToken::new();
        parent.cancel();
        assert!(parent.child().is_cancelled());
    }

    #[test]
    fn dropped_children_are_not_kept_alive_or_accumulated() {
        let parent = CancelToken::new();
        let weak = {
            let child = parent.child();
            Arc::downgrade(&child.inner)
        };
        assert!(weak.upgrade().is_none());
        for _ in 0..10_000 {
            let _ = parent.child();
        }
        assert!(parent.inner.lock().children.len() < 100);
        parent.cancel();
    }

    #[test]
    fn hook_runs_once_also_when_registered_after_cancel() {
        let token = CancelToken::new();
        let (count, hook) = counter();
        token.on_cancel(hook(1));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        token.clone().cancel();
        token.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        token.on_cancel(hook(10));
        assert_eq!(count.load(Ordering::SeqCst), 11);
        token.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn hook_of_a_child_runs_when_the_parent_is_cancelled() {
        let parent = CancelToken::new();
        let child = parent.child();
        let (count, hook) = counter();
        child.on_cancel(hook(1));
        parent.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn hook_runs_on_the_cancelling_thread_and_cancel_from_threads_races_cleanly() {
        let token = CancelToken::new();
        let (count, hook) = counter();
        token.on_cancel(hook(1));
        let (tx, rx) = mpsc::channel();
        let t = token.clone();
        let handle = std::thread::spawn(move || {
            t.cancel();
            tx.send(std::thread::current().id()).unwrap();
        });
        let ran_on = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
        assert_ne!(ran_on, std::thread::current().id());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Many threads cancel while others register: every hook runs exactly once.
        for _ in 0..50 {
            let token = CancelToken::new();
            let count = Arc::new(AtomicU32::new(0));
            let mut handles = Vec::new();
            for i in 0..8 {
                let (token, count) = (token.clone(), count.clone());
                handles.push(std::thread::spawn(move || {
                    if i % 2 == 0 {
                        token.cancel();
                    } else {
                        let c = count.clone();
                        token.on_cancel(move || {
                            c.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(count.load(Ordering::SeqCst), 4);
        }
    }

    #[test]
    fn a_hook_may_cancel_other_tokens_even_cyclically_without_deadlock() {
        let (a, b) = (CancelToken::new(), CancelToken::new());
        let (a2, b2) = (a.clone(), b.clone());
        a.on_cancel(move || b2.cancel());
        b.on_cancel(move || a2.cancel());
        let (tx, rx) = mpsc::channel();
        let a3 = a.clone();
        std::thread::spawn(move || {
            a3.cancel();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("cancel deadlocked");
        assert!(a.is_cancelled() && b.is_cancelled());
    }

    #[test]
    fn a_hook_may_register_hooks_and_make_children_of_its_own_token() {
        let token = CancelToken::new();
        let (count, hook) = counter();
        let t = token.clone();
        let extra = hook(5);
        token.on_cancel(move || {
            t.on_cancel(extra);
            assert!(t.child().is_cancelled());
        });
        token.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn a_panicking_hook_does_not_stop_the_others_or_poison_the_token() {
        let token = CancelToken::new();
        let (count, hook) = counter();
        token.on_cancel(|| panic!("boom"));
        token.on_cancel(hook(1));
        let t = token.clone();
        assert!(catch_unwind(AssertUnwindSafe(move || t.cancel())).is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(token.is_cancelled());
        token.on_cancel(hook(2));
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_panicking_child_hook_does_not_skip_siblings_or_parent_hooks() {
        let parent = CancelToken::new();
        let (first, second) = (parent.child(), parent.child());
        let (count, hook) = counter();
        first.on_cancel(|| panic!("boom in child"));
        second.on_cancel(hook(1));
        parent.on_cancel(hook(10));
        let p = parent.clone();
        // Policy: everything is cancelled first, then the first panic is resumed.
        assert!(catch_unwind(AssertUnwindSafe(move || p.cancel())).is_err());
        assert!(first.is_cancelled() && second.is_cancelled());
        assert_eq!(count.load(Ordering::SeqCst), 11);
        // A second cancel is a no-op and does not panic again.
        parent.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn a_panic_three_levels_down_still_completes_the_cancellation() {
        let root = CancelToken::new();
        let mid = root.child();
        let (bad, good) = (mid.child(), mid.child());
        let sibling = root.child();
        let (count, hook) = counter();
        bad.on_cancel(|| panic!("deep boom"));
        good.on_cancel(hook(1));
        mid.on_cancel(hook(10));
        sibling.on_cancel(hook(100));
        root.on_cancel(hook(1000));
        let r = root.clone();
        let err = catch_unwind(AssertUnwindSafe(move || r.cancel())).unwrap_err();
        assert_eq!(err.downcast_ref::<&str>(), Some(&"deep boom"));
        assert!([&mid, &bad, &good, &sibling]
            .iter()
            .all(|t| t.is_cancelled()));
        assert_eq!(count.load(Ordering::SeqCst), 1111);
    }

    #[test]
    fn the_first_panic_wins_and_later_ones_are_dropped() {
        let parent = CancelToken::new();
        let child = parent.child();
        child.on_cancel(|| panic!("first"));
        parent.on_cancel(|| panic!("second"));
        let p = parent.clone();
        let err = catch_unwind(AssertUnwindSafe(move || p.cancel())).unwrap_err();
        assert_eq!(err.downcast_ref::<&str>(), Some(&"first"));
    }

    #[test]
    fn token_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<CancelToken>();
    }
}
