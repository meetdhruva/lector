use super::*;
use std::fs;

struct Fx {
    _dir: tempfile::TempDir,
    outside: PathBuf,
    root: PathBuf,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git/hooks")).unwrap();
    fs::write(root.join(".git/config"), "[core]\n").unwrap();
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(root.join("src/lib.rs"), "lib\n").unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), "secret\n").unwrap();
    let root = dunce::canonicalize(&root).unwrap();
    let outside = dunce::canonicalize(&outside).unwrap();
    Fx {
        _dir: dir,
        outside,
        root,
    }
}

fn c(fx: &Fx) -> Confinement {
    Confinement::new(&fx.root).unwrap()
}

#[test]
fn plain_paths_inside_and_outside() {
    let fx = fx();
    let c = c(&fx);
    let ok = c.read("src/lib.rs").unwrap();
    assert_eq!(ok.path(), fx.root.join("src/lib.rs"));
    assert_eq!(ok.relative_slash(), "src/lib.rs");
    assert_eq!(ok.read().unwrap(), b"lib\n");
    assert!(c.write("src/new/deep.rs").is_ok());
    assert!(matches!(c.read(""), Err(ConfineError::Empty)));
    assert!(matches!(c.read("  "), Err(ConfineError::Empty)));
    assert!(matches!(c.read("../x"), Err(ConfineError::Outside { .. })));
    assert!(matches!(
        c.read("src/../../x"),
        Err(ConfineError::Outside { .. })
    ));
    assert!(matches!(
        c.write(fx.outside.join("secret")),
        Err(ConfineError::Outside { .. })
    ));
    // Absolute paths inside the root are fine.
    assert!(c.write(fx.root.join("src/abs.rs")).is_ok());
    // `src/..` and `./` are plain.
    assert_eq!(c.read("./src/../a.txt").unwrap().relative_slash(), "a.txt");
    assert!(matches!(
        c.read(".git/config"),
        Err(ConfineError::GitDir { .. })
    ));
}

#[test]
fn git_directory_names_in_every_spelling() {
    let fx = fx();
    let c = c(&fx);
    for p in [
        ".git/config",
        ".GIT/config",
        ".Git./hooks/pre-commit",
        "sub/.git /x",
        ".git\u{200c}/config",
        ".git::$INDEX_ALLOCATION/config",
        "GIT~1/config",
        "git~2/config",
        "nested/.git",
    ] {
        assert!(
            matches!(c.write(p), Err(ConfineError::GitDir { .. })),
            "{p}: {:?}",
            c.write(p)
        );
        assert!(c.read(p).is_err(), "{p}");
    }
    for p in [
        ".gitignore",
        ".github/workflows/ci.yml",
        "git~/x",
        "src/git",
    ] {
        assert!(c.write(p).is_ok(), "{p}");
    }
}

#[test]
fn writes_create_directories_and_keep_modes() {
    let fx = fx();
    let c = c(&fx);
    let target = c.write("src/new/deep.rs").unwrap();
    target.write(b"deep\n", CreateDirs::Yes).unwrap();
    assert_eq!(
        fs::read(fx.root.join("src/new/deep.rs")).unwrap(),
        b"deep\n"
    );
    let missing = c.write("nope/x.rs").unwrap();
    assert!(missing.write(b"x", CreateDirs::No).is_err());
    assert!(!fx.root.join("nope").exists());
    // A read-confined path cannot be written.
    assert!(c
        .read("a.txt")
        .unwrap()
        .write(b"x", CreateDirs::No)
        .is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = fx.root.join("run.sh");
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o750)).unwrap();
        c.write("run.sh")
            .unwrap()
            .write(b"#!/bin/sh\necho\n", CreateDirs::No)
            .unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o750
        );
        fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
        let err = c.write("run.sh").unwrap().write(b"x", CreateDirs::No);
        assert!(
            matches!(err, Err(ConfineError::Io { ref source, .. }) if source.kind() == io::ErrorKind::PermissionDenied)
        );
    }
    let names: Vec<String> = fs::read_dir(&fx.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
}

