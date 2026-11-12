//! Journey: leave a review comment on a changed line; unrelated edits and new commits move the
//! line but the comment stays anchored to it.

use lector_app::comments::{AnchorMatch, CommentStore, ThreadFilter, ThreadState};
use lector_git::{Changeset, Repo as GitRepo};
use serde_json::json;

use crate::fixture::{norm, Repo, Session};

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn a_comment_survives_an_unrelated_edit_in_the_working_tree() {
    let repo = Repo::init("comments");
    repo.write("notes.txt", &numbered(20));
    repo.commit_all("init");
    // The reviewed change: line 10 is edited in the working tree.
    repo.write(
        "notes.txt",
        &numbered(20).replace("line 10", "line ten (edited)"),
    );
    let s = Session::open(&repo);

    let added = s.ok(
        "comments.add",
        json!({"path": "notes.txt", "line": 9, "body": "Why was this renamed?"}),
    );
    let id = added.data["id"].as_u64().expect("thread id");
    assert_eq!(added.data["line"], 9);
    assert_eq!(added.data["state"], "open");
    assert_eq!(added.data["changeset"], "worktree");
    assert_eq!(
        repo.git(&["status", "--porcelain"]).trim(),
        "M notes.txt",
        "comments never touch the repository"
    );

    // An unrelated edit through the document store: two lines at the top of the file.
    s.ok(
        "document.apply_edits",
        json!({"path": "notes.txt", "edits": [{"lines": [0, 0], "new_text": "// header 1\n// header 2\n"}]}),
    );
    s.ok("document.save", json!({"path": "notes.txt"}));
    assert!(repo
        .read("notes.txt")
        .starts_with("// header 1\n// header 2\nline 1\n"));

    // The review syncs its threads with the changed changeset, as the review tab does on refresh.
    let git_repo = GitRepo::discover(&repo.root).expect("repo");
    let store = CommentStore::for_core(&s.core);
    let reports = store
        .sync_changeset(&git_repo, "worktree", &Changeset::WorkingTree)
        .expect("sync");
    assert_eq!(reports.len(), 1);

    let thread = store.thread(&repo.root, id).expect("thread");
    assert_eq!(
        thread.anchor.line, 11,
        "moved down by the two inserted lines"
    );
    assert_eq!(thread.anchor.text, "line ten (edited)");
    assert_eq!(thread.original.line, 9, "the original position is kept");
    assert_eq!(thread.state, ThreadState::Open);
    assert_eq!(thread.matched, AnchorMatch::Exact);
    let listed = s.ok("comments.list", json!({}));
    let t = &listed.data["threads"][0];
    assert_eq!(t["id"], id);
    assert_eq!(t["line"], 11);
    assert_eq!(t["state"], "open");
    assert_eq!(t["comments"][0]["body"], "Why was this renamed?");
    assert_eq!(
        norm(&repo.git(&["status", "--porcelain"])).trim(),
        "M notes.txt"
    );
}

#[test]
fn a_comment_on_a_branch_survives_a_new_commit_elsewhere() {
    let repo = Repo::init("comments-branch");
    repo.write("src/app.txt", &numbered(30));
    repo.commit_all("base");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write(
        "src/app.txt",
        &numbered(30).replace("line 20", "line 20 changed"),
    );
    repo.commit_all("feature");
    let s = Session::open(&repo);
    let key = "branch:main...feature";

    let added = s.ok(
        "comments.add",
        json!({"path": "src/app.txt", "line": 19, "body": "Needs a test.", "changeset": key}),
    );
    let id = added.data["id"].as_u64().expect("thread id");

    // A new commit on the branch adds three lines near the top and edits another place.
    let next = format!(
        "// a\n// b\n// c\n{}",
        numbered(30).replace("line 20", "line 20 changed")
    )
    .replace("line 28", "line 28 also changed");
    repo.write("src/app.txt", &next);
    repo.commit_all("more work");

    let git_repo = GitRepo::discover(&repo.root).expect("repo");
    let cs = Changeset::BranchVsBase {
        branch: "feature".into(),
        base: "main".into(),
    };
    let store = CommentStore::for_core(&s.core);
    store.sync_changeset(&git_repo, key, &cs).expect("sync");
    let thread = store.thread(&repo.root, id).expect("thread");
    assert_eq!(thread.anchor.line, 22, "three lines moved it down");
    assert_eq!(thread.anchor.text, "line 20 changed");
    assert_eq!(thread.state, ThreadState::Open);
    assert!(matches!(
        thread.matched,
        AnchorMatch::Exact | AnchorMatch::Moved
    ));
    let open = store
        .threads(&repo.root, &ThreadFilter::changeset(key))
        .expect("threads");
    assert_eq!(open.len(), 1);

    // When the commented line itself is rewritten beyond recognition, the thread says so
    // instead of pointing at the wrong line.
    let rewritten = next.replace("line 20 changed", "completely different content here");
    repo.write("src/app.txt", &rewritten);
    repo.commit_all("rewrite");
    store.sync_changeset(&git_repo, key, &cs).expect("sync");
    let thread = store.thread(&repo.root, id).expect("thread");
    assert!(
        thread.state == ThreadState::Outdated || thread.confidence < 1.0,
        "{:?} {:?} {}",
        thread.state,
        thread.matched,
        thread.confidence
    );
    assert_eq!(
        thread.original.text, "line 20 changed",
        "the original snippet is kept"
    );
}
