//! Document store tests: tempfile fixtures, fixture git repos built with the
//! `git` CLI, inline and real (scheduler) executors.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lector_buffer::TextEdit;
use lector_config::Settings;
use lector_lsp::{Diagnostic, DiagnosticSeverity, DocumentDiagnostics};
use lector_text::PointUtf16;
use lector_workspace::Project;
use parking_lot::Mutex;

use super::*;
use crate::scheduler::{CancelToken, Scheduler};

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: Project,
}

fn settings() -> Settings {
    let mut s = Settings::default();
    s.lsp.enabled = false;
    s
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixture(files: &[(&str, &[u8])], repo: bool) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    for (rel, bytes) in files {
        std::fs::write(root.join(rel), bytes).unwrap();
    }
    if repo {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
    }
    let project = Project::open(&root, settings()).unwrap();
    Fx {
        _dir: dir,
        root,
        project,
    }
}

fn inline_store() -> DocumentStore {
    let store = DocumentStore::new(Arc::new(InlineExecutor));
    store.set_delays(QueryDelays::IMMEDIATE);
    store
}

fn text(h: &DocHandle) -> String {
    h.text().unwrap().to_string()
}

fn insert(h: &DocHandle, at: usize, s: &str) {
    h.apply_edits(vec![TextEdit {
        range: at..at,
        new_text: s.into(),
    }])
    .unwrap();
}

/// Collects events.
fn record(h: &DocHandle) -> (Arc<Mutex<Vec<DocEvent>>>, Subscription) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let e = events.clone();
    let sub = h.subscribe(move |ev| e.lock().push(ev.clone()));
    (events, sub)
}

#[test]
fn one_document_per_path_shared_by_two_views() {
    let fx = fixture(&[("a.txt", b"hello\n")], false);
    let store = inline_store();
    let path = fx.root.join("a.txt");
    let a = store.open(&path, &fx.project).unwrap();
    // A differently spelled path resolves to the same document.
    let b = store
        .open(&fx.root.join(".").join("a.txt"), &fx.project)
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(store.documents().len(), 1);

    // View A types; view B catches up from its last seen seq.
    let (va, vb) = (EditOrigin::new(), EditOrigin::new());
    let seen_b = b.buffer_view().unwrap().seq;
    let r = a
        .edit_from(va, a.seq(), |buf| {
            buf.set_cursor(5);
            ((), buf.insert(" world"))
        })
        .unwrap();
    assert!(r.catch_up.is_empty());
    assert_eq!(text(&b), "hello world\n");
    let up = b.catch_up(seen_b).unwrap();
    assert_eq!(up.entries.len(), 1);
    assert_eq!(up.entries[0].origin, va);
    assert_eq!(up.view.text.to_string(), "hello world\n");
    // View B edits without having caught up: it receives A's edit first.
    let r = b
        .edit_from(vb, seen_b, |buf| {
            buf.set_cursor(0);
            ((), buf.insert(">"))
        })
        .unwrap();
    assert_eq!(r.catch_up.len(), 1);
    assert_eq!(text(&a), ">hello world\n");
    assert!(a.is_dirty());
    assert_eq!(store.dirty_documents().len(), 1);

    // Dropping every handle forgets the document: reopening reads the disk.
    drop((a, b, r, up));
    assert!(store.get(&path).is_none());
    let c = store.open(&path, &fx.project).unwrap();
    assert_eq!(text(&c), "hello\n");
}

#[test]
fn save_keeps_encoding_and_line_endings_and_checks_the_stamp() {
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("a\r\nb\r\n".encode_utf16().flat_map(|u| u.to_le_bytes()))
        .collect();
    let fx = fixture(
        &[
            ("crlf.txt", b"one\r\ntwo\r\n"),
            ("u16.txt", &utf16),
            ("bom.txt", b"\xEF\xBB\xBFx\n"),
        ],
        false,
    );
    let store = inline_store();
    for name in ["crlf.txt", "u16.txt", "bom.txt"] {
        let path = fx.root.join(name);
        let before = std::fs::read(&path).unwrap();
        let h = store.open(&path, &fx.project).unwrap();
        assert_eq!(
            h.save(SaveRequest::default()).unwrap(),
            SaveOutcome::Unchanged
        );
        let len = h.text().unwrap().len();
        insert(&h, len, "z");
        h.save(SaveRequest::default()).unwrap();
        assert!(!h.is_dirty());
        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "{name}: prefix kept byte for byte"
        );
    }
    // Changed on disk behind our back: typed error, nothing written.
    let path = fx.root.join("crlf.txt");
    let h = store.open(&path, &fx.project).unwrap();
    insert(&h, 0, "mine ");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&path, "theirs, longer\r\n").unwrap();
    match h.save(SaveRequest::default()) {
        Err(DocError::ChangedOnDisk { .. }) => {}
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "theirs, longer\r\n"
    );
    h.save(SaveRequest {
        force: true,
        ..Default::default()
    })
    .unwrap();
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .starts_with("mine one"));
    assert_eq!(h.snapshot().save_count, 1);
}

