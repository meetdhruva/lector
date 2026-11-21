//! Applying a hunk action to the repository (ADR 0002).
//!
//! `lector_review::review::hunk_patch*` compute the patch (a pure function of the diff, the
//! selection and what `repo` reads); this module applies it with `git apply`, to the index or
//! the working tree. It is called by the `git.stage_hunk` / `unstage_hunk` / `discard_hunk`
//! commands, which take the backup and write the journal entry around it.

use lector_diff::Algorithm;
use lector_git::{Changeset, FileChange, Repo};
use lector_review::review::{self, HunkAction, LineSelection};
use lector_review::Result;

/// Stages, unstages or discards the `selection` of `change` (see `review::hunk_patch`).
/// Returns `false` when there was nothing to apply.
pub fn apply_hunk_action(
    repo: &Repo,
    changeset: &Changeset,
    change: &FileChange,
    action: HunkAction,
    selection: &LineSelection,
    algorithm: Algorithm,
) -> Result<bool> {
    let patch = review::hunk_patch(repo, changeset, change, action, selection, algorithm)?;
    apply_patch(repo, patch)
}

/// [`apply_hunk_action`], but refuses (with an error, writing nothing) when either side
/// of the file no longer matches `expected_content_id` (`FileDiff::content_id` of the
/// diff the selection was made in), so rows of a stale diff never stage, unstage or
/// discard the wrong lines.
pub fn apply_hunk_action_if_unchanged(
    repo: &Repo,
    changeset: &Changeset,
    change: &FileChange,
    action: HunkAction,
    selection: &LineSelection,
    algorithm: Algorithm,
    expected_content_id: &str,
) -> Result<bool> {
    let patch = review::hunk_patch_if_unchanged(
        repo,
        changeset,
        change,
        action,
        selection,
        algorithm,
        expected_content_id,
    )?;
    apply_patch(repo, patch)
}

fn apply_patch(repo: &Repo, patch: Option<(Vec<u8>, lector_git::PatchTarget)>) -> Result<bool> {
    match patch {
        Some((patch, target)) => {
            repo.apply_patch(&patch, target, false)?;
            Ok(true)
        }
        None => Ok(false),
    }
}
