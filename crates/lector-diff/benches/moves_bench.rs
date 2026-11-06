//! Benchmarks of moved-code detection and line re-anchoring.
//! `cargo bench -p lector-diff --bench moves_bench`

use criterion::{criterion_group, criterion_main, Criterion};
use lector_diff::moves::{detect_moved, detect_moved_across};
use lector_diff::remap::{reanchor, Anchor, LineMap};
use lector_diff::{diff_lines, DiffOptions};

fn function(i: usize) -> String {
    format!(
        "fn handler_{i}(request: &Request) -> Response {{\n    let user = lookup_user_{i}(request.user_id);\n    let payload = serialize_payload(&user, {i});\n    if payload.is_empty() {{\n        return Response::empty();\n    }}\n    Response::ok(payload)\n}}\n\n"
    )
}

/// A file of `n` functions; `moved` of them are taken out and re-inserted elsewhere in the
/// new version (deterministic scatter), so the diff has many hunks.
fn pair(n: usize, moved: usize) -> (String, String) {
    let fs: Vec<String> = (0..n).map(function).collect();
    let old: String = fs.concat();
    let mut order: Vec<usize> = (0..n).collect();
    for j in 0..moved {
        let from = (j * 37 + 11) % order.len();
        let f = order.remove(from);
        let to = (j * 101 + 7) % (order.len() + 1);
        order.insert(to, f);
    }
    let new: String = order.iter().map(|&i| fs[i].as_str()).collect();
    (old, new)
}

fn bench(c: &mut Criterion) {
    let (old, new) = pair(1500, 60); // ~13k lines
    let d = diff_lines(&old, &new, &DiffOptions::default());
    eprintln!(
        "moves input: {} lines, {} hunks",
        old.lines().count(),
        d.hunks.len()
    );
    c.bench_function("moves/detect_moved 13k lines, 30 scattered moves", |b| {
        b.iter(|| detect_moved(&old, &new, &d, 3))
    });
    let moved = detect_moved(&old, &new, &d, 3);
    eprintln!(
        "moved blocks found: {} (of 60 scattered moves)",
        moved.len()
    );
    c.bench_function("moves/LineMap::new (incl. moves)", |b| {
        b.iter(|| LineMap::new(&old, &new, &d))
    });
    let map = LineMap::new(&old, &new, &d);
    c.bench_function("remap/LineMap::map x13k", |b| {
        b.iter(|| {
            (0..13_000u32).fold(0u32, |acc, l| {
                acc.wrapping_add(map.map(l).line().unwrap_or(0))
            })
        })
    });
    // Cross-file: functions leave file A and arrive in file B.
    let a_old: String = (0..800).map(function).collect();
    let a_new: String = (100..800).map(function).collect();
    let b_old: String = (800..1000).map(function).collect();
    let b_new: String = b_old.clone() + &(0..100).map(function).collect::<String>();
    let da = diff_lines(&a_old, &a_new, &DiffOptions::default());
    let db = diff_lines(&b_old, &b_new, &DiffOptions::default());
    c.bench_function(
        "moves/detect_moved_across 2 files, 100 functions moved",
        |b| b.iter(|| detect_moved_across(&[(&a_old, &a_new, &da), (&b_old, &b_new, &db)])),
    );
    let anchor = Anchor::capture(&old, 5000, 3).unwrap();
    let shifted = format!("// header\n// header\n{new}");
    c.bench_function("remap/reanchor in 13k-line text (moved line)", |b| {
        b.iter(|| reanchor(&anchor, &shifted))
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).warm_up_time(std::time::Duration::from_millis(500)).measurement_time(std::time::Duration::from_secs(2));
    targets = bench
}
criterion_main!(benches);
