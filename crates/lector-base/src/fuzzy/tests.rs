//! Unit and property tests of the scorer.

use super::*;
use proptest::prelude::*;

fn s(query: &str, text: &str) -> Option<i32> {
    Matcher::new().score(&Query::new(query), text)
}

fn m(query: &str, text: &str) -> Option<Match> {
    score(query, text, &Options::default())
}

fn order(query: &str, candidates: &[&str]) -> Vec<String> {
    let reversed: Vec<&str> = candidates.iter().rev().copied().collect();
    Matcher::new()
        .rank(&Query::new(query), &reversed, usize::MAX)
        .into_iter()
        .map(|r| reversed[r.index].to_string())
        .collect()
}

/// The shared ranking table (ADR 0005): query -> candidates, best first. It is the union of
/// the ranking tests of the old completion matcher (lector-lsp) and the old picker matcher
/// (lector-ui). Candidates are fed in reverse order, so a tie would fail the test too.
#[test]
fn shared_ranking_table() {
    let table: &[(&str, &[&str])] = &[
        // Prefix beats later occurrences.
        ("fo", &["foo", "xfoo"]),
        ("fo", &["foobar", "buffoon"]),
        ("pr", &["print", "xxprint"]),
        ("name", &["name", "rename"]),
        // Word boundaries (separator, camelCase) beat scattered matches.
        ("fb", &["foo_bar", "fxxbxx"]),
        ("fb", &["fooBar", "fobar_x"]),
        ("gc", &["get_count", "gic"]),
        ("sl", &["slice", "selfLen"]),
        ("ab", &["ab", "a_b", "axb"]),
        ("tsb", &["Toggle Side Bar", "tasbar", "testsubject"]),
        // Contiguity and word starts in paths.
        ("main", &["src/main.rs", "src/domain.rs"]),
        ("main", &["main.rs", "main_window.rs"]),
        ("lib", &["src/lib.rs", "src/public_library.rs"]),
        // Case: an exact-case match beats a different-case one.
        ("Vec", &["Vec", "vec"]),
        ("vec", &["vec", "Vec"]),
        // Equal scores: shorter text first, then input order (picker tie-break).
        ("fo", &["foo", "foobar", "foobarbaz"]),
        // Exact beats prefix.
        ("name", &["name", "names"]),
    ];
    for (query, expected) in table {
        assert_eq!(
            order(query, expected),
            expected.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "query {query:?}"
        );
    }
}

#[test]
fn scores_order_without_the_tie_break_where_the_old_lsp_table_did() {
    // The completion matcher compared raw scores: the better one must be strictly higher.
    for (q, better, worse) in [
        ("fo", "foo", "xfoo"),
        ("fb", "fooBar", "fobar_x"),
        ("Vec", "Vec", "vec"),
        ("vec", "vec", "Vec"),
        ("ab", "a_b", "axb"),
    ] {
        let (a, b) = (s(q, better).unwrap(), s(q, worse).unwrap());
        assert!(a > b, "{q}: {better} ({a}) should beat {worse} ({b})");
    }
}

#[test]
fn trailing_text_does_not_matter() {
    assert_eq!(s("fo", "foo"), s("fo", "foobarbaz"));
}

#[test]
fn subsequence_required_and_empty_query_matches_everything() {
    assert!(s("fb", "foo_bar").is_some());
    assert!(s("bf", "foo_bar").is_none());
    assert!(s("foox", "foo").is_none());
    assert!(s("abd", "abc").is_none());
    assert!(s("abc", "a_b_c").is_some());
    assert_eq!(s("", "anything"), Some(0));
    assert_eq!(s("  \t", "anything"), Some(0));
    assert_eq!(m("", "x").unwrap().positions, Vec::<usize>::new());
    assert_eq!(s("  ab \t", "ab"), s("ab", "ab"));
}

