//! Terminal session restore: which tabs (and splits) were open, in which folder.
//!
//! Stored in Lector's own data directory (`<data dir>/terminal/sessions.json`), keyed by
//! project root. Only folders and custom tab names are kept: no scrollback, no commands.
//! Restoring starts fresh shells; nothing is run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One saved tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SavedTab {
    /// Working directory of each split, in order.
    pub cwds: Vec<PathBuf>,
    /// Name given with Rename.
    pub title: Option<String>,
    /// Splits are stacked vertically.
    pub vertical: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SessionFile {
    version: u32,
    projects: BTreeMap<String, Vec<SavedTab>>,
}

/// Most tabs restored per project (a corrupt or hostile file cannot spawn hundreds of shells).
pub const MAX_RESTORED_TABS: usize = 12;
/// Most splits restored per tab.
pub const MAX_RESTORED_SPLITS: usize = 4;

/// The session file.
pub fn session_path() -> PathBuf {
    lector_config::paths::data_dir()
        .join("terminal")
        .join("sessions.json")
}

fn read(path: &Path) -> SessionFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Saved tabs of `root` (empty when none or unreadable), capped.
pub fn load(path: &Path, root: &Path) -> Vec<SavedTab> {
    let mut tabs = read(path)
        .projects
        .remove(&root.to_string_lossy().into_owned())
        .unwrap_or_default();
    tabs.truncate(MAX_RESTORED_TABS);
    for tab in &mut tabs {
        tab.cwds.truncate(MAX_RESTORED_SPLITS);
    }
    tabs.retain(|t| !t.cwds.is_empty());
    tabs
}

/// Replaces the saved tabs of `root` (removes the entry when `tabs` is empty). Atomic
/// (temp file + rename) so a crash cannot leave half a file.
pub fn store(path: &Path, root: &Path, tabs: &[SavedTab]) -> std::io::Result<()> {
    let mut file = read(path);
    file.version = 1;
    let key = root.to_string_lossy().into_owned();
    if tabs.is_empty() {
        file.projects.remove(&key);
    } else {
        file.projects.insert(key, tabs.to_vec());
    }
    let text = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t").join("sessions.json");
        assert!(load(&path, Path::new("/a")).is_empty());
        let tabs = vec![
            SavedTab {
                cwds: vec!["/a".into(), "/a/src".into()],
                title: Some("build".into()),
                vertical: true,
            },
            SavedTab {
                cwds: vec!["/a/docs".into()],
                ..SavedTab::default()
            },
        ];
        store(&path, Path::new("/a"), &tabs).unwrap();
        store(&path, Path::new("/b"), &tabs[1..]).unwrap();
        assert_eq!(load(&path, Path::new("/a")), tabs);
        assert_eq!(load(&path, Path::new("/b")), tabs[1..].to_vec());
        store(&path, Path::new("/a"), &[]).unwrap();
        assert!(load(&path, Path::new("/a")).is_empty());
        assert_eq!(load(&path, Path::new("/b")).len(), 1);
        // Garbage is ignored; huge sessions are capped.
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path, Path::new("/b")).is_empty());
        let many: Vec<SavedTab> = (0..50)
            .map(|_| SavedTab {
                cwds: vec!["/x".into(); 9],
                ..SavedTab::default()
            })
            .collect();
        store(&path, Path::new("/c"), &many).unwrap();
        let loaded = load(&path, Path::new("/c"));
        assert_eq!(loaded.len(), MAX_RESTORED_TABS);
        assert!(loaded.iter().all(|t| t.cwds.len() == MAX_RESTORED_SPLITS));
    }
}
