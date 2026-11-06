use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lector_search::Picker;

fn settle(p: &mut Picker<String>) {
    let start = Instant::now();
    while p.tick(10) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "matcher did not settle"
        );
    }
    p.tick(10);
}

fn synthetic(n: usize) -> Vec<String> {
    let dirs = [
        "src", "crates", "tests", "docs", "vendor", "lib", "app", "core",
    ];
    (0..n)
        .map(|i| {
            format!(
                "{}/{}/{}/module_{}_{}.rs",
                dirs[i % 8],
                dirs[(i / 8) % 8],
                dirs[(i / 64) % 8],
                i % 1000,
                i
            )
        })
        .collect()
}

#[test]
fn basic_matching_scores_and_indices() {
    let notified = Arc::new(AtomicUsize::new(0));
    let n2 = notified.clone();
    let mut p: Picker<String> = Picker::new_for_paths(Arc::new(move || {
        n2.fetch_add(1, Ordering::Relaxed);
    }));
    let inj = p.injector();
    for s in ["src/main.rs", "src/lib.rs", "README.md", "docs/Main.md"] {
        inj.push(s.to_string(), s);
    }
    p.set_query("");
    settle(&mut p);
    assert_eq!(p.total_count(), 4);
    assert_eq!(p.matched_count(), 4);
    p.set_query("main");
    settle(&mut p);
    assert_eq!(p.matched_count(), 2);
    let m = p.matches(0..10);
    assert_eq!(m.len(), 2);
    assert!(m[0].score >= m[1].score);
    assert_eq!(m[0].item, "src/main.rs");
    assert_eq!(m[0].indices, vec![4, 5, 6, 7]);
    // smart case: uppercase query is case sensitive
    p.set_query("Main");
    settle(&mut p);
    assert_eq!(p.matched_count(), 1);
    assert_eq!(p.matches(0..1)[0].item, "docs/Main.md");
    // shrinking and growing queries both work (append optimisation)
    p.set_query("Ma");
    settle(&mut p);
    assert_eq!(p.matched_count(), 1);
    p.set_query("zzz");
    settle(&mut p);
    assert_eq!(p.matched_count(), 0);
    assert!(p.matches(0..5).is_empty());
    assert!(notified.load(Ordering::Relaxed) > 0);
    // negation must not use the append shortcut
    p.set_query("rs");
    settle(&mut p);
    assert_eq!(p.matched_count(), 2);
    p.set_query("rs !lib");
    settle(&mut p);
    assert_eq!(p.matched_count(), 1);
    p.set_query("rs !li");
    settle(&mut p);
    assert_eq!(p.matched_count(), 1);
    p.set_query("rs !");
    settle(&mut p);
    assert_eq!(p.matched_count(), 2);
}

#[test]
fn clear_resets() {
    let mut p: Picker<String> = Picker::new(Arc::new(|| {}));
    p.injector().push("a".into(), "a");
    settle(&mut p);
    assert_eq!(p.total_count(), 1);
    p.clear();
    p.tick(10);
    assert_eq!(p.total_count(), 0);
    p.injector().push("b".into(), "b");
    p.set_query("b");
    settle(&mut p);
    assert_eq!(p.matched_count(), 1);
}

#[test]
fn time_to_first_results_200k_paths() {
    let paths = synthetic(200_000);
    let mut p: Picker<String> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    let t0 = Instant::now();
    for s in &paths {
        inj.push(s.clone(), s);
    }
    let inject = t0.elapsed();
    p.set_query("mod12 crate");
    let t1 = Instant::now();
    let mut first = None;
    while p.tick(1) {
        if first.is_none() && p.matched_count() > 0 {
            first = Some(t1.elapsed());
        }
        assert!(t1.elapsed() < Duration::from_secs(20));
    }
    p.tick(1);
    let total = t1.elapsed();
    let first = first.unwrap_or(total);
    let visible = p.matches(0..30);
    println!(
        "picker 200k paths: inject {inject:?}, first results {first:?}, settled {total:?}, matched {}",
        p.matched_count()
    );
    assert!(!visible.is_empty());
    assert!(visible[0].indices.len() >= 5);
}

