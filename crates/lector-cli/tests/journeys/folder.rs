//! `lector undo` of an agent turn in a folder that is not a git repository (ADR 0011): the turn's
//! backups live in the folder's shadow repository under the data directory, and the binary,
//! a separate process opening the same data directory, restores the files from there.

use std::fs;
use std::process::Command;

use lector_app::commands::agent_backup::AgentBackups;
use lector_app::commands::Source;
use lector_app::journal::{ActivityEntry, Journal};
use lector_app::{AppCore, CoreOptions, Project};
use lector_config::Settings;
use serde_json::{json, Value};

use crate::fixture::hermetic;

const BIN: &str = env!("CARGO_BIN_EXE_lector-cli");

#[test]
fn lector_undo_restores_an_agent_turn_in_a_folder_without_git() {
    hermetic();
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = dunce::canonicalize(tmp.path()).expect("canonical tempdir");
    let root = base.join("notes");
    let data = base.join("data");
    let config = base.join("config");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("todo.txt"), "milk\n").expect("write");

    // The turn, as the agent session records it, in a core over the on-disk database.
    let id = {
        let core = AppCore::open(CoreOptions {
            db_path: Some(data.join("lector.redb")),
            data_dir: Some(data.clone()),
            workers: Some(2),
            ..Default::default()
        });
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(&root, settings).expect("project opens");
        assert!(project.repo().is_none());
        let backups = AgentBackups::new(&core, &project);
        let mut ledger = backups.begin_turn("Before agent turn 1: shopping");
        for (rel, text) in [("todo.txt", "milk\neggs\n"), ("lists/new.txt", "bread\n")] {
            backups
                .before_write(Some(&mut ledger), rel, text.len() as u64)
                .expect("backed up");
            let path = root.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(path, text).expect("write");
        }
        let backup = backups
            .finish_turn(&ledger, "Agent turn 1: shopping")
            .expect("turn backed up");
        let journal = Journal::of(&core).expect("journal");
        journal
            .append(ActivityEntry {
                id: 0,
                time: std::time::SystemTime::now(),
                command: "agent.turn".into(),
                summary: "Fake turn 1: shopping (2 files)".into(),
                repo: Some(backups.workdir().to_path_buf()),
                paths: vec![root.join("todo.txt"), root.join("lists/new.txt")],
                undo: backup.undo,
                undone: false,
                args: json!({}),
                source: Source::Agent,
                undo_of: None,
                undone_by: None,
            })
            .expect("journaled")
    };
    assert!(!root.join(".git").exists());

    let out = Command::new(BIN)
        .args(["undo", &id.to_string(), "--json"])
        .current_dir(&root)
        .env("LECTOR_DATA_DIR", &data)
        .env("LECTOR_CONFIG_DIR", &config)
        .env_remove("RUST_LOG")
        .output()
        .expect("lector-cli runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let undo: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(undo["undone"], id);
    assert_eq!(
        fs::read_to_string(root.join("todo.txt")).expect("read"),
        "milk\n"
    );
    assert!(
        !root.join("lists").exists(),
        "created file and folder removed"
    );
    assert!(!root.join(".git").exists());
}
