//! Release-gate measurement: open a 1 GB log file through the workspace model
//! (`Project::load_document`, as a file tab does) and serve the first screen.
//!
//! Ignored by default (writes 1 GB). Run in release mode:
//! `LECTOR_PERF_DIR=/tmp [LECTOR_PERF_DROP_CACHES=1] cargo test -p lector-workspace --release --test perf -- --ignored --nocapture`

use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use lector_config::Settings;
use lector_workspace::project::Project;

#[test]
#[ignore = "writes a 1 GB file; run explicitly in release mode"]
fn open_1gb_log_first_screen() {
    let base = std::env::var_os("LECTOR_PERF_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::tempdir_in(base).expect("tempdir");
    let path = dir.path().join("big.log");
    {
        let file = std::fs::File::create(&path).expect("create");
        let mut w = BufWriter::with_capacity(1 << 20, file);
        let mut written = 0usize;
        let mut i = 0u64;
        while written < 1 << 30 {
            let line = format!(
                "2026-11-03T12:{:02}:{:02}.{:03}Z INFO  request id={i} path=/api/v1/items/{} status=200 took={}ms ✓\n",
                (i / 60_000) % 60,
                (i / 1000) % 60,
                i % 1000,
                i % 9973,
                i % 250
            );
            w.write_all(line.as_bytes()).expect("write");
            written += line.len();
            i += 1;
        }
        w.flush().expect("flush");
    }
    let mut settings = Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(dir.path(), settings).expect("project");

    // Run 0 is cold when LECTOR_PERF_DROP_CACHES=1 (Linux, root: drops the page
    // cache first); later runs are warm.
    if std::env::var_os("LECTOR_PERF_DROP_CACHES").is_some() {
        let _ = std::process::Command::new("sync").status();
        if let Err(err) = std::fs::write("/proc/sys/vm/drop_caches", "3") {
            println!("cannot drop caches: {err}");
        }
    }
    for run in 0..3 {
        let t = Instant::now();
        let doc = project.load_document(Path::new("big.log")).expect("load");
        let loaded = t.elapsed();
        let text = doc.text().expect("text");
        let first: Vec<String> = text.lines(0..80).map(|l| l.into_owned()).collect();
        let _ = doc.line_highlights(0..80);
        let first_screen = t.elapsed();
        let last_row = doc.line_count().saturating_sub(1);
        let t2 = Instant::now();
        let _tail: Vec<String> = text
            .lines(last_row.saturating_sub(80)..last_row + 1)
            .map(|l| l.into_owned())
            .collect();
        let jump_to_end = t2.elapsed();
        assert_eq!(first.len(), 80);
        assert!(doc.is_large());
        println!(
            "run {run}: 1 GB log load_document {loaded:?}, first screen {first_screen:?}, \
             jump to end {jump_to_end:?}, lines {}",
            doc.line_count()
        );
    }
}
