use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{atomic::write_atomic, paths};

/// Maximum entries kept on disk; the oldest are pruned first.
const MAX_ENTRIES: usize = 5000;

/// Hex SHA-256 of file content, for [`ReviewStore`].
pub fn content_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    repo: String,
    changeset: String,
    path: String,
    hash: String,
    at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct FileData {
    #[serde(default)]
    entries: Vec<Entry>,
}

type Key = (String, String, String);

struct Inner {
    file: PathBuf,
    map: HashMap<Key, Entry>,
}

/// Storage behind a [`ReviewStore`] other than the v0.3 JSON file (v0.4: the
/// `lector-app` database). Keys are (repo root, changeset key, path).
pub trait ReviewBackend: Send + Sync {
    /// The content hash recorded for the key, if marked viewed.
    fn get(&self, repo: &str, changeset: &str, path: &str) -> Option<String>;
    /// Records (`Some(hash)`) or clears (`None`) a mark.
    fn set(
        &self,
        repo: &str,
        changeset: &str,
        path: &str,
        hash: Option<&str>,
    ) -> std::io::Result<()>;
    /// Number of stored marks.
    fn len(&self) -> usize;
    /// True when no marks are stored.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// "Viewed" marks for files in a changeset. A mark records the content hash
/// and stops counting once the content differs.
///
/// Since v0.4 the application stores marks in its database
/// ([`ReviewStore::with_backend`], built by `lector_app::db::Db::review_store`); the
/// JSON file ([`ReviewStore::open`]) remains for tests and as the v0.3 import source.
#[derive(Clone)]
pub struct ReviewStore {
    inner: Arc<Mutex<Inner>>,
    backend: Option<Arc<dyn ReviewBackend>>,
}

impl Default for ReviewStore {
    /// The v0.3 JSON store at `data_dir()/review.json` (deprecated in v0.4: prefer
    /// the database-backed store from `lector_app`).
    fn default() -> Self {
        Self::open(paths::review_file())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn key(repo: &Path, changeset: &str, path: &str) -> Key {
    (
        repo.to_string_lossy().into_owned(),
        changeset.to_string(),
        path.to_string(),
    )
}

impl ReviewStore {
    /// Open (or lazily create) the store at `file`. Corrupt files start empty.
    pub fn open(file: impl Into<PathBuf>) -> Self {
        let file = file.into();
        let data: FileData = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let map = data
            .entries
            .into_iter()
            .map(|e| ((e.repo.clone(), e.changeset.clone(), e.path.clone()), e))
            .collect();
        Self {
            inner: Arc::new(Mutex::new(Inner { file, map })),
            backend: None,
        }
    }

    /// A store over another backend (v0.4 database).
    pub fn with_backend(backend: Arc<dyn ReviewBackend>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                file: PathBuf::new(),
                map: HashMap::new(),
            })),
            backend: Some(backend),
        }
    }

    /// Whether `path` is marked viewed for this changeset *at this content*.
    pub fn is_viewed(&self, repo: &Path, changeset: &str, path: &str, content_hash: &str) -> bool {
        if let Some(b) = &self.backend {
            return b
                .get(&repo.to_string_lossy(), changeset, path)
                .is_some_and(|h| h == content_hash);
        }
        self.inner
            .lock()
            .map
            .get(&key(repo, changeset, path))
            .is_some_and(|e| e.hash == content_hash)
    }

    /// Mark / unmark and persist.
    pub fn set_viewed(
        &self,
        repo: &Path,
        changeset: &str,
        path: &str,
        content_hash: &str,
        viewed: bool,
    ) -> std::io::Result<()> {
        if let Some(b) = &self.backend {
            let hash = viewed.then_some(content_hash);
            return b.set(&repo.to_string_lossy(), changeset, path, hash);
        }
        let mut g = self.inner.lock();
        let k = key(repo, changeset, path);
        if viewed {
            g.map.insert(
                k.clone(),
                Entry {
                    repo: k.0,
                    changeset: k.1,
                    path: k.2,
                    hash: content_hash.to_string(),
                    at: now(),
                },
            );
        } else {
            g.map.remove(&k);
        }
        if g.map.len() > MAX_ENTRIES {
            let mut all: Vec<_> = g.map.iter().map(|(k, e)| (e.at, k.clone())).collect();
            all.sort();
            let excess = g.map.len() - MAX_ENTRIES;
            for (_, k) in all.into_iter().take(excess) {
                g.map.remove(&k);
            }
        }
        let data = FileData {
            entries: g.map.values().cloned().collect(),
        };
        let json = serde_json::to_vec(&data).map_err(std::io::Error::other)?;
        write_atomic(&g.file, &json)
    }

    /// Number of stored marks.
    pub fn len(&self) -> usize {
        if let Some(b) = &self.backend {
            return b.len();
        }
        self.inner.lock().map.len()
    }

    /// True when no marks are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewed_resets_on_change_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sub/review.json");
        let repo = Path::new("/r");
        let h1 = content_hash(b"one");
        let h2 = content_hash(b"two");
        let s = ReviewStore::open(&f);
        assert!(!s.is_viewed(repo, "wt", "a.rs", &h1));
        s.set_viewed(repo, "wt", "a.rs", &h1, true).unwrap();
        assert!(s.is_viewed(repo, "wt", "a.rs", &h1));
        assert!(!s.is_viewed(repo, "wt", "a.rs", &h2));
        assert!(!s.is_viewed(repo, "other", "a.rs", &h1));
        let s2 = ReviewStore::open(&f);
        assert!(s2.is_viewed(repo, "wt", "a.rs", &h1));
        s2.set_viewed(repo, "wt", "a.rs", &h1, false).unwrap();
        assert!(!ReviewStore::open(&f).is_viewed(repo, "wt", "a.rs", &h1));
    }

    #[test]
    fn bounded_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("review.json");
        std::fs::write(&f, "garbage").unwrap();
        let s = ReviewStore::open(&f);
        assert!(s.is_empty());
        for i in 0..MAX_ENTRIES + 10 {
            let mut g = s.inner.lock();
            let k = key(Path::new("/r"), "c", &i.to_string());
            g.map.insert(
                k.clone(),
                Entry {
                    repo: k.0,
                    changeset: k.1,
                    path: k.2,
                    hash: "h".into(),
                    at: i as u64,
                },
            );
        }
        s.set_viewed(Path::new("/r"), "c", "new", "h", true)
            .unwrap();
        assert_eq!(s.len(), MAX_ENTRIES);
        assert!(s.is_viewed(Path::new("/r"), "c", "new", "h"));
        assert!(!s.is_viewed(Path::new("/r"), "c", "0", "h"));
    }
}
