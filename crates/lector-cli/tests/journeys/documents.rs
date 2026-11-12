//! Journey: edit a file through the document store, save it, see it in the status, discard the
//! change, undo the discard.

use lector_app::journal::UndoMode;
use serde_json::json;

use crate::fixture::{counts, norm, Repo, Session};

fn repo() -> Repo {
    let repo = Repo::init("documents");
    repo.write("a.txt", "one\ntwo\nthree\n");
    repo.write("windows.txt", "alpha\r\nbeta\r\n");
    repo.commit_all("init");
    repo
}

#[test]
fn edit_save_status_discard_undo() {
    let repo = repo();
    let s = Session::open(&repo);
    assert_eq!(counts(&s.status()), (0, 0, 0), "starts clean");

    // --- open: nothing is dirty
    let opened = s.ok("document.open", json!({"path": "a.txt"}));
    assert_eq!(opened.data["path"], "a.txt");
    assert_eq!(opened.data["dirty"], false);
    assert_eq!(
        opened.data["lines"], 4,
        "three lines and the empty last row"
    );

    // --- edit in the store: the buffer is dirty, the disk and the repository are untouched
    let edit = s.ok(
        "document.apply_edits",
        json!({"path": "a.txt", "edits": [{"lines": [1, 2], "new_text": "TWO\n"}]}),
    );
    assert_eq!(edit.data["dirty"], true);
    assert_eq!(edit.data["changed"], true);
    let read = s.ok("document.read", json!({"path": "a.txt"}));
    assert_eq!(
        norm(read.data["text"].as_str().expect("text")),
        "one\nTWO\nthree\n"
    );
    assert_eq!(read.data["dirty"], true);
    assert_eq!(
        repo.read("a.txt"),
        "one\ntwo\nthree\n",
        "nothing is written until a save"
    );
    assert_eq!(counts(&s.status()), (0, 0, 0));
    let listed = s.ok("document.list", json!({}));
    assert!(
        listed.data.to_string().contains("a.txt"),
        "the open document is listed: {}",
        listed.data
    );

    // --- save: the file changes, the write is journaled, the status shows it
    let saved = s.ok("document.save", json!({"path": "a.txt"}));
    assert_eq!(saved.data["saved"], true);
    assert!(saved.journal_id.is_some(), "saving is journaled");
    assert_eq!(repo.read("a.txt"), "one\nTWO\nthree\n");
    assert_eq!(
        s.ok("document.read", json!({"path": "a.txt"}))
            .data
            .get("dirty"),
        None
    );
    let snap = s.status();
    assert_eq!(counts(&snap), (0, 1, 0));
    let changes = s.ok("git.list_changes", json!({}));
    let file = &changes.data["files"][0];
    assert_eq!(file["path"], "a.txt");
    assert_eq!(file["kind"], "modified");
    assert_eq!(
        (file["added"].as_u64(), file["removed"].as_u64()),
        (Some(1), Some(1))
    );
    // Saving again with nothing to write is a no-op, not an error.
    let again = s.ok("document.save", json!({"path": "a.txt"}));
    assert_eq!(again.data["saved"], false);

    // --- discard: the saved change is thrown away, with a backup
    let discard = s.ok("git.discard", json!({"paths": ["a.txt"]}));
    let discard_id = discard.journal_id.expect("discard is journaled");
    assert!(
        discard.undo.is_some(),
        "and undoable: undo instead of confirm"
    );
    assert_eq!(repo.read("a.txt"), "one\ntwo\nthree\n");
    assert_eq!(counts(&s.status()), (0, 0, 0));

    // --- undo the discard: the saved text comes back byte for byte
    let undone = s
        .journal
        .undo(discard_id, &s.cx, UndoMode::Check)
        .expect("undo the discard");
    assert_eq!(undone.undo_of, Some(discard_id));
    assert_eq!(repo.read("a.txt"), "one\nTWO\nthree\n");
    assert_eq!(counts(&s.status()), (0, 1, 0), "modified again");
    // The log, newest first: the undo is an entry of its own, then the discard, then the
    // saves and the edit that led here.
    let log: Vec<String> = s.journal.list(10).into_iter().map(|e| e.command).collect();
    assert_eq!(&log[..2], ["journal.undo", "git.discard"], "{log:?}");
    assert!(log.contains(&"document.save".to_string()), "{log:?}");
    assert!(log.contains(&"document.apply_edits".to_string()), "{log:?}");
}

#[test]
fn crlf_files_keep_their_line_endings_through_edit_and_save() {
    let repo = repo();
    let s = Session::open(&repo);
    s.ok(
        "document.apply_edits",
        json!({"path": "windows.txt", "edits": [{"lines": [1, 2], "new_text": "BETA\n"}]}),
    );
    s.ok("document.save", json!({"path": "windows.txt"}));
    let bytes = std::fs::read(repo.root.join("windows.txt")).expect("read");
    let text = String::from_utf8(bytes).expect("utf-8");
    assert_eq!(norm(&text), "alpha\nBETA\n");
    assert!(
        text.lines().count() == 2 && !text.contains("\n\n"),
        "no doubled or lost line breaks: {text:?}"
    );
    // The change is one line, not the whole file re-written with other endings.
    let changes = s.ok("git.list_changes", json!({}));
    assert_eq!(changes.data["files"][0]["added"], 1, "{}", changes.data);
    assert_eq!(changes.data["files"][0]["removed"], 1, "{}", changes.data);
}

#[test]
fn a_stale_edit_is_refused_instead_of_clobbering() {
    let repo = repo();
    let s = Session::open(&repo);
    let opened = s.ok("document.open", json!({"path": "a.txt"}));
    let version = opened.data["version"].as_u64().expect("version");
    s.ok(
        "document.apply_edits",
        json!({"path": "a.txt", "edits": [{"lines": [0, 1], "new_text": "ONE\n"}]}),
    );
    // An agent that read the document at `version` and edits it now loses the race.
    let err = s
        .run(
            "document.apply_edits",
            json!({"path": "a.txt", "expected_version": version,
                   "edits": [{"lines": [2, 3], "new_text": "THREE\n"}]}),
        )
        .expect_err("stale version");
    assert!(err.to_string().contains("changed"), "{err}");
    let read = s.ok("document.read", json!({"path": "a.txt"}));
    assert_eq!(
        norm(read.data["text"].as_str().expect("text")),
        "ONE\ntwo\nthree\n"
    );
}
