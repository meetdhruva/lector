//! Most-recently opened project roots (`<data_dir>/recent.json`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic::write_atomic;

/// Maximum number of remembered projects.
pub const MAX_RECENT_PROJECTS: usize = 20;

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentFile {
    #[serde(default)]
    projects: Vec<PathBuf>,
}

/// Stores the list of recently opened project roots, newest first.
#[derive(Debug, Clone)]
pub struct RecentProjects {
    file: PathBuf,
}

impl Default for RecentProjects {
    fn default() -> Self {
        Self::new()
    }
}

impl RecentProjects {
    /// Store at `<data_dir>/recent.json`.
    pub fn new() -> Self {
        Self::with_file(crate::paths::data_dir().join("recent.json"))
    }

    /// Store at an explicit file (tests).
    pub fn with_file(file: impl Into<PathBuf>) -> Self {
        Self { file: file.into() }
    }

    /// Recently opened roots, newest first. A missing or corrupt file yields an empty list.
    pub fn list(&self) -> Vec<PathBuf> {
        std::fs::read(&self.file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RecentFile>(&bytes).ok())
            .map(|f| f.projects)
            .unwrap_or_default()
    }

    /// The newest root that still exists as a directory.
    pub fn most_recent_existing(&self) -> Option<PathBuf> {
        self.list().into_iter().find(|p| p.is_dir())
    }

    /// Moves `root` to the front of the list (deduplicated, capped).
    pub fn record(&self, root: &Path) -> std::io::Result<()> {
        let mut projects = self.list();
        projects.retain(|p| p != root);
        projects.insert(0, root.to_path_buf());
        projects.truncate(MAX_RECENT_PROJECTS);
        let bytes =
            serde_json::to_vec_pretty(&RecentFile { projects }).map_err(std::io::Error::other)?;
        write_atomic(&self.file, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_newest_first_dedup_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecentProjects::with_file(dir.path().join("recent.json"));
        assert!(store.list().is_empty());
        for i in 0..25 {
            store.record(&dir.path().join(format!("p{i}"))).unwrap();
        }
        store.record(&dir.path().join("p3")).unwrap();
        let list = store.list();
        assert_eq!(list.len(), MAX_RECENT_PROJECTS);
        assert_eq!(list[0], dir.path().join("p3"));
        assert_eq!(list.iter().filter(|p| p.ends_with("p3")).count(), 1);
        assert_eq!(store.most_recent_existing(), None);
        std::fs::create_dir(dir.path().join("p24")).unwrap();
        assert_eq!(store.most_recent_existing(), Some(dir.path().join("p24")));
    }

    #[test]
    fn corrupt_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recent.json");
        std::fs::write(&file, b"{not json").unwrap();
        assert!(RecentProjects::with_file(&file).list().is_empty());
    }
}
