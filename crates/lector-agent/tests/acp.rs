//! End-to-end tests against `fake_acp_agent.py` (Python 3): streaming, tool calls, file
//! reads and writes through the document store, permissions, cancel, crash and restart,
//! checkpoints, the turn changeset, undo (with conflicts), commands and provenance.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Once};
use std::time::Duration;

use lector_agent::*;
use lector_app::commands::Source;
use lector_app::documents::DocumentStore;
use lector_app::journal::Journal;
use lector_app::provenance::{LineQuery, ProvenanceKind};
use lector_app::{AppCore, CommandContext, CoreOptions};
use lector_git::Changeset;
use lector_workspace::Project;
use parking_lot::Mutex;
use serde_json::json;

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

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
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
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
        let py = python()?;
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

    fn config(&self) -> AgentConfig {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_acp_agent.py");
        AgentConfig::new("Fake", self.py, &[script.to_str().unwrap()])
    }

    fn options() -> SessionOptions {
        SessionOptions {
            request_timeout: Duration::from_secs(20),
            cancel_grace: Duration::from_secs(5),
            ..Default::default()
        }
    }

    fn start(&self) -> AgentSession {
        AgentManager::of(&self.core)
            .start_with(&self.core, &self.project, &self.config(), Self::options())
            .unwrap()
    }

    fn run(&self, s: &AgentSession, text: &str) -> Turn {
        let id = s.prompt(text.into(), vec![]).unwrap();
        s.wait_turn(id, WAIT).expect("turn finished")
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.root.join(rel)).ok()
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

#[test]
fn streams_messages_thoughts_and_plans() {
    let fx = fx!();
    let s = fx.start();
    assert_eq!(s.status(), SessionStatus::Ready);
    assert_eq!(s.agent_name(), "Fake Agent");
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let _sub = s.subscribe(move |e| ev.lock().push(*e));
    let t = fx.run(
        &s,
        "think pondering\nplan read|edit|test\nsay hello streaming world",
    );
    assert_eq!(t.state, TurnState::Completed("end_turn".into()));
    assert_eq!(t.messages[0].kind, MessageKind::Thought);
    assert_eq!(t.messages[1].text, "hello streaming world");
    assert_eq!(t.plan.len(), 3);
    assert_eq!(t.plan[1].status, PlanStatus::InProgress);
    let evs = events.lock().clone();
    assert!(evs.contains(&AgentEvent::TurnStarted(t.id)));
    assert!(
        evs.iter()
            .filter(|e| **e == AgentEvent::TurnUpdated(t.id))
            .count()
            >= 3
    );
    assert!(evs.contains(&AgentEvent::TurnFinished(t.id)));
    assert_eq!(s.status(), SessionStatus::Ready);
    // Nothing changed: checkpoints exist, no journal entry, nothing to undo.
    assert!(t.before.is_some() && t.after.is_some());
    assert!(t.changed_files.is_empty());
    assert!(matches!(
        s.undo_turn(t.id, false, Source::Internal),
        Err(AgentError::NotUndoable(_))
    ));
    s.stop();
}

