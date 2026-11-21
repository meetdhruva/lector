//! Tests of the bounds on the work the scorer does: the cap on the length of a query atom
//! (long atoms are substring searches), the work counter, the budgeted ranking and the
//! cancellation by work. They use the reviewer's shape: thousands of 256-character names against
//! 100- and 200-character pasted queries.

use std::cell::Cell;
use std::time::{Duration, Instant};

use super::*;

/// `n` names of 256 characters made only of `a` (the worst case for the alignment: every
/// candidate passes the cheap test), numbered at the end.
fn all_a(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("{}{}", "a".repeat(250), i % 1000))
        .collect()
}

/// `n` plausible names of about 40 characters.
fn typical(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("src/module_{}/component_{i}_view.rs", i % 97))
        .collect()
}

fn atom(text: &str) -> Atom {
    Atom::new(text)
}

#[test]
fn atoms_longer_than_the_cap_are_substring_atoms() {
    assert!(atom(&"a".repeat(MAX_FUZZY_ATOM_CHARS)).fail.is_none());
    assert!(atom(&"a".repeat(MAX_FUZZY_ATOM_CHARS + 1)).fail.is_some());
    // The cap counts characters, not bytes.
    assert!(atom(&"é".repeat(MAX_FUZZY_ATOM_CHARS)).fail.is_none());
}

#[test]
fn a_long_atom_matches_only_as_a_case_insensitive_substring() {
    let needle = "the_quick_brown_fox_jumps_over_the_lazy_dog_and_keeps_running_far_away";
    assert!(needle.len() > MAX_FUZZY_ATOM_CHARS);
    let text = format!("src/{}/mod.rs", needle.to_uppercase());
    let found = score(needle, &text, &Options::default()).expect("substring matches");
    assert_eq!(found.positions, (4..4 + needle.len()).collect::<Vec<_>>());
    // All its characters in order, but not contiguous: no match (a short atom would match).
    let scattered = needle.chars().map(|c| format!("{c}-")).collect::<String>();
    assert!(score(needle, &scattered, &Options::default()).is_none());
    assert!(score(&needle[..20], &scattered, &Options::default()).is_some());
    // Longer than the candidate.
    assert!(score(&needle.repeat(5), &text, &Options::default()).is_none());
}

#[test]
fn positions_of_a_long_atom_are_byte_offsets_of_the_run() {
    let needle = format!("é{}ö", "x".repeat(70));
    let text = format!("ab {needle} cd");
    let found = score(&needle, &text, &Options::default()).expect("matches");
    let first = text.find('é').unwrap();
    let expected: Vec<usize> = text[first..]
        .char_indices()
        .take(needle.chars().count())
        .map(|(at, _)| first + at)
        .collect();
    assert_eq!(found.positions, expected);
    assert!(found.positions.windows(2).all(|w| w[0] < w[1]));
    for &at in &found.positions {
        assert!(text.is_char_boundary(at));
    }
}

#[test]
fn a_long_atom_scores_what_the_alignment_gives_a_contiguous_match() {
    // The same contiguous atom, scored by the alignment (64 characters) and by the substring
    // search called directly on the same data, agrees.
    let needle = "wordA_wordB_wordC_wordD_wordE_wordF_wordG_wordH_wordI_wordJ_wordK_wo";
    assert_eq!(needle.len(), MAX_FUZZY_ATOM_CHARS + 4);
    let needle = &needle[..MAX_FUZZY_ATOM_CHARS];
    let a = atom(needle);
    assert!(a.fail.is_none());
    let fail = substring::failure_table(&a.folded);
    for text in [
        needle.to_string(),
        format!("{needle}_tail"),
        format!("head/{needle}"),
        format!("head{needle}"),
        format!("x_{}", needle.to_lowercase()),
    ] {
        let by_alignment = Matcher::new().score(&Query::new(needle), &text).unwrap();
        let mut matcher = Matcher::new();
        matcher.fill_chars(&text);
        let parts = (&a.folded[..], &a.orig[..], &fail[..]);
        let by_search =
            substring::score(parts, &matcher.orig_c, &matcher.fold_c, &matcher.bonus).unwrap();
        assert_eq!(by_alignment, by_search.0, "{text}");
    }
}

