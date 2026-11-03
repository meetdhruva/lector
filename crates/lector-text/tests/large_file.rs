//! Release-gate check: open a 1 GB file and serve the first screen in < 500 ms.
//!
//! Ignored by default (writes 1 GB to the temp dir). Run with:
//! `cargo test -p lector-text --release --test large_file -- --ignored --nocapture`

use std::io::{BufWriter, Write};
use std::time::Instant;

use lector_text::Text;

#[test]
#[ignore = "writes a 1 GB file; run explicitly"]
fn open_1gb_first_screen_under_500ms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("big.txt");
    {
        let file = std::fs::File::create(&path).expect("create");
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        let line = "The quick brown fox jumps over the lazy dog; 0123456789 é\n";
        let block = line.repeat((1 << 20) / line.len());
        let mut written = 0usize;
        while written < 1 << 30 {
            writer.write_all(block.as_bytes()).expect("write");
            written += block.len();
        }
        writer.flush().expect("flush");
    }

    let started = Instant::now();
    let text = Text::open_mapped(&path).expect("open");
    let first_screen: Vec<_> = text.lines(0..80).map(|line| line.into_owned()).collect();
    let elapsed = started.elapsed();
    let mapped = text.as_mapped().expect("mapped");
    println!(
        "1 GB open + first screen: {elapsed:?}; lines = {}; index = {} KiB ({:.3}% of file)",
        text.line_count(),
        mapped.index_memory() / 1024,
        mapped.index_memory() as f64 * 100.0 / text.len() as f64
    );
    assert_eq!(first_screen.len(), 80);
    assert!(elapsed.as_millis() < 500, "took {elapsed:?}");
    let last = text.line_count() - 1;
    assert!(text.line(last).is_empty());
    assert!(text.line(last - 1).starts_with("The quick"));
}
