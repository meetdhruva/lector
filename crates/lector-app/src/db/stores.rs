//! Database-backed replacements for the v0.3 JSON stores of `lector-config`
//! (`ReviewStore` backend, `SessionStore`, `RecentProjects`), with the same methods.

use std::path::{Path, PathBuf};

use lector_config::Session;
use serde::{Deserialize, Serialize};

use super::{now_nanos, Db, DbResult, RECENT, REVIEW, SESSIONS};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

/// Maximum review marks kept; the oldest are pruned first.
pub const MAX_REVIEW_MARKS: u64 = 20_000;
/// Maximum recent projects (same as v0.3).
pub const MAX_RECENT: usize = lector_config::MAX_RECENT_PROJECTS;

pub(super) fn review_key(repo: &str, changeset: &str, path: &str) -> String {
    format!("{repo}\0{changeset}\0{path}")
}

#[derive(Serialize, Deserialize)]
pub(super) struct ReviewMark {
    pub hash: String,
    pub at: u64,
}

fn io(err: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

/// [`lector_config::ReviewBackend`] over the `review` table.
#[derive(Clone, Debug)]
pub struct DbReviewBackend {
    db: Db,
}

impl DbReviewBackend {
    /// Backend over `db`.
    pub fn new(db: Db) -> Self {
        DbReviewBackend { db }
    }

    fn prune(&self) -> DbResult<()> {
        let tx = self.db.inner.db.begin_write()?;
        {
            let mut t = tx.open_table(REVIEW)?;
            let len = t.len()?;
            if len > MAX_REVIEW_MARKS {
                let mut all = Vec::new();
                for item in t.iter()? {
                    let (k, v) = item?;
                    let at = serde_json::from_slice::<ReviewMark>(v.value())
                        .map(|m| m.at)
                        .unwrap_or(0);
                    all.push((at, k.value().to_string()));
                }
                all.sort();
                for (_, k) in all.into_iter().take((len - MAX_REVIEW_MARKS) as usize) {
                    t.remove(k.as_str())?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

impl lector_config::ReviewBackend for DbReviewBackend {
    fn get(&self, repo: &str, changeset: &str, path: &str) -> Option<String> {
        let bytes = self
            .db
            .get_bytes(REVIEW, &review_key(repo, changeset, path))
            .map_err(|e| tracing::warn!("review mark: {e}"))
            .ok()??;
        serde_json::from_slice::<ReviewMark>(&bytes)
            .ok()
            .map(|m| m.hash)
    }

    fn set(
        &self,
        repo: &str,
        changeset: &str,
        path: &str,
        hash: Option<&str>,
    ) -> std::io::Result<()> {
        let key = review_key(repo, changeset, path);
        match hash {
            Some(h) => {
                let v = serde_json::to_vec(&ReviewMark {
                    hash: h.to_string(),
                    at: now_nanos() / 1_000_000_000,
                })
                .map_err(io)?;
                self.db.put_bytes(REVIEW, &key, Some(&v)).map_err(io)?;
                if self.len() as u64 > MAX_REVIEW_MARKS {
                    self.prune().map_err(io)?;
                }
                Ok(())
            }
            None => self.db.put_bytes(REVIEW, &key, None).map_err(io),
        }
    }

    fn len(&self) -> usize {
        self.db.table_len(REVIEW).unwrap_or(0) as usize
    }
}

/// Sessions per project root (replaces `lector_config::SessionStore`).
#[derive(Clone, Debug)]
pub struct DbSessionStore {
    db: Db,
}

impl DbSessionStore {
    /// Store over `db`.
    pub fn new(db: Db) -> Self {
        DbSessionStore { db }
    }

    /// The session for `root`; `None` if missing or unreadable.
    pub fn load(&self, root: &Path) -> Option<Session> {
        let bytes = self
            .db
            .get_bytes(SESSIONS, &lector_config::session_key(root))
            .map_err(|e| tracing::warn!("session: {e}"))
            .ok()??;
        match serde_json::from_slice(&bytes) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("ignoring unreadable session: {e}");
                None
            }
        }
    }

    /// Saves the session for `root`.
    pub fn save(&self, root: &Path, session: &Session) -> std::io::Result<()> {
        let json = serde_json::to_vec(session).map_err(io)?;
        self.db
            .put_bytes(SESSIONS, &lector_config::session_key(root), Some(&json))
            .map_err(io)
    }
}

/// Recently opened project roots, newest first (replaces `lector_config::RecentProjects`).
#[derive(Clone, Debug)]
pub struct DbRecentProjects {
    db: Db,
}

impl DbRecentProjects {
    /// Store over `db`.
    pub fn new(db: Db) -> Self {
        DbRecentProjects { db }
    }

    fn entries(&self) -> DbResult<Vec<(u64, PathBuf)>> {
        let tx = self.db.inner.db.begin_read()?;
        let t = tx.open_table(RECENT)?;
        let mut out = Vec::new();
        for item in t.iter()? {
            let (k, v) = item?;
            out.push((v.value(), PathBuf::from(k.value())));
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.0));
        Ok(out)
    }

    /// Recently opened roots, newest first (at most [`MAX_RECENT`]).
    pub fn list(&self) -> Vec<PathBuf> {
        self.entries()
            .map_err(|e| tracing::warn!("recent projects: {e}"))
            .unwrap_or_default()
            .into_iter()
            .take(MAX_RECENT)
            .map(|(_, p)| p)
            .collect()
    }

    /// The newest root that still exists as a directory.
    pub fn most_recent_existing(&self) -> Option<PathBuf> {
        self.list().into_iter().find(|p| p.is_dir())
    }

    /// Moves `root` to the front (deduplicated, capped).
    pub fn record(&self, root: &Path) -> std::io::Result<()> {
        let run = || -> DbResult<()> {
            let tx = self.db.inner.db.begin_write()?;
            {
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
                for (_, k) in all.into_iter().skip(MAX_RECENT - 1) {
                    t.remove(k.as_str())?;
                }
            }
            tx.commit()?;
            Ok(())
        };
        run().map_err(io)
    }

    /// Forgets `root`. Missing entries are fine.
    pub fn remove(&self, root: &Path) -> std::io::Result<()> {
        let key = root.to_string_lossy();
        let run = || -> DbResult<()> {
            let tx = self.db.inner.db.begin_write()?;
            {
                let mut t = tx.open_table(RECENT)?;
                t.remove(key.as_ref())?;
            }
            tx.commit()?;
            Ok(())
        };
        run().map_err(io)
    }
}
