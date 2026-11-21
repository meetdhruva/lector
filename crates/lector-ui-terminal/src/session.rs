//! Terminal session restore: which tabs (and splits) were open, in which folder.
//!
//! Stored in Lector's state store (`lector_app::store`, concern `terminal_sessions`), one row
//! per project root. Only folders and custom tab names are kept: no scrollback, no commands.
//! Restoring starts fresh shells; nothing is run.
//!
//! Every window saves its own project's tabs from a background task, and two Lector processes
//! can run at once: each save is one database transaction on its project's row, so concurrent
//! saves (of different projects or of the same one) never lose each other. The files of 0.7.0
//! (`terminal/sessions.json`) and 0.7.1 (`terminal/sessions/<hash>.json`) are imported into
//! the store once by `lector_app::store::Store::import_legacy`.

use std::path::Path;

pub use lector_app::store::SavedTab;
use lector_app::store::{StoreResult, TerminalSessions};

/// Most tabs restored per project (a corrupt or hostile value cannot spawn hundreds of shells).
pub const MAX_RESTORED_TABS: usize = 12;
/// Most splits restored per tab.
pub const MAX_RESTORED_SPLITS: usize = 4;

/// Saved tabs of `root` (empty when none or unreadable), capped.
pub fn load(sessions: &TerminalSessions, root: &Path) -> Vec<SavedTab> {
    let mut tabs = sessions.tabs(root);
    tabs.truncate(MAX_RESTORED_TABS);
    for tab in &mut tabs {
        tab.cwds.truncate(MAX_RESTORED_SPLITS);
    }
    tabs.retain(|t| !t.cwds.is_empty());
    tabs
}

/// Replaces the saved tabs of `root` (an empty list is kept as such: the project's terminals
/// were all closed). One transaction.
pub fn save(sessions: &TerminalSessions, root: &Path, tabs: &[SavedTab]) -> StoreResult<()> {
    sessions.save(root, tabs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_app::store::Store;
    use std::path::PathBuf;

    fn tab(cwd: &str) -> SavedTab {
        SavedTab {
            cwds: vec![cwd.into()],
            ..SavedTab::default()
        }
    }

    #[test]
    fn round_trip_per_project() {
        let store = Store::in_memory();
        let s = store.terminal_sessions();
        assert!(load(&s, Path::new("/a")).is_empty());
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
        save(&s, Path::new("/a"), &tabs).unwrap();
        save(&s, Path::new("/b"), &tabs[1..]).unwrap();
        assert_eq!(load(&s, Path::new("/a")), tabs);
        assert_eq!(load(&s, Path::new("/b")), tabs[1..].to_vec());
        save(&s, Path::new("/a"), &[]).unwrap();
        assert!(load(&s, Path::new("/a")).is_empty());
        assert_eq!(load(&s, Path::new("/b")).len(), 1);
        // Huge sessions are capped; tabs without a folder are dropped.
        let mut many: Vec<SavedTab> = (0..50)
            .map(|_| SavedTab {
                cwds: vec!["/x".into(); 9],
                ..SavedTab::default()
            })
            .collect();
        many.insert(0, SavedTab::default());
        save(&s, Path::new("/c"), &many).unwrap();
        let loaded = load(&s, Path::new("/c"));
        assert_eq!(loaded.len(), MAX_RESTORED_TABS - 1);
        assert!(loaded.iter().all(|t| t.cwds.len() == MAX_RESTORED_SPLITS));
    }

    /// Windows (many windows, two processes) save at once: no project loses its tabs.
    #[test]
    fn concurrent_saves_of_different_projects_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("lector.redb"));
        let projects = 24;
        let rounds = 8;
        std::thread::scope(|scope| {
            for p in 0..projects {
                let s = store.terminal_sessions();
                scope.spawn(move || {
                    let root = PathBuf::from(format!("/work/project-{p}"));
                    for round in 0..rounds {
                        let tabs: Vec<SavedTab> = (0..=round)
                            .map(|t| tab(&format!("/work/p{p}/t{t}")))
                            .collect();
                        save(&s, &root, &tabs).unwrap();
                    }
                });
            }
        });
        let s = store.terminal_sessions();
        for p in 0..projects {
            let root = PathBuf::from(format!("/work/project-{p}"));
            let tabs = load(&s, &root);
            assert_eq!(tabs.len(), rounds, "project {p} lost its last save");
            assert_eq!(
                tabs[rounds - 1],
                tab(&format!("/work/p{p}/t{}", rounds - 1))
            );
        }
    }
}
