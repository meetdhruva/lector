//! `review_state`: the "viewed" marks of files in changesets (and, through
//! [`crate::review_queue::HunkReviewStore`], the reviewed hunks, which live in the `kv`
//! keyspace under `review.hunks/...` and are reached through the same [`super::Store`]).
//!
//! Table `review`, keyed by `repo \0 changeset \0 path`, values `{"hash": .., "at": ..}`:
//! the content hash (`lector_review::content_hash`) the file had when it was marked, and the
//! Unix time of the mark. A mark stops counting once the content differs. At most
//! [`MAX_REVIEW_MARKS`] are kept; the oldest go first.

use std::path::Path;

use redb::{ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};

use super::keyspace::{self, Access};
use super::{concerns, StoreError, StoreResult};
use crate::db::{now_nanos, Db, REVIEW};

/// Maximum review marks kept; the oldest are pruned first.
pub const MAX_REVIEW_MARKS: u64 = 20_000;

pub(crate) fn review_key(repo: &str, changeset: &str, path: &str) -> String {
    format!("{repo}\0{changeset}\0{path}")
}

/// One stored mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewMark {
    pub hash: String,
    pub at: u64,
}

/// Viewed marks. Cheap to clone; every clone (and every window) sees the same marks.
#[derive(Clone, Debug)]
pub struct ReviewMarks {
    db: Db,
}

impl ReviewMarks {
    pub(crate) fn new(db: Db) -> Self {
        ReviewMarks { db }
    }

    fn mark(&self, repo: &Path, changeset: &str, path: &str) -> StoreResult<Option<ReviewMark>> {
        let key = review_key(&repo.to_string_lossy(), changeset, path);
        let c = &concerns::REVIEW_STATE;
        keyspace::read(&self.db, c, |tx, access| {
            let Some(t) = keyspace::open_read(tx, REVIEW)? else {
                return Ok(None);
            };
            let bytes = t.get(key.as_str())?.map(|g| g.value().to_vec());
            let Some(bytes) = bytes else { return Ok(None) };
            match c.decode_bytes(access, bytes)? {
                Some(b) => {
                    serde_json::from_slice(&b)
                        .map(Some)
                        .map_err(|source| StoreError::Value {
                            concern: c.name,
                            source,
                        })
                }
                None => Ok(None),
            }
        })
    }

    /// The content hash `path` was marked viewed at, if marked.
    pub fn viewed_hash(&self, repo: &Path, changeset: &str, path: &str) -> Option<String> {
        self.mark(repo, changeset, path)
            .map_err(|e| tracing::warn!("review mark: {e}"))
            .ok()
            .flatten()
            .map(|m| m.hash)
    }

    /// Whether `path` is marked viewed for this changeset *at this content*.
    pub fn is_viewed(&self, repo: &Path, changeset: &str, path: &str, content_hash: &str) -> bool {
        self.viewed_hash(repo, changeset, path)
            .is_some_and(|h| h == content_hash)
    }

    /// Marks / unmarks, in one transaction (pruning the oldest marks beyond the cap).
    pub fn set_viewed(
        &self,
        repo: &Path,
        changeset: &str,
        path: &str,
        content_hash: &str,
        viewed: bool,
    ) -> StoreResult<()> {
        let key = review_key(&repo.to_string_lossy(), changeset, path);
        let mark = ReviewMark {
            hash: content_hash.to_string(),
            at: now_nanos() / 1_000_000_000,
        };
        let bytes = serde_json::to_vec(&mark).map_err(|source| StoreError::Value {
            concern: concerns::REVIEW_STATE.name,
            source,
        })?;
        keyspace::write(&self.db, &concerns::REVIEW_STATE, |tx| {
            let mut t = tx.open_table(REVIEW)?;
            if !viewed {
                t.remove(key.as_str())?;
                return Ok(());
            }
            t.insert(key.as_str(), bytes.as_slice())?;
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
            Ok(())
        })
    }

    /// Number of stored marks.
    pub fn len(&self) -> usize {
        let count = keyspace::read(&self.db, &concerns::REVIEW_STATE, |tx, access| {
            if matches!(access, Access::Newer(_)) {
                return Ok(0);
            }
            Ok(match keyspace::open_read(tx, REVIEW)? {
                Some(t) => t.len()?,
                None => 0,
            })
        });
        count.unwrap_or(0) as usize
    }

    /// True when no marks are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
