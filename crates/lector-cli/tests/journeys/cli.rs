//! Journey through the `lector-cli` binary: `status`, `changes`, `diff` with the stable
//! `--json` schemas, then a write through `run`, the activity log and `undo`.
//!
//! Field names asserted here are the documented, stable ones (fields are only ever added).

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use crate::fixture::{git_in, norm, same_path, thirty_functions, Repo};

const BIN: &str = env!("CARGO_BIN_EXE_lector-cli");

struct Cli<'a> {
    repo: &'a Repo,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Cli<'_> {
    fn run(&self, args: &[&str]) -> Out {
        let out = Command::new(BIN)
            .args(args)
            .current_dir(&self.repo.root)
            .env("LECTOR_DATA_DIR", &self.repo.data)
            .env("LECTOR_CONFIG_DIR", &self.repo.config)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("RUST_LOG")
            .env_remove("NO_COLOR")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .expect("lector-cli runs");
        Out {
            code: out.status.code().expect("exit code"),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert_eq!(out.code, 0, "{args:?}: {}", out.stderr);
        serde_json::from_str(&out.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: not JSON ({e}): {}", out.stdout))
    }
}

/// The object has (at least) these keys.
fn has_keys(v: &Value, keys: &[&str]) {
    let obj = v
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"));
    for key in keys {
        assert!(obj.contains_key(*key), "missing `{key}` in {v}");
    }
}

fn repo() -> Repo {
    let repo = Repo::init("cli");
    repo.write("src/lib.rs", &thirty_functions());
    repo.write("README.md", "# cli\n");
    repo.commit_all("init");
    repo.write(
        "src/lib.rs",
        &thirty_functions()
            .replace("pub fn f3() -> u32 { 3 }", "pub fn f3(x: u8) -> u32 { 33 }")
            .replace("pub fn f27() -> u32 { 27 }", "pub fn f27() -> u32 { 270 }"),
    );
    repo.write("src/new.rs", "pub fn fresh() {}\n");
    git_in(&repo.root, &["add", "src/new.rs"]);
    repo.write("notes.txt", "todo\n");
    repo
}

#[test]
fn status_changes_and_diff_json_have_the_stable_fields() {
    let repo = repo();
    let cli = Cli { repo: &repo };

    let status = cli.json(&["status", "--json"]);
    has_keys(
        &status,
        &[
            "schema",
            "root",
            "branch",
            "detached",
            "head",
            "head_short",
            "upstream",
            "ahead",
            "behind",
            "operation",
            "counts",
            "clean",
        ],
    );
    assert_eq!(status["schema"], 1);
    assert_eq!(status["branch"], "main");
    assert_eq!(status["detached"], false);
    assert_eq!(status["clean"], false);
    has_keys(
        &status["counts"],
        &["staged", "unstaged", "untracked", "conflicted"],
    );
    assert_eq!(status["counts"]["staged"], 1);
    assert_eq!(status["counts"]["unstaged"], 1);
    assert_eq!(status["counts"]["untracked"], 1);
    assert_eq!(status["head"].as_str().expect("head").len(), 40);
    assert!(same_path(
        &PathBuf::from(status["root"].as_str().expect("root")),
        &repo.root
    ));

    let changes = cli.json(&["changes", "--json"]);
    has_keys(&changes, &["schema", "changeset", "files", "totals"]);
    assert_eq!(changes["schema"], 1);
    assert_eq!(changes["changeset"], "worktree");
    let files = changes["files"].as_array().expect("files");
    assert_eq!(files.len(), 3, "{changes}");
    for f in files {
        has_keys(f, &["path", "kind", "added", "removed", "risk"]);
        has_keys(&f["risk"], &["level", "score", "reasons"]);
    }
    has_keys(&changes["totals"], &["files", "high", "added", "removed"]);
    assert_eq!(changes["totals"]["files"], 3);
    let scores: Vec<f64> = files
        .iter()
        .map(|f| f["risk"]["score"].as_f64().expect("score"))
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "riskiest first: {scores:?}"
    );
    assert_eq!(
        files[0]["path"], "src/lib.rs",
        "the signature change ranks first"
    );

    let diff = cli.json(&["diff", "worktree", "--file", "src/lib.rs", "--json"]);
    has_keys(&diff, &["schema", "files"]);
    let file = &diff["files"][0];
    has_keys(file, &["path", "kind", "hunks"]);
    assert_eq!(file["path"], "src/lib.rs");
    let hunks = file["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 2);
    has_keys(&hunks[0], &["header", "lines"]);
    let first: Vec<&str> = hunks[0]["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(first.contains(&"-pub fn f3() -> u32 { 3 }"), "{first:?}");
    assert!(
        first.contains(&"+pub fn f3(x: u8) -> u32 { 33 }"),
        "{first:?}"
    );
    let staged = cli.json(&["diff", "staged", "--json"]);
    assert_eq!(staged["files"][0]["path"], "src/new.rs");
}

#[test]
fn write_through_run_is_journaled_listed_by_activity_and_undone() {
    let repo = repo();
    let cli = Cli { repo: &repo };

    // Writes need an explicit --allow-writes.
    let refused = cli.run(&[
        "run",
        "git.stage_hunk",
        "--args",
        r#"{"path":"src/lib.rs","hunk":0}"#,
    ]);
    assert_eq!(refused.code, 1);
    assert!(
        refused.stderr.contains("--allow-writes"),
        "{}",
        refused.stderr
    );

    let ran = cli.json(&[
        "run",
        "git.stage_hunk",
        "--args",
        r#"{"path":"src/lib.rs","hunk":0}"#,
        "--allow-writes",
        "--json",
    ]);
    assert_eq!(ran["ok"], true);
    assert_eq!(ran["command"], "git.stage_hunk");
    assert_eq!(ran["undoable"], true);
    let id = ran["journal_id"].as_u64().expect("journal id");
    let cached = norm(&git_in(&repo.root, &["diff", "--cached", "--name-only"]));
    assert!(cached.lines().any(|l| l == "src/lib.rs"), "{cached}");
    assert_eq!(cli.json(&["status", "--json"])["counts"]["staged"], 2);

    let activity = cli.json(&["activity", "--json"]);
    has_keys(&activity, &["schema", "entries"]);
    let entry = &activity["entries"][0];
    has_keys(
        entry,
        &[
            "id", "time", "command", "source", "summary", "repo", "paths", "args", "undoable",
            "undone",
        ],
    );
    assert_eq!(entry["id"], id);
    assert_eq!(entry["command"], "git.stage_hunk");
    assert_eq!(entry["source"], "cli");
    assert_eq!(entry["undoable"], true);
    assert_eq!(entry["undone"], false);

    let undo = cli.json(&["undo", &id.to_string(), "--json"]);
    has_keys(&undo, &["schema", "undone", "entry"]);
    assert_eq!(undo["undone"], id);
    let after = norm(&git_in(&repo.root, &["diff", "--cached", "--name-only"]));
    assert_eq!(
        after.trim(),
        "src/new.rs",
        "only the file staged before the journey"
    );
    let activity = cli.json(&["activity", "--json"]);
    assert_eq!(
        activity["entries"][0]["undo_of"], id,
        "the undo is in the log"
    );
    assert_eq!(activity["entries"][1]["undone"], true);
}
