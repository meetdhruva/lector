//! Unit tests of the structural (syntax-tree) diff.

use super::*;
use lector_syntax::LanguageRegistry;
use proptest::prelude::*;

fn run(lang: &str, old: &str, new: &str) -> Option<StructuralDiff> {
    let reg = LanguageRegistry::new();
    let l = reg.by_name(lang).unwrap();
    let (so, sn) = (
        Syntax::parse(&l, old).unwrap(),
        Syntax::parse(&l, new).unwrap(),
    );
    diff(&so, old, &sn, new, &Budget::default())
}

fn texts<'a>(src: &'a str, marks: &[Marked], mark: Mark) -> Vec<&'a str> {
    marks
        .iter()
        .filter(|m| m.mark == mark)
        .map(|m| &src[m.range.clone()])
        .collect()
}

#[test]
fn identical_and_reformatted_code_has_no_changes() {
    let a = "fn add(a: i32, b: i32) -> i32 { let c = a + b; c * 2 }\n";
    let b = "fn add(\n    a: i32,\n    b: i32,\n) -> i32 {\n    let c = a + b;\n    c * 2\n}\n";
    assert!(run("rust", a, a).unwrap().is_empty());
    let d = run("rust", a, b).unwrap();
    // Only the trailing comma is new.
    assert_eq!(texts(b, &d.new, Mark::Added), vec![","], "{d:?}");
    assert!(d.old.is_empty(), "{d:?}");
}

#[test]
fn rust_literal_change() {
    let (a, b) = ("fn f() { g(1, 2) }\n", "fn f() { g(1, 3) }\n");
    let d = run("rust", a, b).unwrap();
    assert_eq!(texts(a, &d.old, Mark::Changed), vec!["2"]);
    assert_eq!(texts(b, &d.new, Mark::Changed), vec!["3"]);
    assert_eq!(d.old.len(), 1);
}

#[test]
fn rust_added_and_removed_statements() {
    let a = "fn f() {\n    one();\n    two();\n}\n";
    let b = "fn f() {\n    one();\n    inserted();\n    two();\n}\n";
    let d = run("rust", a, b).unwrap();
    assert!(d.old.is_empty(), "{d:?}");
    let added = texts(b, &d.new, Mark::Added);
    assert_eq!(added.len(), 1, "{d:?}");
    assert!(added[0].contains("inserted"), "{added:?}");
    let r = run("rust", b, a).unwrap();
    assert!(r.new.is_empty());
    assert_eq!(texts(b, &r.old, Mark::Removed).len(), 1);
}

#[test]
fn moved_function() {
    let first = "fn first() { println!(\"first one\"); }\n";
    let second = "fn second(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    let diff = a - b;\n    if sum > diff { sum * 2 } else { diff * 3 }\n}\n";
    let third = "fn third(items: &[u8]) -> usize {\n    let mut n = 0;\n    for i in items {\n        if *i > 3 { n += 1; }\n    }\n    n\n}\n";
    let a = format!("{first}{second}{third}");
    let b = format!("{second}{third}{first}");
    let d = run("rust", &a, &b).unwrap();
    assert!(!d.is_empty());
    assert!(
        d.old.iter().chain(&d.new).all(|m| m.mark == Mark::Moved),
        "{d:?}"
    );
    assert_eq!(texts(&a, &d.old, Mark::Moved), vec![first.trim_end()]);
    assert_eq!(texts(&b, &d.new, Mark::Moved), vec![first.trim_end()]);
}

#[test]
fn renamed_identifier() {
    let a = "fn compute() { let total = 1; total }\n";
    let b = "fn compute() { let sum = 1; sum }\n";
    let d = run("rust", a, b).unwrap();
    assert_eq!(texts(a, &d.old, Mark::Changed), vec!["total", "total"]);
    assert_eq!(texts(b, &d.new, Mark::Changed), vec!["sum", "sum"]);
}

#[test]
fn string_and_comment_replacement() {
    let a = "fn f() {\n    // old comment text\n    log(\"hello world\");\n}\n";
    let b = "fn f() {\n    // new comment text\n    log(\"hello there\");\n}\n";
    let d = run("rust", a, b).unwrap();
    let changed = texts(a, &d.old, Mark::Changed);
    assert_eq!(changed.len(), 2, "{d:?}");
    assert!(changed[0].contains("old comment") && changed[1].contains("hello world"));
}