#[test]
fn save_cleanup_edits_go_through_the_buffer() {
    let fx = fixture(&[("a.txt", b"x  \ny\t")], false);
    let store = inline_store();
    let h = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    h.save(SaveRequest {
        force: true,
        trim_trailing_whitespace: true,
        ensure_final_newline: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(text(&h), "x\ny\n");
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "x\ny\n"
    );
    assert!(!h.is_dirty(), "buffer equals disk");
}

#[test]
fn external_changes_reload_conflict_and_delete() {
    let fx = fixture(&[("a.rs", b"fn a() {}\n")], false);
    let store = inline_store();
    let path = fx.root.join("a.rs");
    let h = store.open(&path, &fx.project).unwrap();
    let (events, _sub) = record(&h);
    assert_eq!(h.check_disk().unwrap(), DiskChange::Unchanged);

    // Clean: reloaded in place, with a full parse.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&path, "fn b() {}\nfn c() {}\n").unwrap();
    assert_eq!(h.check_disk().unwrap(), DiskChange::Reloaded);
    assert_eq!(text(&h), "fn b() {}\nfn c() {}\n");
    let snap = h.snapshot();
    assert!(!snap.dirty && snap.syntax().is_some() && !snap.syntax_pending);
    assert_eq!(h.outline().value.unwrap().len(), 2);
    assert!(events
        .lock()
        .iter()
        .any(|e| matches!(e, DocEvent::Edited { origin, .. } if *origin == EditOrigin::RELOAD)));
    assert!(events
        .lock()
        .iter()
        .any(|e| matches!(e, DocEvent::Reloaded { .. })));

    // Dirty: conflict, buffer kept; keep mine clears it.
    insert(&h, 0, "// mine\n");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&path, "fn d() {}\n").unwrap();
    assert_eq!(h.check_disk().unwrap(), DiskChange::Conflict);
    let conflict = h.disk_state().conflict.expect("conflict");
    assert_eq!(conflict.disk.text().unwrap().to_string(), "fn d() {}\n");
    assert!(text(&h).starts_with("// mine"));
    h.keep_mine();
    assert!(h.disk_state().conflict.is_none());
    assert_eq!(h.save(SaveRequest::default()).unwrap(), SaveOutcome::Saved);

    // Deleted, then back.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(h.check_disk().unwrap(), DiskChange::Deleted);
    assert!(h.snapshot().disk.deleted);
    // Saving a deleted (clean) document recreates it.
    assert_eq!(h.save(SaveRequest::default()).unwrap(), SaveOutcome::Saved);
    assert!(path.exists() && !h.snapshot().disk.deleted);

    // Revert discards edits.
    insert(&h, 0, "junk");
    h.reload().unwrap();
    assert!(!h.is_dirty());
    assert!(text(&h).starts_with("// mine"));
}

#[test]
fn binary_files_are_not_editable() {
    let fx = fixture(&[("b.bin", b"\x00\x01\x02\x03")], false);
    let store = inline_store();
    let h = store.open(&fx.root.join("b.bin"), &fx.project).unwrap();
    assert!(!h.is_editable());
    assert!(matches!(h.apply_edits(vec![]), Err(DocError::NotEditable)));
    assert!(matches!(
        h.save(SaveRequest::default()),
        Err(DocError::NotEditable)
    ));
}

