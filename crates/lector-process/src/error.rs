use std::time::Duration;

/// Errors of process supervision.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// The program could not be found (not on `PATH`, or the path does not exist).
    #[error("`{program}` was not found")]
    NotFound {
        /// The program as given.
        program: String,
    },
    /// Spawning failed for another reason (permissions, resource limits, ...).
    #[error("cannot start `{program}`: {source}")]
    Spawn {
        /// The program as given.
        program: String,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// The run was cancelled through its [`CancelToken`](crate::CancelToken); the process
    /// tree was terminated.
    #[error("cancelled")]
    Cancelled,
    /// The run exceeded its timeout; the process tree was terminated.
    #[error("timed out after {}s", .after.as_secs_f32())]
    TimedOut {
        /// The configured timeout.
        after: Duration,
    },
    /// The [`Supervisor`](crate::Supervisor) is shutting down and starts no new processes.
    #[error("process supervisor is shutting down")]
    ShuttingDown,
    /// I/O error while talking to a running process.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias for this crate.
pub type Result<T, E = ProcessError> = std::result::Result<T, E>;