#[test]
fn folding_is_documented_and_exact() {
    assert!(s("ärg", "Ärger").is_some());
    assert!(s("ÄRG", "ärger").is_some());
    assert!(s("é", "café").is_some());
    assert!(s("é", "CAFÉ").is_some());
    assert!(s("σ", "ς").is_some(), "final sigma folds to sigma");
    assert!(s("ς", "Σ").is_some());
    assert!(
        s("i", "İstanbul").is_some(),
        "İ folds to i (first char of its lowercase)"
    );
    // Not folded: multi-char expansion, normalisation, accents.
    assert!(s("straße", "STRASSE").is_none());
    assert!(s("ss", "straße").is_none());
    assert!(s("e", "café").is_none());
    assert!(s("e\u{301}", "caf\u{e9}").is_none());
    assert_eq!(fold('A'), 'a');
    assert_eq!(fold('Ω'), 'ω');
    assert_eq!(fold('1'), '1');
}

#[test]
fn ascii_and_unicode_paths_agree() {
    // The same ASCII text scored on the byte path and (through a non-ASCII query char that
    // folds to ASCII: KELVIN SIGN -> k) on the char path.
    let ascii = s("kb", "kilo_byte").unwrap();
    let chars = s("\u{212a}b", "kilo_byte").unwrap();
    // The case bonus differs (the query's K is not the candidate's k), nothing else.
    assert_eq!(ascii - chars, BONUS_CASE);
}

#[test]
fn long_text_is_truncated_not_rejected_with_a_panic() {
    let text = "a".repeat(300);
    assert!(s("aa", &text).is_some());
    assert!(s("b", &text).is_none());
    let tail = format!("{}b", "a".repeat(300));
    assert!(
        s("b", &tail).is_none(),
        "characters past max_chars are ignored"
    );
    let wide = Options { max_chars: 400 };
    assert!(score("b", &tail, &wide).is_some());
    assert!(Matcher::with_options(&Options { max_chars: 0 })
        .score(&Query::new("a"), "a")
        .is_some());
}

#[test]
fn positions_are_byte_offsets_of_the_best_alignment() {
    let p = |q: &str, t: &str| m(q, t).map(|m| m.positions);
    assert_eq!(p("fb", "foo_bar"), Some(vec![0, 4]));
    assert_eq!(p("fb", "fooBar"), Some(vec![0, 3]));
    assert_eq!(p("pr", "print"), Some(vec![0, 1]));
    assert_eq!(p("fo", "xxfooxfo"), Some(vec![2, 3]));
    assert_eq!(p("ts", "toggle sidebar"), Some(vec![0, 7]));
    assert_eq!(p("", "abc"), Some(vec![]));
    assert_eq!(p("z", "abc"), None);
    assert_eq!(p("main", "src/main.rs"), Some(vec![4, 5, 6, 7]));
}

#[test]
fn positions_are_correct_for_multi_byte_text() {
    let p = |q: &str, t: &str| m(q, t).map(|m| m.positions);
    assert_eq!(p("é", "café"), Some(vec![3]));
    assert_eq!(p("hl", "héllo"), Some(vec![0, 3]));
    assert_eq!(p("日本", "今日の日本語"), Some(vec![9, 12]));
    assert_eq!(p("ab", "αβaβb"), Some(vec![4, 7]));
    let text = "ünï_cödé";
    let pos = p("ücd", text).unwrap();
    assert!(pos.iter().all(|&i| text.is_char_boundary(i)));
    assert_eq!(
        pos.iter()
            .map(|&i| text[i..].chars().next().unwrap())
            .collect::<String>(),
        "ücd"
    );
}

#[test]
fn match_score_equals_scorer_score() {
    for (q, t) in [
        ("get_user", "get_user_by_id"),
        ("hm", "HashMap"),
        ("tstr", "to_string"),
        ("ln", "Lïne_nö"),
    ] {
        assert_eq!(m(q, t).unwrap().score, s(q, t).unwrap(), "{q} {t}");
    }
}

