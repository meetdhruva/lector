//! Trigram index benchmarks over a synthetic in-memory corpus.
//! `cargo bench -p lector-search --bench search_bench`

use criterion::{criterion_group, criterion_main, Criterion};
use lector_search::TrigramIndex;

fn corpus(files: usize) -> Vec<(String, Vec<u8>)> {
    let words = [
        "fn",
        "let",
        "struct",
        "impl",
        "match",
        "return",
        "self",
        "config",
        "buffer",
        "render",
        "document",
        "workspace",
        "project",
        "syntax",
        "tree",
        "node",
        "range",
        "offset",
        "line",
        "column",
        "snapshot",
        "version",
        "cache",
        "index",
        "query",
        "result",
        "error",
        "option",
    ];
    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    (0..files)
        .map(|i| {
            let mut text = String::new();
            for _ in 0..(400 + next() % 800) {
                text.push_str(words[(next() % words.len() as u64) as usize]);
                text.push(if next() % 9 == 0 { '\n' } else { ' ' });
                if next() % 50 == 0 {
                    text.push_str(&format!("ident_{}_{} ", next() % 5000, next() % 97));
                }
            }
            (format!("src/mod{}/file{}.rs", i % 40, i), text.into_bytes())
        })
        .collect()
}

fn bench(c: &mut Criterion) {
    let files = corpus(3000);
    let total: usize = files.iter().map(|(_, b)| b.len()).sum();
    eprintln!(
        "corpus: {} files, {:.1} MB",
        files.len(),
        total as f64 / 1e6
    );
    c.bench_function("trigram/build 3000 files (in memory, sequential)", |b| {
        b.iter(|| TrigramIndex::from_contents(files.clone()))
    });
    let idx = TrigramIndex::from_contents(files.clone());
    let bytes = idx.to_bytes();
    eprintln!(
        "index: {} trigrams, serialized {:.2} MB ({:.1}% of corpus)",
        idx.trigram_count(),
        bytes.len() as f64 / 1e6,
        bytes.len() as f64 * 100.0 / total as f64
    );
    c.bench_function("trigram/serialize", |b| b.iter(|| idx.to_bytes()));
    c.bench_function("trigram/deserialize", |b| {
        b.iter(|| TrigramIndex::from_bytes(&bytes).unwrap())
    });
    c.bench_function("trigram/query literal (rare ident)", |b| {
        b.iter(|| idx.candidates_literal("ident_4242_7"))
    });
    c.bench_function("trigram/query literal (common word)", |b| {
        b.iter(|| idx.candidates_literal("snapshot"))
    });
    c.bench_function("trigram/query regex a.*b", |b| {
        b.iter(|| idx.candidates_regex("ident_4242_[0-9]+ .*render").unwrap())
    });
    c.bench_function("trigram/query regex alternation", |b| {
        b.iter(|| {
            idx.candidates_regex("ident_4242_7|ident_4243_8|zzzcache")
                .unwrap()
        })
    });
    let mut one = files[17].1.clone();
    one.extend_from_slice(b" changed");
    let mut idx2 = TrigramIndex::from_contents(files.clone());
    c.bench_function("trigram/update one file", |b| {
        b.iter(|| idx2.update_bytes("src/mod17/file17.rs", &one, 1))
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).warm_up_time(std::time::Duration::from_millis(500)).measurement_time(std::time::Duration::from_secs(2));
    targets = bench
}
criterion_main!(benches);
