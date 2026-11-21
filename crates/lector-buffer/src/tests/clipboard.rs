//! Copy, cut and paste.

use super::*;

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
