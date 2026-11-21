//! Typing: insertion, auto-close and type-over, surround, newline auto-indent.

use super::*;

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
        "()|)",
        "a closer that was already there is not skipped"
    );
    assert_eq!(
        run("|", |b| {
            b.insert("{");
            b.insert("[");
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
        ")|)"
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
        run("|", |b| {
            b.insert("\"");
            b.insert("\"");
        }),
        "\"\"|",
        "type over the quote that was inserted"
    );
    assert_eq!(
        run("\"|\"", |b| {
            b.insert("\"");
        }),
        "\"\"|\"",
        "an existing quote is not skipped"
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
        "key:\n|",
        "`:` does not open a block by default"
    );
    let mut o = options();
    o.indent_after_colon = true;
    assert_eq!(
        run_with("key:|", o, |b| {
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
