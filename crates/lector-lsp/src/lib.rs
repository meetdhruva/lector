//! Read-only Language Server Protocol client for Lector.
//!
//! No async runtime is required: requests return runtime-agnostic futures
//! (built on `futures` channels) and all I/O runs on plain threads.
#![forbid(unsafe_code)]

mod client;
mod config;
mod convert;
mod error;
mod manager;
mod types;

pub use config::{builtin_servers, which, ServerConfig};
pub use error::{LspError, Result};
pub use lector_text::PointUtf16;
pub use manager::{LspManager, LspOptions, ServerState, ServerStatus};
pub use types::{DocumentSymbol, HoverInfo, Location, SymbolKind, WorkspaceSymbol};
