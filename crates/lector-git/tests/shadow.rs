//! The shadow repository: byte-exact snapshots and restores, and proof that the user's
//! `.git` is never touched (contents *and* mtimes compared around every operation).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, SystemTime};

use lector_git::shadow::{PruneOptions, SnapshotEntry, SnapshotKind};
use lector_git::{ChangeKind, Repo, ShadowRepo, ShadowSnapshot, Side};

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "3");
        std::env::set_var("GIT_CONFIG_KEY_0", "gc.auto");
        std::env::set_var("GIT_CONFIG_VALUE_0", "0");
        std::env::set_var("GIT_CONFIG_KEY_1", "maintenance.auto");
        std::env::set_var("GIT_CONFIG_VALUE_1", "false");
        std::env::set_var("GIT_CONFIG_KEY_2", "gc.autoDetach");
        std::env::set_var("GIT_CONFIG_VALUE_2", "false");
    });
}

struct Fx {
    work: tempfile::TempDir,
    data: tempfile::TempDir,
}

type TreeState = BTreeMap<PathBuf, (String, Vec<u8>, Option<SystemTime>, u64)>;

impl Fx {
    fn new() -> Fx {
        hermetic();
        let fx = Fx {
            work: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
        };
        fx.git(&["init", "-q", "-b", "main"]);
        fx.git(&["config", "user.name", "t"]);
        fx.git(&["config", "user.email", "t@t"]);
        fx
    }

    fn path(&self) -> &Path {
        self.work.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
        let p = self.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.path().join(rel)).unwrap()
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    fn repo(&self) -> Repo {
        Repo::discover(self.path()).unwrap()
    }

    fn shadow(&self) -> ShadowRepo {
        ShadowRepo::open(self.data.path(), &self.repo()).unwrap()
    }

    /// Every entry of the user's `.git`: kind, bytes, mtime, size.
    fn dot_git(&self) -> TreeState {
        let root = self.path().join(".git");
        let mut out = BTreeMap::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let rel = path.strip_prefix(&root).unwrap().to_path_buf();
                let mtime = meta.modified().ok();
                if meta.is_dir() {
                    out.insert(rel, ("dir".into(), Vec::new(), mtime, 0));
                    stack.push(path);
                } else {
                    out.insert(
                        rel,
                        ("file".into(), fs::read(&path).unwrap(), mtime, meta.len()),
                    );
                }
            }
        }
        out
    }

    fn assert_git_untouched(&self, before: &TreeState, what: &str) {
        let after = self.dot_git();
        let names = |s: &TreeState| s.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            names(before),
            names(&after),
            "{what}: files in .git changed"
        );
        for (path, b) in before {
            let a = &after[path];
            assert_eq!(b.1, a.1, "{what}: {} content changed", path.display());
            assert_eq!(b.2, a.2, "{what}: {} mtime changed", path.display());
        }
    }
}

fn paths_of(changes: &[lector_git::FileChange]) -> Vec<(String, ChangeKind)> {
    changes
        .iter()
        .map(|c| (c.new_path.clone().or(c.old_path.clone()).unwrap(), c.kind))
        .collect()
}

/// A repository with tracked files of every flavour.
fn rich() -> Fx {
    let fx = Fx::new();
    fx.write("src/lib.rs", "fn a() {}\nfn b() {}\n");
    fx.write("crlf.txt", "one\r\ntwo\r\nthree\r\n");
    fx.write("blob.bin", [0u8, 159, 146, 150, 0, 255, 10, 13, 0]);
    fx.write("weird name 'q' é.txt", "quoted\n");
    fx.write("dir/nested/deep.txt", "deep\n");
    fx.write("run.sh", "#!/bin/sh\necho hi\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fx.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("src/lib.rs", fx.path().join("link")).unwrap();
    }
    fx.commit_all("init");
    fx
}

