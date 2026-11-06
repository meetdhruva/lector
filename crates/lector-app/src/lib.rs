//! Headless application core of Lector (v0.4+). See `docs/ARCHITECTURE.md`, "lector-app".
//!
//! Everything a user (or an agent, the CLI, an MCP client, a test) can do lives here,
//! UI-free. `lector-ui` renders snapshots and dispatches commands.
#![forbid(unsafe_code)]

pub mod commands; // Agent APP
pub mod db; // Agent APP
pub mod documents; // Agent DOCS
pub mod git_service; // Agent APP
pub mod journal; // Agent SAFE
pub mod scheduler; // Agent APP
