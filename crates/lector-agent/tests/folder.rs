//! Agent turns in a folder that is not a git repository (ADR 0011): every write is backed up on
//! its first touch into the folder's shadow repository (created at that first write, never in
//! the folder), a turn that changed files is one undoable journal entry, and undo, its check,
//! "restore anyway" and redo behave as in a repository.

mod common;

use std::fs;

use common::Fx;
use lector_agent::*;
use lector_app::commands::journal_undo::{invoke_undo, invoke_undo_last};
use lector_app::commands::Source;
use lector_app::journal::UndoMode;
use lector_app::CommandContext;
use serde_json::json;

#[test]
fn a_folder_turn_is_backed_up_lazily_and_undone_and_redone_as_one_entry() {
    let fx = need!(Fx::folder());
    let shadow = fx.shadow_path(&fx.root);
    let s = fx.start();
    let t = fx.run(&s, "read a.txt");
    assert!(t.answer().contains("READ:one"), "{}", t.answer());
    assert!(
        !shadow.exists() && !fx.data.join("shadow").exists(),
        "opening the folder, starting the agent and reading create nothing"
    );
    assert!(t.journal_id.is_none());

    let t = fx.run(
        &s,
        "write a.txt one\\nTWO\\nthree\\n\nwrite new/dir/x.txt created\\n\nwrite a.txt again\\n",
    );
    let answer = t.answer();
    assert!(!answer.contains("WRITE-ERROR"), "{answer}");
    assert_eq!(fx.read("a.txt").unwrap(), "again\n");
    assert_eq!(fx.read("new/dir/x.txt").unwrap(), "created\n");
    let paths: Vec<&str> = t.changed_files.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "new/dir/x.txt"]);
    assert!(t.before.is_some() && t.after.is_some());
    assert!(shadow.join("HEAD").is_file(), "created at the first write");
    assert!(!fx.root.join(".git").exists(), "the folder gains no .git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&shadow).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    let turns = fx.entries("agent.turn");
    assert_eq!(turns.len(), 1, "one journal entry for the turn");
    assert!(turns[0].undo.is_some());
    assert_eq!(turns[0].repo.as_deref(), Some(fx.root.as_path()));

    let undo_id = s.undo_turn(t.id, false, Source::Internal).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    assert!(
        fx.read("new/dir/x.txt").is_none(),
        "a created file is removed"
    );
    assert!(
        !fx.root.join("new").exists(),
        "directories the turn created are removed when empty"
    );
    assert!(s.turn(t.id).unwrap().undone);

    let cx = CommandContext::new(fx.core.clone(), Some(fx.project.clone()), Source::Internal);
    invoke_undo(&cx, undo_id, UndoMode::Check).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "again\n");
    assert_eq!(fx.read("new/dir/x.txt").unwrap(), "created\n");
    assert!(!s.turn(t.id).unwrap().undone);
    assert!(!fx.root.join(".git").exists());
    s.stop();
}

#[test]
fn a_folder_undo_refuses_files_edited_since_and_force_backs_them_up() {
    let fx = need!(Fx::folder());
    let s = fx.start();
    let t = fx.run(&s, "write a.txt agent\\n\nwrite b.txt new\\n");
    fs::write(fx.root.join("a.txt"), "user edit\n").unwrap();
    match s.undo_turn(t.id, false, Source::Internal) {
        Err(AgentError::UndoConflict { paths }) => assert_eq!(paths, vec!["a.txt".to_string()]),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.read("a.txt").unwrap(), "user edit\n", "nothing written");
    assert_eq!(fx.read("b.txt").unwrap(), "new\n");
    let undo_id = s.undo_turn(t.id, true, Source::Internal).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    assert!(fx.read("b.txt").is_none());
    // "Restore anyway" backed up what it overwrote: undoing the undo brings the edit back.
    let cx = CommandContext::new(fx.core.clone(), Some(fx.project.clone()), Source::Internal);
    invoke_undo(&cx, undo_id, UndoMode::Check).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "user edit\n");
    assert_eq!(fx.read("b.txt").unwrap(), "new\n");
    s.stop();
}

#[test]
fn a_folder_turn_is_undone_by_the_command_and_by_undo_last() {
    let fx = need!(Fx::folder());
    commands::register(&fx.core);
    let cx = CommandContext::new(fx.core.clone(), Some(fx.project.clone()), Source::Cli);
    let s = fx.start();
    fx.run(&s, "write a.txt first\\n");
    fx.core
        .registry()
        .invoke("agent.undo_turn", json!({}), &cx)
        .unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");

    fx.run(&s, "write src/lib.rs second\\n");
    let undone = invoke_undo_last(&cx, UndoMode::Check).unwrap();
    assert_eq!(fx.read("src/lib.rs").unwrap(), "pub fn f() {}\n");
    let entry = fx.journal().get(undone.undo_of.unwrap()).unwrap();
    assert_eq!(entry.command, "agent.turn", "undo last picks the turn");
    s.stop();
}

