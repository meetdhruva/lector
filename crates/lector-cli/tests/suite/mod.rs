//! The CLI integration suite. It is included by two test crates (`lector-cli` and `lector`)
//! that each define `const BIN: &str` (the binary under test) in their crate root.
//!
//! Every test builds its own hermetic git repository (no global/system git config, no
//! auto-gc) and points `LECTOR_DATA_DIR` / `LECTOR_CONFIG_DIR` at temp folders, so the
//! user's real state is never read or written.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

use crate::BIN;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Fx {
    _tmp: TempDir,
    root: PathBuf,
    data: PathBuf,
    config: PathBuf,
}

/// Repository `main` with one commit (`src/lib.rs`, `README.md`), then: `src/lib.rs`
/// modified (public signature changed, `unwrap` added), `src/new.rs` staged, `README.md`
/// edited, `notes.txt` untracked.
fn fixture() -> Fx {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = dunce::canonicalize(tmp.path()).expect("canonical");
    let root = base.join("repo");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "gc.auto", "0"]);
    git(&root, &["config", "maintenance.auto", "false"]);
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn a() {}\npub fn b() -> u32 {\n    1\n}\n",
    )
    .expect("write");
    std::fs::write(root.join("README.md"), "# demo\n").expect("write");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn a(x: u8) {}\npub fn b() -> u32 {\n    let v: Option<u32> = None;\n    v.unwrap()\n}\n",
    )
    .expect("write");
    std::fs::write(root.join("src/new.rs"), "pub fn fresh() {}\n").expect("write");
    git(&root, &["add", "src/new.rs"]);
    std::fs::write(root.join("README.md"), "# demo\nmore\n").expect("write");
    std::fs::write(root.join("notes.txt"), "todo\n").expect("write");
    Fx {
        data: base.join("data"),
        config: base.join("config"),
        root,
        _tmp: tmp,
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}): {}\nstderr: {}",
                self.stdout, self.stderr
            )
        })
    }
}

impl Fx {
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&self.root)
            .env("LECTOR_DATA_DIR", &self.data)
            .env("LECTOR_CONFIG_DIR", &self.config)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("RUST_LOG")
            .env_remove("NO_COLOR")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        cmd
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = self.command(args).output().expect("binary runs");
        Run {
            code: out.status.code().expect("exit code"),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn ok_json(&self, args: &[&str]) -> Value {
        let r = self.run(args);
        assert_eq!(r.code, 0, "{args:?} failed: {}", r.stderr);
        r.json()
    }
}

fn file<'a>(doc: &'a Value, path: &str) -> &'a Value {
    doc["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("{path} not in {doc}"))
}

// ------------------------------------------------------------------------ status

#[test]
fn status_json_reports_branch_and_counts() {
    let fx = fixture();
    let v = fx.ok_json(&["status", "--json"]);
    assert_eq!(v["schema"], 1);
    assert_eq!(v["branch"], "main");
    assert_eq!(v["detached"], false);
    assert_eq!(v["ahead"], 0);
    assert_eq!(v["behind"], 0);
    assert_eq!(v["operation"], Value::Null);
    assert_eq!(v["clean"], false);
    assert_eq!(
        v["counts"],
        json!({"staged": 1, "unstaged": 2, "untracked": 1, "conflicted": 0})
    );
    assert_eq!(v["head"].as_str().expect("head").len(), 40);
    assert_eq!(Path::new(v["root"].as_str().expect("root")), fx.root);
}

#[test]
fn status_takes_a_path_and_prints_compact_human_output() {
    let fx = fixture();
    let r = fx.run(&["status", fx.root.join("src").to_str().expect("utf-8")]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines[0], "main", "{}", r.stdout);
    assert_eq!(lines[1], "staged 1 · unstaged 2 · untracked 1");
    assert_eq!(lines.len(), 2);
    assert!(
        !r.stdout.contains('\x1b'),
        "no colors when stdout is not a terminal"
    );
}

