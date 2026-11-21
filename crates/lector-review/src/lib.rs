//! Stateless review analysis for Lector (ADR 0001): changeset loading and keys, file diffs,
//! risk facts and scoring, structural diff and conflict parsing.
//!
//! Every function here is a function of its inputs: no globals, no caches that outlive a call,
//! no I/O other than reading through the `lector-git` / `lector-fs` handles passed in, and no
//! writes at all (the `purity` rule of `arch-check` enforces it; ADR 0001/0002).
//! Memoization of derived data (syntax trees, line diffs) is supplied by the caller through
//! [`content::DerivedCache`]; [`content::NoCache`] computes everything on every call.
//! Everything is blocking; the caller owns threading.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod conflicts;
pub mod content;
mod hunks;
pub mod review;
pub mod risk;
pub mod structural;

use std::io;

/// Errors produced by the review analysis.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// File system error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// Git error.
    #[error("git error: {0}")]
    Git(#[from] lector_git::Error),
    /// The caller's [`CancelToken`](lector_base::CancelToken) was set.
    #[error("cancelled")]
    Cancelled,
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl From<lector_base::Cancelled> for Error {
    fn from(_: lector_base::Cancelled) -> Self {
        Error::Cancelled
    }
}

/// `Err(Cancelled)` once `cancel` is set.
pub(crate) fn check_cancel(cancel: &lector_base::CancelToken) -> Result<()> {
    Ok(cancel.check()?)
}

/// `Result` alias for the review analysis.
pub type Result<T> = std::result::Result<T, Error>;

pub use content::{content_hash, ContentId, DerivedCache, NoCache};
