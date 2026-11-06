//! Headless application core of Lector (v0.4+). See `docs/ARCHITECTURE.md`, "lector-app".
//!
//! Everything a user (or an agent, the CLI, an MCP client, a test) can do lives here,
//! UI-free. `lector-ui` renders snapshots and dispatches commands.
#![forbid(unsafe_code)]

pub mod commands; // Agent APP
pub mod comments; // Agent FORGE
mod core; // Agent APP
pub mod db; // Agent APP
pub mod documents; // Agent DOCS
pub mod git_service; // Agent APP
pub mod journal; // Agent SAFE
pub mod provenance; // Agent REVIEWUX
pub mod review_queue; // Agent REVIEWUX
pub mod scheduler; // Agent APP

pub use crate::commands::{
    ActivitySink, Availability, Command, CommandContext, CommandRegistry, CommandSpec, Outcome,
    UiRequest,
};
pub use crate::core::{AppCore, CoreOptions};
pub use crate::db::Db;
pub use crate::git_service::{GitService, GitSnapshot, RepoState};
pub use crate::scheduler::{JobHandle, JobKey, Priority, Scheduler};
