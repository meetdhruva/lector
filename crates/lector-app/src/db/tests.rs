use super::*;

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
