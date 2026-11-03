//! `lector_text::Text` integration and performance budgets.

use std::time::Instant;

use lector_syntax::{LanguageRegistry, Syntax};
use lector_text::Text;

fn rust() -> std::sync::Arc<lector_syntax::Language> {
    LanguageRegistry::new().by_name("rust").expect("rust")
}

#[test]
fn text_and_str_agree() {
    let src = include_str!("samples/sample.rs");
    let text = Text::from_str(src);
    let a = Syntax::parse(&rust(), &text).expect("text");
    let b = Syntax::parse(&rust(), src).expect("str");
    assert_eq!(a.debug_sexp(), b.debug_sexp());
    assert_eq!(
        a.highlights(&text, 0..src.len()),
        b.highlights(src, 0..src.len())
    );
    assert_eq!(a.outline(&text), b.outline(src));
    assert_eq!(a.fold_ranges(&text), b.fold_ranges(src));
    assert_eq!(a.tags(&text), b.tags(src));
}

#[test]
fn markdown_injection_works_over_text() {
    let src = "# T\n\n```rust\nfn main() {}\n```\n\nSome *emphasis*.\n";
    let text = Text::from_str(src);
    let md = LanguageRegistry::new()
        .by_name("markdown")
        .expect("markdown");
    let syn = Syntax::parse(&md, &text).expect("parse");
    let names: Vec<_> = syn
        .highlights(&text, 0..src.len())
        .iter()
        .map(|s| s.id.name())
        .collect();
    assert!(
        names.contains(&"keyword") && names.contains(&"markup.italic"),
        "{names:?}"
    );
}

#[test]
fn mapped_text_parses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a.rs");
    std::fs::write(&path, include_str!("samples/sample.rs")).expect("write");
    let text = Text::open_mapped(&path).expect("map");
    assert!(text.is_mapped());
    let syn = Syntax::parse(&rust(), &text).expect("parse mapped");
    assert!(!syn.outline(&text).is_empty());
    assert!(!syn.highlights(&text, 0..text.len()).is_empty());
}

fn big_rust(target_bytes: usize) -> String {
    let unit = include_str!("samples/sample.rs");
    let mut out = String::with_capacity(target_bytes + unit.len());
    let mut i = 0;
    while out.len() < target_bytes {
        // Rename items so the file stays realistic (distinct identifiers).
        out.push_str(
            &unit
                .replace("fn main", &format!("fn main_{i}"))
                .replace("//!", "//"),
        );
        i += 1;
    }
    out
}

/// Performance budgets from the architecture: parse 1 MB < 150 ms, 100-line
/// window highlight < 2 ms. Numbers are printed (run with `--nocapture`);
/// assertions use 4x slack because the test machine is shared and the dev
/// profile is not fully optimised.
#[test]
fn performance_budgets() {
    let src = big_rust(1_000_000);
    let text = Text::from_str(&src);
    let lang = rust();
    // Warm query compilation (one-off, cached for the process).
    let warm = Syntax::parse(&lang, "fn a() {}").expect("warm");
    let _ = warm.highlights("fn a() {}", 0..9);

    let mut parse_ms = f64::MAX;
    let mut syn = None;
    for _ in 0..3 {
        let t = Instant::now();
        let parsed = Syntax::parse(&lang, &text).expect("parse");
        parse_ms = parse_ms.min(t.elapsed().as_secs_f64() * 1000.0);
        syn = Some(parsed);
    }
    let syn = syn.expect("parsed");
    let t = Instant::now();
    let _ = Syntax::parse(&lang, src.as_str()).expect("parse str");
    let str_ms = t.elapsed().as_secs_f64() * 1000.0;

    // A 100-line window in the middle of the file.
    let mid_row = text.line_count() / 2;
    let start = text.line_range(mid_row).start;
    let end = text.line_range(mid_row + 100).end;
    let mut best = f64::MAX;
    let mut total = 0.0;
    let runs = 20;
    let mut spans = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let hl = syn.highlights(&text, start..end);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        spans = hl.len();
        best = best.min(ms);
        total += ms;
    }
    let t = Instant::now();
    let outline = syn.outline(&text);
    let outline_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let folds = syn.fold_ranges(&text);
    let folds_ms = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "perf: {} bytes | parse (Text, best of 3) {parse_ms:.1} ms (plain &str {str_ms:.1} ms) | highlights(100 lines, {spans} spans) best {best:.2} ms avg {:.2} ms | outline {} items {outline_ms:.1} ms | folds {} {folds_ms:.1} ms",
        src.len(),
        total / runs as f64,
        outline.len(),
        folds.len()
    );
    assert!(parse_ms < 600.0, "parse took {parse_ms} ms");
    assert!(best < 8.0, "highlight window took {best} ms");
}

/// Markdown windows parse their inline/fenced injections on demand; scrolling
/// (repeated windows) is served from the layer cache.
#[test]
fn markdown_window_performance() {
    let unit = "## Section\n\nSome *emphasis*, **strong** text with `code` and a [link](http://x.y).\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\n- item one\n- item two\n\n";
    let src = unit.repeat(3000);
    let text = Text::from_str(&src);
    let md = LanguageRegistry::new()
        .by_name("markdown")
        .expect("markdown");
    let t = Instant::now();
    let syn = Syntax::parse(&md, &text).expect("parse");
    let parse_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mid = text.line_count() / 2;
    let (start, end) = (text.line_range(mid).start, text.line_range(mid + 100).end);
    let t = Instant::now();
    let first = syn.highlights(&text, start..end);
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let again = syn.highlights(&text, start..end);
    let warm_ms = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!("perf-md: {} bytes | parse {parse_ms:.1} ms | window cold {cold_ms:.2} ms warm {warm_ms:.2} ms ({} spans)", src.len(), first.len());
    assert_eq!(first, again);
    assert!(cold_ms < 60.0, "cold markdown window {cold_ms} ms");
}
