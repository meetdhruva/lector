//! Focused unit tests. Fixtures use a tiny notation: `|` is a caret, `«…»` a
//! forward selection and `‹…›` a reversed one (head at `‹`).

use std::time::Duration;

use lector_text::Rope;

use crate::*;

fn options() -> BufferOptions {
    BufferOptions {
        comment: CommentTokens {
            line: Some("//".into()),
            block: None,
        },
        ..BufferOptions::default()
    }
}

fn buf_with(s: &str, o: BufferOptions) -> Buffer {
    let mut text = String::new();
    let mut sels = Vec::new();
    let mut open: Vec<(usize, bool)> = Vec::new();
    for ch in s.chars() {
        match ch {
            '|' => sels.push(Selection::caret(0, text.len())),
            '«' => open.push((text.len(), false)),
            '‹' => open.push((text.len(), true)),
            '»' | '›' => {
                let (start, rev) = open.pop().expect("unbalanced selection marker");
                sels.push(if rev {
                    Selection {
                        id: 0,
                        anchor: text.len(),
                        head: start,
                        goal: None,
                    }
                } else {
                    Selection {
                        id: 0,
                        anchor: start,
                        head: text.len(),
                        goal: None,
                    }
                });
            }
            c => text.push(c),
        }
    }
    let mut b = Buffer::new(Rope::from_str(&text), o);
    b.set_coalesce_window(Duration::ZERO);
    if !sels.is_empty() {
        // `«»` pairs are pushed when closed; restore text order.
        sels.sort_by_key(|s| s.range().start);
        b.set_selections(sels);
    }
    b
}

fn buf(s: &str) -> Buffer {
    buf_with(s, options())
}

fn show(b: &Buffer) -> String {
    let mut text = b.text().to_string();
    let mut marks: Vec<(usize, char)> = Vec::new();
    for s in b.selections() {
        if s.is_empty() {
            marks.push((s.head, '|'));
        } else if s.reversed() {
            marks.push((s.head, '‹'));
            marks.push((s.anchor, '›'));
        } else {
            marks.push((s.anchor, '«'));
            marks.push((s.head, '»'));
        }
    }
    marks.sort_by_key(|(o, c)| (*o, matches!(c, '«' | '‹')));
    for (o, c) in marks.into_iter().rev() {
        text.insert(o, c);
    }
    text
}

/// Runs `f` and returns the resulting notation, checking invariants on the way.
fn run(start: &str, f: impl FnOnce(&mut Buffer)) -> String {
    let mut b = buf(start);
    f(&mut b);
    check(&b);
    show(&b)
}

fn run_with(start: &str, o: BufferOptions, f: impl FnOnce(&mut Buffer)) -> String {
    let mut b = buf_with(start, o);
    f(&mut b);
    check(&b);
    show(&b)
}

fn check(b: &Buffer) {
    let len = b.text().len();
    let sels = b.selections();
    assert!(!sels.is_empty());
    let mut ids = std::collections::HashSet::new();
    for w in sels.windows(2) {
        let (a, c) = (w[0].range(), w[1].range());
        assert!(a.end <= c.start, "overlap: {sels:?}");
        assert!(
            a.start < c.start || (a.start == c.start && a.end < c.end),
            "unsorted {sels:?}"
        );
        assert!(
            !(a.end == c.start && (a.is_empty() || c.is_empty())),
            "unmerged touch {sels:?}"
        );
    }
    for s in sels {
        assert!(ids.insert(s.id), "dup id");
        assert!(s.anchor <= len && s.head <= len);
        let text = b.text().to_string();
        assert!(text.is_char_boundary(s.anchor) && text.is_char_boundary(s.head));
        assert!(crate::text_util::is_grapheme_boundary(b.text(), s.anchor));
        assert!(crate::text_util::is_grapheme_boundary(b.text(), s.head));
    }
}

// ---- selection model --------------------------------------------------------

#[test]
fn set_selections_normalizes() {
    let mut b = buf("héllo wörld|");
    // inside a multi-byte char (é is 2 bytes) -> clipped back
    b.set_selections(vec![Selection::caret(0, 2)]);
    assert_eq!(b.selections()[0].head, 1);
    // overlapping and unsorted merge
    b.set_selections(vec![
        Selection {
            id: 0,
            anchor: 8,
            head: 4,
            goal: None,
        },
        Selection {
            id: 0,
            anchor: 2,
            head: 6,
            goal: None,
        },
    ]);
    assert_eq!(b.selections().len(), 1);
    assert_eq!(b.selections()[0].range(), 1..8);
    // touching non-empty selections stay separate; caret touching merges
    b.set_selections(vec![
        Selection {
            id: 0,
            anchor: 0,
            head: 3,
            goal: None,
        },
        Selection {
            id: 0,
            anchor: 3,
            head: 5,
            goal: None,
        },
        Selection::caret(0, 5),
    ]);
    assert_eq!(b.selections().len(), 2);
    // empty input falls back to a caret at 0
    b.set_selections(vec![]);
    assert_eq!(b.selections().len(), 1);
    assert_eq!(b.selections()[0].range(), 0..0);
}

#[test]
fn ids_are_unique_and_stable() {
    let mut b = buf("a|b|c|");
    let ids: Vec<u64> = b.selections().iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|i| *i != 0));
    b.insert("x");
    let after: Vec<u64> = b.selections().iter().map(|s| s.id).collect();
    assert_eq!(ids, after);
    // duplicate ids supplied are reassigned
    b.set_selections(vec![Selection::caret(7, 0), Selection::caret(7, 2)]);
    let a = b.selections();
    assert_ne!(a[0].id, a[1].id);
}

#[test]
fn clip_to_grapheme_boundaries() {
    let mut b = buf("e\u{301}x\r\ny");
    b.set_selections(vec![Selection::caret(0, 1)]);
    assert_eq!(b.selections()[0].head, 0);
    b.set_selections(vec![Selection::caret(0, 5)]); // between \r and \n
    assert_eq!(b.selections()[0].head, 4);
}

// ---- motions -------------------------------------------------------------------

#[test]
fn left_right_graphemes_and_collapse() {
    assert_eq!(
        run("a|e\u{301}b", |b| b.move_carets(Motion::Right, false)),
        "ae\u{301}|b"
    );
    assert_eq!(
        run("ae\u{301}|b", |b| b.move_carets(Motion::Left, false)),
        "a|e\u{301}b"
    );
    assert_eq!(
        run("a|\r\nb", |b| b.move_carets(Motion::Right, false)),
        "a\r\n|b"
    );
    assert_eq!(
        run("a\r\n|b", |b| b.move_carets(Motion::Left, false)),
        "a|\r\nb"
    );
    assert_eq!(
        run("|👨‍👩‍👧x", |b| b
            .move_carets(Motion::Right, false)),
        "👨‍👩‍👧|x"
    );
    // collapse to the selection edge instead of moving
    assert_eq!(
        run("a«bcd»e", |b| b.move_carets(Motion::Left, false)),
        "a|bcde"
    );
    assert_eq!(
        run("a«bcd»e", |b| b.move_carets(Motion::Right, false)),
        "abcd|e"
    );
    // extend
    assert_eq!(run("a|bc", |b| b.move_carets(Motion::Right, true)), "a«b»c");
    assert_eq!(run("a|bc", |b| b.move_carets(Motion::Left, true)), "‹a›bc");
    assert_eq!(run("|", |b| b.move_carets(Motion::Left, false)), "|");
    assert_eq!(run("ab|", |b| b.move_carets(Motion::Right, false)), "ab|");
}

#[test]
fn carets_merge_when_they_collide() {
    assert_eq!(run("|a|b", |b| b.move_carets(Motion::Right, false)), "a|b|");
    let mut b = buf("a|b|");
    b.move_carets(Motion::Left, false);
    assert_eq!(show(&b), "|a|b");
    b.move_carets(Motion::Left, false);
    assert_eq!(show(&b), "|ab");
}

#[test]
fn up_down_keep_goal_column() {
    let mut b = buf("abcd\nx\nabcd");
    b.set_cursor(3);
    b.move_carets(Motion::Down, false);
    assert_eq!(show(&b), "abcd\nx|\nabcd");
    b.move_carets(Motion::Down, false);
    assert_eq!(show(&b), "abcd\nx\nabc|d");
    b.move_carets(Motion::Down, false);
    assert_eq!(show(&b), "abcd\nx\nabcd|", "last line: end of text");
    b.move_carets(Motion::Up, false);
    b.move_carets(Motion::Up, false);
    assert_eq!(show(&b), "abc|d\nx\nabcd");
    b.move_carets(Motion::Up, false);
    assert_eq!(show(&b), "|abcd\nx\nabcd", "first line: start of text");
    // goal column counts chars, not bytes
    let mut b = buf("éé|é\nabcdef");
    b.move_carets(Motion::Down, false);
    assert_eq!(show(&b), "ééé\nab|cdef");
}

