//! Language Server Protocol client for Lector: navigation (hover,
//! definition, references, symbols), diagnostics and the editing features
//! (formatting, code actions, rename, server-initiated edits). Edits are
//! returned as plain data; this crate never writes files.
//!
//! No async runtime is required: requests return runtime-agnostic futures
//! (built on `futures` channels) and all I/O runs on plain threads.
#![forbid(unsafe_code)]

mod client;
mod config;
mod convert;
mod error;
pub mod install;
mod manager;
mod types;

pub use config::{
    builtin_servers, read_only_settings, which, writes_to_project, ServerConfig, WRITES_TO_PROJECT,
};
pub use error::{LspError, Result};
pub use lector_text::PointUtf16;
pub use manager::{ApplyEditRequest, LspManager, LspOptions, ServerState, ServerStatus};
pub use types::{
    CodeAction, Command, Diagnostic, DiagnosticSeverity, DiagnosticTag, DocumentDiagnostics,
    DocumentState, DocumentSymbol, FormattingOptions, HoverInfo, Location, PrepareRename,
    ServerCapabilities, SymbolKind, TextEdit, WorkspaceEdit, WorkspaceSymbol,
};
