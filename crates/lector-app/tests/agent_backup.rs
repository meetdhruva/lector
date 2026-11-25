//! Agent write backups (ADR 0011) without an agent: in a folder without git nothing is created
//! until the first backup, backups are path-scoped first-touch snapshots, a turn's undo is one
//! `RestoreFiles` record the journal undoes, checks and redoes like any other, from a fresh core
//! too; "undo last" stays in the folder. In a repository, git-ignored files are covered.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Once};
use std::time::SystemTime;

use lector_app::commands::agent_backup::{AgentBackups, MAX_FOLDER_BACKUP, TURN_BACKUP_BUDGET};
use lector_app::commands::journal_undo::{invoke_undo, invoke_undo_last};
use lector_app::commands::{CommandContext, Source};
use lector_app::journal::{
    ActivityEntry, Journal, MemoryStore, UndoError, UndoMode, UndoRecord, UndoStatus,
};
use lector_app::Project;
use lector_app::{AppCore, CoreOptions};
use lector_config::Settings;
use lector_git::ShadowRepo;
use serde_json::json;

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    });
}

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    core: AppCore,
    project: Project,
}

impl Fx {
    fn new(git: bool) -> Fx {
        hermetic();
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let root = base.join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one\n").unwrap();
        if git {
            for args in [
                &["init", "-q", "-b", "main"][..],
                &["add", "-A"],
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-q",
                    "-m",
                    "i",
                ],
            ] {
                let ok = Command::new("git")
                    .args(args)
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "git {args:?}");
            }
        }
        let data = base.join("data");
        let core = AppCore::open(CoreOptions {
            data_dir: Some(data.clone()),
            ..CoreOptions::for_tests()
        });
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(&root, settings).unwrap();
        Fx {
            _tmp: tmp,
            root,
            data,
            core,
            project,
        }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.root.join(rel)).ok()
    }

    fn cx(&self) -> CommandContext {
        CommandContext::new(
            self.core.clone(),
            Some(self.project.clone()),
            Source::Internal,
        )
    }

    /// One agent turn: back up, write (first touch and a second write), finish, journal.
    fn turn(&self, backups: &AgentBackups, journal: &Journal, writes: &[(&str, &str)]) -> u64 {
        let mut ledger = backups.begin_turn("Before agent turn 1: test");
        for (rel, text) in writes {
            backups
                .before_write(Some(&mut ledger), rel, text.len() as u64)
                .unwrap();
            self.write(rel, text);
        }
        let backup = backups
            .finish_turn(&ledger, "Agent turn 1: test")
            .expect("backed up");
        let paths: Vec<PathBuf> = backup
            .changes
            .iter()
            .map(|c| backups.workdir().join(c.path()))
            .collect();
        journal
            .append(entry(backups.workdir(), paths, backup.undo))
            .unwrap()
    }
}

fn entry(workdir: &Path, paths: Vec<PathBuf>, undo: Option<UndoRecord>) -> ActivityEntry {
    ActivityEntry {
        id: 0,
        time: SystemTime::now(),
        command: "agent.turn".into(),
        summary: "Fake turn 1".into(),
        repo: Some(workdir.to_path_buf()),
        paths,
        undo,
        undone: false,
        args: json!({}),
        source: Source::Agent,
        undo_of: None,
        undone_by: None,
    }
}

