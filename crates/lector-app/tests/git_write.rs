//! Git write commands + activity journal, end to end on fixture repositories:
//! every destructive command backs up first and is undoable, the journal records
//! everything, and undo refuses (typed) when files changed since.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use lector_app::commands::git_write::BackupUnavailable;
use lector_app::commands::{CommandContext, CommandError, Source};
use lector_app::journal::{Journal, UndoError, UndoMode, UndoRecord, UndoStatus};
use lector_app::{AppCore, CoreOptions, Outcome};
use lector_config::Settings;
use lector_workspace::Project;
use serde_json::{json, Value};

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "4");
        std::env::set_var("GIT_CONFIG_KEY_0", "gc.auto");
        std::env::set_var("GIT_CONFIG_VALUE_0", "0");
        std::env::set_var("GIT_CONFIG_KEY_1", "maintenance.auto");
        std::env::set_var("GIT_CONFIG_VALUE_1", "false");
        std::env::set_var("GIT_CONFIG_KEY_2", "commit.gpgsign");
        std::env::set_var("GIT_CONFIG_VALUE_2", "false");
        std::env::set_var("GIT_CONFIG_KEY_3", "gc.autoDetach");
        std::env::set_var("GIT_CONFIG_VALUE_3", "false");
    });
}

struct Fx {
    work: tempfile::TempDir,
    data: tempfile::TempDir,
    cx: CommandContext,
    journal: Journal,
}

impl Fx {
    fn new() -> Fx {
        hermetic();
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(work.path()).unwrap();
        let fx_git = |args: &[&str]| git_in(&root, args);
        fx_git(&["init", "-q", "-b", "main"]);
        fx_git(&["config", "user.name", "t"]);
        fx_git(&["config", "user.email", "t@t"]);
        let core = AppCore::open(CoreOptions::for_tests());
        let journal = Journal::install_in(&core, data.path().to_path_buf());
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        // Written below; Project::open needs the directory only.
        let project = Project::open(&root, settings).unwrap();
        let cx = CommandContext::new(core, Some(project), Source::Internal);
        Fx {
            work,
            data,
            cx,
            journal,
        }
    }

    fn root(&self) -> PathBuf {
        fs::canonicalize(self.work.path()).unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(&self.root(), args)
    }

    fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
        let p = self.root().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.root().join(rel)).unwrap()
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    fn run(&self, id: &str, args: Value) -> Result<Outcome, CommandError> {
        self.cx.core.registry().invoke(id, args, &self.cx)
    }

    fn ok(&self, id: &str, args: Value) -> Outcome {
        self.run(id, args).unwrap_or_else(|e| panic!("{id}: {e}"))
    }

    fn undo(&self, outcome: &Outcome) -> Result<lector_app::journal::ActivityEntry, UndoError> {
        self.journal.undo(
            outcome.journal_id.expect("journaled"),
            &self.cx,
            UndoMode::Check,
        )
    }

    fn index(&self) -> String {
        self.git(&["ls-files", "-s"])
    }
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn base() -> Fx {
    let fx = Fx::new();
    fx.write("parser.rs", "fn one() {}\n\nfn two() {}\n\nfn three() {}\n");
    fx.write("crlf.txt", "a\r\nb\r\n");
    fx.write("keep.txt", "keep\n");
    fx.commit_all("init");
    fx
}