/// Release gate: fuzzy file finder over 1M paths, first results < 50 ms.
/// `cargo test -p lector-search --release --test picker -- --ignored --nocapture`
#[test]
#[ignore = "release-gate measurement; run explicitly in release mode"]
fn release_gate_1m_paths() {
    let paths = synthetic(1_000_000);
    let mut p: Picker<String> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    let t0 = Instant::now();
    for s in &paths {
        inj.push(s.clone(), s);
    }
    let inject = t0.elapsed();
    // Let the empty query settle (the finder is open before typing).
    p.set_query("");
    settle(&mut p);
    assert_eq!(p.total_count(), 1_000_000);
    let mut report = Vec::new();
    // Type a query keystroke by keystroke, like the finder does. nucleo publishes a
    // snapshot only when a matching pass completes, so the time until `tick` stops
    // running is the time to the first results for that query.
    let query = "crate mod12";
    for end in 1..=query.len() {
        let q = &query[..end];
        let t = Instant::now();
        p.set_query(q);
        while p.tick(1) {
            assert!(t.elapsed() < Duration::from_secs(20));
        }
        p.tick(1);
        let top = p.matches(0..50);
        assert!(!top.is_empty());
        report.push((q.to_string(), t.elapsed(), p.matched_count()));
    }
    println!("picker 1M paths: inject {inject:?}");
    for (q, results, n) in &report {
        println!("  {q:>12}: results {results:?}, matched {n}");
    }
}

// ---------------------------------------------------------------------------
// Top-k picker behaviour

use lector_search::MAX_RESULTS;

fn paths_picker(items: &[String]) -> Picker<usize> {
    let p: Picker<usize> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    for (i, s) in items.iter().enumerate() {
        inj.push(i, s);
    }
    p
}

fn settle_u(p: &mut Picker<usize>) {
    let start = Instant::now();
    while p.tick(10) {
        assert!(start.elapsed() < Duration::from_secs(20), "did not settle");
    }
}

