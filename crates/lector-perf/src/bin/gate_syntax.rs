//! Instruction-count gates for `lector-syntax`: tree-sitter parse and highlight of a ~2k-line
//! Rust file, and the incremental reparse after a one-line edit.

use std::hint::black_box;
use std::process::ExitCode;

use lector_perf::harness::{gate_main, work, Bench, Work};
use lector_perf::inputs::rust_module;
use lector_syntax::{LanguageRegistry, Point, Syntax, SyntaxEdit};

/// Compiles the language's queries (lazily, on first use) so that only steady state is counted.
fn warm_up(lang: &Option<std::sync::Arc<lector_syntax::Language>>) {
    let warm = "fn main() { let x = 1; }\n";
    if let Some(lang) = lang {
        if let Ok(syntax) = Syntax::parse(lang, warm) {
            black_box(syntax.highlights(warm, 0..warm.len()));
        }
    }
}

/// Parses a ~2k-line Rust file and highlights all of it.
fn parse_and_highlight_2k() -> Work {
    let lang = LanguageRegistry::new().by_name("rust");
    warm_up(&lang);
    let text = rust_module(2_000, 51);
    work(move || {
        let Some(lang) = lang else { return };
        if let Ok(syntax) = Syntax::parse(&lang, text.as_str()) {
            let spans = syntax.highlights(text.as_str(), 0..text.len());
            black_box((spans.len(), syntax.has_errors()));
        }
    })
}

/// Incremental reparse of that file after inserting one line in the middle, then highlighting
/// the 60 lines around the edit (what a repaint after typing costs).
fn reparse_after_edit() -> Work {
    let lang = LanguageRegistry::new().by_name("rust");
    warm_up(&lang);
    let text = rust_module(2_000, 52);
    let syntax = lang
        .as_ref()
        .and_then(|l| Syntax::parse(l, text.as_str()).ok());
    // Insert a line at the start of the line that begins at or after the middle of the text.
    let mid = text.len() / 2;
    let at = text[mid..].find('\n').map_or(text.len(), |i| mid + i + 1);
    let inserted = "    let extra = 1 + 2;\n";
    let mut edited = String::with_capacity(text.len() + inserted.len());
    edited.push_str(&text[..at]);
    edited.push_str(inserted);
    edited.push_str(&text[at..]);
    let row = text[..at].matches('\n').count() as u32;
    let edit = SyntaxEdit {
        start_byte: at,
        old_end_byte: at,
        new_end_byte: at + inserted.len(),
        start: Point { row, column: 0 },
        old_end: Point { row, column: 0 },
        new_end: Point {
            row: row + 1,
            column: 0,
        },
    };
    work(move || {
        let Some(syntax) = syntax else { return };
        if let Ok(next) = syntax.edit_and_reparse(edited.as_str(), &[edit]) {
            let from = at.saturating_sub(1500);
            let to = (at + 1500).min(edited.len());
            let from = (0..=from)
                .rev()
                .find(|&i| edited.is_char_boundary(i))
                .unwrap_or(0);
            let to = (to..=edited.len())
                .find(|&i| edited.is_char_boundary(i))
                .unwrap_or(edited.len());
            black_box(next.highlights(edited.as_str(), from..to).len());
        }
    })
}

fn main() -> ExitCode {
    gate_main(
        "syntax",
        &[
            Bench::new("syntax/parse_highlight_2k", parse_and_highlight_2k),
            Bench::new("syntax/reparse_after_edit", reparse_after_edit),
        ],
    )
}