#[test]
fn forbidden_directories_inside_the_root() {
    let fx = fx();
    let data = fx.root.join("lector-data");
    fs::create_dir_all(data.join("shadow")).unwrap();
    let c = Confinement::new(&fx.root)
        .unwrap()
        .forbid(&data)
        .forbid(&fx.outside);
    assert!(matches!(
        c.read("lector-data/shadow"),
        Err(ConfineError::Forbidden { .. })
    ));
    assert!(matches!(
        c.write("lector-data/new.txt"),
        Err(ConfineError::Forbidden { .. })
    ));
    assert!(c.write("lector-data2/x").is_ok());
}

#[cfg(unix)]
mod links {
    use super::*;
    use std::os::unix::fs::symlink;

    /// The audit's cases: `docs -> .git` and `cfg -> .git/config` committed in a project.
    #[test]
    fn the_audit_cases_are_refused() {
        let fx = fx();
        symlink(".git", fx.root.join("docs")).unwrap();
        symlink(".git/config", fx.root.join("cfg")).unwrap();
        let c = c(&fx);
        for p in ["docs/hooks/pre-commit", "docs/config", "docs/newdir/x"] {
            assert!(
                matches!(c.write(p), Err(ConfineError::GitDir { .. })),
                "{p}: {:?}",
                c.write(p)
            );
            assert!(c.read(p).is_err(), "{p}");
        }
        assert!(matches!(
            c.write("cfg"),
            Err(ConfineError::FinalSymlink { .. })
        ));
        assert!(matches!(c.read("cfg"), Err(ConfineError::GitDir { .. })));
        assert!(matches!(
            c.write(".git/config"),
            Err(ConfineError::GitDir { .. })
        ));
        assert_eq!(fs::read(fx.root.join(".git/config")).unwrap(), b"[core]\n");
        assert!(!fx.root.join(".git/newdir").exists());
    }

    #[test]
    fn link_chains_resolve_and_are_checked() {
        let fx = fx();
        symlink("src/lib.rs", fx.root.join("l1")).unwrap();
        symlink("l1", fx.root.join("l2")).unwrap();
        symlink(fx.root.join("l2"), fx.root.join("l3")).unwrap();
        symlink(".git", fx.root.join("g1")).unwrap();
        symlink("g1/config", fx.root.join("g2")).unwrap();
        symlink(&fx.outside, fx.root.join("o1")).unwrap();
        symlink("o1/secret", fx.root.join("o2")).unwrap();
        let c = c(&fx);
        let r = c.read("l3").unwrap();
        assert_eq!(r.relative_slash(), "src/lib.rs");
        assert_eq!(r.read().unwrap(), b"lib\n");
        assert!(matches!(
            c.write("l3"),
            Err(ConfineError::FinalSymlink { .. })
        ));
        assert!(matches!(c.read("g2"), Err(ConfineError::GitDir { .. })));
        assert!(matches!(c.read("o2"), Err(ConfineError::Outside { .. })));
        assert!(matches!(
            c.write("o1/new"),
            Err(ConfineError::Outside { .. })
        ));
        // A loop.
        symlink("loop2", fx.root.join("loop1")).unwrap();
        symlink("loop1", fx.root.join("loop2")).unwrap();
        assert!(matches!(
            c.read("loop1"),
            Err(ConfineError::Unresolvable { .. })
        ));
        // Dangling links: a write never goes through one; a read resolves to the target.
        symlink(fx.outside.join("planted"), fx.root.join("dangling")).unwrap();
        assert!(matches!(
            c.write("dangling"),
            Err(ConfineError::FinalSymlink { .. })
        ));
        assert!(matches!(
            c.read("dangling"),
            Err(ConfineError::Outside { .. })
        ));
        symlink(fx.outside.join("nodir"), fx.root.join("dlink")).unwrap();
        assert!(matches!(
            c.write("dlink/x.rs"),
            Err(ConfineError::Outside { .. })
        ));
        assert!(!fx.outside.join("planted").exists());
    }

    #[test]
    fn missing_tails_under_a_linked_directory() {
        let fx = fx();
        symlink("src", fx.root.join("src2")).unwrap();
        let c = c(&fx);
        let w = c.write("src2/new/deep.rs").unwrap();
        assert_eq!(w.relative_slash(), "src/new/deep.rs");
        w.write(b"x", CreateDirs::Yes).unwrap();
        assert_eq!(fs::read(fx.root.join("src/new/deep.rs")).unwrap(), b"x");
        // Missing parts, then `..`, then a link again.
        symlink(".git", fx.root.join("docs")).unwrap();
        assert!(matches!(
            c.write("missing/../docs/config"),
            Err(ConfineError::GitDir { .. })
        ));
    }