#[test]
fn wrapped_expression() {
    let a = "fn f() { compute(a, b) }\n";
    let b = "fn f() { Some(compute(a, b)) }\n";
    let d = run("rust", a, b).unwrap();
    assert!(d.old.is_empty(), "{d:?}");
    // `Some(` and `)` are new; `compute(a, b)` is untouched.
    let added = texts(b, &d.new, Mark::Added);
    assert!(added.iter().all(|t| !t.contains("compute")), "{added:?}");
    assert!(!added.is_empty());
}

#[test]
fn typescript() {
    let a = "export function area(w: number, h: number): number {\n  return w * h;\n}\n";
    let b = "export function area(w: number, h: number, scale: number): number {\n  return w * h * scale;\n}\n";
    let d = run("typescript", a, b).unwrap();
    assert!(d.old.is_empty(), "{d:?}");
    let added = texts(b, &d.new, Mark::Added);
    assert!(added.iter().any(|t| t.contains("scale")), "{added:?}");
    let same = "const x = {a: 1, b: [1,2,3]};\n";
    let spaced = "const x = {\n  a: 1,\n  b: [1, 2, 3],\n};\n";
    let d = run("typescript", same, spaced).unwrap();
    assert!(d.old.is_empty());
}

#[test]
fn python() {
    let a = "def f(x):\n    if x:\n        return 1\n    return 2\n";
    let b = "def f(x):\n    if x:\n        return 10\n    return 2\n";
    let d = run("python", a, b).unwrap();
    assert_eq!(texts(a, &d.old, Mark::Changed), vec!["1"]);
    assert_eq!(texts(b, &d.new, Mark::Changed), vec!["10"]);
    // Dedenting a statement out of the `if` is a structural change.
    let c = "def f(x):\n    if x:\n        return 1\n    return 2\n    return 3\n";
    let d = run("python", a, c).unwrap();
    assert!(!d.is_empty());
}

#[test]
fn go_and_json() {
    let a = "package main\n\nfunc main() {\n\tfmt.Println(\"a\", 1)\n}\n";
    let b = "package main\n\nfunc main() {\n\tfmt.Println(\"a\", 2)\n}\n";
    let d = run("go", a, b).unwrap();
    assert_eq!(texts(a, &d.old, Mark::Changed), vec!["1"]);
    let ja = "{\"a\": 1, \"b\": [1, 2]}";
    let jb = "{\n  \"a\": 1,\n  \"b\": [1, 2, 3]\n}";
    let d = run("json", ja, jb).unwrap();
    assert!(d.old.is_empty(), "{d:?}");
    assert!(
        texts(jb, &d.new, Mark::Added)
            .iter()
            .any(|t| t.contains('3')),
        "{d:?}"
    );
}

#[test]
fn empty_sides() {
    let d = run("rust", "", "fn a() {}\n").unwrap();
    assert!(d.old.is_empty());
    assert_eq!(texts("fn a() {}\n", &d.new, Mark::Added).len(), 1);
    let d = run("rust", "fn a() {}\n", "").unwrap();
    assert!(d.new.is_empty());
    assert!(!d.old.is_empty());
}

fn many_changes(n: usize) -> (String, String) {
    let old: String = (0..n)
        .map(|i| format!("fn f{i}() {{ a{i}(); b({i}); }}\n"))
        .collect();
    let new: String = (0..n)
        .map(|i| format!("fn g{i}() {{ c{i}(); d({}); }}\n", i + 1))
        .collect();
    (old, new)
}

fn parse_rust(old: &str, new: &str) -> (Syntax, Syntax) {
    let reg = LanguageRegistry::new();
    let l = reg.by_name("rust").unwrap();
    (
        Syntax::parse(&l, old).unwrap(),
        Syntax::parse(&l, new).unwrap(),
    )
}

