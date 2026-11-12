//! The inline edit model driven end to end against the scripted fake ACP agent
//! (`fake_acp_agent.py`): prompt -> turn -> hunks -> decisions, cancel, follow-up, other files.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::Duration;

use lector_agent::inline_edit::*;
use lector_agent::*;
use lector_app::{AppCore, CoreOptions};
use lector_buffer::TextEdit;
use lector_workspace::Project;

const WAIT: Duration = Duration::from_secs(30);

fn hermetic_git() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "2");
        std::env::set_var("GIT_CONFIG_KEY_0", "gc.auto");
        std::env::set_var("GIT_CONFIG_VALUE_0", "0");
        std::env::set_var("GIT_CONFIG_KEY_1", "commit.gpgsign");
        std::env::set_var("GIT_CONFIG_VALUE_1", "false");
    });
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

struct Fx {
    _work: tempfile::TempDir,
    _data: tempfile::TempDir,
    root: PathBuf,
    core: AppCore,
    project: Project,
    py: &'static str,
}

impl Fx {
    fn new() -> Option<Fx> {
        hermetic_git();
        let py = ["python3", "python"].into_iter().find(|p| {
            Command::new(p)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })?;
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = work.path().join("proj");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let core = AppCore::open(CoreOptions {
            data_dir: Some(data.path().to_path_buf()),
            ..CoreOptions::for_tests()
        });
        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(&root, settings).unwrap();
        let root = project.root().to_path_buf();
        Some(Fx {
            _work: work,
            _data: data,
            root,
            core,
            project,
            py,
        })
    }

    fn start(&self) -> AgentSession {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_acp_agent.py");
        let cfg = AgentConfig::new("Fake", self.py, &[script.to_str().unwrap()]);
        AgentManager::of(&self.core)
            .start_with(
                &self.core,
                &self.project,
                &cfg,
                SessionOptions {
                    request_timeout: Duration::from_secs(20),
                    cancel_grace: Duration::from_secs(5),
                    ..Default::default()
                },
            )
            .unwrap()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.root.join(rel)).unwrap()
    }

    /// An inline edit over the line `needle` of a.txt.
    fn edit(&self, needle: &str) -> InlineEdit {
        let text = self.read("a.txt");
        let at = text.find(needle).unwrap();
        InlineEdit::new(InlineRequest::from_selection(
            "a.txt",
            &text,
            at..at + needle.len(),
        ))
    }
}

macro_rules! fx {
    () => {
        match Fx::new() {
            Some(f) => f,
            None => {
                eprintln!("python3 not found: skipping");
                return;
            }
        }
    };
}

fn apply(text: &str, edits: &[TextEdit]) -> String {
    let mut out = text.to_string();
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    for e in sorted {
        out.replace_range(e.range.clone(), &e.new_text);
    }
    out
}

fn finish(fx: &Fx, s: &AgentSession, edit: &mut InlineEdit, turn: TurnId) -> String {
    s.wait_turn(turn, WAIT).expect("turn finished");
    let buffer = fx.read("a.txt");
    edit.finish_from_session(s, turn, buffer.clone());
    buffer
}

#[test]
fn one_inline_edit_from_prompt_to_decided_hunks() {
    let fx = fx!();
    let s = fx.start();
    let mut edit = fx.edit("two");
    assert_eq!(edit.phase(), &InlinePhase::Prompting);
    // The instruction is the scripted agent's program: change the line, and touch another file.
    let instruction = "write a.txt one\\nTWO\\nthree\\n\nwrite src/lib.rs pub fn g() {}\\n";
    let turn = edit.start(&s, instruction).unwrap();
    assert_eq!(edit.phase(), &InlinePhase::Running { turn });
    // The prompt carried the file, the lines and the selection as context.
    let sent = s.turn(turn).unwrap();
    assert!(sent.prompt.contains("`a.txt`, line 2"), "{}", sent.prompt);
    assert_eq!(
        sent.context,
        ["@a.txt:2-2".to_string(), "@a.txt:1-3".to_string()]
    );
    let buffer = finish(&fx, &s, &mut edit, turn);
    assert_eq!(buffer, "one\nTWO\nthree\n");

    assert_eq!(edit.phase(), &InlinePhase::Reviewing);
    assert_eq!(edit.hunks().len(), 1);
    assert_eq!(edit.hunks()[0].old_text, "two\n");
    assert_eq!(edit.hunk_rows(&edit.hunks()[0].clone()), 1..2);
    assert_eq!(edit.other_files(), ["src/lib.rs".to_string()]);
    assert_eq!(
        edit.other_files_notice().as_deref(),
        Some("The agent also changed 1 other file")
    );
    // The turn is a normal checkpoint: reviewable and undoable.
    let t = s.turn(turn).unwrap();
    assert!(t.changeset().is_some() && t.journal_id.is_some());

    // Undo the hunk: the edit restores the old line in the buffer.
    let id = edit.hunks()[0].id;
    let undo = edit.reject(id).unwrap();
    assert_eq!(apply(&buffer, &[undo]), "one\ntwo\nthree\n");
    assert_eq!(
        edit.phase(),
        &InlinePhase::Done(InlineOutcome::Resolved { kept: 0, undone: 1 })
    );
    s.stop();
}

