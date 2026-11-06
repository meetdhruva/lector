use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use super::*;
use crate::commands::{CommandContext, Source};
use crate::CoreOptions;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "gc.auto=0"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(dir.path()).expect("canonical");
    let g = |a: &[&str]| git(&root, a);
    g(&["init", "-q", "-b", "main"]);
    g(&["config", "user.name", "Ada"]);
    g(&["config", "user.email", "ada@example.com"]);
    g(&["config", "commit.gpgsign", "false"]);
    (dir, root)
}

fn body(n: usize) -> String {
    (0..n)
        .map(|i| format!("fn item_{i}() -> u32 {{ {i} * 7 }}\n"))
        .collect()
}

fn anchor_at(text: &str, line: u32) -> Anchor {
    Anchor::capture(
        "src/lib.rs",
        DiffSide::New,
        text,
        line,
        None,
        "worktree",
        None,
    )
    .expect("line exists")
}

#[test]
fn reanchor_shifted_lines_is_exact() {
    let old = body(30);
    let a = anchor_at(&old, 10);
    let new = format!("// header\n// more\n{old}");
    for old_text in [Some(old.as_str()), None] {
        let r = reanchor_anchor(&a, old_text, &new, MIN_CONFIDENCE);
        assert_eq!(r.line, Some(12));
        assert_eq!(r.matched, AnchorMatch::Exact);
        assert!(r.confidence > 0.9);
    }
}

#[test]
fn reanchor_moved_block() {
    let lines: Vec<String> = body(40).lines().map(str::to_string).collect();
    let old = lines.join("\n") + "\n";
    let a = anchor_at(&old, 6);
    // Move lines 5..10 to the end.
    let mut moved: Vec<String> = lines[..5].to_vec();
    moved.extend(lines[10..].iter().cloned());
    moved.extend(lines[5..10].iter().cloned());
    let new = moved.join("\n") + "\n";
    let r = reanchor_anchor(&a, Some(&old), &new, MIN_CONFIDENCE);
    assert_eq!(r.line, Some(36));
    assert_eq!(r.matched, AnchorMatch::Moved);
}

#[test]
fn reanchor_edited_line_is_fuzzy_and_deleted_is_lost() {
    let old = body(30);
    let a = anchor_at(&old, 10);
    let edited = old.replace(
        "fn item_10() -> u32 { 10 * 7 }",
        "fn item_10() -> u32 { 10 * 8 }",
    );
    let r = reanchor_anchor(&a, Some(&old), &edited, MIN_CONFIDENCE);
    assert_eq!(r.line, Some(10));
    assert_eq!(r.matched, AnchorMatch::Fuzzy);
    assert!(r.confidence < 1.0 && r.confidence >= MIN_CONFIDENCE);

    let gone: String = (0..30)
        .filter(|i| !(8..=12).contains(i))
        .map(|i| format!("fn item_{i}() -> u32 {{ {i} * 7 }}\n"))
        .collect();
    let r = reanchor_anchor(&a, Some(&old), &gone, MIN_CONFIDENCE);
    assert_eq!(r.matched, AnchorMatch::Lost);
    assert_eq!(r.line, None);
}

#[test]
fn store_crud_drafts_and_persistence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("t.redb");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).expect("mkdir");
    let text = body(5);
    let id;
    {
        let store = CommentStore::new(Db::open(&db_path).expect("db"));
        let events = Arc::new(Mutex::new(0));
        let e = events.clone();
        let _sub = store.subscribe(move |_| *e.lock() += 1);
        assert!(store
            .add(NewThread {
                repo: repo.clone(),
                anchor: anchor_at(&text, 1),
                body: "  ".into(),
                ..Default::default()
            })
            .is_err());
        let t = store
            .add(NewThread {
                repo: repo.clone(),
                anchor: anchor_at(&text, 1),
                body: "Why `7`?".into(),
                author: "ada".into(),
                draft: true,
                ..Default::default()
            })
            .expect("add");
        id = t.id;
        assert!(t.is_draft());
        let t = store
            .reply(&repo, id, "Magic number", "bob", false)
            .expect("reply");
        assert_eq!(t.comments.len(), 2);
        assert!(!t.is_draft());
        assert_eq!(t.draft_count(), 1);
        let t = store.set_resolved(&repo, id, true).expect("resolve");
        assert_eq!(t.state, ThreadState::Resolved);
        let other = store
            .add(NewThread {
                repo: repo.clone(),
                anchor: Anchor {
                    changeset: "commit:abc".into(),
                    ..anchor_at(&text, 3)
                },
                body: "other".into(),
                ..Default::default()
            })
            .expect("add");
        assert_eq!(
            store
                .threads(&repo, &ThreadFilter::changeset("worktree"))
                .expect("list")
                .len(),
            1
        );
        let drafts = ThreadFilter {
            drafts_only: true,
            ..Default::default()
        };
        assert_eq!(store.threads(&repo, &drafts).expect("list").len(), 1);
        let pub_t = store
            .mark_published(
                &repo,
                id,
                Some("RT_1".into()),
                &[(t.comments[0].id, "C1".into())],
            )
            .expect("publish");
        assert_eq!(pub_t.draft_count(), 0);
        assert_eq!(pub_t.comments[0].remote_id.as_deref(), Some("C1"));
        let left = store
            .delete_comment(&repo, other.id, other.comments[0].id)
            .expect("delete");
        assert!(left.is_none());
        assert!(matches!(
            store.thread(&repo, other.id),
            Err(CommentError::NotFound(_))
        ));
        assert!(*events.lock() >= 6);
    }
    // Survives reopening the database.
    let store = CommentStore::new(Db::open(&db_path).expect("db"));
    let t = store.thread(&repo, id).expect("persisted");
    assert_eq!(t.comments[1].body, "Magic number");
    assert_eq!(t.state, ThreadState::Resolved);
    assert_eq!(t.remote_id.as_deref(), Some("RT_1"));
}