#[test]
fn budget_exceeded_returns_none_or_reports_skipped_regions() {
    let (old, new) = many_changes(12);
    let (so, sn) = parse_rust(&old, &new);
    let tiny = Budget {
        max_states: 20,
        ..Budget::default()
    };
    match diff(&so, &old, &sn, &new, &tiny) {
        None => {}
        Some(d) => assert!(!d.skipped.is_empty(), "{d:?}"),
    }
    let nodes = Budget {
        max_nodes: 10,
        ..Budget::default()
    };
    assert!(diff(&so, &old, &sn, &new, &nodes).is_none());
    let generous = Budget {
        max_states: 50_000_000,
        max_time: Duration::from_secs(120),
        ..Budget::default()
    };
    let d = diff(&so, &old, &sn, &new, &generous).expect("fits a generous budget");
    assert!(d.skipped.is_empty());
}

#[test]
fn cancelled_token_aborts() {
    let (old, new) = many_changes(4);
    let (so, sn) = parse_rust(&old, &new);
    let token = CancelToken::new();
    token.cancel();
    assert!(diff_cancellable(&so, &old, &sn, &new, &Budget::default(), &token).is_none());
    let live = CancelToken::new();
    assert!(diff_cancellable(&so, &old, &sn, &new, &Budget::default(), &live).is_some());
}

#[test]
fn comment_only_change_marks_only_the_comment() {
    let a = "fn f() {\n    // explains the step\n    step();\n    other();\n}\n";
    let b = "fn f() {\n    // explains the step in more detail\n    step();\n    other();\n}\n";
    let d = run("rust", a, b).unwrap();
    assert_eq!(
        texts(a, &d.old, Mark::Changed),
        vec!["// explains the step"],
        "{d:?}"
    );
    assert_eq!(d.old.len(), 1);
    assert_eq!(d.new.len(), 1);
    assert!(texts(b, &d.new, Mark::Changed)[0].contains("more detail"));
    // A block comment between statements.
    let a = "fn f() { one(); /* old note */ two(); }\n";
    let b = "fn f() { one(); /* new note */ two(); }\n";
    let d = run("rust", a, b).unwrap();
    assert_eq!(texts(a, &d.old, Mark::Changed), vec!["/* old note */"]);
}

#[test]
fn reformatted_block_is_no_change() {
    let a = "fn f(x: i32) -> i32 { if x > 1 { x * 2 } else { x - 1 } }\n";
    let b = "fn f(x: i32) -> i32 {\n    if x > 1 {\n        x * 2\n    } else {\n        x - 1\n    }\n}\n";
    assert!(run("rust", a, b).unwrap().is_empty());
    let py_a = "def f(x):\n    return [x, x + 1, x + 2]\n";
    let py_b = "def f(x):\n    return [\n        x,\n        x + 1,\n        x + 2,\n    ]\n";
    let d = run("python", py_a, py_b).unwrap();
    // Only the trailing comma is new.
    assert!(d.old.is_empty(), "{d:?}");
}

/// A file of `n` functions of ten lines.
fn big_file(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                "fn work_{i}(input: &[u32]) -> u32 {{\n    let mut total = 0;\n    for v in input {{\n        if *v > {i} {{\n            total += v;\n        }} else {{\n            total += 1;\n        }}\n    }}\n    total\n}}\n\n"
            )
        })
        .collect()
}

#[test]
fn work_scales_with_the_change_not_the_file() {
    for n in [20usize, 400] {
        let old = big_file(n);
        let mut new = old.clone();
        let at = new.find(&format!("fn work_{}", n / 2)).unwrap();
        let edit = new[at..].find("total += v;").unwrap() + at;
        new.replace_range(edit..edit + 11, "total += v * 2;");
        let (so, sn) = parse_rust(&old, &new);
        let d = diff(&so, &old, &sn, &new, &Budget::default()).unwrap();
        assert!(d.skipped.is_empty());
        assert!(d.states < 200, "{n} functions: {} states", d.states);
        // `v` becomes `v * 2`: only the new operator and operand are marked.
        assert!(d.old.is_empty(), "{d:?}");
        assert_eq!(d.new.len(), 1, "{d:?}");
        assert_eq!(d.new[0].mark, Mark::Added, "{d:?}");
        assert_eq!(&new[d.new[0].range.clone()], "* 2");
    }
}