#[test]
fn up_down_extend_and_multi() {
    assert_eq!(
        run("ab|\ncd", |b| b.move_carets(Motion::Down, true)),
        "ab«\ncd»"
    );
    assert_eq!(
        run("a|b\nc|d\nef", |b| b.move_carets(Motion::Down, false)),
        "ab\nc|d\ne|f"
    );
    assert_eq!(
        run("a|\nb|\nc", |b| b.move_carets(Motion::Up, false)),
        "|a|\nb\nc",
        "row 0 jumps to the start"
    );
}

#[test]
fn page_motions() {
    let text = (0..10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut b = buf(&text);
    b.set_cursor(3);
    b.move_carets(Motion::PageDown(4), false);
    assert_eq!(
        b.text().offset_to_point(b.selections()[0].head),
        Point::new(4, 3)
    );
    b.move_carets(Motion::PageDown(100), false);
    assert_eq!(
        b.text().offset_to_point(b.selections()[0].head),
        Point::new(9, 3)
    );
    b.move_carets(Motion::PageUp(2), false);
    assert_eq!(
        b.text().offset_to_point(b.selections()[0].head),
        Point::new(7, 3)
    );
    b.move_carets(Motion::PageUp(100), false);
    assert_eq!(
        b.text().offset_to_point(b.selections()[0].head),
        Point::new(0, 3)
    );
    b.move_carets(Motion::PageUp(100), false);
    assert_eq!(b.selections()[0].head, 0);
}

#[test]
fn word_motions() {
    assert_eq!(
        run("|foo bar", |b| b.move_carets(Motion::WordRight, false)),
        "foo| bar"
    );
    assert_eq!(
        run("foo| bar", |b| b.move_carets(Motion::WordRight, false)),
        "foo bar|"
    );
    assert_eq!(
        run("foo bar|", |b| b.move_carets(Motion::WordLeft, false)),
        "foo |bar"
    );
    assert_eq!(
        run("foo |bar", |b| b.move_carets(Motion::WordLeft, false)),
        "|foo bar"
    );
    assert_eq!(
        run("a.b|", |b| b.move_carets(Motion::WordLeft, false)),
        "a.|b"
    );
    assert_eq!(
        run("foo|\nbar", |b| b.move_carets(Motion::WordRight, false)),
        "foo\n|bar"
    );
    assert_eq!(
        run("foo\n|bar", |b| b.move_carets(Motion::WordLeft, false)),
        "foo|\nbar"
    );
    assert_eq!(
        run("|foo", |b| b.move_carets(Motion::WordRight, true)),
        "«foo»"
    );
    assert_eq!(
        run("日本|語", |b| b.move_carets(Motion::WordLeft, false)),
        "日|本語"
    );
}

#[test]
fn smart_home_and_end() {
    assert_eq!(
        run("    fo|o", |b| b.move_carets(Motion::LineStart, false)),
        "    |foo"
    );
    assert_eq!(
        run("    |foo", |b| b.move_carets(Motion::LineStart, false)),
        "|    foo"
    );
    assert_eq!(
        run("|    foo", |b| b.move_carets(Motion::LineStart, false)),
        "    |foo"
    );
    assert_eq!(
        run("  |  foo", |b| b.move_carets(Motion::LineStart, false)),
        "    |foo",
        "inside the indent: first non-blank"
    );
    assert_eq!(
        run("a\nf|oo\r\nb", |b| b.move_carets(Motion::LineEnd, false)),
        "a\nfoo|\r\nb"
    );
    assert_eq!(
        run("fo|o", |b| b.move_carets(Motion::LineStart, true)),
        "‹fo›o"
    );
    assert_eq!(
        run("a|b\nc", |b| b.move_carets(Motion::BufferEnd, false)),
        "ab\nc|"
    );
    assert_eq!(
        run("a\nb|c", |b| b.move_carets(Motion::BufferStart, true)),
        "‹a\nb›c"
    );
    // whitespace-only line toggles between the end of the blanks and column 0
    assert_eq!(
        run("    |", |b| b.move_carets(Motion::LineStart, false)),
        "|    "
    );
    assert_eq!(
        run("|    ", |b| b.move_carets(Motion::LineStart, false)),
        "    |"
    );
}

// ---- select commands -----------------------------------------------------------------

#[test]
fn select_all_and_line() {
    assert_eq!(run("a|b\nc", |b| b.select_all()), "«ab\nc»");
    let mut b = buf("a|\nb\nc");
    b.select_line();
    assert_eq!(show(&b), "«a\n»b\nc");
    b.select_line();
    assert_eq!(show(&b), "«a\nb\n»c");
    b.select_line();
    assert_eq!(show(&b), "«a\nb\nc»");
    b.select_line();
    assert_eq!(show(&b), "«a\nb\nc»");
    // last line without terminator
    assert_eq!(run("a\nb|", |b| b.select_line()), "a\n«b»");
    // multi-line selection selects every touched line; trailing column 0 excluded
    assert_eq!(run("a«b\nc»d\ne", |b| b.select_line()), "«ab\ncd\n»e");
    assert_eq!(run("a«b\n»c", |b| b.select_line()), "«ab\n»c");
    // multi caret
    assert_eq!(run("a|\nb\nc|", |b| b.select_line()), "«a\n»b\n«c»");
    // CRLF
    assert_eq!(run("a|\r\nb", |b| b.select_line()), "«a\r\n»b");
    assert_eq!(run("|", |b| b.select_line()), "|");
}

#[test]
fn select_word_at_offset() {
    assert_eq!(run("foo bar|", |b| b.select_word_at(5)), "foo «bar»");
    assert_eq!(run("foo bar|", |b| b.select_word_at(3)), "«foo» bar");
    assert_eq!(run("foo  bar|", |b| b.select_word_at(4)), "foo«  »bar");
    assert_eq!(run("a|\n\nb", |b| b.select_word_at(2)), "a\n|\nb");
    assert_eq!(
        run("a|b|", |b| b.select_word_at(0)),
        "«ab»",
        "replaces all selections"
    );
}

#[test]
fn select_next_occurrence_cycles() {
    let mut b = buf("foo foo fo|o bar");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "foo foo «foo» bar");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "«foo» foo «foo» bar", "wraps around");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "«foo» «foo» «foo» bar");
    assert!(!b.select_next_occurrence(), "all selected");
    assert_eq!(show(&b), "«foo» «foo» «foo» bar");
    check(&b);
}

#[test]
fn select_next_occurrence_is_case_sensitive_and_handles_empties() {
    let mut b = buf("Foo foo F|oo");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "Foo foo «Foo»", "first press selects the word");
    assert!(b.select_next_occurrence());
    assert_eq!(
        show(&b),
        "«Foo» foo «Foo»",
        "`foo` is not an occurrence of `Foo`"
    );
    assert!(!b.select_next_occurrence());
    assert!(
        !buf("|  ").select_next_occurrence(),
        "no word under the caret"
    );
    let mut e = buf("|");
    assert!(!e.select_next_occurrence());
    // several empty carets: each selects its word
    let mut m = buf("ab| cd|");
    assert!(m.select_next_occurrence());
    assert_eq!(show(&m), "«ab» «cd»");
}

#[test]
fn select_all_occurrences_works() {
    assert_eq!(
        run("a b |a b a", |b| b.select_all_occurrences()),
        "«a» b «a» b «a»"
    );
    assert_eq!(
        run("«ab» abab", |b| b.select_all_occurrences()),
        "«ab» «ab»«ab»"
    );
    assert_eq!(
        run("|  ", |b| b.select_all_occurrences()),
        "|  ",
        "no word under the caret"
    );
    assert_eq!(run("a.|a", |b| b.select_all_occurrences()), "«a».«a»");
}

