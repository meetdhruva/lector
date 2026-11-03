//! Snapshot tests: highlights, outline, folds and definitions for one sample
//! file per language. Accept new snapshots with `INSTA_UPDATE=always`.

use std::path::Path;

use lector_syntax::{LanguageRegistry, Syntax, TextInput, HIGHLIGHT_NAMES};

fn load(name: &str) -> (String, Syntax) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/samples")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
    let lang = LanguageRegistry::new()
        .language_for_path(&path, text.lines().next())
        .unwrap_or_else(|| panic!("no language for {name}"));
    let syntax = Syntax::parse(&lang, text.as_str()).unwrap_or_else(|e| panic!("{name}: {e}"));
    (text, syntax)
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('"', "\\\"")
}

fn highlights_snapshot(text: &str, syntax: &Syntax) -> String {
    let mut out = String::new();
    for span in syntax.highlights(text, 0..text.len()) {
        let p = text.offset_to_point(span.range.start);
        out.push_str(&format!(
            "{}:{} {} \"{}\"\n",
            p.row,
            p.column,
            span.id.name(),
            escape(&text[span.range.clone()])
        ));
    }
    out
}

fn outline_snapshot(text: &str, syntax: &Syntax) -> String {
    let mut out = String::new();
    for item in syntax.outline(text) {
        let p = text.offset_to_point(item.range.start);
        out.push_str(&format!(
            "{}{} {} (line {})\n",
            "  ".repeat(item.depth as usize),
            item.kind.label(),
            escape(&item.name),
            p.row
        ));
    }
    out
}

fn folds_snapshot(text: &str, syntax: &Syntax) -> String {
    syntax
        .fold_ranges(text)
        .iter()
        .map(|r| format!("{}..{}", r.start, r.end))
        .collect::<Vec<_>>()
        .join(" ")
}

fn tags_snapshot(text: &str, syntax: &Syntax) -> String {
    let tags = syntax.tags(text);
    let refs = tags.iter().filter(|t| !t.is_definition).count();
    let mut out = String::new();
    for t in tags.iter().filter(|t| t.is_definition) {
        let p = text.offset_to_point(t.range.start);
        out.push_str(&format!(
            "def {} {} (line {})\n",
            t.kind.label(),
            t.name,
            p.row
        ));
    }
    out.push_str(&format!(
        "references: {}\n",
        if refs > 0 { "present" } else { "none" }
    ));
    out
}

macro_rules! language_snapshots {
    ($($test:ident => $file:literal),* $(,)?) => {$(
        #[test]
        fn $test() {
            let (text, syntax) = load($file);
            assert!(!syntax.has_errors(), "{} should parse without errors", $file);
            let hl = highlights_snapshot(&text, &syntax);
            assert!(!hl.is_empty(), "no highlights for {}", $file);
            insta::assert_snapshot!(concat!(stringify!($test), "_highlights"), hl);
            insta::assert_snapshot!(concat!(stringify!($test), "_outline"), outline_snapshot(&text, &syntax));
            insta::assert_snapshot!(concat!(stringify!($test), "_folds"), folds_snapshot(&text, &syntax));
            insta::assert_snapshot!(concat!(stringify!($test), "_tags"), tags_snapshot(&text, &syntax));
        }
    )*};
}

language_snapshots! {
    rust => "sample.rs",
    typescript => "sample.ts",
    tsx => "sample.tsx",
    javascript => "sample.js",
    python => "sample.py",
    go => "sample.go",
    java => "Sample.java",
    kotlin => "sample.kt",
    swift => "sample.swift",
    c => "sample.c",
    cpp => "sample.cpp",
    csharp => "Sample.cs",
    ruby => "sample.rb",
    php => "sample.php",
    bash => "sample.sh",
    html => "sample.html",
    css => "sample.css",
    json => "sample.json",
    jsonc => "tsconfig.json",
    yaml => "sample.yaml",
    toml => "sample.toml",
    markdown => "sample.md",
    sql => "sample.sql",
}

#[test]
fn every_highlight_id_is_canonical() {
    for file in [
        "sample.rs",
        "sample.md",
        "sample.html",
        "sample.ts",
        "sample.kt",
    ] {
        let (text, syntax) = load(file);
        for span in syntax.highlights(text.as_str(), 0..text.len()) {
            assert!((span.id.0 as usize) < HIGHLIGHT_NAMES.len());
        }
    }
}
