//! Headless application core of Lector: the only stateful layer (ADR 0001). `docs/ARCHITECTURE.md`
//! describes how it fits with the other crates and the flows through it.
//!
//! Everything a user (or an agent, the CLI, an MCP client, a test) can do lives here,
//! UI-free. `lector-ui` renders snapshots and dispatches commands.
#![forbid(unsafe_code)]

pub mod commands; // Agent APP
pub mod comments; // Agent FORGE
pub mod content_cache; // content-addressed caches passed to lector-review (ADR 0001)
mod core; // Agent APP
pub mod db; // Agent APP
pub mod documents; // Agent DOCS
pub mod git_service; // Agent APP
pub mod health; // local crash log and session health
pub mod journal; // Agent SAFE
pub mod os; // reveal in file manager, OS preferences: processes for the UI (ADR 0002)
pub mod project; // the open project: files, git status, symbols, navigation (ADR 0001)
pub mod provenance; // Agent REVIEWUX
pub mod review_queue; // Agent REVIEWUX
pub mod review_service; // review data for views, the CLI and agents; the viewed-marks write (ADR 0001)
pub mod scheduler; // Agent APP
pub mod shutdown; // orderly stop of child processes and the scheduler at quit (ADR 0003)
pub mod store; // every piece of Lector's own state, versioned, in one database (ADR 0004)

pub use crate::commands::{
    ActivitySink, Availability, Command, CommandContext, CommandRegistry, CommandSpec, Outcome,
    UiRequest,
};
pub use crate::core::{AppCore, CoreOptions};
pub use crate::db::Db;
pub use crate::documents::{Document, Indentation, LineEnding, LineHighlights};
pub use crate::git_service::{GitService, GitSnapshot, RepoState};
pub use crate::project::{Project, ProjectError};
pub use crate::scheduler::{JobError, JobHandle, JobKey, JobKind, Priority, Scheduler, Subject};
pub use crate::store::{Store, StoreMode};
