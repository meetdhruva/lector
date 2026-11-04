//! Git access for Lector: gix-backed reads plus CLI-backed writes ([`write`]).
//!
//! [`Repo`] wraps a [gitoxide](https://github.com/GitoxideLabs/gitoxide)
//! repository for all read paths (status, diffs between trees/index/worktree,
//! log, refs, objects). Only blame, line history (`log -L`) and rename-following
//! file history (`log --follow`) shell out to the `git` CLI.
//!
//! Guarantees:
//! * Read paths never write to the repository: no index refresh write-back, no lock
//!   files (`GIT_OPTIONAL_LOCKS=0` for CLI calls), no ref or config changes. Writes
//!   (stage, unstage, discard, apply, commit) live in [`write`], go through the git
//!   CLI and are only called for explicit user actions.
//! * No panics on repository data; all failures are [`Error`]s.
//! * Line numbers are 0-based `u32`; paths are repository-relative with `/`.
//!
//! This crate has no dependency on other Lector crates.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod changes;
mod cli;
mod error;
mod graph;
mod log;
mod oid;
mod permalink;
mod repo;
mod status;
mod types;
pub mod write;

pub use error::{Error, Result};
pub use graph::{graph, EdgeKind, GraphEdge, GraphRow};
pub use oid::Oid;
pub use permalink::{build_permalink, detect_forge, parse_remote_url, Forge, RemoteWeb};
pub use repo::{is_binary, Repo};
pub use types::{
    BlameHunk, Branch, ChangeKind, Changeset, CommitDetail, CommitInfo, FileChange, HeadInfo,
    LogQuery, RefKind, RefLabel, Remote, Side, Signature, Stash, StatusEntry, Tag,
};
pub use write::{CommitOptions, PatchTarget};
