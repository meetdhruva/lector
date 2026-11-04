//! Merge-conflict state (read-only): which operation is in progress (merge,
//! rebase, cherry-pick, revert, am) and which index entries are conflicted, with
//! their stage blobs (base `:1`, ours `:2`, theirs `:3`).
//!
//! Everything here reads files under the git directory and the index through
//! gix; nothing is written. Continuing or aborting an operation lives in
//! [`crate::write`].

use std::collections::BTreeMap;
use std::path::Path;

use gix::bstr::{BStr, ByteSlice};

use crate::error::{Result, ResultExt};
use crate::repo::load_object;
use crate::{Oid, Repo};

/// The kind of multi-step operation in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OperationKind {
    /// `git merge` stopped (`MERGE_HEAD`).
    Merge,
    /// `git rebase` stopped (`rebase-merge/` or `rebase-apply/`).
    Rebase,
    /// `git cherry-pick` stopped (`CHERRY_PICK_HEAD`).
    CherryPick,
    /// `git revert` stopped (`REVERT_HEAD`).
    Revert,
    /// `git am` stopped (`rebase-apply/applying`).
    Am,
}

impl OperationKind {
    /// The git subcommand (`merge`, `rebase`, `cherry-pick`, `revert`, `am`).
    pub fn command(self) -> &'static str {
        match self {
            OperationKind::Merge => "merge",
            OperationKind::Rebase => "rebase",
            OperationKind::CherryPick => "cherry-pick",
            OperationKind::Revert => "revert",
            OperationKind::Am => "am",
        }
    }

    /// Present participle for UI labels ("Merging", "Rebasing", ...).
    pub fn verb(self) -> &'static str {
        match self {
            OperationKind::Merge => "Merging",
            OperationKind::Rebase => "Rebasing",
            OperationKind::CherryPick => "Cherry-picking",
            OperationKind::Revert => "Reverting",
            OperationKind::Am => "Applying patches",
        }
    }
}

/// A merge, rebase, cherry-pick, revert or am that is in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RepoOperation {
    /// What is in progress.
    pub kind: OperationKind,
    /// `rebase-merge/interactive` exists. `git rebase -i` sets it; modern git's
    /// merge backend sets it for plain rebases too (as `git status` reports them).
    pub interactive: bool,
    /// The branch being changed: the current branch for merge / cherry-pick /
    /// revert, the branch being rebased (`rebase-*/head-name`). `None` when detached.
    pub head_name: Option<String>,
    /// What comes in: the merged branch ("feature"), the picked or reverted
    /// commit ("abc1234 Fix parser"), or the rebase target ("main" or a short sha).
    pub incoming: Option<String>,
    /// The incoming commit (`MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
    /// `rebase-*/onto`).
    pub incoming_oid: Option<Oid>,
    /// Rebase / am progress: (current step, total steps), 1-based.
    pub step: Option<(u32, u32)>,
    /// The prepared commit message (`MERGE_MSG`) without comment lines.
    pub message: Option<String>,
}

impl RepoOperation {
    /// A short description: "Merging feature into main", "Rebasing feature onto
    /// main (3/7)", "Cherry-picking abc1234 Fix parser".
    pub fn describe(&self) -> String {
        let mut s = self.kind.verb().to_string();
        match self.kind {
            OperationKind::Rebase => {
                if let Some(head) = &self.head_name {
                    s.push(' ');
                    s.push_str(head);
                }
                if let Some(onto) = &self.incoming {
                    s.push_str(" onto ");
                    s.push_str(onto);
                }
            }
            _ => {
                if let Some(incoming) = &self.incoming {
                    s.push(' ');
                    s.push_str(incoming);
                }
                if self.kind == OperationKind::Merge {
                    if let Some(head) = &self.head_name {
                        s.push_str(" into ");
                        s.push_str(head);
                    }
                }
            }
        }
        if let Some((n, total)) = self.step {
            s.push_str(&format!(" ({n}/{total})"));
        }
        s
    }
}

