//! Incremental re-parse must equal a fresh parse; timing for a 10k-line file.

use std::time::Instant;

use lector_syntax::{LanguageRegistry, Point, Syntax, SyntaxEdit};
use lector_text::Rope;
use proptest::prelude::*;

fn rust() -> std::sync::Arc<lector_syntax::Language> {
    LanguageRegistry::new()
        .by_name("rust")
        .expect("rust grammar")
}

fn extent(text: &str) -> Point {
    let rows = text.matches('\n').count() as u32;
    if rows == 0 {
        Point::new(0, text.len() as u32)
    } else {
        Point::new(
            rows,
            (text.len() - text.rfind('\n').map_or(0, |i| i + 1)) as u32,
        )
    }
}

fn point_of(text: &str, offset: usize) -> Point {
    extent(&text[..offset])
}

/// Replaces `start..end` of `text` by `new`, returning the delta.
fn apply(text: &mut String, start: usize, end: usize, new: &str) -> SyntaxEdit {
    let sp = point_of(text, start);
    let ep = point_of(text, end);
    let edit = SyntaxEdit {
        start_byte: start,
        old_end_byte: end,
        new_end_byte: start + new.len(),
        start: sp,
        old_end: ep,
        new_end: sp + extent(new),
    };
    text.replace_range(start..end, new);
    edit
}

fn program(n: usize) -> String {
    let mut s = String::new();
    for i in 0..n {
        s.push_str(&format!(
            "/// doc {i}\npub fn func_{i}(a: u32, b: &str) -> u32 {{\n    let x_{i} = a + {i};\n    let y = match x_{i} {{\n        0 => 1,\n        _ => b.len() as u32,\n    }};\n    // trailing comment\n    x_{i} * y\n}}\n\n"
        ));
    }
    s
}

#[test]
fn empty_edit_list_reparses_same_tree() {
    let lang = rust();
    let text = program(3);
    let syn = Syntax::parse(&lang, text.as_str()).unwrap();
    let again = syn.edit_and_reparse(text.as_str(), &[]).unwrap();
    assert_eq!(syn.debug_sexp(), again.debug_sexp());
}

#[test]
fn invalid_edits_fall_back_to_full_parse() {
    let lang = rust();
    let mut text = program(2);
    let syn = Syntax::parse(&lang, text.as_str()).unwrap();
    text.push_str("fn z() {}\n");
    let bogus = SyntaxEdit {
        start_byte: 10,
        old_end_byte: 5,
        new_end_byte: 3,
        start: Point::new(0, 10),
        old_end: Point::new(0, 5),
        new_end: Point::new(0, 3),
    };
    let got = syn.edit_and_reparse(text.as_str(), &[bogus]).unwrap();
    let fresh = Syntax::parse(&lang, text.as_str()).unwrap();
    assert_eq!(got.debug_sexp(), fresh.debug_sexp());
}

#[test]
fn works_on_ropes_and_keeps_old_tree_untouched() {
    let lang = rust();
    let mut text = program(4);
    let old = Syntax::parse(&lang, text.as_str()).unwrap();
    let before = old.debug_sexp();
    let at = text.find("func_2").unwrap() + 2;
    let edit = apply(&mut text, at, at, "X");
    let rope = Rope::from_str(&text);
    let new = old.edit_and_reparse(&rope, &[edit]).unwrap();
    assert_eq!(old.debug_sexp(), before);
    assert_eq!(
        new.debug_sexp(),
        Syntax::parse(&lang, text.as_str()).unwrap().debug_sexp()
    );
}

fn op_strategy() -> impl Strategy<Value = (usize, usize, usize)> {
    // (kind, position fraction in 0..1000, payload selector)
    (0usize..6, 0usize..1000, 0usize..16)
}

