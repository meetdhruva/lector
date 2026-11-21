//! Hunk patches: what staging, unstaging or discarding a hunk (or selected lines) of a file's
//! diff would apply (v0.2). The patch is computed here from the diff and what `repo` reads;
//! applying it is `lector-app`'s write (`review_service::apply`, ADR 0002).

use std::ops::Range;

use lector_diff::Algorithm;
use lector_git::{Changeset, FileChange, Repo, Side};

pub use lector_diff::patch::LineSelection;

use crate::review::{
    normalize_checkout_eol, normalize_loaded_pair, pair_content_id, FileDiff, Loaded,
    MAX_DIFF_BYTES,
};
use crate::{Error, Result};

/// What to do with a hunk (or a line selection) of a file's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HunkAction {
    /// Stage it: apply to the index. Needs an `Unstaged` change (index → worktree).
    Stage,
    /// Unstage it: revert it in the index. Needs a `Staged` change (HEAD → index).
    Unstage,
    /// Discard it: revert it in the working tree. Needs an `Unstaged` change.
    Discard,
}

impl HunkAction {
    /// The actions offered for a change of the given changeset (hunk toolbar).
    pub fn available_for(cs: &Changeset) -> &'static [HunkAction] {
        match cs {
            Changeset::Unstaged => &[HunkAction::Stage, HunkAction::Discard],
            Changeset::Staged => &[HunkAction::Unstage],
            _ => &[],
        }
    }
}

impl FileDiff {
    /// Selection of hunk `ix` of [`FileDiff::diff`] (`None` if out of range).
    pub fn hunk_selection(&self, ix: usize) -> Option<LineSelection> {
        self.diff.hunks.get(ix).map(LineSelection::hunk)
    }

    /// The hunk containing (or nearest after) old row `old` / new row `new`.
    pub fn hunk_at(&self, old: Option<u32>, new: Option<u32>) -> Option<usize> {
        let hunks = &self.diff.hunks;
        let within =
            |r: &Range<u32>, row: u32| r.contains(&row) || (r.is_empty() && r.start == row);
        hunks
            .iter()
            .position(|h| {
                old.is_some_and(|o| within(&h.old, o)) || new.is_some_and(|n| within(&h.new, n))
            })
            .or_else(|| {
                let n = new?;
                hunks.iter().position(|h| h.new.start >= n)
            })
    }
}

/// Bytes of `side` as git sees them for patching (`None` when it does not exist).
fn patch_bytes(repo: &Repo, side: &Side) -> Result<Option<Vec<u8>>> {
    Ok(repo.load(side)?)
}

/// Git file mode for the headers of new/deleted-file patches.
fn file_mode(repo: &Repo, rel: &str) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(repo.workdir().join(rel)) {
            if meta.permissions().mode() & 0o111 != 0 {
                return 0o100755;
            }
        }
    }
    let _ = (repo, rel);
    0o100644
}

/// Builds the patch for `action` on the `selection` (rows of the diff of `change`, a
/// file of `changeset`, as shown by [`load_file_diff_with`]) and where to apply it. `Ok(None)` when the
/// selection contains no change (or the file is binary).
///
/// Staging applies `index → worktree` to the index; unstaging and discarding build
/// the reverse pair (`index → HEAD`, `worktree → index`) with the selection swapped and
/// apply it forwards, so context always comes from the content being patched and
/// neighbouring unselected changes never get in the way. Working-tree content is
/// compared in git's normalized form (CRLF → LF when the index uses LF), which is
/// what `git apply` sees after its clean filter.
pub fn hunk_patch(
    repo: &Repo,
    changeset: &Changeset,
    change: &FileChange,
    action: HunkAction,
    selection: &LineSelection,
    algorithm: Algorithm,
) -> Result<Option<(Vec<u8>, lector_git::PatchTarget)>> {
    hunk_patch_checked(repo, changeset, change, action, selection, algorithm, None)
}

/// [`hunk_patch`], but refuses (with an error, computing nothing) when either side of the
/// file no longer matches `expected_content_id` ([`FileDiff::content_id`] of the diff the
/// selection was made in), so rows of a stale diff never address the wrong lines.
pub fn hunk_patch_if_unchanged(
    repo: &Repo,
    changeset: &Changeset,
    change: &FileChange,
    action: HunkAction,
    selection: &LineSelection,
    algorithm: Algorithm,
    expected_content_id: &str,
) -> Result<Option<(Vec<u8>, lector_git::PatchTarget)>> {
    hunk_patch_checked(
        repo,
        changeset,
        change,
        action,
        selection,
        algorithm,
        Some(expected_content_id),
    )
}

fn hunk_patch_checked(
    repo: &Repo,
    changeset: &Changeset,
    change: &FileChange,
    action: HunkAction,
    selection: &LineSelection,
    algorithm: Algorithm,
    expected_content_id: Option<&str>,
) -> Result<Option<(Vec<u8>, lector_git::PatchTarget)>> {
    use lector_diff::patch::{build_patch_with, PatchFile};
    use lector_git::PatchTarget;

    if !HunkAction::available_for(changeset).contains(&action) {
        return Err(Error::Other(format!(
            "cannot {} this change: hunks can be staged and discarded in unstaged changes, \
             and unstaged in staged changes",
            match action {
                HunkAction::Stage => "stage",
                HunkAction::Unstage => "unstage",
                HunkAction::Discard => "discard",
            }
        )));
    }
    let path = change
        .new_path
        .clone()
        .or_else(|| change.old_path.clone())
        .ok_or_else(|| Error::Other("change has no path".into()))?;
    let file = PatchFile {
        path: &path,
        mode: file_mode(repo, &path),
    };
    let old = patch_bytes(repo, &change.old)?;
    let new = patch_bytes(repo, &change.new)?;
    if let Some(expected) = expected_content_id {
        let as_loaded = |b: &Option<Vec<u8>>| match b {
            None => Loaded::Missing,
            Some(b) if b.len() as u64 > MAX_DIFF_BYTES => Loaded::TooLarge(b.len() as u64),
            Some(b) => Loaded::Bytes(b.clone()),
        };
        let old_loaded = as_loaded(&old);
        let new_loaded = normalize_loaded_pair(&old_loaded, &change.new, as_loaded(&new));
        if pair_content_id(change, &old_loaded, &new_loaded) != expected {
            return Err(Error::Other(format!(
                "{} changed since this diff was shown; nothing was applied. \
                 Review the updated diff and try again.",
                change.path()
            )));
        }
    }
    let (base, other, sel, target) = match action {
        HunkAction::Stage => {
            let new = new.map(|w| normalize_checkout_eol(old.as_deref(), w));
            (old, new, selection.clone(), PatchTarget::Index)
        }
        HunkAction::Unstage => (new, old, selection.swapped(), PatchTarget::Index),
        HunkAction::Discard => {
            let new = new.map(|w| normalize_checkout_eol(old.as_deref(), w));
            (new, old, selection.swapped(), PatchTarget::Worktree)
        }
    };
    Ok(
        build_patch_with(&file, base.as_deref(), other.as_deref(), &sel, algorithm)
            .map(|p| (p, target)),
    )
}
