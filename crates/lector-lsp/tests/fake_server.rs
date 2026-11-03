//! Integration tests against `tests/fixtures/fake_lsp.py`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::executor::block_on;
use lector_lsp::{LspManager, LspOptions, PointUtf16, ServerConfig, ServerState};

fn script() -> String {
    format!("{}/tests/fixtures/fake_lsp.py", env!("CARGO_MANIFEST_DIR"))
}

fn have_python() -> bool {
    lector_lsp::which("python3").is_some()
}

fn config(extra: &[&str]) -> ServerConfig {
    let mut args = vec![script()];
    args.extend(extra.iter().map(|s| s.to_string()));
    ServerConfig {
        name: "fake".into(),
        command: "python3".into(),
        args,
        languages: vec!["Rust".into()],
        root_markers: vec![],
    }
}

fn fast() -> LspOptions {
    LspOptions {
        request_timeout: Duration::from_millis(600),
        init_timeout: Duration::from_secs(10),
        restart_base_delay: Duration::from_millis(50),
        max_restarts: 3,
        shutdown_grace: Duration::from_secs(2),
    }
}

fn file(dir: &Path) -> PathBuf {
    let p = dir.join("main.rs");
    std::fs::write(&p, "fn main() {}\n").unwrap();
    p
}

fn log_lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
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

#[test]
fn hover_definition_references_symbols() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = file(dir.path());
    let m = LspManager::with_options(dir.path().into(), vec![config(&[])], fast());
    assert!(m.status().is_empty());
    m.open_document(&f, "Rust", "fn main() {}\n");
    let h = block_on(m.hover(&f, pt(2, 4))).unwrap().unwrap();
    assert_eq!(h.markdown, "hover 2:4");
    assert_eq!(h.range.unwrap(), pt(2, 4)..pt(2, 5));
    let d = block_on(m.definition(&f, pt(1, 0))).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].range.start.row, 2);
    assert_eq!(d[0].path.file_name().unwrap(), "main.rs");
    let r = block_on(m.references(&f, pt(1, 0))).unwrap();
    assert_eq!(r.len(), 2);
    let s = block_on(m.document_symbols(&f)).unwrap();
    assert_eq!(s[0].name, "Outer");
    assert_eq!(s[0].children[0].name, "inner");
    let w = block_on(m.workspace_symbols("abc")).unwrap();
    assert_eq!(w[0].name, "abc_sym");
    assert_eq!(w[0].container.as_deref(), Some("mod"));
    assert_eq!(m.status()[0].state, ServerState::Running);
    m.shutdown();
}

#[test]
fn requests_queue_during_init_and_documents_dedup() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = file(dir.path());
    let log = dir.path().join("log.txt");
    let cfg = config(&[
        "--init-delay",
        "0.5",
        "--log",
        log.to_str().unwrap(),
        "--probe-config",
    ]);
    let m = LspManager::with_options(dir.path().into(), vec![cfg], fast());
    m.open_document(&f, "Rust", "a");
    m.open_document(&f, "Rust", "a"); // dedup
    assert_eq!(m.status()[0].state, ServerState::Starting);
    let t = Instant::now();
    let fut = m.hover(&f, pt(0, 0)); // issued while initializing
    let h = block_on(fut).unwrap().unwrap();
    assert!(t.elapsed() >= Duration::from_millis(300));
    assert_eq!(h.markdown, "hover 0:0");
    m.open_document(&f, "Rust", "b"); // change
    m.close_document(&f);
    m.shutdown();
    wait_for(|| log_lines(&log).iter().any(|l| l == "exit"));
    let lines = log_lines(&log);
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("open ")).count(),
        1,
        "{lines:?}"
    );
    assert!(
        lines.contains(&"open main.rs v1 rust".to_string()),
        "{lines:?}"
    );
    assert!(lines.contains(&"change v2".to_string()), "{lines:?}");
    assert!(
        lines.contains(&"textDocument/didClose".to_string()),
        "{lines:?}"
    );
    let i = |n: &str| lines.iter().position(|l| l == n).unwrap();
    assert!(i("initialize") < i("initialized") && i("initialized") < i("textDocument/didOpen"));
    assert!(i("textDocument/didOpen") < i("textDocument/hover"));
    // Server->client requests answered (config nulls).
    assert!(
        lines.contains(&"response [null, null]".to_string()),
        "{lines:?}"
    );
}