#[test]
fn folder_backups_are_lazy_path_scoped_and_undone_through_the_journal() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    assert!(backups.in_folder());
    assert_eq!(backups.workdir(), fx.root);
    let ledger = backups.begin_turn("Before agent turn 1");
    assert!(ledger.checkpoint.is_none(), "no whole-folder checkpoint");
    assert!(!fx.data.join("shadow").exists(), "nothing created yet");
    assert!(backups.finish_turn(&ledger, "nothing").is_none());

    let journal = Journal::of(&fx.core).unwrap();
    let id = fx.turn(
        &backups,
        &journal,
        &[
            ("a.txt", "two\n"),
            ("new/x.txt", "x\n"),
            ("a.txt", "three\n"),
        ],
    );
    assert!(ShadowRepo::path_for(&fx.data, &fx.root)
        .join("HEAD")
        .is_file());
    let e = journal.get(id).unwrap();
    match &e.undo {
        Some(UndoRecord::RestoreFiles { paths, after, .. }) => {
            assert_eq!(paths, &["a.txt".to_string(), "new/x.txt".to_string()]);
            assert_eq!(after.len(), 2, "the after state, per touched path");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(journal.undo_status(&e), UndoStatus::Available);
    let undo = invoke_undo(&fx.cx(), id, UndoMode::Check).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\n");
    assert!(!fx.root.join("new").exists());
    invoke_undo(&fx.cx(), undo.id, UndoMode::Check).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "three\n");
    assert_eq!(fx.read("new/x.txt").unwrap(), "x\n");
    assert!(!fx.root.join(".git").exists());
}

#[test]
fn a_fresh_journal_finds_a_folders_backups_and_reports_them_gone() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let journal = Journal::of(&fx.core).unwrap();
    fx.turn(&backups, &journal, &[("a.txt", "agent\n")]);
    let record = journal.list(1)[0].undo.clone();

    // Another process (the CLI after a restart): a new core with a new journal over the same
    // data directory.
    let core = AppCore::open(CoreOptions::for_tests());
    let other = Journal::new(Arc::new(MemoryStore::default()), fx.data.clone());
    core.set_extension(Arc::new(other.clone()));
    core.set_activity_sink(Arc::new(other.clone()));
    let cx = CommandContext::new(core, Some(fx.project.clone()), Source::Ui);
    let id = other
        .append(entry(&fx.root, vec![fx.root.join("a.txt")], record.clone()))
        .unwrap();
    fs::write(fx.root.join("a.txt"), "user\n").unwrap();
    assert!(matches!(
        invoke_undo(&cx, id, UndoMode::Check),
        Err(UndoError::Changed { .. })
    ));
    invoke_undo(&cx, id, UndoMode::Force).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\n");

    // The shadow was removed (pruned as an orphan): the entry says the backup is gone.
    fs::remove_dir_all(ShadowRepo::path_for(&fx.data, &fx.root)).unwrap();
    let id = other
        .append(entry(&fx.root, vec![fx.root.join("a.txt")], record))
        .unwrap();
    let e = other.get(id).unwrap();
    assert!(matches!(other.undo_status(&e), UndoStatus::Unavailable(_)));
    assert!(matches!(
        invoke_undo(&cx, id, UndoMode::Force),
        Err(UndoError::BackupGone(_))
    ));
    assert!(
        !ShadowRepo::path_for(&fx.data, &fx.root).exists(),
        "looking for backups recreates nothing"
    );
}

#[test]
fn undo_last_in_a_folder_stays_in_that_folder() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let journal = Journal::of(&fx.core).unwrap();
    fx.turn(&backups, &journal, &[("a.txt", "agent\n")]);
    // A newer undoable entry of another folder must not be picked.
    journal
        .append(entry(
            Path::new("/somewhere/else"),
            Vec::new(),
            Some(UndoRecord::Invoke {
                command: "no.such.command".into(),
                args: json!({}),
            }),
        ))
        .unwrap();
    invoke_undo_last(&fx.cx(), UndoMode::Check).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\n");
}

#[test]
fn files_over_the_limit_are_not_backed_up_in_a_folder() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let mut ledger = backups.begin_turn("t");
    let err = backups
        .before_write(Some(&mut ledger), "a.txt", MAX_FOLDER_BACKUP + 1)
        .unwrap_err();
    assert!(err.contains("64 MiB"), "{err}");
    assert_eq!(ledger.touched().count(), 0);
}

