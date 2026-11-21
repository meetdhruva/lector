//! Upgrade from 0.7.1 (ADR 0004): a data directory assembled from files written by the 0.7.1
//! code (`tests/fixtures/state-0.7.1/`, `tests/fixtures/review-0.7.1.json`) is imported into
//! the state store on the first start: every value arrives exactly, the files are renamed to
//! `*.imported`, a second start changes nothing, and a second process starting during the
//! import is safe.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lector_app::store::{
    session_key, Axis, PaneState, SavedTab, Session, Store, StoreMode, TabKind, TabState,
    TerminalSession, WindowBounds,
};
use lector_app::{AppCore, CoreOptions};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read_dir").flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).expect("copy");
        }
    }
}

/// The data directory of a 0.7.1 user (who started with 0.7.0, and once with 0.3).
fn data_dir_071() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    copy_dir(&fixtures().join("state-0.7.1"), dir.path());
    std::fs::copy(
        fixtures().join("review-0.7.1.json"),
        dir.path().join("review.json"),
    )
    .expect("review fixture");
    dir
}

const LEGACY_FILES: &[&str] = &[
    "notices.json",
    "recent.json",
    "review.json",
    "sessions/4c92ce3babb53d97.json",
    "terminal/sessions.json",
    "terminal/sessions/4d7c8765b52ad319.json",
    "terminal/sessions/88bc787d1125c64c.json",
    "terminal/sessions/b9afcaa6f2b7bcc7.json",
    "update-state.json",
];

fn at(dir: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(dir.to_path_buf(), |p, s| p.join(s))
}

fn with_suffix(path: PathBuf, suffix: &str) -> PathBuf {
    let mut s = path.into_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json")
}

/// The window session the 0.7.1 code saved for `/fixture/project`.
fn expected_session() -> Session {
    let tab = |kind: TabKind, scroll_row: u32, active: bool, locked: bool| TabState {
        kind,
        scroll_row,
        active,
        read_only_locked: locked,
    };
    Session {
        panes: vec![
            PaneState {
                tabs: vec![
                    tab(
                        TabKind::File {
                            path: "/fixture/project/src/main.rs".into(),
                        },
                        120,
                        false,
                        false,
                    ),
                    tab(
                        TabKind::Diff {
                            changeset_key: "worktree".into(),
                            path: "src/lib.rs".into(),
                        },
                        7,
                        true,
                        false,
                    ),
                ],
            },
            PaneState {
                tabs: vec![
                    tab(
                        TabKind::Commit {
                            oid: "0123456789abcdef0123456789abcdef01234567".into(),
                        },
                        0,
                        false,
                        false,
                    ),
                    tab(
                        TabKind::File {
                            path: "/elsewhere/notes.md".into(),
                        },
                        3,
                        true,
                        true,
                    ),
                    tab(
                        TabKind::Other {
                            label: "review:worktree".into(),
                        },
                        0,
                        false,
                        false,
                    ),
                ],
            },
        ],
        axis: Axis::Vertical,
        active_pane: 1,
        sidebar_visible: false,
        sidebar_width: 312.5,
        sidebar_tab: "changes".into(),
        window: Some(WindowBounds {
            x: 40.0,
            y: 60.0,
            width: 1280.0,
            height: 800.0,
            maximized: false,
        }),
    }
}

fn saved(cwds: &[&str], title: Option<&str>, vertical: bool) -> SavedTab {
    SavedTab {
        cwds: cwds.iter().map(PathBuf::from).collect(),
        title: title.map(str::to_string),
        vertical,
    }
}

