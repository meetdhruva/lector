//! Structural diff scaling: files of 500 / 3 000 / 10 000 lines with a realistic
//! agent-style edit (about 50 changed lines spread over the file).
//! `cargo bench -p lector-workspace --bench structural_bench`
//! (custom harness; `STRUCTURAL_BENCH_REPS=n` sets the repetitions)

use std::time::{Duration, Instant};

use lector_syntax::{LanguageRegistry, Syntax};
use lector_workspace::structural::{self, Budget};

/// One generated function (about 14 lines).
fn function(i: usize) -> String {
    format!(
        "pub fn compute_{i}(input: &[u32], scale: u32) -> Result<u32, String> {{\n\
         \x20   let mut total = 0u32;\n\
         \x20   for (index, value) in input.iter().enumerate() {{\n\
         \x20       if *value > {k} {{\n\
         \x20           total += value * scale;\n\
         \x20       }} else {{\n\
         \x20           total += index as u32;\n\
         \x20       }}\n\
         \x20   }}\n\
         \x20   if total == 0 {{\n\
         \x20       return Err(format!(\"empty input for compute_{i}\"));\n\
         \x20   }}\n\
         \x20   Ok(total + {i})\n\
         }}\n\n",
        k = i % 17 + 3
    )
}

/// A file of about `lines` lines.
fn file(lines: usize) -> String {
    let mut out = String::from("use std::fmt;\n\n");
    let mut i = 0;
    while out.lines().count() < lines {
        out.push_str(&function(i));
        i += 1;
    }
    out
}

/// About `changes` changed lines spread evenly: renames, literal tweaks, inserted and removed statements.
fn edit(src: &str, changes: usize) -> String {
    let count = src.matches("pub fn compute_").count();
    let step = (count / changes.max(1)).max(1);
    let mut out = String::with_capacity(src.len() + 4096);
    let mut idx = 0usize;
    for block in src.split("\n\n") {
        if block.starts_with("pub fn compute_") && idx % step == 0 {
            let edited = match (idx / step) % 4 {
                0 => block.replace("total", "sum"),
                1 => block.replace("scale", "factor"),
                2 => block.replacen(
                    "total += index as u32;",
                    "total += index as u32;\n        total += 1;",
                    1,
                ),
                _ => block
                    .replacen("    let mut total = 0u32;\n", "", 1)
                    .replacen("total += value * scale;", "total += value;", 1),
            };
            out.push_str(&edited);
        } else {
            out.push_str(block);
        }
        out.push_str("\n\n");
        if block.starts_with("pub fn compute_") {
            idx += 1;
        }
    }
    out.push_str("pub fn added_at_the_end(a: u32) -> u32 {\n    a + 1\n}\n");
    out
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn main() {
    let reps: usize = std::env::var("STRUCTURAL_BENCH_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let langs = LanguageRegistry::new();
    let Some(rust) = langs.by_name("rust") else {
        eprintln!("rust grammar missing");
        return;
    };
    println!("structural diff, ~50 changed lines, median of {reps} runs (parse excluded)");
    println!(
        "{:>8} {:>8} {:>12} {:>12} {:>12} {:>10} {:>8}",
        "lines", "changed", "structural", "line diff", "result", "states", "skipped"
    );
    for lines in [500usize, 3_000, 10_000] {
        let old = file(lines);
        let new = edit(&old, 12);
        let changed = {
            let d = lector_diff::diff_lines(&old, &new, &lector_diff::DiffOptions::default());
            d.stats().added + d.stats().removed
        };
        let (Ok(so), Ok(sn)) = (
            Syntax::parse(&rust, &old[..]),
            Syntax::parse(&rust, &new[..]),
        ) else {
            eprintln!("parse failed");
            continue;
        };
        let budget = Budget::default();
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..reps {
            let t = Instant::now();
            last = structural::diff(&so, &old[..], &sn, &new[..], &budget);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let (result, states, skipped) = match &last {
            Some(d) => ("ok", d.states, skipped_of(d)),
            None => ("over budget", 0, 0),
        };
        if std::env::var("SHOW_SKIPPED").is_ok() {
            if let Some(d) = &last {
                for r in d.skipped.iter().take(4) {
                    eprintln!(
                        "skipped {:?} old {:?}: {:?}",
                        r.reason,
                        r.old,
                        old[r.old.clone()].chars().take(160).collect::<String>()
                    );
                }
            }
        }
        let mut line_times = Vec::new();
        for _ in 0..reps {
            let t = Instant::now();
            let d = lector_diff::diff_lines(&old, &new, &lector_diff::DiffOptions::default());
            std::hint::black_box(d);
            line_times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "{:>8} {:>8} {:>10.1}ms {:>10.1}ms {:>12} {:>10} {:>8}",
            old.lines().count(),
            changed,
            median(times),
            median(line_times),
            result,
            states,
            skipped
        );
        let _ = Duration::ZERO;
    }
}

fn skipped_of(d: &structural::StructuralDiff) -> usize {
    d.skipped.len()
}
