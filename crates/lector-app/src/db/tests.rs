use super::*;
use lector_config::{PaneState, ReviewStore, SessionStore, TabKind, TabState};

fn session(row: u32) -> lector_config::Session {
    lector_config::Session {
        panes: vec![PaneState {
            tabs: vec![TabState {
                kind: TabKind::File {
                    path: "/a/b.rs".into(),
                },
                scroll_row: row,
                active: true,
            }],
        }],
        ..Default::default()
    }
}

#[test]
fn file_db_persists_and_has_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/lector.redb");
    {
        let db = Db::open(&path).unwrap();
        assert_eq!(db.mode(), &DbMode::File(path.clone()));
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        db.kv_put_json("k", &vec![1, 2, 3]).unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.kv_get_json::<Vec<i32>>("k").unwrap(),
        Some(vec![1, 2, 3])
    );
    db.kv_put("k", None).unwrap();
    assert_eq!(db.kv_get("k").unwrap(), None);
}

#[test]
fn second_open_in_use_never_crashes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lector.redb");
    let a = Db::open_or_fallback(&path);
    a.kv_put("from_a", Some(b"1")).unwrap();
    // A second handle (as another process would) either shares the file or
    // falls back to a private copy — it must work either way.
    let b = Db::open_or_fallback(&path);
    b.kv_put("from_b", Some(b"2")).unwrap();
    assert_eq!(b.kv_get("from_b").unwrap(), Some(b"2".to_vec()));
    if let DbMode::TempCopy { copy, .. } = b.mode().clone() {
        assert_eq!(b.kv_get("from_a").unwrap(), Some(b"1".to_vec()));
        drop(b);
        assert!(!copy.exists(), "temp copy removed on drop");
    }
}

#[test]
fn corrupt_file_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lector.redb");
    std::fs::write(&path, b"definitely not a redb file, just garbage bytes").unwrap();
    let db = Db::open_or_fallback(&path);
    assert!(!matches!(db.mode(), DbMode::File(_)));
    db.kv_put("x", Some(b"y")).unwrap();
}

#[test]
fn review_store_backend_roundtrip() {
    let db = Db::in_memory();
    let store: ReviewStore = db.review_store();
    let repo = Path::new("/r");
    assert!(!store.is_viewed(repo, "wt", "a.rs", "h1"));
    store.set_viewed(repo, "wt", "a.rs", "h1", true).unwrap();
    assert!(store.is_viewed(repo, "wt", "a.rs", "h1"));
    assert!(!store.is_viewed(repo, "wt", "a.rs", "h2"));
    assert!(!store.is_viewed(repo, "staged", "a.rs", "h1"));
    assert_eq!(store.len(), 1);
    // Another handle over the same db sees it (shared between windows).
    assert!(db.review_store().is_viewed(repo, "wt", "a.rs", "h1"));
    store.set_viewed(repo, "wt", "a.rs", "h1", false).unwrap();
    assert!(store.is_empty());
}

#[test]
fn sessions_and_recent() {
    let db = Db::in_memory();
    let dir = tempfile::tempdir().unwrap();
    let s = db.session_store();
    assert!(s.load(dir.path()).is_none());
    s.save(dir.path(), &session(7)).unwrap();
    assert_eq!(s.load(dir.path()), Some(session(7)));

    let r = db.recent_projects();
    for i in 0..25 {
        r.record(&dir.path().join(format!("p{i}"))).unwrap();
    }
    r.record(&dir.path().join("p3")).unwrap();
    let list = r.list();
    assert_eq!(list.len(), MAX_RECENT_PROJECTS_FOR_TEST);
    assert_eq!(list[0], dir.path().join("p3"));
    assert_eq!(list[1], dir.path().join("p24"));
    assert_eq!(r.most_recent_existing(), None);
    std::fs::create_dir(dir.path().join("p24")).unwrap();
    assert_eq!(r.most_recent_existing(), Some(dir.path().join("p24")));
    r.remove(&dir.path().join("p3")).unwrap();
    assert!(!r.list().contains(&dir.path().join("p3")));
    r.remove(&dir.path().join("missing")).unwrap();
}

