//! Dev tool: `cargo run -p lector-syntax --example dump -- <file> [highlights|outline|folds|tags|nodes]`
use std::path::Path;

use lector_syntax::{LanguageRegistry, Syntax};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str).unwrap_or("");
    let what = args.get(2).map(String::as_str).unwrap_or("outline");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let reg = LanguageRegistry::new();
    let first = text.lines().next();
    let Some(lang) = reg.language_for_path(Path::new(path), first) else {
        eprintln!("no language for {path}");
        return;
    };
    let syntax = match Syntax::parse(&lang, text.as_str()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    println!("language: {} (errors: {})", lang.name, syntax.has_errors());
    match what {
        "highlights" => {
            for s in syntax.highlights(text.as_str(), 0..text.len()) {
                let pt = lector_syntax::TextInput::offset_to_point(text.as_str(), s.range.start);
                println!(
                    "{}:{} {} {:?}",
                    pt.row,
                    pt.column,
                    s.id.name(),
                    &text[s.range.clone()]
                );
            }
        }
        "folds" => {
            for r in syntax.fold_ranges(text.as_str()) {
                println!("{}..{}", r.start, r.end);
            }
        }
        "tags" => {
            for t in syntax.tags(text.as_str()) {
                println!(
                    "{} {:?} {:?} {}",
                    if t.is_definition { "def" } else { "ref" },
                    t.kind,
                    t.name,
                    t.range.start
                );
            }
        }
        _ => {
            for i in syntax.outline(text.as_str()) {
                println!(
                    "{}{:?} {:?} {}..{}",
                    "  ".repeat(i.depth as usize),
                    i.kind,
                    i.name,
                    i.range.start,
                    i.range.end
                );
            }
        }
    }
}