#[test]
fn status_of_a_clean_repository_and_with_an_operation() {
    let fx = fixture();
    git(&fx.root, &["add", "-A"]);
    git(&fx.root, &["commit", "-q", "-m", "all"]);
    let r = fx.run(&["status"]);
    assert_eq!(r.stdout, "main\nclean\n");
    assert_eq!(fx.ok_json(&["status", "--json"])["clean"], true);

    // A merge that conflicts shows up as an operation with conflicted paths.
    git(&fx.root, &["checkout", "-q", "-b", "side"]);
    std::fs::write(fx.root.join("README.md"), "side\n").expect("write");
    git(&fx.root, &["commit", "-q", "-am", "side"]);
    git(&fx.root, &["checkout", "-q", "main"]);
    std::fs::write(fx.root.join("README.md"), "main\n").expect("write");
    git(&fx.root, &["commit", "-q", "-am", "main"]);
    let merge = Command::new("git")
        .arg("-C")
        .arg(&fx.root)
        .args(["merge", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git merge");
    assert!(!merge.status.success(), "the merge must conflict");
    let v = fx.ok_json(&["status", "--json"]);
    assert_eq!(v["operation"]["kind"], "merge");
    assert_eq!(v["counts"]["conflicted"], 1);
    assert_eq!(v["clean"], false);
    let human = fx.run(&["status"]).stdout;
    assert!(
        human.contains("Merging") && human.contains("1 conflicted"),
        "{human}"
    );
}

#[test]
fn status_outside_a_repository_is_an_error() {
    let fx = fixture();
    let plain = fx._tmp.path().join("plain");
    std::fs::create_dir_all(&plain).expect("mkdir");
    let r = fx.run(&["status", plain.to_str().expect("utf-8")]);
    assert_eq!(r.code, 1);
    assert!(r.stdout.is_empty());
    assert!(r.stderr.starts_with("lector: error:"), "{}", r.stderr);
    let r = fx.run(&["status", "/definitely/not/here"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
}

#[test]
fn status_is_fast_and_never_starts_the_gui() {
    let fx = fixture();
    fx.run(&["status"]); // warm the file cache
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let r = fx.run(&["status"]);
        assert_eq!(r.code, 0, "{}", r.stderr);
        best = best.min(t.elapsed());
    }
    // The budget is 100 ms in a release build; debug builds on a loaded CI box get slack.
    // Starting the GUI (no display here) would fail or take seconds.
    assert!(best < Duration::from_millis(1500), "status took {best:?}");
}

// ----------------------------------------------------------------------- changes

#[test]
fn changes_json_ranks_files_by_risk() {
    let fx = fixture();
    let v = fx.ok_json(&["changes", "--json"]);
    assert_eq!(v["schema"], 1);
    assert_eq!(v["changeset"], "worktree");
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 4, "{v}");
    assert_eq!(files[0]["path"], "src/lib.rs", "riskiest first: {v}");
    let lib = file(&v, "src/lib.rs");
    assert_eq!(lib["kind"], "modified");
    assert_eq!(lib["added"], 3);
    assert_eq!(lib["removed"], 2);
    let risk = &lib["risk"];
    assert_eq!(risk["level"], "high");
    let score = risk["score"].as_f64().expect("score");
    assert!((0.55..=1.0).contains(&score), "{score}");
    let kinds: Vec<&str> = risk["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|r| r["kind"].as_str().expect("kind"))
        .collect();
    assert!(kinds.contains(&"public_api_changed"), "{kinds:?}");
    assert!(kinds.contains(&"panic_added"), "{kinds:?}");
    assert!(risk["reasons"][0]["message"]
        .as_str()
        .is_some_and(|m| !m.is_empty()));
    let scores: Vec<f64> = files
        .iter()
        .map(|f| f["risk"]["score"].as_f64().expect("score"))
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "sorted: {scores:?}"
    );
    assert_eq!(file(&v, "src/new.rs")["kind"], "added");
    assert_eq!(file(&v, "notes.txt")["kind"], "added");
    assert_eq!(v["totals"]["files"], 4);
    assert_eq!(v["totals"]["high"], 1);
    assert_eq!(v["totals"]["added"], 6);
    assert_eq!(v["totals"]["removed"], 2);
}

#[test]
fn changes_human_output_is_compact() {
    let fx = fixture();
    let r = fx.run(&["changes"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 5, "{}", r.stdout);
    assert!(
        lines[0].starts_with("Working tree · 4 files, +6 -2"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("1 high"), "{}", lines[0]);
    assert!(lines[1].starts_with("HIGH "), "{}", lines[1]);
    assert!(
        lines[1].contains("src/lib.rs") && lines[1].contains("+3 -2"),
        "{}",
        lines[1]
    );
    assert!(
        lines[1].contains("signature"),
        "top reason shown: {}",
        lines[1]
    );
    assert!(!r.stdout.contains('\x1b'));
}

#[test]
fn changes_flags_select_the_changeset() {
    let fx = fixture();
    let staged = fx.ok_json(&["changes", "--staged", "--json"]);
    assert_eq!(staged["changeset"], "staged");
    assert_eq!(staged["files"].as_array().expect("files").len(), 1);
    assert_eq!(staged["files"][0]["path"], "src/new.rs");

    let unstaged = fx.ok_json(&["changes", "--unstaged", "--json", "--no-risk"]);
    assert_eq!(unstaged["changeset"], "unstaged");
    assert_eq!(unstaged["risk_skipped"], "--no-risk");
    assert!(unstaged["files"][0].get("risk").is_none());
    let paths: Vec<&str> = unstaged["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("p"))
        .collect();
    assert_eq!(
        paths,
        vec!["README.md", "src/lib.rs"],
        "by path without risk"
    );

    // A feature branch against main, and an explicit range.
    git(&fx.root, &["add", "-A"]);
    git(&fx.root, &["commit", "-q", "-m", "wip"]);
    git(&fx.root, &["branch", "base", "HEAD~1"]);
    let branch = fx.ok_json(&["changes", "--branch", "base", "--json"]);
    assert_eq!(branch["changeset"], "branch:base...main");
    assert_eq!(branch["files"].as_array().expect("files").len(), 4);
    let range = fx.ok_json(&["changes", "--range", "HEAD~1..HEAD", "--json"]);
    assert_eq!(range["files"].as_array().expect("files").len(), 4);
    let clean = fx.run(&["changes"]);
    assert_eq!(clean.stdout, "No changes in Working tree.\n");
}

#[test]
fn changes_usage_errors_exit_2() {
    let fx = fixture();
    let r = fx.run(&["changes", "--staged", "--unstaged"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stdout.is_empty());
    assert_eq!(fx.run(&["changes", "--nope"]).code, 2);
    assert_eq!(
        fx.run(&["changes", "--range", "HEAD"]).code,
        1,
        "not a range: a runtime error"
    );
}

#[test]
fn changes_in_another_directory_with_dash_c() {
    let fx = fixture();
    let r = Command::new(BIN)
        .args(["changes", "--json", "-C"])
        .arg(&fx.root)
        .current_dir(fx._tmp.path())
        .env("LECTOR_DATA_DIR", &fx.data)
        .env("LECTOR_CONFIG_DIR", &fx.config)
        .output()
        .expect("runs");
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let v: Value = serde_json::from_slice(&r.stdout).expect("json");
    assert_eq!(v["files"].as_array().expect("files").len(), 4);
}

// -------------------------------------------------------------------------- diff

#[test]
fn diff_json_has_hunks() {
    let fx = fixture();
    let v = fx.ok_json(&["diff", "worktree", "--file", "src/lib.rs", "--json"]);
    assert_eq!(v["schema"], 1);
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "src/lib.rs");
    let hunks = files[0]["hunks"].as_array().expect("hunks");
    assert!(!hunks.is_empty());
    assert_eq!(hunks[0]["lines"][0], "-pub fn a() {}");
    assert_eq!(hunks[0]["lines"][1], "+pub fn a(x: u8) {}");
    assert!(files[0].get("moved").is_none(), "only with --moved");

    let staged = fx.ok_json(&["diff", "staged", "--json"]);
    assert_eq!(staged["files"][0]["path"], "src/new.rs");
}

#[test]
fn diff_human_output_is_a_unified_diff() {
    let fx = fixture();
    let r = fx.run(&["diff", "unstaged", "--file", "README.md"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        "Unstaged changes\n\nREADME.md (modified, +1 -0)\n@@ -1 +1,2 @@\n # demo\n+more\n"
    );
}

#[test]
fn diff_accepts_revisions_ranges_and_keys() {
    let fx = fixture();
    git(&fx.root, &["add", "-A"]);
    git(&fx.root, &["commit", "-q", "-m", "second"]);
    for spec in [
        "HEAD",
        "commit:HEAD",
        "HEAD~1..HEAD",
        "HEAD~1...HEAD",
        "range:HEAD~1..HEAD",
    ] {
        let v = fx.ok_json(&["diff", spec, "--json"]);
        assert!(
            !v["files"].as_array().expect("files").is_empty(),
            "{spec}: {v}"
        );
    }
    let r = fx.run(&["diff", "no-such-rev"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("not a changeset"), "{}", r.stderr);
    let r = fx.run(&["diff", "HEAD", "--file", "nope.rs"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("not changed"), "{}", r.stderr);
    assert_eq!(fx.run(&["diff"]).code, 2, "the changeset is required");
}

/// `a.rs` and `b.rs` hold the same three functions; `alpha` moves to the end of `a.rs` and
/// out of `b.rs` into the new `c.rs`.
fn moved_fixture() -> Fx {
    let fx = fixture();
    git(&fx.root, &["add", "-A"]);
    git(&fx.root, &["commit", "-q", "-m", "all"]);
    let alpha = "fn alpha(input: &str) -> usize {\n    let trimmed = input.trim();\n    let count = trimmed.len();\n    count * 2\n}\n";
    let rest = "\nfn middle() {\n    println!(\"middle\");\n}\n\nfn omega() {\n    println!(\"omega\");\n}\n";
    std::fs::write(fx.root.join("a.rs"), format!("{alpha}{rest}")).expect("write");
    std::fs::write(fx.root.join("b.rs"), format!("{alpha}{rest}")).expect("write");
    git(&fx.root, &["add", "a.rs", "b.rs"]);
    git(&fx.root, &["commit", "-q", "-m", "add"]);
    std::fs::write(
        fx.root.join("a.rs"),
        format!("{}\n{alpha}", rest.trim_start_matches('\n')),
    )
    .expect("write");
    std::fs::write(fx.root.join("b.rs"), rest.trim_start_matches('\n')).expect("write");
    std::fs::write(fx.root.join("c.rs"), alpha).expect("write");
    fx
}

#[test]
fn diff_moved_marks_blocks_within_and_between_files() {
    let fx = moved_fixture();
    let v = fx.ok_json(&["diff", "worktree", "--moved", "--json", "--file", "a.rs"]);
    assert_eq!(
        v["files"].as_array().expect("files").len(),
        1,
        "only a.rs is shown"
    );
    let moved = v["files"][0]["moved"].as_array().expect("moved");
    assert_eq!(moved.len(), 1, "{v}");
    assert_eq!(moved[0]["old"], json!([0, 4]));
    assert_eq!(moved[0]["new"][0].as_u64(), Some(8));
    assert_eq!(moved[0]["reindented"], false);
    let moves = v["moves"].as_array().expect("moves");
    assert_eq!(
        moves.len(),
        1,
        "b.rs -> c.rs is found although neither file is shown: {v}"
    );
    assert_eq!(moves[0]["from"]["path"], "b.rs");
    assert_eq!(moves[0]["to"]["path"], "c.rs");

    let plain = fx.ok_json(&["diff", "worktree", "--json", "--file", "a.rs"]);
    assert!(plain["files"][0].get("moved").is_none());
    assert!(plain.get("moves").is_none());

    let human = fx.run(&["diff", "worktree", "--moved"]);
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(
        human
            .stdout
            .contains("moved within file: lines 1-4 -> 9-12"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("moved to c.rs:1-4"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("moved from b.rs:1-4"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("<    let trimmed = input.trim();"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains(">    let trimmed = input.trim();"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("-}\n") && human.stdout.contains("+}\n"),
        "the closing brace is a real change"
    );
}

// ------------------------------------------------------------------------ colors

#[test]
fn colors_only_when_asked_or_on_a_terminal() {
    let fx = fixture();
    let never = fx.run(&["changes"]);
    assert!(!never.stdout.contains('\x1b'));
    let always = fx.run(&["--color", "always", "changes"]);
    assert!(always.stdout.contains("\x1b["), "{}", always.stdout);
    let mut cmd = fx.command(&["--color", "always", "status"]);
    cmd.env("NO_COLOR", "1");
    // `--color always` is an explicit request and wins over NO_COLOR; `auto` honours it.
    let out = cmd.output().expect("runs");
    assert!(String::from_utf8_lossy(&out.stdout).contains("\x1b["));
    let mut cmd = fx.command(&["--color", "auto", "status"]);
    cmd.env("NO_COLOR", "1");
    let out = cmd.output().expect("runs");
    assert!(!String::from_utf8_lossy(&out.stdout).contains('\x1b'));
    // JSON never has colors.
    let json = fx.run(&["--color", "always", "status", "--json"]);
    assert!(!json.stdout.contains('\x1b'));
}

// ---------------------------------------------------------------------- commands

#[test]
fn commands_lists_the_registry_with_schemas() {
    let fx = fixture();
    let v = fx.ok_json(&["commands", "--json"]);
    let commands = v["commands"].as_array().expect("commands");
    let by_id = |id: &str| {
        commands
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing"))
    };
    let list = by_id("git.list_changes");
    assert_eq!(list["args"]["type"], "object");
    assert!(list["args"]["properties"]["changeset"].is_object());
    assert!(list.get("writes").is_none());
    let stage = by_id("git.stage");
    assert_eq!(stage["writes"], true);
    assert_eq!(by_id("git.discard")["destructive"], true);
    assert!(commands.len() >= 20, "{}", commands.len());

    let git_only = fx.ok_json(&["commands", "--json", "--category", "git"]);
    assert!(git_only["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .all(|c| c["category"] == "Git"));

    let human = fx.run(&["commands"]);
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(
        human.stdout.contains("git.list_changes"),
        "{}",
        human.stdout
    );
    assert!(
        human.stdout.contains("git.stage") && human.stdout.contains("[writes]"),
        "{}",
        human.stdout
    );
    assert!(human.stdout.contains("[writes, destructive]"));
}

// --------------------------------------------------------------------------- run

#[test]
fn run_executes_read_commands() {
    let fx = fixture();
    let v = fx.ok_json(&[
        "run",
        "git.list_changes",
        "--args",
        "{\"changeset\":\"staged\"}",
        "--json",
    ]);
    assert_eq!(v["ok"], true);
    assert_eq!(v["command"], "git.list_changes");
    assert_eq!(v["data"]["files"][0]["path"], "src/new.rs");
    assert!(v.get("journal_id").is_none(), "reads are not journaled");

    let human = fx.run(&["run", "git.list_changes"]);
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(
        human.stdout.starts_with("Working tree: 4 files\n{"),
        "{}",
        human.stdout
    );

    // Arguments from a file and from stdin.
    let args_file = fx._tmp.path().join("args.json");
    std::fs::write(&args_file, "{\"path\":\"src/lib.rs\"}").expect("write");
    let at = format!("@{}", args_file.display());
    let v = fx.ok_json(&["run", "document.read", "--args", &at, "--json"]);
    assert!(v["data"]["text"]
        .as_str()
        .expect("text")
        .contains("pub fn a(x: u8)"));
    let mut child = fx
        .command(&["run", "document.read", "--args", "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"{\"path\":\"README.md\"}")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["data"]["text"], "# demo\nmore\n");
}

#[test]
fn run_errors() {
    let fx = fixture();
    let r = fx.run(&["run", "no.such_command"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("unknown command"), "{}", r.stderr);
    let r = fx.run(&["run", "git.list_changes", "--args", "{nope"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("not valid JSON"), "{}", r.stderr);
    let r = fx.run(&["run", "git.list_changes", "--args", "[1]"]);
    assert_eq!(r.code, 1);
    let r = fx.run(&["run", "git.list_changes", "--args", "{\"changeset\": 3}"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("invalid arguments"), "{}", r.stderr);
    assert_eq!(fx.run(&["run"]).code, 2);
}

#[test]
fn run_refuses_writes_without_allow_writes() {
    let fx = fixture();
    let r = fx.run(&["run", "git.stage", "--args", "{\"paths\":[\"README.md\"]}"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("--allow-writes"), "{}", r.stderr);
    assert_eq!(
        git(&fx.root, &["diff", "--cached", "--name-only"]).trim(),
        "src/new.rs",
        "index untouched"
    );
    let activity = fx.ok_json(&["activity", "--json"]);
    assert_eq!(
        activity["entries"],
        json!([]),
        "refused writes are not journaled"
    );
}

#[test]
fn run_writes_are_journaled_and_undoable() {
    let fx = fixture();
    let v = fx.ok_json(&[
        "run",
        "git.stage",
        "--args",
        "{\"paths\":[\"README.md\"]}",
        "--allow-writes",
        "--json",
    ]);
    assert_eq!(v["ok"], true);
    assert_eq!(v["undoable"], true);
    let id = v["journal_id"].as_u64().expect("journal id");
    let staged = git(&fx.root, &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("README.md"), "{staged}");

    let activity = fx.ok_json(&["activity", "--json"]);
    let entries = activity["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], id);
    assert_eq!(entries[0]["command"], "git.stage");
    assert_eq!(entries[0]["source"], "cli");
    assert_eq!(entries[0]["undoable"], true);
    assert_eq!(entries[0]["undone"], false);
    assert_eq!(entries[0]["args"], json!({"paths": ["README.md"]}));
    assert!(entries[0]["time"]
        .as_str()
        .is_some_and(|t| t.ends_with('Z') && t.len() == 20));

    let human = fx.run(&["activity"]);
    assert!(
        human.stdout.contains("git.stage")
            && human.stdout.contains("[cli]")
            && human.stdout.contains("(undoable)"),
        "{}",
        human.stdout
    );

    let undo = fx.run(&["undo", &id.to_string()]);
    assert_eq!(undo.code, 0, "{}", undo.stderr);
    assert!(
        undo.stdout.starts_with(&format!("Undid #{id}:")),
        "{}",
        undo.stdout
    );
    assert_eq!(
        git(&fx.root, &["diff", "--cached", "--name-only"]).trim(),
        "src/new.rs"
    );
    let after = fx.ok_json(&["activity", "--json"]);
    let entries = after["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0]["source"], "cli",
        "the undo is attributed to its caller: {after}"
    );
    assert_eq!(entries[0]["undo_of"], id);
    let original = entries.iter().find(|e| e["id"] == id).expect("original");
    assert_eq!(original["undone"], true);

    let again = fx.run(&["undo", &id.to_string()]);
    assert_eq!(again.code, 1);
    assert!(again.stderr.contains("already undone"), "{}", again.stderr);
    let missing = fx.run(&["undo", "9999"]);
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("no activity entry"),
        "{}",
        missing.stderr
    );
    assert_eq!(fx.run(&["undo", "abc"]).code, 2);
}

#[test]
fn undo_of_a_discard_restores_the_file_and_refuses_after_new_edits() {
    let fx = fixture();
    let v = fx.ok_json(&[
        "run",
        "git.discard",
        "--args",
        "{\"paths\":[\"README.md\"]}",
        "--allow-writes",
        "--json",
    ]);
    let id = v["journal_id"].as_u64().expect("journal id");
    assert_eq!(
        std::fs::read_to_string(fx.root.join("README.md")).expect("read"),
        "# demo\n"
    );

    // The user edits the file again: undo must not silently throw that away.
    std::fs::write(fx.root.join("README.md"), "# newer\n").expect("write");
    let r = fx.run(&["undo", &id.to_string()]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("--force"), "{}", r.stderr);
    assert_eq!(
        std::fs::read_to_string(fx.root.join("README.md")).expect("read"),
        "# newer\n"
    );

    let r = fx.run(&["undo", &id.to_string(), "--force"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        std::fs::read_to_string(fx.root.join("README.md")).expect("read"),
        "# demo\nmore\n"
    );
}

#[test]
fn activity_defaults_to_the_current_repository() {
    let fx = fixture();
    fx.ok_json(&[
        "run",
        "git.stage",
        "--args",
        "{\"paths\":[\"README.md\"]}",
        "--allow-writes",
        "--json",
    ]);
    let other = fx._tmp.path().join("other");
    std::fs::create_dir_all(&other).expect("mkdir");
    git(&other, &["init", "-q", "-b", "main"]);
    let in_other = Command::new(BIN)
        .args(["activity", "--json"])
        .current_dir(&other)
        .env("LECTOR_DATA_DIR", &fx.data)
        .env("LECTOR_CONFIG_DIR", &fx.config)
        .output()
        .expect("runs");
    let v: Value = serde_json::from_slice(&in_other.stdout).expect("json");
    assert_eq!(v["entries"], json!([]));
    let all = Command::new(BIN)
        .args(["activity", "--json", "--all", "-n", "5"])
        .current_dir(&other)
        .env("LECTOR_DATA_DIR", &fx.data)
        .env("LECTOR_CONFIG_DIR", &fx.config)
        .output()
        .expect("runs");
    let v: Value = serde_json::from_slice(&all.stdout).expect("json");
    assert_eq!(v["entries"].as_array().expect("entries").len(), 1);
    assert_eq!(
        fx.run(&["activity", "--limit", "0"]).stdout,
        "No recorded activity.\n"
    );
}

// --------------------------------------------------------------------------- mcp

struct McpSession {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpSession {
    fn start(fx: &Fx, extra: &[&str]) -> McpSession {
        let mut args = vec!["mcp"];
        args.extend_from_slice(extra);
        let mut child = fx
            .command(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lector mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        McpSession {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write");
        stdin.flush().expect("flush");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read");
        assert!(n > 0, "the server closed stdout");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout carries only protocol JSON ({e}): {line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let reply = self.read_line();
        assert_eq!(reply["id"], id, "{reply}");
        assert!(
            reply.get("error").is_none() || method == "tools/call",
            "{reply}"
        );
        reply
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": tool, "arguments": args}))
    }

    fn handshake(&mut self) -> Value {
        let init = self.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "cli-test", "version": "1"}}),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }

    fn finish(mut self) -> String {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait");
        assert!(
            status.success(),
            "the server exits cleanly when stdin closes: {status:?}"
        );
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut self.stdout, &mut rest).expect("drain");
        rest
    }
}

#[test]
fn mcp_server_speaks_json_rpc_over_stdio() {
    let fx = fixture();
    let mut mcp = McpSession::start(&fx, &[]);
    let init = mcp.handshake();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "lector");

    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert!(names.contains(&"git_list_changes"), "{names:?}");
    assert!(
        !names.contains(&"git_stage"),
        "read-only by default: {names:?}"
    );

    let changes = mcp.call("git_list_changes", json!({"changeset": "worktree"}));
    assert_eq!(changes["result"]["isError"], false, "{changes}");
    let paths: Vec<&str> = changes["result"]["structuredContent"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert!(paths.contains(&"src/lib.rs"), "{paths:?}");

    let rejected = mcp.call("git_stage", json!({"paths": ["README.md"]}));
    assert_eq!(rejected["result"]["isError"], true);
    assert!(rejected["result"]["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("--allow-writes"));

    let rest = mcp.finish();
    assert!(rest.is_empty(), "nothing else on stdout: {rest:?}");
    assert_eq!(
        git(&fx.root, &["diff", "--cached", "--name-only"]).trim(),
        "src/new.rs"
    );
    assert_eq!(fx.ok_json(&["activity", "--json"])["entries"], json!([]));
}

#[test]
fn mcp_writes_with_allow_writes_are_journaled_as_mcp() {
    let fx = fixture();
    let mut mcp = McpSession::start(&fx, &["--allow-writes"]);
    mcp.handshake();
    let tools = mcp.request("tools/list", json!({}));
    assert!(tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .any(|t| t["name"] == "git_stage"));
    let staged = mcp.call("git_stage", json!({"paths": ["README.md"]}));
    assert_eq!(staged["result"]["isError"], false, "{staged}");
    let journal_id = staged["result"]["_meta"]["lector"]["journalId"]
        .as_u64()
        .expect("journal id");
    mcp.finish();

    assert!(git(&fx.root, &["diff", "--cached", "--name-only"]).contains("README.md"));
    let activity = fx.ok_json(&["activity", "--json"]);
    let entry = &activity["entries"][0];
    assert_eq!(entry["id"], journal_id);
    assert_eq!(entry["source"], "mcp");
    assert_eq!(entry["command"], "git.stage");
    // The human can take it back from the CLI.
    assert_eq!(fx.run(&["undo", &journal_id.to_string()]).code, 0);
    assert_eq!(
        git(&fx.root, &["diff", "--cached", "--name-only"]).trim(),
        "src/new.rs"
    );
}

#[test]
fn mcp_takes_the_project_path_and_survives_garbage() {
    let fx = fixture();
    let mut cmd = Command::new(BIN);
    cmd.args(["mcp"])
        .arg(&fx.root)
        .current_dir(fx._tmp.path())
        .env("LECTOR_DATA_DIR", &fx.data)
        .env("LECTOR_CONFIG_DIR", &fx.config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(stdin, "garbage").expect("write");
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "git_list_changes", "arguments": {}}})).expect("write");
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    assert_eq!(lines[0]["error"]["code"], -32700);
    assert_eq!(lines[1]["result"]["isError"], false, "{}", lines[1]);
    let banner = String::from_utf8_lossy(&out.stderr);
    assert!(banner.contains("lector mcp: serving"), "{banner}");
}

// ------------------------------------------------------------------------- misc

#[test]
fn version_and_help() {
    let fx = fixture();
    let r = fx.run(&["--version"]);
    assert_eq!(r.code, 0);
    assert!(r.stdout.starts_with("lector "), "{}", r.stdout);
    let r = fx.run(&["status", "--help"]);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("--json"), "{}", r.stdout);
    let r = fx.run(&["--help"]);
    assert_eq!(r.code, 0);
    for sub in [
        "status", "changes", "diff", "commands", "run", "activity", "undo", "mcp",
    ] {
        assert!(r.stdout.contains(sub), "{sub} in help: {}", r.stdout);
    }
}

#[test]
fn closed_stdout_is_not_an_error() {
    let fx = fixture();
    let mut child = fx
        .command(&["commands"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    drop(child.stdout.take()); // the reader goes away immediately, like `| head -0`
    let status = child.wait().expect("wait");
    assert!(status.success(), "{status:?}");
}