#[test]
fn writes_are_checkpointed_reviewable_and_undoable() {
    let fx = fx!();
    let s = fx.start();
    let t = fx.run(
        &s,
        "say editing\nwrite a.txt one\\nTWO\\nthree\\n\nwrite src/new.rs pub fn added() {}\\n",
    );
    assert_eq!(t.state, TurnState::Completed("end_turn".into()), "{t:?}");
    assert_eq!(fx.read("a.txt").unwrap(), "one\nTWO\nthree\n");
    assert_eq!(fx.read("src/new.rs").unwrap(), "pub fn added() {}\n");
    assert_eq!(t.tool_calls.len(), 2);
    assert_eq!(t.tool_calls[0].status, ToolStatus::Completed);
    assert_eq!(t.tool_calls[0].diffs[0].new_text, "one\nTWO\nthree\n");
    assert_eq!(
        t.written,
        vec!["a.txt".to_string(), "src/new.rs".to_string()]
    );
    let paths: Vec<&str> = t.changed_files.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "src/new.rs"]);

    // The turn is a changeset the review code can load (sides live in the shadow repo).
    let cs = t.changeset().unwrap();
    let repo = fx.project.repo().unwrap();
    let changes = repo.changes(&cs).unwrap();
    assert_eq!(changes.len(), 2);
    let old = repo.load(&changes[0].old).unwrap().unwrap();
    let new = repo.load(&changes[0].new).unwrap().unwrap();
    assert_eq!(old, b"one\ntwo\nthree\n");
    assert_eq!(new, b"one\nTWO\nthree\n");
    let key = lector_workspace::review::changeset_key(&cs);
    assert_eq!(
        lector_workspace::review::parse_changeset_key(&key),
        Some(cs.clone())
    );
    let title = lector_workspace::review::changeset_title(Some(repo), &cs);
    assert!(title.contains("turn 1"), "{title}");
    let view = lector_workspace::review::load_changeset(&fx.project, &cs, &fx.core.review_store())
        .unwrap();
    assert_eq!(view.files.len(), 2);

    // Journal: one entry per write and one for the turn (with the undo).
    let journal = Journal::of(&fx.core).unwrap();
    let entries = journal.list(10);
    assert_eq!(entries[0].command, "agent.turn");
    assert!(entries[0].undo.is_some());
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.command == "agent.write_file")
            .count(),
        2
    );

    // Undo the turn, then redo it through the journal.
    let undo_id = s.undo_turn(t.id, false, Source::Internal).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    assert!(fx.read("src/new.rs").is_none());
    assert!(s.turn(t.id).unwrap().undone);
    let cx = CommandContext::new(fx.core.clone(), Some(fx.project.clone()), Source::Internal);
    journal
        .undo(undo_id, &cx, lector_app::journal::UndoMode::Check)
        .unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\nTWO\nthree\n");
    assert!(!s.turn(t.id).unwrap().undone);
    s.stop();
}

#[test]
fn undo_reports_files_edited_since_and_force_overwrites() {
    let fx = fx!();
    let s = fx.start();
    let t = fx.run(&s, "write a.txt agent\\n");
    fs::write(fx.root.join("a.txt"), "user edit\n").unwrap();
    match s.undo_turn(t.id, false, Source::Internal) {
        Err(AgentError::UndoConflict { paths }) => assert_eq!(paths, vec!["a.txt".to_string()]),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.read("a.txt").unwrap(), "user edit\n");
    s.undo_turn(t.id, true, Source::Internal).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    s.stop();
}

#[test]
fn file_access_uses_open_documents_and_stays_in_the_project() {
    let fx = fx!();
    let store = DocumentStore::for_core(&fx.core);
    let handle = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    handle.replace_all("unsaved text\n").unwrap();
    let s = fx.start();
    let t = fx.run(&s, "read a.txt");
    assert!(t.answer().contains("READ:unsaved text"), "{}", t.answer());

    // A write to an open document goes through it: buffer updated and saved.
    let t = fx.run(&s, "write a.txt from agent\\n");
    assert_eq!(t.state.label(), "completed");
    let text = handle.text().unwrap();
    assert_eq!(text.text_for_range(0..text.len()), "from agent\n");
    assert!(!handle.is_dirty());
    assert_eq!(fx.read("a.txt").unwrap(), "from agent\n");

    // Outside the project and inside .git: refused, nothing written.
    let outside = fx._work.path().join("outside.txt");
    let t = fx.run(
        &s,
        &format!(
            "write {} evil\nwrite ../escape.txt evil\nwrite .git/hooks/pre-commit evil\nread /etc/hostname",
            outside.display()
        ),
    );
    assert!(!outside.exists());
    assert!(!fx._work.path().join("escape.txt").exists());
    assert!(!fx.root.join(".git/hooks/pre-commit").exists());
    let answer = t.answer();
    assert_eq!(answer.matches("WRITE-ERROR:").count(), 3, "{answer}");
    assert!(answer.contains("READ-ERROR:"), "{answer}");
    assert!(t.written.is_empty());
    s.stop();
}