#[test]
fn substring_fallback_ranks_sensibly() {
    let needle = "configuration_manager_for_the_workspace_that_keeps_user_settings_in_sync_ok";
    assert!(needle.len() > MAX_FUZZY_ATOM_CHARS);
    let candidates = [
        needle.to_string(),                    // exact
        format!("{needle}_extra"),             // prefix
        format!("src/{needle}.rs"),            // after a path separator
        format!("x_{needle}"),                 // after an underscore
        format!("zz{needle}"),                 // inside a word
        format!("zzzzzzzzzzzzzzzzzz{needle}"), // inside a word, further in
        needle.replacen("keeps", "keepz", 1),  // not a substring
    ];
    let ranked = Matcher::new().rank(&Query::new(needle), &candidates, 10);
    let order: Vec<usize> = ranked.iter().map(|r| r.index).collect();
    assert_eq!(order, [0, 1, 2, 3, 4, 5]);
    assert!(ranked.windows(2).all(|w| w[0].score > w[1].score));
    // Positions are the run in every row.
    for r in &ranked {
        let at = candidates[r.index].find(needle).unwrap();
        assert_eq!(r.positions, (at..at + needle.len()).collect::<Vec<_>>());
    }
    // Several occurrences: the one at a word start beats an earlier one inside a word.
    let twice = format!("zz{needle} {needle}");
    let found = score(needle, &twice, &Options::default()).unwrap();
    let second = twice.rfind(needle).unwrap();
    assert_eq!(found.positions[0], second);
}

#[test]
fn long_and_short_atoms_combine() {
    let needle = "a_very_long_identifier_pasted_from_somewhere_else_in_the_project_tree_now";
    let text = format!("lib/{needle}/main.rs");
    let found = score(&format!("main {needle}"), &text, &Options::default()).unwrap();
    let mut expected: Vec<usize> = (4..4 + needle.len()).collect();
    let main = text.rfind("main").unwrap();
    expected.extend(main..main + 4);
    expected.sort_unstable();
    assert_eq!(found.positions, expected);
    // The score is the sum of both atoms' scores.
    let mut m = Matcher::new();
    let long = m.score(&Query::new(needle), &text).unwrap();
    let short = m.score(&Query::new("main"), &text).unwrap();
    assert_eq!(
        m.score(&Query::new(&format!("main {needle}")), &text),
        Some(long + short)
    );
    // A long atom that does not occur rejects the candidate.
    assert!(m
        .score(&Query::new(&format!("main {needle}x")), &text)
        .is_none());
}

#[test]
fn the_index_path_applies_the_same_cap() {
    let needle = "indexed_completion_candidate_name_that_was_pasted_into_the_editor_window_x";
    let texts = [
        needle.to_string(),
        format!("pre_{needle}_post"),
        needle.replace("pasted", "typed"),
        "unrelated".to_string(),
        needle.to_uppercase(),
    ];
    let options = Options { max_chars: 128 };
    let mut index = Index::new(&options);
    for t in &texts {
        index.push(t);
    }
    let mut matcher = Matcher::with_options(&options);
    for q in [
        needle.to_string(),
        format!("{needle} name"),
        "name indexed".to_string(),
    ] {
        let query = Query::new(&q);
        for (slot, t) in texts.iter().enumerate() {
            assert_eq!(
                matcher.score_indexed(&index, slot, &query),
                Matcher::with_options(&options).score(&query, t),
                "{q:?} / {t:?}"
            );
        }
    }
}

#[test]
fn a_pasted_word_cannot_make_completion_slow() {
    // 10,000 completion items of up to 128 characters (the completion limit) against a 200
    // character pasted word: linear work per candidate, not 200 x 128 alignment cells.
    let options = Options { max_chars: 128 };
    let mut index = Index::with_capacity(&options, 10_000);
    for i in 0..10_000 {
        index.push(&format!("{}{}", "a".repeat(120), i % 1000));
    }
    let word = "a".repeat(200);
    for query in [
        Query::new(&word),
        Query::new(&"a".repeat(100)),
        Query::new(&"a".repeat(65)),
        Query::new(&"a".repeat(64)),
    ] {
        let mut matcher = Matcher::with_options(&options);
        let started = Instant::now();
        for slot in 0..index.len() {
            let _ = matcher.score_indexed(&index, slot, &query);
        }
        let per_candidate = matcher.work() / index.len() as u64;
        assert!(
            per_candidate <= 128 * 70,
            "{per_candidate} units per candidate"
        );
        if query.len() > MAX_FUZZY_ATOM_CHARS {
            assert!(
                per_candidate <= 8 + 4 * 128,
                "{per_candidate} units per candidate"
            );
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "{:?}",
                started.elapsed()
            );
        }
    }
}

