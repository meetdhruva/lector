//! Blame, line history, file history (git CLI backed) and permalinks.
mod common;

use common::Fixture;

fn shas(commits: &[lector_git::CommitInfo]) -> Vec<String> {
    commits.iter().map(|c| c.oid.to_string()).collect()
}

#[test]
fn blame_committed_and_uncommitted() {
    let fx = Fixture::new();
    fx.write("f.txt", "one\ntwo\nthree\nfour\n");
    let c1 = fx.commit_all("first");
    fx.write("f.txt", "one\nTWO\nthree\nfour\n");
    fx.git_as(
        "Bob Builder",
        "bob@example.com",
        &["commit", "-qam", "second"],
    );
    let c2 = fx.rev("HEAD");
    fx.write("f.txt", "one\nTWO\nthree changed\nfour\nfive\n");
    let repo = fx.repo();

    let hunks = repo.blame("f.txt", None).unwrap();
    let simple: Vec<_> = hunks
        .iter()
        .map(|h| (h.lines.clone(), h.uncommitted, h.oid.to_string()))
        .collect();
    let zero = "0".repeat(40);
    assert_eq!(
        simple,
        vec![
            (0..1, false, c1.clone()),
            (1..2, false, c2.clone()),
            (2..3, true, zero.clone()),
            (3..4, false, c1.clone()),
            (4..5, true, zero),
        ]
    );
    assert_eq!(hunks[0].author, "Alice Author");
    assert_eq!(hunks[0].summary, "first");
    assert_eq!(hunks[0].orig_path, "f.txt");
    assert_eq!(hunks[1].author, "Bob Builder");
    assert!(hunks[1].time > common::BASE_TIME);
    assert_eq!(hunks[3].orig_start, 3);

    let committed = repo.blame("f.txt", Some("HEAD")).unwrap();
    assert!(committed.iter().all(|h| !h.uncommitted));
    assert_eq!(committed.len(), 3);
    assert_eq!(committed[2].lines, 2..4);

    assert!(matches!(
        repo.blame("missing.txt", None),
        Err(lector_git::Error::GitCommand { .. })
    ));
}

#[test]
fn blame_across_rename_reports_original_path() {
    let fx = Fixture::new();
    fx.write("old.txt", "alpha\nbeta\ngamma\ndelta\n");
    let c1 = fx.commit_all("create");
    fx.git(&["mv", "old.txt", "new.txt"]);
    fx.commit_all("rename");
    fx.write("new.txt", "alpha\nbeta\ngamma\ndelta\nepsilon\n");
    let c3 = fx.commit_all("append");
    let hunks = fx.repo().blame("new.txt", None).unwrap();
    assert_eq!(hunks.len(), 2);
    assert_eq!(hunks[0].oid.to_string(), c1);
    assert_eq!(hunks[0].orig_path, "old.txt");
    assert_eq!(hunks[0].lines, 0..4);
    assert_eq!(hunks[1].oid.to_string(), c3);
    assert_eq!(hunks[1].orig_path, "new.txt");
}

/// old.txt: c1 create, c2 modify, c3 rename to new.txt, c4 modify, c5 unrelated.
fn renamed_history() -> (Fixture, Vec<String>) {
    let fx = Fixture::new();
    let body = "line one\nline two\nline three\nline four\nline five\nline six\n";
    fx.write("old.txt", body);
    let c1 = fx.commit_all("create");
    fx.write("old.txt", &body.replace("line two", "line 2"));
    let c2 = fx.commit_all("modify two");
    fx.git(&["mv", "old.txt", "new.txt"]);
    let c3 = fx.commit_all("rename");
    fx.write(
        "new.txt",
        &body
            .replace("line two", "line 2")
            .replace("line five", "line 5"),
    );
    let c4 = fx.commit_all("modify five");
    fx.write("other.txt", "x\n");
    let c5 = fx.commit_all("unrelated");
    (fx, vec![c1, c2, c3, c4, c5])
}

#[test]
fn file_history_follows_renames() {
    let (fx, c) = renamed_history();
    let repo = fx.repo();
    let history = repo.file_history("new.txt", 0).unwrap();
    assert_eq!(
        shas(&history),
        vec![c[3].clone(), c[2].clone(), c[1].clone(), c[0].clone()]
    );
    assert_eq!(history[0].summary, "modify five");
    let limited = repo.file_history("new.txt", 2).unwrap();
    assert_eq!(limited.len(), 2);
    // Without --follow semantics, gix log stops at the rename.
    let plain = repo
        .log(&lector_git::LogQuery {
            path: Some("new.txt".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(shas(&plain), vec![c[3].clone(), c[2].clone()]);
}

#[test]
fn line_history_tracks_a_range() {
    let (fx, c) = renamed_history();
    let repo = fx.repo();
    // Line index 1 ("line 2"): changed in c2, created in c1, follows the rename.
    let history = repo.line_history("new.txt", 1..2, 0).unwrap();
    assert_eq!(shas(&history), vec![c[1].clone(), c[0].clone()]);
    // Line index 4 ("line 5"): c4 and c1.
    let history = repo.line_history("new.txt", 4..5, 10).unwrap();
    assert_eq!(shas(&history), vec![c[3].clone(), c[0].clone()]);
    // Range 1..5 covers both.
    let history = repo.line_history("new.txt", 1..5, 0).unwrap();
    assert_eq!(
        shas(&history),
        vec![c[3].clone(), c[1].clone(), c[0].clone()]
    );
    assert!(repo.line_history("new.txt", 100..101, 0).is_err());
}

#[test]
fn permalink_uses_upstream_remote_or_origin() {
    let fx = Fixture::new();
    fx.write("src/lib.rs", "a\nb\nc\n");
    let c1 = fx.commit_all("one");
    fx.write("src/lib.rs", "a\nb\nc\nd\n");
    let c2 = fx.commit_all("two");
    fx.git(&["tag", "v1", &c1]);
    let repo = fx.repo();
    assert_eq!(
        repo.permalink("src/lib.rs", None, None).unwrap(),
        None,
        "no remotes"
    );

    fx.git(&["remote", "add", "origin", "git@github.com:owner/repo.git"]);
    assert_eq!(
        repo.permalink("src/lib.rs", Some(1..3), None)
            .unwrap()
            .unwrap(),
        format!("https://github.com/owner/repo/blob/{c2}/src/lib.rs#L2-L3")
    );
    assert_eq!(
        repo.permalink("src/lib.rs", Some(0..1), Some("v1"))
            .unwrap()
            .unwrap(),
        format!("https://github.com/owner/repo/blob/{c1}/src/lib.rs#L1")
    );

    // The upstream's remote wins over origin.
    fx.git(&["remote", "add", "fork", "https://gitlab.com/me/fork.git"]);
    fx.git(&["config", "branch.main.remote", "fork"]);
    fx.git(&["config", "branch.main.merge", "refs/heads/main"]);
    assert_eq!(
        repo.permalink("src/lib.rs", Some(1..3), None)
            .unwrap()
            .unwrap(),
        format!("https://gitlab.com/me/fork/-/blob/{c2}/src/lib.rs#L2-3")
    );
}
