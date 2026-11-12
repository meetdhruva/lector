//! Agent sessions for Lector (v0.5): drive a coding agent over the Agent Client Protocol
//! (ACP: JSON-RPC 2.0 over the agent's stdio) and review each turn's changes as a
//! changeset. See `docs/ARCHITECTURE.md`, "lector-agent".
//!
//! * [`AgentSession`]: spawn the agent, `initialize`, `session/new`, `session/prompt` with
//!   text and context, stream `session/update`, `session/cancel`; answer the agent's
//!   `fs/read_text_file` (open documents, unsaved text included), `fs/write_text_file`
//!   (through the document store, journaled, only inside the project) and
//!   `session/request_permission` (surfaced to the user).
//! * Every turn is bracketed by working-tree checkpoints in the shadow repository; its
//!   changes are a [`lector_git::Changeset::Snapshots`]; "Undo turn" restores the
//!   checkpoint through the activity journal.
//! * [`AgentManager`] keeps the sessions of a process and provides line provenance.
//! * [`commands::register`] adds the `agent.*` commands.
//!
//! UI-free and runtime-agnostic: plain threads, no async runtime. The protocol is
//! implemented directly on `serde_json` (the `agent-client-protocol` crate needs an async
//! executor and pulls in a large dependency tree).
#![forbid(unsafe_code)]

pub mod commands;
pub mod config;
pub mod context;
mod error;
pub mod inline_edit;
pub mod manager;
pub mod model;
pub mod paths;
pub mod rpc;
pub mod session;
pub mod summary;

pub use config::{available_configs, AgentConfig};
pub use error::AgentError;
pub use manager::AgentManager;
pub use model::*;
pub use session::{
    AgentEvent, AgentSession, SessionId, SessionOptions, SessionStatus, Subscription,
};
