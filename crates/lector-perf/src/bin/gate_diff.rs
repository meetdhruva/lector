//! Instruction-count gates for `lector-diff`: the line diff of two ~5k-line files and the
//! word-level refinement of the changed lines.

use std::hint::black_box;
use std::process::ExitCode;

use lector_diff::{diff_lines, refine_inline, DiffOptions};
use lector_perf::harness::{gate_main, work, Bench, Work};
use lector_perf::inputs::{edited_copy, rust_like_source};

fn pair() -> (String, String) {
    let old = rust_like_source(5_000, 31);
    let new = edited_copy(&old, 6, 32);
    (old, new)
}

/// Histogram line diff of two ~5k-line files with about 6% of the lines edited.
fn line_diff_5k() -> Work {
    let (old, new) = pair();
    work(move || {
        let diff = diff_lines(&old, &new, &DiffOptions::default());
        black_box((diff.hunks.len(), diff.stats()));
    })
}

/// Word-level refinement of every replaced line pair of that diff.
fn word_refine() -> Work {
    let (old, new) = pair();
    let diff = diff_lines(&old, &new, &DiffOptions::default());
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut pairs = Vec::new();
    for h in &diff.hunks {
        for (o, n) in h.old.clone().zip(h.new.clone()) {
            if let (Some(a), Some(b)) = (old_lines.get(o as usize), new_lines.get(n as usize)) {
                pairs.push((a.to_string(), b.to_string()));
            }
        }
    }
    work(move || {
        let mut changed = 0usize;
        for (a, b) in &pairs {
            let r = refine_inline(a, b);
            changed += usize::from(r != Default::default());
        }
        black_box((pairs.len(), changed));
    })
}

fn main() -> ExitCode {
    gate_main(
        "diff",
        &[
            Bench::new("diff/line_diff_5k", line_diff_5k),
            Bench::new("diff/word_refine", word_refine),
        ],
    )
}