const SNIPPETS: &[&str] = &[
    "\n",
    " ",
    "// c\n",
    "let q = 1;\n",
    "    z += 1;\n",
    "x",
    "9",
    "/* b */",
    "fn k() {}\n",
    "{}",
    ";",
    "(",
    ")",
    "}",
    "\"",
    "\n\n",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Edits that keep the program valid: incremental equals fresh.
    #[test]
    fn incremental_equals_fresh_for_valid_edits(ops in prop::collection::vec(op_strategy(), 1..12)) {
        let lang = rust();
        let mut text = program(6);
        let mut syn = Syntax::parse(&lang, text.as_str()).unwrap();
        for (kind, frac, payload) in ops {
            let pos_of = |text: &str, needle: &str| -> Option<usize> {
                let idxs: Vec<usize> = text.match_indices(needle).map(|(i, _)| i).collect();
                if idxs.is_empty() { None } else { Some(idxs[frac % idxs.len()]) }
            };
            let edit = match kind {
                // rename a letter of an identifier
                0 => pos_of(&text, "func_").map(|i| {
                    let c = (b'a' + (payload as u8 % 26)) as char;
                    apply(&mut text, i + 5, i + 6, &c.to_string())
                }),
                // add a statement line
                1 => pos_of(&text, "    // trailing comment\n").map(|i| {
                    apply(&mut text, i, i, &format!("    let w{payload} = {payload};\n"))
                }),
                // delete a comment line
                2 => pos_of(&text, "    // trailing comment\n").map(|i| {
                    apply(&mut text, i, i + "    // trailing comment\n".len(), "")
                }),
                // insert whitespace / comments between items
                3 => pos_of(&text, "\n\n").map(|i| {
                    apply(&mut text, i + 1, i + 1, SNIPPETS[payload % 3 + 1 - usize::from(payload % 3 == 2)])
                }),
                // change a number literal
                4 => pos_of(&text, " + ").map(|i| {
                    apply(&mut text, i + 3, i + 4, &(payload % 10).to_string())
                }),
                // add a whole new function
                _ => Some({
                    let len = text.len();
                    apply(&mut text, len, len, &format!("fn added_{payload}() -> u8 {{ {payload} }}\n"))
                }),
            };
            let Some(edit) = edit else { continue };
            syn = syn.edit_and_reparse(text.as_str(), &[edit]).unwrap();
            let fresh = Syntax::parse(&lang, text.as_str()).unwrap();
            prop_assert!(!fresh.has_errors(), "generator produced invalid Rust:\n{text}");
            prop_assert_eq!(syn.debug_sexp(), fresh.debug_sexp());
        }
    }

    /// Arbitrary (possibly broken) edits: never panics, and whenever the result is
    /// error free the incremental tree equals a fresh parse.
    #[test]
    fn arbitrary_edits_never_panic(ops in prop::collection::vec((0usize..2000, 0usize..8, 0usize..SNIPPETS.len()), 1..10)) {
        let lang = rust();
        let mut text = program(3);
        let mut syn = Syntax::parse(&lang, text.as_str()).unwrap();
        for (pos, del, snip) in ops {
            let mut start = pos % (text.len() + 1);
            while !text.is_char_boundary(start) { start -= 1; }
            let mut end = (start + del).min(text.len());
            while !text.is_char_boundary(end) { end -= 1; }
            let edit = apply(&mut text, start, end, SNIPPETS[snip]);
            syn = syn.edit_and_reparse(text.as_str(), &[edit]).unwrap();
            let fresh = Syntax::parse(&lang, text.as_str()).unwrap();
            if !fresh.has_errors() && !syn.has_errors() {
                prop_assert_eq!(syn.debug_sexp(), fresh.debug_sexp());
            }
        }
    }

    /// A batch of several edits (back to front, as lector-buffer emits them).
    #[test]
    fn batched_edits_back_to_front(picks in prop::collection::vec(0usize..6, 1..6)) {
        let lang = rust();
        let mut text = program(6);
        let syn = Syntax::parse(&lang, text.as_str()).unwrap();
        let mut sites: Vec<usize> = text.match_indices("func_").map(|(i, _)| i + 5).collect();
        sites.sort_unstable();
        sites.dedup();
        let mut chosen: Vec<usize> = picks.iter().map(|p| sites[p % sites.len()]).collect();
        chosen.sort_unstable();
        chosen.dedup();
        let mut edits = Vec::new();
        for &at in chosen.iter().rev() {
            edits.push(apply(&mut text, at, at + 1, "Q"));
        }
        let got = syn.edit_and_reparse(text.as_str(), &edits).unwrap();
        let fresh = Syntax::parse(&lang, text.as_str()).unwrap();
        prop_assert_eq!(got.debug_sexp(), fresh.debug_sexp());
    }
}

/// `cargo test -p lector-syntax --test incremental -- --ignored --nocapture`
#[test]
#[ignore = "timing measurement"]
fn single_char_edit_in_10k_line_file() {
    let lang = rust();
    let mut text = program(1000); // 11 lines per function
    let lines = text.matches('\n').count();
    let mut syn = Syntax::parse(&lang, text.as_str()).unwrap();
    let mut rope = Rope::from_str(&text);
    let mut times = Vec::new();
    let base = text.find("let x_500 ").unwrap() + 4;
    for i in 0..60 {
        let at = base + i;
        let edit = apply(&mut text, at, at, "z");
        rope.replace(at..at, "z");
        let t = Instant::now();
        syn = syn.edit_and_reparse(&rope, &[edit]).unwrap();
        times.push(t.elapsed());
    }
    times.sort();
    let full = Instant::now();
    let fresh = Syntax::parse(&lang, &rope).unwrap();
    let full = full.elapsed();
    println!(
        "{lines} lines: single-char edit_and_reparse median {:?}, p90 {:?}, max {:?}; fresh parse {:?}",
        times[times.len() / 2],
        times[times.len() * 9 / 10],
        times[times.len() - 1],
        full
    );
    assert_eq!(syn.debug_sexp().len(), fresh.debug_sexp().len());
}
