//! Release-audit regression tests: Lector must never write to the user's
//! repository, and must not panic on unusual files or repositories.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lector_config::{ReviewStore, Settings};
use lector_git::{Changeset, LogQuery};
use lector_search::CancelToken;
use lector_workspace::project::Project;
use lector_workspace::review;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        // No background auto-gc/maintenance: newer git detaches it after commits, and
        // its lock files appearing and vanishing would look like Lector writing.
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0")
        .env("GIT_CONFIG_KEY_1", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "gc.autoDetach")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(dir: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
}

fn commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

fn settings() -> Settings {
    let mut s = Settings::default();
    s.lsp.enabled = false;
    s
}

/// Every entry under `root` (files, dirs, symlinks) with content and mtime.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (String, Vec<u8>, Option<std::time::SystemTime>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let mtime = meta.modified().ok();
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.insert(
                    rel,
                    (
                        "link".into(),
                        target.to_string_lossy().as_bytes().to_vec(),
                        mtime,
                    ),
                );
            } else if meta.is_dir() {
                out.insert(rel, ("dir".into(), Vec::new(), mtime));
                stack.push(path);
            } else {
                out.insert(rel, ("file".into(), fs::read(&path).unwrap(), mtime));
            }
        }
    }
    out
}

/// Exercises every read path the UI uses on `project`.
fn exercise(project: &Project, store: &ReviewStore) {
    let files = project.rescan_files(&CancelToken::new());
    let _ = project.refresh_git_status();
    project.index_symbols(&CancelToken::new(), |_, _| {});
    for rel in files.paths.iter() {
        let Ok(doc) = project.load_document(Path::new(&**rel)) else {
            continue;
        };
        let n = doc.line_count();
        let _ = doc.line_highlights(0..n.saturating_add(5));
        let _ = doc.outline();
        let _ = doc.fold_ranges();
        for row in 0..n.min(50) {
            let _ = doc.scopes_at_row(row);
        }
        if let Some(text) = doc.text() {
            for off in (0..=text.len().min(300)).chain([text.len() + 10]) {
                let _ = doc.word_at(off);
            }
        }
        let _ = doc.diff_gutter(project);
        let _ = project.blame(rel);
    }
    let Some(repo) = project.repo() else {
        return;
    };
    let _ = repo.head();
    let _ = repo.status();
    let _ = repo.branches();
    let _ = repo.tags();
    let _ = repo.stashes();
    let _ = repo.remotes();
    let log = repo
        .log(&LogQuery {
            all_refs: true,
            limit: 100,
            ..LogQuery::default()
        })
        .unwrap_or_default();
    let _ = lector_git::graph(&log);
    let mut changesets = vec![
        Changeset::WorkingTree,
        Changeset::Staged,
        Changeset::Unstaged,
        Changeset::Untracked,
    ];
    for c in &log {
        let _ = repo.commit(c.oid);
        changesets.push(Changeset::Commit(c.oid));
    }
    for cs in &changesets {
        let Ok(view) = review::load_changeset(project, cs, store) else {
            continue;
        };
        for file in &view.files {
            let opts = review::diff_options(&project.settings().diff);
            if let Ok(diff) = review::load_file_diff(project, &file.change, &opts) {
                for line in 0..diff.new.line_count().min(20) {
                    let _ = diff.inline_for_new(line);
                }
                let _ = diff.split_segments(3);
                let _ = diff.unified_segments(3);
            }
            let path = file.path().to_string();
            let _ = repo.file_history(&path, 50);
            let _ = repo.line_history(&path, 0..2, 50);
            let _ = repo.permalink(&path, Some(0..1), None);
        }
    }
    if let Some(first) = files.paths.first() {
        let _ = review::file_vs_revision_with(
            repo,
            project.languages(),
            first,
            "HEAD",
            &review::diff_options(&project.settings().diff),
        );
    }
}

#[test]
fn repository_is_byte_identical_after_reading() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    init_repo(&root);
    write(&root, "src/lib.rs", "pub fn a() -> u32 {\n    1\n}\n");
    write(&root, "README.md", "# Demo\n\nText.\n");
    write(&root, "gone.txt", "bye\n");
    write(&root, "touched.txt", "same content\n");
    commit_all(&root, "first");
    git(&root, &["tag", "v1"]);
    git(&root, &["branch", "feature"]);
    write(&root, "src/lib.rs", "pub fn a() -> u32 {\n    2\n}\n");
    commit_all(&root, "second");
    // Staged, unstaged, untracked, deleted, and a stat-dirty file with equal content
    // (git would refresh the index for it; Lector must not).
    write(&root, "staged.txt", "new\n");
    git(&root, &["add", "staged.txt"]);
    git(&root, &["stash", "list"]);
    write(
        &root,
        "src/lib.rs",
        "pub fn a() -> u32 {\n    3\n}\npub fn b() {}\n",
    );
    write(&root, "untracked dir/ünïcode file.txt", "x\n");
    fs::remove_file(root.join("gone.txt")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&root, "touched.txt", "same content\n");

    let before = snapshot(&root);
    let store_dir = tempfile::tempdir().unwrap();
    let store = ReviewStore::open(store_dir.path().join("review.json"));
    let project = Project::open(&root, settings()).unwrap();
    assert!(project.repo().is_some());
    exercise(&project, &store);
    // Subfolder roots map paths through the repo prefix.
    let sub = Project::open(&root.join("src"), settings()).unwrap();
    exercise(&sub, &store);
    let after = snapshot(&root);

    let changed: Vec<_> = before
        .iter()
        .filter(|(k, v)| after.get(*k) != Some(*v))
        .map(|(k, _)| k.display().to_string())
        .chain(
            after
                .keys()
                .filter(|k| !before.contains_key(*k))
                .map(|k| format!("new: {}", k.display())),
        )
        .collect();
    assert!(
        changed.is_empty(),
        "Lector modified the repository: {changed:?}"
    );
}

