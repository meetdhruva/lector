use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::{atomic::write_atomic, paths};

/// What a tab shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabKind {
    /// A file in the working tree.
    File {
        /// Absolute path.
        path: PathBuf,
    },
    /// A file's diff within a changeset.
    Diff {
        /// Stable string key of the changeset.
        changeset_key: String,
        /// Path of the changed file.
        path: String,
    },
    /// A commit's detail view.
    Commit {
        /// Full hex object id.
        oid: String,
    },
    /// Anything else, identified by a label (e.g. "settings").
    Other {
        /// View identifier.
        label: String,
    },
}

/// One tab.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TabState {
    /// Content.
    pub kind: TabKind,
    /// First visible row.
    pub scroll_row: u32,
    /// Whether it is the selected tab of its pane.
    pub active: bool,
}

impl Default for TabKind {
    fn default() -> Self {
        TabKind::Other {
            label: String::new(),
        }
    }
}

/// Split direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Panes side by side.
    #[default]
    Horizontal,
    /// Panes stacked.
    Vertical,
}

/// One pane and its tabs.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneState {
    /// Tabs left to right.
    pub tabs: Vec<TabState>,
}

/// Window position and size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowBounds {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
    /// Maximized.
    pub maximized: bool,
}

/// Restorable UI state for one project root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Panes (one unless split) with their tabs.
    pub panes: Vec<PaneState>,
    /// Arrangement of `panes`.
    pub axis: Axis,
    /// Index into `panes` of the focused pane.
    pub active_pane: usize,
    /// Sidebar shown.
    pub sidebar_visible: bool,
    /// Sidebar width in px.
    pub sidebar_width: f32,
    /// Selected sidebar tab (`files`, `changes`, `history`, `branches`).
    pub sidebar_tab: String,
    /// Last window bounds.
    pub window: Option<WindowBounds>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            panes: vec![PaneState::default()],
            axis: Axis::Horizontal,
            active_pane: 0,
            sidebar_visible: true,
            sidebar_width: 260.0,
            sidebar_tab: "files".into(),
            window: None,
        }
    }
}

/// Persists [`Session`]s as `<dir>/<hash of canonical root>.json`.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    /// Store under `data_dir()/sessions`.
    pub fn new() -> Self {
        Self {
            dir: paths::sessions_dir(),
        }
    }

    /// Store in an explicit directory.
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn file_for(&self, root: &Path) -> PathBuf {
        let canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{hex}.json"))
    }

    /// Load the session for `root`; `None` if missing or corrupt.
    pub fn load(&self, root: &Path) -> Option<Session> {
        let bytes = std::fs::read(self.file_for(root)).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("ignoring corrupt session file: {e}");
                None
            }
        }
    }

    /// Atomically save the session for `root`.
    pub fn save(&self, root: &Path, session: &Session) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(session).map_err(std::io::Error::other)?;
        write_atomic(&self.file_for(root), &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::with_dir(dir.path().join("sessions"));
        let root = dir.path();
        assert!(store.load(root).is_none());
        let s = Session {
            panes: vec![
                PaneState {
                    tabs: vec![
                        TabState {
                            kind: TabKind::File {
                                path: "/a/b.rs".into(),
                            },
                            scroll_row: 42,
                            active: true,
                        },
                        TabState {
                            kind: TabKind::Diff {
                                changeset_key: "wt".into(),
                                path: "x".into(),
                            },
                            scroll_row: 0,
                            active: false,
                        },
                    ],
                },
                PaneState {
                    tabs: vec![TabState {
                        kind: TabKind::Commit { oid: "ab".into() },
                        ..Default::default()
                    }],
                },
            ],
            axis: Axis::Vertical,
            active_pane: 1,
            window: Some(WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0,
                maximized: true,
            }),
            ..Default::default()
        };
        store.save(root, &s).unwrap();
        assert_eq!(store.load(root), Some(s));
        std::fs::write(store.file_for(root), b"{ not json").unwrap();
        assert!(store.load(root).is_none());
        store.save(root, &Session::default()).unwrap();
        assert_eq!(store.load(root), Some(Session::default()));
    }

    #[test]
    fn distinct_roots_distinct_files() {
        let store = SessionStore::with_dir("/nonexistent");
        assert_ne!(
            store.file_for(Path::new("/a")),
            store.file_for(Path::new("/b"))
        );
        assert_eq!(
            store.file_for(Path::new("/a")),
            store.file_for(Path::new("/a"))
        );
    }
}