#[test]
fn stage_and_unstage_undo_by_inverse_command() {
    let fx = base();
    fx.write("parser.rs", "changed\n");
    let staged = fx.ok("git.stage", json!({"paths": ["parser.rs"]}));
    assert_eq!(staged.summary, "Staged parser.rs");
    assert_eq!(fx.git(&["diff", "--cached", "--name-only"]), "parser.rs\n");
    assert!(
        matches!(staged.undo, Some(UndoRecord::Invoke { ref command, .. }) if command == "git.unstage")
    );

    let undo_entry = fx.undo(&staged).unwrap();
    assert_eq!(fx.git(&["diff", "--cached", "--name-only"]), "");
    assert_eq!(fx.read("parser.rs"), b"changed\n");
    // The original is marked undone; the inverse's own entry links back.
    let original = fx.journal.get(staged.journal_id.unwrap()).unwrap();
    assert!(original.undone);
    assert_eq!(original.undone_by, Some(undo_entry.id));
    assert_eq!(undo_entry.undo_of, staged.journal_id);
    // Already undone: refused with a clear error.
    assert!(matches!(fx.undo(&staged), Err(UndoError::AlreadyUndone(_))));
    // The undo is itself undoable (redo) and re-activates the original.
    fx.journal
        .undo(undo_entry.id, &fx.cx, UndoMode::Check)
        .unwrap();
    assert_eq!(fx.git(&["diff", "--cached", "--name-only"]), "parser.rs\n");
    assert!(!fx.journal.get(staged.journal_id.unwrap()).unwrap().undone);
}

#[test]
fn stage_of_a_partially_staged_file_restores_the_exact_old_index() {
    let fx = base();
    fx.write(
        "parser.rs",
        "fn one() { 1 }\n\nfn two() {}\n\nfn three() {}\n",
    );
    fx.git(&["add", "parser.rs"]);
    // More unstaged edits on top.
    fx.write(
        "parser.rs",
        "fn one() { 1 }\n\nfn two() { 2 }\n\nfn three() {}\n",
    );
    let before = fx.index();
    let out = fx.ok("git.stage", json!({"paths": ["parser.rs"]}));
    assert_ne!(fx.index(), before);
    assert!(
        matches!(out.undo, Some(UndoRecord::RestoreIndex { .. })),
        "{:?}",
        out.undo
    );
    fx.undo(&out).unwrap();
    assert_eq!(
        fx.index(),
        before,
        "index restored exactly, not just unstaged"
    );
    assert_eq!(
        fx.read("parser.rs"),
        b"fn one() { 1 }\n\nfn two() { 2 }\n\nfn three() {}\n"
    );

    // Unstage with unstaged edits on top: restoring must bring the *staged* version back.
    let before = fx.index();
    let out = fx.ok("git.unstage", json!({"paths": ["parser.rs"]}));
    assert!(matches!(out.undo, Some(UndoRecord::RestoreIndex { .. })));
    fx.undo(&out).unwrap();
    assert_eq!(fx.index(), before);
}

#[test]
fn stage_all_and_unstage_all_are_undoable() {
    let fx = base();
    fx.write("parser.rs", "x\n");
    fx.write("new.txt", "new\n");
    fx.git(&["add", "keep.txt"]); // no-op: nothing staged yet
    let before = fx.index();
    let out = fx.ok("git.stage_all", json!({}));
    assert!(out.summary.contains("2 files"), "{}", out.summary);
    assert!(fx
        .git(&["status", "--porcelain"])
        .lines()
        .all(|l| l.starts_with('A') || l.starts_with('M')));
    fx.undo(&out).unwrap();
    assert_eq!(fx.index(), before);
    let untracked = fx.git(&["status", "--porcelain"]);
    assert!(untracked.contains("?? new.txt"), "{untracked}");

    fx.git(&["add", "-A"]);
    let staged = fx.index();
    let out = fx.ok("git.unstage_all", json!({}));
    assert_eq!(fx.git(&["diff", "--cached", "--name-only"]), "");
    fx.undo(&out).unwrap();
    assert_eq!(fx.index(), staged);
}

