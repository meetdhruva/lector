//! Line and block comment toggling.

use super::*;

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