#[test]
fn request_timeout() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = file(dir.path());
    let m = LspManager::with_options(dir.path().into(), vec![config(&["--hang-hover"])], fast());
    m.open_document(&f, "Rust", "x");
    let t = Instant::now();
    let r = block_on(m.hover(&f, pt(0, 0)));
    assert!(matches!(r, Err(lector_lsp::LspError::Timeout(_))), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5));
    // Server still usable for other requests.
    assert!(block_on(m.definition(&f, pt(0, 0))).is_ok());
    m.shutdown();
}

#[test]
fn crash_and_restart_reopens_documents() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = file(dir.path());
    let marker = dir.path().join("crashed");
    let log = dir.path().join("log.txt");
    let cfg = config(&[
        "--crash-marker",
        marker.to_str().unwrap(),
        "--log",
        log.to_str().unwrap(),
    ]);
    let m = LspManager::with_options(dir.path().into(), vec![cfg], fast());
    m.open_document(&f, "Rust", "x");
    // First hover crashes the server.
    assert!(block_on(m.hover(&f, pt(0, 0))).is_err());
    // After the backoff the server is back and the doc was re-opened.
    wait_for(|| {
        m.status()[0].state == ServerState::Running
            && log_lines(&log).iter().filter(|l| *l == "start").count() == 2
    });
    let h = block_on(m.hover(&f, pt(1, 1))).unwrap().unwrap();
    assert_eq!(h.markdown, "hover 1:1");
    let opens = log_lines(&log)
        .iter()
        .filter(|l| l.starts_with("open "))
        .count();
    assert_eq!(opens, 2);
    m.shutdown();
}

#[test]
fn repeated_crash_marks_failed() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = file(dir.path());
    let m = LspManager::with_options(dir.path().into(), vec![config(&["--always-crash"])], fast());
    m.open_document(&f, "Rust", "x");
    wait_for(|| matches!(m.status()[0].state, ServerState::Failed(_)));
    assert_eq!(block_on(m.hover(&f, pt(0, 0))).unwrap(), None);
}

#[test]
fn server_not_installed() {
    let dir = tempfile::tempdir().unwrap();
    let f = file(dir.path());
    let mut cfg = config(&[]);
    cfg.command = "lector-no-such-server-binary".into();
    let m = LspManager::with_options(dir.path().into(), vec![cfg], fast());
    m.open_document(&f, "Rust", "x");
    let st = m.status();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].state, ServerState::NotInstalled);
    assert_eq!(block_on(m.hover(&f, pt(0, 0))).unwrap(), None);
    assert!(block_on(m.definition(&f, pt(0, 0))).unwrap().is_empty());
    assert!(block_on(m.workspace_symbols("x")).unwrap().is_empty());
    // Unknown language / unopened file.
    m.open_document(&f, "Cobol", "x");
    assert_eq!(
        block_on(m.hover(&dir.path().join("zz"), pt(0, 0))).unwrap(),
        None
    );
}

#[test]
fn falls_back_to_next_installed_config() {
    if !have_python() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let f = file(dir.path());
    let mut missing = config(&[]);
    missing.name = "missing".into();
    missing.command = "lector-no-such-server-binary".into();
    let m = LspManager::with_options(dir.path().into(), vec![missing, config(&[])], fast());
    m.open_document(&f, "Rust", "x");
    assert!(block_on(m.hover(&f, pt(0, 0))).unwrap().is_some());
    let st = m.status();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].name, "fake");
    m.shutdown();
}

#[test]
#[ignore = "needs rust-analyzer on PATH"]
fn rust_analyzer_hover() {
    let works = std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !works {
        return; // absent, or a rustup proxy stub without the component
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let src = "/// Adds.\nfn add(a: i32, b: i32) -> i32 { a + b }\nfn main() { add(1, 2); }\n";
    let f = dir.path().join("src/main.rs");
    std::fs::write(&f, src).unwrap();
    let opts = LspOptions {
        request_timeout: Duration::from_secs(60),
        ..LspOptions::default()
    };
    let m = LspManager::with_options(dir.path().into(), lector_lsp::builtin_servers(), opts);
    m.open_document(&f, "Rust", src);
    let mut hover = None;
    for _ in 0..30 {
        if let Ok(Some(h)) = block_on(m.hover(&f, pt(2, 12))) {
            hover = Some(h);
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    assert!(hover.unwrap().markdown.contains("add"));
    let defs = block_on(m.definition(&f, pt(2, 12))).unwrap();
    assert_eq!(defs[0].range.start.row, 1);
    m.shutdown();
}