#[test]
fn keeping_the_hunk_needs_no_write() {
    let fx = fx!();
    let s = fx.start();
    let mut edit = fx.edit("two");
    let turn = edit.start(&s, "write a.txt one\\n2\\nthree\\n").unwrap();
    let buffer = finish(&fx, &s, &mut edit, turn);
    assert_eq!(edit.pending_count(), 1);
    assert_eq!(edit.accept_all(), 1);
    assert_eq!(
        edit.phase(),
        &InlinePhase::Done(InlineOutcome::Resolved { kept: 1, undone: 0 })
    );
    assert_eq!(edit.current_text(), buffer);
    assert_eq!(fx.read("a.txt"), "one\n2\nthree\n");
    s.stop();
}

#[test]
fn cancelling_a_running_edit_ends_cancelled_and_changes_nothing() {
    let fx = fx!();
    let s = fx.start();
    let mut edit = fx.edit("two");
    let turn = edit.start(&s, "slow").unwrap();
    assert!(edit.needs_turn_cancel());
    s.cancel();
    let buffer = finish(&fx, &s, &mut edit, turn);
    assert_eq!(edit.phase(), &InlinePhase::Cancelled);
    assert_eq!(buffer, "one\ntwo\nthree\n");
    assert!(!edit.is_active());
    s.stop();
}

#[test]
fn an_agent_that_crashes_fails_the_edit() {
    let fx = fx!();
    let s = fx.start();
    let mut edit = fx.edit("two");
    let turn = edit.start(&s, "crash").unwrap();
    finish(&fx, &s, &mut edit, turn);
    assert!(
        matches!(edit.phase(), InlinePhase::Failed(_)),
        "{:?}",
        edit.phase()
    );
    s.stop();
}

#[test]
fn a_refused_prompt_leaves_the_edit_prompting() {
    let fx = fx!();
    let s = fx.start();
    let mut edit = fx.edit("two");
    // The session is busy with another turn.
    let busy = s.prompt("slow".into(), vec![]).unwrap();
    let err = edit.start(&s, "write a.txt x\\n").unwrap_err();
    assert!(matches!(err, InlineError::Agent(_)), "{err:?}");
    assert_eq!(edit.phase(), &InlinePhase::Prompting);
    s.cancel();
    s.wait_turn(busy, WAIT).unwrap();
    assert_eq!(
        edit.start(&s, "").unwrap_err(),
        InlineError::EmptyInstruction
    );
    s.stop();
}

#[test]
fn a_follow_up_refines_the_pending_hunks_in_a_new_turn() {
    let fx = fx!();
    let s = fx.start();
    let mut edit = fx.edit("two");
    let first = edit.start(&s, "write a.txt one\\nTWO\\nthree\\n").unwrap();
    finish(&fx, &s, &mut edit, first);
    assert_eq!(edit.pending_count(), 1);
    let second = edit
        .follow_up(&s, "write a.txt one\\nTwo!\\nthree\\n")
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(edit.turns(), [first, second]);
    let buffer = finish(&fx, &s, &mut edit, second);
    assert_eq!(buffer, "one\nTwo!\nthree\n");
    assert_eq!(edit.phase(), &InlinePhase::Reviewing);
    // Still one hunk, measured against the original text.
    assert_eq!(edit.hunks().len(), 1);
    assert_eq!(edit.hunks()[0].old_text, "two\n");
    let undo = edit.reject_all();
    assert_eq!(apply(&buffer, &undo), "one\ntwo\nthree\n");
    assert_eq!(s.turns().len(), 2, "two normal turns in one session");
    s.stop();
}

#[test]
fn the_user_editing_the_selection_while_the_agent_works_falls_back() {
    let fx = fx!();
    let s = fx.start();
    let mut edit = fx.edit("two");
    let turn = edit.start(&s, "write a.txt one\\nTWO\\nthree\\n").unwrap();
    // The user types inside the selected line before the turn ends (the scripted agent is quick:
    // the model only needs the order of events, not the timing).
    let effect = edit.user_edited(
        &[EditSpan {
            old: 7..7,
            new_len: 1,
        }],
        "one\ntwo!\nthree\n",
    );
    assert_eq!(effect, EditEffect::SelectionEdited);
    finish(&fx, &s, &mut edit, turn);
    assert_eq!(
        edit.phase(),
        &InlinePhase::Done(InlineOutcome::FellBack(FallbackReason::EditedSelection))
    );
    // The turn is still there to review.
    assert!(s.turn(turn).unwrap().changeset().is_some());
    s.stop();
}
