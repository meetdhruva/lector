//! Backspace, delete, word and line deletion.

use super::*;

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
