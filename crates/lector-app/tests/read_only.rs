//! Write-policy regression: opening the core on a repository and invoking every
//! read command (every registered command with `writes == false`) must not
//! change a single byte or mtime in the work tree or `.git`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use lector_app::commands::{CommandContext, Focus, Source};
use lector_app::{AppCore, CoreOptions};
use lector_config::Settings;
use lector_workspace::Project;
use serde_json::{json, Value};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0")
        .env("GIT_CONFIG_KEY_1", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "gc.autoDetach")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

type Snapshot = BTreeMap<PathBuf, (bool, Vec<u8>, Option<std::time::SystemTime>)>;

fn snapshot(root: &Path) -> Snapshot {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if meta.is_dir() {
                out.insert(rel, (true, Vec::new(), meta.modified().ok()));
                stack.push(path);
            } else {
                out.insert(rel, (false, fs::read(&path).unwrap(), meta.modified().ok()));
            }
        }
    }
    out
}

fn fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(
        d,
        "src/main.rs",
        "fn main() {\n    helper();\n}\n\nfn helper() {}\n",
    );
    write(d, "README.md", "# demo\nhello\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    write(
        d,
        "src/main.rs",
        "fn main() {\n    helper();\n    helper();\n}\n\nfn helper() {}\n",
    );
    git(d, &["commit", "-q", "-am", "second"]);
    let head = git(d, &["rev-parse", "HEAD"]).trim().to_string();
    write(d, "README.md", "# demo\nhello world\n");
    write(d, "src/lib.rs", "pub fn lib() {}\n");
    git(d, &["add", "src/lib.rs"]);
    write(d, "notes.txt", "untracked\n");
    (dir, head)
}

/// Arguments that make each read command do real work on the fixture.
fn args_for(id: &str, head: &str) -> Vec<Value> {
    match id {
        "workspace.open_path" => vec![json!({"path": "src/main.rs", "line": 1})],
        "workspace.open_location" => vec![json!({"location": "src/main.rs:2:5"})],
        "git.open_changeset" => vec![json!({"changeset": format!("commit:{head}")})],
        "search.project" => vec![
            json!({"pattern": "helper"}),
            json!({"pattern": "h.l+o", "regex": true, "include": ["*.md"]}),
        ],
        "symbols.find" => vec![json!({"name": "helper"}), json!({"name": "hel"})],
        "git.list_changes" => ["worktree", "staged", "unstaged", "untracked"]
            .iter()
            .map(|c| json!({ "changeset": c }))
            .chain([json!({"changeset": format!("commit:{head}")}), json!({})])
            .collect(),
        "git.show_diff" => vec![
            json!({}),
            json!({"changeset": "staged"}),
            json!({"changeset": format!("commit:{head}"), "path": "src/main.rs", "context": 1}),
        ],
        "git.blame" => vec![
            json!({"path": "src/main.rs"}),
            json!({"path": "README.md", "start_line": 1}),
        ],
        "git.history" => vec![
            json!({}),
            json!({"path": "src/main.rs"}),
            json!({"path": "src/main.rs", "start_line": 1, "end_line": 3}),
            json!({"author": "t"}),
        ],
        "settings.get" => vec![json!({}), json!({"key": "git.confirm_discard"})],
        "document.open" | "document.read" | "document.close" => {
            vec![json!({"path": "src/main.rs"})]
        }
        "document.set_read_only" => vec![json!({"path": "src/main.rs", "read_only": true})],
        _ => vec![json!({})],
    }
}

#[test]
fn read_commands_write_nothing() {
    let (dir, head) = fixture();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let before = snapshot(&root);

    let core = AppCore::open(CoreOptions::for_tests());
    let mut settings = Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(&root, settings).unwrap();
    let mut cx = CommandContext::new(core.clone(), Some(project.clone()), Source::Internal);
    cx.focus = Focus {
        path: Some(root.join("src/main.rs")),
        ..Default::default()
    };

    let mut ran = 0;
    for command in core.registry().list() {
        let spec = command.spec();
        if spec.writes {
            continue;
        }
        for args in args_for(spec.id, &head) {
            match core.registry().invoke(spec.id, args.clone(), &cx) {
                Ok(outcome) => assert!(outcome.journal_id.is_none()),
                // Commands of other modules may need state this fixture lacks
                // (e.g. `document.close` of a document that is not open); the
                // tree check below still covers them. The core's own read
                // commands must succeed.
                Err(e) if !spec.id.starts_with("document.") => {
                    panic!("{} {args}: {e}", spec.id)
                }
                Err(_) => {}
            }
            ran += 1;
        }
    }
    assert!(ran >= 20, "only {ran} invocations");

    // Shared git state (what every window uses) is read-only too.
    let state = core.git().repo_for(project.repo().unwrap());
    let snap = state.refresh_blocking();
    assert!(snap.status.len() >= 3);
    assert!(core.scheduler().wait_idle(Duration::from_secs(20)));
    drop(state);

    assert_eq!(
        before,
        snapshot(&root),
        "a read command wrote to the repository"
    );
}

#[test]
fn outcomes_are_structured() {
    let (dir, head) = fixture();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let core = AppCore::open(CoreOptions::for_tests());
    let mut settings = Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(&root, settings).unwrap();
    let cx = CommandContext::new(core.clone(), Some(project), Source::Internal);
    let reg = core.registry();

    let changes = reg.invoke("git.list_changes", json!({}), &cx).unwrap().data;
    let paths: Vec<&str> = changes["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["README.md", "notes.txt", "src/lib.rs"]);
    assert_eq!(changes["branch"], "main");

    let diff = reg
        .invoke(
            "git.show_diff",
            json!({"changeset": format!("commit:{head}")}),
            &cx,
        )
        .unwrap()
        .data;
    let file = &diff["files"][0];
    assert_eq!(file["path"], "src/main.rs");
    assert_eq!(file["added"], 1);
    let lines = file["hunks"][0]["lines"].as_array().unwrap();
    assert!(lines.iter().any(|l| l == "+    helper();"));

    let search = reg
        .invoke("search.project", json!({"pattern": "helper"}), &cx)
        .unwrap();
    assert_eq!(search.data["matches"], 3);
    assert_eq!(search.data["files"][0]["path"], "src/main.rs");

    let open = reg
        .invoke(
            "workspace.open_location",
            json!({"location": "src/main.rs:2:5"}),
            &cx,
        )
        .unwrap();
    assert_eq!(
        open.ui,
        vec![lector_app::UiRequest::OpenPath {
            path: root.join("src/main.rs"),
            point: Some(lector_text::Point { row: 1, column: 4 }),
        }]
    );

    // Schema validation and availability errors are reported, not panics.
    assert!(reg.invoke("git.blame", json!({"path": 3}), &cx).is_err());
    assert!(reg.invoke("search.project", json!({}), &cx).is_err());
    assert!(reg.invoke("nope", json!({}), &cx).is_err());
    let headless = CommandContext::new(core.clone(), None, Source::Cli);
    let err = reg
        .invoke("git.list_changes", json!({}), &headless)
        .unwrap_err();
    assert_eq!(err.to_string(), "No folder is open");

    let list = reg
        .invoke("commands.list", json!({}), &headless)
        .unwrap()
        .data;
    let ids: Vec<&str> = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    for id in [
        "git.blame",
        "git.history",
        "search.project",
        "symbols.find",
        "settings.get",
    ] {
        assert!(ids.contains(&id), "{id} missing");
    }
}