#[test]
fn indexed_scores_equal_direct_scores() {
    let texts = ["get_count", "getCount", "GC", "né_ü", "x", ""];
    let mut index = Index::new(&Options::default());
    for t in texts {
        index.push(t);
    }
    assert_eq!(index.len(), texts.len());
    assert!(!index.is_empty());
    let mut matcher = Matcher::new();
    for q in ["gc", "g", "né", "", "zz", "ü"] {
        let query = Query::new(q);
        for (slot, t) in texts.iter().enumerate() {
            assert_eq!(
                matcher.score_indexed(&index, slot, &query),
                matcher.score(&query, t),
                "{q:?} vs {t:?}"
            );
        }
    }
}

#[test]
fn query_prefix_relation() {
    let (a, ab) = (Query::new("a"), Query::new("ab"));
    assert!(a.is_prefix_of(&ab));
    assert!(!ab.is_prefix_of(&a));
    assert!(Query::default().is_prefix_of(&a));
    assert_eq!(ab.len(), 2);
    // Several atoms: the earlier ones must stay, the last one may grow.
    let (push, push_g, push_gi, gi_push) = (
        Query::new("push"),
        Query::new("push g"),
        Query::new("push gi"),
        Query::new("gi push"),
    );
    assert!(push.is_prefix_of(&push_g));
    assert!(push_g.is_prefix_of(&push_gi));
    assert!(!push_gi.is_prefix_of(&push_g));
    assert!(!push.is_prefix_of(&gi_push));
    assert!(Query::new("pu").is_prefix_of(&push_gi));
    assert_eq!(push_gi.len(), 6);
}

#[test]
fn atoms_match_in_any_order() {
    assert!(s("push git", "Git: Push").is_some());
    assert!(s("git push", "Git: Push").is_some());
    assert!(s("fold open", "Open Folder").is_some());
    assert!(s("open fold", "Open Folder").is_some());
    // Every atom must match.
    assert!(s("push zzz", "Git: Push").is_none());
    assert!(s("zzz push", "Git: Push").is_none());
    // Each atom is matched against the whole candidate, not after the previous atom.
    assert!(s("sidebar toggle", "Workspace: Toggle Sidebar").is_some());
    // The characters of one atom stay in order.
    assert!(s("hsup git", "Git: Push").is_none());
}