#[test]
fn read_only_blocks_edits() {
    let fx = fixture(&[("a.txt", b"x\n")], false);
    let store = inline_store();
    let h = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    let (events, _sub) = record(&h);
    h.set_read_only(true);
    assert!(matches!(h.replace_all("y"), Err(DocError::ReadOnly)));
    h.set_read_only(false);
    h.replace_all("y").unwrap();
    assert_eq!(text(&h), "y");
    assert!(events
        .lock()
        .iter()
        .any(|e| matches!(e, DocEvent::ReadOnlyChanged { read_only: true })));
    // Version-guarded edits.
    let v = h.version();
    insert(&h, 0, "a");
    assert!(matches!(
        h.apply_edits_at(v, vec![]),
        Err(DocError::VersionMismatch { .. })
    ));
}

#[test]
fn queries_recompute_only_on_version_change() {
    let fx = fixture(&[("a.rs", b"fn a() {}\nfn b() {\n    1\n}\n")], false);
    let store = inline_store();
    let h = store.open(&fx.root.join("a.rs"), &fx.project).unwrap();
    // Loaded with outline and folds precomputed.
    let r = h.outline();
    assert!(r.current);
    assert_eq!(r.value.unwrap().len(), 2);
    assert_eq!(h.compute_counts().structure, 0);
    insert(&h, 0, "fn z() {}\n");
    let r = h.outline();
    assert!(
        !r.current,
        "never answers from a computation it just scheduled"
    );
    // Inline executor: computed during that call.
    let r = h.outline();
    assert!(r.current);
    assert_eq!(r.value.unwrap().len(), 3);
    assert_eq!(h.compute_counts().structure, 1);
    let _ = h.outline();
    let _ = h.folds();
    assert_eq!(h.compute_counts().structure, 1, "same version: cached");
    assert_eq!(h.folds().value.unwrap().len(), 1);
    assert_eq!(h.scopes_at_row(3).len(), 1);

    // Highlights per chunk, cached by version.
    assert!(h.highlights(0..4).is_none() || h.compute_counts().highlight_chunks > 0);
    let lines = h.highlights(0..4).expect("computed inline");
    assert_eq!(lines.len(), 4);
    let n = h.compute_counts().highlight_chunks;
    let _ = h.highlights(0..4).unwrap();
    assert_eq!(h.compute_counts().highlight_chunks, n);
}

#[test]
fn gutter_and_blame_follow_edits_in_a_repo() {
    let fx = fixture(&[("a.txt", b"one\ntwo\nthree\n")], true);
    let store = inline_store();
    let h = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    let g = h.gutter();
    // Inline: computed during the call; the result is visible on the next one.
    let g = if g.current { g } else { h.gutter() };
    assert!(g.current && g.value.unwrap().is_empty());
    h.apply_edits(vec![TextEdit {
        range: 4..7,
        new_text: "TWO".into(),
    }])
    .unwrap();
    let _ = h.gutter();
    let g = h.gutter();
    assert!(g.current);
    let marks = g.value.unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].rows, 1..2);
    let computes = h.compute_counts().gutter;
    let _ = h.gutter();
    assert_eq!(h.compute_counts().gutter, computes);

    let _ = h.blame_map();
    let b = h.blame_map();
    assert!(b.current);
    let hunks = b.value.unwrap();
    assert!(
        hunks.iter().any(|x| x.uncommitted && x.lines == (1..2)),
        "{hunks:?}"
    );
    assert!(hunks.iter().any(|x| !x.uncommitted && x.lines.contains(&0)));
}

#[test]
fn diagnostics_are_mapped_through_edits() {
    let fx = fixture(&[("a.txt", b"let x = bad;\n")], false);
    let store = inline_store();
    let h = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    let tag = h.version();
    insert(&h, 0, "// c\n");
    let diag = Diagnostic {
        range: PointUtf16 { row: 0, column: 8 }..PointUtf16 { row: 0, column: 11 },
        severity: DiagnosticSeverity::Error,
        message: "bad".into(),
        source: None,
        code: None,
        tags: Vec::new(),
        related: Vec::new(),
    };
    h.set_diagnostics(Some(DocumentDiagnostics {
        path: fx.root.join("a.txt"),
        server: "test".into(),
        version: None,
        tag: Some(tag),
        diagnostics: vec![diag],
    }));
    let d = h.diagnostics();
    assert_eq!(d.len(), 1);
    assert_eq!(&text(&h)[d[0].range.clone()], "bad");
    insert(&h, 5, "    ");
    let d = h.diagnostics();
    assert_eq!(&text(&h)[d[0].range.clone()], "bad");
    h.set_diagnostics(None);
    assert!(h.diagnostics().is_empty());
}