#[test]
fn a_folder_undo_restores_the_executable_bit_and_nested_repositories_stay_confined() {
    let fx = need!(Fx::build(
        |base| {
            let sub = base.join("proj/sub");
            fs::create_dir_all(&sub).unwrap();
            common::git(&sub, &["init", "-q", "-b", "main"]);
            fs::write(base.join("proj/run.sh"), "#!/bin/sh\necho one\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(base.join("proj/run.sh"), fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        },
        "proj"
    ));
    assert!(fx.project.repo().is_none());
    let config = fs::read(fx.root.join("sub/.git/config")).unwrap();
    let s = fx.start();
    let t = fx.run(
        &s,
        "write run.sh #!/bin/sh\\necho two\\n\nwrite sub/f.txt in a nested repository\\n\nwrite sub/.git/config evil\\n",
    );
    let answer = t.answer();
    assert_eq!(answer.matches("WRITE-ERROR:").count(), 1, "{answer}");
    assert_eq!(fs::read(fx.root.join("sub/.git/config")).unwrap(), config);
    assert_eq!(fx.read("sub/f.txt").unwrap(), "in a nested repository\n");
    s.undo_turn(t.id, false, Source::Internal).unwrap();
    assert_eq!(fx.read("run.sh").unwrap(), "#!/bin/sh\necho one\n");
    assert!(fx.read("sub/f.txt").is_none());
    assert!(fx.root.join("sub/.git/config").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(fx.root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    s.stop();
}

#[test]
fn a_file_over_64_mib_is_refused_in_a_folder() {
    let fx = need!(Fx::build(
        |base| {
            let big = vec![b'a'; 65 * 1024 * 1024];
            fs::write(base.join("proj/big.txt"), big).unwrap();
        },
        "proj"
    ));
    let s = fx.start();
    let t = fx.run(&s, "write big.txt small\\n\nwrite a.txt fine\\n");
    let answer = t.answer();
    assert_eq!(answer.matches("WRITE-ERROR:").count(), 1, "{answer}");
    assert!(answer.contains("64 MiB"), "{answer}");
    assert_eq!(
        fs::metadata(fx.root.join("big.txt")).unwrap().len(),
        65 * 1024 * 1024
    );
    assert_eq!(fx.read("a.txt").unwrap(), "fine\n");
    assert_eq!(t.written, vec!["a.txt".to_string()]);
    s.stop();
}

/// ADR 0010 item 6 holds in a folder too: the shadow location is blocked (a file where the
/// shadow directory belongs), so no backup can be taken and every write is refused.
#[test]
fn folder_writes_whose_backup_fails_are_refused() {
    let fx = need!(Fx::folder());
    fs::create_dir_all(&fx.data).unwrap();
    fs::write(fx.data.join("shadow"), "not a directory").unwrap();
    let s = fx.start();
    let t = fx.run(
        &s,
        "write a.txt agent\\n\nwrite new.txt agent\\n\nread a.txt",
    );
    let answer = t.answer();
    assert_eq!(answer.matches("WRITE-ERROR:").count(), 2, "{answer}");
    assert!(answer.contains("backup"), "{answer}");
    assert!(answer.contains("READ:one"), "{answer}");
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    assert!(fx.read("new.txt").is_none());
    assert!(t.written.is_empty());
    assert!(fx.entries("agent.turn").is_empty());
    s.stop();
}

/// A folder inside another repository's work tree is a repository path: the project's repository
/// is the outer one, the turn checkpoint covers it, and no folder-keyed shadow is created.
#[test]
fn a_subfolder_of_a_repository_follows_the_repository_rule() {
    let fx = need!(Fx::build(
        |base| {
            // Lector's data directory of the fixture lies in the outer work tree: ignored.
            fs::write(base.join(".gitignore"), "data/\n").unwrap();
            common::git(base, &["init", "-q", "-b", "main"]);
            common::git(base, &["add", "proj"]);
            common::git(base, &["commit", "-q", "-m", "init"]);
        },
        "proj"
    ));
    let outer = fx
        .project
        .repo()
        .expect("the outer repository")
        .workdir()
        .to_path_buf();
    assert_ne!(outer, fx.root);
    let s = fx.start();
    let t = fx.run(&s, "write a.txt agent\\n");
    assert!(t.before.is_some());
    let paths: Vec<&str> = t.changed_files.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        paths,
        ["proj/a.txt"],
        "repository-relative, as in any repository"
    );
    let turns = fx.entries("agent.turn");
    assert_eq!(turns[0].repo.as_deref(), Some(outer.as_path()));
    assert!(!fx.shadow_path(&fx.root).exists(), "no folder-keyed shadow");
    assert!(fx.shadow_path(&outer).exists());
    s.undo_turn(t.id, false, Source::Internal).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    s.stop();
}
