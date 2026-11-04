//! Conflict state (`Repo::{operation, conflicts, conflict_stage, merge_message}`)
//! and continuing / aborting operations, on fixture repositories with real
//! conflicts made by the git CLI. Hermetic: no global/system config.

use std::path::Path;
use std::process::Command;
use std::sync::Once;

use lector_git::{ConflictKind, ConflictStage, OperationKind, Repo};

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
        for var in ["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "EMAIL"] {
            std::env::remove_var(var);
        }
    });
}

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    fn new() -> Fx {
        hermetic();
        let fx = Fx {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        fx.git(&["init", "-q", "-b", "main"]);
        fx.git(&["config", "user.name", "Ada Lovelace"]);
        fx.git(&["config", "user.email", "ada@example.com"]);
        fx.git(&["config", "core.autocrlf", "false"]);
        fx
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn repo(&self) -> Repo {
        Repo::discover(self.path()).expect("discover")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(self.path())
            .env("GIT_EDITOR", "true")
            .output()
            .expect("git")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Runs git expecting failure (a conflict).
    fn git_fails(&self, args: &[&str]) {
        let out = self.run(args);
        assert!(!out.status.success(), "git {args:?} unexpectedly succeeded");
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path().join(rel)).unwrap()
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    /// main and feature both change `a.txt` line 2; feature also adds `new.txt`
    /// (main adds a different `new.txt`) and deletes `gone.txt` (main modifies it).
    fn conflicting_branches() -> Fx {
        let fx = Fx::new();
        fx.write("a.txt", "one\ntwo\nthree\n");
        fx.write("gone.txt", "keep me\n");
        fx.commit_all("base");
        fx.git(&["checkout", "-q", "-b", "feature"]);
        fx.write("a.txt", "one\nTWO (feature)\nthree\n");
        fx.write("new.txt", "feature version\n");
        std::fs::remove_file(fx.path().join("gone.txt")).unwrap();
        fx.commit_all("feature work");
        fx.git(&["checkout", "-q", "main"]);
        fx.write("a.txt", "one\nTwo (main)\nthree\n");
        fx.write("new.txt", "main version\n");
        fx.write("gone.txt", "keep me, edited\n");
        fx.commit_all("main work");
        fx
    }
}

#[test]
fn no_operation_in_a_clean_repo() {
    let fx = Fx::new();
    fx.write("a.txt", "a\n");
    fx.commit_all("init");
    let repo = fx.repo();
    assert_eq!(repo.operation().unwrap(), None);
    assert!(repo.conflicts().unwrap().is_empty());
    assert_eq!(repo.merge_message().unwrap(), None);
}

#[test]
fn merge_conflicts_with_stages() {
    let fx = Fx::conflicting_branches();
    fx.git_fails(&["merge", "feature"]);
    let repo = fx.repo();
    let op = repo.operation().unwrap().expect("merge in progress");
    assert_eq!(op.kind, OperationKind::Merge);
    assert_eq!(op.head_name.as_deref(), Some("main"));
    assert_eq!(op.incoming.as_deref(), Some("feature"));
    assert_eq!(op.incoming_oid, Some(repo.resolve("feature").unwrap()));
    assert_eq!(op.describe(), "Merging feature into main");
    let msg = repo.merge_message().unwrap().expect("MERGE_MSG");
    assert!(msg.starts_with("Merge branch 'feature'"), "{msg}");
    assert!(!msg.contains('#'), "comments stripped: {msg}");

    let conflicts = repo.conflicts().unwrap();
    let kinds: Vec<(&str, ConflictKind)> = conflicts
        .iter()
        .map(|c| (c.path.as_str(), c.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("a.txt", ConflictKind::BothModified),
            ("gone.txt", ConflictKind::DeletedByThem),
            ("new.txt", ConflictKind::BothAdded),
        ]
    );
    let a = &conflicts[0];
    assert!(a.base.is_some() && a.ours.is_some() && a.theirs.is_some());
    assert_eq!(
        repo.conflict_stage("a.txt", ConflictStage::Base).unwrap(),
        Some(b"one\ntwo\nthree\n".to_vec())
    );
    assert_eq!(
        repo.conflict_stage("a.txt", ConflictStage::Ours).unwrap(),
        Some(b"one\nTwo (main)\nthree\n".to_vec())
    );
    assert_eq!(
        repo.conflict_stage("a.txt", ConflictStage::Theirs).unwrap(),
        Some(b"one\nTWO (feature)\nthree\n".to_vec())
    );
    assert_eq!(
        repo.conflict_stage("new.txt", ConflictStage::Base).unwrap(),
        None
    );
    assert_eq!(
        repo.conflict_stage("gone.txt", ConflictStage::Theirs)
            .unwrap(),
        None
    );
    assert_eq!(
        repo.conflict_stage("not-there.txt", ConflictStage::Ours)
            .unwrap(),
        None
    );
    // Status agrees.
    let status = repo.status().unwrap();
    assert!(status.iter().filter(|e| e.conflicted).count() == 3);
    assert!(fx.read("a.txt").contains("<<<<<<< HEAD"));
}

#[test]
fn continue_fails_while_conflicted_then_commits() {
    let fx = Fx::conflicting_branches();
    fx.git_fails(&["merge", "feature"]);
    let repo = fx.repo();
    let err = repo.continue_operation(OperationKind::Merge).unwrap_err();
    assert!(matches!(err, lector_git::Error::GitCommand { .. }), "{err}");
    fx.write("a.txt", "one\nTwo (main) + TWO (feature)\nthree\n");
    fx.write("new.txt", "both\n");
    repo.stage_paths(&["a.txt", "new.txt", "gone.txt"]).unwrap();
    assert!(repo.conflicts().unwrap().is_empty());
    repo.continue_operation(OperationKind::Merge).unwrap();
    assert_eq!(repo.operation().unwrap(), None);
    let head = repo.commit(repo.resolve("HEAD").unwrap()).unwrap();
    assert_eq!(head.info.parents.len(), 2);
    assert!(head.info.summary.starts_with("Merge branch 'feature'"));
}

#[test]
fn abort_merge_restores_head() {
    let fx = Fx::conflicting_branches();
    let before = fx.git(&["rev-parse", "HEAD"]);
    fx.git_fails(&["merge", "feature"]);
    let repo = fx.repo();
    repo.abort_operation(OperationKind::Merge).unwrap();
    assert_eq!(repo.operation().unwrap(), None);
    assert!(repo.conflicts().unwrap().is_empty());
    assert_eq!(fx.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(fx.read("a.txt"), "one\nTwo (main)\nthree\n");
    assert!(repo.skip_operation_step(OperationKind::Merge).is_err());
}

#[test]
fn rebase_state_with_progress_continue_and_abort() {
    let fx = Fx::new();
    fx.write("a.txt", "1\n2\n3\n");
    fx.commit_all("base");
    fx.git(&["checkout", "-q", "-b", "topic"]);
    fx.write("b.txt", "b\n");
    fx.commit_all("topic 1");
    fx.write("a.txt", "1\n2 topic\n3\n");
    fx.commit_all("topic 2");
    fx.write("c.txt", "c\n");
    fx.commit_all("topic 3");
    fx.git(&["checkout", "-q", "main"]);
    fx.write("a.txt", "1\n2 main\n3\n");
    fx.commit_all("main change");
    fx.git(&["checkout", "-q", "topic"]);
    fx.git_fails(&["rebase", "main"]);
    let repo = fx.repo();
    let op = repo.operation().unwrap().expect("rebase in progress");
    assert_eq!(op.kind, OperationKind::Rebase);
    assert_eq!(op.head_name.as_deref(), Some("topic"));
    assert_eq!(op.incoming.as_deref(), Some("main"));
    assert_eq!(op.step, Some((2, 3)));
    // (modern git's merge backend marks plain rebases interactive too)
    assert_eq!(op.describe(), "Rebasing topic onto main (2/3)");
    assert_eq!(repo.conflicts().unwrap()[0].path, "a.txt");

    // Resolve and continue: never waits for an editor.
    fx.write("a.txt", "1\n2 main+topic\n3\n");
    repo.stage_paths(&["a.txt"]).unwrap();
    repo.continue_operation(OperationKind::Rebase).unwrap();
    assert_eq!(repo.operation().unwrap(), None);
    assert_eq!(fx.read("a.txt"), "1\n2 main+topic\n3\n");
    let log = fx.git(&["log", "--format=%s"]);
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        ["topic 3", "topic 2", "topic 1", "main change", "base"]
    );

    // Abort restores the original branch.
    fx.git(&["checkout", "-q", "-b", "topic2", "HEAD~4"]);
    fx.write("a.txt", "1\n2 topic2\n3\n");
    fx.commit_all("topic2");
    let before = fx.git(&["rev-parse", "HEAD"]);
    fx.git_fails(&["rebase", "main"]);
    assert_eq!(
        repo.operation().unwrap().map(|o| o.kind),
        Some(OperationKind::Rebase)
    );
    repo.abort_operation(OperationKind::Rebase).unwrap();
    assert_eq!(repo.operation().unwrap(), None);
    assert_eq!(fx.git(&["rev-parse", "HEAD"]), before);
}

#[test]
fn interactive_rebase_is_flagged() {
    let fx = Fx::new();
    fx.write("a.txt", "1\n");
    fx.commit_all("base");
    fx.git(&["checkout", "-q", "-b", "topic"]);
    fx.write("a.txt", "topic\n");
    fx.commit_all("topic");
    fx.git(&["checkout", "-q", "main"]);
    fx.write("a.txt", "main\n");
    fx.commit_all("main");
    fx.git(&["checkout", "-q", "topic"]);
    // GIT_SEQUENCE_EDITOR=true keeps the todo list as is.
    let out = Command::new("git")
        .args(["rebase", "-i", "main"])
        .current_dir(fx.path())
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_EDITOR", "true")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let op = fx.repo().operation().unwrap().unwrap();
    assert_eq!(op.kind, OperationKind::Rebase);
    assert!(op.interactive);
    assert_eq!(op.step, Some((1, 1)));
    fx.repo().abort_operation(OperationKind::Rebase).unwrap();
}

#[test]
fn cherry_pick_and_revert_states() {
    let fx = Fx::new();
    fx.write("a.txt", "1\n");
    fx.commit_all("base");
    fx.git(&["checkout", "-q", "-b", "side"]);
    fx.write("a.txt", "side\n");
    fx.commit_all("Side change");
    let side = fx.git(&["rev-parse", "HEAD"]).trim().to_string();
    fx.git(&["checkout", "-q", "main"]);
    fx.write("a.txt", "main\n");
    fx.commit_all("Main change");
    fx.git_fails(&["cherry-pick", "side"]);
    let repo = fx.repo();
    let op = repo.operation().unwrap().unwrap();
    assert_eq!(op.kind, OperationKind::CherryPick);
    assert_eq!(op.incoming_oid.map(|o| o.to_string()), Some(side.clone()));
    assert_eq!(
        op.incoming.as_deref(),
        Some(format!("{} Side change", &side[..7]).as_str())
    );
    assert_eq!(op.head_name.as_deref(), Some("main"));
    // Continue after resolving (message kept, no editor).
    fx.write("a.txt", "main+side\n");
    repo.stage_paths(&["a.txt"]).unwrap();
    repo.continue_operation(OperationKind::CherryPick).unwrap();
    assert_eq!(repo.operation().unwrap(), None);
    assert_eq!(fx.git(&["log", "-1", "--format=%s"]).trim(), "Side change");

    // Revert of an older commit conflicts with a later edit.
    fx.write("a.txt", "later\n");
    fx.commit_all("Later");
    fx.git_fails(&["revert", "--no-edit", "HEAD~1"]);
    let op = repo.operation().unwrap().unwrap();
    assert_eq!(op.kind, OperationKind::Revert);
    repo.abort_operation(OperationKind::Revert).unwrap();
    assert_eq!(repo.operation().unwrap(), None);
    assert_eq!(fx.read("a.txt"), "later\n");
}