#[test]
fn work_per_candidate_is_bounded_whatever_the_query_length() {
    let text = "ab".repeat(128);
    let bound_score = WORK_PER_CANDIDATE + 3 * 256 + (MAX_FUZZY_ATOM_CHARS as u64) * 256;
    for len in [1, 10, 64, 65, 200, 10_000] {
        let query = Query::new(&"ab".repeat(len).chars().take(len).collect::<String>());
        let mut matcher = Matcher::new();
        let _ = matcher.score(&query, &text);
        assert!(
            matcher.work() <= bound_score,
            "score, {len} chars: {}",
            matcher.work()
        );
        let mut matcher = Matcher::new();
        let _ = matcher.matches(&query, &text);
        assert!(
            matcher.work() <= bound_score + 4 * (MAX_FUZZY_ATOM_CHARS as u64) * 256,
            "matches, {len} chars: {}",
            matcher.work()
        );
    }
    // Pasting many words keeps the first MAX_QUERY_ATOMS.
    let words = (0..1000)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let query = Query::new(&words);
    assert_eq!(query.atoms.len(), MAX_QUERY_ATOMS);
    let mut matcher = Matcher::new();
    let _ = matcher.score(&Query::new(&"a ".repeat(1000)), &"a".repeat(256));
    assert!(matcher.work() <= MAX_QUERY_ATOMS as u64 * bound_score);
}

#[test]
fn query_prefix_relation_holds_for_long_atoms() {
    let long = "q".repeat(70);
    let longer = "q".repeat(80);
    assert!(Query::new(&long).is_prefix_of(&Query::new(&longer)));
    assert!(!Query::new(&longer).is_prefix_of(&Query::new(&long)));
    assert!(Query::new(&"q".repeat(10)).is_prefix_of(&Query::new(&longer)));
    // Soundness: whatever matches the longer atom matches the shorter one.
    let text = format!("x{longer}y");
    assert!(score(&longer, &text, &Options::default()).is_some());
    assert!(score(&long, &text, &Options::default()).is_some());
}

#[test]
fn the_ascii_decision_looks_at_the_scored_characters_only() {
    // Non-ASCII text after the matched window must not change the result (nor cost anything).
    let ascii = "ab_cd".repeat(100);
    let tail = format!("{ascii}é");
    let query = Query::new("abcd");
    let mut m = Matcher::new();
    assert_eq!(m.score(&query, &ascii), m.score(&query, &tail));
    assert_eq!(m.matches(&query, &ascii), m.matches(&query, &tail));
    // ... and non-ASCII inside the window still takes the character path.
    let inside = format!("é{ascii}");
    assert!(m.score(&query, &inside).is_some());
}

#[test]
fn budgeted_ranking_gives_up_by_work_not_by_count() {
    let names = all_a(5_000);
    for qlen in [64, 100, 200] {
        let query = Query::new(&"a".repeat(qlen));
        let mut matcher = Matcher::new();
        let started = Instant::now();
        let ranked = matcher.rank_by_budgeted(&query, &names, |s| s, 500, INTERACTIVE_WORK_BUDGET);
        assert!(
            ranked.is_none(),
            "{qlen}-character query is over budget on 256-character names"
        );
        // At most the budget and the candidate in progress was spent, nowhere near the whole list.
        let spent = matcher.work();
        assert!(spent > INTERACTIVE_WORK_BUDGET, "{spent}");
        assert!(spent <= INTERACTIVE_WORK_BUDGET + 20_000, "{spent}");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
        // The matcher is usable afterwards and has no leftover limit.
        assert!(matcher.rank(&Query::new("a"), &names[..10], 3).len() == 3);
    }
}

#[test]
fn budgeted_ranking_is_the_plain_ranking_when_it_fits() {
    let names = typical(300);
    for q in ["view", "mod 1 comp", "zzz", ""] {
        let query = Query::new(q);
        let mut matcher = Matcher::new();
        let budgeted =
            matcher.rank_by_budgeted(&query, &names, |s| s, 100, INTERACTIVE_WORK_BUDGET);
        assert!(budgeted.is_some(), "{q:?} fits the budget");
        assert_eq!(budgeted, Some(matcher.rank(&query, &names, 100)), "{q:?}");
    }
    // A longer list of the same names does not fit for a query that does real work.
    let names = typical(5_000);
    let mut matcher = Matcher::new();
    let query = Query::new("mod 1 comp");
    assert!(matcher
        .rank_by_budgeted(&query, &names, |s| s, 500, INTERACTIVE_WORK_BUDGET)
        .is_none());
}

