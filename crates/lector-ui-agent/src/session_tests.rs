//! The panel, the review header and the inline edit against two agent sessions in a row,
//! driven by the scripted fake ACP agent of `lector-agent`.
//!
//! Turn ids restart at 1 in every session, so anything that keeps only a `TurnId` across the
//! start of a new session acts on the wrong turn. These tests start a second session while the
//! first one's reviews and inline edits are still around.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Once};
use std::time::Duration;

use gpui::{App, AppContext, Entity, TestAppContext, VisualTestContext, WeakEntity};
use lector_agent::inline_edit::InlinePhase;
use lector_agent::summary::TurnSummary;
use lector_agent::{AgentConfig, AgentManager, AgentSession, SessionOptions, TurnId};
use lector_app::{AppCore, CoreOptions};
use lector_theme::{Appearance, ThemeRegistry};
use lector_ui::commands_bridge::CoreGlobal;
use lector_ui::theme::{ThemeMode, ThemeState};
use lector_ui::views::Workspace;
use lector_app::project::Project;

use crate::inline_edit::InlineEdits;
use crate::panel::AgentPanel;
use crate::summary_ui::SummaryHeader;

const WAIT: Duration = Duration::from_secs(30);
const ORIGINAL_A: &str = "one\ntwo\nthree\n";
const ORIGINAL_B: &str = "bee\n";

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
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
}

/// A git project with `a.txt` and `b.txt`, an app core of its own, and the fake agent.
struct Fx {
    _work: tempfile::TempDir,
    _data: tempfile::TempDir,
    root: PathBuf,
    core: AppCore,
    project: Project,
    py: &'static str,
}

impl Fx {
    /// `None` when no Python is installed (the fake agent is a Python script).
    fn new() -> Option<Fx> {
        hermetic_git();
        let py = ["python3", "python"].into_iter().find(|p| {
            std::process::Command::new(p)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })?;
        let work = tempfile::tempdir().expect("tempdir");
        let data = tempfile::tempdir().expect("tempdir");
        let root = work.path().join("proj");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("a.txt"), ORIGINAL_A).expect("write");
        std::fs::write(root.join("b.txt"), ORIGINAL_B).expect("write");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let core = AppCore::open(CoreOptions {
            data_dir: Some(data.path().to_path_buf()),
            ..CoreOptions::for_tests()
        });
        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        settings.session.restore = false;
        let project = Project::open(&root, settings).expect("project");
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

    /// Starts a new session; the manager stops the previous one.
    fn session(&self) -> AgentSession {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../lector-agent/tests/fake_acp_agent.py");
        let cfg = AgentConfig::new("Fake", self.py, &[script.to_str().expect("utf-8 path")]);
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
            .expect("session")
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).expect("read")
    }

    /// Runs `script` as a turn of `session` and waits for it.
    fn turn(&self, session: &AgentSession, script: &str) -> TurnId {
        let id = session
            .prompt(script.to_string(), Vec::new())
            .expect("prompt");
        session.wait_turn(id, WAIT).expect("turn finished");
        id
    }
}

macro_rules! fx {
    () => {
        match Fx::new() {
            Some(f) => f,
            None => {
                eprintln!("python not found: skipping");
                return;
            }
        }
    };
}

/// A workspace window over the fixture project with an Agent panel in it.
fn open<'a>(
    cx: &'a mut TestAppContext,
    fx: &Fx,
) -> (
    Entity<Workspace>,
    Entity<AgentPanel>,
    &'a mut VisualTestContext,
) {
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    settings.session.restore = false;
    let core = fx.core.clone();
    let for_ui = settings.clone();
    cx.update(move |cx: &mut App| {
        ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        cx.set_global(lector_ui::code_view::EditorSettings::default());
        cx.set_global(lector_ui::settings::AppSettings {
            settings: Arc::new(for_ui),
            warnings: Default::default(),
        });
        cx.set_global(CoreGlobal(core));
    });
    let project = Project::open(&fx.root, settings).expect("project");
    let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project.clone(), window, cx));
    cx.run_until_parked();
    let panel =
        cx.update(|window, cx| cx.new(|cx| AgentPanel::new(&ws, fx.project.clone(), window, cx)));
    cx.run_until_parked();
    (ws, panel, cx)
}

