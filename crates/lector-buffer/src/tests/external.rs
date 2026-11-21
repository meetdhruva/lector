//! External edits: mapping selections through apply_edits and replace_all.

use super::*;

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
