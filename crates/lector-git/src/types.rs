//! Plain data types returned by [`crate::Repo`].

use std::ops::Range;

use crate::Oid;

/// Information about `HEAD`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HeadInfo {
    /// Short name of the checked-out branch (`main`), `None` when detached.
    pub branch: Option<String>,
    /// Commit `HEAD` points to, `None` for an unborn branch (no commits yet).
    pub oid: Option<Oid>,
    /// `true` when `HEAD` points directly to a commit.
    pub detached: bool,
    /// Short name of the upstream tracking branch (`origin/main`), if configured and present.
    pub upstream: Option<String>,
    /// Commits on `HEAD` that are not on the upstream.
    pub ahead: u32,
    /// Commits on the upstream that are not on `HEAD`.
    pub behind: u32,
}

/// The kind of change of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ChangeKind {
    /// The path is new.
    Added,
    /// The content (or executable bit) changed.
    Modified,
    /// The path was removed.
    Deleted,
    /// The path was moved (see `renamed_from` / `old_path`).
    Renamed,
    /// The path was copied from another one.
    Copied,
    /// The kind of entry changed (file ↔ symlink ↔ submodule).
    TypeChange,
}

/// One path in `git status`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StatusEntry {
    /// Repository-relative path, `/`-separated.
    pub path: String,
    /// Change between `HEAD` and the index.
    pub staged: Option<ChangeKind>,
    /// Change between the index and the working tree (tracked files only).
    pub unstaged: Option<ChangeKind>,
    /// The file is not tracked (and not ignored).
    pub untracked: bool,
    /// The path has unresolved merge conflicts (`staged`/`unstaged` are `None` then).
    pub conflicted: bool,
    /// For staged renames/copies: the source path.
    pub renamed_from: Option<String>,
}

/// Which pair of trees/states to compare. See [`crate::Repo::changes`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Changeset {
    /// Index → working tree, tracked files only (`git diff`).
    Unstaged,
    /// `HEAD` → index (`git diff --cached`).
    Staged,
    /// Untracked files (each an addition of a [`Side::Workdir`]).
    Untracked,
    /// `HEAD` → working tree including untracked files.
    WorkingTree,
    /// First parent → commit (root commits are compared to the empty tree).
    Commit(Oid),
    /// Tree of `from` → tree of `to`.
    Range {
        /// Old revision.
        from: String,
        /// New revision.
        to: String,
    },
    /// `merge-base(base, branch)` → `branch` (what a pull request would show).
    BranchVsBase {
        /// The branch under review.
        branch: String,
        /// The target branch.
        base: String,
    },
}

/// Where to read one side of a [`FileChange`] from. Load with [`crate::Repo::load`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Side {
    /// The side does not exist (addition/deletion).
    None,
    /// An object in the object database.
    Blob(Oid),
    /// The stage-0 index entry at this path (stage 2, "ours", for conflicts).
    Index(String),
    /// The file in the working tree at this repository-relative path.
    Workdir(String),
}

/// A changed file between two states.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileChange {
    /// Path on the old side (`None` for additions).
    pub old_path: Option<String>,
    /// Path on the new side (`None` for deletions).
    pub new_path: Option<String>,
    /// What happened to the path.
    pub kind: ChangeKind,
    /// How to load the old content.
    pub old: Side,
    /// How to load the new content.
    pub new: Side,
    /// Whether either side is binary. [`crate::Repo::changes`] leaves this `None`
    /// to stay fast; fill it with [`crate::Repo::detect_binary`].
    pub binary: Option<bool>,
}

impl FileChange {
    /// The path to show for this change: the new path, or the old one for deletions.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

/// Parameters for [`crate::Repo::log`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LogQuery {
    /// Starting revision. `None` = `HEAD`. `a..b` ranges are supported.
    pub rev: Option<String>,
    /// Start from all branches, remote branches, tags and `HEAD` (not the stash).
    pub all_refs: bool,
    /// Only commits touching this file or directory (repository-relative).
    pub path: Option<String>,
    /// Case-insensitive substring of `Name <email>`.
    pub author: Option<String>,
    /// Case-insensitive substring of the full commit message.
    pub message: Option<String>,
    /// Number of matching commits to skip.
    pub skip: usize,
    /// Maximum number of commits to return; `0` means unlimited.
    pub limit: usize,
}

/// A person and a point in time.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Signature {
    /// Name.
    pub name: String,
    /// Email address (without angle brackets).
    pub email: String,
    /// Unix seconds.
    pub time: i64,
    /// Timezone offset from UTC in minutes.
    pub offset_minutes: i32,
}

/// Kind of a [`RefLabel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RefKind {
    /// `HEAD` (always labelled, attached or detached).
    Head,
    /// A branch under `refs/heads/`.
    LocalBranch,
    /// A branch under `refs/remotes/` (`origin/main`).
    RemoteBranch,
    /// A tag under `refs/tags/` (annotated tags are peeled to their commit).
    Tag,
}

/// A reference pointing at a commit, for decorating log rows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RefLabel {
    /// Short name (`main`, `origin/main`, `v1.0`, `HEAD`).
    pub name: String,
    /// What kind of reference.
    pub kind: RefKind,
}

/// One commit in a log.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CommitInfo {
    /// The commit id.
    pub oid: Oid,
    /// Parent ids, first parent first.
    pub parents: Vec<Oid>,
    /// Author.
    pub author: Signature,
    /// Committer.
    pub committer: Signature,
    /// First paragraph of the message, joined into one line.
    pub summary: String,
    /// References pointing at this commit.
    pub refs: Vec<RefLabel>,
}

/// A commit with its full message and changes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CommitDetail {
    /// Summary information.
    pub info: CommitInfo,
    /// The full message.
    pub message: String,
    /// Changes against the first parent.
    pub changes: Vec<FileChange>,
}

/// A local or remote-tracking branch.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Branch {
    /// Short name (`main`, or `origin/main` for remote branches).
    pub name: String,
    /// `true` for remote-tracking branches.
    pub remote: bool,
    /// `true` for the checked-out branch.
    pub head: bool,
    /// Tip commit.
    pub oid: Oid,
    /// Short name of the upstream (local branches only).
    pub upstream: Option<String>,
    /// Committer time of the tip (unix seconds).
    pub time: i64,
}

/// A tag.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tag {
    /// Short name (`v1.0`).
    pub name: String,
    /// Target, peeled through annotated tags (usually a commit).
    pub oid: Oid,
}

/// A stash entry (`stash@{index}`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Stash {
    /// `0` is the most recent stash.
    pub index: usize,
    /// The reflog message (`WIP on main: …` / `On main: …`).
    pub message: String,
    /// The stash commit.
    pub oid: Oid,
}

/// One run of consecutive lines attributed to the same commit.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlameHunk {
    /// The commit that last changed these lines (zero for uncommitted lines).
    pub oid: Oid,
    /// 0-based line range in the blamed file.
    pub lines: Range<u32>,
    /// 0-based first line in the original file of that commit.
    pub orig_start: u32,
    /// Path of the file in that commit (differs after renames).
    pub orig_path: String,
    /// Author name.
    pub author: String,
    /// Author time (unix seconds).
    pub time: i64,
    /// Commit summary.
    pub summary: String,
    /// The lines are not committed yet (working tree changes).
    pub uncommitted: bool,
}

/// A configured remote.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Remote {
    /// Remote name (`origin`).
    pub name: String,
    /// Fetch URL as configured.
    pub url: String,
}
