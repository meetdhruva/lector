//! Undo round trip for every write command (ADR 0002, point 5).
//!
//! The registry is enumerated: every command with `writes: true` must either declare
//! `UndoPolicy::NotUndoable(reason)` or have at least one scenario in [`SCENARIOS`]; a write
//! command with neither fails [`every_write_command_has_a_scenario_or_declares_not_undoable`],
//! so a new write command cannot be added without one.
//!
//! A scenario builds a repository with the git CLI (hermetic config, `core.autocrlf=false`),
//! captures its [`State`], runs the command through the registry, undoes the journal entry the
//! way the UI's Undo does (`Journal::undo(id, cx, UndoMode::Check)`), and asserts the state is
//! back. Then it redoes (undo of the undo) and asserts the post-command state, and undoes the
//! original operation once more and asserts the pre-command state again.
//!
//! State = working-tree bytes (tracked and untracked, `.git` excluded), index entries with
//! their stages and intent-to-add flags, every ref (`for-each-ref`, HEAD and its symbolic
//! target), the stash list (commit ids and messages), in-progress operation markers and the
//! repository's local config (upstream settings live there).
//!
//! Commands declared not undoable are run once each in [`not_undoable_commands_are_refused`]:
//! the journal refuses their undo and the refusal changes nothing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use lector_app::commands::{CommandContext, CommandError, Source, UndoPolicy};
use lector_app::journal::{Journal, UndoError, UndoMode};
use lector_app::{AppCore, CoreOptions, Outcome, Project};
use lector_config::Settings;
use serde_json::{json, Value};

/// Write commands with an undo, and the scenario tests (functions in this file) that cover
/// them. [`scenario_table_names_real_tests`] checks every name is a test here.
const SCENARIOS: &[(&str, &[&str])] = &[
    (
        "git.stage",
        &[
            "stage_untracked_file",
            "stage_partially_staged_file",
            "stage_intent_to_add_file",
            "stage_worktree_deletion",
        ],
    ),
    (
        "git.unstage",
        &[
            "unstage_staged_add",
            "unstage_partially_staged_file",
            "unstage_staged_rename",
            "unstage_staged_delete",
            "unstage_intent_to_add_file",
        ],
    ),
    ("git.stage_all", &["stage_all_in_a_mixed_tree"]),
    ("git.unstage_all", &["unstage_all_in_a_mixed_tree"]),
    (
        "git.discard",
        &[
            "discard_unstaged_edit",
            "discard_untracked_files",
            "discard_partially_staged_file",
            "discard_intent_to_add_file",
        ],
    ),
    (
        "git.discard_all",
        &[
            "discard_all_unstaged",
            "discard_all_untracked",
            "discard_all_everything",
        ],
    ),
    (
        "git.stage_hunk",
        &[
            "stage_hunk_by_index",
            "stage_selected_lines_on_partially_staged_file",
        ],
    ),
    ("git.unstage_hunk", &["unstage_hunk_by_index"]),
    (
        "git.discard_hunk",
        &["discard_hunk_by_index", "discard_selected_lines"],
    ),
    (
        "git.accept_file",
        &["accept_current_side", "accept_incoming_side"],
    ),
    ("git.mark_resolved", &["mark_resolved_after_editing"]),
    ("git.resolve_delete", &["resolve_modify_delete_by_deleting"]),
    (
        "git.pull",
        &[
            "pull_fast_forward_with_local_changes",
            "pull_rebase_local_commit",
        ],
    ),
    (
        "git.create_branch",
        &["create_branch", "create_branch_and_switch"],
    ),
    (
        "git.switch_branch",
        &["switch_branch_clean", "switch_branch_stashing_a_mixed_tree"],
    ),
    ("git.rename_branch", &["rename_branch"]),
    ("git.delete_branch", &["delete_branch_with_upstream"]),
    (
        "git.set_upstream",
        &["set_upstream_on_a_new_branch", "remove_upstream"],
    ),
    (
        "git.stash_push",
        &["stash_push_mixed_tree", "stash_push_including_untracked"],
    ),
    (
        "git.stash_apply",
        &["stash_apply_on_clean_tree", "stash_apply_restoring_index"],
    ),
    (
        "git.stash_pop",
        &["stash_pop_on_clean_tree", "stash_pop_restoring_index"],
    ),
    ("git.stash_drop", &["stash_drop"]),
    ("git.stash_store", &["stash_store_after_drop"]),
    ("git.restore_ref", &["restore_ref_moves_a_branch"]),
];

