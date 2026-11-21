//! `terminal_sessions`: which terminal tabs (and splits) were open, in which folder, per
//! project root.
//!
//! Table `terminal_sessions`, keyed by the project root as given (`Path::to_string_lossy`,
//! the key 0.7.0 and 0.7.1 used). Only folders and custom tab names are kept: no
//! scrollback, no commands. Each project is its own row and each save is one transaction,
//! so two windows or two processes saving different projects (the 0.7.0 bug that lost
//! terminal tabs) or the same project never lose a write.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::keyspace::{Concern, Keyspace};
use super::{concerns, StoreResult};
use crate::db::Db;

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

/// The saved terminals of one project. An empty list is kept (the project's terminals were
/// all closed), distinct from a project never saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TerminalSession {
    /// Tabs, left to right.
    pub tabs: Vec<SavedTab>,
}

/// The key of `root`.
pub fn terminal_key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

/// Terminal sessions per project root. Cheap to clone.
#[derive(Clone, Debug)]
pub struct TerminalSessions {
    keyspace: Keyspace<TerminalSession>,
}

impl TerminalSessions {
    pub(crate) fn new(db: Db) -> Self {
        TerminalSessions {
            keyspace: Keyspace::new(db, &concerns::TERMINAL_SESSIONS),
        }
    }

    /// The concern.
    pub fn concern(&self) -> &'static Concern {
        self.keyspace.concern()
    }

    /// The saved session of `root`, if any.
    pub fn get(&self, root: &Path) -> StoreResult<Option<TerminalSession>> {
        self.keyspace.get(&terminal_key(root))
    }

    /// Saved tabs of `root` (empty when none or unreadable; logged), uncapped.
    pub fn tabs(&self, root: &Path) -> Vec<SavedTab> {
        match self.get(root) {
            Ok(s) => s.map(|s| s.tabs).unwrap_or_default(),
            Err(e) => {
                tracing::warn!("terminal session: {e}");
                Vec::new()
            }
        }
    }

    /// Replaces the saved tabs of `root` (one transaction).
    pub fn save(&self, root: &Path, tabs: &[SavedTab]) -> StoreResult<()> {
        self.keyspace.put(
            &terminal_key(root),
            &TerminalSession {
                tabs: tabs.to_vec(),
            },
        )
    }

    /// Read-modify-write of the saved tabs of `root` in one transaction.
    pub fn update<R>(
        &self,
        root: &Path,
        f: impl FnOnce(&mut Vec<SavedTab>) -> R,
    ) -> StoreResult<R> {
        self.keyspace.update(&terminal_key(root), |v| {
            let session = v.get_or_insert_with(TerminalSession::default);
            f(&mut session.tabs)
        })
    }

    /// Every saved project (key, session), sorted by key.
    pub fn entries(&self) -> StoreResult<Vec<(String, TerminalSession)>> {
        self.keyspace.entries()
    }
}