#[test]
fn discard_backs_up_first_and_undo_restores_exact_bytes() {
    let fx = base();
    fx.write("parser.rs", "my precious\r\nedits\r\nwithout newline");
    fx.write("untracked.bin", [0u8, 1, 2, 255, 13, 10, 0]);
    let out = fx.ok(
        "git.discard",
        json!({"paths": ["parser.rs", "untracked.bin"]}),
    );
    assert!(
        out.summary.starts_with("Discarded changes in 2 files"),
        "{}",
        out.summary
    );
    assert_eq!(
        fx.read("parser.rs"),
        b"fn one() {}\n\nfn two() {}\n\nfn three() {}\n"
    );
    assert!(!fx.root().join("untracked.bin").exists());
    let id = out.journal_id.unwrap();
    let entry = fx.journal.get(id).unwrap();
    assert!(matches!(entry.undo, Some(UndoRecord::RestoreFiles { .. })));
    assert_eq!(entry.command, "git.discard");
    assert_eq!(fx.journal.undo_status(&entry), UndoStatus::Available);

    let undo_entry = fx.undo(&out).unwrap();
    assert_eq!(
        fx.read("parser.rs"),
        b"my precious\r\nedits\r\nwithout newline"
    );
    assert_eq!(fx.read("untracked.bin"), [0u8, 1, 2, 255, 13, 10, 0]);
    assert_eq!(undo_entry.command, "journal.undo");
    assert!(undo_entry.summary.starts_with("Undid: Discarded"));
    assert_eq!(
        fx.journal.undo_status(&fx.journal.get(id).unwrap()),
        UndoStatus::Unavailable("Already undone".into())
    );

    // Redo: the undo's own record discards again.
    fx.journal
        .undo(undo_entry.id, &fx.cx, UndoMode::Check)
        .unwrap();
    assert_eq!(
        fx.read("parser.rs"),
        b"fn one() {}\n\nfn two() {}\n\nfn three() {}\n"
    );
    assert!(!fx.root().join("untracked.bin").exists());
}

#[test]
fn undo_refuses_when_files_changed_since_and_force_backs_up_first() {
    let fx = base();
    fx.write("parser.rs", "lost edits\n");
    let out = fx.ok("git.discard", json!({"paths": ["parser.rs"]}));
    // The user keeps working on the discarded file.
    fx.write("parser.rs", "new work after the discard\n");
    let err = fx.undo(&out).unwrap_err();
    match &err {
        UndoError::Changed { paths, .. } => assert_eq!(paths, &["parser.rs".to_owned()]),
        other => panic!("{other:?}"),
    }
    assert!(err.to_string().contains("parser.rs changed since"), "{err}");
    assert_eq!(
        fx.read("parser.rs"),
        b"new work after the discard\n",
        "nothing written"
    );
    assert!(!fx.journal.get(out.journal_id.unwrap()).unwrap().undone);

    // "Restore anyway".
    let forced = fx
        .journal
        .undo(out.journal_id.unwrap(), &fx.cx, UndoMode::Force)
        .unwrap();
    assert_eq!(fx.read("parser.rs"), b"lost edits\n");
    // ... and the work it overwrote is recoverable (redo = restore the backup).
    fx.journal.undo(forced.id, &fx.cx, UndoMode::Force).unwrap();
    assert_eq!(fx.read("parser.rs"), b"new work after the discard\n");
}

#[test]
fn discard_all_scopes_and_describes() {
    let fx = base();
    fx.write("parser.rs", "x\n");
    fx.write("keep.txt", "y\n");
    fx.write("new.txt", "n\n");
    let out = fx.ok("git.discard_all", json!({}));
    assert!(out.summary.contains("2 files"), "{}", out.summary);
    assert!(
        fx.root().join("new.txt").exists(),
        "untracked survives the default scope"
    );
    fx.undo(&out).unwrap();
    assert_eq!(fx.read("parser.rs"), b"x\n");
    let out = fx.ok("git.discard_all", json!({"scope": "untracked"}));
    assert_eq!(out.summary, "Deleted untracked new.txt");
    assert!(!fx.root().join("new.txt").exists());
    fx.undo(&out).unwrap();
    assert_eq!(fx.read("new.txt"), b"n\n");
    let out = fx.ok("git.discard_all", json!({"scope": "all"}));
    assert!(
        out.summary.contains("deleted 1 untracked"),
        "{}",
        out.summary
    );
}