#[test]
fn legacy_comments_become_threads() {
    let db = Db::in_memory();
    let repo = tempfile::tempdir().expect("tempdir");
    let key = repo_key(repo.path());
    db.add_comment(crate::db::Comment {
        id: 0,
        repo: key.clone(),
        changeset: "worktree".into(),
        path: "a.rs".into(),
        line: 4,
        anchor_text: "let x = 1;".into(),
        body: "old note".into(),
        created: 0,
        resolved: false,
    })
    .expect("legacy");
    let store = CommentStore::new(db.clone());
    let v = store
        .threads(repo.path(), &ThreadFilter::default())
        .expect("list");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].comments[0].body, "old note");
    assert!(db.comments(&key, None).expect("legacy").is_empty());
}

#[test]
fn reanchor_path_follows_worktree_edits_and_marks_outdated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().to_path_buf();
    let store = CommentStore::new(Db::in_memory());
    let v1 = body(40);
    store.remember_text(&v1);
    let keep = store
        .add(NewThread {
            repo: repo.clone(),
            anchor: anchor_at(&v1, 20),
            body: "keep".into(),
            ..Default::default()
        })
        .expect("add");
    let lose = store
        .add(NewThread {
            repo: repo.clone(),
            anchor: anchor_at(&v1, 5),
            body: "lose".into(),
            ..Default::default()
        })
        .expect("add");
    // Edit: delete lines 3..8, insert three lines at the top.
    let v2: String = std::iter::once("use std::fmt;\n\n// docs\n".to_string())
        .chain(
            (0..40)
                .filter(|i| !(3..8).contains(i))
                .map(|i| format!("fn item_{i}() -> u32 {{ {i} * 7 }}\n")),
        )
        .collect();
    let reports = store
        .reanchor_path(
            &repo,
            Some("worktree"),
            "src/lib.rs",
            DiffSide::New,
            &v2,
            None,
            &|_| None,
        )
        .expect("reanchor");
    assert_eq!(reports.len(), 2);
    let k = store.thread(&repo, keep.id).expect("keep");
    assert_eq!(k.anchor.line, 20 - 5 + 3);
    assert_eq!(k.matched, AnchorMatch::Exact);
    assert_eq!(k.state, ThreadState::Open);
    let l = store.thread(&repo, lose.id).expect("lose");
    assert_eq!(l.state, ThreadState::Outdated);
    assert_eq!(l.original.text, "fn item_5() -> u32 { 5 * 7 }");
    // Same revision again: nothing to do for anchored threads.
    let again = store
        .reanchor_path(
            &repo,
            Some("worktree"),
            "src/lib.rs",
            DiffSide::New,
            &v2,
            None,
            &|_| None,
        )
        .expect("reanchor");
    assert!(again.iter().all(|r| r.thread == lose.id));
    // The code comes back: the outdated thread re-attaches from its original anchor.
    let reports = store
        .reanchor_path(
            &repo,
            Some("worktree"),
            "src/lib.rs",
            DiffSide::New,
            &v1,
            None,
            &|_| None,
        )
        .expect("reanchor");
    assert!(!reports.is_empty());
    let l = store.thread(&repo, lose.id).expect("lose");
    assert_eq!(l.state, ThreadState::Open);
    assert_eq!(l.anchor.line, 5);
}

