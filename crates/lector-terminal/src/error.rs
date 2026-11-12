//! Error type of the crate.

use std::path::PathBuf;

/// Errors returned by `lector-terminal`.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// The pseudo terminal could not be opened.
    #[error("could not open a pseudo terminal: {0}")]
    OpenPty(String),
    /// The shell (or another program) could not be started.
    #[error("could not start `{program}`: {message}")]
    Spawn {
        /// Program that failed to start.
        program: PathBuf,
        /// Reason reported by the platform.
        message: String,
    },
    /// The working directory does not exist or is not a directory.
    #[error("working directory `{0}` does not exist")]
    InvalidCwd(PathBuf),
    /// Resizing the pseudo terminal failed.
    #[error("could not resize the terminal: {0}")]
    Resize(String),
    /// A search pattern could not be compiled.
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
    /// Writing Lector's shell integration files failed.
    #[error("could not write shell integration files to `{path}`: {source}")]
    Integration {
        /// File that could not be written.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A background thread could not be started.
    #[error("could not start a terminal thread: {0}")]
    Thread(#[source] std::io::Error),
}