#[test]
fn caret_adding() {
    let mut b = buf("abcd\nef\nabc|d");
    b.add_caret_above();
    assert_eq!(show(&b), "abcd\nef|\nabc|d");
    b.add_caret_above();
    assert_eq!(show(&b), "abc|d\nef|\nabc|d");
    b.add_caret_above();
    assert_eq!(show(&b), "abc|d\nef|\nabc|d", "no row above");
    b.add_caret_below();
    check(&b);
    // add_caret at same spot merges
    let mut b = buf("ab|c");
    b.add_caret(2);
    assert_eq!(b.selections().len(), 1);
    b.add_caret(0);
    assert_eq!(show(&b), "|ab|c");
    assert_eq!(b.newest_selection().head, 0);
}

#[test]
fn add_caret_below_goal() {
    let mut b = buf("abcd\nx\nabcd");
    b.set_cursor(3);
    b.add_caret_below();
    b.add_caret_below();
    assert_eq!(show(&b), "abc|d\nx|\nabc|d");
    b.add_caret_below();
    assert_eq!(show(&b), "abc|d\nx|\nabc|d");
}

#[test]
fn split_into_lines_and_collapse() {
    assert_eq!(
        run("«a\nbb\nc»", |b| b.split_selection_into_lines()),
        "a|\nbb|\nc|"
    );
    assert_eq!(
        run("«a\nbb\n»c", |b| b.split_selection_into_lines()),
        "a|\nbb|\nc"
    );
    assert_eq!(run("a«b»c", |b| b.split_selection_into_lines()), "a«b»c");
    assert_eq!(run("a|b", |b| b.split_selection_into_lines()), "a|b");
    let mut b = buf("a|b|c");
    b.add_caret(0);
    b.collapse_selections();
    assert_eq!(show(&b), "|abc");
    let mut b = buf("a«bc»d");
    b.collapse_selections();
    assert_eq!(show(&b), "abc|d");
    b.collapse_selections();
    assert_eq!(show(&b), "abc|d");
}

// ---- typing -----------------------------------------------------------------------

#[test]
fn insert_basic_and_multi() {
    assert_eq!(
        run("ab|c", |b| {
            b.insert("X");
        }),
        "abX|c"
    );
    assert_eq!(
        run("a|b|c", |b| {
            b.insert("X");
        }),
        "aX|bX|c"
    );
    assert_eq!(
        run("«ab»c", |b| {
            b.insert("X");
        }),
        "X|c"
    );
    assert_eq!(
        run("|", |b| {
            b.insert("héllo 😀");
        }),
        "héllo 😀|"
    );
    assert_eq!(
        run("a|", |b| {
            assert!(b.insert("").is_empty());
        }),
        "a|"
    );
    assert_eq!(
        run("a|", |b| {
            b.insert("\n");
        }),
        "a\n|"
    );
}

#[test]
fn auto_close_and_type_over() {
    assert_eq!(
        run("|", |b| {
            b.insert("(");
        }),
        "(|)"
    );
    assert_eq!(
        run("x = |", |b| {
            b.insert("[");
        }),
        "x = [|]"
    );
    assert_eq!(
        run("|", |b| {
            b.insert("{");
        }),
        "{|}"
    );
    assert_eq!(
        run("|a", |b| {
            b.insert("(");
        }),
        "(|a",
        "not before a word"
    );
    assert_eq!(
        run("|)", |b| {
            b.insert("(");
        }),
        "(|))"
    );
    assert_eq!(
        run("(|)", |b| {
            b.insert(")");
        }),
        "()|"
    );
    assert_eq!(
        run("{[|]}", |b| {
            b.insert("]");
            b.insert("}");
        }),
        "{[]}|"
    );
    assert_eq!(
        run("|", |b| {
            b.insert("(");
            b.insert("a");
            b.insert(")");
        }),
        "(a)|"
    );
    assert_eq!(
        run("|)", |b| {
            b.insert(")");
        }),
        ")|"
    );
    // only the caret before a blank / end of text gets a closer
    assert_eq!(
        run("a|b|", |b| {
            b.insert("(");
        }),
        "a(|b(|)"
    );
    let mut o = options();
    o.auto_close_brackets = false;
    assert_eq!(
        run_with("|", o, |b| {
            b.insert("(");
        }),
        "(|"
    );
}

#[test]
fn surround_selection() {
    assert_eq!(
        run("«ab»", |b| {
            b.insert("(");
        }),
        "(«ab»)"
    );
    assert_eq!(
        run("«ab»", |b| {
            b.insert("\"");
        }),
        "\"«ab»\""
    );
    assert_eq!(
        run("‹ab›", |b| {
            b.insert("[");
        }),
        "[‹ab›]"
    );
    assert_eq!(
        run("«ab»", |b| {
            b.insert("x");
        }),
        "x|"
    );
    assert_eq!(
        run("«a»b«c»", |b| {
            b.insert("{");
        }),
        "{«a»}b{«c»}"
    );
}

#[test]
fn auto_close_quotes() {
    assert_eq!(
        run("|", |b| {
            b.insert("\"");
        }),
        "\"|\""
    );
    assert_eq!(
        run("ab|", |b| {
            b.insert("\"");
        }),
        "ab\"|",
        "not inside words"
    );
    assert_eq!(
        run("don|", |b| {
            b.insert("'");
        }),
        "don'|"
    );
    assert_eq!(
        run("\"|\"", |b| {
            b.insert("\"");
        }),
        "\"\"|",
        "type over"
    );
    assert_eq!(
        run("say \"hi|", |b| {
            b.insert("\"");
        }),
        "say \"hi\"|",
        "closing an open string"
    );
    assert_eq!(
        run("x = |", |b| {
            b.insert("'");
        }),
        "x = '|'"
    );
    assert_eq!(
        run("a \\|", |b| {
            b.insert("\"");
        }),
        "a \\\"|",
        "after a backslash"
    );
    assert_eq!(
        run("`|", |b| {
            b.insert("`");
        }),
        "``|",
        "after a backtick: plain"
    );
}

#[test]
fn newline_auto_indent() {
    assert_eq!(
        run("    foo|", |b| {
            b.newline();
        }),
        "    foo\n    |"
    );
    assert_eq!(
        run("fn f() {|", |b| {
            b.newline();
        }),
        "fn f() {\n    |"
    );
    assert_eq!(
        run("fn f() {|}", |b| {
            b.newline();
        }),
        "fn f() {\n    |\n}"
    );
    assert_eq!(
        run("    if x {|}", |b| {
            b.newline();
        }),
        "    if x {\n        |\n    }"
    );
    assert_eq!(
        run("f(|)", |b| {
            b.newline();
        }),
        "f(\n    |\n)"
    );
    assert_eq!(
        run("[|]", |b| {
            b.newline();
        }),
        "[\n    |\n]"
    );
    assert_eq!(
        run("key:|", |b| {
            b.newline();
        }),
        "key:\n    |"
    );
    assert_eq!(
        run("a(|", |b| {
            b.newline();
        }),
        "a(\n    |"
    );
    assert_eq!(
        run("a[ |", |b| {
            b.newline();
        }),
        "a[ \n    |",
        "opener followed by blanks"
    );
    assert_eq!(
        run("a\r\n  b|", |b| {
            b.newline();
        }),
        "a\r\n  b\r\n  |"
    );
    assert_eq!(
        run("a«bc»d", |b| {
            b.newline();
        }),
        "a\n|d"
    );
    assert_eq!(
        run("    |foo", |b| {
            b.newline();
        }),
        "    \n    |foo"
    );
    assert_eq!(
        run("  foo|", |b| {
            b.newline();
        }),
        "  foo\n  |"
    );
    assert_eq!(
        run("|", |b| {
            b.newline();
        }),
        "\n|"
    );
    assert_eq!(
        run("a|\nb|", |b| {
            b.newline();
        }),
        "a\n|\nb\n|"
    );
    let mut o = options();
    o.auto_indent = false;
    assert_eq!(
        run_with("  foo {|", o, |b| {
            b.newline();
        }),
        "  foo {\n|"
    );
}

#[test]
fn newline_with_tabs_style() {
    let mut o = options();
    o.indent = IndentStyle::Tabs;
    assert_eq!(
        run_with("\tfoo|", o.clone(), |b| {
            b.newline();
        }),
        "\tfoo\n\t|"
    );
    assert_eq!(
        run_with("\t{|", o.clone(), |b| {
            b.newline();
        }),
        "\t{\n\t\t|"
    );
    assert_eq!(
        run_with("\t{|}", o, |b| {
            b.newline();
        }),
        "\t{\n\t\t|\n\t}"
    );
}

