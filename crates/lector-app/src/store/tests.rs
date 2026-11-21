//! Store tests: per-concern round trips, atomic updates under concurrency (threads and
//! processes), schema versions, corruption and locks.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use redb::ReadableDatabase;
use serde::{Deserialize, Serialize};

use super::keyspace::STORE_META;
use super::open::{classify, OpenFailure};
use super::*;

fn session(row: u32) -> Session {
    Session {
        panes: vec![PaneState {
            tabs: vec![TabState {
                kind: TabKind::File {
                    path: "/a/b.rs".into(),
                },
                scroll_row: row,
                active: true,
                read_only_locked: false,
            }],
        }],
        ..Default::default()
    }
}

fn tab(cwd: &str) -> SavedTab {
    SavedTab {
        cwds: vec![cwd.into()],
        ..SavedTab::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Counter {
    n: u64,
    seen: Vec<String>,
}

// ---- round trips ------------------------------------------------------------------------

#[test]
fn recent_projects_round_trip() {
    let store = Store::in_memory();
    let dir = tempfile::tempdir().expect("tempdir");
    let r = store.recent_projects();
    for i in 0..25 {
        r.record(&dir.path().join(format!("p{i}"))).expect("record");
    }
    r.record(&dir.path().join("p3")).expect("record");
    let list = r.list();
    assert_eq!(list.len(), MAX_RECENT_PROJECTS);
    assert_eq!(list[0], dir.path().join("p3"));
    assert_eq!(list[1], dir.path().join("p24"));
    assert_eq!(list.iter().filter(|p| p.ends_with("p3")).count(), 1);
    assert_eq!(r.most_recent_existing(), None);
    std::fs::create_dir(dir.path().join("p24")).expect("mkdir");
    assert_eq!(r.most_recent_existing(), Some(dir.path().join("p24")));
    r.remove(&dir.path().join("p3")).expect("remove");
    assert!(!r.list().contains(&dir.path().join("p3")));
    r.remove(&dir.path().join("missing"))
        .expect("remove missing");
}

#[test]
fn window_sessions_round_trip_and_persist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state").join("lector.redb");
    let project = tempfile::tempdir().expect("project");
    {
        let store = Store::open(&path);
        assert_eq!(store.mode(), StoreMode::ReadWrite);
        let s = store.window_sessions();
        assert!(s.load(project.path()).is_none());
        s.save(project.path(), &session(7)).expect("save");
        assert_eq!(s.load(project.path()), Some(session(7)));
    }
    let store = Store::open(&path);
    assert_eq!(
        store.window_sessions().load(project.path()),
        Some(session(7))
    );
    assert_eq!(
        store
            .window_sessions()
            .get_key(&session_key(project.path())),
        Some(session(7))
    );
}

#[test]
fn terminal_sessions_round_trip() {
    let store = Store::in_memory();
    let t = store.terminal_sessions();
    let a = Path::new("/a");
    assert!(t.get(a).expect("get").is_none());
    assert!(t.tabs(a).is_empty());
    let tabs = vec![
        SavedTab {
            cwds: vec!["/a".into(), "/a/src".into()],
            title: Some("build".into()),
            vertical: true,
        },
        tab("/a/docs"),
    ];
    t.save(a, &tabs).expect("save");
    t.save(Path::new("/b"), &tabs[1..]).expect("save");
    assert_eq!(t.tabs(a), tabs);
    assert_eq!(t.tabs(Path::new("/b")), tabs[1..].to_vec());
    // An emptied project is remembered as empty, not as missing.
    t.save(a, &[]).expect("save empty");
    assert_eq!(
        t.get(a).expect("get"),
        Some(TerminalSession { tabs: Vec::new() })
    );
    assert_eq!(t.entries().expect("entries").len(), 2);
}

#[test]
fn review_marks_round_trip() {
    let store = Store::in_memory();
    let marks = store.review_marks();
    let repo = Path::new("/r");
    assert!(!marks.is_viewed(repo, "wt", "a.rs", "h1"));
    marks
        .set_viewed(repo, "wt", "a.rs", "h1", true)
        .expect("set");
    assert!(marks.is_viewed(repo, "wt", "a.rs", "h1"));
    assert!(!marks.is_viewed(repo, "wt", "a.rs", "h2"));
    assert!(!marks.is_viewed(repo, "staged", "a.rs", "h1"));
    assert_eq!(marks.len(), 1);
    // Another handle over the same store sees it (shared between windows).
    assert!(store.review_marks().is_viewed(repo, "wt", "a.rs", "h1"));
    marks
        .set_viewed(repo, "wt", "a.rs", "h1", false)
        .expect("unset");
    assert!(marks.is_empty());
}

#[test]
fn notices_and_update_state_round_trip() {
    let store = Store::in_memory();
    let notices = store.notices::<Counter>();
    assert_eq!(notices.get().expect("get"), None);
    let value = Counter {
        n: 3,
        seen: vec!["x".into()],
    };
    notices.put(&value).expect("put");
    assert_eq!(notices.get().expect("get"), Some(value.clone()));
    let n = notices
        .update(|v| {
            let v = v.get_or_insert_with(Counter::default);
            v.n += 1;
            v.n
        })
        .expect("update");
    assert_eq!(n, 4);
    // The update state is a separate concern.
    let update = store.update_state::<BTreeMap<String, i64>>();
    assert_eq!(update.get().expect("get"), None);
    update
        .put(&BTreeMap::from([("last_checked".into(), 5)]))
        .expect("put");
    assert_eq!(
        update
            .get()
            .expect("get")
            .and_then(|m| m.get("last_checked").copied()),
        Some(5)
    );
    assert_eq!(
        store.notices::<Counter>().get().expect("get").map(|c| c.n),
        Some(4)
    );
}

#[test]
fn hunk_reviews_are_reached_through_the_store() {
    let store = Store::in_memory();
    let hunks = store.hunk_reviews();
    assert!(hunks.load(Path::new("/r"), "wt", "a.rs").is_empty());
    assert!(store
        .db()
        .kv_scan("review.hunks/")
        .expect("scan")
        .is_empty());
}

// ---- update atomicity ---------------------------------------------------------------------

/// Many threads doing read-modify-write on the same key and on their own keys: nothing lost.
#[test]
fn concurrent_updates_lose_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("lector.redb"));
    let ks = Keyspace::<Counter>::new(store.db().clone(), &concerns::NOTICES);
    let threads = 8;
    let rounds = 25;
    std::thread::scope(|scope| {
        for t in 0..threads {
            let ks = ks.clone();
            scope.spawn(move || {
                for r in 0..rounds {
                    ks.update("shared", |v| {
                        let v = v.get_or_insert_with(Counter::default);
                        v.n += 1;
                        v.seen.push(format!("{t}/{r}"));
                    })
                    .expect("update shared");
                    ks.update(&format!("own-{t}"), |v| {
                        v.get_or_insert_with(Counter::default).n += 1;
                    })
                    .expect("update own");
                }
            });
        }
    });
    let shared = ks.get("shared").expect("get").expect("present");
    assert_eq!(shared.n, threads * rounds);
    assert_eq!(shared.seen.len() as u64, threads * rounds);
    for t in 0..threads {
        assert_eq!(
            ks.get(&format!("own-{t}")).expect("get").map(|c| c.n),
            Some(rounds)
        );
    }
}

