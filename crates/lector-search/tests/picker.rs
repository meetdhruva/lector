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