#[test]
fn newline_above_below() {
    assert_eq!(
        run("  a|\nb", |b| {
            b.newline_below();
        }),
        "  a\n  |\nb"
    );
    assert_eq!(
        run("  a|\nb", |b| {
            b.newline_above();
        }),
        "  |\n  a\nb"
    );
    assert_eq!(
        run("a|", |b| {
            b.newline_below();
        }),
        "a\n|"
    );
    assert_eq!(
        run("f() {|", |b| {
            b.newline_below();
        }),
        "f() {\n    |"
    );
    assert_eq!(
        run("a|b|", |b| {
            b.newline_below();
        }),
        "ab\n|"
    );
    assert_eq!(
        run("a|\r\nb|", |b| {
            b.newline_below();
        }),
        "a\r\n|\r\nb\r\n|"
    );
    assert_eq!(
        run("ab«c\nd»e", |b| {
            b.newline_above();
        }),
        "abc\n|\nde",
        "uses the head row only"
    );
}

// ---- deletion ----------------------------------------------------------------------

#[test]
fn backspace_basics() {
    assert_eq!(
        run("ab|c", |b| {
            b.backspace();
        }),
        "a|c"
    );
    assert_eq!(
        run("|abc", |b| {
            b.backspace();
        }),
        "|abc"
    );
    assert_eq!(
        run("a«bc»d", |b| {
            b.backspace();
        }),
        "a|d"
    );
    assert_eq!(
        run("e\u{301}|x", |b| {
            b.backspace();
        }),
        "|x",
        "whole grapheme cluster"
    );
    assert_eq!(
        run("👨‍👩‍👧|", |b| {
            b.backspace();
        }),
        "|"
    );
    assert_eq!(
        run("a\r\n|b", |b| {
            b.backspace();
        }),
        "a|b"
    );
    assert_eq!(
        run("a|b|c", |b| {
            b.backspace();
        }),
        "|c"
    );
    assert_eq!(
        run("|a|bc", |b| {
            b.backspace();
        }),
        "|bc"
    );
    assert_eq!(
        run("ab|\nc|", |b| {
            b.backspace();
        }),
        "a|\n|",
        "carets at line ends"
    );
}

#[test]
fn backspace_indent_units() {
    assert_eq!(
        run("        |x", |b| {
            b.backspace();
        }),
        "    |x"
    );
    assert_eq!(
        run("      |x", |b| {
            b.backspace();
        }),
        "    |x"
    );
    assert_eq!(
        run("  |x", |b| {
            b.backspace();
        }),
        "|x"
    );
    assert_eq!(
        run("a  |b", |b| {
            b.backspace();
        }),
        "a |b"
    );
    assert_eq!(
        run("    |", |b| {
            b.backspace();
        }),
        "|"
    );
    assert_eq!(
        run("\n    |", |b| {
            b.backspace();
        }),
        "\n|"
    );
    assert_eq!(
        run("\t    |x", |b| {
            b.backspace();
        }),
        "\t|x"
    );
    let mut o = options();
    o.indent = IndentStyle::Tabs;
    assert_eq!(
        run_with("\t\t|x", o.clone(), |b| {
            b.backspace();
        }),
        "\t|x"
    );
    assert_eq!(
        run_with("    |x", o, |b| {
            b.backspace();
        }),
        "   |x",
        "tabs style: one char"
    );
}

#[test]
fn backspace_pairs() {
    assert_eq!(
        run("(|)", |b| {
            b.backspace();
        }),
        "|"
    );
    assert_eq!(
        run("[|]", |b| {
            b.backspace();
        }),
        "|"
    );
    assert_eq!(
        run("\"|\"", |b| {
            b.backspace();
        }),
        "|"
    );
    assert_eq!(
        run("(|x)", |b| {
            b.backspace();
        }),
        "|x)"
    );
    assert_eq!(
        run("(|]", |b| {
            b.backspace();
        }),
        "|]"
    );
    let mut o = options();
    o.auto_close_brackets = false;
    assert_eq!(
        run_with("(|)", o, |b| {
            b.backspace();
        }),
        "|)"
    );
}

#[test]
fn delete_forward() {
    assert_eq!(
        run("a|bc", |b| {
            b.delete();
        }),
        "a|c"
    );
    assert_eq!(
        run("abc|", |b| {
            b.delete();
        }),
        "abc|"
    );
    assert_eq!(
        run("a|\r\nb", |b| {
            b.delete();
        }),
        "a|b"
    );
    assert_eq!(
        run("|👨‍👩‍👧x", |b| {
            b.delete();
        }),
        "|x"
    );
    assert_eq!(
        run("a«b»c", |b| {
            b.delete();
        }),
        "a|c"
    );
    assert_eq!(
        run("|a|b", |b| {
            b.delete();
        }),
        "|".to_string(),
        "adjacent carets merge into one range"
    );
    assert_eq!(
        run("|e\u{301}x", |b| {
            b.delete();
        }),
        "|x"
    );
}

#[test]
fn delete_words() {
    assert_eq!(
        run("foo bar|", |b| {
            b.delete_word_left();
        }),
        "foo |"
    );
    assert_eq!(
        run("foo bar  |", |b| {
            b.delete_word_left();
        }),
        "foo |"
    );
    assert_eq!(
        run("foo.bar|", |b| {
            b.delete_word_left();
        }),
        "foo.|"
    );
    assert_eq!(
        run("foo.|", |b| {
            b.delete_word_left();
        }),
        "foo|"
    );
    assert_eq!(
        run("foo\n|bar", |b| {
            b.delete_word_left();
        }),
        "foo|bar"
    );
    assert_eq!(
        run("|", |b| {
            b.delete_word_left();
        }),
        "|"
    );
    assert_eq!(
        run("a«b»c", |b| {
            b.delete_word_left();
        }),
        "a|c"
    );
    assert_eq!(
        run("|foo bar", |b| {
            b.delete_word_right();
        }),
        "| bar"
    );
    assert_eq!(
        run("foo| bar", |b| {
            b.delete_word_right();
        }),
        "foo|"
    );
    assert_eq!(
        run("foo|\nbar", |b| {
            b.delete_word_right();
        }),
        "foo|bar"
    );
    assert_eq!(
        run("abc|", |b| {
            b.delete_word_right();
        }),
        "abc|"
    );
    assert_eq!(
        run("fo|o ba|r", |b| {
            b.delete_word_left();
        }),
        "|o |r"
    );
    // overlapping ranges of neighbouring carets merge
    assert_eq!(
        run("foo|ba|r", |b| {
            b.delete_word_left();
        }),
        "|r"
    );
    assert_eq!(
        run("日本|語", |b| {
            b.delete_word_left();
        }),
        "日|語"
    );
}

#[test]
fn delete_to_line_start_works() {
    assert_eq!(
        run("abc|def", |b| {
            b.delete_to_line_start();
        }),
        "|def"
    );
    assert_eq!(
        run("ab\n|cd", |b| {
            b.delete_to_line_start();
        }),
        "ab|cd"
    );
    assert_eq!(
        run("|ab", |b| {
            b.delete_to_line_start();
        }),
        "|ab"
    );
    assert_eq!(
        run("a\nbc|d\ne|f", |b| {
            b.delete_to_line_start();
        }),
        "a\n|d\n|f"
    );
}

#[test]
fn delete_lines() {
    assert_eq!(
        run("a\nb|\nc", |b| {
            b.delete_line();
        }),
        "a\nc|",
        "column 1 on the next line"
    );
    assert_eq!(
        run("a\n|b\nc", |b| {
            b.delete_line();
        }),
        "a\n|c"
    );
    assert_eq!(
        run("a\nb|", |b| {
            b.delete_line();
        }),
        "a|"
    );
    assert_eq!(
        run("a|\nb", |b| {
            b.delete_line();
        }),
        "b|"
    );
    assert_eq!(
        run("a|", |b| {
            b.delete_line();
        }),
        "|"
    );
    assert_eq!(
        run("«a\nb»\nc", |b| {
            b.delete_line();
        }),
        "c|"
    );
    assert_eq!(
        run("«a\n»b", |b| {
            b.delete_line();
        }),
        "|b"
    );
    assert_eq!(
        run("abc\nde|f\nxyzw", |b| {
            b.delete_line();
        }),
        "abc\nxy|zw"
    );
    assert_eq!(
        run("a|\r\nb\r\nc|", |b| {
            b.delete_line();
        }),
        "b|",
        "carets on first and last line"
    );
    assert_eq!(
        run("a|\nb|\nc", |b| {
            b.delete_line();
        }),
        "c|"
    );
    assert_eq!(
        run("a\n|\n", |b| {
            b.delete_line();
        }),
        "a\n|"
    );
}

