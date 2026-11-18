//! Branch and stash writes (v0.7), through the `git` CLI under `lector-process`.
//!
//! Every function here is a write for an explicit user action. They return what an undo
//! needs (previous ref positions, dropped stash commits) and fail with [`Error::Sync`]
//! carrying a [`SyncErrorKind`] the UI can act on. Nothing is forced implicitly: switching
//! refuses to overwrite local changes (and reports the files, offering
//! `"stash_and_switch"` as data), deleting refuses unmerged branches unless `force`.

use std::ffi::OsStr;

use crate::error::{Error, Result};
use crate::proc::{self, SyncOptions};
use crate::sync::{failure, SyncError, SyncErrorKind};
use crate::{Oid, Repo, Stash};

/// Result of [`Repo::switch_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BranchSwitch {
    /// The branch before (`None` when `HEAD` was detached).
    pub from: Option<String>,
    /// `HEAD` before.
    pub from_oid: Option<Oid>,
    /// The branch now checked out.
    pub to: String,
    /// `HEAD` now.
    pub oid: Option<Oid>,
}

/// What [`Repo::delete_branch`] removed (enough to restore it).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DeletedBranch {
    /// Its name.
    pub name: String,
    /// Its tip.
    pub oid: Oid,
    /// Its upstream (`origin/main`), if one was set.
    pub upstream: Option<String>,
}

/// Options of [`Repo::stash_push`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StashPushOptions {
    /// Message (`-m`); git's default "WIP on <branch>" otherwise.
    pub message: Option<String>,
    /// Include untracked files (`-u`).
    pub include_untracked: bool,
}

/// Result of [`Repo::stash_apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StashApplied {
    /// The stash that was applied.
    pub stash: Stash,
    /// Paths left with conflicts (resolve with the conflict flow; a pop then keeps the stash).
    pub conflicts: Vec<String>,
    /// The stash entry was dropped (pop without conflicts).
    pub dropped: bool,
    /// The stash's staged changes were put back into the index as well
    /// ([`StashApplyOptions::restore_index`] and git applied them; `false` after a
    /// conflicting `--index` apply, which git reports as "Index was not unstashed").
    #[cfg_attr(feature = "serde", serde(default))]
    pub index_restored: bool,
}

/// Options of [`Repo::stash_apply_with`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StashApplyOptions {
    /// Drop the entry after an apply without conflicts (`git stash pop`).
    pub pop: bool,
    /// Refuse ([`SyncErrorKind::RefChanged`]) unless the entry is this commit.
    pub expected: Option<Oid>,
    /// Also restore the stash's staged changes into the index (`--index`). When git cannot
    /// recreate that index ("conflicts in index"), nothing is changed, the stash is kept and
    /// the call fails with [`SyncErrorKind::Other`] carrying the alternative
    /// [`APPLY_WITHOUT_INDEX`]; it never falls back to a flattening apply by itself.
    pub restore_index: bool,
}

/// [`SyncError::alternatives`] entry of a `--index` apply git refused: applying without
/// restoring the index would work (the stash's staged changes come back unstaged).
pub const APPLY_WITHOUT_INDEX: &str = "apply_without_index";

fn invalid(message: String) -> Error {
    SyncError::new(SyncErrorKind::InvalidName, message).into()
}

impl Repo {
    /// Rejects names git would read as options or that `git check-ref-format --branch`
    /// refuses.
    fn check_branch_name(&self, name: &str) -> Result<()> {
        if name.is_empty() || name.starts_with('-') {
            return Err(invalid(format!("invalid branch name {name:?}")));
        }
        let out = proc::write_raw(
            self.workdir(),
            &["check-ref-format", "--branch", name],
            None,
            "checking a branch name",
        )?;
        if !out.success() {
            return Err(invalid(format!("{name:?} is not a valid branch name")));
        }
        Ok(())
    }

    fn sync_write(&self, args: &[&str], what: &'static str) -> Result<lector_process::RunOutput> {
        let out = proc::write_raw(self.require_workdir()?, args, None, what)?;
        if !out.success() {
            return Err(failure(&out, SyncErrorKind::Other).into());
        }
        Ok(out)
    }

