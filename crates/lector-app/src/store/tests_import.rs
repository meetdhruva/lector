//! Import tests: the 0.7.1 fixtures, failure injection at every step, malformed files,
//! links, the size cap, files 0.4+ had imported already, read-only state.

use std::path::{Path, PathBuf};

use super::legacy::{import_with, Step};
use super::*;

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

/// A data dir as a 0.7.1 user who started with 0.7.0 (and once 0.3) has it.
fn data_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    copy_dir(&fixtures().join("state-0.7.1"), dir.path());
    std::fs::copy(
        fixtures().join("review-0.7.1.json"),
        dir.path().join("review.json"),
    )
    .expect("review fixture");
    dir
}

const FILES: &[&str] = &[
    "recent.json",
    "review.json",
    "notices.json",
    "update-state.json",
    "sessions/4c92ce3babb53d97.json",
    "terminal/sessions/4d7c8765b52ad319.json",
    "terminal/sessions/88bc787d1125c64c.json",
    "terminal/sessions/b9afcaa6f2b7bcc7.json",
    "terminal/sessions.json",
];

fn at(dir: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(dir.to_path_buf(), |p, s| p.join(s))
}

fn imported(dir: &Path, rel: &str) -> PathBuf {
    let mut p = at(dir, rel).into_os_string();
    p.push(".imported");
    PathBuf::from(p)
}

/// Everything the fixtures hold, read back from the store, in a comparable form.
fn snapshot(store: &Store) -> String {
    let recent = store.recent_projects().list();
    let window = store
        .window_sessions()
        .get_key("4c92ce3babb53d97")
        .map(|s| serde_json::to_string(&s).expect("json"));
    let terminal = store.terminal_sessions().entries().expect("entries");
    let notices = store.notices::<serde_json::Value>().get().expect("notices");
    let update = store
        .update_state::<serde_json::Value>()
        .get()
        .expect("update");
    let marks = store.review_marks();
    let viewed = (
        marks.viewed_hash(Path::new("/fixture/repo"), "worktree", "src/lib.rs"),
        marks.viewed_hash(Path::new("/fixture/repo"), "commit:abc", "README.md"),
        marks.len(),
    );
    format!("{recent:?}\n{window:?}\n{terminal:?}\n{notices:?}\n{update:?}\n{viewed:?}")
}

fn no_fault(_: Step, _: &str) -> bool {
    false
}

#[test]
fn the_fixtures_arrive_exactly_and_the_files_are_renamed() {
    let dir = data_dir();
    let store = Store::in_memory();
    let report = store.import_legacy(dir.path());
    let mut got = report.imported.clone();
    got.sort();
    let mut want: Vec<String> = FILES.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(got, want);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    for rel in FILES {
        assert!(!at(dir.path(), rel).exists(), "{rel} renamed");
        assert!(imported(dir.path(), rel).exists(), "{rel}.imported kept");
    }
    // Values.
    assert_eq!(
        store.recent_projects().list(),
        vec![
            PathBuf::from("/fixture/newest"),
            PathBuf::from("/fixture/project"),
            PathBuf::from("/fixture/old-project")
        ]
    );
    let t = store.terminal_sessions();
    assert_eq!(
        t.tabs(Path::new("/fixture/only-legacy")),
        vec![SavedTab {
            cwds: vec![
                "/fixture/only-legacy".into(),
                "/fixture/only-legacy/src".into()
            ],
            title: Some("dev".into()),
            vertical: true,
        }]
    );
    // The 0.7.1 per-project file wins over the 0.7.0 entry...
    let project = t.tabs(Path::new("/fixture/project"));
    assert_eq!(project.len(), 2);
    assert_eq!(project[0].title.as_deref(), Some("server"));
    // ...and a project whose terminals were all closed stays empty.
    assert_eq!(
        t.get(Path::new("/fixture/emptied")).expect("get"),
        Some(TerminalSession { tabs: vec![] })
    );
    assert_eq!(t.tabs(Path::new("/fixture/new-in-071")).len(), 1);
    assert!(store.review_marks().is_viewed(
        Path::new("/fixture/repo"),
        "commit:abc",
        "README.md",
        "deadbeef"
    ));
    let notices_file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(imported(dir.path(), "notices.json")).expect("read"))
            .expect("json");
    assert_eq!(
        store.notices::<serde_json::Value>().get().expect("get"),
        Some(notices_file)
    );

    // A second run changes nothing.
    let before = snapshot(&store);
    assert_eq!(store.import_legacy(dir.path()), ImportReport::default());
    assert_eq!(snapshot(&store), before);
}