// ---- indentation --------------------------------------------------------------------

#[test]
fn tab_inserts_to_next_stop() {
    assert_eq!(
        run("|a", |b| {
            b.tab();
        }),
        "    |a"
    );
    assert_eq!(
        run("ab|", |b| {
            b.tab();
        }),
        "ab  |"
    );
    assert_eq!(
        run("abcd|", |b| {
            b.tab();
        }),
        "abcd    |"
    );
    assert_eq!(
        run("\tx|", |b| {
            b.tab();
        }),
        "\tx   |"
    );
    let mut o = options();
    o.indent = IndentStyle::Tabs;
    assert_eq!(
        run_with("ab|", o, |b| {
            b.tab();
        }),
        "ab\t|"
    );
    let mut o = options();
    o.indent = IndentStyle::Spaces(2);
    o.tab_size = 2;
    assert_eq!(
        run_with("a|", o, |b| {
            b.tab();
        }),
        "a |"
    );
    assert_eq!(
        run("a|b|", |b| {
            b.tab();
        }),
        "a   |b  |"
    );
}

#[test]
fn tab_with_selection_indents() {
    assert_eq!(
        run("«a\nb»", |b| {
            b.tab();
        }),
        "«    a\n    b»"
    );
    assert_eq!(
        run("a«b»c", |b| {
            b.tab();
        }),
        "    a«b»c"
    );
}

#[test]
fn indent_outdent() {
    assert_eq!(
        run("«a\n\nb»", |b| {
            b.indent();
        }),
        "«    a\n\n    b»"
    );
    assert_eq!(
        run("a|", |b| {
            b.indent();
        }),
        "    a|"
    );
    assert_eq!(
        run("|", |b| {
            b.indent();
        }),
        "    |"
    );
    assert_eq!(
        run("  a|", |b| {
            b.indent();
        }),
        "    a|",
        "aligns to the next level"
    );
    assert_eq!(
        run("a\nb|\nc", |b| {
            b.indent();
        }),
        "a\n    b|\nc"
    );
    assert_eq!(
        run("a«b\nc»d", |b| {
            b.indent();
        }),
        "    a«b\n    c»d"
    );
    assert_eq!(
        run("«a\n»b", |b| {
            b.indent();
        }),
        "«    a\n»b",
        "last line at column 0 untouched"
    );
    assert_eq!(
        run("    a|", |b| {
            b.outdent();
        }),
        "a|"
    );
    assert_eq!(
        run("  a|", |b| {
            b.outdent();
        }),
        "a|"
    );
    assert_eq!(
        run("\ta|", |b| {
            b.outdent();
        }),
        "a|"
    );
    assert_eq!(
        run("      a|", |b| {
            b.outdent();
        }),
        "    a|"
    );
    assert_eq!(
        run("a|", |b| {
            b.outdent();
        }),
        "a|"
    );
    assert_eq!(
        run("«    a\n  b\nc»", |b| {
            b.outdent();
        }),
        "«a\nb\nc»"
    );
    assert_eq!(
        run("        a|", |b| {
            b.outdent();
        }),
        "    a|"
    );
    let mut o = options();
    o.indent = IndentStyle::Tabs;
    assert_eq!(
        run_with("a|", o.clone(), |b| {
            b.indent();
        }),
        "\ta|"
    );
    assert_eq!(
        run_with("    a|", o, |b| {
            b.outdent();
        }),
        "a|"
    );
}

#[test]
fn indent_keeps_selection_shape() {
    let mut b = buf("x\n«a\nb»\ny");
    b.indent();
    assert_eq!(show(&b), "x\n«    a\n    b»\ny");
    b.outdent();
    assert_eq!(show(&b), "x\n«a\nb»\ny");
    let mut b = buf("a\n  ‹b\nc›");
    b.indent();
    assert_eq!(show(&b), "a\n    ‹b\n    c›");
}

// ---- comments ------------------------------------------------------------------------

#[test]
fn line_comments() {
    assert_eq!(
        run("a|", |b| {
            b.toggle_comment();
        }),
        "// a|"
    );
    assert_eq!(
        run("// a|", |b| {
            b.toggle_comment();
        }),
        "a|"
    );
    assert_eq!(
        run("//a|", |b| {
            b.toggle_comment();
        }),
        "a|"
    );
    assert_eq!(
        run("«    a\n      b»", |b| {
            b.toggle_comment();
        }),
        "«    // a\n    //   b»"
    );
    assert_eq!(
        run("«// a\nb»", |b| {
            b.toggle_comment();
        }),
        "«// // a\n// b»"
    );
    assert_eq!(
        run("«// a\n//b»", |b| {
            b.toggle_comment();
        }),
        "«a\nb»"
    );
    assert_eq!(
        run("«a\n\nb»", |b| {
            b.toggle_comment();
        }),
        "«// a\n\n// b»"
    );
    assert_eq!(
        run("«// a\n\n// b»", |b| {
            b.toggle_comment();
        }),
        "«a\n\nb»"
    );
    assert_eq!(
        run("|", |b| {
            b.toggle_comment();
        }),
        "// |"
    );
    assert_eq!(
        run("    |", |b| {
            b.toggle_comment();
        }),
        "    // |"
    );
    assert_eq!(
        run("  a|\n    b|", |b| {
            b.toggle_comment();
        }),
        "  // a|\n  //   b|"
    );
    assert_eq!(
        run("a\r\n|b", |b| {
            b.toggle_comment();
        }),
        "a\r\n// |b"
    );
    let mut b = buf("a|\nb");
    b.toggle_comment();
    b.toggle_comment();
    assert_eq!(show(&b), "a|\nb");
}

#[test]
fn block_comments() {
    let mut o = options();
    o.comment = CommentTokens {
        line: None,
        block: Some(("/*".into(), "*/".into())),
    };
    assert_eq!(
        run_with("a b|", o.clone(), |b| {
            b.toggle_comment();
        }),
        "/* a b */|"
    );
    assert_eq!(
        run_with("/* a b */|", o.clone(), |b| {
            b.toggle_comment();
        }),
        "a b|"
    );
    assert_eq!(
        run_with("/*a*/|", o.clone(), |b| {
            b.toggle_comment();
        }),
        "a|"
    );
    assert_eq!(
        run_with("«  a\nb  »", o.clone(), |b| {
            b.toggle_comment();
        }),
        "«  /* a\nb */  »"
    );
    assert_eq!(
        run_with("|\n", o, |b| {
            b.toggle_comment();
        }),
        "|\n"
    );
}

#[test]
fn comments_without_tokens_do_nothing() {
    let mut o = options();
    o.comment = CommentTokens::default();
    assert_eq!(
        run_with("a|", o, |b| {
            assert!(b.toggle_comment().is_empty());
        }),
        "a|"
    );
}

// ---- lines ----------------------------------------------------------------------------

#[test]
fn duplicate_lines() {
    assert_eq!(
        run("a|\nb", |b| {
            b.duplicate_line();
        }),
        "a\na|\nb"
    );
    assert_eq!(
        run("a|", |b| {
            b.duplicate_line();
        }),
        "a\na|"
    );
    assert_eq!(
        run("«a\nb»\nc", |b| {
            b.duplicate_line();
        }),
        "a\nb\n«a\nb»\nc"
    );
    assert_eq!(
        run("a|\nb|", |b| {
            b.duplicate_line();
        }),
        "a\na|\nb\nb|"
    );
    assert_eq!(
        run("a|b|\nc", |b| {
            b.duplicate_line();
        }),
        "ab\na|b|\nc"
    );
    assert_eq!(
        run("a|\r\nb", |b| {
            b.duplicate_line();
        }),
        "a\r\na|\r\nb"
    );
    assert_eq!(
        run("a\r\nb|", |b| {
            b.duplicate_line();
        }),
        "a\r\nb\r\nb|"
    );
    assert_eq!(
        run("«a\n»b", |b| {
            b.duplicate_line();
        }),
        "a\n«a\n»b"
    );
}