#[test]
fn sync_changeset_survives_a_rebase() {
    let (_dir, root) = repo_fixture();
    let g = |a: &[&str]| git(&root, a);
    fs::create_dir_all(root.join("src")).expect("mkdir");
    fs::write(root.join("src/lib.rs"), body(40)).expect("write");
    g(&["add", "."]);
    g(&["commit", "-q", "-m", "base"]);
    g(&["checkout", "-q", "-b", "feature"]);
    let feature = body(40).replace(
        "fn item_30() -> u32 { 30 * 7 }",
        "fn item_30() -> u32 { 30 * 7 + feature() }",
    );
    fs::write(root.join("src/lib.rs"), &feature).expect("write");
    g(&["commit", "-q", "-am", "feature"]);

    let repo = Repo::discover(&root).expect("repo");
    let cs = Changeset::BranchVsBase {
        branch: "feature".into(),
        base: "main".into(),
    };
    let key = lector_workspace::review::changeset_key(&cs);
    let store = CommentStore::new(Db::in_memory());
    let (text, blob) = side_text(&repo, Some(&cs), "src/lib.rs", DiffSide::New).expect("text");
    assert!(blob.is_some());
    let anchor =
        Anchor::capture("src/lib.rs", DiffSide::New, &text, 30, None, &key, blob).expect("anchor");
    let t = store
        .add(NewThread {
            repo: root.clone(),
            anchor,
            body: "Is feature() cheap?".into(),
            ..Default::default()
        })
        .expect("add");

    // main gains lines at the top; feature is rebased onto it.
    g(&["checkout", "-q", "main"]);
    let main2 = format!("//! crate docs\n//! more docs\n\n{}", body(40));
    fs::write(root.join("src/lib.rs"), main2).expect("write");
    g(&["commit", "-q", "-am", "docs"]);
    g(&["checkout", "-q", "feature"]);
    g(&["rebase", "-q", "main"]);

    let repo = Repo::discover(&root).expect("repo");
    let reports = store.sync_changeset(&repo, &key, &cs).expect("sync");
    assert_eq!(reports.len(), 1);
    let t = store.thread(&root, t.id).expect("thread");
    assert_eq!(t.anchor.line, 33);
    assert_eq!(t.matched, AnchorMatch::Exact);
    assert_eq!(t.anchor.text, "fn item_30() -> u32 { 30 * 7 + feature() }");
    assert_eq!(t.original.line, 30);
}

#[test]
fn commands_round_trip() {
    let (_dir, root) = repo_fixture();
    fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("write");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    fs::write(root.join("a.txt"), "one\ntwo!\nthree\nfour\n").expect("write");
    let core = AppCore::open(CoreOptions::for_tests());
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    let project = lector_workspace::Project::open(&root, settings).expect("project");
    let cx = CommandContext::new(core.clone(), Some(project), Source::Internal);
    let reg = core.registry();
    for id in [
        "comments.list",
        "comments.add",
        "comments.reply",
        "comments.resolve",
        "comments.delete",
        "comments.export",
    ] {
        assert!(reg.get(id).is_some(), "{id} registered");
    }
    let out = reg
        .invoke(
            "comments.add",
            json!({"path": "a.txt", "line": 1, "body": "Why the bang?", "draft": true}),
            &cx,
        )
        .expect("add");
    let id = out.data["id"].as_u64().expect("id");
    assert_eq!(out.data["snippet"], "two!");
    assert_eq!(out.data["draft"], true);
    reg.invoke(
        "comments.reply",
        json!({"thread": id, "body": "Emphasis."}),
        &cx,
    )
    .expect("reply");
    let list = reg.invoke("comments.list", json!({}), &cx).expect("list");
    assert_eq!(
        list.data["threads"][0]["comments"].as_array().map(Vec::len),
        Some(2)
    );
    assert!(reg
        .invoke(
            "comments.add",
            json!({"path": "a.txt", "line": 99, "body": "x"}),
            &cx
        )
        .is_err());
    let md = reg
        .invoke("comments.export", json!({}), &cx)
        .expect("export");
    let text = md.data["markdown"].as_str().unwrap_or_default();
    assert!(
        text.contains("a.txt:2") && text.contains("Why the bang?"),
        "{text}"
    );
    reg.invoke("comments.resolve", json!({"thread": id}), &cx)
        .expect("resolve");
    let open = reg
        .invoke("comments.list", json!({"state": "open"}), &cx)
        .expect("list");
    assert_eq!(open.data["threads"].as_array().map(Vec::len), Some(0));
    reg.invoke("comments.delete", json!({"thread": id}), &cx)
        .expect("delete");
    let all = reg.invoke("comments.list", json!({}), &cx).expect("list");
    assert_eq!(all.data["threads"].as_array().map(Vec::len), Some(0));
    // Nothing was written into the repository.
    assert!(git(&root, &["status", "--porcelain"]).contains("a.txt"));
}
