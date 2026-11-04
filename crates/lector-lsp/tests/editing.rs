//! Editing features against `tests/fixtures/fake_lsp.py --editing`:
//! capabilities, diagnostics, formatting, code actions (+ resolve, commands),
//! prepare/rename, and server-initiated `workspace/applyEdit`.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use futures::executor::block_on;
use lector_lsp::{
    DiagnosticSeverity, FormattingOptions, LspManager, LspOptions, PointUtf16, ServerConfig,
};

/// (server, label, edit count) of a received `workspace/applyEdit`.
type Applied = (String, Option<String>, usize);

fn have_python() -> bool {
    lector_lsp::which("python3").is_some()
}

fn manager(dir: &Path, log: Option<&Path>) -> LspManager {
    let mut args = vec![
        format!("{}/tests/fixtures/fake_lsp.py", env!("CARGO_MANIFEST_DIR")),
        "--editing".to_string(),
    ];
    if let Some(log) = log {
        args.push("--log".into());
        args.push(log.display().to_string());
    }
    let cfg = ServerConfig {
        name: "fake".into(),
        command: "python3".into(),
        args,
        languages: vec!["Rust".into()],
        root_markers: vec![],
    };
    let opts = LspOptions {
        request_timeout: Duration::from_secs(5),
        ..LspOptions::default()
    };
    LspManager::with_options(dir.to_path_buf(), vec![cfg], opts)
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting");
}

fn pt(row: u32, column: u32) -> PointUtf16 {
    PointUtf16 { row, column }
}

fn setup(text: &str) -> (tempfile::TempDir, PathBuf, LspManager) {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("main.rs");
    std::fs::write(&f, text).unwrap();
    let m = manager(dir.path(), None);
    m.open_document_tagged(&f, "Rust", text, 7);
    wait_for(|| m.capabilities(&f).is_some());
    (dir, f, m)
}

#[test]
fn capabilities_and_document_state() {
    if !have_python() {
        return;
    }
    let (_dir, f, m) = setup("fn a() {}\n");
    let caps = m.capabilities(&f).unwrap();
    assert!(caps.formatting && caps.range_formatting && caps.rename && caps.prepare_rename);
    assert!(caps.code_actions && caps.code_action_resolve);
    assert_eq!(caps.commands, vec!["fake.applyEdit".to_string()]);
    assert_eq!(m.server_capabilities("fake"), Some(caps));
    let st = m.document_state(&f).unwrap();
    assert_eq!((st.version, st.tag), (1, Some(7)));
    m.open_document_tagged(&f, "Rust", "fn b() {}\n", 8);
    let st = m.document_state(&f).unwrap();
    assert_eq!((st.version, st.tag), (2, Some(8)));
    // Same text, new tag (undo back to a synced text): no new version.
    m.open_document_tagged(&f, "Rust", "fn b() {}\n", 9);
    assert_eq!(m.document_state(&f).unwrap().version, 2);
    assert_eq!(m.document_state(&f).unwrap().tag, Some(9));
    m.shutdown();
}

#[test]
fn diagnostics_are_stored_tagged_and_notified() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("main.rs");
    let m = manager(dir.path(), None);
    let (tx, rx) = mpsc::channel::<PathBuf>();
    let tx = Mutex::new(tx);
    m.on_diagnostics(move |p| {
        let _ = tx.lock().unwrap().send(p.to_path_buf());
    });
    m.open_document_tagged(&f, "Rust", "let bad = 1;\n  warn\n", 1);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), f);
    let d = m.diagnostics(&f).unwrap();
    assert_eq!(d.server, "fake");
    assert_eq!(d.version, Some(1));
    assert_eq!(d.tag, Some(1));
    assert_eq!(d.diagnostics.len(), 2);
    assert_eq!(d.diagnostics[0].severity, DiagnosticSeverity::Error);
    assert_eq!(d.diagnostics[0].range, pt(0, 4)..pt(0, 7));
    assert_eq!(d.diagnostics[0].code.as_deref(), Some("F1"));
    assert_eq!(d.diagnostics[1].severity, DiagnosticSeverity::Warning);
    assert_eq!(m.all_diagnostics().len(), 1);
    // A new version republishes with the new tag.
    m.open_document_tagged(&f, "Rust", "let ok = 1;\n", 2);
    wait_for(|| m.diagnostics(&f).is_some_and(|d| d.tag == Some(2)));
    assert!(m.diagnostics(&f).unwrap().diagnostics.is_empty());
    assert!(m.all_diagnostics().is_empty());
    // Closing clears and notifies.
    m.open_document_tagged(&f, "Rust", "bad\n", 3);
    wait_for(|| m.diagnostics(&f).is_some_and(|d| d.tag == Some(3)));
    while rx.try_recv().is_ok() {}
    m.close_document(&f);
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), f);
    assert!(m.diagnostics(&f).is_none());
    m.shutdown();
}

