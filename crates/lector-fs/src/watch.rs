//! Debounced, recursive file watching with git-state classification.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify_debouncer_full::notify::event::{AccessKind, MetadataKind, ModifyKind, RenameMode};
use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache,
};

use crate::{Error, Result};

/// What happened to a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsEventKind {
    /// The path was created (or renamed into place).
    Created,
    /// The path's content or metadata changed (also: replaced atomically).
    Modified,
    /// The path was removed (or renamed away).
    Removed,
}

/// A change to one path (absolute, as reported by the OS).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FsEvent {
    /// The affected path.
    pub path: PathBuf,
    /// What happened.
    pub kind: FsEventKind,
}

/// A debounced batch of changes delivered to the [`watch`] sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// Changes to files outside `.git`, deduplicated per path, in order of
    /// first occurrence.
    Files(Vec<FsEvent>),
    /// Git state changed: `.git/HEAD`, `index`, `refs/**`, `packed-refs`,
    /// `FETCH_HEAD`, `ORIG_HEAD`, `MERGE_HEAD` (and similar). Coalesced to
    /// at most one per batch. Other `.git` internals are ignored.
    GitStateChanged,
    /// Events were lost (queue overflow, watcher error): rescan everything.
    Rescan,
}

/// A running watcher; dropping it stops watching.
pub struct Watcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Watcher")
    }
}

