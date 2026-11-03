#![allow(clippy::single_range_in_vec_init)]
use super::*;
use proptest::prelude::*;

fn d(old: &str, new: &str) -> LineDiff {
    diff_lines(old, new, &DiffOptions::default())
}

fn hunks(ld: &LineDiff) -> Vec<(Range<u32>, Range<u32>)> {
    ld.hunks
        .iter()
        .map(|h| (h.old.clone(), h.new.clone()))
        .collect()
}

fn apply(old: &str, new: &str, ld: &LineDiff) -> String {
    let o: Vec<&str> = old.split('\n').collect();
    let n: Vec<&str> = new.split('\n').collect();
    let mut out: Vec<&str> = Vec::new();
    let mut pos = 0usize;
    for h in &ld.hunks {
        out.extend(&o[pos..h.old.start as usize]);
        out.extend(&n[h.new.start as usize..h.new.end as usize]);
        pos = h.old.end as usize;
    }
    out.extend(&o[pos..]);
    out.join("\n")
}

#[test]
fn identical_and_empty() {
    assert!(d("a\nb", "a\nb").hunks.is_empty());
    let ld = d("", "");
    assert!(ld.hunks.is_empty());
    assert_eq!((ld.old_lines, ld.new_lines), (1, 1));
}

#[test]
fn line_counts_match_text_semantics() {
    let ld = d("a\n", "a\nb\n");
    assert_eq!((ld.old_lines, ld.new_lines), (2, 3));
    assert_eq!(hunks(&ld), vec![(1..1, 1..2)]);
}

#[test]
fn missing_trailing_newline() {
    let ld = d("a\nb\n", "a\nb");
    assert_eq!(hunks(&ld), vec![(2..3, 2..2)]);
    assert_eq!(apply("a\nb\n", "a\nb", &ld), "a\nb");
}

#[test]
fn crlf_exact_vs_ignore_eol() {
    let ld = d("a\r\nb\r\n", "a\nb\n");
    assert_eq!(ld.stats().added, 2);
    let o = DiffOptions {
        whitespace: Whitespace::IgnoreEol,
        ..Default::default()
    };
    assert!(diff_lines("a\r\nb\r\n", "a\nb\n", &o).hunks.is_empty());
}

#[test]
fn whitespace_modes() {
    let mk = |ws| DiffOptions {
        whitespace: ws,
        ..Default::default()
    };
    let (a, b) = ("fn  x() {\n  y\n}\n", "fn x() {\n    y   \n}\n");
    assert_eq!(diff_lines(a, b, &mk(Whitespace::Exact)).hunks.len(), 1);
    assert!(diff_lines(a, b, &mk(Whitespace::IgnoreAll))
        .hunks
        .is_empty());
    assert!(diff_lines(a, b, &mk(Whitespace::IgnoreChange))
        .hunks
        .is_empty());
    assert_eq!(diff_lines(a, b, &mk(Whitespace::IgnoreEol)).hunks.len(), 1);
    // Hunks still refer to original lines.
    let c = "a\n  b\nc\n";
    let e = "a\n b\nX\n";
    let ld = diff_lines(c, e, &mk(Whitespace::IgnoreAll));
    assert_eq!(hunks(&ld), vec![(2..3, 2..3)]);
}

#[test]
fn indent_heuristic_slides_block() {
    let old = "fn a() {\n    x();\n}\n\nfn c() {\n    z();\n}\n";
    let new = "fn a() {\n    x();\n}\n\nfn b() {\n    y();\n}\n\nfn c() {\n    z();\n}\n";
    let ld = d(old, new);
    assert_eq!(ld.hunks.len(), 1);
    // The inserted block is the whole `fn b` item plus a blank, not split mid-function.
    let h = &ld.hunks[0];
    assert_eq!(h.new.end - h.new.start, 4);
    let n: Vec<&str> = new.split('\n').collect();
    assert!(n[h.new.start as usize].starts_with("fn b") || n[h.new.start as usize].is_empty());
}

#[test]
fn myers_option_works() {
    let o = DiffOptions {
        algorithm: Algorithm::Myers,
        ..Default::default()
    };
    let ld = diff_lines("a\nb\nc", "a\nx\nc", &o);
    assert_eq!(hunks(&ld), vec![(1..2, 1..2)]);
}

#[test]
fn binary_detection() {
    assert!(is_binary(b"ab\0cd"));
    assert!(!is_binary(b"plain text"));
    let mut v = vec![b'a'; 9000];
    v[8500] = 0;
    assert!(!is_binary(&v));
}

