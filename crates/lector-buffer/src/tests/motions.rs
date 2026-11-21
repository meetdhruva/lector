//! The selection model and cursor motions.

use super::*;

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
