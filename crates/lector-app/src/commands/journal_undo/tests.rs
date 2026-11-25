//! `journal.undo` / `journal.undo_last` (ADR 0012, item 1): the registry runs the journal's undo,
//! records it once, keeps the redo, and keeps sandboxed callers inside their project.

use std::path::{Path, PathBuf};
use std::process::Command;

use lector_config::Settings;
use serde_json::json;

use super::*;
use crate::commands::{Source, Sources, UndoPolicy};
use crate::core::CoreOptions;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn settings() -> Settings {
    let mut s = Settings::default();
    s.lsp.enabled = false;
    s
}

/// A committed repository at `<tmp>/<name>` with `a.txt` and `sub/b.txt`.
fn repo(tmp: &Path, name: &str) -> PathBuf {
    let root = tmp.join(name);
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    std::fs::write(root.join("sub/b.txt"), "bee\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    root
}

struct Fx {
    _tmp: tempfile::TempDir,
    core: AppCore,
    a: PathBuf,
    b: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let a = repo(&root, "a");
        let b = repo(&root, "b");
        let core = AppCore::open(CoreOptions {
            data_dir: Some(root.join("data")),
            ..CoreOptions::for_tests()
        });
        Fx {
            _tmp: tmp,
            core,
            a,
            b,
        }
    }

    fn cx(&self, root: &Path, source: Source) -> CommandContext {
        let project = Project::open(root, settings()).unwrap();
        CommandContext::new(self.core.clone(), Some(project), source)
    }

    fn journal(&self) -> Journal {
        Journal::of(&self.core).unwrap()
    }
}

fn staged(root: &Path) -> String {
    git(root, &["diff", "--cached", "--name-only"])
}

#[test]
fn both_commands_are_undoable_writes_for_every_source_and_record_themselves() {
    let core = AppCore::open(CoreOptions::for_tests());
    for id in [UNDO, UNDO_LAST] {
        let command = core.registry().get(id).expect("registered");
        let spec = command.spec();
        assert!(spec.writes, "{id}");
        assert_eq!(spec.undo, UndoPolicy::Undoable, "{id}");
        assert!(spec.records_itself, "{id}");
        assert_eq!(spec.allowed_sources(), Sources::All, "{id}");
        for source in [Source::Ui, Source::Cli, Source::Mcp, Source::Internal] {
            assert!(
                core.registry()
                    .list_for(source)
                    .iter()
                    .any(|c| c.spec().id == id),
                "{id} not listed for {source:?}"
            );
        }
    }
    assert_eq!(
        core.registry().get(UNDO_LAST).unwrap().spec().ui_flow,
        Some("workspace::UndoLastOperation")
    );
}

#[test]
fn journal_undo_records_the_undo_once_and_undoing_it_redoes() {
    let fx = Fx::new();
    let cx = fx.cx(&fx.a, Source::Ui);
    std::fs::write(fx.a.join("a.txt"), "two\n").unwrap();
    let reg = fx.core.registry();
    let stage = reg
        .invoke("git.stage", json!({"paths": ["a.txt"]}), &cx)
        .unwrap();
    let staged_id = stage.journal_id.unwrap();
    assert_eq!(staged(&fx.a).trim(), "a.txt");

    let undo = reg.invoke(UNDO, json!({"entry": staged_id}), &cx).unwrap();
    assert_eq!(staged(&fx.a), "");
    let undo_id = undo.journal_id.expect("the undo's entry");
    assert!(undo.undo.is_some(), "the undo carries its redo");
    // One entry for the undo (the inverse command's), none for the invocation itself.
    let entries = fx.journal().list(10);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0].id, undo_id);
    assert_eq!(entries[0].undo_of, Some(staged_id));
    assert!(fx.journal().get(staged_id).unwrap().undone);

    // Undoing the undo redoes: the original operation is current again.
    let redo = invoke_undo(&cx, undo_id, UndoMode::Check).unwrap();
    assert_eq!(redo.undo_of, Some(undo_id));
    assert_eq!(staged(&fx.a).trim(), "a.txt");
    assert!(!fx.journal().get(staged_id).unwrap().undone);
    assert!(fx.journal().get(undo_id).unwrap().undone);
    assert_eq!(fx.journal().list(10).len(), 3);
}

