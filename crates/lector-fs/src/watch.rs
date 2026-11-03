//! Debounced, recursive file watching with git-state classification.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, MetadataKind, ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::Result;

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
    // Owns the channel sender; dropping it ends the debounce thread.
    _watcher: RecommendedWatcher,
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
    if let Ok(canonical) = dunce::canonicalize(root) {
        if canonical != root {
            roots.push(canonical);
        }
    }
    // Don't follow symlinks (like the walker): a link to `/` or `$HOME` inside the
    // project would otherwise make the watcher crawl and watch that whole tree.
    let config = notify::Config::default().with_follow_symlinks(false);
    // Raw backend events, debounced here rather than by notify-debouncer-full: that
    // crate drops a path created and removed within one window, which loses real
    // deletions on macOS (FSEvents tags a deletion with the file's earlier "created"
    // flag), and its file-id cache walks the whole tree following symlinks on start.
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = tx.send(event);
        },
        config,
    )?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    std::thread::Builder::new()
        .name("lector-fs-watch".into())
        .spawn(move || debounce_loop(&rx, debounce, &roots, &sink))?;
    Ok(Watcher { _watcher: watcher })
}

/// Collects events until the stream has been quiet for `debounce` (or a batch has
/// been open for `max_latency`, so a steady stream of writes still gets delivered),
/// then hands the converted batch to `sink`. Returns when the watcher is dropped.
fn debounce_loop(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce: Duration,
    roots: &[PathBuf],
    sink: &impl Fn(WatchEvent),
) {
    let debounce = debounce.max(Duration::from_millis(10));
    let max_latency = debounce * 10;
    while let Ok(first) = rx.recv() {
        let opened = Instant::now();
        let mut batch = vec![first];
        let disconnected = loop {
            let remaining = max_latency.saturating_sub(opened.elapsed());
            if remaining.is_zero() {
                break false;
            }
            match rx.recv_timeout(debounce.min(remaining)) {
                Ok(event) => batch.push(event),
                Err(mpsc::RecvTimeoutError::Timeout) => break false,
                Err(mpsc::RecvTimeoutError::Disconnected) => break true,
            }
        };
        if disconnected {
            return;
        }
        for event in convert(batch, roots) {
            sink(event);
        }
    }
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

/// Folds a later event kind for the same path into an earlier one. Returns the
/// merged kind and whether the sequence was contradictory (created and removed in
/// the same batch, in either order), which is resolved against the disk later.
fn merge(old: FsEventKind, new: FsEventKind) -> (FsEventKind, bool) {
    use FsEventKind::*;
    match (old, new) {
        (Created, Removed) | (Removed, Created) => (new, true),
        (Created, _) => (Created, false),
        (Removed, Modified) => (Modified, true),
        (_, new) => (new, false),
    }
}

#[derive(Default)]
struct Batch {
    events: Vec<Slot>,
    index: HashMap<PathBuf, usize>,
    git_changed: bool,
}

struct Slot {
    event: FsEvent,
    /// The first kind seen for this path in the batch.
    first: FsEventKind,
    /// Created and removed within the batch: the final state comes from the disk.
    contradictory: bool,
}

impl Batch {
    fn add(&mut self, path: &Path, kind: FsEventKind, roots: &[PathBuf]) {
        match classify(path, roots) {
            PathClass::Ignored => {}
            PathClass::GitState => self.git_changed = true,
            PathClass::File => match self.index.get(path) {
                Some(&slot) => {
                    let slot = &mut self.events[slot];
                    let (merged, contradictory) = merge(slot.event.kind, kind);
                    slot.event.kind = merged;
                    slot.contradictory |= contradictory;
                }
                None => {
                    self.index.insert(path.to_path_buf(), self.events.len());
                    self.events.push(Slot {
                        event: FsEvent {
                            path: path.to_path_buf(),
                            kind,
                        },
                        first: kind,
                        contradictory: false,
                    });
                }
            },
        }
    }

    /// The batch's file events. Contradictory sequences are settled by whether the
    /// path exists now: backends disagree on what they report (FSEvents, for one,
    /// tags a deletion with the file's earlier "created" and "modified" flags too).
    fn into_file_events(self) -> Vec<FsEvent> {
        self.events
            .into_iter()
            .map(|slot| {
                let mut event = slot.event;
                if slot.contradictory {
                    event.kind = match exists_kind(&event.path) {
                        FsEventKind::Removed => FsEventKind::Removed,
                        _ if slot.first == FsEventKind::Created => FsEventKind::Created,
                        _ => FsEventKind::Modified,
                    };
                }
                event
            })
            .collect()
    }
}

fn exists_kind(path: &Path) -> FsEventKind {
    if path.symlink_metadata().is_ok() {
        FsEventKind::Modified
    } else {
        FsEventKind::Removed
    }
}

fn convert(results: Vec<notify::Result<Event>>, roots: &[PathBuf]) -> Vec<WatchEvent> {
    let mut events = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok(event) => events.push(event),
            Err(_) => return vec![WatchEvent::Rescan],
        }
    }
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
    let git_changed = batch.git_changed;
    let files = batch.into_file_events();
    if !files.is_empty() {
        out.push(WatchEvent::Files(files));
    }
    if git_changed {
        out.push(WatchEvent::GitStateChanged);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, Flag, RemoveKind};

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/repo")]
    }

    fn event(kind: EventKind, paths: &[&str]) -> notify::Result<Event> {
        let mut event = Event::new(kind);
        for path in paths {
            event = event.add_path(PathBuf::from(path));
        }
        Ok(event)
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
        // A real directory: contradictory sequences are settled against the disk.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let roots = vec![root.clone()];
        let p = |rel: &str| root.join(rel);
        let s = |rel: &str| p(rel).to_string_lossy().into_owned();
        std::fs::write(p("c.txt"), "recreated").unwrap();
        std::fs::write(p("f.rs"), "kept").unwrap();
        let events = vec![
            event(EventKind::Create(CreateKind::File), &[&s("a.txt")]),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &[&s("a.txt")],
            ),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &[&s("b.txt")],
            ),
            // Editor temp file: created and deleted within the batch.
            event(EventKind::Create(CreateKind::File), &[&s("tmp~")]),
            event(EventKind::Remove(RemoveKind::File), &[&s("tmp~")]),
            // Atomic save: removed and recreated.
            event(EventKind::Remove(RemoveKind::File), &[&s("c.txt")]),
            event(EventKind::Create(CreateKind::File), &[&s("c.txt")]),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[&s("d"), &s("e")],
            ),
            // FSEvents-style deletion carrying the file's earlier flags.
            event(EventKind::Create(CreateKind::File), &[&s("g.rs")]),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &[&s("g.rs")],
            ),
            event(EventKind::Remove(RemoveKind::File), &[&s("g.rs")]),
            // ...and the same flags on a file that still exists.
            event(EventKind::Create(CreateKind::File), &[&s("f.rs")]),
            event(EventKind::Remove(RemoveKind::File), &[&s("f.rs")]),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &[&s(".git/index")],
            ),
            event(
                EventKind::Create(CreateKind::File),
                &[&s(".git/refs/heads/x")],
            ),
            event(
                EventKind::Create(CreateKind::File),
                &[&s(".git/objects/12/34")],
            ),
        ];
        let out = convert(events, &roots);
        let file = |rel: &str, kind| FsEvent { path: p(rel), kind };
        assert_eq!(
            out,
            vec![
                WatchEvent::Files(vec![
                    file("a.txt", FsEventKind::Created),
                    file("b.txt", FsEventKind::Modified),
                    file("tmp~", FsEventKind::Removed),
                    file("c.txt", FsEventKind::Modified),
                    file("d", FsEventKind::Removed),
                    file("e", FsEventKind::Created),
                    file("g.rs", FsEventKind::Removed),
                    file("f.rs", FsEventKind::Created),
                ]),
                WatchEvent::GitStateChanged,
            ]
        );
    }

    #[test]
    fn errors_and_rescan_flags_become_rescan() {
        let roots = roots();
        let error = Err(notify::Error::generic("queue overflow"));
        assert_eq!(convert(vec![error], &roots), vec![WatchEvent::Rescan]);
        let flagged = Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan));
        assert_eq!(convert(vec![flagged], &roots), vec![WatchEvent::Rescan]);
        assert_eq!(convert(vec![], &roots), vec![]);
    }
}