#[cfg(unix)]
fn project_load_rejects(root: &Path, rel: &str) -> bool {
    if !root.join(rel).exists() {
        return true;
    }
    let p = Project::open(root, settings()).unwrap();
    p.load_document(Path::new(rel)).is_err()
}

#[test]
fn unusual_files_do_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("files");
    fs::create_dir(&root).unwrap();
    init_repo(&root);
    write(&root, "empty.rs", "");
    write(&root, "newline_only.rs", "\n");
    write(&root, "crlf.rs", "fn a() {\r\n    let x = 1;\r\n}\r\n");
    write(&root, "lone_cr.py", "a = 1\rb = 2\r\n\r");
    write(
        &root,
        "invalid.rs",
        b"fn \xff\xfe() { \"\xe2\x82\" }\n// \xc3\n".as_slice(),
    );
    write(&root, "utf16.txt", b"\xff\xfeh\x00i\x00\n\x00".as_slice());
    // Not "nul.*": NUL is a reserved device name on Windows.
    write(&root, "zeros.bin", b"\x00\x00\x00".as_slice());
    write(
        &root,
        "with space/ünï cödé.md",
        "# Ünïcödé 🎉\n\n- ✓ item\n",
    );
    let long = format!("fn x() {{ let s = \"{}\"; }}\n", "é".repeat(400_000));
    write(&root, "long_line.rs", long);
    write(&root, "deep.json", "[".repeat(5000));
    commit_all(&root, "files");
    // Edits with multibyte content and changed line endings.
    write(&root, "crlf.rs", "fn a() {\n    let x = 2;\n}\n");
    write(
        &root,
        "with space/ünï cödé.md",
        "# Ünïcödé 🎉\n\n- ✗ item\n",
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../files", root.join("loop")).unwrap();
        std::os::unix::fs::symlink(".", root.join("with space/self")).unwrap();
        std::os::unix::fs::symlink("does-not-exist", root.join("broken")).unwrap();
        // An untracked FIFO must not block status, diffs or loading.
        let _ = Command::new("mkfifo").arg(root.join("pipe")).status();
        assert!(project_load_rejects(&root, "pipe"));
    }
    let store_dir = tempfile::tempdir().unwrap();
    let store = ReviewStore::open(store_dir.path().join("review.json"));
    let project = Project::open(&root, settings()).unwrap();
    exercise(&project, &store);
    // Revisions that look like options are rejected, not passed to git.
    let repo = project.repo().unwrap();
    assert!(repo
        .blame("crlf.rs", Some("--contents=/etc/passwd"))
        .is_err());
    // A file that disappears after being listed.
    let missing = project.load_document(Path::new("missing.rs"));
    assert!(missing.is_err());
}

#[test]
fn unusual_repositories_do_not_panic() {
    let store_dir = tempfile::tempdir().unwrap();
    let store = ReviewStore::open(store_dir.path().join("review.json"));
    let dir = tempfile::tempdir().unwrap();

    // Not a repository.
    let plain = dir.path().join("plain");
    write(&plain, "a.txt", "hello\n");
    let p = Project::open(&plain, settings()).unwrap();
    assert!(p.repo().is_none());
    exercise(&p, &store);
    assert!(p.blame("a.txt").is_err());

    // Empty repository (no commits), with staged and untracked files.
    let empty = dir.path().join("empty");
    fs::create_dir(&empty).unwrap();
    init_repo(&empty);
    write(&empty, "staged.rs", "fn a() {}\n");
    git(&empty, &["add", "staged.rs"]);
    write(&empty, "untracked.rs", "fn b() {}\n");
    let p = Project::open(&empty, settings()).unwrap();
    assert!(p.repo().is_some());
    let before = snapshot(&empty);
    exercise(&p, &store);
    assert!(p.repo().unwrap().head().is_ok());
    assert_eq!(before, snapshot(&empty));

    // Detached HEAD.
    let detached = dir.path().join("detached");
    fs::create_dir(&detached).unwrap();
    init_repo(&detached);
    write(&detached, "a.txt", "1\n");
    commit_all(&detached, "one");
    write(&detached, "a.txt", "2\n");
    commit_all(&detached, "two");
    git(&detached, &["checkout", "-q", "--detach", "HEAD~1"]);
    let p = Project::open(&detached, settings()).unwrap();
    let head = p.repo().unwrap().head().unwrap();
    assert!(head.detached);
    exercise(&p, &store);

    // Bare repository: opened as a plain folder.
    let bare = dir.path().join("bare.git");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            detached.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let p = Project::open(&bare, settings()).unwrap();
    exercise(&p, &store);

    // Submodule.
    let outer = dir.path().join("outer");
    fs::create_dir(&outer).unwrap();
    init_repo(&outer);
    write(&outer, "main.rs", "fn main() {}\n");
    commit_all(&outer, "outer");
    git(
        &outer,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            detached.to_str().unwrap(),
            "sub",
        ],
    );
    commit_all(&outer, "add sub");
    write(&outer, "sub/a.txt", "dirty\n");
    let before = snapshot(&outer);
    let p = Project::open(&outer, settings()).unwrap();
    exercise(&p, &store);
    let sub = Project::open(&outer.join("sub"), settings()).unwrap();
    exercise(&sub, &store);
    assert_eq!(before, snapshot(&outer));
}