#[test]
fn move_lines_up_down() {
    assert_eq!(
        run("a\nb|\nc", |b| {
            b.move_lines(true);
        }),
        "b|\na\nc"
    );
    assert_eq!(
        run("a\nb|\nc", |b| {
            b.move_lines(false);
        }),
        "a\nc\nb|"
    );
    assert_eq!(
        run("a|\nb", |b| {
            assert!(b.move_lines(true).is_empty());
        }),
        "a|\nb"
    );
    assert_eq!(
        run("a\nb|", |b| {
            assert!(b.move_lines(false).is_empty());
        }),
        "a\nb|"
    );
    assert_eq!(
        run("a\nb|", |b| {
            b.move_lines(true);
        }),
        "b|\na"
    );
    assert_eq!(
        run("a|\nb", |b| {
            b.move_lines(false);
        }),
        "b\na|"
    );
    assert_eq!(
        run("a\n«b\nc»\nd", |b| {
            b.move_lines(true);
        }),
        "«b\nc»\na\nd"
    );
    assert_eq!(
        run("a\n«b\nc»\nd", |b| {
            b.move_lines(false);
        }),
        "a\nd\n«b\nc»"
    );
    assert_eq!(
        run("a\r\nb|\r\nc", |b| {
            b.move_lines(true);
        }),
        "b|\r\na\r\nc"
    );
    assert_eq!(
        run("a\nb|\nc\nd|", |b| {
            b.move_lines(true);
        }),
        "b|\na\nd|\nc"
    );
    assert_eq!(
        run("a|\nb\nc|\nd", |b| {
            b.move_lines(false);
        }),
        "b\na|\nd\nc|"
    );
    assert_eq!(
        run("a\n«b\n»c", |b| {
            b.move_lines(true);
        }),
        "«b\n»a\nc"
    );
    assert_eq!(
        run("«a\n»b\nc", |b| {
            b.move_lines(false);
        }),
        "b\n«a\n»c"
    );
    assert_eq!(
        run("a\nb|\nc|", |b| {
            b.move_lines(true);
        }),
        "b|\nc|\na"
    );
    assert_eq!(
        run("a|\nb|\nc", |b| {
            b.move_lines(true);
        }),
        "a|\nb|\nc",
        "group at the top cannot move"
    );
    assert_eq!(
        run("ab|\ncd", |b| {
            b.move_lines(false);
        }),
        "cd\nab|"
    );
}

// ---- clipboard ------------------------------------------------------------------------------

#[test]
fn copy_and_cut() {
    assert_eq!(buf("a«bc»d").copy(), "bc");
    assert_eq!(buf("a|b\nc").copy(), "ab\n");
    assert_eq!(buf("a\nb|").copy(), "b\n");
    assert_eq!(buf("a|\nb|").copy(), "a\nb\n");
    assert_eq!(
        buf("a|b|\nc").copy(),
        "ab\n",
        "two carets on one line copy it once"
    );
    assert_eq!(buf("«a»x«b»").copy(), "a\nb");
    assert_eq!(buf("a\r\n«b»\r\n«c»").copy(), "b\r\nc");
    assert_eq!(buf("|").copy(), "\n");
    let mut b = buf("a«bc»d");
    let (t, tx) = b.cut();
    assert_eq!(
        (t.as_str(), show(&b).as_str(), tx.is_empty()),
        ("bc", "a|d", false)
    );
    let mut b = buf("a\nb|\nc");
    let (t, _) = b.cut();
    assert_eq!(
        (t.as_str(), show(&b).as_str()),
        ("b\n", "a\nc|"),
        "caret keeps its column"
    );
    let mut b = buf("a|\nb");
    let (t, _) = b.cut();
    assert_eq!((t.as_str(), show(&b).as_str()), ("a\n", "b|"));
    let mut b = buf("«a»x«b»");
    b.cut();
    assert_eq!(show(&b), "|x|");
}

#[test]
fn paste_variants() {
    assert_eq!(
        run("a|c", |b| {
            b.paste("b");
        }),
        "ab|c"
    );
    assert_eq!(
        run("a«x»c", |b| {
            b.paste("b");
        }),
        "ab|c"
    );
    assert_eq!(
        run("|\n|", |b| {
            b.paste("x\ny");
        }),
        "x|\ny|"
    );
    assert_eq!(
        run("|a\n|b", |b| {
            b.paste("1\n2\n");
        }),
        "1|a\n2|b"
    );
    assert_eq!(
        run("|\n|\n|", |b| {
            b.paste("x\ny");
        }),
        "x\ny|\nx\ny|\nx\ny|"
    );
    assert_eq!(
        run("a|", |b| {
            b.paste("1\n2");
        }),
        "a1\n2|"
    );
    assert_eq!(
        run("x\r\n|", |b| {
            b.paste("a\nb");
        }),
        "x\r\na\r\nb|"
    );
    assert_eq!(
        run("x\n|", |b| {
            b.paste("a\r\nb");
        }),
        "x\na\nb|"
    );
    assert_eq!(
        run("|", |b| {
            b.paste("a\r\nb");
        }),
        "a\r\nb|",
        "no line break in the document: keep as is"
    );
    assert_eq!(
        run("|", |b| {
            assert!(b.paste("").is_empty());
        }),
        "|"
    );
}

#[test]
fn whole_line_paste_goes_above() {
    let mut b = buf("foo\nba|r");
    let clip = b.copy();
    assert_eq!(clip, "bar\n");
    b.set_cursor(1);
    b.paste(&clip);
    check(&b);
    assert_eq!(show(&b), "bar\nf|oo\nbar");
    // a different text is an ordinary paste
    let mut b = buf("foo\nba|r");
    let _ = b.copy();
    b.set_cursor(1);
    b.paste("x\n");
    assert_eq!(show(&b), "fx\n|oo\nbar");
    // copying a selection forgets the line clip
    let mut b = buf("foo\nba|r");
    let clip = b.copy();
    b.set_selections(vec![Selection {
        id: 0,
        anchor: 0,
        head: 1,
        goal: None,
    }]);
    let _ = b.copy();
    b.set_cursor(2);
    b.paste(&clip);
    assert_eq!(show(&b), "fobar\n|o\nbar");
    // cut line then paste elsewhere
    let mut b = buf("a\nb|\nc");
    let (t, _) = b.cut();
    b.set_cursor(b.text().len());
    b.paste(&t);
    assert_eq!(show(&b), "a\nb\nc|");
}

#[test]
fn whole_line_paste_multi_caret() {
    let mut b = buf("a|\nb|\nc\nd");
    let clip = b.copy();
    assert_eq!(clip, "a\nb\n");
    b.set_selections(vec![Selection::caret(0, 4), Selection::caret(0, 6)]);
    b.paste(&clip);
    check(&b);
    // two carets on two rows, two lines: one line above each row
    assert_eq!(show(&b), "a\nb\na\n|c\nb\n|d");
    // same row for both carets: the whole clip once
    let mut b = buf("a|\nb|\nc");
    let clip = b.copy();
    b.set_selections(vec![Selection::caret(0, 4), Selection::caret(0, 5)]);
    b.paste(&clip);
    assert_eq!(show(&b), "a\nb\na\nb\n|c|");
}

// ---- undo / redo ---------------------------------------------------------------------------

fn coalescing(s: &str) -> Buffer {
    let mut b = buf(s);
    b.set_coalesce_window(Duration::from_secs(3600));
    b
}

#[test]
fn undo_redo_restores_text_and_selections() {
    let mut b = buf("a|b|c");
    b.insert("X");
    assert_eq!(show(&b), "aX|bX|c");
    let tx = b.undo();
    assert_eq!(show(&b), "a|b|c");
    assert_eq!(tx.deltas.len(), 2);
    let tx = b.redo();
    assert_eq!(show(&b), "aX|bX|c");
    assert_eq!(tx.deltas.len(), 2);
    assert!(
        b.undo().deltas.len() == 2 && b.undo().is_empty(),
        "nothing left to undo"
    );
    assert!(!b.can_undo());
    assert!(b.can_redo());
    b.redo();
    assert!(!b.can_redo());
    assert!(b.redo().is_empty());
}

#[test]
fn typing_coalesces() {
    let mut b = coalescing("|");
    for c in ["a", "b", "c"] {
        b.insert(c);
    }
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "|");
    b.redo();
    assert_eq!(show(&b), "abc|");
    // newline breaks the run (and is its own step)
    let mut b = coalescing("|");
    b.insert("a");
    b.newline();
    b.insert("b");
    b.insert("c");
    assert_eq!(b.undo_depth(), 3);
    b.undo();
    assert_eq!(show(&b), "a\n|");
    // a caret jump breaks the run
    let mut b = coalescing("|");
    b.insert("a");
    b.set_cursor(0);
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
    // moving the caret breaks the run
    let mut b = coalescing("|");
    b.insert("a");
    b.move_carets(Motion::Left, false);
    b.move_carets(Motion::Right, false);
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
    // paste breaks the run
    let mut b = coalescing("|");
    b.insert("a");
    b.paste("zz");
    b.insert("b");
    assert_eq!(b.undo_depth(), 3);
    // typing then backspace are different kinds
    let mut b = coalescing("|");
    b.insert("a");
    b.insert("b");
    b.backspace();
    b.backspace();
    assert_eq!(b.undo_depth(), 2);
    // window zero: nothing merges
    let mut b = buf("|");
    b.insert("a");
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
}

