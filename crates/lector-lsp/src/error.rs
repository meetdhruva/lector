/// Errors from the LSP client.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// The server did not answer in time.
    #[error("language server request `{0}` timed out")]
    Timeout(String),
    /// The server process exited or its pipes closed.
    #[error("language server exited")]
    ServerExited,
    /// The server answered with a JSON-RPC error.
    #[error("language server error {code}: {message}")]
    Server {
        /// JSON-RPC error code.
        code: i64,
        /// Message from the server.
        message: String,
    },
    /// Spawning or talking to the process failed.
    #[error("language server I/O error: {0}")]
    Io(String),
    /// A response could not be understood.
    #[error("language server protocol error: {0}")]
    Protocol(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, LspError>;
