//! Dev tool: `cargo run --release -p lector-syntax --example bench -- <file>` (parse + window highlight timings).
use std::time::Instant;

use lector_syntax::{LanguageRegistry, Syntax};
use lector_text::Text;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_default();
    let src = std::fs::read_to_string(&path).unwrap_or_default();
    let text = Text::from_str(&src);
    let reg = LanguageRegistry::new();
    let Some(lang) = reg.language_for_path(std::path::Path::new(&path), src.lines().next()) else {
        eprintln!("unknown language");
        return;
    };
    let mut best = f64::MAX;
    let mut syn = None;
    for _ in 0..5 {
        let t = Instant::now();
        let s = Syntax::parse(&lang, &text);
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        syn = s.ok();
    }
    let Some(syn) = syn else { return };
    let mid = text.line_count() / 2;
    let (start, end) = (text.line_range(mid).start, text.line_range(mid + 100).end);
    let _ = syn.highlights(&text, start..end);
    let mut hbest = f64::MAX;
    for _ in 0..20 {
        let t = Instant::now();
        let _ = syn.highlights(&text, start..end);
        hbest = hbest.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "{} bytes, {} lines: parse best {best:.1} ms, 100-line highlights best {hbest:.2} ms",
        src.len(),
        text.line_count()
    );
}
