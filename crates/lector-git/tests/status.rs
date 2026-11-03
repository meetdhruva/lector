mod common;

use std::collections::BTreeMap;

use common::Fixture;
use lector_git::{ChangeKind, StatusEntry};

fn letter(kind: Option<ChangeKind>) -> char {
    match kind {
        None => ' ',
        Some(ChangeKind::Added) => 'A',
        Some(ChangeKind::Modified) => 'M',
        Some(ChangeKind::Deleted) => 'D',
        Some(ChangeKind::Renamed) => 'R',
        Some(ChangeKind::Copied) => 'C',
        Some(ChangeKind::TypeChange) => 'T',
    }
}

/// Our status in `git status --porcelain=v1` notation: path → (XY, rename source).
fn ours_as_porcelain(entries: &[StatusEntry]) -> BTreeMap<String, (String, Option<String>)> {
    let mut map = BTreeMap::new();
    for e in entries {
        if e.conflicted {
            map.insert(e.path.clone(), ("UU".to_owned(), None));
            continue;
        }
        if e.staged.is_some() || e.unstaged.is_some() {
            let xy = format!("{}{}", letter(e.staged), letter(e.unstaged));
            map.insert(e.path.clone(), (xy, e.renamed_from.clone()));
        }
        if e.untracked {
            // git lists a staged deletion + untracked file of the same path twice; keep the tracked one.
            map.entry(e.path.clone()).or_insert(("??".to_owned(), None));
        }
    }
    map
}

/// `git status --porcelain=v1 -z -uall`, conflicts normalized to `UU`.
fn git_porcelain(fx: &Fixture) -> BTreeMap<String, (String, Option<String>)> {
    let out = fx.git(&["status", "--porcelain=v1", "-z", "-uall"]);
    let mut map = BTreeMap::new();
    let mut fields = out.split('\0').filter(|s| !s.is_empty());
    while let Some(field) = fields.next() {
        let (xy, path) = field.split_at(2);
        let path = path.trim_start().to_owned();
        let mut xy = xy.to_owned();
        let mut source = None;
        if xy.starts_with('R') || xy.starts_with('C') {
            source = fields.next().map(str::to_owned);
        }
        if matches!(xy.as_str(), "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU") {
            xy = "UU".to_owned();
        }
        map.entry(path).or_insert((xy, source));
    }
    map
}

fn setup_all_kinds() -> Fixture {
    let fx = Fixture::new();
    fx.write("modified.txt", "one\n");
    fx.write("staged.txt", "one\n");
    fx.write("both.txt", "one\n");
    fx.write("deleted.txt", "gone soon\n");
    fx.write("staged_deleted.txt", "gone soon\n");
    fx.write(
        "rename_me.txt",
        "a fairly long line of content that survives the rename\nline 2\nline 3\n",
    );
    fx.write("dir/nested.txt", "nested\n");
    fx.write(".gitignore", "*.log\n");
    fx.commit_all("initial");

    fx.write("modified.txt", "one\ntwo\n");
    fx.write("staged.txt", "one\nstaged\n");
    fx.git(&["add", "staged.txt"]);
    fx.write("both.txt", "one\nstaged\n");
    fx.git(&["add", "both.txt"]);
    fx.write("both.txt", "one\nstaged\nand unstaged\n");
    fx.remove("deleted.txt");
    fx.git(&["rm", "-q", "staged_deleted.txt"]);
    fx.git(&["mv", "rename_me.txt", "renamed.txt"]);
    fx.write("added.txt", "new\n");
    fx.git(&["add", "added.txt"]);
    fx.write("untracked.txt", "u\n");
    fx.write("newdir/deep/untracked2.txt", "u\n");
    fx.write("ignored.log", "ignored\n");
    fx
}

#[test]
fn status_all_kinds() {
    let fx = setup_all_kinds();
    let repo = fx.repo();
    let status = repo.status().unwrap();
    let by_path: BTreeMap<_, _> = status.iter().map(|e| (e.path.as_str(), e)).collect();

    assert_eq!(by_path["modified.txt"].unstaged, Some(ChangeKind::Modified));
    assert_eq!(by_path["modified.txt"].staged, None);
    assert_eq!(by_path["staged.txt"].staged, Some(ChangeKind::Modified));
    assert_eq!(by_path["staged.txt"].unstaged, None);
    assert_eq!(by_path["both.txt"].staged, Some(ChangeKind::Modified));
    assert_eq!(by_path["both.txt"].unstaged, Some(ChangeKind::Modified));
    assert_eq!(by_path["deleted.txt"].unstaged, Some(ChangeKind::Deleted));
    assert_eq!(
        by_path["staged_deleted.txt"].staged,
        Some(ChangeKind::Deleted)
    );
    assert_eq!(by_path["renamed.txt"].staged, Some(ChangeKind::Renamed));
    assert_eq!(
        by_path["renamed.txt"].renamed_from.as_deref(),
        Some("rename_me.txt")
    );
    assert!(!by_path.contains_key("rename_me.txt"));
    assert_eq!(by_path["added.txt"].staged, Some(ChangeKind::Added));
    assert!(by_path["untracked.txt"].untracked);
    assert!(by_path["newdir/deep/untracked2.txt"].untracked);
    assert!(!by_path.contains_key("ignored.log"));
    assert!(!by_path.contains_key("dir/nested.txt"));
    assert!(status.windows(2).all(|w| w[0].path < w[1].path), "sorted");

    // Differential: same picture as git.
    assert_eq!(ours_as_porcelain(&status), git_porcelain(&fx));
}

