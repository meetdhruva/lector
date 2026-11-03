//! Error type for the crate.

/// Errors produced while parsing or preparing grammars.
#[derive(Debug, thiserror::Error)]
pub enum SyntaxError {
    /// The text is larger than [`crate::MAX_PARSE_BYTES`].
    #[error("file too large to parse ({len} bytes, limit {max})")]
    TooLarge {
        /// Size of the text.
        len: usize,
        /// The enforced limit.
        max: usize,
    },
    /// The grammar could not be loaded into a parser (ABI mismatch).
    #[error("grammar for {0} could not be loaded: {1}")]
    Grammar(String, String),
    /// The parser returned no tree (cancelled or failed).
    #[error("parse of {0} did not complete")]
    ParseFailed(String),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, SyntaxError>;
