//! End-to-end watcher tests against the real OS backend. Timeouts are
//! generous; each test waits for the expected condition rather than for a
//! fixed number of events.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lector_fs::{watch, FsEventKind, WatchEvent};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    rx: mpsc::Receiver<WatchEvent>,
    seen: Vec<WatchEvent>,
    _watcher: lector_fs::Watcher,
}

impl Harness {
    fn new() -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonicalize so reported paths compare equal (macOS /private/var).
        let root = dunce::canonicalize(dir.path()).expect("canonicalize");
        std::fs::create_dir_all(root.join(".git/refs/heads")).expect("git dirs");
        std::fs::create_dir_all(root.join(".git/objects/ab")).expect("git dirs");
        std::fs::create_dir_all(root.join("src")).expect("src");
        let (tx, rx) = mpsc::channel();
        let watcher = watch(&root, Duration::from_millis(50), move |event| {
            let _ = tx.send(event);
        })
        .expect("watch");
        // Give backends with asynchronous setup (FSEvents) a moment.
        std::thread::sleep(Duration::from_millis(200));
        Harness {
            _dir: dir,
            root,
            rx,
            seen: Vec::new(),
            _watcher: watcher,
        }
    }

    /// Receives events until `done` holds for an event or the timeout elapses.
    fn wait_for(&mut self, what: &str, done: impl Fn(&WatchEvent) -> bool) {
        if self.seen.iter().any(&done) {
            return;
        }
        let deadline = Instant::now() + TIMEOUT;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(remaining) {
                Ok(event) => {
                    let matched = done(&event);
                    self.seen.push(event);
                    if matched {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        panic!("timed out waiting for {what}; saw {:?}", self.seen);
    }
}

fn has_file(event: &WatchEvent, path: &Path, kinds: &[FsEventKind]) -> bool {
    match event {
        WatchEvent::Files(files) => files
            .iter()
            .any(|e| e.path == path && kinds.contains(&e.kind)),
        // A rescan also covers the change.
        WatchEvent::Rescan => true,
        WatchEvent::GitStateChanged => false,
    }
}

#[test]
fn reports_file_create_modify_remove() {
    let mut h = Harness::new();
    let file = h.root.join("src/main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write");
    h.wait_for("create", |e| {
        has_file(e, &file, &[FsEventKind::Created, FsEventKind::Modified])
    });

    std::thread::sleep(Duration::from_millis(150));
    std::fs::write(&file, "fn main() { println!(); }\n").expect("rewrite");
    h.wait_for("modify", |e| {
        has_file(e, &file, &[FsEventKind::Modified, FsEventKind::Created])
    });

    std::fs::remove_file(&file).expect("remove");
    h.wait_for("remove", |e| has_file(e, &file, &[FsEventKind::Removed]));
}

#[test]
fn reports_git_state_and_ignores_git_internals() {
    let mut h = Harness::new();
    std::fs::write(h.root.join(".git/objects/ab/cdef"), b"blob").expect("object");
    std::fs::write(h.root.join(".git/refs/heads/main"), b"0123\n").expect("ref");
    h.wait_for("git state", |e| {
        matches!(e, WatchEvent::GitStateChanged | WatchEvent::Rescan)
    });

    let marker = h.root.join("marker.txt");
    std::fs::write(&marker, "x").expect("marker");
    h.wait_for("marker", |e| {
        has_file(e, &marker, &[FsEventKind::Created, FsEventKind::Modified])
    });

    for event in &h.seen {
        if let WatchEvent::Files(files) = event {
            for file in files {
                assert!(
                    !file.path.components().any(|c| c.as_os_str() == ".git"),
                    "unexpected .git path in Files: {file:?}"
                );
            }
        }
    }
}

#[test]
fn dropping_the_watcher_stops_events() {
    let h = Harness::new();
    let root = h.root.clone();
    let Harness {
        _dir, rx, _watcher, ..
    } = h;
    drop(_watcher);
    std::fs::write(root.join("after-drop.txt"), "x").expect("write");
    // The channel's sender lived in the watcher, so it disconnects or stays silent.
    if let Ok(WatchEvent::Files(files)) = rx.recv_timeout(Duration::from_millis(500)) {
        assert!(files.iter().all(|f| !f.path.ends_with("after-drop.txt")));
    }
    drop(_dir);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_into_large_trees() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(dir.path()).expect("canonicalize");
    std::fs::create_dir_all(root.join("src")).expect("src");
    // A loop and a link to the filesystem root must not be crawled.
    std::os::unix::fs::symlink(&root, root.join("src/loop")).expect("symlink");
    std::os::unix::fs::symlink("/", root.join("rootfs")).expect("symlink");
    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    let _watcher = watch(&root, Duration::from_millis(50), move |event| {
        let _ = tx.send(event);
    })
    .expect("watch");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "watch setup took {:?}",
        started.elapsed()
    );
    std::thread::sleep(Duration::from_millis(100));
    let file = root.join("src/a.rs");
    std::fs::write(&file, "x").expect("write");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for the create event");
        let event = rx.recv_timeout(remaining).expect("event");
        if has_file(
            &event,
            &file,
            &[FsEventKind::Created, FsEventKind::Modified],
        ) {
            break;
        }
    }
}
