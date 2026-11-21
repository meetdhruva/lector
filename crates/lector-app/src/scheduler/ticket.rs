//! A job's cancellation state: its [`CancelToken`] plus whether it started (ADR 0010 item 4).
//!
//! Who may stop a job differs by kind:
//! * a read is skipped when its token was cancelled before it started (a replaced job, a closed
//!   subject, a dropped task, an explicit cancel), and a running one sees the token;
//! * a [write](super::JobKind::is_write) runs once queued even if somebody cancelled its token
//!   (its result is then dropped); only the scheduler can keep it from starting, explicitly
//!   ([`Ticket::revoke`]: `cancel_key`) or at quit ([`Ticket::abort`]: refused while quitting,
//!   past the network grace, shutdown). Revoking a write that already started does nothing: a
//!   write either never starts or runs to completion and reports its result.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use lector_base::CancelToken;

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const REVOKED: u8 = 2;

/// Shared by the scheduler's registry (under the job's key), the job's body and its handle.
#[derive(Clone)]
pub(super) struct Ticket {
    token: CancelToken,
    state: Arc<AtomicU8>,
    write: bool,
}

impl Ticket {
    /// A queued job's ticket; `write` from [`JobKind::runs_once_queued`](super::JobKind).
    pub(super) fn new(write: bool) -> Ticket {
        Ticket {
            token: CancelToken::new(),
            state: Arc::new(AtomicU8::new(QUEUED)),
            write,
        }
    }

    /// The token handed to the job.
    pub(super) fn token(&self) -> &CancelToken {
        &self.token
    }

    /// Called by the body as it starts: whether it may run. A read whose token is cancelled
    /// may not; a write may unless the scheduler revoked it.
    pub(super) fn start(&self) -> bool {
        let started = self
            .state
            .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        started && (self.write || !self.token.is_cancelled())
    }

    /// An explicit cancellation (`cancel_key`, a replacement, a closed subject). Returns whether
    /// it took effect: always for a read (its token is cancelled), for a write only if it had
    /// not started. Cancels the token, which runs its `on_cancel` hooks: never call it with the
    /// scheduler lock held.
    pub(super) fn revoke(&self) -> bool {
        super::pool::assert_unlocked("cancelling a job's token");
        let prevented = self
            .state
            .compare_exchange(QUEUED, REVOKED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if prevented || !self.write {
            self.token.cancel();
            true
        } else {
            false
        }
    }

    /// Cancellation at quit: a queued job never starts, a running one sees its token (network
    /// writes past their grace, shutdown). Same lock rule as [`Ticket::revoke`].
    pub(super) fn abort(&self) {
        super::pool::assert_unlocked("cancelling a job's token");
        let _ = self
            .state
            .compare_exchange(QUEUED, REVOKED, Ordering::AcqRel, Ordering::Acquire);
        self.token.cancel();
    }

    /// Whether a revoke would keep it from running ([`Ticket::revoke`] takes effect).
    pub(super) fn revocable(&self) -> bool {
        !self.write || self.state.load(Ordering::Acquire) == QUEUED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_with_a_cancelled_token_does_not_start() {
        let t = Ticket::new(false);
        t.token().cancel();
        assert!(!t.start());
    }

    #[test]
    fn a_write_with_a_cancelled_token_starts() {
        let t = Ticket::new(true);
        t.token().cancel();
        assert!(t.start());
    }

    #[test]
    fn a_revoked_write_does_not_start_and_a_started_one_cannot_be_revoked() {
        let queued = Ticket::new(true);
        assert!(queued.revoke());
        assert!(!queued.start());
        let running = Ticket::new(true);
        assert!(running.start());
        assert!(!running.revoke());
        assert!(!running.token().is_cancelled());
        running.abort();
        assert!(running.token().is_cancelled());
    }
}