#[test]
fn in_a_repository_ignored_files_join_the_turn_and_tracked_ones_need_no_backup() {
    let fx = Fx::new(true);
    fx.write(".gitignore", ".env\n");
    fx.write(".env", "SECRET=keep\n");
    let backups = AgentBackups::new(&fx.core, &fx.project);
    assert!(!backups.in_folder());
    let mut ledger = backups.begin_turn("Before agent turn 1");
    assert!(ledger.checkpoint.is_some());
    assert!(backups
        .before_write(Some(&mut ledger), "a.txt", 3)
        .unwrap()
        .is_none());
    assert!(backups
        .before_write(Some(&mut ledger), ".env", 3)
        .unwrap()
        .is_some());
    assert!(
        backups
            .before_write(Some(&mut ledger), ".env", 3)
            .unwrap()
            .is_none(),
        "first touch only"
    );
    fx.write("a.txt", "two\n");
    fx.write(".env", "SECRET=agent\n");
    let backup = backups.finish_turn(&ledger, "Agent turn 1").unwrap();
    let paths: Vec<&str> = backup.changes.iter().map(|c| c.path()).collect();
    assert_eq!(paths, [".env", "a.txt"]);
    let journal = Journal::of(&fx.core).unwrap();
    let id = journal
        .append(entry(backups.workdir(), Vec::new(), backup.undo))
        .unwrap();
    invoke_undo(&fx.cx(), id, UndoMode::Check).unwrap();
    assert_eq!(fx.read(".env").unwrap(), "SECRET=keep\n");
    assert_eq!(fx.read("a.txt").unwrap(), "one\n");
}

fn sparse(path: &Path, len: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::File::create(path).unwrap().set_len(len).unwrap();
}

fn snapshot_count(fx: &Fx) -> usize {
    ShadowRepo::existing_folder(&fx.data, &fx.root)
        .map(|s| s.snapshots().unwrap().len())
        .unwrap_or(0)
}

/// An agent may only write files: a directory, FIFO or device named as the target is refused
/// before any backup, so nothing below it is walked or copied (in a folder, no shadow is even
/// created). In a repository the same holds for an ignored directory (`node_modules`).
#[test]
fn writes_to_anything_but_a_file_are_refused_before_any_backup() {
    let fx = Fx::new(false);
    sparse(&fx.root.join("Documents/big.iso"), MAX_FOLDER_BACKUP + 1);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let mut ledger = backups.begin_turn("t");
    let err = backups
        .before_write(Some(&mut ledger), "Documents", 3)
        .unwrap_err();
    assert!(err.contains("not a regular file"), "{err}");
    let err = backups.before_write(None, "Documents", 3).unwrap_err();
    assert!(err.contains("not a regular file"), "{err}");
    assert!(
        !fx.data.join("shadow").exists(),
        "no shadow, so nothing was read into one"
    );
    assert_eq!(ledger.touched().count(), 0);

    let repo = Fx::new(true);
    repo.write(".gitignore", "node_modules/\n");
    sparse(
        &repo.root.join("node_modules/pkg/blob.bin"),
        MAX_FOLDER_BACKUP + 1,
    );
    let backups = AgentBackups::new(&repo.core, &repo.project);
    let mut ledger = backups.begin_turn("t");
    let shadow = ShadowRepo::open(&repo.data, repo.project.repo().expect("a repository")).unwrap();
    let before = shadow.snapshots().unwrap().len();
    let err = backups
        .before_write(Some(&mut ledger), "node_modules", 3)
        .unwrap_err();
    assert!(err.contains("not a regular file"), "{err}");
    assert_eq!(shadow.snapshots().unwrap().len(), before, "no backup taken");
}

/// The size limit applies to every write, not only the first touch of a file.
#[test]
fn every_write_in_a_folder_checks_the_size_limit() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let mut ledger = backups.begin_turn("t");
    backups.before_write(Some(&mut ledger), "a.txt", 3).unwrap();
    let err = backups
        .before_write(Some(&mut ledger), "a.txt", MAX_FOLDER_BACKUP + 1)
        .unwrap_err();
    assert!(err.contains("64 MiB"), "{err}");
}

