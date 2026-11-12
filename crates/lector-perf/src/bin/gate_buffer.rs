//! Instruction-count gates for `lector-buffer`: editing with many carets, typing in a big
//! buffer, select-all-occurrences.

use std::hint::black_box;
use std::process::ExitCode;

use lector_buffer::{Buffer, BufferOptions, Motion, Selection};
use lector_perf::harness::{gate_main, work, Bench, Work};
use lector_perf::inputs::rust_like_source;
use lector_text::{Point, Rope};

fn carets(rope: &Rope, count: u32, step: u32) -> Vec<Selection> {
    (0..count)
        .map(|i| Selection::caret(0, rope.point_to_offset(Point::new(i * step, 4))))
        .collect()
}

/// 500 carets on a 2k-line buffer: type a character, move right and left, backspace, undo, redo.
fn multi_caret_edit() -> Work {
    let rope = Rope::from_str(&rust_like_source(2_000, 21));
    let sels = carets(&rope, 500, 4);
    let mut buf = Buffer::new(rope, BufferOptions::default());
    buf.set_selections(sels);
    work(move || {
        buf.insert("x");
        buf.move_carets(Motion::Right, false);
        buf.move_carets(Motion::Left, false);
        buf.backspace();
        buf.undo();
        buf.redo();
        black_box((buf.text().len(), buf.version()));
    })
}

/// 300 single-caret keystrokes (insert, newline, backspace) in the middle of a 20k-line buffer.
fn typing_in_big_buffer() -> Work {
    let rope = Rope::from_str(&rust_like_source(20_000, 22));
    let middle = rope.len() / 2;
    let mut buf = Buffer::new(rope, BufferOptions::default());
    buf.set_cursor(middle);
    work(move || {
        for i in 0..300 {
            match i % 3 {
                0 => {
                    buf.insert("a");
                }
                1 => {
                    buf.newline();
                }
                _ => {
                    buf.backspace();
                }
            }
        }
        black_box(buf.text().len());
    })
}

/// Selecting every occurrence of a word in a 5k-line buffer, then typing over all of them.
fn select_all_occurrences() -> Work {
    let mut text = String::new();
    for i in 0..5_000 {
        text.push_str(&format!("let needle_{} = needle + {i}; // needle\n", i % 7));
    }
    let rope = Rope::from_str(&text);
    let mut buf = Buffer::new(rope, BufferOptions::default());
    buf.set_cursor(7);
    work(move || {
        buf.select_all_occurrences();
        buf.insert("pin");
        black_box((buf.selections().len(), buf.text().len()));
    })
}

fn main() -> ExitCode {
    gate_main(
        "buffer",
        &[
            Bench::new("buffer/multi_caret_edit", multi_caret_edit),
            Bench::new("buffer/typing_in_big_buffer", typing_in_big_buffer),
            Bench::new("buffer/select_all_occurrences", select_all_occurrences),
        ],
    )
}
