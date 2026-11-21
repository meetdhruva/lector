//! Duplicating and moving lines.

use super::*;

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
