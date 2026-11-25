//! Combining snapshots: one snapshot whose entries are the first record of each path among
//! several (an agent turn's "before" state is the set of its first-touch backups).

use std::collections::BTreeSet;

use crate::error::{Error, Result};

use super::{Plan, ShadowRepo, ShadowSnapshot, SnapshotKind};

impl ShadowRepo {
    /// Records one snapshot that holds, for every path any of `parts` covers, the entry of the
    /// first part that covers it (contents, mode, link, absence or "skipped"). No file is read:
    /// the blobs are already in the shadow. The result is a whole-tree snapshot when any part
    /// is one (paths no part records are then absent), else a `Paths` snapshot. Index snapshots
    /// cannot be merged. Writes only to the shadow repository.
    pub fn merge_snapshots(&self, parts: &[&ShadowSnapshot], note: &str) -> Result<ShadowSnapshot> {
        if parts.is_empty() {
            return Err(Error::Parse("no snapshots to merge".into()));
        }
        let mut manifests = Vec::with_capacity(parts.len());
        for part in parts {
            let manifest = self.manifest(part)?;
            if manifest.kind == SnapshotKind::Index {
                return Err(Error::Parse("index snapshots cannot be merged".into()));
            }
            manifests.push(manifest);
        }
        let whole = manifests.iter().any(|m| m.kind == SnapshotKind::Worktree);
        let mut plan = Plan::new(if whole {
            SnapshotKind::Worktree
        } else {
            SnapshotKind::Paths
        });
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (part, manifest) in parts.iter().zip(manifests) {
            for (path, (mode, oid)) in self.tree_files(part)? {
                if seen.insert(path.clone()) {
                    plan.recorded.push((path, mode, oid));
                }
            }
            for path in manifest.absent {
                if seen.insert(path.clone()) {
                    plan.absent.push(path);
                }
            }
            for path in manifest.skipped {
                if seen.insert(path.clone()) {
                    plan.skipped.push(path);
                }
            }
        }
        self.write_snapshot(plan, note)
    }
}