/// The 0.7.0 bug (two windows saving terminal tabs at once lost a project) as a store-level
/// regression test: projects saved concurrently all survive, and tabs added concurrently to
/// one project through `update` are all there.
#[test]
fn concurrent_terminal_saves_lose_no_tabs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("lector.redb"));
    let projects = 16;
    let rounds = 6;
    std::thread::scope(|scope| {
        for p in 0..projects {
            let t = store.terminal_sessions();
            scope.spawn(move || {
                let root = PathBuf::from(format!("/work/project-{p}"));
                for round in 0..rounds {
                    let tabs: Vec<SavedTab> = (0..=round)
                        .map(|i| tab(&format!("/work/p{p}/t{i}")))
                        .collect();
                    t.save(&root, &tabs).expect("save");
                    t.update(Path::new("/work/shared"), |tabs| {
                        tabs.push(tab(&format!("/work/shared/{p}-{round}")))
                    })
                    .expect("update");
                }
            });
        }
    });
    let t = store.terminal_sessions();
    for p in 0..projects {
        let tabs = t.tabs(Path::new(&format!("/work/project-{p}")));
        assert_eq!(tabs.len(), rounds, "project {p} lost its last save");
    }
    assert_eq!(t.tabs(Path::new("/work/shared")).len(), projects * rounds);
}

// ---- schema versions ------------------------------------------------------------------------

const TEST_V1: Concern = Concern {
    name: "test_versioned",
    version: 1,
    migrate: None,
};

fn v1_to_v2(from: u64, value: &[u8]) -> Result<Vec<u8>, String> {
    #[derive(Deserialize)]
    struct V1 {
        n: u64,
    }
    if from != 1 {
        return Err(format!("unknown version {from}"));
    }
    let old: V1 = serde_json::from_slice(value).map_err(|e| e.to_string())?;
    serde_json::to_vec(&serde_json::json!({ "count": old.n, "migrated": true }))
        .map_err(|e| e.to_string())
}