/// Watches `root` recursively using the platform's recommended backend
/// (inotify, FSEvents, ReadDirectoryChangesW, ...) and calls `sink` from a
/// background thread with debounced batches: within one batch,
/// [`WatchEvent::Rescan`] is sent alone; otherwise [`WatchEvent::Files`]
/// (if any) precedes [`WatchEvent::GitStateChanged`] (if any).
pub fn watch(
    root: &Path,
    debounce: Duration,
    sink: impl Fn(WatchEvent) + Send + 'static,
) -> Result<Watcher> {
    let mut roots = vec![root.to_path_buf()];
    if let Ok(canonical) = root.canonicalize() {
        if canonical != root {
            roots.push(canonical);
        }
    }
    let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
        for event in convert(result, &roots) {
            sink(event);
        }
    })
    .map_err(Error::Notify)?;
    debouncer
        .watch(root, RecursiveMode::Recursive)
        .map_err(Error::Notify)?;
    Ok(Watcher {
        _debouncer: debouncer,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum PathClass {
    File,
    GitState,
    Ignored,
}

const GIT_STATE_FILES: &[&str] = &[
    "HEAD",
    "index",
    "packed-refs",
    "FETCH_HEAD",
    "ORIG_HEAD",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "REBASE_HEAD",
];

fn classify(path: &Path, roots: &[PathBuf]) -> PathClass {
    let relative = roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
    let components: Vec<&OsStr> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let Some(git_at) = components.iter().position(|name| *name == ".git") else {
        return PathClass::File;
    };
    let inside = &components[git_at + 1..];
    let is_state_file = |name: &OsStr| GIT_STATE_FILES.iter().any(|state| name == *state);
    let is_lock = |name: &OsStr| name.to_str().is_some_and(|name| name.ends_with(".lock"));
    let state = match inside {
        // The `.git` directory (or gitdir file) itself appeared/vanished.
        [] => true,
        [name] => is_state_file(name),
        [first, .., last] if *first == "refs" => !is_lock(last),
        [first, _, name] if *first == "worktrees" => is_state_file(name),
        _ => false,
    };
    if state {
        PathClass::GitState
    } else {
        PathClass::Ignored
    }
}

/// Folds a later event kind for the same path into an earlier one.
fn merge(old: FsEventKind, new: FsEventKind) -> Option<FsEventKind> {
    use FsEventKind::*;
    match (old, new) {
        (Created, Removed) => None,
        (Created, _) => Some(Created),
        (Removed, Created | Modified) => Some(Modified),
        (_, new) => Some(new),
    }
}

#[derive(Default)]
struct Batch {
    events: Vec<Option<FsEvent>>,
    index: HashMap<PathBuf, usize>,
    git_changed: bool,
}

impl Batch {
    fn add(&mut self, path: &Path, kind: FsEventKind, roots: &[PathBuf]) {
        match classify(path, roots) {
            PathClass::Ignored => {}
            PathClass::GitState => self.git_changed = true,
            PathClass::File => match self.index.get(path) {
                Some(&slot) => {
                    let merged = self.events[slot].as_ref().map(|event| event.kind);
                    let merged = match merged {
                        Some(old) => merge(old, kind),
                        None => Some(kind),
                    };
                    self.events[slot] = merged.map(|kind| FsEvent {
                        path: path.to_path_buf(),
                        kind,
                    });
                }
                None => {
                    self.index.insert(path.to_path_buf(), self.events.len());
                    self.events.push(Some(FsEvent {
                        path: path.to_path_buf(),
                        kind,
                    }));
                }
            },
        }
    }
}

fn exists_kind(path: &Path) -> FsEventKind {
    if path.symlink_metadata().is_ok() {
        FsEventKind::Modified
    } else {
        FsEventKind::Removed
    }
}

fn convert(result: DebounceEventResult, roots: &[PathBuf]) -> Vec<WatchEvent> {
    let events: Vec<DebouncedEvent> = match result {
        Ok(events) => events,
        Err(_) => return vec![WatchEvent::Rescan],
    };
    let mut batch = Batch::default();
    for event in &events {
        if event.need_rescan() {
            return vec![WatchEvent::Rescan];
        }
        let paths = &event.paths;
        match event.kind {
            EventKind::Access(AccessKind::Close(_)) | EventKind::Access(_) => {}
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => {}
            EventKind::Create(_) => {
                for path in paths {
                    batch.add(path, FsEventKind::Created, roots);
                }
            }
            EventKind::Remove(_) => {
                for path in paths {
                    batch.add(path, FsEventKind::Removed, roots);
                }
            }
            EventKind::Modify(ModifyKind::Name(mode)) => match (mode, paths.as_slice()) {
                (RenameMode::Both, [from, to, ..]) => {
                    batch.add(from, FsEventKind::Removed, roots);
                    batch.add(to, FsEventKind::Created, roots);
                }
                (RenameMode::From, _) => {
                    for path in paths {
                        batch.add(path, FsEventKind::Removed, roots);
                    }
                }
                (RenameMode::To, _) => {
                    for path in paths {
                        batch.add(path, FsEventKind::Created, roots);
                    }
                }
                _ => {
                    for path in paths {
                        let kind = match exists_kind(path) {
                            FsEventKind::Removed => FsEventKind::Removed,
                            _ => FsEventKind::Created,
                        };
                        batch.add(path, kind, roots);
                    }
                }
            },
            EventKind::Modify(_) => {
                for path in paths {
                    batch.add(path, FsEventKind::Modified, roots);
                }
            }
            EventKind::Any | EventKind::Other => {
                for path in paths {
                    batch.add(path, exists_kind(path), roots);
                }
            }
        }
    }
    let mut out = Vec::new();
    let files: Vec<FsEvent> = batch.events.into_iter().flatten().collect();
    if !files.is_empty() {
        out.push(WatchEvent::Files(files));
    }
    if batch.git_changed {
        out.push(WatchEvent::GitStateChanged);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify_debouncer_full::notify::event::{CreateKind, DataChange, Flag, RemoveKind};
    use notify_debouncer_full::notify::Event;
    use std::time::Instant;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/repo")]
    }

    fn event(kind: EventKind, paths: &[&str]) -> DebouncedEvent {
        let mut event = Event::new(kind);
        for path in paths {
            event = event.add_path(PathBuf::from(path));
        }
        DebouncedEvent::new(event, Instant::now())
    }

    #[test]
    fn classifies_git_paths() {
        let roots = roots();
        let class = |p: &str| classify(Path::new(p), &roots);
        assert_eq!(class("/repo/src/main.rs"), PathClass::File);
        assert_eq!(class("/repo/.gitignore"), PathClass::File);
        for state in [
            "/repo/.git/HEAD",
            "/repo/.git/index",
            "/repo/.git/packed-refs",
            "/repo/.git/FETCH_HEAD",
            "/repo/.git/ORIG_HEAD",
            "/repo/.git/MERGE_HEAD",
            "/repo/.git/refs/heads/main",
            "/repo/.git/refs/remotes/origin/feature/x",
            "/repo/.git/refs/tags",
            "/repo/.git",
            "/repo/sub/.git/HEAD",
            "/repo/.git/worktrees/wt/HEAD",
        ] {
            assert_eq!(class(state), PathClass::GitState, "{state}");
        }
        for ignored in [
            "/repo/.git/objects/ab/cdef",
            "/repo/.git/logs/HEAD",
            "/repo/.git/index.lock",
            "/repo/.git/refs/heads/main.lock",
            "/repo/.git/config",
            "/repo/.git/hooks/pre-commit",
        ] {
            assert_eq!(class(ignored), PathClass::Ignored, "{ignored}");
        }
    }

    #[test]
    fn converts_and_coalesces_batches() {
        let roots = roots();
        let events = vec![
            event(EventKind::Create(CreateKind::File), &["/repo/a.txt"]),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &["/repo/a.txt"],
            ),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &["/repo/b.txt"],
            ),
            event(EventKind::Create(CreateKind::File), &["/repo/tmp~"]),
            event(EventKind::Remove(RemoveKind::File), &["/repo/tmp~"]),
            event(EventKind::Remove(RemoveKind::File), &["/repo/c.txt"]),
            event(EventKind::Create(CreateKind::File), &["/repo/c.txt"]),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/repo/d", "/repo/e"],
            ),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &["/repo/.git/index"],
            ),
            event(
                EventKind::Create(CreateKind::File),
                &["/repo/.git/refs/heads/x"],
            ),
            event(
                EventKind::Create(CreateKind::File),
                &["/repo/.git/objects/12/34"],
            ),
        ];
        let out = convert(Ok(events), &roots);
        let file = |path: &str, kind| FsEvent {
            path: PathBuf::from(path),
            kind,
        };
        assert_eq!(
            out,
            vec![
                WatchEvent::Files(vec![
                    file("/repo/a.txt", FsEventKind::Created),
                    file("/repo/b.txt", FsEventKind::Modified),
                    file("/repo/c.txt", FsEventKind::Modified),
                    file("/repo/d", FsEventKind::Removed),
                    file("/repo/e", FsEventKind::Created),
                ]),
                WatchEvent::GitStateChanged,
            ]
        );
    }

    #[test]
    fn errors_and_rescan_flags_become_rescan() {
        let roots = roots();
        assert_eq!(convert(Err(vec![]), &roots), vec![WatchEvent::Rescan]);
        let mut flagged = event(EventKind::Other, &[]);
        flagged.event = flagged.event.clone().set_flag(Flag::Rescan);
        assert_eq!(convert(Ok(vec![flagged]), &roots), vec![WatchEvent::Rescan]);
        assert_eq!(convert(Ok(vec![]), &roots), vec![]);
    }
}
