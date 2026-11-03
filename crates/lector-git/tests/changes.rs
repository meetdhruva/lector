mod common;

use common::Fixture;
use lector_git::{ChangeKind, Changeset, FileChange, Oid, Repo, Side};

fn summary(changes: &[FileChange]) -> Vec<(String, ChangeKind)> {
    changes
        .iter()
        .map(|c| (c.path().to_owned(), c.kind))
        .collect()
}

fn load(repo: &Repo, side: &Side) -> Option<String> {
    repo.load(side)
        .unwrap()
        .map(|b| String::from_utf8(b).unwrap())
}

/// History: c1 (a, b, long) → c2 (modify a, rename long→moved, delete b, add c)
fn fixture() -> (Fixture, String, String) {
    let fx = Fixture::new();
    fx.write("a.txt", "a1\n");
    fx.write("b.txt", "b1\n");
    fx.write(
        "long.txt",
        "this is a long enough file\nwith several lines\nso that rename detection\nworks well\n",
    );
    let c1 = fx.commit_all("c1");
    fx.write("a.txt", "a2\n");
    fx.git(&["mv", "long.txt", "moved.txt"]);
    fx.remove("b.txt");
    fx.write("c.txt", "c\n");
    let c2 = fx.commit_all("c2");
    (fx, c1, c2)
}

#[test]
fn commit_changeset_with_rename() {
    let (fx, c1, c2) = fixture();
    let repo = fx.repo();
    let changes = repo
        .changes(&Changeset::Commit(Oid::from_hex(&c2).unwrap()))
        .unwrap();
    assert_eq!(
        summary(&changes),
        vec![
            ("a.txt".into(), ChangeKind::Modified),
            ("b.txt".into(), ChangeKind::Deleted),
            ("c.txt".into(), ChangeKind::Added),
            ("moved.txt".into(), ChangeKind::Renamed),
        ]
    );
    let rename = &changes[3];
    assert_eq!(rename.old_path.as_deref(), Some("long.txt"));
    assert!(load(&repo, &rename.old)
        .unwrap()
        .starts_with("this is a long"));
    let a = &changes[0];
    assert_eq!(load(&repo, &a.old).as_deref(), Some("a1\n"));
    assert_eq!(load(&repo, &a.new).as_deref(), Some("a2\n"));
    assert_eq!(changes[1].new, Side::None);
    assert!(changes.iter().all(|c| c.binary.is_none()));

    // Root commit compares against the empty tree.
    let root = repo
        .changes(&Changeset::Commit(Oid::from_hex(&c1).unwrap()))
        .unwrap();
    assert_eq!(root.len(), 3);
    assert!(root
        .iter()
        .all(|c| c.kind == ChangeKind::Added && c.old == Side::None));

    // Differential against git's name-status.
    let git = fx.git(&["show", "--format=", "--name-status", "-M", &c2]);
    let mut git_lines: Vec<String> = git
        .lines()
        .map(|l| {
            let mut parts = l.split('\t');
            let status = parts.next().unwrap().chars().next().unwrap();
            let path = l.rsplit('\t').next().unwrap().to_owned();
            format!("{status} {path}")
        })
        .collect();
    git_lines.sort();
    let mut ours: Vec<String> = changes
        .iter()
        .map(|c| {
            let s = match c.kind {
                ChangeKind::Added => 'A',
                ChangeKind::Modified => 'M',
                ChangeKind::Deleted => 'D',
                ChangeKind::Renamed => 'R',
                ChangeKind::Copied => 'C',
                ChangeKind::TypeChange => 'T',
            };
            format!("{s} {}", c.path())
        })
        .collect();
    ours.sort();
    assert_eq!(ours, git_lines);
}

#[test]
fn range_and_branch_vs_base() {
    let (fx, c1, _c2) = fixture();
    let repo = fx.repo();
    let range = repo
        .changes(&Changeset::Range {
            from: c1.clone(),
            to: "HEAD".into(),
        })
        .unwrap();
    assert_eq!(range.len(), 4);

    fx.git(&["checkout", "-q", "-b", "feature", &c1]);
    fx.write("feature.txt", "f\n");
    fx.commit_all("feature work");
    fx.git(&["checkout", "-q", "main"]);
    // main moved on after the fork; the PR view must only show the feature's change.
    let pr = repo
        .changes(&Changeset::BranchVsBase {
            branch: "feature".into(),
            base: "main".into(),
        })
        .unwrap();
    assert_eq!(
        summary(&pr),
        vec![("feature.txt".into(), ChangeKind::Added)]
    );
    // A plain range would include main's changes, reversed.
    let range = repo
        .changes(&Changeset::Range {
            from: "main".into(),
            to: "feature".into(),
        })
        .unwrap();
    assert!(range.len() > 1);

    let err = repo
        .changes(&Changeset::Range {
            from: "nope".into(),
            to: "HEAD".into(),
        })
        .unwrap_err();
    assert!(
        matches!(err, lector_git::Error::InvalidRevision { .. }),
        "{err}"
    );
}

