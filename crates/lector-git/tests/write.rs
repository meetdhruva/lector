//! `lector_git::write`: staging, unstaging, discarding, applying patches and
//! committing through the git CLI, against fixture repositories built with git.

use std::path::Path;
use std::process::Command;
use std::sync::Once;

use lector_git::{ChangeKind, CommitOptions, Error, PatchTarget, Repo};

/// Hermetic git for this test binary: Lector's own git calls inherit the process
/// environment, so the machine's global/system config (signing, hooks paths,
/// identity) must not leak in.
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
        std::env::set_var("GIT_CONFIG_KEY_2", "commit.gpgsign");
        std::env::set_var("GIT_CONFIG_VALUE_2", "false");
        for var in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "EMAIL",
        ] {
            std::env::remove_var(var);
        }
    });
}

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    /// A repository with an identity configured locally.
    fn new() -> Fx {
        hermetic();
        let fx = Fx {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        fx.git(&["init", "-q", "-b", "main"]);
        fx.git(&["config", "user.name", "Ada Lovelace"]);
        fx.git(&["config", "user.email", "ada@example.com"]);
        fx.git(&["config", "user.useConfigOnly", "true"]);
        fx
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn repo(&self) -> Repo {
        Repo::discover(self.path()).expect("discover")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.path().join(rel)).ok()
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    /// `git status --porcelain` (sorted lines).
    fn status(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .git(&["status", "--porcelain", "--untracked-files=all"])
            .lines()
            .map(|l| l.replace('"', ""))
            .collect();
        lines.sort();
        lines
    }
}

#[test]
fn stage_and_unstage_paths_including_deletions_and_untracked() {
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    fx.write("b.txt", "b\n");
    fx.commit_all("init");
    fx.write("a.txt", "a2\n");
    std::fs::remove_file(fx.path().join("b.txt")).unwrap();
    fx.write("new dir/c [1].txt", "c\n");
    let repo = fx.repo();
    repo.stage_paths(&["a.txt", "b.txt", "new dir/c [1].txt"])
        .unwrap();
    assert_eq!(
        fx.status(),
        ["A  new dir/c [1].txt", "D  b.txt", "M  a.txt"]
    );
    let staged = repo.changes(&lector_git::Changeset::Staged).unwrap();
    assert_eq!(staged.len(), 3);
    repo.unstage_paths(&["b.txt", "new dir/c [1].txt"]).unwrap();
    assert_eq!(
        fx.status(),
        [" D b.txt", "?? new dir/c [1].txt", "M  a.txt"]
    );
    repo.unstage_all().unwrap();
    repo.stage_all().unwrap();
    assert_eq!(fx.status().len(), 3);
    assert!(fx
        .status()
        .iter()
        .all(|l| !l.starts_with(' ') && !l.starts_with('?')));
    repo.unstage_all().unwrap();
    assert_eq!(
        fx.status(),
        [" D b.txt", " M a.txt", "?? new dir/c [1].txt"]
    );
}

#[test]
fn literal_pathspecs_never_glob() {
    // `[x].txt` is a valid file name everywhere (unlike `*`) and, as a glob, matches
    // `x.txt`: a literal pathspec must only ever touch the bracketed file.
    let fx = Fx::new();
    fx.write("x.txt", "x\n");
    fx.commit_all("init");
    fx.write("[x].txt", "bracket\n");
    fx.write("x.txt", "changed\n");
    fx.repo().stage_paths(&["[x].txt"]).unwrap();
    assert_eq!(fx.status(), [" M x.txt", "A  [x].txt"]);
    fx.repo().unstage_paths(&["[x].txt"]).unwrap();
    assert_eq!(fx.status(), [" M x.txt", "?? [x].txt"]);
    fx.repo().discard_paths(&["[x].txt"]).unwrap();
    assert!(fx.read("[x].txt").is_none());
    assert_eq!(fx.read("x.txt").as_deref(), Some("changed\n"));
}

#[test]
fn unborn_head_staging() {
    let fx = Fx::new();
    fx.write("first.txt", "1\n");
    fx.write("second.txt", "2\n");
    let repo = fx.repo();
    repo.stage_paths(&["first.txt", "second.txt"]).unwrap();
    assert_eq!(fx.status(), ["A  first.txt", "A  second.txt"]);
    repo.unstage_paths(&["first.txt"]).unwrap();
    assert_eq!(fx.status(), ["?? first.txt", "A  second.txt"]);
    repo.unstage_all().unwrap();
    assert_eq!(fx.status(), ["?? first.txt", "?? second.txt"]);
    assert_eq!(repo.last_commit_message().unwrap(), None);
    repo.stage_all().unwrap();
    let oid = repo
        .create_commit(&CommitOptions {
            message: "Root commit".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(fx.git(&["rev-parse", "HEAD"]).trim(), oid.to_string());
}

#[test]
fn discard_restores_tracked_and_deletes_untracked() {
    let fx = Fx::new();
    fx.write("keep.txt", "original\n");
    fx.write("gone.txt", "bring me back\n");
    fx.commit_all("init");
    fx.write("keep.txt", "edited\n");
    std::fs::remove_file(fx.path().join("gone.txt")).unwrap();
    fx.write("scratch/tmp.log", "junk\n");
    fx.write("ignored.log", "keep me\n");
    fx.write(".gitignore", "ignored.log\n");
    let repo = fx.repo();
    repo.discard_paths(&["keep.txt", "gone.txt", "scratch/tmp.log"])
        .unwrap();
    assert_eq!(fx.read("keep.txt").as_deref(), Some("original\n"));
    assert_eq!(fx.read("gone.txt").as_deref(), Some("bring me back\n"));
    assert!(fx.read("scratch/tmp.log").is_none());
    assert_eq!(fx.read("ignored.log").as_deref(), Some("keep me\n"));
    assert_eq!(fx.status(), ["?? .gitignore"]);
    // Staged content survives a discard of the working-tree changes.
    fx.write("keep.txt", "staged\n");
    fx.git(&["add", "keep.txt"]);
    fx.write("keep.txt", "staged + more\n");
    repo.discard_paths(&["keep.txt"]).unwrap();
    assert_eq!(fx.read("keep.txt").as_deref(), Some("staged\n"));
    assert!(repo.discard_paths(&["../outside"]).is_err());
}

#[test]
fn apply_patch_to_index_and_worktree() {
    let fx = Fx::new();
    fx.write("f.txt", "one\ntwo\nthree\n");
    fx.commit_all("init");
    fx.write("f.txt", "one\nTWO\nthree\n");
    let patch = "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
    let repo = fx.repo();
    repo.apply_patch(patch, PatchTarget::Index, false).unwrap();
    assert_eq!(fx.status(), ["M  f.txt"]);
    repo.apply_patch(patch, PatchTarget::Index, true).unwrap();
    assert_eq!(fx.status(), [" M f.txt"]);
    repo.apply_patch(patch, PatchTarget::Worktree, true)
        .unwrap();
    assert_eq!(fx.read("f.txt").as_deref(), Some("one\ntwo\nthree\n"));
    // A stale patch fails with git's message instead of corrupting anything.
    match repo.apply_patch(patch, PatchTarget::Worktree, true) {
        Err(Error::GitCommand { stderr, .. }) => {
            assert!(stderr.contains("patch does not apply"), "{stderr}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn commit_amend_sign_off_and_messages() {
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    let repo = fx.repo();
    repo.stage_all().unwrap();
    let first = repo
        .create_commit(&CommitOptions {
            message: "Add a\n\nWith a body line.\n".into(),
            amend: false,
            sign_off: true,
        })
        .unwrap();
    let msg = repo.last_commit_message().unwrap().unwrap();
    assert!(msg.starts_with("Add a\n\nWith a body line.\n"), "{msg:?}");
    assert!(msg.contains("Signed-off-by: Ada Lovelace <ada@example.com>"));
    let author = fx.git(&["log", "-1", "--format=%an <%ae>"]);
    assert_eq!(author.trim(), "Ada Lovelace <ada@example.com>");

    // Nothing staged: git's own explanation comes back.
    match repo.create_commit(&CommitOptions {
        message: "empty".into(),
        ..Default::default()
    }) {
        Err(Error::GitCommand { stderr, .. }) => {
            assert!(stderr.contains("nothing to commit"), "{stderr}")
        }
        other => panic!("{other:?}"),
    }

    // Amend rewrites the tip.
    let amended = repo
        .create_commit(&CommitOptions {
            message: "Add a (reworded)".into(),
            amend: true,
            sign_off: false,
        })
        .unwrap();
    assert_ne!(amended, first);
    assert_eq!(fx.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
    assert_eq!(
        repo.last_commit_message()
            .unwrap()
            .as_deref()
            .map(str::trim),
        Some("Add a (reworded)")
    );
}

#[cfg(unix)]
#[test]
fn hooks_run_and_failures_surface_verbatim() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    let hook = fx.path().join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'lint failed: trailing whitespace in a.txt' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = fx.repo();
    repo.stage_all().unwrap();
    let err = repo
        .create_commit(&CommitOptions {
            message: "blocked".into(),
            ..Default::default()
        })
        .unwrap_err();
    match err {
        Error::GitCommand { stderr, .. } => {
            assert!(
                stderr.contains("lint failed: trailing whitespace"),
                "{stderr}"
            )
        }
        other => panic!("{other:?}"),
    }
    // A commit-msg hook can rewrite the message; it still runs (no --no-verify).
    std::fs::remove_file(&hook).unwrap();
    let msg_hook = fx.path().join(".git/hooks/commit-msg");
    std::fs::write(&msg_hook, "#!/bin/sh\necho 'Hook-Ran: yes' >> \"$1\"\n").unwrap();
    std::fs::set_permissions(&msg_hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.create_commit(&CommitOptions {
        message: "allowed".into(),
        ..Default::default()
    })
    .unwrap();
    assert!(repo
        .last_commit_message()
        .unwrap()
        .unwrap()
        .contains("Hook-Ran: yes"));
}

#[test]
fn identity_missing_and_present() {
    let fx = Fx::new();
    let repo = fx.repo();
    assert_eq!(
        repo.identity().unwrap(),
        Some(("Ada Lovelace".into(), "ada@example.com".into()))
    );
    fx.git(&["config", "--unset", "user.name"]);
    fx.git(&["config", "--unset", "user.email"]);
    assert_eq!(repo.identity().unwrap(), None);
    fx.write("a.txt", "a\n");
    repo.stage_all().unwrap();
    match repo.create_commit(&CommitOptions {
        message: "who am I".into(),
        ..Default::default()
    }) {
        Err(Error::GitCommand { stderr, .. }) => {
            assert!(stderr.contains("Please tell me who you are"), "{stderr}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn index_lock_is_reported() {
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    let lock = fx.path().join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    match fx.repo().stage_paths(&["a.txt"]) {
        Err(Error::GitCommand { stderr, .. }) => {
            assert!(stderr.contains("index.lock"), "{stderr}")
        }
        other => panic!("{other:?}"),
    }
    std::fs::remove_file(lock).unwrap();
    fx.repo().stage_paths(&["a.txt"]).unwrap();
}

#[test]
fn staged_rename_unstages_both_sides() {
    let fx = Fx::new();
    fx.write("old.txt", "a\nb\nc\nd\n");
    fx.commit_all("init");
    fx.git(&["mv", "old.txt", "new.txt"]);
    let repo = fx.repo();
    let staged = repo.changes(&lector_git::Changeset::Staged).unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].kind, ChangeKind::Renamed);
    let paths: Vec<&str> = [staged[0].old_path.as_deref(), staged[0].new_path.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    repo.unstage_paths(&paths).unwrap();
    assert_eq!(fx.status(), [" D old.txt", "?? new.txt"]);
}