    /// Tip of local branch `name`, if it exists.
    pub fn branch_tip(&self, name: &str) -> Result<Option<Oid>> {
        self.rev_parse_opt(&format!("refs/heads/{name}"))
    }

    /// The upstream of local branch `name` (`origin/main`), if set and present.
    pub fn branch_upstream(&self, name: &str) -> Result<Option<String>> {
        let refname = format!("refs/heads/{name}");
        let out = proc::read(
            self.workdir(),
            &["for-each-ref", "--format=%(upstream:short)", &refname],
            "reading the upstream",
        )?;
        let v = String::from_utf8_lossy(&out).trim().to_owned();
        Ok((!v.is_empty()).then_some(v))
    }

    /// `git branch <name> [<start>]`; returns the new tip. `start` defaults to `HEAD`.
    pub fn create_branch(&self, name: &str, start: Option<&str>) -> Result<Oid> {
        self.check_branch_name(name)?;
        if self.branch_tip(name)?.is_some() {
            return Err(SyncError::new(
                SyncErrorKind::AlreadyExists,
                format!("a branch named {name} already exists"),
            )
            .into());
        }
        let start = start.unwrap_or("HEAD");
        let Some(tip) = self.rev_parse_opt(&format!("{start}^{{commit}}"))? else {
            return Err(SyncError::new(
                SyncErrorKind::NotFound,
                format!("{start} is not a commit"),
            )
            .into());
        };
        let tip_hex = tip.to_string();
        self.sync_write(
            &["branch", "--end-of-options", name, &tip_hex],
            "creating a branch",
        )?;
        Ok(tip)
    }

    /// `git switch <name>` (git's own guard refuses when local changes would be overwritten:
    /// [`SyncErrorKind::LocalChanges`] with the files and the `"stash_and_switch"`
    /// alternative). Remote-only names create a tracking branch, as in a terminal.
    /// `sync` carries credentials for LFS / hooks that need them.
    pub fn switch_branch(&self, name: &str, sync: &SyncOptions) -> Result<BranchSwitch> {
        if name.is_empty() || name.starts_with('-') {
            return Err(invalid(format!("invalid branch name {name:?}")));
        }
        let workdir = self.require_workdir()?;
        let head = self.head()?;
        let os: Vec<&OsStr> = ["switch", name].into_iter().map(OsStr::new).collect();
        let cmd = proc::git(workdir, &os, &format!("git switch {name}"));
        let out = proc::run(cmd, sync, "switching branches", None)?;
        if !out.success() {
            let mut err = failure(&out, SyncErrorKind::Other);
            if err.kind == SyncErrorKind::LocalChanges {
                err.alternatives.push("stash_and_switch".into());
            }
            return Err(err.into());
        }
        let now = self.head()?;
        Ok(BranchSwitch {
            from: head.branch.filter(|_| !head.detached),
            from_oid: head.oid,
            to: now.branch.unwrap_or_else(|| name.to_owned()),
            oid: now.oid,
        })
    }