#[test]
fn permissions_once_always_reject_and_auto_reads() {
    let fx = fx!();
    let s = fx.start();
    let (tx, rx) = std::sync::mpsc::channel();
    let _sub = s.subscribe(move |e| {
        if let AgentEvent::PermissionRequested(id) = e {
            let _ = tx.send(*id);
        }
    });
    let answer = |choice: PermissionChoice| {
        let id = rx.recv_timeout(WAIT).unwrap();
        let pending = s.pending_permissions();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        s.answer_permission(id, choice);
    };
    let id = s.prompt("ask Run the tests".into(), vec![]).unwrap();
    answer(PermissionChoice::AllowOnce);
    let t = s.wait_turn(id, WAIT).unwrap();
    assert!(t.answer().contains("PERMISSION:yes"));
    assert_eq!(t.permissions[0].title, "Run the tests");
    assert_eq!(t.permissions[0].answer, Some(PermissionChoice::AllowOnce));
    assert!(t.timeline.contains(&TurnItem::Permission(0)));

    let id = s.prompt("ask Edit files".into(), vec![]).unwrap();
    answer(PermissionChoice::Reject);
    assert!(s
        .wait_turn(id, WAIT)
        .unwrap()
        .answer()
        .contains("PERMISSION:no"));

    let id = s.prompt("ask Edit files".into(), vec![]).unwrap();
    answer(PermissionChoice::AllowAlways);
    assert!(s
        .wait_turn(id, WAIT)
        .unwrap()
        .answer()
        .contains("PERMISSION:always"));
    // Same kind again: answered automatically.
    let t = fx.run(&s, "ask Edit more");
    assert!(t.answer().contains("PERMISSION:always"));
    assert!(t.permissions[0].automatic);
    // Reads inside the project are allowed without asking.
    let t = fx.run(&s, "askread Read src/lib.rs");
    assert!(t.answer().contains("PERMISSION:yes"));
    assert!(rx.try_recv().is_err());
    s.stop();
}

#[test]
fn cancel_stops_streaming_and_pending_permissions() {
    let fx = fx!();
    let s = fx.start();
    let (tx, rx) = std::sync::mpsc::channel();
    let _sub = s.subscribe(move |e| {
        let _ = tx.send(*e);
    });
    let id = s.prompt("slow".into(), vec![]).unwrap();
    // Wait for some streaming.
    while rx.recv_timeout(WAIT).unwrap() != AgentEvent::TurnUpdated(id) {}
    std::thread::sleep(Duration::from_millis(100));
    s.cancel();
    let t = s.wait_turn(id, WAIT).unwrap();
    assert_eq!(t.state, TurnState::Cancelled);
    assert_eq!(s.status(), SessionStatus::Ready);

    let id = s.prompt("ask Dangerous".into(), vec![]).unwrap();
    loop {
        if let AgentEvent::PermissionRequested(_) = rx.recv_timeout(WAIT).unwrap() {
            break;
        }
    }
    s.cancel();
    let t = s.wait_turn(id, WAIT).unwrap();
    assert_eq!(t.state, TurnState::Cancelled);
    assert!(t
        .permissions
        .iter()
        .all(|p| !p.pending && p.answer.is_none()));
    assert!(t.answer().contains("PERMISSION:cancelled"));
    // The session keeps working.
    assert!(fx.run(&s, "say again").answer().contains("again"));
    s.stop();
}