const TEST_V2: Concern = Concern {
    name: "test_versioned",
    version: 2,
    migrate: Some(v1_to_v2),
};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct V1Value {
    n: u64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct V2Value {
    count: u64,
    migrated: bool,
}

fn raw_version(store: &Store, concern: &str) -> Option<u64> {
    let tx = store.db().raw().begin_read().expect("read");
    let t = tx.open_table(STORE_META).expect("meta");
    let v = t
        .get(format!("version:{concern}").as_str())
        .expect("get")
        .map(|g| g.value());
    v
}

#[test]
fn older_versions_are_migrated_and_newer_ones_are_left_alone() {
    let store = Store::in_memory();
    let old = Keyspace::<V1Value>::new(store.db().clone(), &TEST_V1);
    old.put("a", &V1Value { n: 4 }).expect("put");
    old.put("b", &V1Value { n: 9 }).expect("put");
    assert_eq!(raw_version(&store, "test_versioned"), Some(1));

    // A newer build reads the old values migrated, without writing.
    let new = Keyspace::<V2Value>::new(store.db().clone(), &TEST_V2);
    assert_eq!(new.access().expect("access"), Access::Older(1));
    assert_eq!(
        new.get("a").expect("get"),
        Some(V2Value {
            count: 4,
            migrated: true
        })
    );
    assert_eq!(raw_version(&store, "test_versioned"), Some(1));
    // Its first write migrates every row and records version 2, in one transaction.
    new.update("a", |v| {
        if let Some(v) = v {
            v.count += 1;
        }
    })
    .expect("update");
    assert_eq!(raw_version(&store, "test_versioned"), Some(2));
    assert_eq!(new.access().expect("access"), Access::Current);
    assert_eq!(new.get("a").expect("get").map(|v| v.count), Some(5));
    assert_eq!(new.get("b").expect("get").map(|v| v.count), Some(9));

    // The old build now finds a newer schema: reads see nothing, writes are refused, the
    // stored values stay as the newer build wrote them.
    assert_eq!(old.access().expect("access"), Access::Newer(2));
    assert_eq!(old.get("a").expect("get"), None);
    assert!(old.entries().expect("entries").is_empty());
    assert_eq!(old.len().expect("len"), 0);
    assert!(matches!(
        old.put("a", &V1Value { n: 0 }),
        Err(StoreError::NewerSchema {
            found: 2,
            known: 1,
            ..
        })
    ));
    assert!(old.update("c", |_| ()).is_err());
    assert!(old.remove("b").is_err());
    assert_eq!(new.get("a").expect("get").map(|v| v.count), Some(5));
    assert_eq!(new.get("b").expect("get").map(|v| v.count), Some(9));
    assert_eq!(new.get("c").expect("get"), None);
}

#[test]
fn a_failing_migration_writes_nothing() {
    fn broken(_: u64, _: &[u8]) -> Result<Vec<u8>, String> {
        Err("cannot".into())
    }
    const BROKEN: Concern = Concern {
        name: "test_versioned",
        version: 2,
        migrate: Some(broken),
    };
    let store = Store::in_memory();
    Keyspace::<V1Value>::new(store.db().clone(), &TEST_V1)
        .put("a", &V1Value { n: 1 })
        .expect("put");
    let ks = Keyspace::<V2Value>::new(store.db().clone(), &BROKEN);
    assert!(matches!(
        ks.get("a"),
        Err(StoreError::Migration { from: 1, .. })
    ));
    assert!(ks
        .put(
            "z",
            &V2Value {
                count: 0,
                migrated: false
            }
        )
        .is_err());
    assert_eq!(raw_version(&store, "test_versioned"), Some(1));
    assert_eq!(
        Keyspace::<V1Value>::new(store.db().clone(), &TEST_V1)
            .get("a")
            .expect("get"),
        Some(V1Value { n: 1 })
    );
}

/// Sets the stored version of a real concern, as a newer Lector would have.
fn set_version(store: &Store, concern: &str, version: u64) {
    let tx = store.db().raw().begin_write().expect("write");
    tx.open_table(STORE_META)
        .expect("meta")
        .insert(format!("version:{concern}").as_str(), version)
        .expect("insert");
    tx.commit().expect("commit");
}

#[test]
fn every_real_concern_is_read_only_under_a_newer_schema() {
    let store = Store::in_memory();
    let root = Path::new("/p");
    store.recent_projects().record(root).expect("record");
    store
        .window_sessions()
        .save(root, &session(1))
        .expect("save");
    store
        .terminal_sessions()
        .save(root, &[tab("/p")])
        .expect("save");
    store
        .review_marks()
        .set_viewed(root, "wt", "a", "h", true)
        .expect("set");
    store
        .notices::<Counter>()
        .put(&Counter::default())
        .expect("put");
    store
        .update_state::<Counter>()
        .put(&Counter::default())
        .expect("put");
    for c in [
        &concerns::RECENT_PROJECTS,
        &concerns::WINDOW_SESSIONS,
        &concerns::TERMINAL_SESSIONS,
        &concerns::REVIEW_STATE,
        &concerns::NOTICES,
        &concerns::UPDATE_STATE,
    ] {
        set_version(&store, c.name, c.version + 1);
    }
    assert!(store.recent_projects().list().is_empty());
    assert!(store.recent_projects().record(Path::new("/q")).is_err());
    assert!(store.recent_projects().remove(root).is_err());
    assert_eq!(store.window_sessions().load(root), None);
    assert!(store.window_sessions().save(root, &session(2)).is_err());
    assert!(store.terminal_sessions().tabs(root).is_empty());
    assert!(store.terminal_sessions().save(root, &[]).is_err());
    assert!(!store.review_marks().is_viewed(root, "wt", "a", "h"));
    assert_eq!(store.review_marks().len(), 0);
    assert!(store
        .review_marks()
        .set_viewed(root, "wt", "a", "h", false)
        .is_err());
    assert_eq!(store.notices::<Counter>().get().expect("get"), None);
    assert!(store.notices::<Counter>().put(&Counter::default()).is_err());
    assert!(store.update_state::<Counter>().update(|_| ()).is_err());
    // Back to the known versions: everything is still there, untouched.
    for c in [
        &concerns::RECENT_PROJECTS,
        &concerns::WINDOW_SESSIONS,
        &concerns::TERMINAL_SESSIONS,
        &concerns::REVIEW_STATE,
        &concerns::NOTICES,
        &concerns::UPDATE_STATE,
    ] {
        set_version(&store, c.name, c.version);
    }
    assert_eq!(store.recent_projects().list(), vec![root.to_path_buf()]);
    assert_eq!(store.window_sessions().load(root), Some(session(1)));
    assert_eq!(store.terminal_sessions().tabs(root), vec![tab("/p")]);
    assert!(store.review_marks().is_viewed(root, "wt", "a", "h"));
    assert!(store.notices::<Counter>().get().expect("get").is_some());
}

// ---- opening: corruption, locks, newer files ------------------------------------------------

fn listing(dir: &Path) -> Vec<(String, u64)> {
    let mut out: Vec<_> = walk(dir)
        .into_iter()
        .map(|p| {
            let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let rel = p
                .strip_prefix(dir)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            (rel, len)
        })
        .collect();
    out.sort();
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push(p.clone());
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn errors_are_classified_as_locks_or_corruption() {
    use redb::{DatabaseError as D, StorageError as S};
    assert!(matches!(
        classify(&D::DatabaseAlreadyOpen),
        OpenFailure::Locked(_)
    ));
    assert!(matches!(
        classify(&D::LockProtocolMismatch),
        OpenFailure::Locked(_)
    ));
    assert!(matches!(
        classify(&D::Storage(S::Corrupted("Invalid magic number".into()))),
        OpenFailure::Corrupt(_)
    ));
    assert!(matches!(
        classify(&D::Storage(S::Io(std::io::Error::from(
            std::io::ErrorKind::UnexpectedEof
        )))),
        OpenFailure::Corrupt(_)
    ));
    assert!(matches!(
        classify(&D::Storage(S::Io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )))),
        OpenFailure::Unavailable(_)
    ));
    assert!(matches!(
        classify(&D::UpgradeRequired(1)),
        OpenFailure::Unavailable(_)
    ));
    // A sharing violation is a lock on Windows only.
    let sharing = D::Storage(S::Io(std::io::Error::from_raw_os_error(32)));
    assert_eq!(
        matches!(classify(&sharing), OpenFailure::Locked(_)),
        cfg!(windows)
    );
}

