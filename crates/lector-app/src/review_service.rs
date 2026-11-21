//! [`ReviewService`]: the app's entry point to review data (ADR 0001, ADR 0002).
//!
//! `lector-review` is stateless and never writes; everything that touches state or the
//! disk on behalf of a view goes through here:
//!
//! * loading changesets and file diffs, with the app's content cache ([`ContentCache`]) as
//!   the [`DerivedCache`] of the analysis,
//! * the "viewed" marks: read when a changeset is loaded, written by [`ReviewService::set_viewed`]
//!   (same store and file format as before; ADR 0004 moves the storage later),
//! * [`apply`]: applying a hunk patch to the index or working tree (called by the
//!   `git.*_hunk` commands only).
//!
//! All methods are blocking and meant to be called from background work. Each takes a
//! [`CancelToken`]; a cancelled call returns [`lector_review::Error::Cancelled`] (or `None`)
//! without finishing the work, checked before it starts and, for changesets, between files.
//! Results are the plain `lector-review` types, identical to what the loaders return.

pub mod apply;

use std::path::Path;

use lector_config::ReviewStore;
use lector_diff::{DiffOptions, DiffStats};
use lector_git::{Changeset, FileChange, Repo};
use lector_review::review::{self, ChangesetView, FileDiff, ReviewFile};
use lector_review::Result;
use lector_syntax::LanguageRegistry;

pub use lector_search::CancelToken;

use crate::content_cache::ContentCache;

/// Review data for views, the CLI, MCP and agents. Cheap to clone; `Send + Sync`.
///
/// Reach it from the app with [`AppCore::review`](crate::AppCore::review).
#[derive(Clone)]
pub struct ReviewService {
    store: Option<ReviewStore>,
    cache: ContentCache,
}

impl std::fmt::Debug for ReviewService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewService").finish_non_exhaustive()
    }
}

impl ReviewService {
    /// A service over `store` (the viewed marks) and the process-wide [`ContentCache`].
    pub fn new(store: ReviewStore) -> Self {
        Self::with_cache(store, ContentCache::global().clone())
    }

    /// A service over `store` and a specific cache (tests, isolated cores).
    pub fn with_cache(store: ReviewStore, cache: ContentCache) -> Self {
        Self {
            store: Some(store),
            cache,
        }
    }

    /// A service without viewed marks (nothing is viewed, [`set_viewed`](Self::set_viewed)
    /// fails) over the process-wide cache: for callers that only need diffs.
    pub fn without_marks() -> Self {
        Self {
            store: None,
            cache: ContentCache::global().clone(),
        }
    }

    /// The changes of `cs` (a commit, a range between revisions, staged, unstaged, a branch
    /// against its base...) with stats and viewed state.
    pub fn changeset(
        &self,
        repo: &Repo,
        cs: &Changeset,
        opts: &DiffOptions,
        cancel: &CancelToken,
    ) -> Result<ChangesetView> {
        let marks = self.marks(repo.workdir());
        review::load_changeset_cancellable(repo, cs, &marks, opts, cancel)
    }

    /// The changes from revision `rev` to the working tree (including untracked files); the
    /// view's changeset is `Range { from: rev, to: WORKDIR_REV }`.
    pub fn changeset_vs_workdir(
        &self,
        repo: &Repo,
        rev: &str,
        opts: &DiffOptions,
        cancel: &CancelToken,
    ) -> Result<ChangesetView> {
        let marks = self.marks(repo.workdir());
        review::load_vs_workdir_cancellable(repo, rev, &marks, opts, cancel)
    }

    /// The base branch to compare `branch` against by default: its upstream, else the first of
    /// `main`, `master`, `origin/main`, `origin/master`, `develop` that exists. `None` when
    /// there is none or the call was cancelled.
    pub fn default_base(&self, repo: &Repo, branch: &str, cancel: &CancelToken) -> Option<String> {
        if cancel.is_cancelled() {
            return None;
        }
        review::default_base(repo, branch)
    }

    /// Loads and diffs one change (both sides parsed and diffed through the content cache).
    pub fn file_diff(
        &self,
        repo: &Repo,
        languages: &LanguageRegistry,
        change: &FileChange,
        opts: &DiffOptions,
        cancel: &CancelToken,
    ) -> Result<FileDiff> {
        check(cancel)?;
        review::load_file_diff_with(repo, languages, change, opts, &self.cache)
    }

    /// Diff of the working copy of `rel_path` against revision `rev` ("Compare with...").
    pub fn file_vs_revision(
        &self,
        repo: &Repo,
        languages: &LanguageRegistry,
        rel_path: &str,
        rev: &str,
        opts: &DiffOptions,
        cancel: &CancelToken,
    ) -> Result<FileDiff> {
        check(cancel)?;
        review::file_vs_revision_with(repo, languages, rel_path, rev, opts, &self.cache)
    }

    /// Stats (`None`: binary or too large), binary flag and content hash of one change.
    pub fn file_summary(
        &self,
        repo: &Repo,
        change: &FileChange,
        opts: &DiffOptions,
        cancel: &CancelToken,
    ) -> Result<(Option<DiffStats>, bool, String)> {
        check(cancel)?;
        review::file_summary(repo, change, opts)
    }

    /// Human title of `cs` ("abc1234 Fix parser", "Working tree", ...); a commit's summary is
    /// read from `repo`.
    pub fn changeset_title(&self, repo: Option<&Repo>, cs: &Changeset) -> String {
        review::changeset_title(repo, cs)
    }

    /// Whether `path` of changeset `changeset_key` is marked viewed at `content_hash`.
    pub fn is_viewed(
        &self,
        repo_root: &Path,
        changeset_key: &str,
        path: &str,
        content_hash: &str,
    ) -> bool {
        self.store
            .as_ref()
            .is_some_and(|s| s.is_viewed(repo_root, changeset_key, path, content_hash))
    }

    /// Marks `file` viewed or not in the store and updates `file.viewed`. This is the only
    /// write of review state; `file.viewed` is left unchanged when the store fails.
    pub fn set_viewed(
        &self,
        repo_root: &Path,
        changeset_key: &str,
        file: &mut ReviewFile,
        viewed: bool,
    ) -> std::io::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no review store"))?;
        store.set_viewed(
            repo_root,
            changeset_key,
            file.path(),
            &file.content_hash,
            viewed,
        )?;
        file.viewed = viewed;
        Ok(())
    }

    fn marks<'a>(&'a self, root: &'a Path) -> impl review::ViewedMarks + 'a {
        move |key: &str, path: &str, hash: &str| self.is_viewed(root, key, path, hash)
    }
}

fn check(cancel: &CancelToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(lector_review::Error::Cancelled)
    } else {
        Ok(())
    }
}
