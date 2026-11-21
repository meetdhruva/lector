//! Tab, indent and outdent.

use super::*;

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
