//! Helpers of restoring: removing emptied directories, writing restored files, intent-to-add.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

use crate::error::{Error, Result};

use super::command::exec;
use super::util::{apply_permissions, make_symlink};
use super::util::{check_rel, is_within};
use super::{RestoreReport, ShadowRepo, ShadowSnapshot, SnapshotKind};
use crate::Oid;

impl ShadowRepo {
    pub(super) fn remove_empty_parents(&self, file: &Path) {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d == self.workdir || !d.starts_with(&self.workdir) {
                break;
            }
            if fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    /// Stages one restored file: writes it to a temp file next to its destination (creating
    /// missing parent directories) and returns the temp path; [`ShadowRepo::commit_restored`]
    /// renames it into place. Staging every file first means a failure while reading blobs or
    /// writing data leaves the work tree as it was.
    pub(super) fn stage_restored(
        &self,
        rel: &str,
        mode: &str,
        reader: &mut dyn Read,
        size: u64,
    ) -> Result<PathBuf> {
        let abs = self.workdir.join(rel);
        let parent = abs
            .parent()
            .ok_or_else(|| Error::Parse(format!("bad path {rel:?}")))?;
        fs::create_dir_all(parent)?;
        let existing = fs::symlink_metadata(&abs).ok();
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let tmp = parent.join(format!(
            ".lector-restore-{}-{}.tmp",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<()> {
            if mode == "120000" {
                let mut target = Vec::with_capacity(size as usize);
                reader.read_to_end(&mut target)?;
                make_symlink(&target, &tmp)?;
            } else {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                io::copy(reader, &mut file)?;
                file.sync_all()?;
                drop(file);
                apply_permissions(&tmp, existing.as_ref(), mode == "100755")?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(tmp),
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                Err(err)
            }
        }
    }

    /// Renames a staged file ([`ShadowRepo::stage_restored`]) into place.
    pub(super) fn commit_restored(&self, tmp: &Path, rel: &str) -> Result<()> {
        fs::rename(tmp, self.workdir.join(rel))?;
        Ok(())
    }

    /// Gives the restored file at `rel` its recorded final-component spelling when its entry is
    /// stored under another one (a case-insensitive or normalising file system kept the old
    /// entry's name when the staged file was renamed onto it). A metadata-only rename of the same
    /// entry, so the file is never missing. `Ok` also when the file system will not store the
    /// recorded spelling (the entry still resolves to the file afterwards); `Err(spelling on
    /// disk)` when the rename failed or the entry no longer resolves.
    pub(super) fn respell(&self, rel: &str) -> std::result::Result<(), String> {
        let abs = self.workdir.join(rel);
        let (Some(dir), Some(name)) = (abs.parent(), abs.file_name()) else {
            return Ok(());
        };
        let Some(stored) = super::util::stored_name(dir, name) else {
            return Ok(());
        };
        if stored.as_os_str() == name {
            return Ok(());
        }
        let shown = || match rel.rsplit_once('/') {
            Some((parent, _)) => format!("{parent}/{}", stored.to_string_lossy()),
            None => stored.to_string_lossy().into_owned(),
        };
        if fs::rename(dir.join(&stored), &abs).is_err() {
            return Err(shown());
        }
        match super::util::stored_name(dir, name) {
            // Renamed, or the file system keeps its own form of the name (HFS+ and NFD).
            Some(_) => Ok(()),
            None => Err(shown()),
        }
    }

    /// What is at `rel` now that a restore must not replace or remove: a directory, a FIFO,
    /// socket or device, or a parent component that is a file (so nothing can be created below
    /// it). `None` when the path is absent, a regular file or a link.
    pub(super) fn blocks_restore(&self, rel: &str) -> Option<String> {
        let mut current = self.workdir.clone();
        let comps: Vec<&str> = rel.split('/').collect();
        for comp in &comps[..comps.len().saturating_sub(1)] {
            current.push(comp);
            match fs::symlink_metadata(&current) {
                Ok(meta) if !meta.is_dir() => {
                    return Some(format!(
                        "{} is now a file where the backup has a directory",
                        comps[..comps.iter().position(|c| c == comp).unwrap_or(0) + 1].join("/")
                    ))
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        match fs::symlink_metadata(self.workdir.join(rel)) {
            Ok(meta) if meta.is_dir() => Some(format!("{rel} is now a directory")),
            Ok(meta) if !meta.is_file() && !meta.file_type().is_symlink() => Some(format!(
                "{rel} is now not a regular file (a FIFO, socket or device)"
            )),
            _ => None,
        }
    }

    /// The paths among (or below) `rels` whose index entry is intent-to-add: the only entries
    /// the index-to-working-tree diff shows as added.
    pub(super) fn intent_to_add(&self, rels: &[String]) -> Result<BTreeSet<String>> {
        self.needs_repository()?;
        let mut out = BTreeSet::new();
        for chunk in rels.chunks(500) {
            let mut args = vec![
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules",
                "--diff-filter=A",
                "--",
            ];
            args.extend(chunk.iter().map(String::as_str));
            let output = exec(self.user_git(&args), "git diff", None)?;
            for name in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                out.insert(String::from_utf8_lossy(name).into_owned());
            }
        }
        Ok(out)
    }
}

impl ShadowRepo {
    /// [`ShadowRepo::restore_paths`], returning what it could not complete beyond the contents:
    /// the restored files whose entry keeps another spelling than the recorded one. Every error
    /// is a restore that failed as described there.
    pub fn restore_paths_reporting(
        &self,
        snap: &ShadowSnapshot,
        paths: &[&str],
    ) -> Result<RestoreReport> {
        let manifest = self.manifest(snap)?;
        if manifest.kind == SnapshotKind::Index {
            return Err(Error::Parse(
                "this snapshot recorded the index, not files; use restore_index".into(),
            ));
        }
        let files = self.tree_files(snap)?;
        let full = manifest.kind == SnapshotKind::Worktree;

        // Plan first: nothing is written when any requested path is unusable.
        let mut writes: Vec<(String, String, Oid)> = Vec::new(); // (rel, mode, oid)
        let mut deletes: Vec<String> = Vec::new();
        let mut not_backed_up: Vec<String> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        for path in paths {
            let rel = check_rel(path)?;
            if self.no_symlink_parents(&rel).is_err() {
                blocked.push(format!("{rel} is now reached through a link"));
                continue;
            }
            let mut found = false;
            for (name, (mode, oid)) in &files {
                if is_within(name, &rel) {
                    found = true;
                    writes.push((name.clone(), mode.clone(), *oid));
                }
            }
            if manifest.skipped.iter().any(|s| is_within(s, &rel)) {
                not_backed_up.push(rel.clone());
                continue;
            }
            if manifest.absent.iter().any(|a| a == &rel) || (!found && full) {
                found = true;
                deletes.push(rel.clone());
            }
            if !found {
                return Err(Error::Parse(format!(
                    "{rel:?} is not part of this snapshot"
                )));
            }
        }
        if !not_backed_up.is_empty() {
            return Err(Error::Io(io::Error::other(format!(
                "{} was not backed up (too large, or not a regular file), so it cannot be restored from this backup; nothing was restored",
                not_backed_up.join(", ")
            ))));
        }
        // A path restored from a file and deleted at the same time can't be both.
        writes.sort();
        writes.dedup();
        deletes.sort();
        deletes.dedup();
        for rel in writes.iter().map(|(r, ..)| r).chain(&deletes) {
            if self.no_symlink_parents(rel).is_err() {
                blocked.push(format!("{rel} is now reached through a link"));
            } else if let Some(why) = self.blocks_restore(rel) {
                blocked.push(why);
            }
        }
        if !blocked.is_empty() {
            blocked.sort();
            blocked.dedup();
            return Err(Error::Io(io::Error::other(format!(
                "{}; move it away to restore; nothing was restored",
                blocked.join("; ")
            ))));
        }

        // Stage every file, then put them in place.
        let oids: Vec<Oid> = writes.iter().map(|(_, _, o)| *o).collect();
        let mut staged: Vec<(PathBuf, String)> = Vec::with_capacity(writes.len());
        let staging = self.cat_blobs(&oids, |i, reader, size| {
            let (rel, mode, _) = &writes[i];
            staged.push((self.stage_restored(rel, mode, reader, size)?, rel.clone()));
            Ok(())
        });
        if staging.is_err() || staged.len() != writes.len() {
            for (tmp, rel) in &staged {
                let _ = fs::remove_file(tmp);
                self.remove_empty_parents(&self.workdir.join(rel));
            }
            return staging.and(Err(Error::Git(
                "the backup's contents could not be read; nothing was restored".into(),
            )));
        }
        let mut done = 0usize;
        let partly = |done: usize, err: Error| {
            if done == 0 {
                err
            } else {
                Error::Io(io::Error::other(format!(
                    "the restore stopped after {done} of {} paths ({err}); the rest is unchanged",
                    writes.len() + deletes.len()
                )))
            }
        };
        let mut report = RestoreReport::default();
        for (i, (tmp, rel)) in staged.iter().enumerate() {
            if let Err(err) = self.commit_restored(tmp, rel) {
                for (tmp, _) in &staged[i..] {
                    let _ = fs::remove_file(tmp);
                }
                return Err(partly(done, err));
            }
            done += 1;
            // The content is in place; now the recorded spelling of its name.
            if let Err(now) = self.respell(rel) {
                report.unrenamed.push((now, rel.clone()));
            }
        }
        for rel in &deletes {
            let abs = self.workdir.join(rel);
            // On a case-insensitive or normalising file system `A.txt` (recorded absent) can be
            // the file just restored as `a.txt`: removing it would undo the restore.
            if writes
                .iter()
                .any(|(w, ..)| super::util::same_entry(&abs, &self.workdir.join(w)))
            {
                done += 1;
                continue;
            }
            match fs::remove_file(&abs) {
                Ok(()) => self.remove_empty_parents(&abs),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(partly(done, err.into())),
            }
            done += 1;
        }
        Ok(report)
    }
}