// ---------------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------------

fn core() -> AppCore {
    AppCore::open(CoreOptions::for_tests())
}

#[test]
fn every_write_command_has_a_scenario_or_declares_not_undoable() {
    let core = core();
    let mut missing = Vec::new();
    for command in core.registry().list() {
        let spec = command.spec();
        if !spec.writes {
            assert!(
                matches!(spec.undo, UndoPolicy::NothingToUndo),
                "read command `{}` declares an undo",
                spec.id
            );
            continue;
        }
        let covered = SCENARIOS
            .iter()
            .any(|(id, s)| *id == spec.id && !s.is_empty());
        match spec.undo {
            UndoPolicy::NotUndoable(reason) => {
                assert!(!reason.trim().is_empty(), "`{}`: empty reason", spec.id);
                assert!(!spec.undoable, "`{}`: undoable flag disagrees", spec.id);
                assert!(
                    !covered,
                    "`{}` is declared not undoable but has round-trip scenarios",
                    spec.id
                );
            }
            UndoPolicy::Undoable => {
                assert!(spec.undoable, "`{}`: undoable flag disagrees", spec.id);
                if !covered {
                    missing.push(spec.id);
                }
            }
            UndoPolicy::Undeclared | UndoPolicy::NothingToUndo => {
                panic!(
                    "write command `{}` declares no undo: mark it `.undoable()` and add a \
                     scenario to tests/undo_roundtrip.rs, or `.not_undoable(reason)`",
                    spec.id
                )
            }
        }
    }
    assert!(
        missing.is_empty(),
        "undoable write commands without an undo round-trip scenario: {missing:?}"
    );
    for (id, _) in SCENARIOS {
        let command = core
            .registry()
            .get(id)
            .unwrap_or_else(|| panic!("scenario for unknown command `{id}`"));
        assert!(command.spec().writes, "scenario for read command `{id}`");
    }
}

#[test]
fn scenario_table_names_real_tests() {
    let source = include_str!("undo_roundtrip.rs");
    for (id, names) in SCENARIOS {
        for name in *names {
            assert!(
                source.contains(&format!("#[test]\nfn {name}()"))
                    || source.contains(&format!("]\nfn {name}()")),
                "scenario `{name}` of `{id}` is not a test function in this file"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------------

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (k, v) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Ann"),
            ("GIT_AUTHOR_EMAIL", "ann@example.com"),
            ("GIT_COMMITTER_NAME", "Ann"),
            ("GIT_COMMITTER_EMAIL", "ann@example.com"),
            ("GIT_CONFIG_COUNT", "5"),
            ("GIT_CONFIG_KEY_0", "gc.auto"),
            ("GIT_CONFIG_VALUE_0", "0"),
            ("GIT_CONFIG_KEY_1", "maintenance.auto"),
            ("GIT_CONFIG_VALUE_1", "false"),
            ("GIT_CONFIG_KEY_2", "commit.gpgsign"),
            ("GIT_CONFIG_VALUE_2", "false"),
            ("GIT_CONFIG_KEY_3", "gc.autoDetach"),
            ("GIT_CONFIG_VALUE_3", "false"),
            ("GIT_CONFIG_KEY_4", "core.autocrlf"),
            ("GIT_CONFIG_VALUE_4", "false"),
        ] {
            std::env::set_var(k, v);
        }
    });
}