/// A touched file that grew past the limit by other means during the turn is not stored at the
/// end: the turn's undo still restores the state before the turn, the check still notices later
/// edits (the after-state is a hash, not a copy), and redoing that file says it cannot.
#[test]
fn a_file_grown_past_the_limit_during_the_turn_is_not_stored() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let mut ledger = backups.begin_turn("Before agent turn 1");
    backups.before_write(Some(&mut ledger), "a.txt", 4).unwrap();
    // Another program appends to the file during the turn.
    sparse(&fx.root.join("a.txt"), MAX_FOLDER_BACKUP + 1);
    let shadow_dir = ShadowRepo::path_for(&fx.data, &fx.root);
    let backup = backups.finish_turn(&ledger, "Agent turn 1").unwrap();
    let paths: Vec<&str> = backup.changes.iter().map(|c| c.path()).collect();
    assert_eq!(paths, ["a.txt"], "still a change of the turn");
    let size: u64 = fs::read_dir(shadow_dir.join("objects/pack"))
        .map(|d| {
            d.filter_map(|e| e.ok()?.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    assert!(
        size < 1024 * 1024,
        "the large file was stored ({size} bytes)"
    );
    let journal = Journal::of(&fx.core).unwrap();
    let id = journal
        .append(entry(backups.workdir(), Vec::new(), backup.undo))
        .unwrap();
    let undo = invoke_undo(&fx.cx(), id, UndoMode::Check).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\n");
    match invoke_undo(&fx.cx(), undo.id, UndoMode::Force) {
        Err(UndoError::Failed(why)) => assert!(why.contains("not backed up"), "{why}"),
        other => panic!("redo of a file too large to store: {other:?}"),
    }
    assert_eq!(fx.read("a.txt").unwrap(), "one\n", "nothing written");
}

/// One turn backs up at most `TURN_BACKUP_BUDGET` of path-scoped data (its first-touch backups
/// and what it will record at the end), so many large files cannot fill the disk.
#[test]
fn a_turn_refuses_writes_beyond_its_backup_budget() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let mut ledger = backups.begin_turn("t");
    let each: u64 = 60 * 1024 * 1024;
    let mut refused = None;
    for i in 0..20 {
        if let Err(err) = backups.before_write(Some(&mut ledger), &format!("f{i}.bin"), each) {
            refused = Some((i, err));
            break;
        }
    }
    let (i, err) = refused.expect("refused before 20 x 60 MiB");
    assert_eq!(i as u64, TURN_BACKUP_BUDGET / each, "{err}");
    assert!(err.contains("1024 MiB"), "{err}");
}

/// Every turn trims its shadow before its first backup (not only once per process): a snapshot
/// older than the retention is gone after the next turn's first write.
#[test]
fn each_turn_trims_its_shadow_before_its_first_backup() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let journal = Journal::of(&fx.core).unwrap();
    fx.turn(&backups, &journal, &[("a.txt", "agent\n")]);
    let shadow = ShadowRepo::existing_folder(&fx.data, &fx.root).unwrap();
    let newest = shadow.snapshots().unwrap()[0].commit.to_string();
    let ok = Command::new("git")
        .arg("--git-dir")
        .arg(shadow.git_dir())
        .args([
            "update-ref",
            "refs/lector/snapshots/1000000000-00000000-1",
            &newest,
        ])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let old = snapshot_count(&fx);
    fx.turn(&backups, &journal, &[("a.txt", "again\n")]);
    let refs = shadow.snapshots().unwrap();
    assert!(
        refs.iter().all(|s| s.time > 1_000_000_000),
        "the old snapshot was trimmed"
    );
    assert!(refs.len() >= old, "the turn's own snapshots are there");
}