    #[test]
    fn parent_dir_after_a_link_is_the_targets_parent() {
        let fx = fx();
        symlink(".git/hooks", fx.root.join("h")).unwrap();
        symlink(&fx.outside, fx.root.join("out")).unwrap();
        let c = c(&fx);
        // h/.. is .git, h/../.. is the root.
        assert!(matches!(
            c.read("h/../config"),
            Err(ConfineError::GitDir { .. })
        ));
        assert_eq!(c.read("h/../../a.txt").unwrap().relative_slash(), "a.txt");
        assert!(matches!(
            c.read("out/../x"),
            Err(ConfineError::Outside { .. })
        ));
    }

    #[test]
    fn a_root_that_is_a_link() {
        let fx = fx();
        let link = fx.outside.join("rootlink");
        symlink(&fx.root, &link).unwrap();
        let c = Confinement::new(&link).unwrap();
        assert_eq!(c.root(), fx.root.as_path());
        assert_eq!(c.read("a.txt").unwrap().path(), fx.root.join("a.txt"));
        assert_eq!(
            c.write(link.join("src/x.rs")).unwrap().path(),
            fx.root.join("src/x.rs")
        );
        assert!(matches!(
            c.read(link.join(".git/config")),
            Err(ConfineError::GitDir { .. })
        ));
    }

    /// A directory swapped for a link after the check: the write fails and nothing lands in
    /// `.git`.
    #[test]
    fn a_directory_swapped_for_a_link_after_the_check() {
        let fx = fx();
        fs::create_dir(fx.root.join("sub")).unwrap();
        let c = c(&fx);
        let w = c.write("sub/hooks/pre-commit").unwrap();
        let r = {
            fs::write(fx.root.join("sub/f"), "f").unwrap();
            c.read("sub/f").unwrap()
        };
        fs::rename(fx.root.join("sub"), fx.root.join("sub.old")).unwrap();
        symlink(".git", fx.root.join("sub")).unwrap();
        assert!(matches!(
            w.write(b"evil", CreateDirs::Yes),
            Err(ConfineError::Changed { .. })
        ));
        assert!(!fx.root.join(".git/hooks/pre-commit").exists());
        fs::write(fx.root.join(".git/f"), "git").unwrap();
        assert!(matches!(r.read(), Err(ConfineError::Changed { .. })));
    }