#[test]
fn status_conflict_from_failed_merge() {
    let fx = Fixture::new();
    fx.write("c.txt", "base\n");
    fx.write("other.txt", "x\n");
    fx.commit_all("base");
    fx.git(&["checkout", "-q", "-b", "topic"]);
    fx.write("c.txt", "topic\n");
    fx.write("topic_only.txt", "t\n");
    fx.commit_all("topic change");
    fx.git(&["checkout", "-q", "main"]);
    fx.write("c.txt", "main\n");
    fx.commit_all("main change");
    assert!(
        !fx.git_allow_fail(&["merge", "-q", "topic"]),
        "merge must conflict"
    );

    let status = fx.repo().status().unwrap();
    let c = status.iter().find(|e| e.path == "c.txt").unwrap();
    assert!(c.conflicted);
    let t = status.iter().find(|e| e.path == "topic_only.txt").unwrap();
    assert_eq!(t.staged, Some(ChangeKind::Added));
    assert_eq!(ours_as_porcelain(&status), git_porcelain(&fx));

    // Index side of a conflict loads "ours".
    let ours = fx
        .repo()
        .load(&lector_git::Side::Index("c.txt".into()))
        .unwrap()
        .unwrap();
    assert_eq!(ours, b"main\n");
}

#[test]
fn status_type_change_and_executable_bit() {
    let fx = Fixture::new();
    fx.write("link_or_file", "content\n");
    fx.write("script.sh", "echo\n");
    fx.commit_all("init");
    fx.remove("link_or_file");
    std::os::unix::fs::symlink("script.sh", fx.file("link_or_file")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(fx.file("script.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let status = fx.repo().status().unwrap();
    let link = status.iter().find(|e| e.path == "link_or_file").unwrap();
    assert_eq!(link.unstaged, Some(ChangeKind::TypeChange));
    let script = status.iter().find(|e| e.path == "script.sh").unwrap();
    assert_eq!(script.unstaged, Some(ChangeKind::Modified));
    assert_eq!(ours_as_porcelain(&status), git_porcelain(&fx));
    // Symlinks load as their target, like git stores them.
    let bytes = fx
        .repo()
        .load(&lector_git::Side::Workdir("link_or_file".into()))
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"script.sh");
}

#[test]
fn status_on_unborn_repo() {
    let fx = Fixture::new();
    fx.write("a.txt", "a\n");
    fx.write("b.txt", "b\n");
    fx.git(&["add", "a.txt"]);
    let status = fx.repo().status().unwrap();
    assert_eq!(status.len(), 2);
    assert_eq!(status[0].staged, Some(ChangeKind::Added));
    assert!(status[1].untracked);
    assert_eq!(ours_as_porcelain(&status), git_porcelain(&fx));
}

#[test]
fn status_is_independent_of_current_dir_and_subdir_discovery() {
    let fx = setup_all_kinds();
    let from_subdir = lector_git::Repo::discover(&fx.file("dir")).unwrap();
    assert_eq!(from_subdir.workdir(), fx.repo().workdir());
    assert_eq!(from_subdir.status().unwrap(), fx.repo().status().unwrap());
}

fn index_fingerprint(fx: &Fixture) -> (std::time::SystemTime, Vec<u8>) {
    let path = fx.path().join(".git/index");
    let meta = std::fs::metadata(&path).unwrap();
    (meta.modified().unwrap(), std::fs::read(&path).unwrap())
}

#[test]
fn status_never_writes_the_index() {
    let fx = setup_all_kinds();
    // Make every entry stat-dirty (content unchanged) so a refreshing status would want to write.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    for file in ["staged.txt", "dir/nested.txt", ".gitignore"] {
        let content = std::fs::read(fx.file(file)).unwrap();
        std::fs::write(fx.file(file), content).unwrap();
    }
    let before = index_fingerprint(&fx);
    let repo = fx.repo();
    for _ in 0..3 {
        repo.status().unwrap();
        repo.changes(&lector_git::Changeset::Unstaged).unwrap();
        repo.changes(&lector_git::Changeset::WorkingTree).unwrap();
        repo.blame("staged.txt", None).unwrap();
    }
    let after = index_fingerprint(&fx);
    assert_eq!(before, after, ".git/index must not be modified");
    assert!(!fx.path().join(".git/index.lock").exists());

    // Sanity check of the setup: a plain `git status` *does* refresh (rewrite) the index here.
    let out = fx
        .command(&["status", "--porcelain"])
        .env("GIT_OPTIONAL_LOCKS", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_ne!(
        index_fingerprint(&fx),
        before,
        "setup should leave the index stat-dirty"
    );
}

#[test]
fn bare_repo_reads_work_but_workdir_calls_fail() {
    let fx = Fixture::new();
    fx.write("a.txt", "a\n");
    fx.commit_all("init");
    let bare = tempfile::tempdir().unwrap();
    let bare_path = bare.path().join("bare.git");
    fx.git(&["clone", "-q", "--bare", ".", bare_path.to_str().unwrap()]);
    let repo = lector_git::Repo::discover(&bare_path).unwrap();
    assert!(repo.is_bare());
    assert!(matches!(repo.status(), Err(lector_git::Error::Bare)));
    assert!(matches!(
        repo.changes(&lector_git::Changeset::WorkingTree),
        Err(lector_git::Error::Bare)
    ));
    assert!(matches!(
        repo.blame("a.txt", None),
        Err(lector_git::Error::Bare)
    ));
    assert_eq!(repo.file_at("HEAD", "a.txt").unwrap().unwrap(), b"a\n");
    assert_eq!(repo.log(&Default::default()).unwrap().len(), 1);
}

#[test]
fn not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    // A tempdir might live inside some repo on dev machines; use an isolated ceiling.
    let err = lector_git::Repo::discover(&dir.path().join("missing")).unwrap_err();
    assert!(matches!(err, lector_git::Error::NotARepository(_)));
}