#[test]
fn user_git_dir_is_untouched_by_every_shadow_operation() {
    let fx = rich();
    // Dirty state: modified, untracked, staged, deleted.
    fx.write("src/lib.rs", "fn a() { 1 }\nfn b() {}\n");
    fx.write("new.txt", "untracked\n");
    fx.write("crlf.txt", "one\r\nTWO\r\nthree\r\n");
    fx.git(&["add", "crlf.txt"]);
    fs::remove_file(fx.path().join("dir/nested/deep.txt")).unwrap();
    // Loose objects, a pack, mixed: pack part of the history so alternates have both.
    fx.git(&["gc", "-q"]);
    fx.write("later.txt", "later\n");
    fx.commit_all("later");
    fx.write("src/lib.rs", "fn a() { 2 }\n");

    let before = fx.dot_git();
    // Let mtimes differ measurably if anything were to be touched.
    std::thread::sleep(Duration::from_millis(1100));

    let shadow = fx.shadow();
    fx.assert_git_untouched(&before, "open");

    let paths = [
        "src/lib.rs",
        "new.txt",
        "missing.txt",
        "dir",
        "crlf.txt",
        "run.sh",
        "link",
        "later.txt",
        "blob.bin",
    ];
    let s1 = shadow.snapshot_paths(&paths, "paths").unwrap();
    fx.assert_git_untouched(&before, "snapshot_paths");
    let s2 = shadow.snapshot_index(&paths, "index").unwrap();
    fx.assert_git_untouched(&before, "snapshot_index");
    let s3 = shadow.snapshot_worktree("whole").unwrap();
    fx.assert_git_untouched(&before, "snapshot_worktree");

    shadow.changes_between(&s1, &s3).unwrap();
    shadow.changes_to_worktree(&s1, None).unwrap();
    shadow.entries(&s3).unwrap();
    shadow.index_entries(&s2).unwrap();
    shadow.file_at(&s1, "src/lib.rs").unwrap();
    shadow.fingerprint_worktree(&paths).unwrap();
    shadow.snapshots().unwrap();
    fx.assert_git_untouched(&before, "reads");

    // Same content again: everything already exists in the user's objects.
    fx.git(&["checkout", "-q", "--", "."]);
    let after_checkout = fx.dot_git();
    std::thread::sleep(Duration::from_millis(1100));
    let s4 = shadow.snapshot_worktree("clean").unwrap();
    shadow
        .snapshot_paths(&["src/lib.rs", "later.txt"], "clean paths")
        .unwrap();
    fx.assert_git_untouched(&after_checkout, "snapshot of already-stored content");

    // Restores write the working tree only (not .git); restore_index writes the index
    // but nothing else.
    shadow
        .restore_paths(&s1, &["src/lib.rs", "new.txt"])
        .unwrap();
    fx.assert_git_untouched(&after_checkout, "restore_paths");
    shadow
        .prune_with(PruneOptions {
            keep: Duration::ZERO,
            max_bytes: None,
        })
        .unwrap();
    fx.assert_git_untouched(&after_checkout, "prune");
    let _ = (s4, s2);
}

#[test]
fn restore_index_changes_only_the_index_file() {
    let fx = rich();
    fx.write("src/lib.rs", "staged\n");
    fx.git(&["add", "src/lib.rs"]);
    fx.write("src/lib.rs", "worktree\n");
    let shadow = fx.shadow();
    let snap = shadow
        .snapshot_index(&["src/lib.rs", "nothing.txt"], "before")
        .unwrap();
    fx.git(&["reset", "-q"]);
    assert_eq!(fx.git(&["diff", "--cached", "--name-only"]), "");
    let before = fx.dot_git();
    shadow.restore_index(&snap, &["src/lib.rs"]).unwrap();
    assert_eq!(fx.git(&["diff", "--cached", "--name-only"]), "src/lib.rs\n");
    assert_eq!(fx.git(&["show", ":src/lib.rs"]), "staged\n");
    assert_eq!(fx.read("src/lib.rs"), b"worktree\n", "worktree untouched");
    let after = fx.dot_git();
    let changed: Vec<_> = before
        .iter()
        .filter(|(k, v)| after.get(*k).map(|a| a.1 != v.1).unwrap_or(true))
        .map(|(k, _)| k.display().to_string())
        .collect();
    assert!(changed.iter().all(|c| c == "index"), "{changed:?}");
}