#[test]
fn a_corrupt_database_is_copied_aside_and_replaced() {
    for garbage in [
        b"definitely not a redb file, just garbage bytes".to_vec(),
        vec![0u8; 8192],
        vec![0xA5u8; 3],
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lector.redb");
        std::fs::write(&path, &garbage).expect("write");
        let store = Store::open(&path);
        assert_eq!(store.mode(), StoreMode::ReadWrite, "{garbage:?}");
        let aside = store
            .recovered_from()
            .expect("the damaged file was set aside")
            .to_path_buf();
        assert_eq!(aside.parent(), Some(dir.path()));
        assert!(aside
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("lector.redb.corrupt-")));
        assert_eq!(std::fs::read(&aside).expect("aside"), garbage);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&aside)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        // The fresh database is the file, and it works across a restart.
        store
            .recent_projects()
            .record(Path::new("/x"))
            .expect("record");
        drop(store);
        let again = Store::open(&path);
        assert_eq!(again.recovered_from(), None);
        assert_eq!(again.recent_projects().list(), vec![PathBuf::from("/x")]);
    }
}

#[test]
fn a_database_from_a_newer_lector_is_read_but_never_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    {
        let store = Store::open(&path);
        store
            .recent_projects()
            .record(Path::new("/kept"))
            .expect("record");
        let tx = store.db().raw().begin_write().expect("write");
        tx.open_table(crate::db::META)
            .expect("meta")
            .insert("schema_version", crate::db::SCHEMA_VERSION + 1)
            .expect("insert");
        tx.commit().expect("commit");
    }
    let before = std::fs::read(&path).expect("read");
    let store = Store::open(&path);
    let StoreMode::ReadOnly { reason } = store.mode() else {
        panic!("expected read-only state, got {:?}", store.mode());
    };
    assert!(reason.contains("newer"), "{reason}");
    // Seeded from the file...
    assert_eq!(store.recent_projects().list(), vec![PathBuf::from("/kept")]);
    // ...changes stay in memory.
    store
        .recent_projects()
        .record(Path::new("/memory"))
        .expect("record");
    assert_eq!(store.recent_projects().list()[0], PathBuf::from("/memory"));
    let legacy = dir.path().join("notices.json");
    std::fs::write(&legacy, "{}").expect("legacy");
    assert_eq!(store.import_legacy(dir.path()), ImportReport::default());
    drop(store);
    assert_eq!(
        std::fs::read(&path).expect("read"),
        before,
        "file untouched"
    );
    assert!(legacy.exists(), "not imported, not renamed");
    assert_eq!(
        listing(dir.path()),
        vec![
            ("lector.redb".into(), before.len() as u64),
            ("notices.json".into(), 2)
        ]
    );
}

// ---- real processes ---------------------------------------------------------------------------

const CHILD_ENV: &str = "LECTOR_STORE_TEST_CHILD";
const CHILD_DB_ENV: &str = "LECTOR_STORE_TEST_DB";
const READY: &str = "LECTOR-STORE-CHILD-READY";

