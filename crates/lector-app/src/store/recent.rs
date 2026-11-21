//! `recent_projects`: recently opened project roots, newest first.
//!
//! Table `recent`: root path → write stamp (nanoseconds; the newest wins). Written in one
//! transaction per change (dedup and cap included).

use std::path::{Path, PathBuf};

use redb::ReadableTable;

use super::keyspace::{self, Access};
use super::{concerns, StoreResult};
use crate::db::{now_nanos, Db, RECENT};

/// Most remembered projects.
pub const MAX_RECENT_PROJECTS: usize = 20;

/// Recently opened project roots, newest first. Cheap to clone.
#[derive(Clone, Debug)]
pub struct RecentProjects {
    db: Db,
}

impl RecentProjects {
    pub(crate) fn new(db: Db) -> Self {
        RecentProjects { db }
    }

    fn entries(&self) -> StoreResult<Vec<(u64, PathBuf)>> {
        keyspace::read(&self.db, &concerns::RECENT_PROJECTS, |tx, access| {
            if matches!(access, Access::Newer(_)) {
                return Ok(Vec::new());
            }
            let t = match tx.open_table(RECENT) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            let mut out = Vec::new();
            for item in t.iter()? {
                let (k, v) = item?;
                out.push((v.value(), PathBuf::from(k.value())));
            }
            out.sort_by_key(|e| std::cmp::Reverse(e.0));
            Ok(out)
        })
    }

    /// Recently opened roots, newest first (at most [`MAX_RECENT_PROJECTS`]).
    pub fn list(&self) -> Vec<PathBuf> {
        self.entries()
            .map_err(|e| tracing::warn!("recent projects: {e}"))
            .unwrap_or_default()
            .into_iter()
            .take(MAX_RECENT_PROJECTS)
            .map(|(_, p)| p)
            .collect()
    }

    /// The newest root that still exists as a directory.
    pub fn most_recent_existing(&self) -> Option<PathBuf> {
        self.list().into_iter().find(|p| p.is_dir())
    }

    /// Moves `root` to the front (deduplicated, capped), in one transaction.
    pub fn record(&self, root: &Path) -> StoreResult<()> {
        keyspace::write(&self.db, &concerns::RECENT_PROJECTS, |tx| {
            let mut t = tx.open_table(RECENT)?;
            let mut newest = 0;
            let mut all = Vec::new();
            for item in t.iter()? {
                let (k, v) = item?;
                newest = newest.max(v.value());
                all.push((v.value(), k.value().to_string()));
            }
            let stamp = now_nanos().max(newest + 1);
            let key = root.to_string_lossy();
            t.insert(key.as_ref(), stamp)?;
            all.retain(|(_, k)| k.as_str() != key.as_ref());
            all.sort_by_key(|e| std::cmp::Reverse(e.0));
            for (_, k) in all.into_iter().skip(MAX_RECENT_PROJECTS - 1) {
                t.remove(k.as_str())?;
            }
            Ok(())
        })
    }

    /// Forgets `root`. Missing entries are fine.
    pub fn remove(&self, root: &Path) -> StoreResult<()> {
        let key = root.to_string_lossy();
        keyspace::write(&self.db, &concerns::RECENT_PROJECTS, |tx| {
            tx.open_table(RECENT)?.remove(key.as_ref())?;
            Ok(())
        })
    }
}