#[test]
fn backspace_and_delete_coalesce() {
    let mut b = coalescing("abc|");
    b.backspace();
    b.backspace();
    b.backspace();
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "abc|");
    let mut b = coalescing("|abc");
    b.delete();
    b.delete();
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "|abc");
}

#[test]
fn coalescing_respects_time_window() {
    let mut b = buf("|");
    b.set_coalesce_window(Duration::from_millis(50));
    b.insert("a");
    std::thread::sleep(Duration::from_millis(120));
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
    b.break_undo_group();
    b.set_coalesce_window(Duration::from_secs(10));
    b.insert("c");
    b.insert("d");
    b.break_undo_group();
    b.insert("e");
    assert_eq!(b.undo_depth(), 4);
}

#[test]
fn explicit_transactions_group_edits() {
    let mut b = buf("|");
    b.begin_transaction();
    b.insert("a");
    b.newline();
    b.insert("b");
    b.end_transaction();
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "|");
    b.redo();
    assert_eq!(show(&b), "a\nb|");
    // nested
    let mut b = buf("|");
    b.begin_transaction();
    b.insert("a");
    b.begin_transaction();
    b.insert("b");
    b.end_transaction();
    b.insert("c");
    b.end_transaction();
    b.insert("d");
    assert_eq!(b.undo_depth(), 2);
    // empty transaction creates nothing
    let mut b = buf("|");
    b.begin_transaction();
    b.end_transaction();
    assert_eq!(b.undo_depth(), 0);
    // a transaction does not merge into the previous coalescing run
    let mut b = coalescing("|");
    b.insert("a");
    b.begin_transaction();
    b.insert("b");
    b.end_transaction();
    b.insert("c");
    assert_eq!(b.undo_depth(), 2 + 1);
}

#[test]
fn new_edit_clears_redo() {
    let mut b = buf("|");
    b.insert("a");
    b.undo();
    assert!(b.can_redo());
    b.insert("b");
    assert!(!b.can_redo());
    assert_eq!(show(&b), "b|");
}

#[test]
fn undo_after_set_selections_restores_old_selection_state() {
    let mut b = buf("a|b");
    b.insert("X");
    b.set_cursor(0);
    b.undo();
    assert_eq!(show(&b), "a|b");
}

#[test]
fn dirty_tracking() {
    let mut b = buf("a|");
    assert!(!b.is_dirty());
    b.insert("b");
    assert!(b.is_dirty());
    b.undo();
    assert!(!b.is_dirty(), "undo back to the loaded state is clean");
    b.redo();
    b.mark_saved();
    assert!(!b.is_dirty());
    b.insert("c");
    assert!(b.is_dirty());
    b.undo();
    assert!(!b.is_dirty());
    b.undo();
    assert!(b.is_dirty(), "undo past the saved state is dirty");
    // selections only changes do not dirty
    let mut b = buf("abc|");
    b.move_carets(Motion::Left, false);
    b.select_all();
    assert!(!b.is_dirty());
}

#[test]
fn history_is_bounded() {
    let mut b = buf("|");
    for i in 0..10_050 {
        b.insert(if i % 2 == 0 { "a" } else { "b" });
    }
    assert_eq!(b.undo_depth(), 10_000);
    let mut n = 0;
    while b.can_undo() {
        b.undo();
        n += 1;
    }
    assert_eq!(n, 10_000);
    assert_eq!(b.text().len(), 50);
}

#[test]
fn history_memory_is_bounded_by_cost() {
    let mut b = buf("|");
    for _ in 0..200 {
        b.add_caret_below();
    }
    let many: Vec<Selection> = (0..2000).map(|i| Selection::caret(0, i)).collect();
    b.set_text_for_test(&"x".repeat(2000), many);
    for _ in 0..300 {
        b.insert("y");
    }
    assert!(
        b.undo_depth() < 300,
        "cost cap drops old steps with 2000 carets each"
    );
    assert!(b.undo_depth() > 20);
}

// ---- deltas -------------------------------------------------------------------------------------

#[test]
fn deltas_are_exact() {
    let mut b = buf("ab\ncd|\nef|");
    let tx = b.insert("é\n");
    // back to front: the later caret first
    assert_eq!(tx.deltas.len(), 2);
    let d0 = tx.deltas[0];
    assert_eq!(
        d0,
        EditDelta {
            start_byte: 8,
            old_end_byte: 8,
            new_end_byte: 8 + "é\n".len(),
            start: Point::new(2, 2),
            old_end: Point::new(2, 2),
            new_end: Point::new(3, 0),
        }
    );
    assert_eq!(tx.deltas[1].start, Point::new(1, 2));
    assert_eq!(tx.deltas[1].new_end, Point::new(2, 0));
}

#[test]
fn deltas_for_replacement_and_deletion() {
    let mut b = buf("a«b\nc»d");
    let tx = b.insert("xy");
    assert_eq!(
        tx.deltas,
        vec![EditDelta {
            start_byte: 1,
            old_end_byte: 4,
            new_end_byte: 3,
            start: Point::new(0, 1),
            old_end: Point::new(1, 1),
            new_end: Point::new(0, 3),
        }]
    );
    let tx = b.undo();
    assert_eq!(
        tx.deltas,
        vec![EditDelta {
            start_byte: 1,
            old_end_byte: 3,
            new_end_byte: 4,
            start: Point::new(0, 1),
            old_end: Point::new(0, 3),
            new_end: Point::new(1, 1),
        }]
    );
}

#[test]
fn no_change_means_empty_transaction() {
    let mut b = buf("|");
    let v = b.version();
    assert!(b.backspace().is_empty());
    assert!(b.delete().is_empty());
    assert!(b.outdent().is_empty());
    assert!(b.undo().is_empty());
    assert!(!b.is_dirty());
    assert_eq!(b.version(), v);
    assert_eq!(b.undo_depth(), 0);
}

// ---- external edits ----------------------------------------------------------------------------------

#[test]
fn apply_edits_maps_selections() {
    let mut b = buf("hello |world");
    let tx = b.apply_edits(vec![TextEdit {
        range: 0..5,
        new_text: "HELLO!".into(),
    }]);
    assert_eq!(show(&b), "HELLO! |world");
    assert_eq!(tx.deltas.len(), 1);
    // an insertion exactly at the caret pushes it along with its text
    let mut b = buf("|abc");
    b.apply_edits(vec![TextEdit {
        range: 0..0,
        new_text: "X".into(),
    }]);
    assert_eq!(show(&b), "X|abc");
    // edits are minimized: only the differing middle is replaced
    let mut b = buf("fn a(|) {}");
    let tx = b.apply_edits(vec![TextEdit {
        range: 0..10,
        new_text: "fn a() { }".into(),
    }]);
    assert_eq!(tx.deltas.len(), 1);
    assert_eq!(tx.deltas[0].start_byte, 8);
    assert_eq!(show(&b), "fn a(|) { }");
    // several edits are one undo step
    let mut b = buf("aXbXc|");
    b.apply_edits(vec![
        TextEdit {
            range: 3..4,
            new_text: "--".into(),
        },
        TextEdit {
            range: 1..2,
            new_text: "".into(),
        },
    ]);
    assert_eq!(show(&b), "ab--c|");
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "aXbXc|");
}

#[test]
fn apply_edits_ignores_bad_edits() {
    let mut b = buf("abcdef|");
    // overlapping second edit is dropped; out-of-range clamps; no-ops vanish
    let tx = b.apply_edits(vec![
        TextEdit {
            range: 0..3,
            new_text: "x".into(),
        },
        TextEdit {
            range: 2..5,
            new_text: "y".into(),
        },
        TextEdit {
            range: 5..99,
            new_text: "!".into(),
        },
        TextEdit {
            range: 4..4,
            new_text: "".into(),
        },
    ]);
    assert_eq!(show(&b), "xde!|");
    assert_eq!(tx.deltas.len(), 2);
    assert!(b.apply_edits(vec![]).is_empty());
    assert!(
        b.apply_edits(vec![TextEdit {
            range: 0..1,
            new_text: "x".into()
        }])
        .is_empty(),
        "identical text"
    );
    // multi-byte clipping
    let mut b = buf("é|");
    b.apply_edits(vec![TextEdit {
        range: 1..1,
        new_text: "X".into(),
    }]);
    check(&b);
}

