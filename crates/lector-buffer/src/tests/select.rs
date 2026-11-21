//! Select commands: all, line, word, occurrences, carets and splitting.

use super::*;

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