#[test]
fn crash_fails_the_turn_and_restart_recovers() {
    let fx = fx!();
    let s = fx.start();
    let t = fx.run(&s, "write a.txt half done\\n\ncrash");
    match &t.state {
        TurnState::Failed(msg) => assert!(msg.contains("boom"), "{msg}"),
        other => panic!("{other:?}"),
    }
    // The write before the crash is still checkpointed and undoable.
    assert_eq!(t.changed_files.len(), 1);
    assert!(matches!(s.status(), SessionStatus::Crashed(_)));
    assert!(matches!(
        s.prompt("say x".into(), vec![]),
        Err(AgentError::NotRunning(_))
    ));
    s.restart().unwrap();
    assert_eq!(s.status(), SessionStatus::Ready);
    let t2 = fx.run(&s, "garbage\nsay still fine");
    assert_eq!(t2.id, TurnId(2));
    assert!(t2.answer().contains("still fine"));
    s.undo_turn(t.id, false, Source::Internal).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    s.stop();
    assert_eq!(s.status(), SessionStatus::Stopped);
}

#[test]
fn start_failures_are_reported() {
    let fx = fx!();
    let cfg = AgentConfig::new("Nope", "lector-no-such-agent-binary", &[]);
    let err = AgentSession::start_with(&fx.core, &fx.project, &cfg, Fx::options()).unwrap_err();
    assert!(matches!(err, AgentError::Start { .. }), "{err}");
    // A program that exits at once.
    let cfg = AgentConfig::new(
        "Exit",
        fx.py,
        &[
            "-c",
            "import sys; sys.stderr.write('no acp here\\n'); sys.exit(1)",
        ],
    );
    let err = AgentSession::start_with(&fx.core, &fx.project, &cfg, Fx::options()).unwrap_err();
    assert!(err.to_string().contains("no acp here"), "{err}");
}

#[test]
fn context_is_embedded_or_inlined() {
    let fx = fx!();
    let s = fx.start();
    let ctx = vec![
        PromptContext::File {
            path: "src/lib.rs".into(),
        },
        PromptContext::Range {
            path: "a.txt".into(),
            start: 1,
            end: 2,
        },
        PromptContext::Diff {
            label: "Changes".into(),
            diff: "+x".into(),
        },
    ];
    let id = s.prompt("say ok".into(), ctx.clone()).unwrap();
    let t = s.wait_turn(id, WAIT).unwrap();
    assert!(
        t.answer().contains("CONTEXT:resource,resource,text"),
        "{}",
        t.answer()
    );
    assert_eq!(t.context, vec!["@src/lib.rs", "@a.txt:2-2", "Changes"]);
    s.stop();

    std::env::set_var("FAKE_ACP_NO_EMBED", "1");
    let s = AgentSession::start_with(&fx.core, &fx.project, &fx.config(), Fx::options()).unwrap();
    std::env::remove_var("FAKE_ACP_NO_EMBED");
    let id = s.prompt("say ok".into(), ctx).unwrap();
    let t = s.wait_turn(id, WAIT).unwrap();
    assert!(
        t.answer().contains("CONTEXT:text,text,text"),
        "{}",
        t.answer()
    );
    s.stop();
}

#[test]
fn commands_drive_the_session() {
    let fx = fx!();
    commands::register(&fx.core);
    let reg = fx.core.registry();
    let cx = CommandContext::new(fx.core.clone(), Some(fx.project.clone()), Source::Cli);
    // No session yet.
    assert!(reg.invoke("agent.turns", json!({}), &cx).is_err());
    let s = fx.start();
    let out = reg
        .invoke(
            "agent.prompt",
            json!({"text": "write a.txt via command\\n", "wait": true}),
            &cx,
        )
        .unwrap();
    assert_eq!(out.data["state"]["state"], "completed");
    assert_eq!(out.data["changed_files"][0]["path"], "a.txt");
    assert!(out.data["changeset"]
        .as_str()
        .unwrap()
        .starts_with("snapshots:"));
    let turns = reg.invoke("agent.turns", json!({}), &cx).unwrap();
    assert_eq!(turns.data["turns"].as_array().unwrap().len(), 1);
    fs::write(fx.root.join("a.txt"), "edited by user\n").unwrap();
    let err = reg.invoke("agent.undo_turn", json!({}), &cx).unwrap_err();
    assert!(err.to_string().contains("a.txt"), "{err}");
    reg.invoke("agent.undo_turn", json!({"force": true}), &cx)
        .unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), "one\ntwo\nthree\n");
    // Feedback without comments needs text.
    assert!(reg.invoke("agent.send_comments", json!({}), &cx).is_err());
    let out = reg
        .invoke(
            "agent.send_comments",
            json!({"text": "a.txt:1 please keep the header", "wait": true}),
            &cx,
        )
        .unwrap();
    assert!(out.data["answer"]
        .as_str()
        .unwrap()
        .contains("please keep the header"));
    assert!(reg.invoke("agent.cancel", json!({}), &cx).is_err());
    let list = reg.invoke("agent.list_configs", json!({}), &cx).unwrap();
    assert!(list.data.is_array());
    s.stop();
}

