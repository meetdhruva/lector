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
}

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
    /// it. For the checked-out branch it runs `git reset --keep <target>` (which refuses to
    /// lose local changes); other refs go through `git update-ref` with the expected value.
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
            let t = target.to_string();
            self.sync_write(&["reset", "--keep", &t], "restoring the branch")?;
            return Ok(());
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

    /// Paths a stash entry touches (tracked changes and its untracked files).
    pub fn stash_paths(&self, index: usize) -> Result<Vec<String>> {
        let stash = self.stash_at(index)?;
        let oid = stash.oid.to_string();
        let mut paths = Vec::new();
        let tracked = proc::read(
            self.workdir(),
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                &format!("{oid}^1"),
                &oid,
                "--",
            ],
            "listing stash paths",
        )?;
        let untracked_commit = format!("{oid}^3");
        let untracked = match self.rev_parse_opt(&untracked_commit)? {
            Some(_) => proc::read(
                self.workdir(),
                &["ls-tree", "-r", "-z", "--name-only", &untracked_commit],
                "listing stash paths",
            )?,
            None => Vec::new(),
        };
        for name in tracked
            .split(|b| *b == 0)
            .chain(untracked.split(|b| *b == 0))
        {
            if !name.is_empty() {
                let p = String::from_utf8_lossy(name).into_owned();
                if !paths.contains(&p) {
                    paths.push(p);
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
    /// overwritten fail with [`SyncErrorKind::LocalChanges`].
    pub fn stash_apply(&self, index: usize, pop: bool) -> Result<StashApplied> {
        let stash = self.stash_at(index)?;
        let spec = format!("stash@{{{index}}}");
        let verb = if pop { "pop" } else { "apply" };
        let out = proc::write_raw(
            self.require_workdir()?,
            &["stash", verb, &spec],
            None,
            "applying a stash",
        )?;
        let conflicts: Vec<String> = self
            .status()?
            .into_iter()
            .filter(|e| e.conflicted)
            .map(|e| e.path)
            .collect();
        if !out.success() && conflicts.is_empty() {
            return Err(failure(&out, SyncErrorKind::Other).into());
        }
        let dropped = pop && self.stash_index_of(stash.oid)?.is_none();
        Ok(StashApplied {
            stash,
            conflicts,
            dropped,
        })
    }

    /// `git stash drop stash@{index}`; returns the dropped entry (its commit stays reachable
    /// until gc, so [`stash_store`](Self::stash_store) can bring it back).
    pub fn stash_drop(&self, index: usize) -> Result<Stash> {
        let stash = self.stash_at(index)?;
        let spec = format!("stash@{{{index}}}");
        self.sync_write(&["stash", "drop", "-q", &spec], "dropping a stash")?;
        Ok(stash)
    }

    /// `git stash store -m <message> <oid>`: puts a (dropped) stash commit back on top.
    pub fn stash_store(&self, oid: Oid, message: &str) -> Result<()> {
        let o = oid.to_string();
        self.sync_write(&["stash", "store", "-m", message, &o], "restoring a stash")
            .map(drop)
    }
}