const MAX_RECENT_PROJECTS_FOR_TEST: usize = lector_config::MAX_RECENT_PROJECTS;

#[test]
fn imports_v03_json_once() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // v0.3 stores written by the v0.3 code paths.
    let review = ReviewStore::open(data.path().join("review.json"));
    review
        .set_viewed(Path::new("/r"), "wt", "a.rs", "h1", true)
        .unwrap();
    SessionStore::with_dir(data.path().join("sessions"))
        .save(project.path(), &session(42))
        .unwrap();
    let recent = lector_config::RecentProjects::with_file(data.path().join("recent.json"));
    recent.record(Path::new("/old")).unwrap();
    recent.record(Path::new("/new")).unwrap();

    let db = Db::in_memory();
    let n = db.import_v03(data.path()).unwrap();
    assert_eq!(n, 4);
    assert!(db
        .review_store()
        .is_viewed(Path::new("/r"), "wt", "a.rs", "h1"));
    assert_eq!(db.session_store().load(project.path()), Some(session(42)));
    assert_eq!(
        db.recent_projects().list(),
        vec![PathBuf::from("/new"), PathBuf::from("/old")]
    );
    // Second import is a no-op and does not clobber newer state.
    db.session_store()
        .save(project.path(), &session(1))
        .unwrap();
    assert_eq!(db.import_v03(data.path()).unwrap(), 0);
    assert_eq!(db.session_store().load(project.path()), Some(session(1)));
    // The JSON files are left in place.
    assert!(data.path().join("review.json").exists());
}

#[test]
fn cache_trims_oldest() {
    let db = Db::in_memory();
    for i in 0..10 {
        db.cache_put(&format!("k{i}"), &[0u8; 100]).unwrap();
    }
    assert_eq!(db.cache_get("k0").unwrap().map(|v| v.len()), Some(100));
    let removed = db.trim_cache(5 * 108).unwrap();
    assert_eq!(removed, 5);
    assert_eq!(db.cache_get("k0").unwrap(), None);
    assert!(db.cache_get("k9").unwrap().is_some());
}

#[test]
fn journal_append_list_update() {
    let db = Db::in_memory();
    assert_eq!(db.append_journal(b"a").unwrap(), 1);
    assert_eq!(db.append_journal(b"b").unwrap(), 2);
    assert_eq!(db.append_journal(b"c").unwrap(), 3);
    let all = db.journal_list(10, None).unwrap();
    assert_eq!(
        all.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    let page = db.journal_list(1, Some(3)).unwrap();
    assert_eq!(page, vec![(2, b"b".to_vec())]);
    db.journal_put(2, b"B").unwrap();
    assert_eq!(db.journal_get(2).unwrap(), Some(b"B".to_vec()));
    assert_eq!(db.journal_get(9).unwrap(), None);
}

#[test]
fn comments_crud() {
    let db = Db::in_memory();
    let c = db
        .add_comment(Comment {
            id: 0,
            repo: "/r".into(),
            changeset: "worktree".into(),
            path: "a.rs".into(),
            line: 3,
            anchor_text: "fn main() {".into(),
            body: "why?".into(),
            created: 0,
            resolved: false,
        })
        .unwrap();
    assert_eq!(c.id, 1);
    assert!(c.created > 0);
    let mut c2 = c.clone();
    c2.changeset = "staged".into();
    let c2 = db.add_comment(c2).unwrap();
    assert_eq!(db.comments("/r", None).unwrap().len(), 2);
    assert_eq!(db.comments("/r", Some("staged")).unwrap(), vec![c2.clone()]);
    assert!(db.comments("/other", None).unwrap().is_empty());
    let mut resolved = c.clone();
    resolved.resolved = true;
    db.update_comment(&resolved).unwrap();
    assert!(db.comments("/r", Some("worktree")).unwrap()[0].resolved);
    db.delete_comment("/r", c.id).unwrap();
    assert_eq!(db.comments("/r", None).unwrap(), vec![c2]);
}