#[test]
fn replace_all_keeps_carets_near() {
    let mut b = buf("line1\nline|2\nline3");
    let tx = b.replace_all("line1\nline2\nline3 changed");
    assert_eq!(show(&b), "line1\nline|2\nline3 changed");
    assert_eq!(tx.deltas.len(), 1);
    // change above shifts the caret with its text
    let mut b = buf("line1\nline|2\nline3");
    b.replace_all("X\nline1\nline2\nline3");
    assert_eq!(show(&b), "X\nline1\nline|2\nline3");
    // caret inside the changed region keeps row/column
    let mut b = buf("abc\nde|f\n");
    b.replace_all("ABC\nDEF\n");
    assert_eq!(show(&b), "ABC\nDE|F\n");
    // shrinking clips
    let mut b = buf("abc\ndef\nghi|");
    b.replace_all("x");
    assert_eq!(show(&b), "x|");
    // identical
    let mut b = buf("same|");
    assert!(b.replace_all("same").is_empty());
    // undoable
    let mut b = buf("a|");
    b.replace_all("zzz");
    b.undo();
    assert_eq!(show(&b), "a|");
    // crlf boundary
    let mut b = buf("a\r\nb|");
    b.replace_all("a\nb");
    assert_eq!(b.text().to_string(), "a\nb");
    check(&b);
}

// ---- misc ---------------------------------------------------------------------------------------------

#[test]
fn empty_buffer_edge_cases() {
    for f in [
        |b: &mut Buffer| {
            b.backspace();
        },
        |b: &mut Buffer| {
            b.delete();
        },
        |b: &mut Buffer| {
            b.delete_line();
        },
        |b: &mut Buffer| {
            b.duplicate_line();
        },
        |b: &mut Buffer| {
            b.move_lines(true);
        },
        |b: &mut Buffer| {
            b.move_lines(false);
        },
        |b: &mut Buffer| {
            b.outdent();
        },
        |b: &mut Buffer| {
            b.select_line();
        },
        |b: &mut Buffer| {
            b.select_all();
        },
        |b: &mut Buffer| {
            b.add_caret_above();
        },
        |b: &mut Buffer| {
            b.add_caret_below();
        },
        |b: &mut Buffer| {
            b.delete_word_left();
        },
        |b: &mut Buffer| {
            b.delete_word_right();
        },
        |b: &mut Buffer| {
            b.toggle_comment();
        },
        |b: &mut Buffer| {
            b.cut();
        },
        |b: &mut Buffer| {
            b.copy();
        },
        |b: &mut Buffer| {
            b.select_all_occurrences();
        },
    ] {
        let mut b = Buffer::new(Rope::new(), options());
        f(&mut b);
        check(&b);
    }
}

#[test]
fn end_of_buffer_without_newline() {
    assert_eq!(
        run("abc|", |b| {
            b.delete();
        }),
        "abc|"
    );
    assert_eq!(
        run("abc|", |b| {
            b.move_carets(Motion::Down, false);
        }),
        "abc|"
    );
    assert_eq!(
        run("abc|", |b| {
            b.duplicate_line();
        }),
        "abc\nabc|"
    );
    assert_eq!(
        run("a\nbc|", |b| {
            b.indent();
        }),
        "a\n    bc|"
    );
}

#[test]
fn tabs_vs_spaces_options() {
    let mut o = options();
    o.indent = IndentStyle::Spaces(2);
    o.tab_size = 2;
    assert_eq!(
        run_with("a|", o.clone(), |b| {
            b.indent();
        }),
        "  a|"
    );
    assert_eq!(
        run_with("{|", o.clone(), |b| {
            b.newline();
        }),
        "{\n  |"
    );
    assert_eq!(
        run_with("    |x", o, |b| {
            b.backspace();
        }),
        "  |x"
    );
}

#[test]
fn clone_is_independent() {
    let mut a = buf("a|");
    a.insert("b");
    let mut c = a.clone();
    c.insert("c");
    assert_eq!(show(&a), "ab|");
    assert_eq!(show(&c), "abc|");
    c.undo();
    c.undo();
    assert_eq!(show(&c), "a|");
    assert_eq!(show(&a), "ab|");
}

#[test]
fn unicode_text_through_ops() {
    assert_eq!(
        run("😀|😀", |b| {
            b.insert("é");
        }),
        "😀é|😀"
    );
    assert_eq!(
        run("é|😀", |b| {
            b.backspace();
        }),
        "|😀"
    );
    assert_eq!(
        run("a|\u{301}b", |b| {
            b.move_carets(Motion::Right, false);
        }),
        "a\u{301}|b"
    );
    assert_eq!(
        run("日本語|", |b| {
            b.select_word_at(3);
        }),
        "日«本»語"
    );
}

impl Buffer {
    /// Test helper: replaces text and selections without history.
    fn set_text_for_test(&mut self, text: &str, sels: Vec<Selection>) {
        self.text = Rope::from_str(text);
        self.history = crate::history::History::new();
        self.history.window = Duration::ZERO;
        self.set_selections(sels);
    }
}

#[test]
fn buffer_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Buffer>();
    assert_send_sync::<Transaction>();
}

#[test]
fn newest_selection_survives_for_escape() {
    let mut b = buf("foo foo foo|");
    b.select_word_at(0);
    b.select_next_occurrence();
    b.select_next_occurrence();
    assert_eq!(show(&b), "«foo» «foo» «foo»");
    assert_eq!(b.newest_selection().range(), 8..11);
    b.collapse_selections();
    assert_eq!(show(&b), "foo foo «foo»");
    b.collapse_selections();
    assert_eq!(show(&b), "foo foo foo|");
    // an edit keeps tracking the same caret
    let mut b = buf("a|b|c|");
    b.add_caret(0);
    b.insert("X");
    b.collapse_selections();
    assert_eq!(show(&b), "X|aXbXcX");
    assert_eq!(b.selections().len(), 1);
    assert_eq!(b.selections()[0].head, 1);
}

#[test]
fn hash_comments_and_multiline_occurrences() {
    let mut o = options();
    o.comment.line = Some("#".into());
    assert_eq!(
        run_with("«a\n  b»", o.clone(), |b| {
            b.toggle_comment();
        }),
        "«# a\n#   b»"
    );
    assert_eq!(
        run_with("«# a\n#   b»", o, |b| {
            b.toggle_comment();
        }),
        "«a\n  b»"
    );
    let mut b = buf("«a\nb»\nx\na\nb\nx");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "«a\nb»\nx\n«a\nb»\nx");
    assert!(!b.select_next_occurrence());
}

#[test]
fn paste_distribution_is_one_undo_step() {
    let mut b = buf("|a\n|b\n|c");
    b.paste("1\n2\n3");
    assert_eq!(show(&b), "1|a\n2|b\n3|c");
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "|a\n|b\n|c");
}

#[test]
fn reversed_selection_keeps_direction_through_edits() {
    let mut b = buf("x\n‹ab›\ny");
    b.indent();
    assert_eq!(show(&b), "x\n‹    ab›\ny");
    b.move_lines(true);
    assert_eq!(show(&b), "‹    ab›\nx\ny");
    let s = b.selections()[0];
    assert!(s.reversed());
}

#[test]
fn crlf_documents_stay_crlf() {
    let mut b = buf("a|\r\nb");
    b.newline();
    b.insert("x");
    b.paste("p\nq");
    b.duplicate_line();
    b.newline_below();
    check(&b);
    let text = b.text().to_string();
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "no bare LF: {text:?}"
    );
}

#[test]
fn large_buffer_edits_have_exact_points() {
    let mut text = String::new();
    for i in 0..20_000 {
        text.push_str(&format!("line {i} ünï\n"));
    }
    let mut b = Buffer::new(Rope::from_str(&text), options());
    b.set_cursor(text.len() / 2);
    let at = b.selections()[0].head;
    let before_point = b.text().offset_to_point(at);
    let tx = b.insert("é");
    assert_eq!(tx.deltas.len(), 1);
    let d = tx.deltas[0];
    assert_eq!(d.start, before_point);
    assert_eq!(
        d.new_end,
        Point::new(before_point.row, before_point.column + 2)
    );
    assert_eq!(b.text().offset_to_point(d.new_end_byte), d.new_end);
}