#[test]
fn before_save_hooks_edit_then_save() {
    let fx = fixture(&[("a.txt", b"x\n")], false);
    let store = inline_store();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let id = store.register_before_save(Arc::new(move |doc: &DocSnapshot, _: &CancelToken| {
        c.fetch_add(1, Ordering::SeqCst);
        let len = doc.text().map_or(0, |t| t.len());
        Ok(vec![TextEdit {
            range: len..len,
            new_text: "formatted\n".into(),
        }])
    }));
    let h = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    insert(&h, 0, "y");
    h.save(SaveRequest {
        run_hooks: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(fx.root.join("a.txt")).unwrap(),
        "yx\nformatted\n"
    );
    assert!(!h.is_dirty());
    // Hooks off, or unregistered: not called.
    insert(&h, 0, "z");
    h.save(SaveRequest::default()).unwrap();
    store.unregister_before_save(id);
    insert(&h, 0, "w");
    h.save(SaveRequest {
        run_hooks: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn save_as_moves_the_document() {
    let fx = fixture(&[("a.txt", b"x\n")], false);
    let store = inline_store();
    let h = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    let (events, _sub) = record(&h);
    let to = fx.root.join("b.rs");
    h.save(SaveRequest {
        path: Some(to.clone()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(h.path(), to);
    assert!(store.get(&to).is_some());
    assert!(store.get(&fx.root.join("a.txt")).is_none());
    assert_eq!(
        h.live_document().language().map(|l| l.name.as_str()),
        Some("Rust")
    );
    assert!(events
        .lock()
        .iter()
        .any(|e| matches!(e, DocEvent::PathChanged { .. })));
}

#[test]
fn scheduler_cancels_superseded_work_and_notifies() {
    let mut src = String::new();
    for i in 0..400 {
        src.push_str(&format!("fn f{i}() {{ let x = {i}; }}\n"));
    }
    let fx = fixture(&[("big.rs", src.as_bytes())], false);
    let store = DocumentStore::new(Arc::new(Scheduler::new(2)));
    store.set_delays(QueryDelays {
        structure: Duration::from_millis(40),
        ..QueryDelays::IMMEDIATE
    });
    let h = store.open(&fx.root.join("big.rs"), &fx.project).unwrap();
    let (events, _sub) = record(&h);
    // A burst of edits, each asking for the outline: the debounce keeps only
    // the newest request.
    for i in 0..30 {
        insert(&h, 0, &format!("fn g{i}() {{}}\n"));
        let r = h.outline();
        assert!(!r.current, "never blocks for a result");
    }
    let target = h.version();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r = h.outline();
        if r.current {
            assert_eq!(r.version, target);
            assert_eq!(r.value.unwrap().len(), 430);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "outline never arrived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        h.compute_counts().structure <= 3,
        "superseded requests were dropped: {:?}",
        h.compute_counts()
    );
    assert!(events.lock().iter().any(|e| matches!(e, DocEvent::QueryReady { kind: QueryKind::Structure, version } if *version == target)));
    assert!(events
        .lock()
        .iter()
        .any(|e| matches!(e, DocEvent::Reparsed { .. })));
    // Highlights arrive by event for the current version.
    assert!(h.request_highlights(&[0]));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while h.highlight_chunk(0).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "highlights never arrived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn subscribers_may_call_back_into_the_handle() {
    let fx = fixture(&[("a.txt", b"x\n")], false);
    let store = inline_store();
    let h = store.open(&fx.root.join("a.txt"), &fx.project).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (h2, s2) = (h.clone(), seen.clone());
    let _sub = h.subscribe(move |ev| {
        if let DocEvent::Edited { version, .. } = ev {
            // Reading inside a callback must not deadlock.
            assert_eq!(h2.snapshot().version, *version);
            s2.lock().push(*version);
        }
    });
    insert(&h, 0, "a");
    insert(&h, 0, "b");
    assert_eq!(seen.lock().len(), 2);
}
