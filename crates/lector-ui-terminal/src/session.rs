//! Terminal session restore: which tabs (and splits) were open, in which folder.
//!
//! Stored in Lector's own data directory, one small file per project root
//! (`<data dir>/terminal/sessions/<hash of the root>.json`). Only folders and custom tab names
//! are kept: no scrollback, no commands. Restoring starts fresh shells; nothing is run.
//!
//! Why one file per project: every window saves its own project's tabs from a background task,
//! and two Lector processes can run at once. With a single shared file each save was
//! read -> modify -> write, so two concurrent saves of different projects could lose one of
//! them. A file per root means writers of different projects never touch the same file; each
//! write goes through a uniquely named temp file (process id + counter) and an atomic rename,
//! so even two writers of the same project leave one complete file (the later rename wins).
//! The single `sessions.json` of 0.7.0 and earlier is only ever read (a project without a file
//! of its own falls back to its entry there) and never written again; an emptied project keeps
//! an empty file so its old legacy entry does not come back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

/// The legacy single file (0.7.0 and earlier): all projects in one map.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct LegacyFile {
    version: u32,
    projects: BTreeMap<String, Vec<SavedTab>>,
}

/// One project's file.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ProjectFile {
    version: u32,
    /// The project root this file belongs to (guards against a hash collision).
    root: String,
    tabs: Vec<SavedTab>,
}

/// Most tabs restored per project (a corrupt or hostile file cannot spawn hundreds of shells).
pub const MAX_RESTORED_TABS: usize = 12;
/// Most splits restored per tab.
pub const MAX_RESTORED_SPLITS: usize = 4;

/// The legacy session file; per-project files live next to it in `sessions/`.
pub fn session_path() -> PathBuf {
    lector_config::paths::data_dir()
        .join("terminal")
        .join("sessions.json")
}

/// FNV-1a of `text`: a stable name (std's hashers may change between releases).
fn stable_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The file of `root`, in the `sessions/` folder next to the legacy file `path`.
pub fn project_file(path: &Path, root: &Path) -> PathBuf {
    let key = root.to_string_lossy();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join("sessions")
        .join(format!("{:016x}.json", stable_hash(&key)))
}

fn read_project(file: &Path, root: &Path) -> Option<Vec<SavedTab>> {
    let text = std::fs::read_to_string(file).ok()?;
    let parsed: ProjectFile = serde_json::from_str(&text).ok()?;
    (parsed.root == root.to_string_lossy()).then_some(parsed.tabs)
}

fn read_legacy(path: &Path, root: &Path) -> Vec<SavedTab> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<LegacyFile>(&text).ok())
        .and_then(|mut f| f.projects.remove(&root.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Saved tabs of `root` (empty when none or unreadable), capped.
pub fn load(path: &Path, root: &Path) -> Vec<SavedTab> {
    let mut tabs =
        read_project(&project_file(path, root), root).unwrap_or_else(|| read_legacy(path, root));
    tabs.truncate(MAX_RESTORED_TABS);
    for tab in &mut tabs {
        tab.cwds.truncate(MAX_RESTORED_SPLITS);
    }
    tabs.retain(|t| !t.cwds.is_empty());
    tabs
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Orders the writes of this process (renames of one file never interleave).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// A temp file name no other writer uses: process id and a per-process counter.
fn unique_temp(file: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = file
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    file.with_file_name(name)
}

/// Replaces the saved tabs of `root` (an empty list is stored as an empty file, so a legacy
/// entry of the old single file does not come back). Atomic (unique temp file + rename) so a
/// crash cannot leave half a file; writers of other projects never touch this file.
pub fn store(path: &Path, root: &Path, tabs: &[SavedTab]) -> std::io::Result<()> {
    let file = project_file(path, root);
    let text = serde_json::to_string_pretty(&ProjectFile {
        version: 2,
        root: root.to_string_lossy().into_owned(),
        tabs: tabs.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = unique_temp(&file);
    std::fs::write(&tmp, text)?;
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut result = std::fs::rename(&tmp, &file);
    // Windows refuses to replace a file another thread or process has open for a moment.
    for _ in 0..5 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        result = std::fs::rename(&tmp, &file);
    }
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(cwd: &str) -> SavedTab {
        SavedTab {
            cwds: vec![cwd.into()],
            ..SavedTab::default()
        }
    }

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
        std::fs::write(project_file(&path, Path::new("/b")), "{not json").unwrap();
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

    /// Windows (many windows, two processes) save at once: no project loses its tabs.
    #[test]
    fn concurrent_saves_of_different_projects_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal").join("sessions.json");
        let projects = 24;
        let rounds = 8;
        std::thread::scope(|scope| {
            for p in 0..projects {
                let path = &path;
                scope.spawn(move || {
                    let root = PathBuf::from(format!("/work/project-{p}"));
                    for round in 0..rounds {
                        let tabs: Vec<SavedTab> = (0..=round)
                            .map(|t| tab(&format!("/work/p{p}/t{t}")))
                            .collect();
                        store(path, &root, &tabs).unwrap();
                    }
                });
            }
        });
        for p in 0..projects {
            let root = PathBuf::from(format!("/work/project-{p}"));
            let tabs = load(&path, &root);
            assert_eq!(tabs.len(), rounds, "project {p} lost its last save");
            assert_eq!(
                tabs[rounds - 1],
                tab(&format!("/work/p{p}/t{}", rounds - 1))
            );
        }
    }

    /// Temp files never collide and none is left behind, even for writers of one project.
    #[test]
    fn temp_files_are_unique_and_cleaned_up() {
        let file = Path::new("/x/sessions/0123.json");
        let names: std::collections::HashSet<PathBuf> =
            (0..200).map(|_| unique_temp(file)).collect();
        assert_eq!(names.len(), 200);
        assert!(names
            .iter()
            .all(|n| n.parent() == file.parent() && n != file));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let root = Path::new("/same/project");
        std::thread::scope(|scope| {
            for t in 0..16 {
                let path = &path;
                scope.spawn(move || {
                    for n in 0..10 {
                        store(path, root, &[tab(&format!("/w{t}/{n}"))]).unwrap();
                    }
                });
            }
        });
        // One complete file with one tab, and no `.tmp` litter.
        assert_eq!(load(&path, root).len(), 1);
        let leftovers: Vec<_> = std::fs::read_dir(project_file(&path, root).parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The single file of 0.7.0 still loads, is never rewritten, and an emptied project does
    /// not get its old entry back.
    #[test]
    fn the_old_single_file_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal").join("sessions.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = r#"{"version":1,"projects":{
            "/old/a":[{"cwds":["/old/a","/old/a/src"],"title":"dev","vertical":true}],
            "/old/b":[{"cwds":["/old/b"],"title":null,"vertical":false}]}}"#;
        std::fs::write(&path, legacy).unwrap();
        let a = load(&path, Path::new("/old/a"));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].title.as_deref(), Some("dev"));
        assert!(a[0].vertical);
        // Saving one project writes its own file and leaves the legacy file as it was.
        store(&path, Path::new("/old/a"), &[tab("/new/a")]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
        assert_eq!(load(&path, Path::new("/old/a")), vec![tab("/new/a")]);
        assert_eq!(load(&path, Path::new("/old/b")).len(), 1);
        // Closing every terminal of /old/b is remembered despite the legacy entry.
        store(&path, Path::new("/old/b"), &[]).unwrap();
        assert!(load(&path, Path::new("/old/b")).is_empty());
    }
}
