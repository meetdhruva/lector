//! Benchmarks of the structural diff, risk assessment and content identity.
//! `cargo bench -p lector-review --bench algo_bench` (the content cache lookups are in
//! `cargo bench -p lector-app --bench cache_bench`).

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use lector_diff::DiffOptions;
use lector_git::{ChangeKind, FileChange, Side};
use lector_review::review::diff_bytes;
use lector_review::risk;
use lector_review::structural::{self, Budget};
use lector_review::{ContentId, NoCache};
use lector_syntax::{LanguageRegistry, Syntax};
use lector_text::Text;

const RUST_SMALL: &str = include_str!("../../lector-app/src/project/navigation.rs");
const RUST_MEDIUM: &str = include_str!("../../lector-app/src/project/symbols.rs");
const RUST_LARGE: &str = include_str!("../../lector-app/src/project.rs");

/// A realistic agent-style edit: renames, a few literal tweaks, one inserted function.
fn edit(src: &str) -> String {
    let mut out = src
        .replace("let ", "let mut ")
        .replacen("let mut ", "let ", 3);
    out = out.replacen("true", "false", 2);
    out = out.replacen("0", "1", 4);
    if let Some(at) = out.find("\nfn ").or_else(|| out.find("\npub fn ")) {
        out.insert_str(at, "\nfn inserted_by_agent(a: usize, b: usize) -> usize {\n    if a > b { a - b } else { b - a }\n}\n");
    }
    out
}

fn bench(c: &mut Criterion) {
    let langs = LanguageRegistry::new();
    let rust = langs.by_name("rust").unwrap();
    let mut group = c.benchmark_group("structural");
    group.sample_size(15);
    for (name, src) in [
        ("small", RUST_SMALL),
        ("medium", RUST_MEDIUM),
        ("large", RUST_LARGE),
    ] {
        let new = edit(src);
        let (old_t, new_t) = (Text::from(src), Text::from(new.as_str()));
        let (so, sn) = (
            Syntax::parse(&rust, &old_t).unwrap(),
            Syntax::parse(&rust, &new_t).unwrap(),
        );
        let budget = Budget::default();
        let probe = structural::diff(&so, &old_t, &sn, &new_t, &budget);
        eprintln!(
            "structural {name}: {} lines, result: {}",
            src.lines().count(),
            probe
                .as_ref()
                .map_or("over budget".to_string(), |d| format!(
                    "{} old / {} new marks, {} states",
                    d.old.len(),
                    d.new.len(),
                    d.states
                ))
        );
        group.bench_function(
            format!("diff {name} ({} lines, small edit)", src.lines().count()),
            |b| b.iter(|| structural::diff(&so, &old_t, &sn, &new_t, &budget)),
        );
    }
    group.finish();

    // Risk assessment of the medium file edit.
    let new = edit(RUST_MEDIUM);
    let change = FileChange {
        old_path: Some("src/symbols.rs".into()),
        new_path: Some("src/symbols.rs".into()),
        kind: ChangeKind::Modified,
        old: Side::Workdir("src/symbols.rs".into()),
        new: Side::Workdir("src/symbols.rs".into()),
        binary: None,
    };
    let fd = diff_bytes(
        change,
        Some(RUST_MEDIUM.as_bytes().to_vec()),
        Some(new.into_bytes()),
        &langs,
        &DiffOptions::default(),
        &NoCache,
    );
    c.bench_function("risk/assess medium file (tree-sitter API compare)", |b| {
        b.iter(|| risk::assess(&fd, &langs))
    });

    // Content identity (the cache key).
    let text = Text::from(RUST_LARGE);
    c.bench_function("cache/ContentId::of_text 60 KB", |b| {
        b.iter(|| ContentId::of_text(&text))
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_millis(500)).measurement_time(Duration::from_secs(2));
    targets = bench
}
criterion_main!(benches);
