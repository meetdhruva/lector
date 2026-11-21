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
//! Network and branch operations (v0.7: [`sync`], fetch / pull / push / branches / stash)
//! also use the CLI, run under `lector-process` (cancellable, whole process tree killed).
//!
//! Semantics worth knowing: `Changeset::Unstaged` compares the index with the working tree
//! (`Side::Index` → `Side::Workdir`), `Staged` a `HEAD` blob with the index, `WorkingTree` a
//! `HEAD` blob with the working tree (following staged renames). `Side::Index` of a conflicted
//! path loads stage 2 ("ours"). The `HEAD` commit carries a `RefKind::Head` label (attached or
//! detached), and [`graph`] edges run from a row's centre to the next row's, with lanes
//! compacting when they end. Calls that need a working tree return [`Error::Bare`] in a bare
//! repository. A changed repository config is picked up on the next call (a cheap stat).
//!
//! The write paths (the `write` module, the shadow repository) run git through `std::process` so that
//! tty-based signing pinentries behave as in a terminal; blame, `log -L` / `--follow` and the
//! [`sync`] operations run under `lector_process::Supervisor`. Writes run git with
//! `--literal-pathspecs --no-pager -c core.quotepath=off`, `GIT_TERMINAL_PROMPT=0` and
//! `GIT_EDITOR=true`, passing paths in chunks of 500; absolute paths and paths containing `..`
//! are refused.
//!
//! Among Lector crates this one depends on `lector-base` and `lector-process` only.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod branch;
mod changes;
mod cli;
mod conflict;
mod error;
mod graph;
mod log;
mod oid;
mod permalink;
mod pipes;
mod proc;
mod repo;
pub mod shadow;
mod status;
pub mod sync;
mod types;
pub mod write;

pub use conflict::{
    merged_name_from_message, strip_message_comments, ConflictEntry, ConflictKind, ConflictStage,
    OperationKind, RepoOperation,
};
pub use error::{Error, Result};
pub use graph::{graph, EdgeKind, GraphEdge, GraphRow};
pub use oid::Oid;
pub use permalink::{build_permalink, detect_forge, parse_remote_url, Forge, RemoteWeb};
pub use repo::{is_binary, Repo};
pub use shadow::{ShadowRepo, ShadowSnapshot};
pub use types::{
    BlameHunk, Branch, ChangeKind, Changeset, CommitDetail, CommitInfo, FileChange, HeadInfo,
    LogQuery, RefKind, RefLabel, Remote, Side, Signature, Stash, StatusEntry, Tag,
};
pub use write::{CommitOptions, PatchTarget};

pub use branch::{
    BranchSwitch, DeletedBranch, StashApplied, StashApplyOptions, StashPushOptions,
    APPLY_WITHOUT_INDEX,
};
pub use proc::SyncOptions;
pub use sync::{
    FetchOptions, FetchOutcome, Progress, PullMode, PullOutcome, PullStatus, PushOptions,
    PushOutcome, PushRefStatus, PushedRef, RefUpdate, SyncError, SyncErrorKind, SyncEvent,
    UpstreamStatus,
};