/// Every value of the fixtures, checked in the store. `dir` holds the renamed files.
fn assert_every_value_arrived(store: &Store, dir: &Path) {
    assert_eq!(
        store.recent_projects().list(),
        [
            "/fixture/newest",
            "/fixture/project",
            "/fixture/old-project"
        ]
        .map(PathBuf::from)
        .to_vec()
    );
    let root = Path::new("/fixture/project");
    assert_eq!(session_key(root), "4c92ce3babb53d97");
    assert_eq!(store.window_sessions().load(root), Some(expected_session()));

    let t = store.terminal_sessions();
    let mut terminal = t.entries().expect("entries");
    terminal.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        terminal,
        vec![
            (
                "/fixture/emptied".to_string(),
                TerminalSession { tabs: vec![] }
            ),
            (
                "/fixture/new-in-071".to_string(),
                TerminalSession {
                    tabs: vec![saved(&["/fixture/new-in-071/docs"], None, false)]
                }
            ),
            (
                "/fixture/only-legacy".to_string(),
                TerminalSession {
                    tabs: vec![saved(
                        &["/fixture/only-legacy", "/fixture/only-legacy/src"],
                        Some("dev"),
                        true
                    )]
                }
            ),
            (
                "/fixture/project".to_string(),
                TerminalSession {
                    tabs: vec![
                        saved(&["/fixture/project"], Some("server"), false),
                        saved(
                            &["/fixture/project/web", "/fixture/project/api"],
                            None,
                            true
                        ),
                    ]
                }
            ),
        ]
    );

    let marks = store.review_marks();
    assert_eq!(marks.len(), 2);
    assert!(marks.is_viewed(
        Path::new("/fixture/repo"),
        "worktree",
        "src/lib.rs",
        "536e506bb90914c243a12b397b9a998f85ae2cbd9ba02dfd03a9e155ca5ca0f4"
    ));
    assert!(marks.is_viewed(
        Path::new("/fixture/repo"),
        "commit:abc",
        "README.md",
        "deadbeef"
    ));

    // The notices and the update state, value for value as the files hold them (their types
    // live in lector-ui-agent and lector-forge, whose tests read them back from the store).
    assert_eq!(
        store.notices::<serde_json::Value>().get().expect("notices"),
        Some(json(&at(dir, "notices.json.imported")))
    );
    assert_eq!(
        store
            .update_state::<serde_json::Value>()
            .get()
            .expect("update state"),
        Some(json(&at(dir, "update-state.json.imported")))
    );
}

fn assert_renamed_once(dir: &Path) {
    for rel in LEGACY_FILES {
        assert!(!at(dir, rel).exists(), "{rel} was renamed");
        assert!(
            with_suffix(at(dir, rel), ".imported").exists(),
            "{rel}.imported is kept"
        );
        assert!(
            !with_suffix(at(dir, rel), ".imported.2").exists(),
            "{rel} renamed once"
        );
    }
    // Byte for byte what 0.7.1 wrote.
    assert_eq!(
        std::fs::read(at(dir, "recent.json.imported")).expect("read"),
        std::fs::read(fixtures().join("state-0.7.1/recent.json")).expect("read")
    );
}

#[test]
fn the_first_start_imports_0_7_1_state_and_the_second_changes_nothing() {
    let dir = data_dir_071();
    let db = dir.path().join("lector.redb");
    let opts = || CoreOptions {
        db_path: Some(db.clone()),
        import_from: Some(dir.path().to_path_buf()),
        data_dir: Some(dir.path().to_path_buf()),
        workers: Some(1),
        ..Default::default()
    };
    {
        let core = AppCore::open(opts());
        assert_eq!(core.store().mode(), StoreMode::ReadWrite);
        assert_every_value_arrived(core.store(), dir.path());
    }
    assert_renamed_once(dir.path());
    let listing = |d: &Path| {
        let mut v: Vec<String> = walk(d)
            .into_iter()
            .filter(|p| !p.ends_with("lector.redb"))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let files_before = listing(dir.path());
    // Second start: nothing to import, same values, no file touched.
    let store = Store::open(&db);
    assert_eq!(store.import_legacy(dir.path()), Default::default());
    assert_every_value_arrived(&store, dir.path());
    assert_eq!(listing(dir.path()), files_before);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

const CHILD_ENV: &str = "LECTOR_UPGRADE_TEST_CHILD";

/// Not a test of its own: the second process of the test below (this test binary re-run with
/// `CHILD_ENV` set to the data directory). Does nothing in a normal run.
#[test]
fn child_process_entry() {
    let Some(dir) = std::env::var_os(CHILD_ENV).map(PathBuf::from) else {
        return;
    };
    let store = Store::open(&dir.join("lector.redb"));
    store.import_legacy(&dir);
}

#[test]
fn a_second_process_starting_during_the_import_is_safe() {
    for _ in 0..3 {
        let dir = data_dir_071();
        let exe = std::env::current_exe().expect("current exe");
        let mut children: Vec<_> = (0..2)
            .map(|_| {
                Command::new(&exe)
                    .args(["--exact", "child_process_entry", "--test-threads=1"])
                    .env(CHILD_ENV, dir.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn")
            })
            .collect();
        let store = Store::open(&dir.path().join("lector.redb"));
        let report = store.import_legacy(dir.path());
        for child in &mut children {
            assert!(child.wait().expect("wait").success());
        }
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(store.mode(), StoreMode::ReadWrite);
        assert_every_value_arrived(&store, dir.path());
        assert_renamed_once(dir.path());
    }
}