#[test]
fn discard_hunk_by_index_and_undo() {
    let fx = base();
    fx.write(
        "parser.rs",
        "fn one() { 1 }\n\nfn two() {}\n\nfn three() { 3 }\n",
    );
    let out = fx.ok("git.discard_hunk", json!({"path": "parser.rs", "hunk": 0}));
    assert_eq!(out.summary, "Discarded hunk in parser.rs");
    assert_eq!(
        fx.read("parser.rs"),
        b"fn one() {}\n\nfn two() {}\n\nfn three() { 3 }\n"
    );
    fx.undo(&out).unwrap();
    assert_eq!(
        fx.read("parser.rs"),
        b"fn one() { 1 }\n\nfn two() {}\n\nfn three() { 3 }\n"
    );
}

#[test]
fn stage_hunk_and_selected_lines_undo_through_the_index_backup() {
    let fx = base();
    fx.write(
        "parser.rs",
        "fn one() { 1 }\n\nfn two() {}\n\nfn three() { 3 }\n",
    );
    let before = fx.index();
    let out = fx.ok("git.stage_hunk", json!({"path": "parser.rs", "hunk": 1}));
    assert_eq!(out.summary, "Staged hunk in parser.rs");
    assert!(fx.git(&["diff", "--cached"]).contains("three() { 3 }"));
    assert!(!fx.git(&["diff", "--cached"]).contains("one() { 1 }"));
    fx.undo(&out).unwrap();
    assert_eq!(fx.index(), before);

    // Selected lines, with the content id guard: a stale id refuses and writes nothing.
    let stale = fx.run(
        "git.stage_hunk",
        json!({"path": "parser.rs", "new_lines": [[4, 5]], "old_lines": [[4, 5]], "content_id": "stale"}),
    );
    assert!(stale.is_err());
    assert_eq!(fx.index(), before);
    let out = fx.ok(
        "git.stage_hunk",
        json!({"path": "parser.rs", "new_lines": [[4, 5]], "old_lines": [[4, 5]]}),
    );
    assert_eq!(out.summary, "Staged 2 lines in parser.rs");
    // Unstage the staged hunk, then undo that.
    let after_stage = fx.index();
    let out2 = fx.ok("git.unstage_hunk", json!({"path": "parser.rs", "hunk": 0}));
    assert_ne!(fx.index(), after_stage);
    fx.undo(&out2).unwrap();
    assert_eq!(fx.index(), after_stage);
}

