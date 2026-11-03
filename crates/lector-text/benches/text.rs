//! Benchmarks for rope construction, conversions and mmap open.
//!
//! Run quickly with: `cargo bench -p lector-text -- --quick`
//! (the mmap benchmark generates a ~200 MB file in the system temp dir once).

use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use lector_text::{MappedText, Point, Rope};

fn sample_text(target_len: usize) -> String {
    let lines = [
        "fn main() {\n",
        "    let value = compute(42, \"héllo wörld\");\n",
        "    println!(\"{value:?}\");\n",
        "}\n",
        "\n",
        "// A longer comment line that pads the file a bit so lines vary in length 😀\n",
    ];
    let mut text = String::with_capacity(target_len + 128);
    let mut i = 0;
    while text.len() < target_len {
        text.push_str(lines[i % lines.len()]);
        i += 1;
    }
    text
}

/// Deterministic pseudo-random offsets.
fn offsets(len: usize, count: usize) -> Vec<usize> {
    let mut state = 0x9E3779B97F4A7C15u64;
    (0..count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % len as u64) as usize
        })
        .collect()
}

fn rope_benches(c: &mut Criterion) {
    let text = sample_text(10 << 20);
    let mut group = c.benchmark_group("rope");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    group.bench_function("from_str_10MB", |b| {
        b.iter(|| Rope::from(black_box(text.as_str())))
    });

    let rope = Rope::from(text.as_str());
    let offsets = offsets(text.len(), 1024);
    group.bench_function("offset_to_point_x1024", |b| {
        b.iter(|| {
            for &offset in &offsets {
                black_box(rope.offset_to_point(offset));
            }
        })
    });
    let points: Vec<Point> = offsets.iter().map(|&o| rope.offset_to_point(o)).collect();
    group.bench_function("point_to_offset_x1024", |b| {
        b.iter(|| {
            for &point in &points {
                black_box(rope.point_to_offset(point));
            }
        })
    });
    group.bench_function("line_range_x1024", |b| {
        b.iter(|| {
            for point in &points {
                black_box(rope.line_range(point.row));
            }
        })
    });
    group.finish();
}

fn mmap_benches(c: &mut Criterion) {
    let path = std::env::temp_dir().join("lector-text-bench-200mb.txt");
    let target = 200usize << 20;
    if std::fs::metadata(&path).map(|m| m.len() as usize).ok() != Some(target) {
        let chunk = sample_text(1 << 20);
        let Ok(file) = std::fs::File::create(&path) else {
            return;
        };
        let mut writer = BufWriter::new(file);
        let mut written = 0;
        while written < target {
            let take = chunk.len().min(target - written);
            if writer.write_all(&chunk.as_bytes()[..take]).is_err() {
                return;
            }
            written += take;
        }
        if writer.flush().is_err() {
            return;
        }
    }
    let mut group = c.benchmark_group("mapped");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));
    group.bench_function("open_200MB", |b| {
        b.iter(|| black_box(MappedText::open(&path).map(|m| m.line_count())))
    });
    let Ok(mapped) = MappedText::open(&path) else {
        return;
    };
    let offsets = offsets(mapped.len(), 1024);
    group.bench_function("offset_to_point_x1024", |b| {
        b.iter(|| {
            for &offset in &offsets {
                black_box(mapped.offset_to_point(offset));
            }
        })
    });
    let rows: Vec<u32> = offsets
        .iter()
        .map(|&o| mapped.offset_to_point(o).row)
        .collect();
    group.bench_function("line_x1024", |b| {
        b.iter(|| {
            for &row in &rows {
                black_box(mapped.line(row));
            }
        })
    });
    group.finish();
}

criterion_group!(benches, rope_benches, mmap_benches);
criterion_main!(benches);