#[test]
fn formatting_whole_and_range() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.txt");
    let f = dir.path().join("main.rs");
    let m = manager(dir.path(), Some(&log));
    let text = "a  \nb\t\nc\n";
    m.open_document(&f, "Rust", text);
    let opts = FormattingOptions {
        tab_size: 2,
        insert_spaces: false,
    };
    let edits = block_on(m.formatting(&f, opts)).unwrap();
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0].range, pt(0, 1)..pt(0, 3));
    assert_eq!(edits[0].new_text, "");
    assert_eq!(edits[1].range, pt(1, 1)..pt(1, 2));
    let edits = block_on(m.range_formatting(&f, pt(1, 0)..pt(1, 2), opts)).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].range.start.row, 1);
    m.shutdown();
    wait_for(|| {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("format tab=2 spaces=False")
    });
}

#[test]
fn code_actions_resolve_and_commands() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.txt");
    let f = dir.path().join("main.rs");
    let m = manager(dir.path(), Some(&log));
    let applied: Arc<Mutex<Vec<Applied>>> = Arc::default();
    {
        let applied = applied.clone();
        m.set_apply_edit_handler(move |req| {
            applied.lock().unwrap().push((
                req.server.clone(),
                req.label.clone(),
                req.edit.changes[0].1.len(),
            ));
            req.respond(true, None);
        });
    }
    m.open_document(&f, "Rust", "x bad y\n");
    wait_for(|| m.diagnostics(&f).is_some());
    // Range with the diagnostic: the quick fix comes first and is preferred.
    let actions = block_on(m.code_actions(&f, pt(0, 0)..pt(0, 0), false)).unwrap();
    assert_eq!(actions.len(), 3, "{actions:?}"); // quick fix needs the overlap
    let actions = block_on(m.code_actions(&f, pt(0, 3)..pt(0, 5), true)).unwrap();
    assert_eq!(actions.len(), 4);
    let fix = &actions[0];
    assert!(fix.is_quick_fix() && fix.is_preferred);
    assert_eq!(fix.diagnostics.len(), 1);
    let edit = fix.edit.as_ref().unwrap();
    assert_eq!(edit.changes[0].0, f);
    assert_eq!(edit.changes[0].1[0].new_text, "good");
    assert_eq!(actions[3].disabled_reason.as_deref(), Some("not here"));
    // Resolve fills in the edit (with the document version).
    let header = actions[1].clone();
    assert!(header.edit.is_none());
    let resolved = block_on(m.resolve_code_action(&f, header)).unwrap();
    let edit = resolved.edit.unwrap();
    assert_eq!(edit.version_for(&f), Some(1));
    assert_eq!(edit.changes[0].1[0].new_text, "// header\n");
    // Commands run on the server, which asks the client to apply an edit.
    let cmd = actions[2].command.clone().unwrap();
    assert_eq!(cmd.command, "fake.applyEdit");
    block_on(m.execute_command(&f, &cmd)).unwrap();
    wait_for(|| !applied.lock().unwrap().is_empty());
    assert_eq!(
        applied.lock().unwrap()[0],
        ("fake".to_string(), Some("cmd".to_string()), 1)
    );
    m.shutdown();
    wait_for(|| {
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        log.contains("response {\"applied\": true}")
            && log.contains("codeAction diags=1 trigger=2")
            && log.contains("codeAction diags=0 trigger=1")
    });
}

#[test]
fn apply_edit_without_handler_or_dropped_is_rejected() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.txt");
    let f = dir.path().join("main.rs");
    let m = manager(dir.path(), Some(&log));
    m.open_document(&f, "Rust", "x\n");
    let actions = block_on(m.code_actions(&f, pt(0, 0)..pt(0, 0), false)).unwrap();
    let cmd = actions
        .iter()
        .find_map(|a| a.command.clone())
        .expect("command");
    block_on(m.execute_command(&f, &cmd)).unwrap();
    wait_for(|| {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("\"applied\": false")
    });
    // A handler that drops the request also answers applied=false.
    m.set_apply_edit_handler(drop);
    block_on(m.execute_command(&f, &cmd)).unwrap();
    wait_for(|| {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .matches("\"applied\": false")
            .count()
            == 2
    });
    m.shutdown();
}