#[test]
fn commit_is_journaled_but_not_undoable() {
    let fx = base();
    fx.write("parser.rs", "c\n");
    fx.ok("git.stage", json!({"paths": ["parser.rs"]}));
    let out = fx.ok(
        "git.commit",
        json!({"message": "Fix parser\n\nbody", "sign_off": true}),
    );
    assert!(out.undo.is_none());
    assert!(out.summary.starts_with("Committed "), "{}", out.summary);
    assert!(
        out.summary.contains("cannot be undone here"),
        "{}",
        out.summary
    );
    assert!(fx
        .git(&["log", "-1", "--format=%B"])
        .contains("Signed-off-by"));
    let entry = fx.journal.get(out.journal_id.unwrap()).unwrap();
    assert_eq!(entry.command, "git.commit");
    match fx.journal.undo_status(&entry) {
        UndoStatus::Unavailable(why) => assert!(why.contains("git reset --soft"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(fx.undo(&out), Err(UndoError::NotUndoable(_))));
    // Empty messages and nothing to commit are plain errors.
    assert!(fx.run("git.commit", json!({"message": "  "})).is_err());
    let nothing = fx
        .run("git.commit", json!({"message": "again"}))
        .unwrap_err();
    assert!(
        nothing.to_string().contains("nothing to commit"),
        "{nothing}"
    );
    // Amend works.
    let am = fx.ok(
        "git.commit",
        json!({"message": "Fix parser better", "amend": true}),
    );
    assert!(am.summary.starts_with("Amended "));
}

fn make_conflict(fx: &Fx) {
    fx.write("c.txt", "base\n");
    fx.commit_all("base c");
    fx.git(&["checkout", "-q", "-b", "feature"]);
    fx.write("c.txt", "feature side\n");
    fx.commit_all("feature c");
    fx.git(&["checkout", "-q", "main"]);
    fx.write("c.txt", "main side\n");
    fx.commit_all("main c");
    let merge = Command::new("git")
        .args(["merge", "feature"])
        .current_dir(fx.root())
        .output()
        .unwrap();
    assert!(!merge.status.success());
}

#[test]
fn conflict_accept_mark_resolved_continue_and_their_undos() {
    let fx = base();
    make_conflict(&fx);
    let with_markers = fx.read("c.txt");
    assert!(String::from_utf8_lossy(&with_markers).contains("<<<<<<<"));
    let stages = fx.git(&["ls-files", "-u"]);

    // Marking resolved while markers remain is refused unless allowed.
    let err = fx
        .run("git.mark_resolved", json!({"path": "c.txt"}))
        .unwrap_err();
    assert!(err.to_string().contains("conflict markers"), "{err}");

    let accept = fx.ok(
        "git.accept_file",
        json!({"path": "c.txt", "side": "theirs"}),
    );
    assert_eq!(accept.summary, "Accepted all incoming changes in c.txt");
    assert_eq!(fx.read("c.txt"), b"feature side\n");
    let marked = fx.ok("git.mark_resolved", json!({"path": "c.txt"}));
    assert_eq!(fx.git(&["ls-files", "-u"]), "");

    // Undo mark-resolved: conflict stages are back. Undo accept: markers are back, byte for byte.
    fx.undo(&marked).unwrap();
    assert_eq!(fx.git(&["ls-files", "-u"]), stages);
    fx.undo(&accept).unwrap();
    assert_eq!(fx.read("c.txt"), with_markers);

    // Resolve ours, mark resolved, continue the merge.
    fx.ok("git.accept_file", json!({"path": "c.txt", "side": "ours"}));
    assert_eq!(fx.read("c.txt"), b"main side\n");
    fx.ok("git.mark_resolved", json!({"path": "c.txt"}));
    let cont = fx.ok("git.continue_operation", json!({}));
    assert!(cont.undo.is_none());
    assert_eq!(
        fx.git(&["rev-list", "--parents", "-n1", "HEAD"])
            .split_whitespace()
            .count(),
        3,
        "merge commit"
    );
}

#[test]
fn abort_is_destructive_and_irreversible_and_resolve_delete_is_undoable() {
    let fx = base();
    make_conflict(&fx);
    let spec = fx.cx.core.registry().get("git.abort_operation").unwrap();
    assert!(spec.spec().destructive && !spec.spec().undoable);
    let out = fx.ok("git.abort_operation", json!({}));
    assert!(out.undo.is_none());
    assert!(out.summary.contains("cannot be undone"));
    assert_eq!(fx.git(&["ls-files", "-u"]), "");
    let entry = fx.journal.get(out.journal_id.unwrap()).unwrap();
    assert!(matches!(
        fx.journal.undo_status(&entry),
        UndoStatus::Unavailable(ref why) if why.contains("cannot be undone")
    ));

    // Modify/delete conflict resolved by deleting; undo brings back file and conflict stages.
    let fx = base();
    fx.write("m.txt", "base\n");
    fx.commit_all("m");
    fx.git(&["checkout", "-q", "-b", "gone"]);
    fx.git(&["rm", "-q", "m.txt"]);
    fx.git(&["commit", "-q", "-m", "delete m"]);
    fx.git(&["checkout", "-q", "main"]);
    fx.write("m.txt", "modified on main\n");
    fx.commit_all("modify m");
    let merge = Command::new("git")
        .args(["merge", "gone"])
        .current_dir(fx.root())
        .output()
        .unwrap();
    assert!(!merge.status.success());
    let stages = fx.git(&["ls-files", "-u"]);
    assert!(!stages.is_empty());
    let out = fx.ok("git.resolve_delete", json!({"path": "m.txt"}));
    assert!(!fx.root().join("m.txt").exists());
    assert_eq!(fx.git(&["ls-files", "-u"]), "");
    assert!(matches!(out.undo, Some(UndoRecord::Multi { .. })));
    fx.undo(&out).unwrap();
    assert_eq!(fx.read("m.txt"), b"modified on main\n");
    assert_eq!(fx.git(&["ls-files", "-u"]), stages);
}

#[test]
fn undo_last_skips_undos_and_non_undoable_entries() {
    let fx = base();
    fx.write("parser.rs", "a\n");
    fx.write("keep.txt", "b\n");
    let first = fx.ok("git.discard", json!({"paths": ["parser.rs"]}));
    let second = fx.ok("git.discard", json!({"paths": ["keep.txt"]}));
    fx.write("new.txt", "n");
    fx.ok("git.stage", json!({"paths": ["new.txt"]}));
    fx.ok("git.commit", json!({"message": "add new"}));
    // The commit is not undoable; the newest undoable one is the stage before it.
    let last = fx.journal.last_undoable(None).unwrap();
    assert_eq!(last.command, "git.stage");
    let u1 = fx.journal.undo_last(&fx.cx, UndoMode::Check).unwrap();
    assert!(u1.undo_of.is_some());
    assert_eq!(
        fx.git(&["diff", "--cached", "--name-only"]),
        "",
        "stage undone (the commit stays)"
    );
    // The undo's own entry is skipped: the next undoable one is the second discard, then the first.
    assert_eq!(
        fx.journal.last_undoable(None).unwrap().id,
        second.journal_id.unwrap()
    );
    fx.journal.undo_last(&fx.cx, UndoMode::Check).unwrap();
    assert_eq!(fx.read("keep.txt"), b"b\n");
    assert_eq!(
        fx.journal.last_undoable(None).unwrap().id,
        first.journal_id.unwrap()
    );
    fx.journal.undo_last(&fx.cx, UndoMode::Check).unwrap();
    assert_eq!(fx.read("parser.rs"), b"a\n");
    assert!(matches!(
        fx.journal.undo_last(&fx.cx, UndoMode::Check),
        Err(UndoError::NotUndoable(_))
    ));
}

#[test]
fn without_a_backup_destructive_commands_refuse_until_confirmed() {
    let fx = base();
    fx.write("parser.rs", "edits\n");
    // Break the shadow location: a regular file where the data dir should be.
    let broken = fx.data.path().join("shadow");
    fs::write(&broken, "not a directory").unwrap();
    let err = fx
        .run("git.discard", json!({"paths": ["parser.rs"]}))
        .unwrap_err();
    match err {
        CommandError::Failed(e) => {
            assert!(e.downcast_ref::<BackupUnavailable>().is_some(), "{e}");
        }
        other => panic!("{other}"),
    }
    assert_eq!(fx.read("parser.rs"), b"edits\n", "nothing was discarded");
    assert!(lector_app::commands::git_write::backup_available(
        &fx.cx.core,
        fx.cx.project.as_ref().unwrap().repo().unwrap()
    )
    .is_err());

    let out = fx.ok(
        "git.discard",
        json!({"paths": ["parser.rs"], "allow_without_backup": true}),
    );
    assert!(out.undo.is_none());
    assert!(out.summary.contains("cannot be undone"));
    assert_eq!(
        fx.read("parser.rs"),
        b"fn one() {}\n\nfn two() {}\n\nfn three() {}\n"
    );
}

#[test]
fn pruned_backups_disable_undo_with_a_reason() {
    let fx = base();
    fx.write("parser.rs", "edits\n");
    let out = fx.ok("git.discard", json!({"paths": ["parser.rs"]}));
    let repo = fx.cx.project.as_ref().unwrap().repo().unwrap().clone();
    let shadow = lector_git::ShadowRepo::open(fx.data.path(), &repo).unwrap();
    shadow.prune(std::time::Duration::ZERO).unwrap();
    let entry = fx.journal.get(out.journal_id.unwrap()).unwrap();
    match fx.journal.undo_status(&entry) {
        UndoStatus::Unavailable(why) => assert!(why.contains("cleaned up"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(fx.undo(&out), Err(UndoError::BackupGone(_))));
}

#[test]
fn journal_is_persisted_bounded_and_notifies() {
    hermetic();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("lector.redb");
    let data = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(work.path()).unwrap();
    git_in(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "a\n").unwrap();
    git_in(&root, &["add", "-A"]);

    let opts = CoreOptions {
        db_path: Some(db_path.clone()),
        workers: Some(2),
        ..Default::default()
    };
    let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let id = {
        let core = AppCore::open(opts.clone());
        let journal = Journal::install_in(&core, data.path().to_path_buf());
        let sink = seen.clone();
        let _sub = journal.subscribe(move |e| sink.lock().push(*e));
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        let cx = CommandContext::new(
            core.clone(),
            Some(Project::open(&root, settings).unwrap()),
            Source::Agent,
        );
        let out = core
            .registry()
            .invoke("git.unstage", json!({"paths": ["a.txt"]}), &cx)
            .unwrap();
        out.journal_id.unwrap()
    };
    assert_eq!(seen.lock().len(), 1);
    // A new process (new core over the same file) sees the entry, with its source.
    let core = AppCore::open(opts);
    let journal = Journal::install_in(&core, data.path().to_path_buf());
    let entry = journal.get(id).expect("entry survived the restart");
    assert_eq!(entry.command, "git.unstage");
    assert_eq!(entry.source, Source::Agent);
    assert_eq!(entry.repo.as_deref(), Some(root.as_path()));
    assert_eq!(journal.list(10).len(), 1);

    // Bounded: the oldest entries are dropped, ids keep counting.
    let small = Journal::with_limit(
        std::sync::Arc::new(lector_app::journal::MemoryStore::default()),
        data.path().to_path_buf(),
        5,
    );
    for i in 0..12 {
        small.append(lector_app::journal::ActivityEntry {
            id: 0,
            time: std::time::SystemTime::now(),
            command: "x".into(),
            summary: format!("e{i}"),
            repo: None,
            paths: vec![],
            undo: None,
            undone: false,
            args: Value::Null,
            source: Source::Internal,
            undo_of: None,
            undone_by: None,
        });
    }
    let list = small.list(100);
    assert_eq!(list.len(), 5);
    assert_eq!(list[0].summary, "e11");
    assert_eq!(list[0].id, 12);
    assert_eq!(list[4].summary, "e7");
    assert_eq!(
        small
            .list_before(2, Some(10))
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        [9, 8]
    );
}

#[test]
fn every_git_write_command_has_a_closed_schema_and_the_right_flags() {
    let fx = base();
    let reg = fx.cx.core.registry();
    for id in [
        "git.stage",
        "git.unstage",
        "git.discard",
        "git.stage_hunk",
        "git.unstage_hunk",
        "git.discard_hunk",
        "git.stage_all",
        "git.unstage_all",
        "git.discard_all",
        "git.commit",
        "git.accept_file",
        "git.mark_resolved",
        "git.resolve_delete",
        "git.continue_operation",
        "git.abort_operation",
    ] {
        let cmd = reg.get(id).unwrap_or_else(|| panic!("{id} not registered"));
        let spec = cmd.spec();
        assert!(spec.writes, "{id}");
        assert_eq!(spec.args_schema["type"], "object", "{id}");
        assert_eq!(spec.args_schema["additionalProperties"], false, "{id}");
    }
    for id in [
        "git.discard",
        "git.discard_hunk",
        "git.discard_all",
        "git.accept_file",
        "git.resolve_delete",
        "git.abort_operation",
    ] {
        assert!(reg.get(id).unwrap().spec().destructive, "{id}");
    }
    for id in [
        "git.stage",
        "git.unstage",
        "git.commit",
        "git.continue_operation",
    ] {
        assert!(!reg.get(id).unwrap().spec().destructive, "{id}");
    }
    assert!(reg.get("git.commit").unwrap().spec().ui_action.is_none());
    assert!(!reg.get("git.commit").unwrap().spec().undoable);
    // Typos are rejected before anything runs.
    assert!(matches!(
        fx.run("git.discard", json!({"pathz": ["a"]})),
        Err(CommandError::InvalidArgs(_))
    ));
    // Paths outside the repository are refused.
    assert!(fx
        .run("git.stage", json!({"paths": ["/etc/passwd"]}))
        .is_err());
    assert!(fx.run("git.discard", json!({"paths": ["../x"]})).is_err());
}