/// One stage of a conflicted index entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ConflictStage {
    /// `:1` — the common ancestor.
    Base = 1,
    /// `:2` — "ours" (`HEAD`; during a rebase: the branch being rebased onto).
    Ours = 2,
    /// `:3` — "theirs" (the incoming change).
    Theirs = 3,
}

/// How a path conflicts, from which stages are present (like `git status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ConflictKind {
    /// Base, ours and theirs: both sides modified the file ("UU").
    BothModified,
    /// Ours and theirs, no base: both sides added the path ("AA").
    BothAdded,
    /// Base and theirs: we deleted, they modified ("DU").
    DeletedByUs,
    /// Base and ours: they deleted, we modified ("UD").
    DeletedByThem,
    /// Only ours ("AU").
    AddedByUs,
    /// Only theirs ("UA").
    AddedByThem,
    /// Only base: deleted on both sides ("DD").
    BothDeleted,
}

impl ConflictKind {
    /// Kind from which stages exist.
    pub fn from_stages(base: bool, ours: bool, theirs: bool) -> ConflictKind {
        match (base, ours, theirs) {
            (true, true, true) => ConflictKind::BothModified,
            (false, true, true) => ConflictKind::BothAdded,
            (true, false, true) => ConflictKind::DeletedByUs,
            (true, true, false) => ConflictKind::DeletedByThem,
            (false, true, false) => ConflictKind::AddedByUs,
            (false, false, true) => ConflictKind::AddedByThem,
            _ => ConflictKind::BothDeleted,
        }
    }

    /// Human label ("both modified", "deleted by them", ...).
    pub fn label(self) -> &'static str {
        match self {
            ConflictKind::BothModified => "both modified",
            ConflictKind::BothAdded => "both added",
            ConflictKind::DeletedByUs => "deleted by us",
            ConflictKind::DeletedByThem => "deleted by them",
            ConflictKind::AddedByUs => "added by us",
            ConflictKind::AddedByThem => "added by them",
            ConflictKind::BothDeleted => "both deleted",
        }
    }

    /// Whether the working file can contain conflict markers (both sides have content).
    pub fn has_markers(self) -> bool {
        matches!(self, ConflictKind::BothModified | ConflictKind::BothAdded)
    }
}

/// A conflicted path with its stage blobs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConflictEntry {
    /// Repository-relative path (`/`-separated).
    pub path: String,
    /// Stage 1 (common ancestor).
    pub base: Option<Oid>,
    /// Stage 2 (ours).
    pub ours: Option<Oid>,
    /// Stage 3 (theirs).
    pub theirs: Option<Oid>,
    /// Derived from the stages present.
    pub kind: ConflictKind,
}