/// Not a test of its own: the body of the child processes the tests below spawn (this test
/// binary re-run with `CHILD_ENV` set). Does nothing in a normal run.
#[test]
fn child_process_entry() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    let path = PathBuf::from(std::env::var_os(CHILD_DB_ENV).expect("db path"));
    let wait_for_release = || {
        println!("{READY}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        // Returns when the parent closes our stdin.
        while std::io::stdin()
            .read_line(&mut line)
            .map(|n| n > 0)
            .unwrap_or(false)
        {}
    };
    match mode.as_str() {
        // Holds the file the way a process that excludes others does (redb's ExclusiveWriter:
        // platforms without byte-range locks, redb before 4.3).
        "hold-exclusive" => {
            let db = crate::db::builder(redb::ConcurrencyMode::ExclusiveWriter)
                .create(&path)
                .expect("exclusive open");
            wait_for_release();
            drop(db);
        }
        // A second Lector: shares the file, writes, then checks it sees the parent's write.
        "share" => {
            let store = Store::open(&path);
            assert_eq!(store.mode(), StoreMode::ReadWrite);
            store
                .notices::<Counter>()
                .put(&Counter {
                    n: 1,
                    seen: vec!["child".into()],
                })
                .expect("put");
            wait_for_release();
            let seen = store
                .notices::<Counter>()
                .get()
                .expect("get")
                .unwrap_or_default();
            if !seen.seen.iter().any(|s| s == "parent") {
                std::process::exit(3);
            }
        }
        // Concurrent read-modify-writes on one key from another process.
        "increment" => {
            let store = Store::open(&path);
            let ks = Keyspace::<Counter>::new(store.db().clone(), &concerns::NOTICES);
            for i in 0..20 {
                ks.update("shared", |v| {
                    let v = v.get_or_insert_with(Counter::default);
                    v.n += 1;
                    v.seen.push(format!("child-{}-{i}", std::process::id()));
                })
                .expect("update");
            }
        }
        // Starts on a (possibly corrupt) file at the same time as others, writes its own key,
        // and checks the key is still in the database it uses when released.
        "recover-write" => {
            // Start together: wait for the parent's go file.
            let mut go = path.clone().into_os_string();
            go.push(".go");
            let go = PathBuf::from(go);
            while !go.exists() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let store = Store::open(&path);
            if store.mode() != StoreMode::ReadWrite {
                std::process::exit(4);
            }
            let root = PathBuf::from(format!("/child/{}", std::process::id()));
            store
                .terminal_sessions()
                .save(&root, &[tab("/w")])
                .expect("save");
            wait_for_release();
            if store.terminal_sessions().tabs(&root).is_empty() {
                std::process::exit(5);
            }
        }
        other => panic!("unknown child mode {other}"),
    }
}

fn spawn_child(mode: &str, db: &Path) -> std::process::Child {
    let exe = std::env::current_exe().expect("current exe");
    Command::new(exe)
        .args([
            "--exact",
            "store::tests::child_process_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, mode)
        .env(CHILD_DB_ENV, db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child")
}

/// Waits until the child printed the ready line.
fn wait_ready(child: &mut std::process::Child) {
    let out = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(out).lines();
    for line in lines.by_ref() {
        if line.expect("line").contains(READY) {
            // Keep draining in the background so the child never blocks on a full pipe.
            std::thread::spawn(move || for _ in lines {});
            return;
        }
    }
    panic!("the child exited before it was ready");
}

fn release(mut child: std::process::Child) -> std::process::ExitStatus {
    drop(child.stdin.take());
    child.wait().expect("wait")
}

#[test]
fn a_database_locked_by_another_process_gives_read_only_state_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    {
        let store = Store::open(&path);
        store
            .recent_projects()
            .record(Path::new("/before"))
            .expect("record");
    }
    // A legacy file that a writable process would import and rename.
    let legacy = dir.path().join("recent.json");
    std::fs::write(&legacy, r#"{"projects":["/legacy"]}"#).expect("legacy");
    let mut child = spawn_child("hold-exclusive", &path);
    wait_ready(&mut child);
    let before = listing(dir.path());

    let store = Store::open(&path);
    let StoreMode::ReadOnly { reason } = store.mode() else {
        panic!("expected read-only state, got {:?}", store.mode());
    };
    assert!(reason.contains("another process"), "{reason}");
    assert_eq!(store.recovered_from(), None);
    // The process works, in memory.
    store
        .recent_projects()
        .record(Path::new("/memory"))
        .expect("record");
    assert_eq!(store.recent_projects().list()[0], PathBuf::from("/memory"));
    store
        .terminal_sessions()
        .save(Path::new("/p"), &[tab("/p")])
        .expect("save");
    assert_eq!(store.import_legacy(dir.path()), ImportReport::default());
    drop(store);

    assert_eq!(listing(dir.path()), before, "nothing written anywhere");
    assert!(release(child).success());
    // The file still holds what it held, and nothing of the read-only process.
    let store = Store::open(&path);
    assert_eq!(store.mode(), StoreMode::ReadWrite);
    assert_eq!(
        store.recent_projects().list(),
        vec![PathBuf::from("/before")]
    );
    assert!(store.terminal_sessions().tabs(Path::new("/p")).is_empty());
    assert!(legacy.exists());
}

#[test]
fn two_processes_share_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    let mut child = spawn_child("share", &path);
    wait_ready(&mut child);
    let store = Store::open(&path);
    assert_eq!(store.mode(), StoreMode::ReadWrite);
    let notices = store.notices::<Counter>();
    assert_eq!(
        notices.get().expect("get").map(|c| c.seen),
        Some(vec!["child".to_string()])
    );
    notices
        .update(|v| {
            v.get_or_insert_with(Counter::default)
                .seen
                .push("parent".into())
        })
        .expect("update");
    assert!(release(child).success(), "the child saw the parent's write");
}

#[test]
fn updates_from_several_processes_lose_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    let store = Store::open(&path);
    let children: Vec<_> = (0..2).map(|_| spawn_child("increment", &path)).collect();
    let ks = Keyspace::<Counter>::new(store.db().clone(), &concerns::NOTICES);
    for i in 0..20 {
        ks.update("shared", |v| {
            let v = v.get_or_insert_with(Counter::default);
            v.n += 1;
            v.seen.push(format!("parent-{i}"));
        })
        .expect("update");
    }
    for child in children {
        assert!(release(child).success());
    }
    let shared = ks.get("shared").expect("get").expect("present");
    assert_eq!(shared.n, 60);
    assert_eq!(shared.seen.len(), 60);
}

/// A second handle on the same file inside one process (the app opens it once; tests and
/// tools might not): it works, shares or is read-only, and never leaves a copy behind.
#[test]
fn a_second_open_in_the_same_process_never_crashes_or_copies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    let a = Store::open(&path);
    a.recent_projects().record(Path::new("/a")).expect("record");
    let b = Store::open(&path);
    match b.mode() {
        StoreMode::ReadWrite => {
            assert_eq!(b.recent_projects().list(), vec![PathBuf::from("/a")]);
            b.recent_projects().record(Path::new("/b")).expect("record");
            assert_eq!(a.recent_projects().list()[0], PathBuf::from("/b"));
        }
        StoreMode::ReadOnly { .. } => {
            b.recent_projects().record(Path::new("/b")).expect("memory");
            assert_eq!(a.recent_projects().list(), vec![PathBuf::from("/a")]);
        }
    }
    drop((a, b));
    let names: Vec<String> = listing(dir.path()).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["lector.redb".to_string()]);
}