/// The import interrupted at every step of every file, then run again, ends exactly like an
/// uninterrupted import, with every file renamed once.
#[test]
fn an_interrupted_import_completes_on_the_next_start() {
    let reference = {
        let dir = data_dir();
        let store = Store::in_memory();
        store.import_legacy(dir.path());
        snapshot(&store)
    };
    for step in [Step::Read, Step::Commit, Step::AfterCommit, Step::Rename] {
        for rel in FILES {
            let dir = data_dir();
            let store = Store::in_memory();
            let fail = |s: Step, r: &str| s == step && r == *rel;
            let first = import_with(&store, dir.path(), &fail);
            match step {
                Step::Read | Step::Commit => {
                    assert!(
                        at(dir.path(), rel).exists(),
                        "{step:?} {rel}: left in place"
                    );
                    assert!(!first.imported.iter().any(|r| r == rel));
                }
                Step::AfterCommit => assert!(at(dir.path(), rel).exists()),
                Step::Rename => {
                    assert!(first.imported.iter().any(|r| r == rel));
                    assert!(at(dir.path(), rel).exists());
                }
            }
            let second = import_with(&store, dir.path(), &no_fault);
            assert!(
                second.skipped.is_empty(),
                "{step:?} {rel}: {:?}",
                second.skipped
            );
            assert_eq!(snapshot(&store), reference, "{step:?} {rel}");
            for f in FILES {
                assert!(!at(dir.path(), f).exists(), "{step:?} {rel}: {f} renamed");
                assert!(imported(dir.path(), f).exists());
                let mut twice = imported(dir.path(), f).into_os_string();
                twice.push(".2");
                assert!(
                    !PathBuf::from(twice).exists(),
                    "{step:?} {rel}: {f} renamed once"
                );
            }
            assert_eq!(
                import_with(&store, dir.path(), &no_fault),
                ImportReport::default()
            );
        }
    }
}

#[test]
fn a_file_found_again_after_its_import_is_only_renamed() {
    let dir = data_dir();
    let store = Store::in_memory();
    store.import_legacy(dir.path());
    // The user changed state since, then an old copy of a file reappears (a downgrade, a
    // backup restore): it does not overwrite anything.
    store
        .terminal_sessions()
        .save(Path::new("/fixture/project"), &[])
        .expect("save");
    store
        .recent_projects()
        .remove(Path::new("/fixture/newest"))
        .expect("remove");
    std::fs::copy(
        fixtures().join("state-0.7.1/recent.json"),
        dir.path().join("recent.json"),
    )
    .expect("copy");
    std::fs::copy(
        fixtures().join("state-0.7.1/terminal/sessions/4d7c8765b52ad319.json"),
        at(dir.path(), "terminal/sessions/4d7c8765b52ad319.json"),
    )
    .expect("copy");
    let report = store.import_legacy(dir.path());
    assert!(report.imported.is_empty());
    assert_eq!(report.already_imported.len(), 2);
    assert!(store
        .terminal_sessions()
        .tabs(Path::new("/fixture/project"))
        .is_empty());
    assert!(!store
        .recent_projects()
        .list()
        .contains(&PathBuf::from("/fixture/newest")));
    assert!(dir.path().join("recent.json.imported.2").exists());
    assert!(!dir.path().join("recent.json").exists());
}

#[test]
fn a_malformed_file_is_left_in_place_and_does_not_block_the_others() {
    let dir = data_dir();
    std::fs::write(dir.path().join("notices.json"), "{ not json").expect("write");
    std::fs::write(
        at(dir.path(), "terminal/sessions/88bc787d1125c64c.json"),
        "[1, 2]",
    )
    .expect("write");
    let store = Store::in_memory();
    let report = store.import_legacy(dir.path());
    let skipped: Vec<&str> = report.skipped.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        skipped,
        ["notices.json", "terminal/sessions/88bc787d1125c64c.json"]
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notices.json")).expect("read"),
        "{ not json"
    );
    assert!(!imported(dir.path(), "notices.json").exists());
    assert_eq!(report.imported.len(), FILES.len() - 2);
    assert_eq!(
        store.notices::<serde_json::Value>().get().expect("get"),
        None
    );
    // The emptied project's own file was unreadable: its 0.7.0 entry is used, as 0.7.1 did.
    assert_eq!(
        store
            .terminal_sessions()
            .tabs(Path::new("/fixture/emptied"))
            .len(),
        1
    );
    assert_eq!(store.recent_projects().list().len(), 3);
    // Still there on the next start, still skipped, nothing else happens.
    let again = store.import_legacy(dir.path());
    assert_eq!(again.skipped.len(), 2);
    assert!(again.imported.is_empty());
}

