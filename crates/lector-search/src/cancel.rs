use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation flag shared between a caller and background work.
///
/// Cloning yields a handle to the same flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Creates a token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation; visible to all clones.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Returns true once [`cancel`](Self::cancel) was called on any clone.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
