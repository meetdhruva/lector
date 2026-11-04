//! Quick benchmarks for the editing hot paths.
//!
//! `cargo bench -p lector-buffer` (targets: insert one char in a 100k-line
//! buffer < 50 µs; typing with 1000 carets < 5 ms; select all occurrences with
//! 10k matches reasonable).

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use lector_buffer::{Buffer, BufferOptions, Motion, Selection};
use lector_text::Rope;

fn big_text(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 40);
    for i in 0..lines {
        s.push_str(&format!(
            "    let value_{i} = compute({i}, \"text\"); // line {i}\n"
        ));
    }
    s
}

fn bench_insert(c: &mut Criterion) {
    let text = big_text(100_000);
    let rope = Rope::from_str(&text);
    let mut g = c.benchmark_group("insert");

    let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
    buf.set_cursor(text.len() / 2);
    g.bench_function("one_char_100k_lines_coalesced", |b| {
        b.iter(|| buf.insert("a"));
    });

    let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
    buf.set_coalesce_window(Duration::ZERO);
    buf.set_cursor(text.len() / 2);
    g.bench_function("one_char_100k_lines_own_undo_step", |b| {
        b.iter(|| buf.insert("a"));
    });

    let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
    buf.set_cursor(text.len() / 2);
    g.bench_function("backspace_100k_lines", |b| {
        b.iter(|| {
            buf.insert("a");
            buf.backspace()
        });
    });

    let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
    buf.set_cursor(text.len() / 2);
    g.bench_function("newline_100k_lines", |b| {
        b.iter(|| buf.newline());
    });
    g.finish();
}

fn bench_multi_caret(c: &mut Criterion) {
    let text = big_text(2_000);
    let rope = Rope::from_str(&text);
    let mut g = c.benchmark_group("multi_caret");
    let sels: Vec<Selection> = (0..1000)
        .map(|i| Selection::caret(0, rope.point_to_offset(lector_text::Point::new(i * 2, 4))))
        .collect();

    // Fresh buffer per iteration so the text does not grow between samples.
    let mut base = Buffer::new(rope.clone(), BufferOptions::default());
    base.set_selections(sels.clone());
    g.bench_function("type_char_1000_carets", |b| {
        b.iter_batched(
            || base.clone(),
            |mut buf| buf.insert("x"),
            BatchSize::SmallInput,
        );
    });

    let mut typed = base.clone();
    typed.insert("x");
    g.bench_function("backspace_1000_carets", |b| {
        b.iter_batched(
            || typed.clone(),
            |mut buf| buf.backspace(),
            BatchSize::SmallInput,
        );
    });

    // Carets far apart in a big file: gaps are shared with the old rope.
    let big = Rope::from_str(&big_text(100_000));
    let spread: Vec<Selection> = (0..1000)
        .map(|i| Selection::caret(0, big.point_to_offset(lector_text::Point::new(i * 100, 4))))
        .collect();
    let mut spread_buf = Buffer::new(big, BufferOptions::default());
    spread_buf.set_selections(spread);
    g.bench_function("type_char_1000_carets_spread_100k_lines", |b| {
        b.iter_batched(
            || spread_buf.clone(),
            |mut buf| buf.insert("x"),
            BatchSize::SmallInput,
        );
    });

    let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
    buf.set_selections(sels.clone());
    g.bench_function("move_right_1000_carets", |b| {
        b.iter(|| {
            buf.move_carets(Motion::Right, false);
            buf.move_carets(Motion::Left, false);
        });
    });

    g.bench_function("undo_redo_1000_carets", |b| {
        b.iter_batched(
            || {
                let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
                buf.set_selections(sels.clone());
                buf.insert("x");
                buf
            },
            |mut buf| {
                buf.undo();
                buf.redo();
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

fn bench_occurrences(c: &mut Criterion) {
    let mut s = String::new();
    for i in 0..10_000 {
        s.push_str(&format!("let needle_{} = needle + {i}; // needle\n", i % 7));
    }
    let rope = Rope::from_str(&s);
    let mut g = c.benchmark_group("occurrences");
    g.bench_function("select_all_10k_matches", |b| {
        b.iter_batched(
            || {
                let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
                buf.set_cursor(7);
                buf
            },
            |mut buf| buf.select_all_occurrences(),
            BatchSize::LargeInput,
        );
    });
    g.bench_function("select_next_occurrence", |b| {
        let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
        buf.set_cursor(7);
        buf.select_next_occurrence();
        b.iter_batched(
            || buf.clone(),
            |mut buf| buf.select_next_occurrence(),
            BatchSize::LargeInput,
        );
    });
    g.bench_function("type_with_10k_carets", |b| {
        b.iter_batched(
            || {
                let mut buf = Buffer::new(rope.clone(), BufferOptions::default());
                buf.set_cursor(7);
                buf.select_all_occurrences();
                buf
            },
            |mut buf| buf.insert("z"),
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

fn config() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(2))
        .sample_size(30)
}

criterion_group! {
    name = benches;
    config = config();
    targets = bench_insert, bench_multi_caret, bench_occurrences
}
criterion_main!(benches);
