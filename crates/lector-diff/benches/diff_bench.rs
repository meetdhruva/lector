use criterion::{criterion_group, criterion_main, Criterion};
use lector_diff::{diff_lines, DiffOptions};

fn make(n: usize, change_every: usize, tweak: bool) -> String {
    let mut s = String::new();
    for i in 0..n {
        if tweak && i % change_every == 0 {
            s.push_str(&format!("    let changed_{i} = compute({i}) + 1;\n"));
        } else {
            s.push_str(&format!("    let value_{i} = compute({i}); // line {i}\n"));
        }
    }
    s
}

fn bench(c: &mut Criterion) {
    let old = make(10_000, 20, false);
    let new = make(10_000, 20, true);
    c.bench_function("diff 10k lines 5% changed", |b| {
        b.iter(|| diff_lines(&old, &new, &DiffOptions::default()))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
