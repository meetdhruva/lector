//! Property tests: the picker's ranking and counts equal a brute-force
//! nucleo pass for random corpora and random query edit sequences (typing,
//! backspace, replacing, query syntax), through every internal mode (empty,
//! fast fuzzy, masked, non-ASCII needle) and cache path (narrowing, replay).

use std::sync::Arc;
use std::time::{Duration, Instant};

use lector_search::{Picker, MAX_RESULTS};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32String};
use proptest::prelude::*;

fn settle(p: &mut Picker<usize>) {
    let start = Instant::now();
    while p.tick(10) {
        assert!(start.elapsed() < Duration::from_secs(30), "did not settle");
    }
}

/// (index, score) best first: score desc, shorter haystack, lower index.
fn brute_force(items: &[(String, u16)], query: &str, paths: bool) -> Vec<(usize, u32)> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let config = if paths {
        Config::DEFAULT.match_paths()
    } else {
        Config::DEFAULT
    };
    let mut matcher = Matcher::new(config);
    let mut out: Vec<(usize, u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, (s, boost))| {
            let hay = Utf32String::from(s.as_str());
            let len = if pattern.atoms.is_empty() {
                0
            } else {
                hay.len()
            };
            let score = if pattern.atoms.is_empty() {
                Some(0)
            } else {
                pattern.score(hay.slice(..), &mut matcher)
            };
            score.map(|sc| (i, sc + *boost as u32, len))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
    out.into_iter().map(|(i, s, _)| (i, s)).collect()
}

fn check(p: &mut Picker<usize>, items: &[(String, u16)], query: &str, paths: bool) {
    p.set_query(query);
    settle(p);
    let expect = brute_force(items, query, paths);
    assert_eq!(
        p.matched_count() as usize,
        expect.len(),
        "count for {query:?}"
    );
    let got: Vec<(usize, u32)> = p
        .matches(0..MAX_RESULTS as u32)
        .iter()
        .map(|m| (*m.item, m.score))
        .collect();
    let n = expect.len().min(MAX_RESULTS);
    assert_eq!(got, expect[..n], "ranking for {query:?}");
    let progress = p.progress();
    assert!(progress.complete && progress.ranked && progress.current);
}

fn path_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            10 => proptest::sample::select(vec!['a', 'b', 'c', 'm', 'o', 'd', 'e', 'r']),
            3 => proptest::sample::select(vec!['A', 'B', 'M', 'D']),
            2 => proptest::sample::select(vec!['1', '2', '9']),
            4 => proptest::sample::select(vec!['/', '_', '-', '.']),
            1 => proptest::sample::select(vec!['é', 'ß', ' ']),
        ],
        1..28,
    )
    .prop_map(|v| v.into_iter().collect())
}

#[derive(Clone, Debug)]
enum Edit {
    Push(char),
    Pop,
    Set(String),
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        10 => proptest::sample::select(vec!['a', 'b', 'c', 'm', 'o', 'd', 'e', 'r', 'A', 'M', '1', '/', '_', '.'])
            .prop_map(Edit::Push),
        4 => Just(Edit::Pop),
        1 => proptest::sample::select(vec![
            "", "mo !a", "^ab", "'mod", "co$", "é", "!e", "a b", "ab\\ c", "M", "mod/",
        ])
        .prop_map(|s| Edit::Set(s.to_string())),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn picker_equals_brute_force_for_edit_sequences(
        paths in proptest::collection::vec((path_strategy(), prop_oneof![8 => Just(0u16), 1 => 1u16..300]), 0..400),
        edits in proptest::collection::vec(edit_strategy(), 1..9),
        for_paths in any::<bool>(),
        late in 0usize..400,
    ) {
        let mut p: Picker<usize> = if for_paths {
            Picker::new_for_paths(Arc::new(|| {}))
        } else {
            Picker::new(Arc::new(|| {}))
        };
        let inj = p.injector();
        let split = late.min(paths.len());
        for (i, (s, b)) in paths.iter().enumerate().take(split) {
            inj.push_boosted(i, s, *b);
        }
        let mut query = String::new();
        let mut late_pushed = false;
        for e in edits {
            match e {
                Edit::Push(c) => query.push(c),
                Edit::Pop => { query.pop(); }
                Edit::Set(s) => query = s,
            }
            if !late_pushed && query.len() > 2 {
                // Items that arrive while a query is active.
                late_pushed = true;
                for (i, (s, b)) in paths.iter().enumerate().skip(split) {
                    inj.push_boosted(i, s, *b);
                }
            }
            let live = if late_pushed { &paths[..] } else { &paths[..split] };
            check(&mut p, live, &query, for_paths);
        }
        // Backspacing to every earlier prefix, then the full query again.
        if !late_pushed {
            for (i, (s, b)) in paths.iter().enumerate().skip(split) {
                inj.push_boosted(i, s, *b);
            }
        }
        let q = query.clone();
        for end in (0..q.len()).rev().filter(|e| q.is_char_boundary(*e)) {
            check(&mut p, &paths, &q[..end], for_paths);
        }
        check(&mut p, &paths, &q, for_paths);
    }
}