// ---- recovery between processes, file modes ------------------------------------------------

fn aside_copies(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read_dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("lector.redb.corrupt-"))
        })
        .collect();
    v.sort();
    v
}

/// Four processes start at once on one damaged file: exactly one moves it aside, all of them
/// end up in one shared fresh database, and every process's write is in it.
#[test]
fn processes_recovering_one_corrupt_file_together_share_one_fresh_database() {
    for _ in 0..3 {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lector.redb");
        let garbage = b"definitely not a redb file, just garbage bytes".repeat(20);
        std::fs::write(&path, &garbage).expect("write");
        let mut children: Vec<_> = (0..4)
            .map(|_| spawn_child("recover-write", &path))
            .collect();
        // Let every child start, then release them at once.
        std::thread::sleep(std::time::Duration::from_millis(400));
        std::fs::write(dir.path().join("lector.redb.go"), b"").expect("go");
        for child in &mut children {
            wait_ready(child);
        }
        let pids: Vec<u32> = children.iter().map(|c| c.id()).collect();
        for child in children {
            let status = release(child);
            assert!(status.success(), "child failed: {status:?}");
        }
        std::fs::remove_file(dir.path().join("lector.redb.go")).expect("go");
        let asides = aside_copies(dir.path());
        assert_eq!(asides.len(), 1, "{asides:?}");
        assert_eq!(std::fs::read(&asides[0]).expect("aside"), garbage);
        let store = Store::open(&path);
        assert_eq!(store.mode(), StoreMode::ReadWrite);
        assert_eq!(store.recovered_from(), None);
        for pid in pids {
            assert_eq!(
                store
                    .terminal_sessions()
                    .tabs(Path::new(&format!("/child/{pid}")))
                    .len(),
                1,
                "the write of process {pid} survived"
            );
        }
    }
}

/// A file another process is still writing (its header not complete yet) looks damaged for a
/// moment: it must not be moved aside.
#[test]
fn a_file_being_initialised_is_not_moved_aside() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    // A valid database, prepared elsewhere.
    let ready = dir.path().join("ready.redb");
    {
        let store = Store::open(&ready);
        store
            .recent_projects()
            .record(Path::new("/kept"))
            .expect("record");
    }
    // What a half-initialised file looks like: a truncated header.
    std::fs::write(&path, [0x72u8, 0x65, 0x64]).expect("partial");
    let finisher = {
        let (ready, path) = (ready.clone(), path.clone());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            std::fs::rename(&ready, &path).expect("finish");
        })
    };
    let store = Store::open(&path);
    finisher.join().expect("join");
    assert_eq!(store.mode(), StoreMode::ReadWrite);
    assert_eq!(store.recovered_from(), None);
    assert!(aside_copies(dir.path()).is_empty());
    assert_eq!(store.recent_projects().list(), vec![PathBuf::from("/kept")]);
}

