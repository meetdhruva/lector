//! The error type shared by every part of `arch-check`.

use std::fmt;
use std::io;

/// Anything that stops `arch-check` from producing a verdict (exit code 2).
#[derive(Debug)]
pub enum ArchError {
    /// Reading or writing a file failed.
    Io {
        /// The file or directory involved.
        path: String,
        /// The underlying error.
        source: io::Error,
    },
    /// Running or parsing `cargo metadata` failed.
    Cargo(String),
    /// A checked-in configuration file (`arch/*`) is malformed.
    Config(String),
    /// Bad command line.
    Usage(String),
}

impl ArchError {
    /// Wrap an I/O error with the path it concerns.
    pub fn io(path: impl Into<String>, source: io::Error) -> Self {
        ArchError::Io {
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for ArchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchError::Io { path, source } => write!(f, "{path}: {source}"),
            ArchError::Cargo(m) => write!(f, "cargo metadata: {m}"),
            ArchError::Config(m) => write!(f, "{m}"),
            ArchError::Usage(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ArchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArchError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, ArchError>;