/// Runs the executor (and lets real time pass) until `done`.
fn wait_for(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    for _ in 0..1500 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
        cx.executor().advance_clock(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn empty_summary() -> Rc<TurnSummary> {
    Rc::new(TurnSummary {
        files: Vec::new(),
        added: 0,
        removed: 0,
        tests_touched: 0,
        unanalyzed: 0,
    })
}

fn header_for(
    session: &AgentSession,
    turn: TurnId,
    panel: &Entity<AgentPanel>,
    cx: &mut VisualTestContext,
) -> Entity<SummaryHeader> {
    let (summary, session, panel) = (empty_summary(), session.clone(), panel.downgrade());
    cx.update(move |_, cx| {
        cx.new(move |_| {
            SummaryHeader::new(summary, session, turn, panel, WeakEntity::new_invalid())
        })
    })
}

/// "Undo turn" in a review tab of an old session restores that session's files, not those of
/// the turn with the same id in the session started since.
#[gpui::test]
fn undo_from_an_old_sessions_review_header_touches_only_that_session(cx: &mut TestAppContext) {
    let fx = fx!();
    let (_ws, panel, cx) = open(cx, &fx);

    // Session 1, turn 1 changes a.txt; session 2 (which stops session 1), turn 1 changes b.txt.
    let first = fx.session();
    let t1 = fx.turn(&first, "write a.txt ONE\\ntwo\\nthree\\n");
    cx.run_until_parked();
    let second = fx.session();
    cx.run_until_parked();
    let t2 = fx.turn(&second, "write b.txt BEE\\n");
    cx.run_until_parked();
    assert_eq!(
        (t1, t2),
        (TurnId(1), TurnId(1)),
        "turn ids restart per session"
    );
    assert_ne!(first, second);
    assert_eq!(
        panel.read_with(cx, |p, _| p.session().cloned()),
        Some(second.clone())
    );
    assert_eq!(fx.read("a.txt"), "ONE\ntwo\nthree\n");
    assert_eq!(fx.read("b.txt"), "BEE\n");

    // The review tab of session 1's turn kept its header; the panel has moved on.
    let old_header = header_for(&first, TurnId(1), &panel, cx);
    assert_eq!(old_header.read_with(cx, |h, _| h.undo_unavailable()), None);
    old_header.update(cx, |h, cx| h.undo(cx));
    wait_for(cx, "the undo of the old session's turn", |_| {
        fx.read("a.txt") == ORIGINAL_A
    });
    cx.run_until_parked();
    assert_eq!(
        fx.read("a.txt"),
        ORIGINAL_A,
        "the old turn's file is restored"
    );
    assert_eq!(
        fx.read("b.txt"),
        "BEE\n",
        "the new session's file is untouched"
    );
    assert!(first.turn(TurnId(1)).is_some_and(|t| t.undone));
    assert!(second.turn(TurnId(1)).is_some_and(|t| !t.undone));
    assert!(old_header
        .read_with(cx, |h, _| h.undo_unavailable())
        .is_some());
    // Nothing about the old turn leaks into what the panel shows for the new session.
    assert!(panel.read_with(cx, |p, _| p.undo_conflict.is_none()
        && p.busy_turn_action.is_none()));

    // The current session's own header still undoes the current session's turn.
    let new_header = header_for(&second, TurnId(1), &panel, cx);
    new_header.update(cx, |h, cx| h.undo(cx));
    wait_for(cx, "the undo of the new session's turn", |_| {
        fx.read("b.txt") == ORIGINAL_B
    });
    assert_eq!(fx.read("b.txt"), ORIGINAL_B);
    assert_eq!(fx.read("a.txt"), ORIGINAL_A);
    first.stop();
    second.stop();
}

/// The panel's "last turn" actions carry the session along.
#[gpui::test]
fn last_turn_is_bound_to_its_session_and_summaries_are_per_session(cx: &mut TestAppContext) {
    let fx = fx!();
    let (_ws, panel, cx) = open(cx, &fx);
    let first = fx.session();
    fx.turn(&first, "write a.txt ONE\\ntwo\\nthree\\n");
    wait_for(cx, "the summary of the first session", |cx| {
        panel.read_with(cx, |p, _| {
            matches!(
                p.summaries.get(&(first.id(), TurnId(1))),
                Some(
                    crate::panel::SummaryState::Ready(_) | crate::panel::SummaryState::Unavailable
                )
            )
        })
    });
    let second = fx.session();
    cx.run_until_parked();
    // A new session has no turns and no summary: the old turn 1's card is not its turn 1's.
    assert_eq!(panel.read_with(cx, |p, _| p.last_turn()), None);
    fx.turn(&second, "write b.txt BEE\\n");
    wait_for(cx, "the second session's turn", |cx| {
        panel.read_with(cx, |p, _| p.last_turn().is_some())
    });
    let (session, turn) = panel
        .read_with(cx, |p, _| p.last_turn())
        .expect("last turn");
    assert_eq!((session, turn), (second.clone(), TurnId(1)));
    wait_for(cx, "the summary of the second session", |cx| {
        panel.read_with(cx, |p, _| {
            p.summaries.contains_key(&(second.id(), TurnId(1)))
        })
    });
    assert_eq!(panel.read_with(cx, |p, _| p.summaries.len()), 2);
    first.stop();
    second.stop();
}

/// Refine after a new agent session started: the follow-up runs on the current session (the
/// original one was stopped by it), the inline edit follows that session, and the hunks arrive.
/// When that turn fails the edit leaves "Working" with the failure.
#[gpui::test]
fn refining_after_a_new_session_follows_the_new_session(cx: &mut TestAppContext) {
    let fx = fx!();
    let (ws, panel, cx) = open(cx, &fx);
    let path = fx.root.join("a.txt");
    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(path.clone(), Some(2), Some(1), window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    let inline = cx.update(|_, cx| {
        cx.new(|cx| InlineEdits::new(cx.weak_entity(), ws.downgrade(), panel.downgrade()))
    });
    let first = fx.session();
    cx.run_until_parked();
    assert_eq!(
        panel.read_with(cx, |p, _| p.session().cloned()),
        Some(first.clone())
    );

    // Edit line 2 with the first session.
    ws.update_in(cx, |ws, window, cx| {
        inline.update(cx, |i, cx| i.begin_for_test(ws, view.clone(), window, cx))
    });
    cx.run_until_parked();
    inline.update_in(cx, |i, window, cx| {
        i.submit_for_test(&path, "write a.txt one\\nTWO\\nthree\\n", false, window, cx)
    });
    let reviewing = |cx: &mut VisualTestContext| {
        inline.read_with(cx, |i, _| {
            matches!(i.phase_of(&path), Some(InlinePhase::Reviewing))
        })
    };
    wait_for(cx, "the first result", reviewing);
    assert_eq!(
        inline.read_with(cx, |i, _| i.session_of(&path)),
        Some(first.clone())
    );

    // A new session starts (the panel switches to it; the first one stops) ...
    let second = fx.session();
    cx.run_until_parked();
    assert_eq!(
        panel.read_with(cx, |p, _| p.session().cloned()),
        Some(second.clone())
    );
    // ... and the user refines the pending change.
    inline.update_in(cx, |i, window, cx| {
        i.submit_for_test(&path, "write a.txt one\\nTwo\\nthree\\n", true, window, cx)
    });
    wait_for(cx, "the refined result (stuck on Working)", |cx| {
        reviewing(cx) && inline.read_with(cx, |i, _| i.session_of(&path)) == Some(second.clone())
    });
    assert_eq!(fx.read("a.txt"), "one\nTwo\nthree\n");
    assert_eq!(
        second.turns().len(),
        1,
        "the follow-up ran on the new session"
    );
    assert_eq!(first.turns().len(), 1);

    // Refine again; this turn fails (the agent dies): the edit says so instead of working on.
    inline.update_in(cx, |i, window, cx| {
        i.submit_for_test(&path, "crash", true, window, cx)
    });
    wait_for(cx, "the failed refinement to end", |cx| {
        !matches!(
            inline.read_with(cx, |i, _| i.phase_of(&path)),
            Some(InlinePhase::Running { .. })
        )
    });
    // The pending change is back for review, with the failure said next to it.
    assert!(reviewing(cx));
    let notes = inline.read_with(cx, |i, _| i.notes_of(&path));
    assert!(
        notes
            .iter()
            .any(|n| n.contains("The agent failed") && n.contains("boom")),
        "{notes:?}"
    );
    first.stop();
    second.stop();
}

/// A turn that fails before changing anything ends the inline edit as failed, with the reason.
#[gpui::test]
fn a_failed_first_turn_leaves_working_with_the_reason(cx: &mut TestAppContext) {
    let fx = fx!();
    let (ws, panel, cx) = open(cx, &fx);
    let path = fx.root.join("a.txt");
    let view = ws
        .update_in(cx, |ws, window, cx| {
            ws.open_path_at(path.clone(), Some(2), Some(1), window, cx)
        })
        .expect("file view");
    cx.run_until_parked();
    let inline = cx.update(|_, cx| {
        cx.new(|cx| InlineEdits::new(cx.weak_entity(), ws.downgrade(), panel.downgrade()))
    });
    let session = fx.session();
    cx.run_until_parked();
    ws.update_in(cx, |ws, window, cx| {
        inline.update(cx, |i, cx| i.begin_for_test(ws, view.clone(), window, cx))
    });
    cx.run_until_parked();
    inline.update_in(cx, |i, window, cx| {
        i.submit_for_test(&path, "crash", false, window, cx)
    });
    wait_for(cx, "the failure", |cx| {
        inline.read_with(cx, |i, _| {
            matches!(i.phase_of(&path), Some(InlinePhase::Failed(_)))
        })
    });
    let phase = inline.read_with(cx, |i, _| i.phase_of(&path));
    let Some(InlinePhase::Failed(why)) = phase else {
        panic!("expected a failure, got {phase:?}");
    };
    assert!(why.contains("boom"), "{why}");
    assert_eq!(fx.read("a.txt"), ORIGINAL_A);
    session.stop();
}