#[test]
fn a_changed_file_refuses_with_its_paths_and_force_restores_anyway() {
    let fx = Fx::new();
    let cx = fx.cx(&fx.a, Source::Cli);
    std::fs::write(fx.a.join("a.txt"), "mine\n").unwrap();
    let discard = fx
        .core
        .registry()
        .invoke("git.discard", json!({"paths": ["a.txt"]}), &cx)
        .unwrap();
    let id = discard.journal_id.unwrap();
    std::fs::write(fx.a.join("a.txt"), "later\n").unwrap();

    match invoke_undo(&cx, id, UndoMode::Check) {
        Err(UndoError::Changed { id: got, paths }) => {
            assert_eq!(got, id);
            assert_eq!(paths, ["a.txt"]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(fx.a.join("a.txt")).unwrap(),
        "later\n"
    );
    let forced = invoke_undo(&cx, id, UndoMode::Force).unwrap();
    assert_eq!(forced.command, UNDO);
    assert_eq!(forced.args, json!({"entry": id}));
    assert_eq!(forced.source, Source::Cli);
    assert_eq!(
        std::fs::read_to_string(fx.a.join("a.txt")).unwrap(),
        "mine\n"
    );
    // "Restore anyway" backed up what it overwrote: its redo brings "later" back.
    invoke_undo(&cx, forced.id, UndoMode::Check).unwrap();
    assert_eq!(
        std::fs::read_to_string(fx.a.join("a.txt")).unwrap(),
        "later\n"
    );
    assert!(matches!(
        invoke_undo(&cx, 999, UndoMode::Check),
        Err(UndoError::NotFound(999))
    ));
    assert!(matches!(
        fx.core
            .registry()
            .invoke(UNDO, json!({"entry": id, "mode": "maybe"}), &cx),
        Err(CommandError::InvalidArgs(_))
    ));
}

#[test]
fn undo_last_picks_the_newest_entry_of_the_project() {
    let fx = Fx::new();
    let (cx_a, cx_b) = (fx.cx(&fx.a, Source::Ui), fx.cx(&fx.b, Source::Ui));
    std::fs::write(fx.a.join("a.txt"), "two\n").unwrap();
    std::fs::write(fx.b.join("a.txt"), "mine\n").unwrap();
    let reg = fx.core.registry();
    let staged_a = reg
        .invoke("git.stage", json!({"paths": ["a.txt"]}), &cx_a)
        .unwrap()
        .journal_id
        .unwrap();
    reg.invoke("git.discard", json!({"paths": ["a.txt"]}), &cx_b)
        .unwrap();
    let undone = invoke_undo_last(&cx_a, UndoMode::Check).unwrap();
    assert_eq!(undone.undo_of, Some(staged_a));
    assert_eq!(staged(&fx.a), "", "a's stage undone");
    let b_text = || std::fs::read_to_string(fx.b.join("a.txt")).unwrap();
    assert_eq!(b_text(), "one\n", "b's newer discard untouched");
    match invoke_undo_last(&cx_a, UndoMode::Check) {
        Err(UndoError::NotUndoable(why)) => assert_eq!(why, "Nothing to undo."),
        other => panic!("{other:?}"),
    }
    // `all`: the newest entry of any project, for callers that act for the user.
    reg.invoke(UNDO_LAST, json!({"scope": "all"}), &cx_a)
        .unwrap();
    assert_eq!(b_text(), "mine\n");
}

/// ADR 0012: an MCP client (or any sandboxed caller) cannot undo entries of another project,
/// by id or as "undo last", and cannot widen `undo_last` to every project.
#[test]
fn sandboxed_callers_only_undo_entries_of_their_project() {
    let fx = Fx::new();
    std::fs::write(fx.b.join("a.txt"), "two\n").unwrap();
    let reg = fx.core.registry();
    let b_entry = reg
        .invoke(
            "git.stage",
            json!({"paths": ["a.txt"]}),
            &fx.cx(&fx.b, Source::Ui),
        )
        .unwrap()
        .journal_id
        .unwrap();

    for source in [Source::Mcp, Source::Agent] {
        let cx = fx.cx(&fx.a, source);
        let err = reg
            .invoke(UNDO, json!({"entry": b_entry}), &cx)
            .unwrap_err();
        assert!(err.to_string().contains("not in this project"), "{err}");
        assert!(matches!(
            invoke_undo_last(&cx, UndoMode::Check),
            Err(UndoError::NotUndoable(_))
        ));
        let err = reg
            .invoke(UNDO_LAST, json!({"scope": "all"}), &cx)
            .unwrap_err();
        assert!(err.to_string().contains("own project"), "{err}");
        assert_eq!(staged(&fx.b).trim(), "a.txt", "nothing undone ({source:?})");
        assert!(!fx.journal().get(b_entry).unwrap().undone);
    }
    // In its own project the same entry can be undone.
    let cx = fx.cx(&fx.b, Source::Mcp);
    invoke_undo(&cx, b_entry, UndoMode::Check).unwrap();
    assert_eq!(staged(&fx.b), "");
}

/// A project opened on a subfolder of a repository: a sandboxed caller may not undo a write to a
/// file of the repository outside that folder, even though the entry is of the same repository.
#[test]
fn sandboxed_callers_stay_inside_a_subfolder_project() {
    let fx = Fx::new();
    std::fs::write(fx.a.join("a.txt"), "mine\n").unwrap();
    std::fs::write(fx.a.join("sub/b.txt"), "mine\n").unwrap();
    let reg = fx.core.registry();
    let ui = fx.cx(&fx.a, Source::Ui);
    let outside = reg
        .invoke("git.discard", json!({"paths": ["a.txt"]}), &ui)
        .unwrap()
        .journal_id
        .unwrap();
    let inside = reg
        .invoke("git.discard", json!({"paths": ["sub/b.txt"]}), &ui)
        .unwrap()
        .journal_id
        .unwrap();

    let sub = fx.cx(&fx.a.join("sub"), Source::Mcp);
    let err = reg
        .invoke(UNDO, json!({"entry": outside}), &sub)
        .unwrap_err();
    assert!(err.to_string().contains("not in this project"), "{err}");
    assert_eq!(
        std::fs::read_to_string(fx.a.join("a.txt")).unwrap(),
        "one\n"
    );
    invoke_undo(&sub, inside, UndoMode::Check).unwrap();
    assert_eq!(
        std::fs::read_to_string(fx.a.join("sub/b.txt")).unwrap(),
        "mine\n"
    );
    // "Undo last" skips what the caller may not undo instead of picking it.
    match invoke_undo_last(&sub, UndoMode::Check) {
        Err(UndoError::NotUndoable(why)) => assert_eq!(why, "Nothing to undo."),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(fx.a.join("a.txt")).unwrap(),
        "one\n"
    );
}
