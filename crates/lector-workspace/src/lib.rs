//! UI-free application model for Lector.
//!
//! Everything here is synchronous and meant to be called from background
//! threads; the UI (`lector-ui`) owns threading and presentation.

pub mod document;
pub mod navigation;
pub mod project;
pub mod review;
pub mod symbols;

use std::io;

/// Errors produced by the workspace model.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// File system error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// Git error.
    #[error("git error: {0}")]
    Git(#[from] lector_git::Error),
    /// Syntax (tree-sitter) error.
    #[error("syntax error: {0}")]
    Syntax(#[from] lector_syntax::SyntaxError),
    /// Language server error.
    #[error("language server error: {0}")]
    Lsp(#[from] lector_lsp::LspError),
    /// The project is not inside a git repository.
    #[error("not a git repository")]
    NotARepository,
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// `Result` alias for the workspace model.
pub type Result<T> = std::result::Result<T, Error>;

pub use document::{Document, Indentation, LineEnding, LineHighlights};
pub use navigation::{Location, NavHistory};
pub use project::{FileGitState, FileList, FileStatus, GitStatusMap, Project};
pub use symbols::{Target, TargetSource};
