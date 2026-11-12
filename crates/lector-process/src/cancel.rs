use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation flag shared between a caller and a running process.
///
/// Same shape as `lector_search::CancelToken` (an `Arc<AtomicBool>`); this crate has no
/// internal dependencies, so it defines its own. Bridge the two with
/// [`CancelToken::from_flag`] / [`CancelToken::flag`]. Cloning yields a handle to the same flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps an existing shared flag (e.g. another crate's token).
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        CancelToken(flag)
    }

    /// The shared flag.
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }

    /// Requests cancellation; visible to all clones.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// True once [`cancel`](Self::cancel) was called on any clone.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_the_flag() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
        let flag = Arc::new(AtomicBool::new(false));
        let c = CancelToken::from_flag(flag.clone());
        flag.store(true, Ordering::SeqCst);
        assert!(c.is_cancelled());
    }
}