    /// `git branch -m <old> <new>`.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        if old.is_empty() || old.starts_with('-') {
            return Err(invalid(format!("invalid branch name {old:?}")));
        }
        self.check_branch_name(new)?;
        if self.branch_tip(old)?.is_none() {
            return Err(
                SyncError::new(SyncErrorKind::NotFound, format!("no branch named {old}")).into(),
            );
        }
        self.sync_write(&["branch", "-m", old, new], "renaming a branch")
            .map(drop)
    }

    /// `git branch -d <name>` (refuses unmerged branches: [`SyncErrorKind::NotMerged`]), or
    /// `-D` with `force`. Returns its tip and upstream so it can be restored.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<DeletedBranch> {
        if name.is_empty() || name.starts_with('-') {
            return Err(invalid(format!("invalid branch name {name:?}")));
        }
        let Some(oid) = self.branch_tip(name)? else {
            return Err(
                SyncError::new(SyncErrorKind::NotFound, format!("no branch named {name}")).into(),
            );
        };
        let upstream = self.branch_upstream(name)?;
        let flag = if force { "-D" } else { "-d" };
        self.sync_write(&["branch", flag, name], "deleting a branch")?;
        Ok(DeletedBranch {
            name: name.to_owned(),
            oid,
            upstream,
        })
    }

    /// Sets (`git branch --set-upstream-to=<upstream> <branch>`) or removes
    /// (`--unset-upstream`) the upstream of `branch`; returns the previous one.
    pub fn set_upstream(&self, branch: &str, upstream: Option<&str>) -> Result<Option<String>> {
        if branch.is_empty() || branch.starts_with('-') {
            return Err(invalid(format!("invalid branch name {branch:?}")));
        }
        if self.branch_tip(branch)?.is_none() {
            return Err(SyncError::new(
                SyncErrorKind::NotFound,
                format!("no branch named {branch}"),
            )
            .into());
        }
        let previous = self.branch_upstream(branch)?;
        match upstream {
            Some(up) => {
                if up.is_empty() || up.starts_with('-') {
                    return Err(invalid(format!("invalid upstream {up:?}")));
                }
                let arg = format!("--set-upstream-to={up}");
                self.sync_write(&["branch", &arg, branch], "setting the upstream")?;
            }
            None => {
                if previous.is_some()
                    || self
                        .config_value(&format!("branch.{branch}.merge"))?
                        .is_some()
                {
                    self.sync_write(
                        &["branch", "--unset-upstream", branch],
                        "removing the upstream",
                    )?;
                }
            }
        }
        Ok(previous)
    }

    /// Moves, creates or deletes a branch ref back to a recorded position — the undo of
    /// pull, branch delete and branch create. Refuses ([`SyncErrorKind::RefChanged`]) unless
    /// the ref is exactly at `expected` (`None` = must not exist). `target == None` deletes
    /// it. For the checked-out branch it runs `git reset --keep <target>` keeping the index:
    /// see [`reset_keep_preserving_index`](Self::reset_keep_preserving_index). Other refs go
    /// through `git update-ref` with the expected value.
    pub fn restore_ref(
        &self,
        refname: &str,
        expected: Option<Oid>,
        target: Option<Oid>,
    ) -> Result<()> {
        let Some(branch) = refname.strip_prefix("refs/heads/") else {
            return Err(invalid(format!(
                "only branches can be restored, not {refname}"
            )));
        };
        if branch.is_empty() || branch.starts_with('-') {
            return Err(invalid(format!("invalid branch name {branch:?}")));
        }
        let current = self.rev_parse_opt(refname)?;
        if current != expected {
            return Err(SyncError::new(
                SyncErrorKind::RefChanged,
                format!(
                    "{branch} is at {} now, not {}",
                    current.map_or("nothing".into(), |o| o.short()),
                    expected.map_or("nothing".into(), |o| o.short())
                ),
            )
            .into());
        }
        let head = self.head()?;
        let checked_out = head.branch.as_deref() == Some(branch) && !head.detached;
        if checked_out {
            let Some(target) = target else {
                return Err(invalid(format!(
                    "cannot delete the checked-out branch {branch}"
                )));
            };
            let Some(head_oid) = current else {
                return Err(invalid(format!("{branch} has no commit")));
            };
            return self.reset_keep_preserving_index(head_oid, target);
        }
        let zero = "0".repeat(40);
        let exp = expected.map_or(zero, |o| o.to_string());
        match target {
            Some(t) => {
                let t = t.to_string();
                self.sync_write(
                    &["update-ref", "-m", "lector: undo", refname, &t, &exp],
                    "restoring the branch",
                )?;
            }
            None => {
                self.sync_write(
                    &["update-ref", "-m", "lector: undo", "-d", refname, &exp],
                    "restoring the branch",
                )?;
            }
        }
        Ok(())
    }

    /// Moves the checked-out branch from `head` to `target` like `git reset --keep`, without
    /// losing what is staged. `reset --keep` keeps working-tree changes but resets every index
    /// entry to `target`, so staged content of files the move does not touch would be lost.
    /// Here:
    /// * a path that differs between `head` and `target` and has local changes (staged or
    ///   not) refuses the whole move with [`SyncErrorKind::LocalChanges`] (its paths) before
    ///   anything is written — both versions cannot be kept;
    /// * unresolved conflicts refuse it with [`SyncErrorKind::DirtyWorktree`];
    /// * otherwise the staged entries (`git diff --cached <head>`, renames as delete + add)
    ///   are read first and written back after the reset with `git update-index
    ///   --index-info` (staged deletions removed again), so the index ends up exactly as
    ///   before for every path the move does not touch.
    ///
    /// Intent-to-add entries (`git add -N`) are not staged content and come back untracked.
    pub fn reset_keep_preserving_index(&self, head: Oid, target: Oid) -> Result<()> {
        let workdir = self.require_workdir()?;
        let status = self.status()?;
        let conflicted: Vec<String> = status
            .iter()
            .filter(|e| e.conflicted)
            .map(|e| e.path.clone())
            .collect();
        if !conflicted.is_empty() {
            let mut err = SyncError::new(
                SyncErrorKind::DirtyWorktree,
                "the repository has unresolved conflicts; nothing was changed",
            )
            .with_hint("Resolve or abort the conflicts first.");
            err.paths = conflicted;
            return Err(err.into());
        }
        let (h, t) = (head.to_string(), target.to_string());
        let moved = nul_names(&proc::read(
            workdir,
            &[
                "diff",
                "--no-ext-diff",
                "--name-only",
                "-z",
                "--no-renames",
                &h,
                &t,
                "--",
            ],
            "listing the files a branch move changes",
        )?);
        let staged = nul_names(&proc::read(
            workdir,
            &[
                "diff",
                "--no-ext-diff",
                "--cached",
                "--name-only",
                "-z",
                "--no-renames",
                &h,
                "--",
            ],
            "listing staged changes",
        )?);
        let unstaged = nul_names(&proc::read(
            workdir,
            &[
                "diff",
                "--no-ext-diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--",
            ],
            "listing unstaged changes",
        )?);
        let mut overlap: Vec<String> = moved
            .iter()
            .filter(|p| staged.contains(*p) || unstaged.contains(*p))
            .cloned()
            .collect();
        overlap.sort();
        overlap.dedup();
        if !overlap.is_empty() {
            let mut err = SyncError::new(
                SyncErrorKind::LocalChanges,
                format!(
                    "moving the branch would change files that have local changes ({}); \
                     nothing was changed",
                    overlap.join(", ")
                ),
            )
            .with_hint("Commit or stash these changes first.");
            err.paths = overlap;
            return Err(err.into());
        }
        // Staged entries to put back after the reset: (path, Some((mode, oid))) for a staged
        // version, (path, None) for a staged deletion.
        let mut saved: Vec<(String, Option<(String, String)>)> = Vec::new();
        if !staged.is_empty() {
            let listing = proc::read(workdir, &["ls-files", "--stage", "-z"], "reading the index")?;
            let mut entries: std::collections::HashMap<String, (String, String)> =
                std::collections::HashMap::new();
            for rec in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                let rec = String::from_utf8_lossy(rec);
                let Some((meta, path)) = rec.split_once('\t') else {
                    continue;
                };
                let mut parts = meta.split(' ');
                let (Some(mode), Some(oid), Some("0")) = (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                entries.insert(path.to_owned(), (mode.to_owned(), oid.to_owned()));
            }
            for path in &staged {
                saved.push((path.clone(), entries.get(path).cloned()));
            }
        }
        self.sync_write(&["reset", "--keep", &t], "restoring the branch")?;
        if saved.is_empty() {
            return Ok(());
        }
        let zero = "0".repeat(t.len());
        let mut input = Vec::new();
        for (path, entry) in &saved {
            let line = match entry {
                Some((mode, oid)) => format!("{mode} {oid}\t{path}"),
                None => format!("0 {zero}\t{path}"),
            };
            input.extend_from_slice(line.as_bytes());
            input.push(0);
        }
        let out = proc::write_raw(
            workdir,
            &["update-index", "-z", "--index-info"],
            Some(&input),
            "restoring staged changes",
        )?;
        if !out.success() {
            let listing: Vec<String> = saved
                .iter()
                .map(|(p, e)| match e {
                    Some((mode, oid)) => format!("{mode} {oid} {p}"),
                    None => format!("deleted {p}"),
                })
                .collect();
            let mut err = failure(&out, SyncErrorKind::Other);
            err.message = format!(
                "the branch was restored but its staged changes could not be staged again \
                 (the files themselves are unchanged): {}\nStaged entries: {}",
                err.message,
                listing.join("; ")
            );
            err.paths = saved.into_iter().map(|(p, _)| p).collect();
            return Err(err.into());
        }
        Ok(())
    }

    /// The stash entry at `index`, or [`SyncErrorKind::NotFound`].
    pub fn stash_at(&self, index: usize) -> Result<Stash> {
        self.stashes()?
            .into_iter()
            .find(|s| s.index == index)
            .ok_or_else(|| {
                SyncError::new(SyncErrorKind::NotFound, format!("no stash@{{{index}}}")).into()
            })
    }

    /// Index of the stash entry with commit `oid` (stash indices shift; oids don't).
    pub fn stash_index_of(&self, oid: Oid) -> Result<Option<usize>> {
        Ok(self
            .stashes()?
            .into_iter()
            .find(|s| s.oid == oid)
            .map(|s| s.index))
    }

    /// Paths a stash entry touches (tracked changes and its untracked files), by the names
    /// they have in the stash. See [`stash_apply_paths`](Self::stash_apply_paths) for the
    /// paths an apply can write now.
    pub fn stash_paths(&self, index: usize) -> Result<Vec<String>> {
        let stash = self.stash_at(index)?;
        let oid = stash.oid.to_string();
        let mut paths = nul_names(&proc::read(
            self.workdir(),
            &[
                "diff",
                "--no-ext-diff",
                "--name-only",
                "-z",
                "--no-renames",
                &format!("{oid}^1"),
                &oid,
                "--",
            ],
            "listing stash paths",
        )?);
        for p in self.stash_untracked_paths(stash.oid)? {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        Ok(paths)
    }

    /// The untracked files saved in stash commit `oid` (its third parent), if any.
    fn stash_untracked_paths(&self, oid: Oid) -> Result<Vec<String>> {
        let untracked_commit = format!("{oid}^3");
        if self.rev_parse_opt(&untracked_commit)?.is_none() {
            return Ok(Vec::new());
        }
        Ok(nul_names(&proc::read(
            self.workdir(),
            &["ls-tree", "-r", "-z", "--name-only", &untracked_commit],
            "listing stash paths",
        )?))
    }

    /// Every path applying stash entry `index` now can write: its own paths
    /// ([`stash_paths`](Self::stash_paths)) plus where the merge carries them when the
    /// current index renamed them since the stash was made — the destination of each rename
    /// between the stash's base and the index (`git diff --cached -M <base>`, the same
    /// detection the merge uses) whose source is a stash path, and the stash's paths moved
    /// along with directory renames. A superset is harmless (backups of untouched paths
    /// restore them unchanged); callers that need certainty compare `git status` before and
    /// after the apply as well.
    pub fn stash_apply_paths(&self, index: usize) -> Result<Vec<String>> {
        let stash = self.stash_at(index)?;
        let mut paths = self.stash_paths(index)?;
        let base = format!("{}^1", stash.oid);
        let raw = proc::read(
            self.workdir(),
            &[
                "diff",
                "--no-ext-diff",
                "--cached",
                "--name-status",
                "-z",
                "-M",
                &base,
                "--",
            ],
            "finding renames since the stash",
        )?;
        let mut tokens = raw
            .split(|b| *b == 0)
            .map(|t| String::from_utf8_lossy(t).into_owned());
        let mut renames: Vec<(String, String)> = Vec::new();
        while let Some(status) = tokens.next() {
            if status.is_empty() {
                continue;
            }
            let Some(first) = tokens.next() else { break };
            if status.starts_with('R') || status.starts_with('C') {
                let Some(second) = tokens.next() else { break };
                if status.starts_with('R') {
                    renames.push((first, second));
                }
            }
        }
        let own = paths.clone();
        for (from, to) in &renames {
            if own.contains(from) && !paths.contains(to) {
                paths.push(to.clone());
            }
            // Directory renames: a/x -> b/x carries the stash's other a/... paths to b/...
            let (Some((from_dir, _)), Some((to_dir, _))) =
                (from.rsplit_once('/'), to.rsplit_once('/'))
            else {
                continue;
            };
            if from_dir == to_dir {
                continue;
            }
            let prefix = format!("{from_dir}/");
            for p in &own {
                if let Some(rest) = p.strip_prefix(&prefix) {
                    let moved = format!("{to_dir}/{rest}");
                    if !paths.contains(&moved) {
                        paths.push(moved);
                    }
                }
            }
        }
        Ok(paths)
    }

    /// `git stash push [-u] [-m <message>]`. `Ok(None)` when there was nothing to stash.
    pub fn stash_push(&self, opts: &StashPushOptions) -> Result<Option<Stash>> {
        let before = self.stashes()?.first().map(|s| s.oid);
        let mut args = vec!["stash", "push"];
        if opts.include_untracked {
            args.push("--include-untracked");
        }
        if let Some(m) = &opts.message {
            args.push("-m");
            args.push(m);
        }
        self.sync_write(&args, "stashing")?;
        let top = self.stashes()?.into_iter().next();
        Ok(top.filter(|s| Some(s.oid) != before))
    }

    /// `git stash apply` / `pop stash@{index}`. Conflicts are reported in the outcome (not an
    /// error); a pop that conflicts keeps the stash, as git does. Local changes that would be
    /// overwritten fail with [`SyncErrorKind::LocalChanges`]. Same as
    /// [`stash_apply_expecting`](Self::stash_apply_expecting) without an expected commit.
    pub fn stash_apply(&self, index: usize, pop: bool) -> Result<StashApplied> {
        self.stash_apply_expecting(index, None, pop)
    }

    /// [`stash_apply`](Self::stash_apply) of the entry at `index`, refused with
    /// [`SyncErrorKind::RefChanged`] ("the stash list changed — refresh") when that entry is
    /// not commit `expected` (when given) or when `stash@{index}` moves between reading the
    /// list and running git (another git process pushed or dropped a stash). An apply runs
    /// on the commit itself; a pop names `stash@{index}`, re-checked right before.
    pub fn stash_apply_expecting(
        &self,
        index: usize,
        expected: Option<Oid>,
        pop: bool,
    ) -> Result<StashApplied> {
        self.stash_apply_with(
            index,
            &StashApplyOptions {
                pop,
                expected,
                restore_index: false,
            },
        )
    }

    /// `git stash apply|pop [--index]` of the entry at `index` with [`StashApplyOptions`].
    /// Conflicts are reported in the outcome (a pop then keeps the stash). Refusals change
    /// nothing:
    /// * the stash's untracked files must not exist in the work tree (git would apply the
    ///   tracked changes, create some untracked files and then fail half way):
    ///   [`SyncErrorKind::LocalChanges`] with those paths;
    /// * local changes git would overwrite: [`SyncErrorKind::LocalChanges`];
    /// * `restore_index` when git cannot recreate the stashed index: [`SyncErrorKind::Other`]
    ///   with the alternative [`APPLY_WITHOUT_INDEX`].
    ///
    /// Any failure keeps the stash.
    pub fn stash_apply_with(&self, index: usize, opts: &StashApplyOptions) -> Result<StashApplied> {
        let workdir = self.require_workdir()?;
        let stash = self.stash_at_expecting(index, opts.expected)?;
        let existing: Vec<String> = self
            .stash_untracked_paths(stash.oid)?
            .into_iter()
            .filter(|p| workdir.join(p).symlink_metadata().is_ok())
            .collect();
        if !existing.is_empty() {
            let mut err = SyncError::new(
                SyncErrorKind::LocalChanges,
                format!(
                    "untracked files saved in the stash already exist ({}); nothing was changed",
                    existing.join(", ")
                ),
            )
            .with_hint("Move or delete those files, then apply the stash again.");
            err.paths = existing;
            return Err(err.into());
        }
        let oid = stash.oid.to_string();
        let spec = format!("stash@{{{index}}}");
        let mut args: Vec<&str> = vec!["stash", if opts.pop { "pop" } else { "apply" }];
        if opts.restore_index {
            args.push("--index");
        }
        if opts.pop {
            self.ensure_stash_unchanged(index, stash.oid)?;
            args.push(&spec);
        } else {
            args.push(&oid);
        }
        let out = proc::write_raw(workdir, &args, None, "applying a stash")?;
        let stderr = out.stderr_text();
        if !out.success() && opts.restore_index && stderr.contains("Try without --index") {
            let mut err = SyncError::new(
                SyncErrorKind::Other,
                "the staged changes saved in the stash cannot be restored on top of the \
                 current index; nothing was changed and the stash is kept",
            )
            .with_hint(
                "Apply the stash without restoring the index: its changes come back unstaged.",
            );
            err.alternatives.push(APPLY_WITHOUT_INDEX.into());
            return Err(err.into());
        }
        let conflicts: Vec<String> = self
            .status()?
            .into_iter()
            .filter(|e| e.conflicted)
            .map(|e| e.path)
            .collect();
        if !out.success() && conflicts.is_empty() {
            return Err(failure(&out, SyncErrorKind::Other).into());
        }
        let dropped = opts.pop && self.stash_index_of(stash.oid)?.is_none();
        let index_restored =
            opts.restore_index && out.success() && !stderr.contains("Index was not unstashed");
        Ok(StashApplied {
            stash,
            conflicts,
            dropped,
            index_restored,
        })
    }

    /// `git stash drop stash@{index}`; returns the dropped entry (its commit stays reachable
    /// until gc, so [`stash_store`](Self::stash_store) can bring it back). Same as
    /// [`stash_drop_expecting`](Self::stash_drop_expecting) without an expected commit.
    pub fn stash_drop(&self, index: usize) -> Result<Stash> {
        self.stash_drop_expecting(index, None)
    }

    /// [`stash_drop`](Self::stash_drop), refused with [`SyncErrorKind::RefChanged`] when the
    /// entry at `index` is not `expected` (when given) or `stash@{index}` moved just before
    /// the drop would run.
    pub fn stash_drop_expecting(&self, index: usize, expected: Option<Oid>) -> Result<Stash> {
        let stash = self.stash_at_expecting(index, expected)?;
        let spec = format!("stash@{{{index}}}");
        self.ensure_stash_unchanged(index, stash.oid)?;
        self.sync_write(&["stash", "drop", "-q", &spec], "dropping a stash")?;
        Ok(stash)
    }

    fn stash_at_expecting(&self, index: usize, expected: Option<Oid>) -> Result<Stash> {
        let stash = self.stash_at(index)?;
        if expected.is_some_and(|e| e != stash.oid) {
            return Err(stash_list_changed());
        }
        Ok(stash)
    }

    /// `stash@{index}` still names `oid` (checked right before a command that takes the index).
    fn ensure_stash_unchanged(&self, index: usize, oid: Oid) -> Result<()> {
        let now = self.rev_parse_opt(&format!("stash@{{{index}}}"))?;
        if now == Some(oid) {
            Ok(())
        } else {
            Err(stash_list_changed())
        }
    }

    /// `git stash store -m <message> <oid>`: puts a (dropped) stash commit back on top.
    pub fn stash_store(&self, oid: Oid, message: &str) -> Result<()> {
        let o = oid.to_string();
        self.sync_write(&["stash", "store", "-m", message, &o], "restoring a stash")
            .map(drop)
    }
}

/// NUL-separated names (`-z` output) as strings, empty records skipped.
fn nul_names(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect()
}

fn stash_list_changed() -> Error {
    SyncError::new(
        SyncErrorKind::RefChanged,
        "the stash list changed \u{2014} refresh",
    )
    .with_hint("Refresh the stash list and pick the entry again.")
    .into()
}
