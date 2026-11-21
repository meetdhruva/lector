//! Housekeeping of the shadow repository: deleting snapshot refs, `git gc` and pack consolidation.

use std::collections::BTreeSet;
use std::fs;

use crate::error::Result;
use crate::Oid;

use super::command::exec;
use super::{ShadowRepo, ShadowSnapshot, PACK_CONSOLIDATE_AT, REF_PREFIX};

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