#[test]
fn a_region_over_its_budget_falls_back_to_line_and_word_diff() {
    // Region 1 is a one-token edit; region 2 replaces a whole function by
    // a differently shaped one, which the tiny region budget cannot solve.
    let old = "fn a() { one(1); }\n\nfn b(x: i32) -> i32 {\n    let y = x + 1;\n    let z = y * 2;\n    if z > 10 { z } else { y }\n}\n\nfn c() { three(3); }\n";
    let new = "fn a() { one(2); }\n\nfn b(x: i32) -> i32 {\n    match x {\n        0 => 1,\n        n if n > 5 => n * 2,\n        _ => x,\n    }\n}\n\nfn c() { three(3); }\n";
    let (so, sn) = parse_rust(old, new);
    let budget = Budget {
        max_region_states: 40,
        ..Budget::default()
    };
    let d = diff(&so, old, &sn, new, &budget).unwrap();
    assert_eq!(d.skipped.len(), 1, "{d:?}");
    assert_eq!(d.skipped[0].reason, SkipReason::RegionBudget);
    // The small edit is still structural and exact.
    assert_eq!(texts(old, &d.old, Mark::Changed).first(), Some(&"1"));
    // The skipped region is marked from the line diff: nothing in it is hidden.
    let region_old = &old[d.skipped[0].old.clone()];
    assert!(region_old.contains("let y = x + 1;"));
    assert!(
        d.old.iter().any(|m| old[m.range.clone()].contains("let y")),
        "{d:?}"
    );
    assert!(
        d.new.iter().any(|m| new[m.range.clone()].contains("match")),
        "{d:?}"
    );
    assert_eq!(unmarked(old, &d.old), unmarked(new, &d.new));
    // With the default budget the same input is fully structural.
    let full = diff(&so, old, &sn, new, &Budget::default()).unwrap();
    assert!(full.skipped.is_empty());
    assert_eq!(unmarked(old, &full.old), unmarked(new, &full.new));
}

#[test]
fn exhausted_total_budget_skips_the_remaining_regions() {
    let (old, new) = many_changes(30);
    let (so, sn) = parse_rust(&old, &new);
    let budget = Budget {
        max_states: 300,
        ..Budget::default()
    };
    let d = diff(&so, &old, &sn, &new, &budget).expect("some regions fit");
    assert!(!d.skipped.is_empty());
    assert!(d
        .skipped
        .iter()
        .all(|r| r.reason == SkipReason::TotalBudget));
    assert_eq!(unmarked(&old, &d.old), unmarked(&new, &d.new));
    for pair in d.skipped.windows(2) {
        assert!(
            pair[0].old.end <= pair[1].old.start,
            "regions overlap: {:?}",
            d.skipped
        );
    }
}

/// The text of a side without its marked ranges and without whitespace.
fn unmarked(src: &str, marks: &[Marked]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for m in marks {
        out.push_str(&src[at..m.range.start]);
        at = m.range.end;
    }
    out.push_str(&src[at..]);
    out.chars().filter(|c| !c.is_whitespace()).collect()
}

fn stmt(n: u8) -> &'static str {
    match n {
        0 => "let a = 1;",
        1 => "let b = foo(2, 3);",
        2 => "if x { y(); }",
        3 => "return bar;",
        4 => "baz.qux(1);",
        5 => "let c = [1, 2];",
        6 => "for i in 0..n { total += i * 2; }",
        7 => "let d = compute(a, b) + helper(c);",
        8 => "match v { Some(w) => w, None => 0 };",
        _ => "while ok { step(); }",
    }
}

/// A function made of statements; `mutations` tweak the new side.
fn program(funcs: &[Vec<u8>]) -> String {
    funcs
        .iter()
        .enumerate()
        .map(|(i, body)| {
            format!(
                "fn f{i}() {{\n{}\n}}\n\n",
                body.iter().map(|n| stmt(*n)).collect::<Vec<_>>().join("\n")
            )
        })
        .collect()
}

