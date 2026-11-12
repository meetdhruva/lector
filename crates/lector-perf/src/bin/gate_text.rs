//! Instruction-count gates for `lector-text`: rope construction, edits and line lookups.

use std::hint::black_box;
use std::process::ExitCode;

use lector_perf::harness::{gate_main, work, Bench, Work};
use lector_perf::inputs::{rust_like_source, Rng};
use lector_text::{Point, Rope};

/// Building a rope from 1 MB of text (about 22k lines) and reading its summary.
fn rope_build() -> Work {
    let text = rust_like_source(22_000, 11);
    work(move || {
        let rope = Rope::from_str(&text);
        black_box((rope.len(), rope.line_count(), rope.max_point()));
    })
}

/// 2000 edits (inserts, deletes, replaces) scattered through a 1 MB rope, each followed by the
/// line lookups an editor does after a change: offset to point, line range, point to offset.
fn edit_and_line_lookup() -> Work {
    let text = rust_like_source(22_000, 12);
    let rope = Rope::from_str(&text);
    work(move || {
        let mut rope = rope;
        let mut rng = Rng::new(99);
        for i in 0..2000usize {
            let at = rope.clip_offset(rng.below(rope.len()), lector_text::Bias::Left);
            match i % 3 {
                0 => rope.replace(at..at, "let inserted = 42;\n"),
                1 => {
                    let end = rope.clip_offset((at + 24).min(rope.len()), lector_text::Bias::Left);
                    rope.replace(at..end, "");
                }
                _ => {
                    let end = rope.clip_offset((at + 8).min(rope.len()), lector_text::Bias::Left);
                    rope.replace(at..end, "renamed");
                }
            }
            let probe = rng.below(rope.len().max(1));
            let point = rope.offset_to_point(probe);
            let range = rope.line_range(point.row);
            let back = rope.point_to_offset(Point::new(point.row, 0));
            black_box((range, back, rope.line_len(point.row)));
        }
        black_box(rope.line_count());
    })
}

/// Rendering a screenful at 500 scroll positions: `lines(rows)` for 60 rows each time.
fn scroll_lines() -> Work {
    let text = rust_like_source(22_000, 13);
    let rope = Rope::from_str(&text);
    work(move || {
        let mut rng = Rng::new(5);
        let last = rope.line_count().saturating_sub(61) as usize;
        let mut total = 0usize;
        for _ in 0..500 {
            let top = rng.below(last.max(1)) as u32;
            total += rope.lines(top..top + 60).map(|l| l.len()).sum::<usize>();
        }
        black_box(total);
    })
}

fn main() -> ExitCode {
    gate_main(
        "text",
        &[
            Bench::new("text/rope_build_1mb", rope_build),
            Bench::new("text/edit_and_line_lookup", edit_and_line_lookup),
            Bench::new("text/scroll_lines", scroll_lines),
        ],
    )
}