/// A turn that creates a file another program then grows past the limit: the turn is undoable
/// (the file is its only change), check mode removes the file when it is unchanged since the turn
/// ended, and the undo says it cannot be redone (its content was never backed up); redo refuses.
#[test]
fn a_created_file_grown_past_the_limit_is_undone_but_cannot_be_redone() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let journal = Journal::of(&fx.core).unwrap();
    let mut ledger = backups.begin_turn("Before agent turn 1");
    backups
        .before_write(Some(&mut ledger), "big.log", 3)
        .unwrap();
    fx.write("big.log", "hi\n");
    sparse(&fx.root.join("big.log"), MAX_FOLDER_BACKUP + 1);
    let backup = backups.finish_turn(&ledger, "Agent turn 1").unwrap();
    let paths: Vec<&str> = backup.changes.iter().map(|c| c.path()).collect();
    assert_eq!(paths, ["big.log"], "the turn's only change");
    let id = journal
        .append(entry(backups.workdir(), Vec::new(), backup.undo))
        .unwrap();
    let undo = invoke_undo(&fx.cx(), id, UndoMode::Check).unwrap();
    assert!(
        !fx.root.join("big.log").exists(),
        "the before-state: absent"
    );
    assert!(
        undo.summary.contains("cannot be redone"),
        "{}",
        undo.summary
    );
    assert_eq!(undo.args["not_redoable"], json!(["big.log"]));
    match invoke_undo(&fx.cx(), undo.id, UndoMode::Check) {
        Err(UndoError::Failed(why)) => assert!(why.contains("not backed up"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// Force never destroys content Lector could not back up: when the large file changed after the
/// turn ended, check mode reports it and force refuses with the reason, leaving it in place.
#[test]
fn force_refuses_to_remove_a_large_file_changed_since_the_turn() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let journal = Journal::of(&fx.core).unwrap();
    let mut ledger = backups.begin_turn("Before agent turn 1");
    backups
        .before_write(Some(&mut ledger), "big.log", 3)
        .unwrap();
    backups.before_write(Some(&mut ledger), "a.txt", 3).unwrap();
    fx.write("big.log", "hi\n");
    fx.write("a.txt", "two\n");
    sparse(&fx.root.join("big.log"), MAX_FOLDER_BACKUP + 1);
    let backup = backups.finish_turn(&ledger, "Agent turn 1").unwrap();
    let id = journal
        .append(entry(backups.workdir(), Vec::new(), backup.undo))
        .unwrap();
    // The other program keeps writing after the turn ended.
    sparse(&fx.root.join("big.log"), MAX_FOLDER_BACKUP + 2);
    assert!(matches!(
        invoke_undo(&fx.cx(), id, UndoMode::Check),
        Err(UndoError::Changed { .. })
    ));
    match invoke_undo(&fx.cx(), id, UndoMode::Force) {
        Err(UndoError::Failed(why)) => {
            assert!(why.contains("big.log") && why.contains("back"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fs::metadata(fx.root.join("big.log")).unwrap().len(),
        MAX_FOLDER_BACKUP + 2,
        "kept"
    );
    assert_eq!(
        fx.read("a.txt").unwrap(),
        "two\n",
        "nothing restored: all or nothing"
    );
    assert!(!journal.get(id).unwrap().undone);
}

/// Codex finding: in a repository a turn deletes a file over the checkpoint's 256 MiB limit. The
/// checkpoint did not store it, so the turn's undo cannot bring it back: it must fail naming the
/// file, write nothing and leave the turn not undone, never succeed as a no-op.
#[test]
fn undoing_a_repository_turn_that_deleted_an_unstored_file_fails_naming_it() {
    let fx = Fx::new(true);
    sparse(
        &fx.root.join("huge.bin"),
        lector_git::shadow::WORKTREE_MAX_FILE + 1,
    );
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let ledger = backups.begin_turn("Before agent turn 1");
    assert!(ledger.checkpoint.is_some());
    fs::remove_file(fx.root.join("huge.bin")).unwrap();
    fx.write("a.txt", "two\n");
    let backup = backups.finish_turn(&ledger, "Agent turn 1").unwrap();
    let paths: Vec<&str> = backup.changes.iter().map(|c| c.path()).collect();
    assert_eq!(paths, ["a.txt", "huge.bin"]);
    let journal = Journal::of(&fx.core).unwrap();
    let id = journal
        .append(entry(backups.workdir(), Vec::new(), backup.undo))
        .unwrap();
    match invoke_undo(&fx.cx(), id, UndoMode::Check) {
        Err(UndoError::Failed(why)) => assert!(why.contains("huge.bin"), "{why}"),
        other => panic!("a no-op undo: {other:?}"),
    }
    assert!(!journal.get(id).unwrap().undone);
    assert_eq!(fx.read("a.txt").unwrap(), "two\n", "all or nothing");
}

/// A restore record with no paths is not an undo: it is refused, never a successful no-op.
#[test]
fn an_undo_record_without_paths_is_refused() {
    let fx = Fx::new(false);
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let journal = Journal::of(&fx.core).unwrap();
    let id = fx.turn(&backups, &journal, &[("a.txt", "agent\n")]);
    let Some(UndoRecord::RestoreFiles { shadow, .. }) = journal.get(id).unwrap().undo else {
        panic!("a turn record");
    };
    let empty = journal
        .append(entry(
            backups.workdir(),
            Vec::new(),
            Some(UndoRecord::RestoreFiles {
                shadow,
                paths: Vec::new(),
                after: Default::default(),
            }),
        ))
        .unwrap();
    assert!(invoke_undo(&fx.cx(), empty, UndoMode::Force).is_err());
    assert!(!journal.get(empty).unwrap().undone);
}

/// Self-audit b/d: a touched file whose parent became a link during the turn keeps the turn
/// undoable on record (never a silently dropped undo), its after-state lists every changed path
/// (an empty map would turn the undo check off), and the undo refuses naming it, writing nothing.
#[cfg(unix)]
#[test]
fn a_parent_replaced_by_a_link_keeps_the_turn_on_record_and_the_undo_refuses() {
    let fx = Fx::new(false);
    fx.write("d/x.txt", "x\n");
    let outside = fx.root.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let backups = AgentBackups::new(&fx.core, &fx.project);
    let mut ledger = backups.begin_turn("Before agent turn 1");
    backups.before_write(Some(&mut ledger), "a.txt", 6).unwrap();
    backups
        .before_write(Some(&mut ledger), "d/x.txt", 6)
        .unwrap();
    fx.write("a.txt", "agent\n");
    fx.write("d/x.txt", "agent\n");
    fs::remove_dir_all(fx.root.join("d")).unwrap();
    std::os::unix::fs::symlink(&outside, fx.root.join("d")).unwrap();
    let backup = backups
        .finish_turn(&ledger, "Agent turn 1")
        .expect("the turn is on record");
    match &backup.undo {
        Some(UndoRecord::RestoreFiles { paths, after, .. }) => {
            assert_eq!(paths.len(), 2);
            assert_eq!(after.len(), 2, "{after:?}");
        }
        other => panic!("{other:?}"),
    }
    let journal = Journal::of(&fx.core).unwrap();
    let id = journal
        .append(entry(backups.workdir(), Vec::new(), backup.undo))
        .unwrap();
    assert!(invoke_undo(&fx.cx(), id, UndoMode::Force).is_err());
    assert_eq!(fx.read("a.txt").unwrap(), "agent\n", "nothing restored");
    assert!(!journal.get(id).unwrap().undone);
}

/// Self-audit b: an index record with no paths is refused like a file record without paths.
#[test]
fn an_index_undo_record_without_paths_is_refused() {
    let fx = Fx::new(true);
    let shadow = ShadowRepo::open(&fx.data, fx.project.repo().unwrap()).unwrap();
    let snap = shadow.snapshot_index(&["a.txt"], "index").unwrap();
    let journal = Journal::of(&fx.core).unwrap();
    let workdir = fx.project.repo().unwrap().workdir().to_path_buf();
    let id = journal
        .append(entry(
            &workdir,
            Vec::new(),
            Some(UndoRecord::RestoreIndex {
                shadow: snap,
                paths: Vec::new(),
                after: Default::default(),
            }),
        ))
        .unwrap();
    assert!(invoke_undo(&fx.cx(), id, UndoMode::Force).is_err());
}

// ---------------------------------------------------------------------------------------------
// One file, one identity: keys by on-disk spelling (case-insensitive / normalising file systems).
// ---------------------------------------------------------------------------------------------

/// Case and NFC/NFD folding, as macOS and Windows compare names.
fn fold(s: &str) -> String {
    s.replace("e\u{301}", "\u{e9}").to_lowercase()
}

/// A resolver that behaves like a case-insensitive, normalisation-insensitive file system over
/// the real directory: a name reaches the listed entry it folds to. With it, the folding logic
/// runs on any platform; the test writes to the name the "file system" stores.
struct FoldingNames;

impl lector_fs::EntryNames for FoldingNames {
    fn on_disk(&self, dir: &Path, name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
        let wanted = fold(&name.to_string_lossy());
        fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name())
            .find(|n| fold(&n.to_string_lossy()) == wanted)
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `a.txt` then `A.txt` in one turn on a folding file system: one file, one first-touch backup,
/// and the undo restores the original bytes under the original name. The same for a file the
/// turn creates (`New.txt`, stored by the file system as `new.txt`, the way HFS+ stores NFD) and
/// for NFC then NFD spellings, in a folder and in a repository (git-ignored files).
#[test]
fn spellings_of_one_file_share_one_ledger_entry_with_a_folding_resolver() {
    for git in [false, true] {
        let fx = Fx::new(git);
        if git {
            fx.write(".gitignore", "*.env\n");
        }
        let ext = if git { "env" } else { "txt" };
        fx.write(&format!("Readme.{ext}"), "original\n");
        fx.write(&format!("caf\u{e9}.{ext}"), "nfc\n");
        let backups =
            AgentBackups::new(&fx.core, &fx.project).with_entry_names(Arc::new(FoldingNames));
        let mut ledger = backups.begin_turn("Before agent turn 1");
        // Writes as a folding file system would carry them out: to the stored name.
        let write = |ledger: &mut lector_app::commands::agent_backup::TurnLedger, req: &str| {
            backups.before_write(Some(ledger), req, 6).unwrap();
            let key = backups.file_key(req);
            fx.write(&key, &format!("{req}\n"));
            backups.after_write(ledger);
        };
        write(&mut ledger, &format!("README.{ext}"));
        write(&mut ledger, &format!("readme.{ext}"));
        write(&mut ledger, &format!("cafe\u{301}.{ext}"));
        write(&mut ledger, &format!("CAF\u{c9}.{ext}"));
        // A new file the file system stores under another spelling than the one requested.
        backups
            .before_write(Some(&mut ledger), &format!("New.{ext}"), 6)
            .unwrap();
        fx.write(&format!("new.{ext}"), "created\n");
        backups.after_write(&mut ledger);
        write(&mut ledger, &format!("NEW.{ext}"));
        let mut touched: Vec<&str> = ledger.touched().collect();
        touched.sort();
        assert_eq!(
            touched,
            [
                format!("Readme.{ext}"),
                format!("caf\u{e9}.{ext}"),
                format!("new.{ext}")
            ],
            "git={git}: one entry per file, in its on-disk spelling"
        );
        let backup = backups.finish_turn(&ledger, "Agent turn 1").unwrap();
        let journal = Journal::of(&fx.core).unwrap();
        let id = journal
            .append(entry(backups.workdir(), Vec::new(), backup.undo))
            .unwrap();
        invoke_undo(&fx.cx(), id, UndoMode::Check).unwrap();
        assert_eq!(fx.read(&format!("Readme.{ext}")).unwrap(), "original\n");
        assert_eq!(fx.read(&format!("caf\u{e9}.{ext}")).unwrap(), "nfc\n");
        assert!(fx.read(&format!("new.{ext}")).is_none(), "git={git}");
        let names = names_in(&fx.root);
        assert!(
            names.contains(&format!("Readme.{ext}")),
            "git={git}: {names:?}"
        );
        assert!(
            !names.contains(&format!("README.{ext}")),
            "never renamed: {names:?}"
        );
    }
}

/// How the temp directory's file system compares names, probed at run time.
fn probe_folding(dir: &Path) -> (bool, bool) {
    let probe = dir.join("probe-case");
    fs::write(&probe, "p").unwrap();
    let case = dir.join("PROBE-CASE").exists();
    fs::remove_file(&probe).unwrap();
    let nfc = dir.join("probe-caf\u{e9}");
    fs::write(&nfc, "p").unwrap();
    let normalising = dir.join("probe-cafe\u{301}").exists();
    fs::remove_file(&nfc).unwrap();
    (case, normalising)
}

/// The real file system, every platform: two spellings of one file in a turn (`a.txt` then
/// `A.txt`, NFC then NFD, and a file the turn creates as `New.txt` then writes as `new.txt`). On
/// a case-insensitive / normalising file system that is one file with one first-touch backup and
/// the undo restores the original bytes under the original name; on a case-sensitive one they
/// are two files, each undone on its own. Prints which branch ran.
#[test]
fn spellings_of_one_file_on_the_real_file_system() {
    for git in [false, true] {
        let fx = Fx::new(git);
        let (case, normalising) = probe_folding(&fx.root);
        println!("git={git}: case-insensitive={case} normalising={normalising}");
        require_ci_folding(&fx.root, case, Some(normalising));
        if git {
            fx.write(".gitignore", "*.env\n");
        }
        let ext = if git { "env" } else { "txt" };
        let a = format!("a.{ext}");
        let nfc = format!("caf\u{e9}.{ext}");
        fx.write(&a, "original\n");
        fx.write(&nfc, "nfc\n");
        let backups = AgentBackups::new(&fx.core, &fx.project);
        let mut ledger = backups.begin_turn("Before agent turn 1");
        // Each write as a program would do it: to the spelling it was asked for.
        for req in [
            a.clone(),
            format!("A.{ext}"),
            nfc.clone(),
            format!("cafe\u{301}.{ext}"),
            format!("New.{ext}"),
            format!("new.{ext}"),
        ] {
            backups.before_write(Some(&mut ledger), &req, 6).unwrap();
            fx.write(&req, &format!("{req}\n"));
            backups.after_write(&mut ledger);
        }
        let touched: Vec<String> = ledger.touched().map(str::to_owned).collect();
        let expected = (if case { 2 } else { 4 }) + if normalising { 1 } else { 2 };
        assert_eq!(touched.len(), expected, "git={git}: {touched:?}");
        let backup = backups.finish_turn(&ledger, "Agent turn 1").unwrap();
        let journal = Journal::of(&fx.core).unwrap();
        let id = journal
            .append(entry(backups.workdir(), Vec::new(), backup.undo))
            .unwrap();
        invoke_undo(&fx.cx(), id, UndoMode::Check).unwrap();
        assert_eq!(fx.read(&a).unwrap(), "original\n", "git={git}");
        assert_eq!(fx.read(&nfc).unwrap(), "nfc\n", "git={git}");
        let names = names_in(&fx.root);
        assert!(names.contains(&a), "the original name: {names:?}");
        assert!(!names.contains(&format!("A.{ext}")), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&format!("new.{ext}"))),
            "{names:?}"
        );
        assert!(
            !names.contains(&format!("cafe\u{301}.{ext}")) || normalising,
            "the NFD file the turn created is gone: {names:?}"
        );
    }
}

/// On CI for macOS and Windows the temp directory must be case-insensitive (and on macOS,
/// APFS, insensitive to Unicode normalisation on lookup): if a runner's is not, this fails
/// loudly instead of silently running the case-sensitive branch.
fn require_ci_folding(dir: &Path, case: bool, normalising: Option<bool>) {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let os = std::env::consts::OS;
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        assert!(
            case,
            "CI on {os}: {} is case-sensitive, so the case-insensitive branch would not run",
            dir.display()
        );
    }
    if cfg!(target_os = "macos") {
        if let Some(normalising) = normalising {
            assert!(
                normalising,
                "CI on macOS: {} does not fold Unicode normalisation (APFS does)",
                dir.display()
            );
        }
    }
}
