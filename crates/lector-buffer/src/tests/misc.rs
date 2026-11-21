//! Edge cases: empty buffers, tab and space options, unicode, CRLF, large buffers and assorted regressions.

use super::*;

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