#[test]
fn restore_index_handles_unstaged_new_files_and_conflict_stages() {
    let fx = rich();
    // Path not in the index at snapshot time -> restore removes the staged entry.
    fx.write("fresh.txt", "x\n");
    let shadow = fx.shadow();
    let snap = shadow
        .snapshot_index(&["fresh.txt"], "before staging")
        .unwrap();
    fx.git(&["add", "fresh.txt"]);
    assert!(fx.git(&["ls-files"]).contains("fresh.txt"));
    shadow.restore_index(&snap, &["fresh.txt"]).unwrap();
    assert!(!fx.git(&["ls-files"]).contains("fresh.txt"));
    assert_eq!(fx.read("fresh.txt"), b"x\n");

    // A real conflict: stages 1..3 survive a snapshot / resolution / restore.
    fx.commit_all("fresh");
    fx.git(&["checkout", "-q", "-b", "feature"]);
    fx.write("c.txt", "base\n");
    fx.commit_all("base c");
    fx.write("c.txt", "feature\n");
    fx.commit_all("feature c");
    fx.git(&["checkout", "-q", "main"]);
    fx.git(&["merge", "-q", "feature", "--no-edit", "--ff-only"]);
    fx.git(&["checkout", "-q", "-b", "other", "HEAD~1"]);
    fx.write("c.txt", "other\n");
    fx.commit_all("other c");
    let merge = Command::new("git")
        .args(["merge", "feature"])
        .current_dir(fx.path())
        .output()
        .unwrap();
    assert!(!merge.status.success());
    let stages = fx.git(&["ls-files", "-u"]);
    assert_eq!(stages.lines().count(), 3, "{stages}");
    let snap = shadow.snapshot_index(&["c.txt"], "conflicted").unwrap();
    assert_eq!(shadow.index_entries(&snap).unwrap().len(), 3);
    fx.write("c.txt", "resolved\n");
    fx.git(&["add", "c.txt"]);
    assert_eq!(fx.git(&["ls-files", "-u"]), "");
    shadow.restore_index(&snap, &["c.txt"]).unwrap();
    assert_eq!(fx.git(&["ls-files", "-u"]), stages);
}