#[test]
fn prepare_rename_and_rename_across_files() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("other.rs"), "use foo;\nfoo();\n").unwrap();
    let (dir, f, m) = {
        let f = dir.path().join("main.rs");
        let m = manager(dir.path(), None);
        m.open_document(&f, "Rust", "fn foo() {}\nfoo();\n");
        (dir, f, m)
    };
    let p = block_on(m.prepare_rename(&f, pt(1, 1))).unwrap().unwrap();
    assert_eq!(p.range, Some(pt(1, 0)..pt(1, 3)));
    assert_eq!(p.placeholder.as_deref(), Some("foo"));
    assert!(block_on(m.prepare_rename(&f, pt(0, 2))).unwrap().is_some());
    assert!(block_on(m.prepare_rename(&f, pt(1, 5))).unwrap().is_none());
    let edit = block_on(m.rename(&f, pt(0, 4), "bar")).unwrap().unwrap();
    assert_eq!(edit.file_count(), 2);
    assert_eq!(edit.changes[0].0, f);
    assert_eq!(edit.changes[0].1.len(), 2);
    assert_eq!(edit.version_for(&f), Some(1));
    let other = dir.path().join("other.rs");
    assert_eq!(edit.changes[1].0, other);
    assert_eq!(edit.version_for(&other), None);
    assert!(edit.changes[1].1.iter().all(|e| e.new_text == "bar"));
    // Errors from the server surface as errors.
    assert!(block_on(m.rename(&f, pt(1, 5), "x")).is_err());
    m.shutdown();
}

#[test]
fn no_server_answers_empty() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("main.rs");
    let m = LspManager::new(dir.path().into(), vec![]);
    m.open_document(&f, "Rust", "x");
    assert!(block_on(m.formatting(&f, FormattingOptions::default()))
        .unwrap()
        .is_empty());
    assert!(block_on(m.code_actions(&f, pt(0, 0)..pt(0, 0), false))
        .unwrap()
        .is_empty());
    assert!(block_on(m.prepare_rename(&f, pt(0, 0))).unwrap().is_none());
    assert!(m.capabilities(&f).is_none());
    assert!(m.diagnostics(&f).is_none());
    assert!(m.document_state(&f).is_none());
}

#[test]
#[ignore = "needs pylsp on PATH (pip install 'python-lsp-server[rope,yapf,pyflakes]')"]
fn pylsp_diagnostics_format_rename() {
    if lector_lsp::which("pylsp").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("m.py");
    let text = "import os\ndef add(a,b):\n    return a+b\nprint(add(1,2))\n";
    std::fs::write(&f, text).unwrap();
    let m = LspManager::with_options(
        dir.path().into(),
        lector_lsp::builtin_servers()
            .into_iter()
            .filter(|c| c.name == "pylsp")
            .collect(),
        LspOptions {
            request_timeout: Duration::from_secs(30),
            ..LspOptions::default()
        },
    );
    m.open_document_tagged(&f, "Python", text, 1);
    wait_for(|| m.diagnostics(&f).is_some_and(|d| !d.diagnostics.is_empty()));
    let d = m.diagnostics(&f).unwrap();
    assert_eq!(d.tag, Some(1));
    assert!(
        d.diagnostics.iter().any(|d| d.message.contains("os")),
        "{d:?}"
    );
    let caps = m.capabilities(&f).unwrap();
    assert!(
        caps.formatting && caps.rename && caps.code_actions,
        "{caps:?}"
    );
    let edits = block_on(m.formatting(&f, FormattingOptions::default())).unwrap();
    assert!(!edits.is_empty());
    let edit = block_on(m.rename(&f, pt(1, 5), "plus")).unwrap().unwrap();
    assert_eq!(edit.changes[0].0, f);
    // rope answers with one whole-document edit.
    assert!(
        edit.changes[0]
            .1
            .iter()
            .any(|e| e.new_text.contains("def plus(")),
        "{edit:?}"
    );
    m.shutdown();
    // Read-only settings keep rope's cache out of the project.
    assert!(!dir.path().join(".ropeproject").exists());
}
