mod common;

use common::Fixture;
use lector_git::{graph, CommitInfo, LogQuery, Oid, RefKind, Repo};

struct History {
    fx: Fixture,
    c1: String,
    c2: String,
    c3: String,
    c4: String,
    f2: String,
    merge: String,
    remote_only: String,
}

/// main: c1 ─ c2 ─ c3 ─ c4 ─ M
///                └ f1 ─ f2 ┘   (feature)
/// tags: v1 (light, c1), v2 (annotated, c3); origin/main = remote_only (child of c3)
fn history() -> History {
    let fx = Fixture::new();
    fx.write("a.txt", "a1\n");
    let c1 = fx.commit_all("Initial commit");
    fx.write("b.txt", "b1\n");
    fx.git(&["add", "-A"]);
    fx.git_as(
        "Bob Builder",
        "bob@example.com",
        &["commit", "-q", "-m", "Add b\n\nLonger body text."],
    );
    let c2 = fx.rev("HEAD");
    fx.git(&["checkout", "-q", "-b", "feature"]);
    fx.write("docs/notes.md", "notes\n");
    fx.commit_all("f1: docs");
    fx.write("src/x.rs", "fn x() {}\n");
    let f2 = fx.commit_all("f2: add x");
    fx.git(&["checkout", "-q", "main"]);
    fx.write("a.txt", "a2\n");
    let c3 = fx.commit_all("Fix bug in Parser");
    fx.git(&["tag", "v1", &c1]);
    fx.git(&["tag", "-a", "v2", "-m", "release 2", &c3]);
    // A commit that only exists on the "remote".
    fx.git(&["checkout", "-q", "-b", "tmp"]);
    fx.write("remote.txt", "r\n");
    let remote_only = fx.commit_all("remote work");
    fx.git(&["checkout", "-q", "main"]);
    fx.git(&["branch", "-q", "-D", "tmp"]);
    fx.git(&["update-ref", "refs/remotes/origin/main", &remote_only]);
    fx.git(&[
        "config",
        "remote.origin.url",
        "git@github.com:owner/repo.git",
    ]);
    fx.git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/*:refs/remotes/origin/*",
    ]);
    fx.git(&["config", "branch.main.remote", "origin"]);
    fx.git(&["config", "branch.main.merge", "refs/heads/main"]);
    fx.write("b.txt", "b2\n");
    let c4 = fx.commit_all("Change b");
    fx.git(&["merge", "-q", "--no-ff", "-m", "Merge feature", "feature"]);
    let merge = fx.rev("HEAD");
    History {
        fx,
        c1,
        c2,
        c3,
        c4,
        f2,
        merge,
        remote_only,
    }
}

fn shas(commits: &[CommitInfo]) -> Vec<String> {
    commits.iter().map(|c| c.oid.to_string()).collect()
}

fn git_shas(fx: &Fixture, args: &[&str]) -> Vec<String> {
    let mut full = vec!["log", "--format=%H"];
    full.extend_from_slice(args);
    fx.git(&full).lines().map(str::to_owned).collect()
}

fn q() -> LogQuery {
    LogQuery::default()
}

#[test]
fn log_matches_git_cli() {
    let h = history();
    let repo = h.fx.repo();
    let range = format!("{}..main", h.c2);
    let cases: Vec<(LogQuery, Vec<&str>)> = vec![
        (q(), vec![]),
        (
            LogQuery {
                all_refs: true,
                ..q()
            },
            vec!["--all"],
        ),
        (
            LogQuery {
                path: Some("a.txt".into()),
                ..q()
            },
            vec!["--", "a.txt"],
        ),
        (
            LogQuery {
                path: Some("src".into()),
                ..q()
            },
            vec!["--", "src"],
        ),
        (
            LogQuery {
                path: Some("src/".into()),
                ..q()
            },
            vec!["--", "src/"],
        ),
        (
            LogQuery {
                author: Some("Bob".into()),
                ..q()
            },
            vec!["--author=Bob"],
        ),
        (
            LogQuery {
                message: Some("parser".into()),
                ..q()
            },
            vec!["-i", "--grep=parser"],
        ),
        (
            LogQuery {
                message: Some("body text".into()),
                ..q()
            },
            vec!["--grep=body text"],
        ),
        (
            LogQuery {
                skip: 1,
                limit: 2,
                ..q()
            },
            vec!["--skip=1", "-n", "2"],
        ),
        (
            LogQuery {
                rev: Some(range.clone()),
                ..q()
            },
            vec![&range],
        ),
        (
            LogQuery {
                rev: Some("feature".into()),
                ..q()
            },
            vec!["feature"],
        ),
        (
            LogQuery {
                rev: Some("v2".into()),
                ..q()
            },
            vec!["v2"],
        ),
    ];
    for (query, args) in cases {
        assert_eq!(
            shas(&repo.log(&query).unwrap()),
            git_shas(&h.fx, &args),
            "query {query:?}"
        );
    }
}

#[test]
fn log_commit_fields_and_refs() {
    let h = history();
    let repo = h.fx.repo();
    let log = repo.log(&q()).unwrap();
    let merge = &log[0];
    assert_eq!(merge.oid.to_string(), h.merge);
    assert_eq!(merge.parents.len(), 2);
    assert_eq!(merge.parents[0].to_string(), h.c4);
    assert_eq!(merge.parents[1].to_string(), h.f2);
    assert_eq!(merge.summary, "Merge feature");
    assert_eq!(merge.author.name, "Alice Author");
    assert_eq!(merge.author.email, "alice@example.com");
    assert_eq!(merge.committer.name, "Carl Committer");
    assert_eq!(merge.author.offset_minutes, 60);
    assert!(merge.author.time > common::BASE_TIME);
    let labels: Vec<_> = merge
        .refs
        .iter()
        .map(|r| (r.kind, r.name.as_str()))
        .collect();
    assert_eq!(
        labels,
        vec![(RefKind::Head, "HEAD"), (RefKind::LocalBranch, "main")]
    );

    let c3 = log.iter().find(|c| c.oid.to_string() == h.c3).unwrap();
    assert_eq!(c3.refs.len(), 1);
    assert_eq!(
        (c3.refs[0].kind, c3.refs[0].name.as_str()),
        (RefKind::Tag, "v2")
    );

    let all = repo
        .log(&LogQuery {
            all_refs: true,
            ..q()
        })
        .unwrap();
    let remote = all
        .iter()
        .find(|c| c.oid.to_string() == h.remote_only)
        .unwrap();
    assert_eq!(remote.refs[0].kind, RefKind::RemoteBranch);
    assert_eq!(remote.refs[0].name, "origin/main");

    let bob = log.iter().find(|c| c.oid.to_string() == h.c2).unwrap();
    assert_eq!(bob.summary, "Add b");
    let detail = repo.commit(bob.oid).unwrap();
    assert_eq!(detail.message, "Add b\n\nLonger body text.\n");
    assert_eq!(detail.changes.len(), 1);
    assert_eq!(detail.changes[0].path(), "b.txt");
    assert_eq!(detail.info, *bob);
}

#[test]
fn log_with_unborn_head_is_empty() {
    let fx = Fixture::new();
    let repo = fx.repo();
    assert!(repo.log(&q()).unwrap().is_empty());
    let head = repo.head().unwrap();
    assert_eq!(head.branch.as_deref(), Some("main"));
    assert_eq!(head.oid, None);
    assert_eq!(repo.head_text("x").unwrap(), None);
    assert!(repo.branches().unwrap().is_empty());
}

#[test]
fn head_branches_tags() {
    let h = history();
    let repo = h.fx.repo();
    let head = repo.head().unwrap();
    assert_eq!(head.branch.as_deref(), Some("main"));
    assert!(!head.detached);
    assert_eq!(head.oid.unwrap().to_string(), h.merge);
    assert_eq!(head.upstream.as_deref(), Some("origin/main"));
    // main has c4, f1, f2, merge that origin/main lacks; origin/main has remote_only.
    assert_eq!((head.ahead, head.behind), (4, 1));
    let git_counts =
        h.fx.git(&["rev-list", "--left-right", "--count", "main...origin/main"]);
    assert_eq!(
        git_counts.split_whitespace().collect::<Vec<_>>(),
        vec!["4", "1"]
    );

    let branches = repo.branches().unwrap();
    let names: Vec<_> = branches
        .iter()
        .map(|b| (b.name.as_str(), b.remote, b.head))
        .collect();
    assert_eq!(
        names,
        vec![
            ("feature", false, false),
            ("main", false, true),
            ("origin/main", true, false)
        ]
    );
    let main = &branches[1];
    assert_eq!(main.upstream.as_deref(), Some("origin/main"));
    assert_eq!(main.oid.to_string(), h.merge);
    assert!(main.time > common::BASE_TIME);
    assert_eq!(branches[0].upstream, None);

    let tags = repo.tags().unwrap();
    assert_eq!(tags.len(), 2);
    assert_eq!(
        (tags[0].name.as_str(), tags[0].oid.to_string()),
        ("v1", h.c1.clone())
    );
    assert_eq!(
        (tags[1].name.as_str(), tags[1].oid.to_string()),
        ("v2", h.c3.clone())
    );

    // Detached HEAD.
    h.fx.git(&["checkout", "-q", "--detach", &h.c3]);
    let head = repo.head().unwrap();
    assert!(head.detached);
    assert_eq!(head.branch, None);
    assert_eq!(head.upstream, None);
    assert_eq!(head.oid.unwrap().to_string(), h.c3);
    let log = repo.log(&q()).unwrap();
    assert_eq!(log[0].refs[0].kind, RefKind::Head);
}

#[test]
fn stashes() {
    let h = history();
    let repo = h.fx.repo();
    assert!(repo.stashes().unwrap().is_empty());
    h.fx.write("a.txt", "stash one\n");
    h.fx.git(&["stash", "push", "-q", "-m", "first"]);
    h.fx.write("a.txt", "stash two\n");
    h.fx.git(&["stash", "push", "-q", "-m", "second"]);
    let stashes = repo.stashes().unwrap();
    assert_eq!(stashes.len(), 2);
    assert_eq!(stashes[0].index, 0);
    assert_eq!(stashes[0].message, "On main: second");
    assert_eq!(stashes[0].oid.to_string(), h.fx.rev("stash@{0}"));
    assert_eq!(stashes[1].message, "On main: first");
    assert_eq!(stashes[1].oid.to_string(), h.fx.rev("stash@{1}"));
    // The stash is not part of an all-refs log.
    let all = repo
        .log(&LogQuery {
            all_refs: true,
            ..q()
        })
        .unwrap();
    assert!(!all.iter().any(|c| c.oid == stashes[0].oid));
}

#[test]
fn objects_and_revisions() {
    let h = history();
    let repo = h.fx.repo();
    assert_eq!(repo.file_at("v1", "a.txt").unwrap().unwrap(), b"a1\n");
    assert_eq!(repo.file_at("HEAD", "a.txt").unwrap().unwrap(), b"a2\n");
    assert_eq!(
        repo.file_at("HEAD", "src/x.rs").unwrap().unwrap(),
        b"fn x() {}\n"
    );
    assert_eq!(repo.file_at("HEAD", "missing").unwrap(), None);
    assert_eq!(repo.file_at("HEAD", "src").unwrap(), None);
    assert!(matches!(
        repo.file_at("no-such-rev", "a.txt"),
        Err(lector_git::Error::InvalidRevision { .. })
    ));
    h.fx.write("a.txt", "dirty\n");
    assert_eq!(repo.head_text("a.txt").unwrap().unwrap(), b"a2\n");

    assert_eq!(repo.resolve("v2").unwrap().to_string(), h.c3);
    assert_eq!(repo.resolve("HEAD^2").unwrap().to_string(), h.f2);
    assert_eq!(repo.resolve(&h.c1[..10]).unwrap().to_string(), h.c1);
    assert!(repo.resolve("nope").is_err());

    let c4 = Oid::from_hex(&h.c4).unwrap();
    let f2 = Oid::from_hex(&h.f2).unwrap();
    assert_eq!(repo.merge_base(c4, f2).unwrap().unwrap().to_string(), h.c2);

    // Unrelated history has no merge base.
    h.fx.git(&["checkout", "-q", "--orphan", "lonely"]);
    h.fx.git(&["rm", "-rqf", "--cached", "."]);
    let lonely = h.fx.commit_all("lonely root");
    let lonely = Oid::from_hex(&lonely).unwrap();
    assert_eq!(repo.merge_base(c4, lonely).unwrap(), None);
    assert!(matches!(
        repo.changes(&lector_git::Changeset::BranchVsBase {
            branch: "lonely".into(),
            base: "main".into()
        }),
        Err(lector_git::Error::NoMergeBase(..))
    ));
    let remotes = repo.remotes().unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].name, "origin");
    assert_eq!(remotes[0].url, "git@github.com:owner/repo.git");
}

#[test]
fn graph_on_real_history() {
    let h = history();
    let repo: Repo = h.fx.repo();
    let log = repo
        .log(&LogQuery {
            all_refs: true,
            ..q()
        })
        .unwrap();
    let rows = graph(&log);
    assert_eq!(rows.len(), log.len());
    for (i, row) in rows.iter().enumerate() {
        assert!(row.lane < row.lanes, "row {i}: {row:?}");
        for edge in &row.edges {
            assert!(edge.from_lane < row.lanes);
            if let Some(next) = rows.get(i + 1) {
                assert!(
                    edge.to_lane < next.lanes.max(1),
                    "row {i} edge {edge:?} next {next:?}"
                );
            }
        }
        // Every non-root node has an edge leaving it, unless its parents are absent.
        if !log[i].parents.is_empty() && i + 1 < rows.len() {
            assert!(row.edges.iter().any(|e| e.from_lane == row.lane), "row {i}");
        }
    }
    // The merge row has a MergeIn edge.
    let merge_row = log
        .iter()
        .position(|c| c.oid.to_string() == h.merge)
        .unwrap();
    assert!(rows[merge_row]
        .edges
        .iter()
        .any(|e| e.kind == lector_git::EdgeKind::MergeIn));
}