fn git_raw(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs")
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = git_raw(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Fx {
    _tmp: tempfile::TempDir,
    /// The working repository (the project).
    repo: PathBuf,
    /// A second clone that pushes "upstream" commits.
    other: PathBuf,
    cx: CommandContext,
    journal: Journal,
}

const PARSER: &str = "fn one() {}\n\nfn two() {}\n\nfn three() {}\n\nfn four() {}\n";

impl Fx {
    /// A repository `repo` (branch `main`, pushed to the bare `origin` with upstream set) with
    /// committed files and a clean tree.
    fn clean() -> Fx {
        hermetic();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = dunce::canonicalize(tmp.path()).expect("canonical tmp");
        let origin = root.join("origin.git");
        let (repo, other) = (root.join("repo"), root.join("other"));
        let s = |p: &Path| p.to_string_lossy().into_owned();
        git_in(&root, &["init", "-q", "--bare", "-b", "main", &s(&origin)]);
        git_in(&root, &["clone", "-q", &s(&origin), &s(&repo)]);
        let fx_files: &[(&str, &str)] = &[
            ("a.txt", "a1\na2\na3\n"),
            ("b.txt", "b1\nb2\n"),
            ("d.txt", "delete me\n"),
            ("old.txt", "renamed content\nline 2\nline 3\n"),
            ("keep.txt", "keep\n"),
            ("parser.rs", PARSER),
            ("sub/deep.txt", "deep\n"),
        ];
        for (rel, content) in fx_files {
            write_file(&repo, rel, content);
        }
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-q", "-m", "initial"]);
        git_in(&repo, &["push", "-q", "-u", "origin", "main"]);
        git_in(&root, &["clone", "-q", &s(&origin), &s(&other)]);

        let core = core();
        let journal = Journal::install_in(&core, root.join("data"));
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(&repo, settings).expect("project");
        // The UI's own source: undo runs exactly as the Undo button does.
        let cx = CommandContext::new(core, Some(project), Source::Ui);
        Fx {
            _tmp: tmp,
            repo,
            other,
            cx,
            journal,
        }
    }

    /// [`Fx::clean`] plus every index edge case at once:
    /// `a.txt` partially staged (MM), `b.txt` modified unstaged, `added.txt` staged add,
    /// `d.txt` staged delete, `old.txt` → `new.txt` staged rename, `ita.txt` intent-to-add,
    /// `u.txt` and `dir/u2.txt` untracked.
    fn mixed() -> Fx {
        let fx = Fx::clean();
        fx.write("a.txt", "a1 staged\na2\na3\n");
        fx.git(&["add", "a.txt"]);
        fx.write("a.txt", "a1 staged\na2\na3 unstaged\n");
        fx.write("b.txt", "b1\nb2 edited\n");
        fx.write("added.txt", "added\n");
        fx.git(&["add", "added.txt"]);
        fx.git(&["rm", "-q", "d.txt"]);
        fx.git(&["mv", "old.txt", "new.txt"]);
        fx.write("ita.txt", "intent to add\n");
        fx.git(&["add", "-N", "ita.txt"]);
        fx.write("u.txt", "untracked\n");
        fx.write("dir/u2.txt", "untracked deeper\n");
        fx
    }

    /// [`Fx::mixed`] without the intent-to-add entry: `git stash` itself refuses to stash
    /// an intent-to-add entry ("not uptodate. Cannot merge"; git 2.43), so a stash of the
    /// mixed tree fails before Lector changes anything.
    fn stashable() -> Fx {
        let fx = Fx::mixed();
        fx.git(&["rm", "-q", "--cached", "ita.txt"]);
        fx
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(&self.repo, args)
    }

    fn write(&self, rel: &str, content: &str) {
        write_file(&self.repo, rel, content);
    }

    fn run(&self, id: &str, args: Value) -> Result<Outcome, CommandError> {
        self.cx.core.registry().invoke(id, args, &self.cx)
    }

    fn ok(&self, id: &str, args: Value) -> Outcome {
        self.run(id, args).unwrap_or_else(|e| panic!("{id}: {e}"))
    }

    /// Commits a change in the other clone and pushes it to `origin/main`.
    fn upstream_commit(&self, rel: &str, content: &str) {
        write_file(&self.other, rel, content);
        git_in(&self.other, &["add", "-A"]);
        git_in(
            &self.other,
            &["commit", "-q", "-m", &format!("upstream {rel}")],
        );
        git_in(&self.other, &["push", "-q", "origin", "main"]);
    }

    fn state(&self) -> State {
        State::capture(&self.repo)
    }

    /// The round trip: command, undo, redo, undo again. Returns the command's outcome.
    fn round_trip(&self, id: &str, args: Value) -> Outcome {
        let before = self.state();
        let out = self.ok(id, args);
        let after = self.state();
        assert_ne!(
            before, after,
            "`{id}` changed nothing: the scenario tests nothing"
        );
        assert!(out.undo.is_some(), "`{id}` returned no undo record");
        let entry = out.journal_id.expect("journaled");

        let undone = self
            .journal
            .undo(entry, &self.cx, UndoMode::Check)
            .unwrap_or_else(|e| panic!("undo of `{id}`: {e}"));
        State::assert_same(&before, &self.state(), &format!("after undoing `{id}`"));

        let redone = self
            .journal
            .undo(undone.id, &self.cx, UndoMode::Check)
            .unwrap_or_else(|e| panic!("redo of `{id}`: {e}"));
        State::assert_same(&after, &self.state(), &format!("after redoing `{id}`"));

        // Redo made the original operation current again: undoing it once more works too.
        let original = self.journal.get(entry).expect("entry");
        assert!(!original.undone, "redo of `{id}` left the original undone");
        let _ = redone;
        self.journal
            .undo(entry, &self.cx, UndoMode::Check)
            .unwrap_or_else(|e| panic!("second undo of `{id}`: {e}"));
        State::assert_same(
            &before,
            &self.state(),
            &format!("after undoing `{id}` again"),
        );
        out
    }
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(p, content).expect("write");
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

/// Everything a write command may change in a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    /// Working-tree files (`/`-separated relative path → bytes), `.git` excluded.
    worktree: BTreeMap<String, Vec<u8>>,
    /// `git ls-files -s` (mode, object, stage, path), one entry per line.
    index: String,
    /// Index entries marked intent-to-add.
    intent_to_add: String,
    /// `git for-each-ref` (name, object, symbolic target), HEAD and its symbolic ref.
    refs: String,
    /// The stash list: commit id and message of each entry.
    stashes: String,
    /// In-progress operation markers under `.git` that exist.
    operation: Vec<&'static str>,
    /// `git config --local --list`.
    config: String,
}

impl State {
    fn capture(repo: &Path) -> State {
        let mut worktree = BTreeMap::new();
        walk(repo, repo, &mut worktree);
        let git = |args: &[&str]| git_in(repo, args);
        let head = {
            let out = git_raw(repo, &["rev-parse", "-q", "--verify", "HEAD"]);
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let head_ref = {
            let out = git_raw(repo, &["symbolic-ref", "-q", "HEAD"]);
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let refs = format!(
            "{}HEAD {head} -> {head_ref}\n",
            git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(symref)"
            ])
        );
        let stashes = {
            let out = git_raw(repo, &["stash", "list", "--format=%H %gs"]);
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let git_dir = repo.join(".git");
        let operation = [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
            "MERGE_MSG",
        ]
        .into_iter()
        .filter(|m| git_dir.join(m).exists())
        .collect();
        State {
            worktree,
            index: git(&["ls-files", "-s"]),
            intent_to_add: intent_to_add(repo),
            refs,
            stashes,
            operation,
            config: git(&["config", "--local", "--list"]),
        }
    }

    /// Asserts equality field by field, naming what differs.
    fn assert_same(expected: &State, actual: &State, when: &str) {
        let mut diffs = Vec::new();
        if expected.worktree != actual.worktree {
            let keys: std::collections::BTreeSet<&String> = expected
                .worktree
                .keys()
                .chain(actual.worktree.keys())
                .collect();
            for k in keys {
                let (e, a) = (expected.worktree.get(k), actual.worktree.get(k));
                if e != a {
                    diffs.push(format!(
                        "worktree {k}: expected {:?}, got {:?}",
                        e.map(|b| String::from_utf8_lossy(b).into_owned()),
                        a.map(|b| String::from_utf8_lossy(b).into_owned())
                    ));
                }
            }
        }
        let mut field = |name: &str, e: &str, a: &str| {
            if e != a {
                diffs.push(format!("{name}:\n--- expected\n{e}--- got\n{a}"));
            }
        };
        field("index", &expected.index, &actual.index);
        field(
            "intent-to-add",
            &expected.intent_to_add,
            &actual.intent_to_add,
        );
        field("refs", &expected.refs, &actual.refs);
        field("stashes", &expected.stashes, &actual.stashes);
        field("config", &expected.config, &actual.config);
        if expected.operation != actual.operation {
            diffs.push(format!(
                "operation: expected {:?}, got {:?}",
                expected.operation, actual.operation
            ));
        }
        assert!(
            diffs.is_empty(),
            "{when}, state differs:\n{}",
            diffs.join("\n")
        );
    }
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if dir == root && entry.file_name() == ".git" {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.insert(rel, fs::read(&path).unwrap_or_default());
        }
    }
}

/// Paths whose index entry is intent-to-add: the only entries the working-tree diff (index
/// against files) shows as added.
fn intent_to_add(repo: &Path) -> String {
    git_in(repo, &["diff", "--name-only", "--diff-filter=A"])
}

// ---------------------------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------------------------

#[test]
fn stage_untracked_file() {
    let fx = Fx::mixed();
    fx.round_trip("git.stage", json!({"paths": ["u.txt"]}));
}

#[test]
fn stage_partially_staged_file() {
    let fx = Fx::mixed();
    fx.round_trip("git.stage", json!({"paths": ["a.txt"]}));
}

#[test]
fn stage_intent_to_add_file() {
    let fx = Fx::mixed();
    fx.round_trip("git.stage", json!({"paths": ["ita.txt"]}));
}

#[test]
fn stage_worktree_deletion() {
    let fx = Fx::mixed();
    fs::remove_file(fx.repo.join("keep.txt")).expect("rm");
    fx.round_trip("git.stage", json!({"paths": ["keep.txt"]}));
}

#[test]
fn unstage_staged_add() {
    let fx = Fx::mixed();
    fx.round_trip("git.unstage", json!({"paths": ["added.txt"]}));
}

#[test]
fn unstage_partially_staged_file() {
    let fx = Fx::mixed();
    fx.round_trip("git.unstage", json!({"paths": ["a.txt"]}));
}

#[test]
fn unstage_staged_rename() {
    let fx = Fx::mixed();
    fx.round_trip("git.unstage", json!({"paths": ["new.txt"]}));
}

#[test]
fn unstage_staged_delete() {
    let fx = Fx::mixed();
    fx.round_trip("git.unstage", json!({"paths": ["d.txt"]}));
}

#[test]
fn unstage_intent_to_add_file() {
    let fx = Fx::mixed();
    fx.round_trip("git.unstage", json!({"paths": ["ita.txt"]}));
}

#[test]
fn stage_all_in_a_mixed_tree() {
    let fx = Fx::mixed();
    fx.round_trip("git.stage_all", json!({}));
}

#[test]
fn unstage_all_in_a_mixed_tree() {
    let fx = Fx::mixed();
    fx.round_trip("git.unstage_all", json!({}));
}

// ---------------------------------------------------------------------------------------------
// Discard
// ---------------------------------------------------------------------------------------------

#[test]
fn discard_unstaged_edit() {
    let fx = Fx::mixed();
    fx.round_trip("git.discard", json!({"paths": ["b.txt"]}));
}

#[test]
fn discard_untracked_files() {
    let fx = Fx::mixed();
    fx.round_trip("git.discard", json!({"paths": ["u.txt", "dir/u2.txt"]}));
}

#[test]
fn discard_partially_staged_file() {
    let fx = Fx::mixed();
    fx.round_trip("git.discard", json!({"paths": ["a.txt"]}));
}

#[test]
fn discard_intent_to_add_file() {
    let fx = Fx::mixed();
    fx.round_trip("git.discard", json!({"paths": ["ita.txt"]}));
}

#[test]
fn discard_all_unstaged() {
    let fx = Fx::mixed();
    fx.round_trip("git.discard_all", json!({"scope": "unstaged"}));
}

#[test]
fn discard_all_untracked() {
    let fx = Fx::mixed();
    fx.round_trip("git.discard_all", json!({"scope": "untracked"}));
}

#[test]
fn discard_all_everything() {
    let fx = Fx::mixed();
    fx.round_trip("git.discard_all", json!({"scope": "all"}));
}

// ---------------------------------------------------------------------------------------------
// Hunks and lines
// ---------------------------------------------------------------------------------------------

const PARSER_EDITED: &str = "fn one() { 1 }\n\nfn two() {}\n\nfn three() {}\n\nfn four() { 4 }\n";

#[test]
fn stage_hunk_by_index() {
    let fx = Fx::mixed();
    fx.write("parser.rs", PARSER_EDITED);
    fx.round_trip("git.stage_hunk", json!({"path": "parser.rs", "hunk": 1}));
}

#[test]
fn stage_selected_lines_on_partially_staged_file() {
    let fx = Fx::mixed();
    fx.write("parser.rs", PARSER_EDITED);
    fx.git(&["add", "parser.rs"]);
    fx.write(
        "parser.rs",
        "fn one() { 1 }\n\nfn two() { 2 }\n\nfn three() { 3 }\n\nfn four() { 4 }\n",
    );
    // Row 2 of both sides: `fn two` only.
    fx.round_trip(
        "git.stage_hunk",
        json!({"path": "parser.rs", "old_lines": [[2, 3]], "new_lines": [[2, 3]]}),
    );
}

#[test]
fn unstage_hunk_by_index() {
    let fx = Fx::mixed();
    fx.write("parser.rs", PARSER_EDITED);
    fx.git(&["add", "parser.rs"]);
    fx.write(
        "parser.rs",
        "fn one() { 1 }\n\nfn two() {}\n\nfn three() {}\n\nfn four() { 44 }\n",
    );
    fx.round_trip("git.unstage_hunk", json!({"path": "parser.rs", "hunk": 0}));
}

#[test]
fn discard_hunk_by_index() {
    let fx = Fx::mixed();
    fx.write("parser.rs", PARSER_EDITED);
    fx.round_trip("git.discard_hunk", json!({"path": "parser.rs", "hunk": 0}));
}

#[test]
fn discard_selected_lines() {
    let fx = Fx::mixed();
    // Row 2 of a.txt's unstaged diff (index "a3" → worktree "a3 unstaged").
    fx.round_trip(
        "git.discard_hunk",
        json!({"path": "a.txt", "old_lines": [[2, 3]], "new_lines": [[2, 3]]}),
    );
}

// ---------------------------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------------------------

/// A merge of `feature` into `main` that conflicts in `c.txt`, with an unrelated untracked
/// file and an unstaged edit around it.
fn conflicted() -> Fx {
    let fx = Fx::clean();
    fx.write("c.txt", "base\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "base c"]);
    fx.git(&["checkout", "-q", "-b", "feature"]);
    fx.write("c.txt", "feature side\n");
    fx.git(&["commit", "-q", "-am", "feature c"]);
    fx.git(&["checkout", "-q", "main"]);
    fx.write("c.txt", "main side\n");
    fx.git(&["commit", "-q", "-am", "main c"]);
    let merge = git_raw(&fx.repo, &["merge", "feature"]);
    assert!(!merge.status.success(), "the merge conflicts");
    fx.write("u.txt", "untracked\n");
    fx
}

#[test]
fn accept_current_side() {
    let fx = conflicted();
    fx.round_trip("git.accept_file", json!({"path": "c.txt", "side": "ours"}));
}

#[test]
fn accept_incoming_side() {
    let fx = conflicted();
    fx.round_trip(
        "git.accept_file",
        json!({"path": "c.txt", "side": "theirs"}),
    );
}

#[test]
fn mark_resolved_after_editing() {
    let fx = conflicted();
    fx.write("c.txt", "merged by hand\n");
    fx.round_trip("git.mark_resolved", json!({"path": "c.txt"}));
}

#[test]
fn resolve_modify_delete_by_deleting() {
    let fx = Fx::clean();
    fx.git(&["checkout", "-q", "-b", "gone"]);
    fx.git(&["rm", "-q", "keep.txt"]);
    fx.git(&["commit", "-q", "-m", "delete keep"]);
    fx.git(&["checkout", "-q", "main"]);
    fx.write("keep.txt", "modified on main\n");
    fx.git(&["commit", "-q", "-am", "modify keep"]);
    let merge = git_raw(&fx.repo, &["merge", "gone"]);
    assert!(!merge.status.success(), "modify/delete conflict");
    fx.round_trip("git.resolve_delete", json!({"path": "keep.txt"}));
}

// ---------------------------------------------------------------------------------------------
// Pull, branches, upstream, refs
// ---------------------------------------------------------------------------------------------

#[test]
fn pull_fast_forward_with_local_changes() {
    let fx = Fx::clean();
    fx.upstream_commit("upstream.txt", "from upstream\n");
    fx.ok("git.fetch", json!({}));
    // Local changes the pull does not touch: staged, unstaged and untracked.
    fx.write("a.txt", "a1 staged\na2\na3\n");
    fx.git(&["add", "a.txt"]);
    fx.write("b.txt", "b1\nb2 edited\n");
    fx.write("u.txt", "untracked\n");
    fx.round_trip("git.pull", json!({}));
}

#[test]
fn pull_rebase_local_commit() {
    let fx = Fx::clean();
    fx.upstream_commit("upstream.txt", "from upstream\n");
    fx.write("local.txt", "local\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "local"]);
    fx.ok("git.fetch", json!({}));
    fx.round_trip("git.pull", json!({"mode": "rebase"}));
}

#[test]
fn create_branch() {
    let fx = Fx::mixed();
    fx.round_trip("git.create_branch", json!({"name": "topic"}));
}

#[test]
fn create_branch_and_switch() {
    let fx = Fx::mixed();
    fx.round_trip(
        "git.create_branch",
        json!({"name": "topic", "switch": true}),
    );
}

#[test]
fn switch_branch_clean() {
    let fx = Fx::clean();
    fx.git(&["branch", "other"]);
    fx.git(&["checkout", "-q", "other"]);
    fx.write("other.txt", "only on other\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "other"]);
    fx.git(&["checkout", "-q", "main"]);
    fx.round_trip("git.switch_branch", json!({"name": "other"}));
}

#[test]
fn switch_branch_stashing_a_mixed_tree() {
    let fx = Fx::stashable();
    fx.git(&["branch", "other"]);
    fx.round_trip(
        "git.switch_branch",
        json!({"name": "other", "stash_changes": true}),
    );
}

#[test]
fn rename_branch() {
    let fx = Fx::clean();
    fx.git(&["branch", "topic"]);
    fx.round_trip(
        "git.rename_branch",
        json!({"name": "topic", "new_name": "renamed"}),
    );
}

#[test]
fn delete_branch_with_upstream() {
    let fx = Fx::clean();
    fx.git(&["branch", "--track", "topic", "origin/main"]);
    fx.round_trip("git.delete_branch", json!({"name": "topic"}));
}

#[test]
fn set_upstream_on_a_new_branch() {
    let fx = Fx::clean();
    fx.git(&["branch", "topic"]);
    fx.round_trip(
        "git.set_upstream",
        json!({"branch": "topic", "upstream": "origin/main"}),
    );
}

#[test]
fn remove_upstream() {
    let fx = Fx::clean();
    fx.round_trip(
        "git.set_upstream",
        json!({"branch": "main", "upstream": null}),
    );
}

#[test]
fn restore_ref_moves_a_branch() {
    let fx = Fx::clean();
    let first = fx.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fx.git(&["branch", "topic"]);
    fx.git(&["checkout", "-q", "topic"]);
    fx.write("t.txt", "t\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "t"]);
    let tip = fx.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fx.git(&["checkout", "-q", "main"]);
    fx.round_trip(
        "git.restore_ref",
        json!({"ref": "refs/heads/topic", "expected": tip, "target": first}),
    );
}

// ---------------------------------------------------------------------------------------------
// Stash
// ---------------------------------------------------------------------------------------------

#[test]
fn stash_push_mixed_tree() {
    let fx = Fx::stashable();
    fx.round_trip("git.stash_push", json!({"message": "wip"}));
}

#[test]
fn stash_push_including_untracked() {
    let fx = Fx::stashable();
    fx.round_trip(
        "git.stash_push",
        json!({"message": "wip", "include_untracked": true}),
    );
}

/// A stash of the mixed tree (staged and unstaged changes, untracked files) on a clean tree,
/// plus an older stash below it.
fn stashed() -> Fx {
    let fx = Fx::clean();
    fx.write("keep.txt", "older stash\n");
    fx.git(&["stash", "push", "-q", "-m", "older"]);
    fx.write("a.txt", "a1 staged\na2\na3\n");
    fx.git(&["add", "a.txt"]);
    fx.write("a.txt", "a1 staged\na2\na3 unstaged\n");
    fx.write("b.txt", "b1\nb2 edited\n");
    fx.write("added.txt", "added\n");
    fx.git(&["add", "added.txt"]);
    fx.git(&["rm", "-q", "d.txt"]);
    fx.git(&["mv", "old.txt", "new.txt"]);
    fx.write("u.txt", "untracked\n");
    fx.git(&["stash", "push", "-q", "--include-untracked", "-m", "mixed"]);
    fx
}

#[test]
fn stash_apply_on_clean_tree() {
    let fx = stashed();
    fx.round_trip("git.stash_apply", json!({"index": 0}));
}

#[test]
fn stash_apply_restoring_index() {
    let fx = stashed();
    fx.round_trip(
        "git.stash_apply",
        json!({"index": 0, "restore_index": true}),
    );
}

#[test]
fn stash_pop_on_clean_tree() {
    let fx = stashed();
    fx.round_trip("git.stash_pop", json!({"index": 0}));
}

#[test]
fn stash_pop_restoring_index() {
    let fx = stashed();
    fx.round_trip("git.stash_pop", json!({"index": 0, "restore_index": true}));
}

#[test]
fn stash_drop() {
    let fx = stashed();
    fx.round_trip("git.stash_drop", json!({"index": 0}));
}

#[test]
fn stash_store_after_drop() {
    let fx = stashed();
    let oid = fx.git(&["rev-parse", "stash@{0}"]).trim().to_owned();
    let message = fx.git(&["log", "-g", "-1", "--format=%gs", "refs/stash"]);
    fx.git(&["stash", "drop", "-q", "stash@{0}"]);
    fx.round_trip(
        "git.stash_store",
        json!({"oid": oid, "message": message.trim()}),
    );
}

// ---------------------------------------------------------------------------------------------
// Commands declared not undoable
// ---------------------------------------------------------------------------------------------

/// Runs `id`, then asserts the journal refuses its undo and that the refusal wrote nothing.
fn refused(fx: &Fx, id: &str, args: Value) {
    let out = fx.ok(id, args);
    assert!(out.undo.is_none(), "`{id}` is declared not undoable");
    let entry = out.journal_id.expect("journaled");
    let before = fx.state();
    match fx.journal.undo(entry, &fx.cx, UndoMode::Check) {
        Err(UndoError::NotUndoable(why)) => assert!(!why.is_empty()),
        other => panic!("undo of `{id}`: {other:?}"),
    }
    State::assert_same(
        &before,
        &fx.state(),
        &format!("after refusing to undo `{id}`"),
    );
}

#[test]
fn not_undoable_commands_are_refused() {
    let fx = Fx::mixed();
    refused(&fx, "git.commit", json!({"message": "commit\n\nbody"}));
    refused(
        &fx,
        "git.commit",
        json!({"message": "amended", "amend": true, "sign_off": true}),
    );
    assert!(fx
        .git(&["log", "-1", "--format=%B"])
        .contains("Signed-off-by: Ann <ann@example.com>"));
    refused(&fx, "git.fetch", json!({}));
    refused(&fx, "git.push", json!({}));

    let fx = conflicted();
    fx.write("c.txt", "merged\n");
    fx.git(&["add", "c.txt"]);
    refused(&fx, "git.abort_operation", json!({}));
    let fx = Fx::clean();
    fx.git(&["checkout", "-q", "-b", "pick"]);
    fx.write("a.txt", "picked\n");
    fx.git(&["commit", "-q", "-am", "pick"]);
    fx.git(&["checkout", "-q", "main"]);
    fx.write("a.txt", "main\n");
    fx.git(&["commit", "-q", "-am", "main"]);
    assert!(!git_raw(&fx.repo, &["cherry-pick", "pick"]).status.success());
    fx.write("a.txt", "resolved\n");
    fx.git(&["add", "a.txt"]);
    refused(&fx, "git.continue_operation", json!({}));
}

#[test]
fn the_commit_command_reports_what_the_composer_shows() {
    let fx = Fx::mixed();
    let out = fx.ok(
        "git.commit",
        json!({"message": "\n\n  Subject line  \n\nbody"}),
    );
    assert_eq!(out.data["subject"], "Subject line");
    assert_eq!(out.data["upstream"]["branch"], "main");
    assert_eq!(out.data["upstream"]["upstream"], "origin/main");
    assert_eq!(out.data["upstream"]["ahead"], 1);
    let oid = fx.git(&["rev-parse", "HEAD"]);
    assert_eq!(out.data["oid"], oid.trim());
}
