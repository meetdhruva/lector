//! Errors of agent sessions.

use crate::model::TurnId;
use crate::rpc::RpcError;

/// Failure of an [`crate::AgentSession`] operation.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The agent program could not be started or did not initialize.
    #[error("cannot start {name}: {message}")]
    Start {
        /// Agent name.
        name: String,
        /// Why (includes the end of the agent's stderr when useful).
        message: String,
    },
    /// The agent is working on another turn.
    #[error("the agent is busy with turn {0}")]
    Busy(TurnId),
    /// The agent process is not running (crashed or stopped); restart it.
    #[error("the agent is not running: {0}")]
    NotRunning(String),
    /// No such turn.
    #[error("no turn {0}")]
    NoTurn(TurnId),
    /// The turn cannot be undone (no checkpoint, nothing changed, already undone...).
    #[error("{0}")]
    NotUndoable(String),
    /// Files of the turn changed since it finished; undo again with `force` to overwrite them.
    #[error("changed since the turn: {}", paths.join(", "))]
    UndoConflict {
        /// Repository-relative paths edited since the turn.
        paths: Vec<String>,
    },
    /// A protocol error.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}