#[test]
fn worktree_changesets() {
    let (fx, _c1, _c2) = fixture();
    fx.write("a.txt", "a3 staged\n");
    fx.git(&["add", "a.txt"]);
    fx.write("a.txt", "a4 worktree\n");
    fx.write("c.txt", "c changed\n");
    fx.remove("moved.txt");
    fx.write("new_staged.txt", "ns\n");
    fx.git(&["add", "new_staged.txt"]);
    fx.write("untracked.txt", "u\n");
    let repo = fx.repo();

    let unstaged = repo.changes(&Changeset::Unstaged).unwrap();
    assert_eq!(
        summary(&unstaged),
        vec![
            ("a.txt".into(), ChangeKind::Modified),
            ("c.txt".into(), ChangeKind::Modified),
            ("moved.txt".into(), ChangeKind::Deleted),
        ]
    );
    assert_eq!(
        load(&repo, &unstaged[0].old).as_deref(),
        Some("a3 staged\n")
    );
    assert_eq!(
        load(&repo, &unstaged[0].new).as_deref(),
        Some("a4 worktree\n")
    );
    assert_eq!(unstaged[0].old, Side::Index("a.txt".into()));
    assert_eq!(unstaged[0].new, Side::Workdir("a.txt".into()));

    let staged = repo.changes(&Changeset::Staged).unwrap();
    assert_eq!(
        summary(&staged),
        vec![
            ("a.txt".into(), ChangeKind::Modified),
            ("new_staged.txt".into(), ChangeKind::Added),
        ]
    );
    assert_eq!(load(&repo, &staged[0].old).as_deref(), Some("a2\n"));
    assert_eq!(load(&repo, &staged[0].new).as_deref(), Some("a3 staged\n"));

    let untracked = repo.changes(&Changeset::Untracked).unwrap();
    assert_eq!(
        summary(&untracked),
        vec![("untracked.txt".into(), ChangeKind::Added)]
    );
    assert_eq!(untracked[0].new, Side::Workdir("untracked.txt".into()));

    let wt = repo.changes(&Changeset::WorkingTree).unwrap();
    assert_eq!(
        summary(&wt),
        vec![
            ("a.txt".into(), ChangeKind::Modified),
            ("c.txt".into(), ChangeKind::Modified),
            ("moved.txt".into(), ChangeKind::Deleted),
            ("new_staged.txt".into(), ChangeKind::Added),
            ("untracked.txt".into(), ChangeKind::Added),
        ]
    );
    assert_eq!(load(&repo, &wt[0].old).as_deref(), Some("a2\n"));
    assert_eq!(load(&repo, &wt[0].new).as_deref(), Some("a4 worktree\n"));
    assert!(matches!(wt[0].old, Side::Blob(_)));
}

#[test]
fn working_tree_follows_staged_rename() {
    let (fx, _c1, _c2) = fixture();
    fx.git(&["mv", "moved.txt", "moved_again.txt"]);
    let repo = fx.repo();
    let wt = repo.changes(&Changeset::WorkingTree).unwrap();
    assert_eq!(
        summary(&wt),
        vec![("moved_again.txt".into(), ChangeKind::Renamed)]
    );
    assert_eq!(wt[0].old_path.as_deref(), Some("moved.txt"));
    let staged = repo.changes(&Changeset::Staged).unwrap();
    assert_eq!(staged[0].kind, ChangeKind::Renamed);
    assert_eq!(staged[0].old_path.as_deref(), Some("moved.txt"));
}

#[test]
fn binary_detection_is_lazy() {
    let fx = Fixture::new();
    fx.write("text.txt", "hello\n");
    std::fs::write(fx.file("bin.dat"), b"\x00\x01\x02binary").unwrap();
    let head = fx.commit_all("init");
    let repo = fx.repo();
    let mut changes = repo
        .changes(&Changeset::Commit(Oid::from_hex(&head).unwrap()))
        .unwrap();
    assert!(changes.iter().all(|c| c.binary.is_none()));
    assert!(repo.detect_binary(&mut changes[0]).unwrap());
    assert_eq!(changes[0].binary, Some(true));
    assert!(!repo.detect_binary(&mut changes[1]).unwrap());
    assert!(lector_git::is_binary(b"a\0b"));
    assert!(!lector_git::is_binary(b"plain"));
}

#[test]
fn load_sides() {
    let (fx, _c1, c2) = fixture();
    let repo = fx.repo();
    assert_eq!(repo.load(&Side::None).unwrap(), None);
    assert_eq!(
        repo.load(&Side::Workdir("missing.txt".into())).unwrap(),
        None
    );
    assert_eq!(repo.load(&Side::Index("missing.txt".into())).unwrap(), None);
    assert_eq!(
        load(&repo, &Side::Index("a.txt".into())).as_deref(),
        Some("a2\n")
    );
    // A commit id is not a blob.
    assert_eq!(
        repo.load(&Side::Blob(Oid::from_hex(&c2).unwrap())).unwrap(),
        None
    );
}