    /// A file swapped for a link after the check: the write sees the link and refuses (and the
    /// `renameat` that ends a write would replace a link appearing later instead of following
    /// it); the link's target is untouched.
    #[test]
    fn a_file_swapped_for_a_link_after_the_check() {
        let fx = fx();
        let c = c(&fx);
        let w = c.write("a.txt").unwrap();
        let r = c.read("a.txt").unwrap();
        fs::remove_file(fx.root.join("a.txt")).unwrap();
        symlink(".git/config", fx.root.join("a.txt")).unwrap();
        assert!(matches!(r.read(), Err(ConfineError::Changed { .. })));
        assert!(matches!(
            w.write(b"new\n", CreateDirs::No),
            Err(ConfineError::FinalSymlink { .. })
        ));
        assert_eq!(fs::read(fx.root.join(".git/config")).unwrap(), b"[core]\n");
        assert!(fs::symlink_metadata(fx.root.join("a.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn non_regular_files_are_not_read() {
        let fx = fx();
        let fifo = fx.root.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        let c = c(&fx);
        assert!(c.read("pipe").unwrap().read().is_err());
        assert!(c
            .write("pipe")
            .unwrap()
            .write(b"x", CreateDirs::No)
            .is_err());
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        const SEGMENTS: &[&str] = &[
            "a.txt", "src", "new", "..", ".", ".git", "docs", "cfg", "out", "s2", "dang", "x",
        ];

        fn setup() -> Fx {
            let fx = fx();
            symlink(".git", fx.root.join("docs")).unwrap();
            symlink(".git/config", fx.root.join("cfg")).unwrap();
            symlink(&fx.outside, fx.root.join("out")).unwrap();
            symlink("src", fx.root.join("s2")).unwrap();
            symlink(fx.outside.join("nodir"), fx.root.join("dang")).unwrap();
            fx
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]
            /// Whatever the path, an accepted one resolves inside the root, outside `.git`, and a
            /// write target is never a link; with `CreateDirs::Yes` the write lands exactly there.
            #[test]
            fn accepted_paths_stay_inside(segs in proptest::collection::vec(0..SEGMENTS.len(), 1..6), write in any::<bool>()) {
                let fx = setup();
                let c = Confinement::new(&fx.root).unwrap();
                let rel: PathBuf = segs.iter().map(|i| SEGMENTS[*i]).collect();
                let access = if write { Access::Write } else { Access::Read };
                if let Ok(ok) = c.check(&rel, access) {
                    let p = ok.path();
                    prop_assert!(p.starts_with(&fx.root), "{rel:?} -> {p:?}");
                    prop_assert!(!ok.relative().components().any(|c| is_git_dir_name(c.as_os_str())), "{rel:?} -> {p:?}");
                    if write {
                        prop_assert!(!fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()));
                        if ok.write(b"w", CreateDirs::Yes).is_ok() {
                            prop_assert_eq!(fs::read(&p).unwrap(), b"w".to_vec());
                        }
                    }
                }
                prop_assert_eq!(fs::read(fx.root.join(".git/config")).unwrap(), b"[core]\n".to_vec());
                prop_assert_eq!(fs::read_dir(&fx.outside).unwrap().count(), 1);
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    /// The 8.3 short name of `.git` is `GIT~1` when short names are enabled; either the canonical
    /// spelling or the name check refuses it.
    #[test]
    fn short_names_and_stream_suffixes() {
        let fx = fx();
        let c = c(&fx);
        for p in [
            "GIT~1\\config",
            "git~1/hooks/pre-commit",
            ".git.\\config",
            ".git \\config",
            ".git::$INDEX_ALLOCATION\\config",
            ".GIT\\config",
        ] {
            assert!(c.write(p).is_err(), "{p}");
            assert!(c.read(p).is_err(), "{p}");
        }
        assert!(c.write("src\\new.rs").is_ok());
        assert!(c.write("C:\\Windows\\x").is_err());
    }
}

/// Codex finding (step 8 review): the root's actual git directory may not be named `.git`.
mod git_dirs {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn plain_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap().join("proj");
        fs::create_dir_all(root.join("src")).unwrap();
        (dir, root)
    }

    fn assert_git_dir(c: &Confinement, p: &str) {
        for access in [Access::Read, Access::Write] {
            assert!(
                matches!(c.check(p, access), Err(ConfineError::GitDir { .. })),
                "{p} ({access:?}): {:?}",
                c.check(p, access)
            );
        }
    }

    /// `.git` is a link to `metadata` (git follows it): both spellings are refused.
    #[cfg(unix)]
    #[test]
    fn a_dot_git_link_to_a_directory_inside_the_root() {
        let (_dir, root) = plain_root();
        fs::create_dir_all(root.join("metadata/hooks")).unwrap();
        fs::write(root.join("metadata/config"), "[core]\n").unwrap();
        std::os::unix::fs::symlink("metadata", root.join(".git")).unwrap();
        let c = Confinement::new(&root).unwrap();
        assert_eq!(c.git_dirs(), [root.join("metadata")]);
        for p in [
            ".git/hooks/pre-commit",
            ".git/config",
            ".git/new/x",
            "metadata/hooks/pre-commit",
            "metadata/config",
            "metadata",
            "src/../metadata/config",
        ] {
            assert_git_dir(&c, p);
        }
        assert!(c.write("src/a.rs").is_ok());
        assert!(c.write("metadata2/x").is_ok());
        // The path as written is checked lexically too, in absolute form.
        assert_git_dir(&c, root.join(".git/hooks/x").to_str().unwrap());
        assert!(!root.join("metadata/hooks/pre-commit").exists());
    }

    /// `.git` is a `gitdir:` file pointing inside the root, whose `commondir` points further.
    #[test]
    fn a_dot_git_file_and_its_commondir_inside_the_root() {
        let (_dir, root) = plain_root();
        fs::create_dir_all(root.join("store/main/worktrees/wt")).unwrap();
        fs::create_dir_all(root.join("store/main/hooks")).unwrap();
        fs::write(root.join(".git"), "gitdir: store/main/worktrees/wt\n").unwrap();
        fs::write(root.join("store/main/worktrees/wt/commondir"), "../..\n").unwrap();
        let c = Confinement::new(&root).unwrap();
        assert!(
            c.git_dirs().contains(&root.join("store/main")),
            "{:?}",
            c.git_dirs()
        );
        assert!(c.git_dirs().contains(&root.join("store/main/worktrees/wt")));
        for p in [
            ".git",
            "store/main/hooks/pre-commit",
            "store/main/worktrees/wt/HEAD",
        ] {
            assert_git_dir(&c, p);
        }
        assert!(c.write("store/other.txt").is_ok());
    }

    /// A real linked worktree: opened at the worktree, its git directory and the main
    /// repository's common directory are found; opened at the main repository (which contains
    /// the worktree), the worktree's `.git` file is refused by name.
    #[test]
    fn a_linked_worktree() {
        let (_dir, root) = plain_root();
        if !git(&root, &["init", "-q", "-b", "main"]) {
            return;
        }
        fs::write(root.join("a.txt"), "a\n").unwrap();
        assert!(git(&root, &["add", "a.txt"]));
        assert!(git(&root, &["commit", "-q", "-m", "init"]));
        assert!(git(&root, &["worktree", "add", "-q", "wt", "-b", "side"]));
        let wt = Confinement::new(&root.join("wt")).unwrap();
        // Both lie outside the worktree's root: nothing to forbid, and the name rule still holds.
        assert!(wt.git_dirs().is_empty(), "{:?}", wt.git_dirs());
        assert_git_dir(&wt, ".git");
        let main = Confinement::new(&root).unwrap();
        assert_eq!(main.git_dirs(), [root.join(".git")]);
        assert_git_dir(&main, "wt/.git");
        assert_git_dir(&main, ".git/worktrees/wt/HEAD");
        assert!(main.write("wt/a.txt").is_ok());
    }

    /// A submodule: its `.git` file and the superproject's `modules/<name>` are refused, and a
    /// confinement opened at the submodule finds its git directory in the superproject.
    #[test]
    fn a_submodule() {
        let (dir, root) = plain_root();
        let upstream = dunce::canonicalize(dir.path()).unwrap().join("up");
        fs::create_dir_all(&upstream).unwrap();
        if !git(&upstream, &["init", "-q", "-b", "main"]) {
            return;
        }
        fs::write(upstream.join("u.txt"), "u\n").unwrap();
        assert!(git(&upstream, &["add", "u.txt"]));
        assert!(git(&upstream, &["commit", "-q", "-m", "u"]));
        assert!(git(&root, &["init", "-q", "-b", "main"]));
        let url = upstream.to_string_lossy().into_owned();
        assert!(git(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &url,
                "sub"
            ]
        ));
        let c = Confinement::new(&root).unwrap();
        for p in [
            "sub/.git",
            ".git/modules/sub/config",
            ".git/modules/sub/hooks/x",
        ] {
            assert_git_dir(&c, p);
        }
        assert!(c.write("sub/u.txt").is_ok());
        let inner = Confinement::new(&root.join("sub")).unwrap();
        assert_git_dir(&inner, ".git");
        assert!(inner.git_dirs().is_empty(), "outside the submodule's root");
    }

    /// The superproject's `.git` is a link and the submodule lives under it: the module
    /// directory is inside the forbidden git directory.
    #[cfg(unix)]
    #[test]
    fn modules_under_a_linked_git_directory() {
        let (_dir, root) = plain_root();
        fs::create_dir_all(root.join("meta/modules/sub/hooks")).unwrap();
        std::os::unix::fs::symlink("meta", root.join(".git")).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/.git"), "gitdir: ../meta/modules/sub\n").unwrap();
        let c = Confinement::new(&root).unwrap();
        assert_git_dir(&c, "meta/modules/sub/hooks/pre-commit");
        assert_git_dir(&c, "sub/.git");
    }
}