#[cfg(unix)]
#[test]
fn the_database_and_its_aside_copy_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    let dir = tempfile::tempdir().expect("tempdir");
    // A new database is created owner-only, whatever the umask.
    let path = dir.path().join("lector.redb");
    drop(Store::open(&path));
    assert_eq!(mode(&path), 0o600);
    // An existing database keeps the mode its owner gave it.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
    drop(Store::open(&path));
    assert_eq!(mode(&path), 0o640);
    // A damaged world-readable file is set aside owner-only, and replaced by a private one.
    std::fs::write(&path, b"garbage garbage garbage").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let store = Store::open(&path);
    let aside = store.recovered_from().expect("set aside").to_path_buf();
    assert_eq!(mode(&aside), 0o600);
    assert_eq!(mode(&path), 0o600);
}

// ---- read-only state: indicator, headless warning, health probe ------------------------------

#[test]
fn the_indicator_and_the_headless_warning_say_what_is_lost_and_why() {
    assert_eq!(StoreMode::ReadWrite.indicator(), None);
    assert_eq!(StoreMode::ReadWrite.headless_warning(), None);
    let ro = StoreMode::ReadOnly {
        reason: "the database is in use by another process that excludes this one".into(),
    };
    let i = ro.indicator().expect("indicator");
    assert_eq!(i.label, "State not saved");
    assert!(
        i.detail.contains("review marks, comments and undo history"),
        "{}",
        i.detail
    );
    assert!(i.detail.contains("lost when Lector quits"), "{}", i.detail);
    assert!(i.detail.contains("another process"), "{}", i.detail);
    let w = ro.headless_warning().expect("warning");
    assert!(w.starts_with("lector: warning: "), "{w}");
    assert!(
        w.contains("another process") && w.contains("not kept"),
        "{w}"
    );
    assert!(!w.contains('\n'));
}

