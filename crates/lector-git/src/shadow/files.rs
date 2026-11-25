//! Path-scoped snapshots of named files only: never a directory walk, never a FIFO or device
//! opened, never a file over a size limit stored (ADR 0011).

use std::{fs, io};

use crate::error::{Error, Result};

use super::capture::too_large;
use super::util::{check_rel, is_executable};
use super::{FileKind, Oversize, Plan, PlannedFile, ShadowRepo, ShadowSnapshot, SnapshotKind};

impl ShadowRepo {
    /// Records the working-tree contents of exactly the files `paths` name (exact bytes,
    /// executable bit, symlinks as links, absent paths as absent), like
    /// [`ShadowRepo::snapshot_paths`] but bounded: a directory is never walked and a FIFO,
    /// socket or device never opened, and a file larger than `max_file` (also when it grows past
    /// it before it is read) is never stored. What `oversize` says happens to such a path:
    /// [`Oversize::Refuse`] fails the snapshot (nothing recorded), [`Oversize::Skip`] records an
    /// oversized file as skipped without reading it (a restore of it then refuses) and fails for
    /// the rest, [`Oversize::SkipAll`] records every such path as skipped. Writes only to the
    /// shadow.
    pub fn snapshot_files(
        &self,
        paths: &[&str],
        max_file: u64,
        oversize: Oversize,
        note: &str,
    ) -> Result<ShadowSnapshot> {
        let mut plan = Plan::new(SnapshotKind::Paths);
        plan.max_file = max_file;
        plan.oversize = oversize;
        for path in paths {
            let rel = check_rel(path)?;
            if let Err(err) = self.no_symlink_parents(&rel) {
                // Reached through a link now: never followed. Recording the end of a turn must
                // not fail on it (that would drop the whole turn's undo): it is skipped, unread.
                match oversize {
                    Oversize::SkipAll => {
                        plan.skipped.push(rel);
                        continue;
                    }
                    Oversize::Refuse | Oversize::Skip => return Err(err),
                }
            }
            let meta = match fs::symlink_metadata(self.workdir.join(&rel)) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    plan.absent.push(rel);
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            let ty = meta.file_type();
            if ty.is_symlink() {
                plan.files.push(PlannedFile {
                    rel,
                    kind: FileKind::Link,
                    size: 0,
                });
            } else if ty.is_file() && meta.len() <= max_file {
                plan.files.push(PlannedFile {
                    rel,
                    kind: FileKind::File {
                        executable: is_executable(&meta),
                    },
                    size: meta.len(),
                });
            } else {
                match oversize {
                    Oversize::SkipAll => plan.skipped.push(rel),
                    Oversize::Skip if ty.is_file() => plan.skipped.push(rel),
                    Oversize::Refuse if ty.is_file() => return Err(too_large(&rel, max_file)),
                    Oversize::Refuse | Oversize::Skip => {
                        return Err(Error::Io(io::Error::other(format!(
                            "{rel} is not a regular file (a directory, FIFO, socket or device); only files are backed up here"
                        ))))
                    }
                }
            }
        }
        self.write_snapshot(plan, note)
    }

    /// Records `paths` as absent without looking at the work tree: for a file an agent turn
    /// created, whose name the file system stored differently from the spelling requested
    /// (case, Unicode normalisation), so the turn's "did not exist" is keyed by the stored name.
    /// Writes only to the shadow.
    pub fn record_absent(&self, paths: &[&str], note: &str) -> Result<ShadowSnapshot> {
        let mut plan = Plan::new(SnapshotKind::Paths);
        for path in paths {
            plan.absent.push(check_rel(path)?);
        }
        self.write_snapshot(plan, note)
    }
}
