//! Benchmarks of the content cache: parse miss vs hit.
//! `cargo bench -p lector-app --bench cache_bench` (moved from the old `algo_bench`; the
//! structural diff and risk benchmarks are in `cargo bench -p lector-review --bench algo_bench`).

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use lector_app::content_cache::ContentCache;
use lector_review::ContentId;
use lector_syntax::{LanguageRegistry, Syntax};
use lector_text::Text;

const RUST_LARGE: &str = include_str!("../src/project.rs");

fn bench(c: &mut Criterion) {
    let langs = LanguageRegistry::new();
    let rust = langs.by_name("rust").unwrap();
    let text = Text::from(RUST_LARGE);
    let id = ContentId::of_text(&text);
    c.bench_function("cache/syntax parse (miss)", |b| {
        b.iter(|| Syntax::parse(&rust, &text).unwrap())
    });
    let cache = ContentCache::default();
    cache.syntax(id, &rust, &text).unwrap();
    c.bench_function("cache/syntax lookup (hit)", |b| {
        b.iter(|| cache.syntax(id, &rust, &text).unwrap())
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_millis(500)).measurement_time(Duration::from_secs(2));
    targets = bench
}
criterion_main!(benches);