fn mutate(src: &str, how: u8, at: usize) -> String {
    let mut out = src.to_string();
    let pick = |pat: &str, out: &str| -> Option<usize> {
        let hits: Vec<usize> = out.match_indices(pat).map(|(i, _)| i).collect();
        (!hits.is_empty()).then(|| hits[at % hits.len()])
    };
    match how % 6 {
        0 => {
            if let Some(i) = pick("foo", &out) {
                out.replace_range(i..i + 3, "renamed");
            }
        }
        1 => {
            if let Some(i) = pick("1", &out) {
                out.replace_range(i..i + 1, "77");
            }
        }
        2 => {
            if let Some(i) = pick("\n    ", &out) {
                out.insert_str(i, "\n    inserted_call(9);");
            }
        }
        3 => {
            if let Some(i) = pick("compute(a, b)", &out) {
                out.replace_range(i..i + 13, "Some(compute(a, b))");
            }
        }
        4 => {
            if let Some(i) = pick("helper(c)", &out) {
                out.replace_range(i..i + 9, "");
            }
        }
        _ => {
            if let Some(i) = pick("y();", &out) {
                out.replace_range(i..i + 4, "y(); z();");
            }
        }
    }
    out
}

/// Whitespace-only reformatting: random breaks and indentation between tokens.
fn reformat(src: &str, seed: u64) -> String {
    let mut out = String::new();
    let mut x = seed | 1;
    for ch in src.chars() {
        if ch == ' ' || ch == '\n' {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.push_str(match x % 4 {
                0 => "\n        ",
                1 => "  ",
                2 => "\n",
                _ => " ",
            });
        } else {
            out.push(ch);
        }
    }
    out
}

fn funcs() -> impl Strategy<Value = Vec<Vec<u8>>> {
    proptest::collection::vec(proptest::collection::vec(0u8..10, 1..8), 1..6)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Whitespace reformatting never changes anything; identical token
    /// sequences are never marked.
    #[test]
    fn reformatting_marks_nothing(f in funcs(), seed in any::<u64>()) {
        let old = program(&f);
        let new = reformat(&old, seed);
        let d = run("rust", &old, &new).unwrap();
        prop_assert!(d.is_empty(), "{d:?}\n{new}");
    }

    /// The marked ranges of both sides cover everything that differs and
    /// nothing else: what remains unmarked is identical (modulo
    /// whitespace), in order. Marks are sorted, disjoint and in bounds,
    /// and the unmarked remainder stays identical for every budget (also
    /// when regions fall back to the line + word diff).
    #[test]
    fn unmarked_remainders_are_identical(
        f in funcs(),
        edits in proptest::collection::vec((any::<u8>(), any::<usize>()), 0..6),
        region_budget in prop_oneof![Just(1usize), Just(60), Just(2_000), Just(60_000)],
        total_budget in prop_oneof![Just(30usize), Just(5_000), Just(400_000)],
    ) {
        let old = program(&f);
        let mut new = old.clone();
        for (how, at) in edits {
            new = mutate(&new, how, at);
        }
        let (so, sn) = parse_rust(&old, &new);
        let budget = Budget { max_region_states: region_budget, max_states: total_budget, ..Budget::default() };
        let Some(d) = diff(&so, &old, &sn, &new, &budget) else {
            // Nothing could be solved within the budget: the caller shows the line diff.
            return Ok(());
        };
        for (marks, src) in [(&d.old, &old), (&d.new, &new)] {
            let mut prev = 0;
            for m in marks {
                prop_assert!(m.range.start >= prev && m.range.start < m.range.end && m.range.end <= src.len());
                prop_assert!(src.is_char_boundary(m.range.start) && src.is_char_boundary(m.range.end));
                prev = m.range.end;
            }
        }
        prop_assert_eq!(unmarked(&old, &d.old), unmarked(&new, &d.new), "{:?}", d);
        if old == new || old.split_whitespace().collect::<Vec<_>>() == new.split_whitespace().collect::<Vec<_>>() {
            prop_assert!(d.is_empty());
        }
        if budget.max_region_states >= 60_000 && budget.max_states >= 400_000 {
            // A generous budget never skips on these small inputs.
            prop_assert!(d.skipped.is_empty(), "{:?}", d.skipped);
        }
    }
}