#[test]
fn restores_exact_bytes_modes_links_and_absence() {
    let fx = rich();
    let shadow = fx.shadow();
    let originals: Vec<(&str, Vec<u8>)> = vec![
        ("src/lib.rs", fx.read("src/lib.rs")),
        ("crlf.txt", fx.read("crlf.txt")),
        ("blob.bin", fx.read("blob.bin")),
        ("weird name 'q' é.txt", fx.read("weird name 'q' é.txt")),
        ("dir/nested/deep.txt", fx.read("dir/nested/deep.txt")),
        ("run.sh", fx.read("run.sh")),
    ];
    // Uncommitted edits that "discard" would lose, plus an untracked file, plus a path
    // that does not exist.
    fx.write("src/lib.rs", "edited\nwithout final newline");
    fx.write("crlf.txt", "mixed\r\nendings\n");
    fx.write("untracked.dat", [1u8, 0, 2, 0, 3, 13, 10]);
    let names: Vec<&str> = originals.iter().map(|(n, _)| *n).collect();
    let mut all = names.clone();
    all.extend(["untracked.dat", "link", "ghost.txt"]);
    let snap = shadow.snapshot_paths(&all, "Before discarding").unwrap();
    assert_eq!(snap.note, "Before discarding");
    assert_eq!(shadow.kind(&snap).unwrap(), SnapshotKind::Paths);
    assert!(shadow.contains(&snap));

    // Destroy: discard everything the way git would, and more.
    fx.git(&["checkout", "--", "."]);
    fs::remove_file(fx.path().join("untracked.dat")).unwrap();
    fs::remove_dir_all(fx.path().join("dir")).unwrap();
    fs::remove_file(fx.path().join("run.sh")).unwrap();
    fx.write("ghost.txt", "appeared later");
    #[cfg(unix)]
    {
        fs::remove_file(fx.path().join("link")).unwrap();
        fx.write("link", "now a regular file");
    }

    shadow.restore_paths(&snap, &all).unwrap();

    for (name, bytes) in &originals {
        // Uncommitted state at snapshot time == the originals for unchanged files.
        let expected = match *name {
            "src/lib.rs" => b"edited\nwithout final newline".to_vec(),
            "crlf.txt" => b"mixed\r\nendings\n".to_vec(),
            _ => bytes.clone(),
        };
        assert_eq!(fx.read(name), expected, "{name}");
    }
    assert_eq!(fx.read("untracked.dat"), [1u8, 0, 2, 0, 3, 13, 10]);
    assert!(
        !fx.path().join("ghost.txt").exists(),
        "recorded absent -> deleted"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(fx.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "exec bit kept: {mode:o}");
        let mode = fs::metadata(fx.path().join("src/lib.rs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0);
        let target = fs::read_link(fx.path().join("link")).unwrap();
        assert_eq!(target, Path::new("src/lib.rs"));
    }
    // No temp files left behind.
    assert!(!fx
        .git(&["status", "--porcelain", "-uall"])
        .contains(".lector-restore"));
}

#[test]
fn restore_keeps_permissions_of_an_existing_file() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let fx = rich();
        let shadow = fx.shadow();
        let snap = shadow.snapshot_paths(&["src/lib.rs"], "s").unwrap();
        fs::set_permissions(
            fx.path().join("src/lib.rs"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        fx.write("src/lib.rs", "changed");
        shadow.restore_paths(&snap, &["src/lib.rs"]).unwrap();
        let mode = fs::metadata(fx.path().join("src/lib.rs"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_eq!(fx.read("src/lib.rs"), b"fn a() {}\nfn b() {}\n");
    }
}

#[test]
fn restore_refuses_unsafe_paths_and_writes_nothing() {
    let fx = rich();
    let shadow = fx.shadow();
    let snap = shadow
        .snapshot_paths(&["src/lib.rs", "ghost.txt"], "s")
        .unwrap();
    for bad in ["../outside.txt", "/etc/passwd", ".git/config", "dir/.git/x"] {
        assert!(shadow.snapshot_paths(&[bad], "x").is_err(), "{bad}");
        assert!(shadow.restore_paths(&snap, &[bad]).is_err(), "{bad}");
    }
    // Not part of the snapshot.
    assert!(shadow.restore_paths(&snap, &["crlf.txt"]).is_err());

    // A directory now stands where an absent-recorded file was: refuse, change nothing.
    fx.write("src/lib.rs", "changed");
    fs::create_dir_all(fx.path().join("ghost.txt")).unwrap();
    fx.write("ghost.txt/inner", "x");
    let err = shadow
        .restore_paths(&snap, &["src/lib.rs", "ghost.txt"])
        .unwrap_err();
    assert!(err.to_string().contains("directory"), "{err}");
    assert_eq!(
        fx.read("src/lib.rs"),
        b"changed",
        "nothing written on refusal"
    );

    // Never write through a symlinked directory.
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        fs::remove_dir_all(fx.path().join("dir")).unwrap();
        std::os::unix::fs::symlink(outside.path(), fx.path().join("dir")).unwrap();
        assert!(shadow
            .snapshot_paths(&["dir/nested/deep.txt"], "x")
            .is_err());
        let snap2 = shadow.snapshot_paths(&["src/lib.rs"], "s2").unwrap();
        assert!(shadow.restore_paths(&snap2, &["dir/x.txt"]).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[test]
fn restore_recreates_deleted_directories_and_prunes_empty_ones_it_deletes() {
    let fx = rich();
    let shadow = fx.shadow();
    let snap = shadow
        .snapshot_paths(&["dir", "made/up/later.txt"], "s")
        .unwrap();
    fs::remove_dir_all(fx.path().join("dir")).unwrap();
    fx.write("made/up/later.txt", "created after");
    shadow
        .restore_paths(&snap, &["dir", "made/up/later.txt"])
        .unwrap();
    assert_eq!(fx.read("dir/nested/deep.txt"), b"deep\n");
    assert!(
        !fx.path().join("made").exists(),
        "emptied parents are removed"
    );
}

#[test]
fn changes_between_reports_file_changes_with_loadable_sides() {
    let fx = rich();
    let shadow = fx.shadow();
    fx.write("src/lib.rs", "v1\n");
    fx.write("gone.txt", "will go\n");
    let a = shadow
        .snapshot_paths(&["src/lib.rs", "gone.txt", "added.txt", "crlf.txt"], "a")
        .unwrap();
    fx.write("src/lib.rs", "v2\n");
    fs::remove_file(fx.path().join("gone.txt")).unwrap();
    fx.write("added.txt", "new\n");
    let b = shadow
        .snapshot_paths(&["src/lib.rs", "gone.txt", "added.txt", "crlf.txt"], "b")
        .unwrap();
    let changes = shadow.changes_between(&a, &b).unwrap();
    assert_eq!(
        paths_of(&changes),
        vec![
            ("added.txt".to_owned(), ChangeKind::Added),
            ("gone.txt".to_owned(), ChangeKind::Deleted),
            ("src/lib.rs".to_owned(), ChangeKind::Modified),
        ]
    );
    let lib = changes
        .iter()
        .find(|c| c.new_path.as_deref() == Some("src/lib.rs"))
        .unwrap();
    assert_eq!(shadow.load(&lib.old).unwrap().unwrap(), b"v1\n");
    assert_eq!(shadow.load(&lib.new).unwrap().unwrap(), b"v2\n");
    assert_eq!(shadow.load(&Side::None).unwrap(), None);

    // A narrower snapshot never turns the rest into deletions.
    let narrow = shadow.snapshot_paths(&["src/lib.rs"], "narrow").unwrap();
    let changes = shadow.changes_between(&narrow, &b).unwrap();
    assert!(changes.is_empty(), "{:?}", paths_of(&changes));

    // And against the live working tree.
    fx.write("src/lib.rs", "v3\n");
    let live = shadow.changes_to_worktree(&a, None).unwrap();
    let kinds = paths_of(&live);
    assert!(
        kinds.contains(&("src/lib.rs".to_owned(), ChangeKind::Modified)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("gone.txt".to_owned(), ChangeKind::Deleted)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("added.txt".to_owned(), ChangeKind::Added)),
        "{kinds:?}"
    );
    assert!(!kinds.iter().any(|(p, _)| p == "crlf.txt"));
    let only = shadow
        .changes_to_worktree(&a, Some(&["src/lib.rs"]))
        .unwrap();
    assert_eq!(only.len(), 1);
}

#[test]
fn worktree_snapshot_honours_gitignore_and_includes_untracked() {
    let fx = rich();
    fx.write(".gitignore", "target/\n*.log\n");
    fx.commit_all("ignore");
    fx.write("target/out.bin", "build output");
    fx.write("debug.log", "log");
    fx.write("untracked/new.rs", "fn new() {}\n");
    fx.write("src/lib.rs", "dirty\n");
    fs::remove_file(fx.path().join("crlf.txt")).unwrap();
    let shadow = fx.shadow();
    let snap = shadow.snapshot_worktree("checkpoint").unwrap();
    assert_eq!(shadow.kind(&snap).unwrap(), SnapshotKind::Worktree);
    let present: Vec<String> = shadow
        .entries(&snap)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e {
            SnapshotEntry::Present { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert!(
        present.contains(&"untracked/new.rs".to_owned()),
        "{present:?}"
    );
    assert!(present.contains(&"src/lib.rs".to_owned()));
    assert!(present.contains(&".gitignore".to_owned()));
    assert!(
        !present
            .iter()
            .any(|p| p.starts_with("target/") || p.ends_with(".log")),
        "{present:?}"
    );
    assert!(
        !present.contains(&"crlf.txt".to_owned()),
        "deleted file not recorded"
    );
    assert!(!present.iter().any(|p| p.starts_with(".git/")));
    assert_eq!(
        shadow.file_at(&snap, "src/lib.rs").unwrap().unwrap(),
        b"dirty\n"
    );

    // Restoring from a whole-tree snapshot: edit, delete and add after the checkpoint.
    fx.write("src/lib.rs", "worse\n");
    fx.write("agent_made.txt", "created after checkpoint");
    shadow
        .restore_paths(&snap, &["src/lib.rs", "agent_made.txt", "crlf.txt"])
        .unwrap();
    assert_eq!(fx.read("src/lib.rs"), b"dirty\n");
    assert!(!fx.path().join("agent_made.txt").exists());
    assert!(!fx.path().join("crlf.txt").exists());
}

#[test]
fn awkward_names_survive() {
    let fx = Fx::new();
    // Windows forbids tab, newline, backslash, quote and `*` in file names.
    #[cfg(not(windows))]
    let names = [
        "with space.txt",
        "tab\there.txt",
        "new\nline.txt",
        "back\\slash.txt",
        "quo\"te.txt",
        "ünïcödé/файл.txt",
        "-leading-dash",
        "*glob[x].txt",
    ];
    #[cfg(windows)]
    let names = [
        "with space.txt",
        "ünïcödé/файл.txt",
        "-leading-dash",
        "glob[x].txt",
        "it's #1 & more%.txt",
    ];
    for n in names {
        fx.write(n, format!("content of {n}"));
    }
    let shadow = fx.shadow();
    let snap = shadow.snapshot_paths(&names, "odd").unwrap();
    for n in names {
        fs::remove_file(fx.path().join(n)).unwrap();
    }
    shadow.restore_paths(&snap, &names).unwrap();
    for n in names {
        assert_eq!(fx.read(n), format!("content of {n}").as_bytes(), "{n:?}");
    }
    let state_a = shadow.fingerprint_worktree(&names).unwrap();
    assert_eq!(state_a, shadow.fingerprint_worktree(&names).unwrap());
    fx.write("with space.txt", "changed");
    assert_ne!(state_a, shadow.fingerprint_worktree(&names).unwrap());
}

#[test]
fn fingerprint_notices_content_exec_bit_and_absence() {
    let fx = rich();
    let shadow = fx.shadow();
    let paths = ["src/lib.rs", "run.sh", "ghost"];
    let base = shadow.fingerprint_worktree(&paths).unwrap();
    assert_eq!(base, shadow.fingerprint_worktree(&paths).unwrap());
    fx.write("ghost", "x");
    let with_ghost = shadow.fingerprint_worktree(&paths).unwrap();
    assert_ne!(base, with_ghost);
    fx.write("src/lib.rs", "other");
    assert_ne!(with_ghost, shadow.fingerprint_worktree(&paths).unwrap());
    // Directories expand to their files.
    let d1 = shadow.fingerprint_worktree(&["dir"]).unwrap();
    fx.write("dir/nested/extra.txt", "x");
    assert_ne!(d1, shadow.fingerprint_worktree(&["dir"]).unwrap());
}

#[test]
fn shadow_location_is_stable_per_workdir_and_uses_alternates() {
    let fx = rich();
    let a = fx.shadow();
    let b = fx.shadow();
    assert_eq!(a.git_dir(), b.git_dir());
    assert!(a.git_dir().starts_with(fx.data.path().join("shadow")));
    assert!(a.git_dir().extension().is_some_and(|e| e == "git"));
    let alternates = fs::read_to_string(a.git_dir().join("objects/info/alternates")).unwrap();
    let user_objects = dunce::canonicalize(fx.path().join(".git/objects")).unwrap();
    assert_eq!(alternates.trim(), user_objects.to_string_lossy());
    assert!(
        !a.git_dir().starts_with(fx.path()),
        "never inside the user's repo"
    );

    let other = rich();
    let s_other = ShadowRepo::open(fx.data.path(), &other.repo()).unwrap();
    assert_ne!(a.git_dir(), s_other.git_dir());

    // A snapshot of content that exists in the user's history is shared, not copied.
    let before = a.snapshot_paths(&["src/lib.rs"], "x").unwrap();
    assert!(a.contains(&before));
    // The shadow can read the user's objects through the alternates.
    let head = fx.git(&["rev-parse", "HEAD"]);
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(a.git_dir())
        .args(["cat-file", "-t", head.trim()])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "commit");
}

#[test]
fn worktrees_use_the_main_repositorys_objects() {
    let fx = rich();
    let wt = tempfile::tempdir().unwrap();
    let wt_path = wt.path().join("wt");
    fx.git(&[
        "worktree",
        "add",
        "-q",
        wt_path.to_str().unwrap(),
        "-b",
        "wt-branch",
    ]);
    let repo = Repo::discover(&wt_path).unwrap();
    let shadow = ShadowRepo::open(fx.data.path(), &repo).unwrap();
    let alternates = fs::read_to_string(shadow.git_dir().join("objects/info/alternates")).unwrap();
    let main_objects = dunce::canonicalize(fx.path().join(".git/objects")).unwrap();
    assert_eq!(alternates.trim(), main_objects.to_string_lossy());
    fs::write(wt_path.join("src/lib.rs"), "wt edit").unwrap();
    let snap = shadow.snapshot_paths(&["src/lib.rs"], "wt").unwrap();
    fs::write(wt_path.join("src/lib.rs"), "gone").unwrap();
    shadow.restore_paths(&snap, &["src/lib.rs"]).unwrap();
    assert_eq!(fs::read(wt_path.join("src/lib.rs")).unwrap(), b"wt edit");
}

#[test]
fn prune_drops_old_snapshots_and_enforces_the_size_cap() {
    let fx = Fx::new();
    let shadow = fx.shadow();
    let mut snaps: Vec<ShadowSnapshot> = Vec::new();
    for i in 0..6u32 {
        // ~600 KB of incompressible-ish data per snapshot.
        let mut data = Vec::new();
        let mut x = 0x9e3779b9u32.wrapping_mul(i + 1);
        for _ in 0..150_000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            data.extend_from_slice(&x.to_le_bytes());
        }
        fx.write("big.bin", &data);
        snaps.push(
            shadow
                .snapshot_paths(&["big.bin"], &format!("s{i}"))
                .unwrap(),
        );
    }
    assert_eq!(shadow.snapshots().unwrap().len(), 6);

    let report = shadow
        .prune_with(PruneOptions {
            keep: Duration::from_secs(3600),
            max_bytes: Some(1_500_000),
        })
        .unwrap();
    assert!(report.removed >= 3, "{report:?}");
    assert!(report.kept >= 1 && report.kept < 6);
    assert!(report.bytes <= 1_500_000 || report.kept == 1, "{report:?}");
    let left = shadow.snapshots().unwrap();
    assert_eq!(left.len(), report.kept);
    assert!(shadow.contains(left.last().unwrap()) && left.iter().all(|s| shadow.contains(s)));
    // The newest snapshot is the one that stays.
    let newest = snaps.last().unwrap();
    assert!(left.iter().any(|s| s.commit == newest.commit));
    // A pruned snapshot says so clearly.
    let gone = snaps
        .iter()
        .find(|s| !left.iter().any(|l| l.commit == s.commit))
        .unwrap();
    assert!(!shadow.contains(gone));
    let err = shadow.restore_paths(gone, &["big.bin"]).unwrap_err();
    assert!(
        err.to_string().contains("pruned") || err.to_string().contains("no longer"),
        "{err}"
    );

    // Age-based: keep = 0 removes everything.
    shadow.prune(Duration::ZERO).unwrap();
    assert!(shadow.snapshots().unwrap().is_empty());
}

#[test]
fn many_snapshots_stay_cheap_and_consolidate() {
    let fx = Fx::new();
    fx.write("a.txt", "a");
    let shadow = fx.shadow();
    for i in 0..45 {
        fx.write("a.txt", format!("version {i}"));
        shadow.snapshot_paths(&["a.txt"], &format!("n{i}")).unwrap();
    }
    let packs = fs::read_dir(shadow.git_dir().join("objects/pack"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "pack")
        })
        .count();
    assert!(packs < 40, "{packs} packs");
    assert_eq!(shadow.snapshots().unwrap().len(), 45);
}

#[test]
fn bare_repositories_are_refused() {
    hermetic();
    let dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(dir.path())
        .output()
        .unwrap();
    let repo = Repo::discover(dir.path()).unwrap();
    assert!(ShadowRepo::open(data.path(), &repo).is_err());
}

#[test]
fn fetch_pull_request_into_shadow_leaves_user_repo_untouched() {
    use lector_git::shadow::FetchOptions;
    // "Upstream" with a pull request ref.
    let up = Fx::new();
    up.write("src/lib.rs", "fn a() {}\n");
    up.commit_all("base");
    up.git(&["checkout", "-q", "-b", "feature"]);
    up.write("src/lib.rs", "fn a() { 42 }\nfn b() {}\n");
    up.commit_all("feature");
    let head = up.git(&["rev-parse", "HEAD"]).trim().to_string();
    up.git(&["update-ref", "refs/pull/7/head", "HEAD"]);
    up.git(&["checkout", "-q", "main"]);
    let base = up.git(&["rev-parse", "HEAD"]).trim().to_string();
    let url = up.path().to_string_lossy().into_owned();

    // The user's clone knows main only.
    let fx = Fx::new();
    fx.git(&["fetch", "-q", &url, "main:refs/remotes/origin/main"]);
    fx.write("local.txt", "dirty\n");
    let shadow = fx.shadow();
    let before = fx.dot_git();

    shadow
        .fetch(
            &url,
            &[
                "+refs/pull/7/head:refs/lector/pr/7/head",
                "+refs/heads/main:refs/lector/pr/7/base",
            ],
        )
        .unwrap();
    fx.assert_git_untouched(&before, "fetch");
    assert_eq!(
        shadow
            .resolve("refs/lector/pr/7/head")
            .unwrap()
            .map(|o| o.to_string()),
        Some(head.clone())
    );
    assert_eq!(
        shadow
            .merge_base(&head, "refs/lector/pr/7/base")
            .unwrap()
            .map(|o| o.to_string()),
        Some(base.clone())
    );
    // Commits exist only in the shadow.
    let in_user = Command::new("git")
        .args(["cat-file", "-e", &format!("{head}^{{commit}}")])
        .current_dir(fx.path())
        .output()
        .unwrap();
    assert!(
        !in_user.status.success(),
        "PR head leaked into the user repo"
    );
    // The shadow can be read as a repository for review (alternates included).
    let shadow_repo = Repo::discover(shadow.git_dir()).unwrap();
    let text = shadow_repo.file_at(&head, "src/lib.rs").unwrap().unwrap();
    assert_eq!(text, b"fn a() { 42 }\nfn b() {}\n");

    // Fetching again (nothing new) is fine and still leaves the user repo alone.
    let opts = FetchOptions {
        http_header: Some("Authorization: Basic c2VjcmV0".into()),
    };
    assert!(!format!("{opts:?}").contains("c2VjcmV0"));
    shadow
        .fetch_with(&url, &["+refs/pull/7/head:refs/lector/pr/7/head"], &opts)
        .unwrap();
    fx.assert_git_untouched(&before, "second fetch");

    // Refspecs must stay inside refs/lector/ and out of the snapshot namespace.
    for bad in [
        "refs/pull/7/head:refs/heads/main",
        "refs/pull/7/head",
        "+refs/pull/7/head:refs/lector/snapshots/x",
        "--upload-pack=evil:refs/lector/x",
    ] {
        assert!(shadow.fetch(&url, &[bad]).is_err(), "{bad}");
    }
    assert!(shadow
        .fetch("--upload-pack=x", &["a:refs/lector/a"])
        .is_err());
    assert!(shadow.resolve("refs/lector/pr/8/head").unwrap().is_none());
    fx.assert_git_untouched(&before, "refusals");
}

/// Regression: the alternates entry was written unquoted, so a repository path containing a
/// newline (or starting with `"`) produced a broken entry and the shadow could not see the
/// user's objects.
#[cfg(unix)]
#[test]
fn alternates_entry_is_quoted_for_awkward_paths() {
    hermetic();
    let base = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let work = base.path().join("we\"ird\nrepo");
    fs::create_dir_all(&work).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q", "-b", "main"]);
    fs::write(work.join("a.txt"), "a\n").unwrap();
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "init",
    ]);
    let repo = Repo::discover(&work).unwrap();
    let shadow = ShadowRepo::open(data.path(), &repo).unwrap();
    let alternates = fs::read_to_string(shadow.git_dir().join("objects/info/alternates")).unwrap();
    assert_eq!(alternates.lines().count(), 1, "{alternates:?}");
    let head = git(&["rev-parse", "HEAD"]);
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(shadow.git_dir())
        .args(["cat-file", "-t", head.trim()])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "commit");
}