/// Brute-force ranking with nucleo directly: (score desc, shorter haystack, lower index).
fn brute_force(items: &[String], query: &str) -> Vec<(usize, u32)> {
    use nucleo::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo::{Config, Matcher, Utf32String};
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut out: Vec<(usize, u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let hay = Utf32String::from(s.as_str());
            pattern
                .score(hay.slice(..), &mut matcher)
                .map(|sc| (i, sc, hay.len()))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
    out.into_iter().map(|(i, s, _)| (i, s)).collect()
}

#[test]
fn top_k_matches_brute_force_ranking() {
    let items = synthetic(5000);
    let mut p = paths_picker(&items);
    // Typed incrementally (exercises narrowing) and with shortenings (history reuse).
    for q in [
        "m",
        "mo",
        "mod1",
        "mod12",
        "mod1",
        "src",
        "src/cr",
        "mod9 core",
        "zz",
        "m",
    ] {
        p.set_query(q);
        settle_u(&mut p);
        let expect = brute_force(&items, q);
        assert_eq!(p.matched_count() as usize, expect.len(), "count for {q:?}");
        let got: Vec<(usize, u32)> = p
            .matches(0..MAX_RESULTS as u32)
            .iter()
            .map(|m| (*m.item, m.score))
            .collect();
        let n = expect.len().min(MAX_RESULTS);
        assert_eq!(got, expect[..n], "ranking for {q:?}");
    }
}

#[test]
fn shorter_paths_win_ties_and_boost_orders() {
    let mut p: Picker<&'static str> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    inj.push("long", "aaa/bbb/ccc/lib.rs");
    inj.push("short", "lib.rs");
    inj.push_boosted("recent", "zzz/zzz/lib.rs", 500);
    p.set_query("");
    while p.tick(5) {}
    let order: Vec<&str> = p.matches(0..10).iter().map(|m| *m.item).collect();
    assert_eq!(order[0], "recent", "boost orders the empty query");
    p.set_query("lib");
    while p.tick(5) {}
    let order: Vec<&str> = p.matches(0..10).iter().map(|m| *m.item).collect();
    assert_eq!(order[0], "recent");
    // Same score without the boost: shorter path first.
    let mut p: Picker<&'static str> = Picker::new_for_paths(Arc::new(|| {}));
    let inj = p.injector();
    inj.push("long", "aaa/bbb/ccc/lib.rs");
    inj.push("short", "lib.rs");
    p.set_query("lib.rs");
    while p.tick(5) {}
    let order: Vec<&str> = p.matches(0..10).iter().map(|m| *m.item).collect();
    assert_eq!(order, vec!["short", "long"]);
}

#[test]
fn items_injected_after_matching_are_found() {
    let items = synthetic(2000);
    let mut p = paths_picker(&items[..1000]);
    p.set_query("mod7");
    settle_u(&mut p);
    let first = p.matched_count();
    let inj = p.injector();
    for (i, s) in items.iter().enumerate().skip(1000) {
        inj.push(i, s);
    }
    settle_u(&mut p);
    let expect = brute_force(&items, "mod7");
    assert!(p.matched_count() > first);
    assert_eq!(p.matched_count() as usize, expect.len());
    assert_eq!(p.total_count(), 2000);
    // Extending the query after new items arrived uses the grown candidate set.
    p.set_query("mod77");
    settle_u(&mut p);
    assert_eq!(
        p.matched_count() as usize,
        brute_force(&items, "mod77").len()
    );
}

#[test]
fn concurrent_injection_while_typing() {
    let items = Arc::new(synthetic(30_000));
    let mut p = paths_picker(&[]);
    let inj = p.injector();
    let items2 = items.clone();
    let t = std::thread::spawn(move || {
        for (i, s) in items2.iter().enumerate() {
            inj.push(i, s);
        }
    });
    for q in ["c", "cr", "cra", "crat", "crate"] {
        p.set_query(q);
        p.tick(2);
    }
    t.join().unwrap();
    settle_u(&mut p);
    assert_eq!(p.total_count(), 30_000);
    assert_eq!(
        p.matched_count() as usize,
        brute_force(&items, "crate").len()
    );
}

#[test]
fn query_syntax_and_negation_still_work_with_history() {
    let items: Vec<String> = ["src/main.rs", "src/lib.rs", "src/libx.rs", "README.md"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut p = paths_picker(&items);
    for q in [
        "rs", "rs !lib", "rs !li", "rs !libx", "^src", "^src rs$", "'main", "rs !",
    ] {
        p.set_query(q);
        settle_u(&mut p);
        assert_eq!(
            p.matched_count() as usize,
            brute_force(&items, q).len(),
            "{q:?}"
        );
    }
}

#[test]
fn tick_reports_running_and_results_are_partial_then_complete() {
    let items = synthetic(300_000);
    let mut p = paths_picker(&items);
    settle_u(&mut p);
    p.set_query("c");
    // Partial results show up quickly; the pass completes later.
    let t = Instant::now();
    let mut saw_results = false;
    while p.tick(1) {
        if !saw_results && !p.matches(0..5).is_empty() {
            saw_results = true;
        }
        assert!(t.elapsed() < Duration::from_secs(20));
    }
    assert!(saw_results || p.matched_count() > 0);
    assert!(!p.matches(0..5).is_empty());
    assert!(p.matched_count() > 250_000);
}

#[test]
fn drop_and_clear_do_not_hang() {
    let items = synthetic(1000);
    let mut p = paths_picker(&items);
    p.set_query("mod");
    p.clear();
    assert_eq!(p.total_count(), 0);
    p.injector().push(1, "a/b.rs");
    p.set_query("b");
    settle_u(&mut p);
    assert_eq!(p.matched_count(), 1);
    drop(p);
}