#[test]
fn whitespace_between_atoms_is_ignored() {
    let reference = s("push git", "Git: Push");
    assert!(reference.is_some());
    for q in [
        "push   git",
        "  push git",
        "push git  ",
        "\tpush\n git\u{3000}",
        " push git ",
    ] {
        assert_eq!(s(q, "Git: Push"), reference, "{q:?}");
    }
    // All-whitespace is the empty query: everything, score 0, input order.
    for q in ["", " ", " \t\n\u{3000}"] {
        assert_eq!(s(q, "anything"), Some(0));
        assert!(Query::new(q).is_empty());
        let items = ["b", "a", "c"];
        let r = Matcher::new().rank(&Query::new(q), &items, usize::MAX);
        assert_eq!(r.iter().map(|x| x.index).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(r.iter().all(|x| x.score == 0 && x.positions.is_empty()));
    }
}

#[test]
fn score_is_the_sum_of_the_atom_scores_and_one_atom_is_unchanged() {
    for (a, b, t) in [
        ("push", "git", "Git: Push"),
        ("fo", "ba", "foo_bar"),
        ("ab", "ab", "ab"),
        ("é", "ü", "Ünï café"),
    ] {
        let both = s(&format!("{a} {b}"), t);
        let sum = s(a, t).zip(s(b, t)).map(|(x, y)| x + y);
        assert_eq!(both, sum, "{a} {b} {t}");
    }
    // A single atom is scored exactly as before, whatever surrounds it.
    assert_eq!(s(" fb ", "foo_bar"), s("fb", "foo_bar"));
    assert_eq!(m(" fb ", "foo_bar"), m("fb", "foo_bar"));
}

#[test]
fn repeated_atoms_count_twice() {
    let (once, twice) = (
        s("git", "Git: Push").unwrap(),
        s("git git", "Git: Push").unwrap(),
    );
    assert_eq!(twice, 2 * once);
    // The same word twice is not satisfied by a text that has only part of it.
    assert!(s("git git", "gi").is_none());
    // More specific (more atoms) wins over fewer for the same candidate, so a longer
    // query narrows rather than reshuffles.
    let items = ["Git: Push", "Git: Pull", "Git: Push Tags"];
    let r = Matcher::new().rank(&Query::new("git push"), &items, usize::MAX);
    let names: Vec<&str> = r.iter().map(|x| items[x.index]).collect();
    assert_eq!(names, ["Git: Push", "Git: Push Tags"]);
}

#[test]
fn atoms_may_overlap_on_the_same_characters() {
    // `ab` and `bc` both use the `b` of `abc`.
    assert!(s("ab bc", "abc").is_some());
    assert_eq!(m("ab bc", "abc").unwrap().positions, vec![0, 1, 2]);
    // The same atom twice matches one occurrence twice.
    assert!(s("ab ab", "ab").is_some());
    assert_eq!(m("ab ab", "ab").unwrap().positions, vec![0, 1]);
    // `a a` is satisfied by a single `a`.
    assert!(s("a a", "a").is_some());
}

#[test]
fn positions_are_the_sorted_union_of_the_atoms_positions() {
    let p = |q: &str, t: &str| m(q, t).map(|m| m.positions);
    assert_eq!(p("push git", "Git: Push"), Some(vec![0, 1, 2, 5, 6, 7, 8]));
    assert_eq!(p("git push", "Git: Push"), p("push git", "Git: Push"));
    assert_eq!(
        p("fold open", "Open Folder"),
        Some(vec![0, 1, 2, 3, 5, 6, 7, 8])
    );
    // Duplicates collapse.
    assert_eq!(p("git git", "Git: Push"), Some(vec![0, 1, 2]));
    // Multi-byte text: byte offsets, increasing.
    let text = "Ünï café";
    let found = p("café ünï", text).unwrap();
    assert!(found.windows(2).all(|w| w[0] < w[1]));
    assert!(found.iter().all(|&i| text.is_char_boundary(i)));
    assert_eq!(
        found
            .iter()
            .map(|&i| text[i..].chars().next().unwrap())
            .collect::<String>(),
        "Ünïcafé"
    );
}

#[test]
fn indexed_scores_equal_direct_scores_for_several_atoms() {
    let texts = ["Git: Push", "Open Folder", "né ü", ""];
    let mut index = Index::new(&Options::default());
    for t in texts {
        index.push(t);
    }
    let mut matcher = Matcher::new();
    for q in ["push git", "fold open", "né ü", "x y", "git"] {
        let query = Query::new(q);
        for (slot, t) in texts.iter().enumerate() {
            assert_eq!(
                matcher.score_indexed(&index, slot, &query),
                matcher.score(&query, t),
                "{q:?} vs {t:?}"
            );
        }
    }
}

#[test]
fn rank_cancellable_matches_rank_when_not_cancelled() {
    let items: Vec<String> = (0..3000).map(|i| format!("item_{i}_name")).collect();
    let q = Query::new("itm 7 nm");
    let token = CancelToken::new();
    let mut matcher = Matcher::new();
    let a = matcher.rank_cancellable(&q, &items, 50, &token).unwrap();
    assert_eq!(a, matcher.rank(&q, &items, 50));
    let b = matcher
        .rank_by_cancellable(&q, &items, |s| s.as_str(), 50, &token)
        .unwrap();
    assert_eq!(a, b);
}

#[test]
fn rank_cancellable_stops_promptly_when_cancelled() {
    use std::cell::Cell;
    // Already cancelled: nothing is scored.
    let token = CancelToken::new();
    token.cancel();
    let items = vec!["abc"; 100_000];
    let seen = Cell::new(0usize);
    let r = Matcher::new().rank_by_cancellable(
        &Query::new("a"),
        &items,
        |s| {
            seen.set(seen.get() + 1);
            s
        },
        10,
        &token,
    );
    assert_eq!(r, Err(Cancelled));
    assert_eq!(seen.get(), 0);

    // Cancelled while ranking (from inside the scorer's input): stops within one check
    // interval, not after the whole list.
    let token = CancelToken::new();
    let seen = Cell::new(0usize);
    let r = Matcher::new().rank_by_cancellable(
        &Query::new("a"),
        &items,
        |s| {
            seen.set(seen.get() + 1);
            if seen.get() == 5000 {
                token.cancel();
            }
            s
        },
        10,
        &token,
    );
    assert_eq!(r, Err(Cancelled));
    assert!(
        seen.get() > 5000 && seen.get() as u64 <= 5000 + CANCEL_CHECK_WORK / WORK_PER_CANDIDATE
    );

    // From another thread: returns long before the uncancelled run would.
    let items = std::sync::Arc::new(vec!["a_long_candidate_name_for_scoring"; 2_000_000]);
    let token = CancelToken::new();
    let (t, i) = (token.clone(), items.clone());
    let started = std::time::Instant::now();
    let worker = std::thread::spawn(move || {
        Matcher::new().rank_cancellable(&Query::new("ac nm"), &i, 10, &t)
    });
    std::thread::sleep(std::time::Duration::from_millis(5));
    token.cancel();
    assert_eq!(worker.join().unwrap(), Err(Cancelled));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn rank_limits_orders_and_gives_positions() {
    let items = ["xxfoo", "foo", "bar", "foo_bar", "fOo"];
    let mut mt = Matcher::new();
    let all = mt.rank(&Query::new("foo"), &items, usize::MAX);
    let names: Vec<&str> = all.iter().map(|r| items[r.index]).collect();
    assert_eq!(names, ["foo", "fOo", "foo_bar", "xxfoo"]);
    assert_eq!(all[0].positions, vec![0, 1, 2]);
    let top2 = mt.rank(&Query::new("foo"), &items, 2);
    assert_eq!(top2.iter().map(|r| r.index).collect::<Vec<_>>(), [1, 4]);
    assert!(mt.rank(&Query::new("foo"), &items, 0).is_empty());
    let none = mt.rank(&Query::new("qq"), &items, 5);
    assert!(none.is_empty());
    let empty = mt.rank(&Query::new(""), &items, 3);
    assert_eq!(empty.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 1, 2]);
    let by = mt.rank_by(&Query::new("b"), &[(1, "abc"), (2, "bcd")], |t| t.1, 9);
    assert_eq!(by[0].index, 1);
}

#[test]
fn long_queries_do_not_panic() {
    let long_q = "a".repeat(500);
    assert_eq!(s(&long_q, "aaa"), None);
    assert_eq!(s(&long_q, &"a".repeat(600)), None);
    assert!(m(&"ab".repeat(20), &"ab".repeat(100)).is_some());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn never_panics_on_arbitrary_unicode(q in "\\PC{0,12}", t in "\\PC{0,60}") {
        let mut mt = Matcher::new();
        let query = Query::new(&q);
        let _ = mt.score(&query, &t);
        let _ = mt.matches(&query, &t);
        let _ = mt.rank(&query, &[t.as_str(), "x"], 3);
    }

    #[test]
    fn deterministic_and_reusing_a_matcher_changes_nothing(q in "[a-zA-Z_é]{0,6}", t in "[a-zA-Z_é ./]{0,40}", u in "[a-zA-Z_é]{0,40}") {
        let query = Query::new(&q);
        let mut one = Matcher::new();
        let first = one.score(&query, &t);
        let _ = one.score(&query, &u); // dirty the scratch buffers
        let _ = one.matches(&query, &u);
        prop_assert_eq!(one.score(&query, &t), first);
        prop_assert_eq!(Matcher::new().score(&query, &t), first);
        prop_assert_eq!(m(&q, &t).map(|x| x.score), first);
    }

    #[test]
    fn positions_are_increasing_char_boundaries_and_spell_the_query(q in "[a-zA-Zé日_]{1,6}", t in "[a-zA-Zé日_ /.]{0,50}") {
        if let Some(found) = m(&q, &t) {
            prop_assert_eq!(found.positions.len(), q.chars().count());
            prop_assert!(found.positions.windows(2).all(|w| w[0] < w[1]));
            let mut folded_query = q.chars().map(fold);
            for &p in &found.positions {
                prop_assert!(t.is_char_boundary(p));
                let c = t[p..].chars().next().unwrap();
                prop_assert_eq!(Some(fold(c)), folded_query.next());
            }
        }
    }

    #[test]
    fn atoms_are_independent_order_free_and_positions_are_a_sorted_union(
        a in "[a-d]{1,3}", b in "[a-d]{1,3}", t in "[a-d_ ]{0,24}"
    ) {
        let both = m(&format!("{a} {b}"), &t);
        let swapped = m(&format!("{b} {a}"), &t);
        prop_assert_eq!(&both, &swapped);
        let (ma, mb) = (m(&a, &t), m(&b, &t));
        prop_assert_eq!(both.is_some(), ma.is_some() && mb.is_some());
        if let (Some(both), Some(ma), Some(mb)) = (both, ma, mb) {
            prop_assert_eq!(both.score, ma.score + mb.score);
            let mut union: Vec<usize> = ma.positions.iter().chain(&mb.positions).copied().collect();
            union.sort_unstable();
            union.dedup();
            prop_assert_eq!(both.positions, union);
        }
    }

    #[test]
    fn no_match_without_the_characters_in_order(q in "[a-e]{1,5}", t in "[a-e]{0,20}") {
        let in_order = {
            let mut it = t.chars();
            q.chars().all(|c| it.any(|x| x == c))
        };
        prop_assert_eq!(s(&q, &t).is_some(), in_order);
    }

    #[test]
    fn adding_a_matching_prefix_never_lowers_rank_below_a_scattered_match(q in "[a-m]{2,4}", pad in "[n-z]{1,4}") {
        // A: the query typed in full at the start; B: the same letters scattered with
        // padding. Appending text to A (not changing the prefix) keeps A above B.
        let scattered: String = q.chars().flat_map(|c| [c].into_iter().chain(pad.chars())).collect();
        let rank_of = |first: &str, second: &str| {
            let items = [first, second];
            Matcher::new().rank(&Query::new(&q), &items, 2)[0].index
        };
        for extra in ["", "_more", "Tail", "zzzzzz"] {
            let prefixed = format!("{q}{extra}");
            prop_assert_eq!(rank_of(&prefixed, &scattered), 0);
            prop_assert_eq!(rank_of(&scattered, &prefixed), 1);
        }
    }

    #[test]
    fn rank_agrees_with_score_and_is_sorted(q in "[a-c]{1,3}", ts in proptest::collection::vec("[a-c_]{0,8}", 0..12)) {
        let mut mt = Matcher::new();
        let query = Query::new(&q);
        let ranked = mt.rank(&query, &ts, usize::MAX);
        let expected = ts.iter().filter(|t| mt.score(&query, t).is_some()).count();
        prop_assert_eq!(ranked.len(), expected);
        for w in ranked.windows(2) {
            prop_assert!(w[0].score >= w[1].score);
        }
        for r in &ranked {
            prop_assert_eq!(Some(r.score), mt.score(&query, &ts[r.index]));
        }
    }
}
