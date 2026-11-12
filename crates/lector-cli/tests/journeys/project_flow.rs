//! Journey: open a project, look at its status, see the changes, read a diff, stage one hunk,
//! commit it, find both in the activity log, undo what can be undone.

use lector_app::commands::UiRequest;
use lector_app::journal::{UndoError, UndoMode};
use serde_json::json;

use crate::fixture::{counts, norm, same_path, thirty_functions, Repo, Session};

/// `src/lib.rs` edited in two places far apart (two hunks), `notes.txt` untracked.
fn dirty_repo() -> Repo {
    let repo = Repo::init("project");
    repo.write("src/lib.rs", &thirty_functions());
    repo.write("README.md", "# project\n");
    repo.commit_all("init");
    let edited = thirty_functions()
        .replace("pub fn f3() -> u32 { 3 }", "pub fn f3() -> u32 { 33 }")
        .replace("pub fn f27() -> u32 { 27 }", "pub fn f27() -> u32 { 270 }");
    repo.write("src/lib.rs", &edited);
    repo.write("notes.txt", "todo\n");
    repo
}

#[test]
fn open_status_changes_diff_stage_hunk_commit_activity_undo() {
    let repo = dirty_repo();
    let s = Session::open(&repo);

    // --- open the project and a file in it
    assert!(s.project().repo().is_some(), "the folder is a repository");
    let opened = s.ok(
        "workspace.open_path",
        json!({"path": "src/lib.rs", "line": 2}),
    );
    assert_eq!(opened.data["path"], "src/lib.rs");
    assert!(
        opened.ui.iter().any(|u| matches!(u,
            UiRequest::OpenPath { path, point } if same_path(path, &repo.root.join("src/lib.rs"))
                && point.is_some_and(|p| p.row == 2))),
        "asks the UI to open the file at the line: {:?}",
        opened.ui
    );
    assert!(opened.journal_id.is_none(), "opening is not a write");

    // --- status
    let snap = s.status();
    assert_eq!(snap.branch_label().as_deref(), Some("main"));
    assert_eq!(counts(&snap), (0, 1, 1), "one modified file, one untracked");

    // --- changes
    let changes = s.ok("git.list_changes", json!({}));
    assert_eq!(changes.data["changeset"], "worktree");
    assert_eq!(changes.data["branch"], "main");
    let paths: Vec<&str> = changes.data["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(paths.contains(&"src/lib.rs") && paths.contains(&"notes.txt"));

    // --- diff
    let diff = s.ok(
        "git.show_diff",
        json!({"changeset": "worktree", "path": "src/lib.rs"}),
    );
    let file = &diff.data["files"][0];
    assert_eq!(file["path"], "src/lib.rs");
    assert_eq!(
        (file["added"].as_u64(), file["removed"].as_u64()),
        (Some(2), Some(2))
    );
    let hunks = file["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 2, "two separate hunks: {file}");
    let first: Vec<&str> = hunks[0]["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .filter_map(|l| l.as_str())
        .collect();
    assert!(first.contains(&"-pub fn f3() -> u32 { 3 }"), "{first:?}");
    assert!(first.contains(&"+pub fn f3() -> u32 { 33 }"), "{first:?}");

    // --- stage the first hunk only
    let staged = s.ok("git.stage_hunk", json!({"path": "src/lib.rs", "hunk": 0}));
    let stage_id = staged.journal_id.expect("staging is journaled");
    assert!(staged.undo.is_some(), "and undoable");
    let cached = norm(&repo.git(&["diff", "--cached"]));
    assert!(cached.contains("+pub fn f3() -> u32 { 33 }"), "{cached}");
    assert!(
        !cached.contains("270"),
        "the second hunk is not staged: {cached}"
    );
    let snap = s.status();
    let lib = snap
        .status
        .iter()
        .find(|e| e.path == "src/lib.rs")
        .expect("lib.rs in status");
    assert!(
        lib.staged.is_some() && lib.unstaged.is_some(),
        "partially staged"
    );

    // --- commit what was staged
    let commit = s.ok("git.commit", json!({"message": "Change f3"}));
    let oid = commit.data["oid"].as_str().expect("oid").to_string();
    assert_eq!(oid.len(), 40);
    let commit_id = commit.journal_id.expect("commits are journaled");
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), oid);
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]).trim(), "Change f3");
    let committed = norm(&repo.git(&["show", "HEAD:src/lib.rs"]));
    assert!(committed.contains("f3() -> u32 { 33 }"));
    assert!(
        committed.contains("f27() -> u32 { 27 }"),
        "hunk 2 did not go in"
    );
    assert_eq!(
        repo.git(&["diff", "--name-only"]).trim(),
        "src/lib.rs",
        "the unstaged hunk is still in the working tree"
    );
    assert_eq!(counts(&s.status()), (0, 1, 1));

    // --- the activity log shows both, newest first
    let entries = s.journal.list(10);
    let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
    assert_eq!(commands, ["git.commit", "git.stage_hunk"]);
    assert_eq!(entries[0].id, commit_id);
    assert_eq!(entries[1].id, stage_id);
    for e in &entries {
        let entry_repo = e.repo.as_ref().expect("entries know their repository");
        assert!(same_path(entry_repo, &repo.root), "{entry_repo:?}");
        assert!(!e.undone);
    }
    assert!(
        entries[0].summary.contains("Change f3"),
        "{}",
        entries[0].summary
    );
    assert!(entries[0].undo.is_none(), "a commit is not undoable here");
    assert!(entries[1].undo.is_some());

    // --- undo: the commit refuses with a typed error; a new stage undoes cleanly
    assert!(matches!(
        s.journal.undo(commit_id, &s.cx, UndoMode::Check),
        Err(UndoError::NotUndoable(_))
    ));
    let second = s.ok("git.stage_hunk", json!({"path": "src/lib.rs", "hunk": 0}));
    assert_eq!(counts(&s.status()).0, 1, "the remaining hunk is staged");
    let second_id = second.journal_id.expect("journaled");
    let undone = s
        .journal
        .undo_last(&s.cx, UndoMode::Check)
        .expect("the newest undoable entry is the second stage");
    assert_eq!(undone.undo_of, Some(second_id));
    assert_eq!(
        repo.git(&["diff", "--cached", "--name-only"]).trim(),
        "",
        "index restored"
    );
    assert!(
        norm(&repo.git(&["diff"])).contains("270"),
        "the change is back in the working tree only"
    );
    assert!(s.journal.get(second_id).expect("entry").undone);
    assert_eq!(
        s.journal.list(1)[0].command,
        "journal.undo",
        "the undo is in the log too"
    );
}

#[test]
fn a_read_only_walk_writes_nothing_and_leaves_no_journal_entries() {
    let repo = dirty_repo();
    let before = (
        repo.git(&["status", "--porcelain"]),
        repo.read("src/lib.rs"),
    );
    let s = Session::open(&repo);
    s.ok("workspace.open_path", json!({"path": "README.md"}));
    s.ok("git.list_changes", json!({}));
    s.ok("git.show_diff", json!({"changeset": "worktree"}));
    s.ok("git.history", json!({"limit": 5}));
    assert!(s.journal.list(10).is_empty(), "reads are not journaled");
    assert_eq!(
        before,
        (
            repo.git(&["status", "--porcelain"]),
            repo.read("src/lib.rs")
        ),
        "nothing happens as a side effect of reading"
    );
}
