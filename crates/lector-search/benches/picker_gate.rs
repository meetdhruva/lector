//! Fuzzy file finder gate: < 50 ms to first results per keystroke on 1M paths.
//! `cargo bench -p lector-search --bench picker_gate`
//! (custom harness: prints the per-keystroke timings)

use std::sync::Arc;
use std::time::{Duration, Instant};

use lector_search::Picker;

fn synthetic(n: usize) -> Vec<String> {
    let dirs = [
        "src", "crates", "tests", "docs", "vendor", "lib", "app", "core",
    ];
    (0..n)
        .map(|i| {
            format!(
                "{}/{}/{}/module_{}_{}.rs",
                dirs[i % 8],
                dirs[(i / 8) % 8],
                dirs[(i / 64) % 8],
                i % 1000,
                i
            )
        })
        .collect()
}

fn run(paths: &[String], query: &str) {
    let mut p: Picker<u32> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    let t0 = Instant::now();
    for (i, s) in paths.iter().enumerate() {
        inj.push(i as u32, s);
    }
    let inject = t0.elapsed();
    p.set_query("");
    while p.tick(5) {}
    println!(
        "\nquery `{query}` typed keystroke by keystroke over {} paths (inject {inject:?})",
        paths.len()
    );
    println!(
        "{:>14} {:>12} {:>12} {:>10}",
        "query", "first", "settled", "matched"
    );
    for end in 1..=query.len() {
        let q = &query[..end];
        let t = Instant::now();
        p.set_query(q);
        let mut first = None;
        loop {
            let running = p.tick(1);
            if first.is_none() && !p.matches(0..1).is_empty() {
                first = Some(t.elapsed());
            }
            if !running {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(30));
        }
        let settled = t.elapsed();
        let top = p.matches(0..50);
        println!(
            "{:>14} {:>12?} {:>12?} {:>10}  top: {}",
            q,
            first.unwrap_or(settled),
            settled,
            p.matched_count(),
            top.first()
                .map(|m| paths[*m.item as usize].as_str())
                .unwrap_or("-")
        );
    }
    // Backspacing is served from the cached match sets.
    for end in (1..query.len()).rev() {
        let q = &query[..end];
        let t = Instant::now();
        p.set_query(q);
        while p.tick(1) {}
        println!("{:>14} backspace settled {:?}", q, t.elapsed());
    }
}

fn main() {
    let paths = synthetic(1_000_000);
    run(&paths, "crate mod12");
    run(&paths, "src/mod999");
}
