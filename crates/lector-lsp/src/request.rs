//! Cancellation and outcome types shared by the interactive requests
//! (completion, completion resolve, signature help).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct TokenInner {
    cancelled: AtomicBool,
    hooks: Mutex<Vec<Hook>>,
}

/// A cancellation flag for one in-flight request. Clones share the flag;
/// cancelling sends `$/cancelRequest` to the server and makes the request's
/// future resolve with [`RequestOutcome::Cancelled`] without waiting for the
/// server's answer. (Same shape as `lector_search::CancelToken`, but this
/// crate cannot depend on `lector-search`.)
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<TokenInner>,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

    /// Requests cancellation (idempotent; visible to every clone).
    pub fn cancel(&self) {
        if self.inner.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        let hooks = std::mem::take(&mut *self.inner.hooks.lock());
        for hook in hooks {
            hook();
        }
    }

    /// Whether [`CancelToken::cancel`] was called on any clone.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Runs `f` once on cancellation (immediately if already cancelled).
    pub(crate) fn on_cancel(&self, f: impl FnOnce() + Send + 'static) {
        {
            let mut hooks = self.inner.hooks.lock();
            // Checked under the lock: `cancel` flips the flag before it takes
            // the hooks, so either we see the flag or `cancel` sees our hook.
            if !self.is_cancelled() {
                hooks.push(Box::new(f));
                return;
            }
        }
        f();
    }
}

/// How an interactive request ended (when the server did not fail).
#[derive(Debug, Clone, PartialEq)]
pub enum RequestOutcome<T> {
    /// The answer, for the document version the request was made at.
    Done(T),
    /// Cancelled by the caller's token or superseded by a newer request of the
    /// same kind for the same document. Nothing to show.
    Cancelled,
    /// The document changed (a newer version was synced, or the server
    /// answered `ContentModified`) while the request was in flight; the
    /// answer was dropped. Re-issue the request if it is still wanted.
    Stale,
    /// No language server for the document, the server is not running, or it
    /// does not support the feature.
    Unavailable,
}

impl<T> RequestOutcome<T> {
    /// The answer, if there is one.
    pub fn done(self) -> Option<T> {
        match self {
            RequestOutcome::Done(v) => Some(v),
            _ => None,
        }
    }

    /// Whether this is [`RequestOutcome::Done`].
    pub fn is_done(&self) -> bool {
        matches!(self, RequestOutcome::Done(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_run_once_on_cancel_and_immediately_afterwards() {
        let token = CancelToken::new();
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = count.clone();
        token.on_cancel(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 0);
        token.clone().cancel();
        token.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let c = count.clone();
        token.on_cancel(move || {
            c.fetch_add(10, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 11);
        assert!(token.is_cancelled());
    }
}
