//! Housekeeping of the shadow repository: deleting snapshot refs, `git gc` and pack consolidation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use crate::error::Result;
use crate::Oid;

use super::command::exec;
use super::{now_secs, ShadowRepo, ShadowSnapshot, PACK_CONSOLIDATE_AT, REF_PREFIX};

impl ShadowRepo {
    /// Deletes the shadow repositories below `data_dir` whose work tree is gone (the folder or
    /// repository was deleted or renamed: its canonical path, and so its key, changed) once their
    /// newest snapshot is older than `keep`, the same age at which [`ShadowRepo::prune`] would
    /// have removed every snapshot. A shadow whose work tree exists is left to `prune` (it is
    /// trimmed when opened); one in use (locked) or without the `lector-workdir` marker is left
    /// alone. Returns how many were deleted. Only Lector's data directory is touched.
    pub fn prune_orphans(data_dir: &Path, keep: Duration) -> Result<usize> {
        let dir = data_dir.join("shadow");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        let cutoff = now_secs() - keep.as_secs() as i64;
        let mut removed = 0;
        for entry in entries.filter_map(|e| e.ok()) {
            let git_dir = entry.path();
            let is_dir = fs::symlink_metadata(&git_dir).is_ok_and(|m| m.is_dir());
            if !is_dir || git_dir.extension().is_none_or(|x| x != "git") {
                continue;
            }
            let Ok(marker) = fs::read_to_string(git_dir.join("lector-workdir")) else {
                continue;
            };
            let workdir = PathBuf::from(marker.trim_end_matches(['\n', '\r']));
            if workdir.as_os_str().is_empty()
                || fs::symlink_metadata(&workdir).is_ok()
                || git_dir.join("lector.lock").exists()
            {
                continue;
            }
            let orphan = ShadowRepo {
                git_dir: git_dir.clone(),
                workdir,
                folder: true,
            };
            let newest = orphan.snapshots()?.first().map(|s| s.time);
            if newest.is_none_or(|t| t <= cutoff) {
                fs::remove_dir_all(&git_dir)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

impl ShadowRepo {
    pub(super) fn delete_snapshots(&self, snaps: &[ShadowSnapshot]) -> Result<usize> {
        if snaps.is_empty() {
            return Ok(0);
        }
        let wanted: BTreeSet<Oid> = snaps.iter().map(|s| s.commit).collect();
        let out = exec(
            {
                let mut c = self.shadow_git();
                c.args([
                    "for-each-ref",
                    "--format=%(objectname) %(refname)",
                    REF_PREFIX,
                ]);
                c
            },
            "git for-each-ref",
            None,
        )?;
        let mut input = String::new();
        let mut count = 0;
        for line in String::from_utf8_lossy(&out).lines() {
            let Some((oid, name)) = line.split_once(' ') else {
                continue;
            };
            if Oid::from_hex(oid).is_some_and(|o| wanted.contains(&o)) {
                input.push_str(&format!("delete {name}\n"));
                count += 1;
            }
        }
        if !input.is_empty() {
            let mut c = self.shadow_git();
            c.args(["update-ref", "--stdin"]);
            exec(c, "git update-ref", Some(input.as_bytes()))?;
        }
        Ok(count)
    }

    /// Repack local objects, drop unreachable ones. Callers hold the lock.
    pub(super) fn gc(&self) -> Result<()> {
        let mut c = self.shadow_git();
        c.args(["gc", "--prune=now", "--quiet"]);
        exec(c, "git gc", None).map(drop)
    }

    /// Every snapshot adds one small pack (see `fastimport.unpackLimit`); merge them
    /// once there are many so lookups stay fast. Best effort.
    pub(super) fn consolidate_if_needed(&self) {
        let packs = fs::read_dir(self.git_dir.join("objects").join("pack"))
            .map(|dir| {
                dir.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                    .count()
            })
            .unwrap_or(0);
        if packs >= PACK_CONSOLIDATE_AT {
            let mut c = self.shadow_git();
            c.args(["repack", "-a", "-d", "-l", "-q"]);
            let _ = exec(c, "git repack", None);
        }
    }
}
