//! ADR 0011: backups in a folder without git are path-scoped. A turn in a folder with 20,000 other
//! files runs exactly the git commands it runs in a folder with none, every one of them against
//! the shadow repository (`--git-dir`), and none lists, walks or stages the folder.
//!
//! `git` is replaced on `PATH` by a logging wrapper for this test binary only (one test, so the
//! process-wide `PATH` change cannot affect another test).

#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::Fx;
use lector_app::commands::Source;

fn real_git() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// Runs one turn (overwrite, create in a new directory), then undoes it; returns the logged git
/// invocations (`cwd<TAB>args`).
fn run_scenario(extra_files: usize, wrapper_dir: &Path) -> (Vec<String>, PathBuf) {
    let fx = need_or_panic(Fx::build(
        |base| {
            for i in 0..extra_files {
                let dir = base.join(format!("proj/many/d{}", i / 500));
                if i % 500 == 0 {
                    fs::create_dir_all(&dir).unwrap();
                }
                fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
            }
        },
        "proj",
    ));
    let log = wrapper_dir.join(format!("log-{extra_files}"));
    std::env::set_var("LECTOR_TEST_GIT_LOG", &log);
    let s = fx.start();
    let t = fx.run(&s, "write a.txt changed\\n\nwrite new/x.txt created\\n");
    assert!(!t.answer().contains("WRITE-ERROR"), "{}", t.answer());
    s.undo_turn(t.id, false, Source::Internal).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    s.stop();
    let lines = fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (lines, fx.root.clone())
}

fn need_or_panic(fx: Option<Fx>) -> Fx {
    fx.expect("python3 is needed for this test")
}

#[test]
fn folder_backups_cost_the_same_in_a_folder_with_20000_files() {
    if common::python().is_none() {
        eprintln!("python3 not found: skipping");
        return;
    }
    let wrapper = tempfile::tempdir().unwrap();
    let git = real_git();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\t%s\\n' \"$PWD\" \"$*\" >> \"$LECTOR_TEST_GIT_LOG\"\nexec '{}' \"$@\"\n",
        git.display()
    );
    let path = wrapper.path().join("git");
    fs::write(&path, script).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![wrapper.path().to_path_buf()];
    paths.extend(std::env::split_paths(&old_path));
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());

    let (empty, _) = run_scenario(0, wrapper.path());
    let (many, root) = run_scenario(20_000, wrapper.path());
    assert!(
        !empty.is_empty(),
        "the wrapper logged nothing: is it on PATH?"
    );
    assert_eq!(
        empty.len(),
        many.len(),
        "git invocations differ:\n{empty:#?}\nvs\n{many:#?}"
    );
    eprintln!("git invocations for one turn and its undo: {}", many.len());
    for line in &many {
        let (cwd, args) = line.split_once('\t').unwrap_or(("", line));
        // Every command names the shadow: `--git-dir` (which turns discovery off; such commands
        // may run in the folder, where `hash-object --stdin-paths` resolves the relative paths),
        // or `init --bare <shadow>` creating it, which runs outside the folder.
        if args.contains(" init --bare ") {
            assert!(
                !Path::new(cwd).starts_with(&root),
                "git init ran in the folder"
            );
        } else {
            assert!(
                args.contains("--git-dir"),
                "a git command without the shadow named explicitly (cwd {cwd}): {args}"
            );
        }
        for walk in [
            "ls-files",
            "status",
            " add ",
            "check-ignore",
            "rev-parse --show-object-format",
        ] {
            assert!(!format!(" {args} ").contains(walk), "{walk:?} ran: {args}");
        }
    }
}