#[test]
fn provenance_explains_lines_written_by_a_turn() {
    let fx = fx!();
    let s = fx.start();
    let t = fx.run(&s, "write a.txt one\\nfrom the agent\\nthree\\n");
    assert_eq!(t.changed_files.len(), 1);
    let repo = fx.project.repo().unwrap().workdir().to_path_buf();
    let q = |line| LineQuery {
        repo: repo.clone(),
        path: "a.txt".into(),
        line,
        rev: None,
    };
    let p = fx.core.provenance().explain(&q(1)).unwrap();
    assert_eq!(p.kind, ProvenanceKind::AgentTurn);
    assert!(p.title.contains("turn 1"), "{}", p.title);
    assert!(p.detail.contains("from the agent"));
    // Line 0 is unchanged by the agent: not attributed to it.
    let p0 = fx.core.provenance().explain(&q(0));
    assert!(p0.is_none_or(|p| p.kind != ProvenanceKind::AgentTurn));
    // The user rewrites the line: no longer the agent's.
    fs::write(fx.root.join("a.txt"), "one\nmine now\nthree\n").unwrap();
    let p = fx.core.provenance().explain(&q(1));
    assert!(p.is_none_or(|p| p.kind != ProvenanceKind::AgentTurn));
    s.stop();
    let _ = Changeset::WorkingTree;
}

/// Regression: checkpoints leave ignored files out, so an agent overwriting an ignored file
/// (`.env`) during a checkpointed turn was neither backed up nor undoable; and a dangling
/// symlink inside the project let a write create its target outside the project.
#[test]
fn ignored_files_are_backed_up_and_dangling_links_refused() {
    let fx = fx!();
    fs::write(fx.root.join(".gitignore"), ".env\n").unwrap();
    fs::write(fx.root.join(".env"), "SECRET=keep\n").unwrap();
    let outside = fx._work.path().join("planted.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, fx.root.join("link.txt")).unwrap();
    let s = fx.start();
    let t = fx.run(&s, "write .env SECRET=agent\\n\nwrite link.txt evil\\n");
    assert_eq!(fx.read(".env").unwrap(), "SECRET=agent\n");
    assert!(
        !outside.exists(),
        "write-through to a target outside the project"
    );
    // Symlinks need privileges on Windows, so the dangling-link half is Unix-only.
    #[cfg(unix)]
    assert!(t.answer().contains("WRITE-ERROR:"), "{}", t.answer());
    #[cfg(not(unix))]
    let _ = t;
    let journal = Journal::of(&fx.core).unwrap();
    let entry = journal
        .list(20)
        .into_iter()
        .find(|e| e.command == "agent.write_file" && e.summary.contains(".env"))
        .expect("journaled");
    assert!(
        entry.undo.is_some(),
        "the ignored file's write must be undoable"
    );
    let cx = CommandContext::new(fx.core.clone(), Some(fx.project.clone()), Source::Internal);
    journal
        .undo(entry.id, &cx, lector_app::journal::UndoMode::Check)
        .unwrap();
    assert_eq!(fx.read(".env").unwrap(), "SECRET=keep\n");
    s.stop();
}
