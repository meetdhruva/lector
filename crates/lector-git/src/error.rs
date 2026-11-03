use std::path::PathBuf;

/// Errors returned by `lector-git`.
///
/// Errors coming from `gix` are flattened into [`Error::Git`] with their full
/// message chain so callers don't need to depend on `gix` themselves.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No git repository was found at or above the given path.
    #[error("not a git repository (or any parent): {}", .0.display())]
    NotARepository(PathBuf),
    /// The operation needs a working tree but the repository is bare.
    #[error("operation requires a working tree, but the repository is bare")]
    Bare,
    /// A revision could not be parsed or resolved.
    #[error("invalid revision '{rev}': {message}")]
    InvalidRevision {
        /// The revision as given by the caller.
        rev: String,
        /// Why it could not be resolved.
        message: String,
    },
    /// The two revisions have no common ancestor.
    #[error("no merge base between '{0}' and '{1}'")]
    NoMergeBase(String, String),
    /// The `git` executable could not be found on `PATH`.
    #[error("the `git` executable was not found on PATH (needed for {0})")]
    GitNotInstalled(&'static str),
    /// A `git` CLI invocation failed.
    #[error("`git {command}` failed ({status}): {stderr}")]
    GitCommand {
        /// The subcommand and arguments (without the repository path).
        command: String,
        /// Exit status description.
        status: String,
        /// Trimmed standard error output.
        stderr: String,
    },
    /// Output of the `git` CLI could not be parsed.
    #[error("could not parse git output: {0}")]
    Parse(String),
    /// I/O error while reading the working tree.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Error reported by gitoxide while reading repository data.
    #[error("git: {0}")]
    Git(String),
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Convert any displayable gix error into [`Error::Git`], keeping the cause chain.
pub(crate) fn git_err<E: std::fmt::Display>(err: E) -> Error {
    Error::Git(format!("{err:#}"))
}

/// Extension to map arbitrary gix results into this crate's [`Result`].
pub(crate) trait ResultExt<T> {
    fn git(self) -> Result<T>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for std::result::Result<T, E> {
    fn git(self) -> Result<T> {
        self.map_err(git_err)
    }
}