#[test]
fn stats() {
    let ld = d("a\nb\nc\n", "a\nx\ny\nc\n");
    assert_eq!(
        ld.stats(),
        DiffStats {
            added: 2,
            removed: 1
        }
    );
}

#[test]
fn gutter() {
    let ld = d("a\nb\nc\nd\ne", "a\nINS\nb\nd\nE");
    let marks = gutter_marks(&ld);
    assert_eq!(
        marks,
        vec![
            GutterMark {
                rows: 1..2,
                kind: GutterKind::Added
            },
            GutterMark {
                rows: 3..3,
                kind: GutterKind::Deleted
            },
            GutterMark {
                rows: 4..5,
                kind: GutterKind::Modified
            },
        ]
    );
}

fn render_split(old: &str, new: &str, rows: &[SplitRow]) -> String {
    let o: Vec<&str> = old.split('\n').collect();
    let n: Vec<&str> = new.split('\n').collect();
    rows.iter()
        .map(|r| {
            let l = r.old.map_or(String::new(), |i| o[i as usize].to_string());
            let rr = r.new.map_or(String::new(), |i| n[i as usize].to_string());
            let mark = match r.kind {
                RowKind::Context => ' ',
                RowKind::Added => '+',
                RowKind::Removed => '-',
                RowKind::Changed => '~',
            };
            format!(
                "{mark} {:>3} {:<14}| {:>3} {}",
                r.old.map_or("".into(), |i| (i + 1).to_string()),
                l,
                r.new.map_or("".into(), |i| (i + 1).to_string()),
                rr
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_unified(old: &str, new: &str, rows: &[UnifiedRow]) -> String {
    let o: Vec<&str> = old.split('\n').collect();
    let n: Vec<&str> = new.split('\n').collect();
    rows.iter()
        .map(|r| match r.kind {
            RowKind::Removed => format!(
                "-{:>3}     {}",
                r.old.unwrap() + 1,
                o[r.old.unwrap() as usize]
            ),
            RowKind::Added => format!(
                "+    {:>3} {}",
                r.new.unwrap() + 1,
                n[r.new.unwrap() as usize]
            ),
            _ => format!(
                " {:>3} {:>3} {}",
                r.old.unwrap() + 1,
                r.new.unwrap() + 1,
                o[r.old.unwrap() as usize]
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const OLD: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
const NEW: &str = "one\ntwo\nTHREE\nthree-and-a-half\nfour\nfive\nsix\nseven\nnine\nten\nextra\n";

#[test]
fn snapshot_split() {
    let ld = d(OLD, NEW);
    insta::assert_snapshot!(render_split(OLD, NEW, &split_rows(&ld)));
}

#[test]
fn snapshot_unified() {
    let ld = d(OLD, NEW);
    insta::assert_snapshot!(render_unified(OLD, NEW, &unified_rows(&ld)));
}

#[test]
fn snapshot_folded() {
    let ld = d(OLD, NEW);
    let rows = split_rows(&ld);
    insta::assert_debug_snapshot!(fold_context(&rows, 1));
}

#[test]
fn fold_edge_cases() {
    let none: Vec<SplitRow> = vec![];
    assert!(fold_context(&none, 3).is_empty());
    let ld = d("a\nb\nc\nd\ne", "a\nb\nc\nd\ne");
    let rows = split_rows(&ld);
    assert_eq!(fold_context(&rows, 3), vec![Segment::Folded { rows: 0..5 }]);
    let ld = d("a\nb", "a\nB");
    let rows = unified_rows(&ld);
    assert_eq!(fold_context(&rows, 3), vec![Segment::Rows(0..3)]);
}

#[test]
fn split_pairs_changed() {
    let ld = d("a\nb\nc\nd", "a\nX\nY\nZ\nd");
    let rows = split_rows(&ld);
    let kinds: Vec<_> = rows.iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        vec![
            RowKind::Context,
            RowKind::Changed,
            RowKind::Changed,
            RowKind::Added,
            RowKind::Context
        ]
    );
}

#[test]
fn inline_basic() {
    let (o, n) = ("let total = count + 1;", "let total = counts + 12;");
    let c = refine_inline(o, n);
    let o_txt: Vec<&str> = c.old.iter().map(|r| &o[r.clone()]).collect();
    let n_txt: Vec<&str> = c.new.iter().map(|r| &n[r.clone()]).collect();
    assert!(o_txt.is_empty());
    assert_eq!(n_txt, vec!["s", "2"]);
}

#[test]
fn inline_word_replace_dissimilar_whole_word() {
    let (o, n) = ("call(alpha, beta)", "call(gamma, beta)");
    let c = refine_inline(o, n);
    assert_eq!(
        c.old.iter().map(|r| &o[r.clone()]).collect::<Vec<_>>(),
        vec!["alpha"]
    );
    assert_eq!(
        c.new.iter().map(|r| &n[r.clone()]).collect::<Vec<_>>(),
        vec!["gamma"]
    );
}

#[test]
fn inline_dissimilar_is_whole() {
    let c = refine_inline("completely different text", "nothing alike here!");
    assert_eq!(c.old, vec![0.."completely different text".len()]);
    assert_eq!(c.new, vec![0.."nothing alike here!".len()]);
}

#[test]
fn inline_unicode_and_edges() {
    let c = refine_inline("héllo wörld", "héllo wörld!");
    assert_eq!(c.old, Vec::<Range<usize>>::new());
    assert_eq!(c.new, vec!["héllo wörld".len().."héllo wörld!".len()]);
    assert_eq!(refine_inline("", "abc").new, vec![0..3]);
    assert_eq!(refine_inline("same", "same"), InlineChanges::default());
}

fn text_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just("a"),
            Just("b"),
            Just("c"),
            Just(""),
            Just("  x"),
            Just("}"),
            Just("fn f()")
        ],
        0..30,
    )
    .prop_map(|v| v.join("\n"))
}

proptest! {
    #[test]
    fn hunks_reproduce_new(old in text_strategy(), new in text_strategy(), myers in any::<bool>()) {
        let o = DiffOptions {
            algorithm: if myers { Algorithm::Myers } else { Algorithm::Histogram },
            ..Default::default()
        };
        let ld = diff_lines(&old, &new, &o);
        prop_assert_eq!(apply(&old, &new, &ld), new.clone());
        prop_assert_eq!(ld.old_lines as usize, old.split('\n').count());
        prop_assert_eq!(ld.new_lines as usize, new.split('\n').count());
    }

    #[test]
    fn rows_cover_every_line_once(old in text_strategy(), new in text_strategy()) {
        let ld = d(&old, &new);
        let split = split_rows(&ld);
        let uni = unified_rows(&ld);
        for (olds, news) in [
            (split.iter().filter_map(|r| r.old).collect::<Vec<_>>(), split.iter().filter_map(|r| r.new).collect::<Vec<_>>()),
            (uni.iter().filter_map(|r| r.old).collect::<Vec<_>>(), uni.iter().filter_map(|r| r.new).collect::<Vec<_>>()),
        ] {
            prop_assert_eq!(olds, (0..ld.old_lines).collect::<Vec<_>>());
            prop_assert_eq!(news, (0..ld.new_lines).collect::<Vec<_>>());
        }
        // fold segments tile the rows exactly
        let segs = fold_context(&split, 2);
        let mut pos = 0;
        for s in segs {
            let r = match s { Segment::Rows(r) => r, Segment::Folded { rows } => rows };
            prop_assert_eq!(r.start, pos);
            pos = r.end;
        }
        prop_assert_eq!(pos, split.len());
    }

    #[test]
    fn whitespace_modes_never_panic(old in text_strategy(), new in text_strategy()) {
        for ws in [Whitespace::IgnoreAll, Whitespace::IgnoreChange, Whitespace::IgnoreEol] {
            let ld = diff_lines(&old, &new, &DiffOptions { whitespace: ws, ..Default::default() });
            prop_assert!(ld.hunks.iter().all(|h| h.old.end <= ld.old_lines && h.new.end <= ld.new_lines));
        }
    }

    #[test]
    fn inline_ranges_valid(a in "[a-zA-Zé_ (){}+0-9]{0,40}", b in "[a-zA-Zé_ (){}+0-9]{0,40}") {
        let c = refine_inline(&a, &b);
        for (s, rs) in [(&a, &c.old), (&b, &c.new)] {
            let mut prev = 0;
            for r in rs {
                prop_assert!(r.start >= prev && r.start < r.end && r.end <= s.len());
                prop_assert!(s.is_char_boundary(r.start) && s.is_char_boundary(r.end));
                prev = r.end;
            }
        }
    }
}