#[test]
fn the_positions_of_the_rows_count_against_the_budget() {
    // Cheap to select (a long atom, a few hundred units a candidate) but 500 rows of positions
    // each cost an alignment: the budget covers both phases.
    let names = all_a(600);
    let query = Query::new(&"a".repeat(64));
    let mut matcher = Matcher::new();
    // The selection alone fits in this budget; only the positions overrun it.
    let selection = names.len() as u64 * (WORK_PER_CANDIDATE + 3 * 256 + 64 * 256);
    assert!(matcher
        .rank_by_budgeted(&query, &names, |s| s, 500, selection / 100)
        .is_none());
}

#[test]
fn cancellation_is_prompt_on_long_names_and_long_queries() {
    // Cancelled from inside the candidate source at candidate 100: the ranking stops within the
    // candidates that fit in one check interval, for every shape of query.
    let names = all_a(5_000);
    for qlen in [6, 64, 100, 200] {
        let query = Query::new(&"a".repeat(qlen));
        let per_candidate = {
            let mut m = Matcher::new();
            let _ = m.score(&query, &names[0]);
            m.work()
        };
        let token = CancelToken::new();
        let seen = Cell::new(0usize);
        let r = Matcher::new().rank_by_cancellable(
            &query,
            &names,
            |s| {
                seen.set(seen.get() + 1);
                if seen.get() == 100 {
                    token.cancel();
                }
                s
            },
            500,
            &token,
        );
        assert_eq!(r, Err(Cancelled), "{qlen}");
        let late = seen.get() as u64 - 100;
        assert!(
            late <= CANCEL_CHECK_WORK / per_candidate + 1,
            "{qlen}-character query: {late} more candidates ({per_candidate} units each)"
        );
        assert!(late < 5_000 - 100, "{qlen}");
    }
}

#[test]
fn cancelling_from_another_thread_returns_quickly_on_the_worst_shape() {
    // Uncancelled, this ranking (64-character query, 5,000 all-`a` names) runs for several
    // hundred milliseconds in a dev build.
    let names = std::sync::Arc::new(all_a(5_000));
    let token = CancelToken::new();
    let (t, n) = (token.clone(), names.clone());
    let query = Query::new(&"a".repeat(64));
    let worker = std::thread::spawn(move || Matcher::new().rank_cancellable(&query, &n, 500, &t));
    std::thread::sleep(Duration::from_millis(10));
    let cancelled_at = Instant::now();
    token.cancel();
    assert_eq!(worker.join().unwrap(), Err(Cancelled));
    assert!(
        cancelled_at.elapsed() < Duration::from_millis(250),
        "{:?}",
        cancelled_at.elapsed()
    );
}

/// Measures the nanoseconds per unit of [`Matcher::work`] on several shapes, to calibrate
/// [`INTERACTIVE_WORK_BUDGET`]: `cargo test -p lector-base --lib -- --ignored --nocapture
/// calibrate_work_units` (the workspace dev profile, as the UI is run during development).
#[test]
#[ignore = "prints timings"]
fn calibrate_work_units() {
    let pseudo = |n: usize, len: usize, alphabet: &str| -> Vec<String> {
        let a: Vec<char> = alphabet.chars().collect();
        let mut x: u64 = 12345;
        (0..n)
            .map(|_| {
                (0..len)
                    .map(|_| {
                        x = x
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        a[(x >> 33) as usize % a.len()]
                    })
                    .collect()
            })
            .collect()
    };
    let shapes: Vec<(&str, Vec<String>, Vec<String>)> = vec![
        (
            "all-a 256",
            all_a(5_000),
            [6, 16, 32, 64, 65, 100, 200]
                .iter()
                .map(|n| "a".repeat(*n))
                .collect(),
        ),
        (
            "two-letter 256",
            pseudo(5_000, 256, "ab"),
            [6, 32, 64, 100]
                .iter()
                .map(|n| "ab".repeat(n / 2))
                .collect(),
        ),
        (
            "typical 40",
            pseudo(5_000, 40, "abcdefghijklmnopqrstuvwxyz_/"),
            ["abc", "mainrs", "a b c", "abcde"]
                .map(String::from)
                .to_vec(),
        ),
    ];
    for (label, names, queries) in shapes {
        for q in queries {
            let query = Query::new(&q);
            let mut matcher = Matcher::new();
            let started = Instant::now();
            let kept = matcher.rank(&query, &names, 500).len();
            let took = started.elapsed();
            eprintln!(
                "{label:15} q={:3} kept={kept:3} time={took:>10?} work={:>10} ns/unit={:.2}",
                q.len(),
                matcher.work(),
                took.as_nanos() as f64 / matcher.work() as f64
            );
        }
    }
}