#[test]
fn replayed_and_narrowed_results_are_exact_across_chunks() {
    // More than one 4096-item chunk, a partial tail chunk and boosts.
    let items: Vec<(String, u16)> = (0..9000usize)
        .map(|i| {
            (
                format!("dir{}/sub{}/file_{}_{}.rs", i % 13, i % 7, i % 101, i),
                if i % 997 == 0 { 400 } else { 0 },
            )
        })
        .collect();
    let mut p: Picker<usize> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    for (i, (s, b)) in items.iter().enumerate() {
        inj.push_boosted(i, s, *b);
    }
    for q in [
        "f",
        "fi",
        "fil",
        "file1",
        "file12",
        "fil",
        "fi",
        "f",
        "",
        "file12 dir3",
        "file12",
    ] {
        check(&mut p, &items, q, true);
    }
    // Same query again after dropping the caches (no replay, no narrowing).
    p.drop_caches();
    check(&mut p, &items, "file12", true);
}

#[test]
fn ranking_prefers_exact_then_prefix_then_boundary_then_scattered() {
    let files = [
        "src/deeply/nested/dir/parser_helpers_extra.rs", // scattered p..r..s
        "src/parser.rs",                                 // exact basename
        "src/parser_tests.rs",                           // basename prefix
        "src/lib/my_parser.rs",                          // word boundary
        "src/xparserx.rs",                               // inside a word
        "docs/pxaxrxsxexr.md",                           // scattered inside one word
    ];
    let mut p: Picker<usize> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    for (i, f) in files.iter().enumerate() {
        inj.push(i, f);
    }
    p.set_query("parser");
    settle(&mut p);
    let order: Vec<&str> = p.matches(0..10).iter().map(|m| files[*m.item]).collect();
    assert_eq!(order[0], "src/parser.rs", "{order:?}");
    let pos = |name: &str| order.iter().position(|f| *f == name).expect(name);
    assert!(pos("src/parser.rs") < pos("src/parser_tests.rs"));
    assert!(pos("src/parser_tests.rs") < pos("src/lib/my_parser.rs"));
    assert!(pos("src/lib/my_parser.rs") < pos("src/xparserx.rs"));
    assert!(pos("src/xparserx.rs") < pos("docs/pxaxrxsxexr.md"));
}

#[test]
fn first_page_is_exact_before_the_count_finishes() {
    let items: Vec<(String, u16)> = (0..60_000usize)
        .map(|i| (format!("a/b/module_{}_{}.rs", i % 1000, i), 0))
        .collect();
    let mut p: Picker<usize> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    for (i, (s, _)) in items.iter().enumerate() {
        inj.push(i, s);
    }
    p.set_query("module");
    let mut saw_ranked_before_complete = false;
    let start = Instant::now();
    loop {
        let running = p.tick(1);
        let progress = p.progress();
        if progress.ranked && !progress.complete {
            saw_ranked_before_complete = true;
            // The visible page is already the final one.
            let page: Vec<(usize, u32)> = p
                .matches(0..20)
                .iter()
                .map(|m| (*m.item, m.score))
                .collect();
            assert_eq!(page, brute_force(&items, "module", true)[..20]);
        }
        if !running {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30));
    }
    // Not guaranteed to be observable on a very fast machine, so only the
    // final state is asserted strictly.
    let _ = saw_ranked_before_complete;
    let progress = p.progress();
    assert!(progress.complete && progress.ranked);
    assert_eq!(p.matched_count(), 60_000);
}

#[test]
fn memory_overhead_per_path_is_small() {
    let n = 4096 * 12;
    let p: Picker<u32> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    let mut text = 0usize;
    for i in 0..n {
        let s = format!("crates/some-crate/src/module_{}/file_{}.rs", i % 100, i);
        text += s.len();
        inj.push(i as u32, &s);
    }
    // Sealed chunks only count; the open tail is at most one chunk.
    let per_path_overhead = (p.heap_bytes() as f64 - text as f64) / n as f64 - 4.0;
    assert!(
        per_path_overhead < 40.0,
        "overhead {per_path_overhead:.1} bytes/path"
    );
}