#[test]
fn a_read_only_store_reports_its_first_state_write_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    {
        let store = Store::open(&path);
        let tx = store.db().raw().begin_write().expect("write");
        tx.open_table(crate::db::META)
            .expect("meta")
            .insert("schema_version", crate::db::SCHEMA_VERSION + 1)
            .expect("insert");
        tx.commit().expect("commit");
    }
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store = Store::open(&path);
    assert!(matches!(store.mode(), StoreMode::ReadOnly { .. }));
    let c = calls.clone();
    store.on_read_only_write(move || {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    // Reads and cache writes are not state changes.
    let _ = store.recent_projects().list();
    store.db().cache_put("k", b"v").expect("cache");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    store
        .recent_projects()
        .record(Path::new("/a"))
        .expect("record");
    store.db().kv_put("x", Some(b"1")).expect("kv");
    store
        .terminal_sessions()
        .save(Path::new("/p"), &[])
        .expect("save");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    // In read-write state the hook never runs.
    let rw = Store::in_memory();
    let c = calls.clone();
    rw.on_read_only_write(move || {
        c.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
    });
    rw.recent_projects()
        .record(Path::new("/a"))
        .expect("record");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn the_health_probe_creates_nothing_and_names_the_problem() {
    use super::open::Probe;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lector.redb");
    assert_eq!(Store::probe(&path), Probe::Missing);
    assert!(!path.exists(), "the probe creates nothing");
    drop(Store::open(&path));
    assert_eq!(Store::probe(&path), Probe::Healthy);
    assert_eq!(Probe::Healthy.line(), None);
    // Held by an excluding process: the same line as the window's indicator.
    let mut child = spawn_child("hold-exclusive", &path);
    wait_ready(&mut child);
    let probe = Store::probe(&path);
    assert!(matches!(probe, Probe::ReadOnly(_)), "{probe:?}");
    let line = probe.line().expect("line");
    assert!(
        line.contains("lost when Lector quits") && line.contains("another process"),
        "{line}"
    );
    assert!(release(child).success());
    // Damaged: reported, not touched.
    std::fs::write(&path, b"garbage garbage garbage").expect("write");
    let probe = Store::probe(&path);
    assert!(matches!(probe, Probe::Damaged(_)), "{probe:?}");
    assert!(probe.line().expect("line").contains("damaged"));
    assert_eq!(
        std::fs::read(&path).expect("read"),
        b"garbage garbage garbage"
    );
    assert!(aside_copies(dir.path()).is_empty());
}

// ---- the health probe agrees with Store::open --------------------------------------------

/// Every file in `dir` (recursively): name, bytes, modification time.
fn exact_listing(dir: &Path) -> Vec<(String, Vec<u8>, Option<std::time::SystemTime>)> {
    let mut out: Vec<_> = walk(dir)
        .into_iter()
        .map(|p| {
            let meta = std::fs::symlink_metadata(&p).ok();
            let bytes = if p.is_file() {
                std::fs::read(&p).unwrap_or_default()
            } else {
                Vec::new()
            };
            (
                p.to_string_lossy().into_owned(),
                bytes,
                meta.and_then(|m| m.modified().ok()),
            )
        })
        .collect();
    out.sort();
    out
}

/// What `Store::open` did, in the probe's terms.
fn agrees(probe: &super::open::Probe, store: &Store) -> bool {
    use super::open::Probe;
    match probe {
        Probe::Missing | Probe::Healthy => {
            store.mode() == StoreMode::ReadWrite && store.recovered_from().is_none()
        }
        Probe::Damaged(_) => {
            store.mode() == StoreMode::ReadWrite && store.recovered_from().is_some()
        }
        Probe::ReadOnly(_) => matches!(store.mode(), StoreMode::ReadOnly { .. }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ProbeCase {
    Missing,
    Healthy,
    NewerSchema,
    Locked,
    Damaged,
    #[cfg(unix)]
    Unreadable,
    #[cfg(unix)]
    ReadableNotWritable,
    #[cfg(unix)]
    HealthyInReadOnlyDir,
    #[cfg(unix)]
    MissingInReadOnlyDir,
    #[cfg(unix)]
    DamagedInReadOnlyDir,
}

#[cfg(unix)]
fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// The probe of `lector health` says what `Store::open` will do, for every state of the file,
/// and leaves the folder exactly as it was (names, bytes, modification times).
#[test]
fn the_health_probe_agrees_with_open_and_changes_nothing() {
    use super::open::Probe;
    let mut cases = vec![
        ProbeCase::Missing,
        ProbeCase::Healthy,
        ProbeCase::NewerSchema,
        ProbeCase::Locked,
        ProbeCase::Damaged,
    ];
    #[cfg(unix)]
    cases.extend([
        ProbeCase::Unreadable,
        ProbeCase::ReadableNotWritable,
        ProbeCase::HealthyInReadOnlyDir,
        ProbeCase::MissingInReadOnlyDir,
        ProbeCase::DamagedInReadOnlyDir,
    ]);
    let mut skipped = Vec::new();
    for case in cases {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("data");
        std::fs::create_dir(&dir).expect("mkdir");
        let path = dir.join("lector.redb");
        let healthy = |path: &Path| {
            let store = Store::open(path);
            store
                .recent_projects()
                .record(Path::new("/kept"))
                .expect("record");
        };
        let mut child = None;
        // Restores permissions so that the store can be opened and the folder cleaned up.
        let mut restore: Box<dyn Fn()> = Box::new(|| {});
        match case {
            ProbeCase::Missing => {}
            ProbeCase::Healthy => healthy(&path),
            ProbeCase::NewerSchema => {
                healthy(&path);
                let store = Store::open(&path);
                let tx = store.db().raw().begin_write().expect("write");
                tx.open_table(crate::db::META)
                    .expect("meta")
                    .insert("schema_version", crate::db::SCHEMA_VERSION + 1)
                    .expect("insert");
                tx.commit().expect("commit");
            }
            ProbeCase::Locked => {
                healthy(&path);
                let mut c = spawn_child("hold-exclusive", &path);
                wait_ready(&mut c);
                child = Some(c);
            }
            ProbeCase::Damaged => std::fs::write(&path, b"garbage garbage garbage").expect("write"),
            #[cfg(unix)]
            ProbeCase::Unreadable | ProbeCase::ReadableNotWritable => {
                healthy(&path);
                let mode = if case == ProbeCase::Unreadable {
                    0o000
                } else {
                    0o444
                };
                chmod(&path, mode);
                let p = path.clone();
                restore = Box::new(move || chmod(&p, 0o600));
                // Privileged (root, CAP_DAC_OVERRIDE): permissions do not apply.
                if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
                    restore();
                    skipped.push(case);
                    continue;
                }
            }
            #[cfg(unix)]
            ProbeCase::HealthyInReadOnlyDir
            | ProbeCase::MissingInReadOnlyDir
            | ProbeCase::DamagedInReadOnlyDir => {
                match case {
                    ProbeCase::HealthyInReadOnlyDir => healthy(&path),
                    ProbeCase::DamagedInReadOnlyDir => {
                        std::fs::write(&path, b"garbage garbage garbage").expect("write")
                    }
                    _ => {}
                }
                chmod(&dir, 0o555);
                let d = dir.clone();
                restore = Box::new(move || chmod(&d, 0o755));
                let privileged = std::fs::File::create(dir.join("probe-privilege")).is_ok();
                if privileged {
                    let _ = std::fs::remove_file(dir.join("probe-privilege"));
                    restore();
                    skipped.push(case);
                    continue;
                }
            }
        }
        let before = exact_listing(tmp.path());
        let probe = Store::probe(&path);
        assert_eq!(
            exact_listing(tmp.path()),
            before,
            "{case:?}: the probe changed nothing"
        );
        let expected_kind = match case {
            ProbeCase::Missing => "missing",
            ProbeCase::Healthy => "healthy",
            ProbeCase::Damaged => "damaged",
            #[cfg(unix)]
            ProbeCase::HealthyInReadOnlyDir => "healthy",
            _ => "read-only",
        };
        let kind = match &probe {
            Probe::Missing => "missing",
            Probe::Healthy => "healthy",
            Probe::Damaged(_) => "damaged",
            Probe::ReadOnly(_) => "read-only",
        };
        assert_eq!(kind, expected_kind, "{case:?}: {probe:?}");
        assert_eq!(
            probe.line().is_some(),
            matches!(kind, "damaged" | "read-only")
        );
        let store = Store::open(&path);
        assert!(
            agrees(&probe, &store),
            "{case:?}: probe {probe:?}, open {:?} (recovered {:?})",
            store.mode(),
            store.recovered_from()
        );
        drop(store);
        restore();
        if let Some(c) = child {
            assert!(release(c).success());
        }
    }
    if !skipped.is_empty() {
        // An explicit note, not a silent pass: these need an unprivileged user.
        eprintln!(
            "the_health_probe_agrees_with_open_and_changes_nothing: skipped {skipped:?} (running with privileges that ignore file permissions)"
        );
    }
}