impl ConflictEntry {
    /// The blob of `stage`, if present.
    pub fn stage(&self, stage: ConflictStage) -> Option<Oid> {
        match stage {
            ConflictStage::Base => self.base,
            ConflictStage::Ours => self.ours,
            ConflictStage::Theirs => self.theirs,
        }
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn short_ref(name: &str) -> String {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .or_else(|| name.strip_prefix("refs/tags/"))
        .unwrap_or(name)
        .to_string()
}

/// Drops `#` comment lines and surrounding blank lines from a prepared message.
pub fn strip_message_comments(message: &str) -> String {
    let kept: Vec<&str> = message
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.trim_end())
        .collect();
    kept.join("\n").trim().to_string()
}

/// The merged branch named in a `MERGE_MSG` subject ("Merge branch 'feature'
/// into main" → "feature"; "Merge remote-tracking branch 'origin/x'" → "origin/x").
pub fn merged_name_from_message(message: &str) -> Option<String> {
    let subject = message.lines().find(|l| !l.trim().is_empty())?;
    let rest = subject.strip_prefix("Merge ")?;
    let start = rest.find('\'')? + 1;
    let len = rest[start..].find('\'')?;
    let name = &rest[start..start + len];
    (!name.is_empty()).then(|| name.to_string())
}

impl Repo {
    /// The operation in progress, if any. Checked in git's order: rebase
    /// (`rebase-merge/`, `rebase-apply/`), am, merge, cherry-pick, revert.
    pub fn operation(&self) -> Result<Option<RepoOperation>> {
        let dir = self.git_dir().to_path_buf();
        let merge_dir = dir.join("rebase-merge");
        let apply_dir = dir.join("rebase-apply");
        if merge_dir.is_dir() || apply_dir.is_dir() {
            let (state, am) = if merge_dir.is_dir() {
                (merge_dir, false)
            } else {
                let am = apply_dir.join("applying").exists();
                (apply_dir, am)
            };
            let interactive = state.join("interactive").exists();
            let head_name = read_trimmed(&state.join("head-name"))
                .filter(|h| h != "detached HEAD")
                .map(|h| short_ref(&h));
            let onto = read_trimmed(&state.join("onto")).and_then(|s| s.parse::<Oid>().ok());
            let step = if state.join("msgnum").exists() {
                let n = read_trimmed(&state.join("msgnum")).and_then(|s| s.parse().ok());
                let total = read_trimmed(&state.join("end")).and_then(|s| s.parse().ok());
                n.zip(total)
            } else {
                let n = read_trimmed(&state.join("next")).and_then(|s| s.parse().ok());
                let total = read_trimmed(&state.join("last")).and_then(|s| s.parse().ok());
                n.zip(total)
            };
            let incoming = onto.map(|oid| self.name_for(oid));
            return Ok(Some(RepoOperation {
                kind: if am {
                    OperationKind::Am
                } else {
                    OperationKind::Rebase
                },
                interactive,
                head_name,
                incoming,
                incoming_oid: onto,
                step,
                message: None,
            }));
        }
        let head_name = self.head().ok().and_then(|h| h.branch);
        let merge_msg = std::fs::read_to_string(dir.join("MERGE_MSG"))
            .ok()
            .map(|m| strip_message_comments(&m))
            .filter(|m| !m.is_empty());
        let first_oid = |file: &str| -> Option<Oid> {
            let text = std::fs::read_to_string(dir.join(file)).ok()?;
            text.lines().next()?.trim().parse().ok()
        };
        if dir.join("MERGE_HEAD").exists() {
            let oid = first_oid("MERGE_HEAD");
            let incoming = merge_msg
                .as_deref()
                .and_then(merged_name_from_message)
                .or_else(|| oid.map(|o| self.name_for(o)));
            return Ok(Some(RepoOperation {
                kind: OperationKind::Merge,
                interactive: false,
                head_name,
                incoming,
                incoming_oid: oid,
                step: None,
                message: merge_msg,
            }));
        }
        for (file, kind) in [
            ("CHERRY_PICK_HEAD", OperationKind::CherryPick),
            ("REVERT_HEAD", OperationKind::Revert),
        ] {
            if dir.join(file).exists() {
                let oid = first_oid(file);
                let incoming = oid.map(|o| self.commit_label(o));
                return Ok(Some(RepoOperation {
                    kind,
                    interactive: false,
                    head_name,
                    incoming,
                    incoming_oid: oid,
                    step: None,
                    message: merge_msg,
                }));
            }
        }
        Ok(None)
    }

    /// A local (else remote) branch whose tip is `oid`, else the short sha.
    fn name_for(&self, oid: Oid) -> String {
        let branches = self.branches().unwrap_or_default();
        branches
            .iter()
            .filter(|b| b.oid == oid)
            .min_by_key(|b| (b.remote, b.name.len()))
            .map(|b| b.name.clone())
            .unwrap_or_else(|| oid.short())
    }

    /// "abc1234 Subject" for a commit (short sha alone if it can't be read).
    fn commit_label(&self, oid: Oid) -> String {
        match self.commit_infos(&[oid]) {
            Ok(infos) if !infos.is_empty() => format!("{} {}", oid.short(), infos[0].summary),
            _ => oid.short(),
        }
    }