#[test]
fn files_larger_than_the_cap_are_not_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let big = dir.path().join("review.json");
    let f = std::fs::File::create(&big).expect("create");
    f.set_len(MAX_LEGACY_BYTES + 1).expect("set_len");
    drop(f);
    std::fs::write(dir.path().join("recent.json"), r#"{"projects":["/a"]}"#).expect("write");
    let store = Store::in_memory();
    let report = store.import_legacy(dir.path());
    assert_eq!(report.imported, vec!["recent.json".to_string()]);
    assert_eq!(report.skipped.len(), 1);
    assert!(
        report.skipped[0].1.contains("more than"),
        "{:?}",
        report.skipped
    );
    assert_eq!(
        std::fs::metadata(&big).expect("meta").len(),
        MAX_LEGACY_BYTES + 1
    );
}

#[cfg(unix)]
#[test]
fn links_out_of_the_data_dir_are_never_followed() {
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(
        outside.path().join("secret.json"),
        r#"{"projects":["/stolen"]}"#,
    )
    .expect("write");
    std::fs::create_dir_all(outside.path().join("sessions")).expect("mkdir");
    std::fs::write(
        outside.path().join("sessions").join("x.json"),
        r#"{"root":"/stolen","tabs":[{"cwds":["/stolen"]}]}"#,
    )
    .expect("write");
    let dir = tempfile::tempdir().expect("tempdir");
    std::os::unix::fs::symlink(
        outside.path().join("secret.json"),
        dir.path().join("recent.json"),
    )
    .expect("symlink");
    std::fs::create_dir(dir.path().join("terminal")).expect("mkdir");
    std::os::unix::fs::symlink(
        outside.path().join("sessions"),
        dir.path().join("terminal").join("sessions"),
    )
    .expect("symlink");
    let store = Store::in_memory();
    let report = store.import_legacy(dir.path());
    assert!(report.imported.is_empty(), "{report:?}");
    assert_eq!(report.skipped.len(), 2, "{report:?}");
    assert!(store.recent_projects().list().is_empty());
    assert!(store
        .terminal_sessions()
        .entries()
        .expect("entries")
        .is_empty());
    // The links and their targets are untouched.
    assert!(std::fs::symlink_metadata(dir.path().join("recent.json"))
        .expect("link")
        .file_type()
        .is_symlink());
    assert!(outside.path().join("secret.json").exists());
    assert!(outside.path().join("sessions").join("x.json").exists());
}

/// Users of 0.4 to 0.7.1 had the v0.3 stores copied into the database already, and edited
/// them there since: the old files must not bring anything back.
#[test]
fn v03_files_already_in_the_database_are_not_copied_again() {
    let dir = data_dir();
    let store = Store::in_memory();
    {
        let tx = store.db().raw().begin_write().expect("write");
        tx.open_table(crate::db::META)
            .expect("meta")
            .insert(crate::db::KEY_IMPORTED_V03, 1)
            .expect("flag");
        tx.commit().expect("commit");
    }
    store
        .recent_projects()
        .record(Path::new("/current"))
        .expect("record");
    let report = store.import_legacy(dir.path());
    assert_eq!(report.imported.len(), FILES.len());
    // Only the current state, not the v0.3 files' content...
    assert_eq!(
        store.recent_projects().list(),
        vec![PathBuf::from("/current")]
    );
    assert!(store.review_marks().is_empty());
    assert_eq!(store.window_sessions().get_key("4c92ce3babb53d97"), None);
    // ...but the 0.7 files (never in the database) are imported.
    assert_eq!(
        store.terminal_sessions().entries().expect("entries").len(),
        4
    );
    assert!(store
        .notices::<serde_json::Value>()
        .get()
        .expect("get")
        .is_some());
    for rel in FILES {
        assert!(imported(dir.path(), rel).exists(), "{rel}");
    }
}

#[test]
fn values_already_in_the_store_win_over_files() {
    let dir = data_dir();
    let store = Store::in_memory();
    store
        .terminal_sessions()
        .save(Path::new("/fixture/only-legacy"), &[])
        .expect("save");
    store
        .notices::<serde_json::Value>()
        .put(&serde_json::json!({"updates_asked": false}))
        .expect("put");
    store.import_legacy(dir.path());
    assert!(store
        .terminal_sessions()
        .tabs(Path::new("/fixture/only-legacy"))
        .is_empty());
    assert_eq!(
        store.notices::<serde_json::Value>().get().expect("get"),
        Some(serde_json::json!({"updates_asked": false}))
    );
}
