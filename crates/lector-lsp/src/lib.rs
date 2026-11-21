//! Language Server Protocol client for Lector: navigation (hover,
//! definition, references, symbols), diagnostics, the editing features
//! (formatting, code actions, rename, server-initiated edits), code completion
//! and signature help (with UI-free session models). Edits are returned as
//! plain data; this crate never writes files.
//!
//! No async runtime is required: requests return runtime-agnostic futures
//! (built on `futures` channels) and all I/O runs on plain threads.
//!
//! Servers: [`builtin_servers`] lists known servers in priority order (the first whose
//! command is on `PATH` serves a language, so pyright before pylsp); the caller applies the
//! user's `lsp.servers.<name>` overrides to the [`ServerConfig`]s. Each server gets reader,
//! writer, stderr and request-timeout owner threads; launches and restarts run on the
//! caller's `TaskRunner` ([`LspOptions`]), inline without one. "No writes into the project":
//! [`read_only_settings`] is sent as `initializationOptions` and answers
//! `workspace/configuration`, clangd runs with `--background-index=false`, and servers in
//! [`WRITES_TO_PROJECT`] are started by `lector-app` only when the user enables them.
//! Server-initiated `workspace/applyEdit` goes to the handler set with
//! [`LspManager::set_apply_edit_handler`] (answered `applied: false` without one); workspace
//! edits with file operations (create, rename, delete) are a protocol error. Diagnostics are
//! stored per document with the LSP version they were published for and the caller's tag of
//! the text with that version.
#![forbid(unsafe_code)]

mod client;
mod completion;
mod config;
mod convert;
mod error;
pub mod install;
mod manager;
mod request;
mod session;
mod signature;
mod snippet;
mod types;

pub use completion::{
    CompletionCapabilities, CompletionItem, CompletionKind, CompletionRequest, CompletionResponse,
    CompletionTrigger, Documentation, InsertFormat, InsertTextMode, ItemEdit, LabelDetails,
    ResolvedCompletion, SignatureHelpCapabilities,
};
pub use config::{
    builtin_servers, read_only_settings, which, writes_to_project, ServerConfig, WRITES_TO_PROJECT,
};
pub use error::{LspError, Result};
pub use lector_text::PointUtf16;
pub use manager::{ApplyEditRequest, LspManager, LspOptions, ServerState, ServerStatus};
pub use request::RequestOutcome;
pub use session::{
    ByteEdit, CloseReason, CommitError, CompletionCommit, CompletionSession, InsertMode, Match,
    OpenReason, SessionOptions, Update, WordChars,
};
pub use signature::{
    ActiveSignature, ParameterInfo, SignatureHelp, SignatureInfo, SignatureRequest,
    SignatureSession, SignatureTrigger,
};
pub use snippet::{Snippet, TabStop};
pub use types::{
    CodeAction, Command, Diagnostic, DiagnosticSeverity, DiagnosticTag, DocumentDiagnostics,
    DocumentState, DocumentSymbol, FormattingOptions, HoverInfo, Location, PrepareRename,
    ServerCapabilities, SymbolKind, TextEdit, WorkspaceEdit, WorkspaceSymbol,
};
