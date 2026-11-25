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
                .before_write(Some(&mut ledger), rel, text.len())
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
        .before_write(Some(&mut ledger), "a.txt", MAX_FOLDER_BACKUP as usize + 1)
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
        .before_write(Some(&mut ledger), "a.txt", MAX_FOLDER_BACKUP as usize + 1)
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
    let each = 60 * 1024 * 1024;
    let mut refused = None;
    for i in 0..20 {
        if let Err(err) = backups.before_write(Some(&mut ledger), &format!("f{i}.bin"), each) {
            refused = Some((i, err));
            break;
        }
    }
    let (i, err) = refused.expect("refused before 20 x 60 MiB");
    assert_eq!(i as u64, TURN_BACKUP_BUDGET / each as u64, "{err}");
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
