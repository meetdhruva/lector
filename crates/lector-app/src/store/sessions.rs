//! `window_sessions`: restorable window state (panes, tabs, sidebar, bounds) per project root.
//!
//! Table `sessions`, keyed by [`session_key`] (16 hex characters of the SHA-256 of the
//! canonical root, the same key as the v0.3 file stem `sessions/<key>.json` and the
//! database rows of 0.4 to 0.7.1), values are [`Session`] as JSON.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::keyspace::{Concern, Keyspace};
use super::{concerns, StoreResult};
use crate::db::Db;

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
    /// A file tab locked read-only (opened from a link that points outside the project): it
    /// comes back locked after a restart.
    pub read_only_locked: bool,
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

/// Stable key of a project root for session storage: 16 hex chars of the SHA-256 of
/// the canonical root (also the v0.3 file stem `sessions/<key>.json`).
pub fn session_key(root: &Path) -> String {
    let canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Window sessions per project root. Cheap to clone.
#[derive(Clone, Debug)]
pub struct WindowSessions {
    keyspace: Keyspace<Session>,
}

impl WindowSessions {
    pub(crate) fn new(db: Db) -> Self {
        WindowSessions {
            keyspace: Keyspace::new(db, &concerns::WINDOW_SESSIONS),
        }
    }

    /// The concern.
    pub fn concern(&self) -> &'static Concern {
        self.keyspace.concern()
    }

    /// The session for `root`; `None` if missing or unreadable (logged).
    pub fn load(&self, root: &Path) -> Option<Session> {
        self.get_key(&session_key(root))
    }

    /// The session stored under a [`session_key`].
    pub fn get_key(&self, key: &str) -> Option<Session> {
        self.keyspace
            .get(key)
            .map_err(|e| tracing::warn!("ignoring unreadable session: {e}"))
            .ok()
            .flatten()
    }

    /// Saves the session for `root`.
    pub fn save(&self, root: &Path, session: &Session) -> StoreResult<()> {
        self.keyspace.put(&session_key(root), session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_v03_file_stem() {
        // The value 0.7.1 computed for this root (the stem of the fixture file written by it).
        assert_eq!(
            session_key(Path::new("/fixture/project")),
            "4c92ce3babb53d97"
        );
        assert_eq!(session_key(Path::new("/fixture/project")).len(), 16);
    }
}