    /// The prepared merge / cherry-pick message (`.git/MERGE_MSG`) without
    /// comment lines; `None` when absent or empty.
    pub fn merge_message(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(self.git_dir().join("MERGE_MSG")) {
            Ok(m) => {
                let m = strip_message_comments(&m);
                Ok((!m.is_empty()).then_some(m))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Conflicted paths (index entries with stages 1–3), sorted by path, with
    /// the blob of every stage present.
    pub fn conflicts(&self) -> Result<Vec<ConflictEntry>> {
        let repo = self.local();
        let index = repo.index_or_empty().git()?;
        let mut map: BTreeMap<String, [Option<Oid>; 3]> = BTreeMap::new();
        for entry in index.entries() {
            let stage = entry.stage_raw();
            if !(1..=3).contains(&stage) {
                continue;
            }
            let path = entry.path(&index).to_str_lossy().into_owned();
            map.entry(path).or_default()[stage as usize - 1] = Some(Oid::from(entry.id));
        }
        Ok(map
            .into_iter()
            .map(|(path, [base, ours, theirs])| ConflictEntry {
                kind: ConflictKind::from_stages(base.is_some(), ours.is_some(), theirs.is_some()),
                path,
                base,
                ours,
                theirs,
            })
            .collect())
    }

    /// The content of one stage of a conflicted path (`git show :<n>:<path>`);
    /// `Ok(None)` when that stage is absent (or the path is not conflicted).
    pub fn conflict_stage(&self, path: &str, stage: ConflictStage) -> Result<Option<Vec<u8>>> {
        let repo = self.local();
        let index = repo.index_or_empty().git()?;
        let path: &BStr = path.as_bytes().as_bstr();
        let gix_stage = match stage {
            ConflictStage::Base => gix::index::entry::Stage::Base,
            ConflictStage::Ours => gix::index::entry::Stage::Ours,
            ConflictStage::Theirs => gix::index::entry::Stage::Theirs,
        };
        match index.entry_by_path_and_stage(path, gix_stage) {
            Some(entry) => {
                let id = entry.id;
                load_object(&repo, id)
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_stages() {
        assert_eq!(
            ConflictKind::from_stages(true, true, true),
            ConflictKind::BothModified
        );
        assert_eq!(
            ConflictKind::from_stages(false, true, true),
            ConflictKind::BothAdded
        );
        assert_eq!(
            ConflictKind::from_stages(true, false, true),
            ConflictKind::DeletedByUs
        );
        assert_eq!(
            ConflictKind::from_stages(true, true, false),
            ConflictKind::DeletedByThem
        );
        assert_eq!(
            ConflictKind::from_stages(false, true, false),
            ConflictKind::AddedByUs
        );
        assert_eq!(
            ConflictKind::from_stages(false, false, true),
            ConflictKind::AddedByThem
        );
        assert_eq!(
            ConflictKind::from_stages(true, false, false),
            ConflictKind::BothDeleted
        );
        assert!(ConflictKind::BothAdded.has_markers());
        assert!(!ConflictKind::DeletedByUs.has_markers());
    }

    #[test]
    fn merge_message_names() {
        assert_eq!(
            merged_name_from_message("Merge branch 'feature' into main\n\n# Conflicts:"),
            Some("feature".into())
        );
        assert_eq!(
            merged_name_from_message("Merge remote-tracking branch 'origin/x'"),
            Some("origin/x".into())
        );
        assert_eq!(merged_name_from_message("Fix parser"), None);
        assert_eq!(merged_name_from_message(""), None);
        assert_eq!(merged_name_from_message("Merge branch ''"), None);
    }

    #[test]
    fn strips_comments() {
        let msg = "Merge branch 'f'\n\n# Conflicts:\n#\ta.txt\n";
        assert_eq!(strip_message_comments(msg), "Merge branch 'f'");
        assert_eq!(strip_message_comments("# only\n"), "");
    }

    #[test]
    fn describes_operations() {
        let op = RepoOperation {
            kind: OperationKind::Merge,
            interactive: false,
            head_name: Some("main".into()),
            incoming: Some("feature".into()),
            incoming_oid: None,
            step: None,
            message: None,
        };
        assert_eq!(op.describe(), "Merging feature into main");
        let op = RepoOperation {
            kind: OperationKind::Rebase,
            head_name: Some("feature".into()),
            incoming: Some("main".into()),
            step: Some((3, 7)),
            ..op
        };
        assert_eq!(op.describe(), "Rebasing feature onto main (3/7)");
    }
}
