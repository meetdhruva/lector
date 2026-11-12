//! Language Server Protocol client for Lector: navigation (hover,
//! definition, references, symbols), diagnostics, the editing features
//! (formatting, code actions, rename, server-initiated edits), code completion
//! and signature help (with UI-free session models). Edits are returned as
//! plain data; this crate never writes files.
//!
//! No async runtime is required: requests return runtime-agnostic futures
//! (built on `futures` channels) and all I/O runs on plain threads.
#![forbid(unsafe_code)]

mod client;
mod completion;
mod config;
mod convert;
mod error;
mod fuzzy;
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
pub use request::{CancelToken, RequestOutcome};
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
