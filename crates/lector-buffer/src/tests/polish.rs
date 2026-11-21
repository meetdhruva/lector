//! Tests of occurrence matching, typed-over closers, electric outdent, toggling carets and no-op edits.

use super::*;

fn typed(start: &str, keys: &[&str]) -> String {
    run(start, |b| {
        for k in keys {
            b.insert(k);
        }
    })
}

#[test]
fn next_occurrence_from_a_caret_matches_whole_words() {
    let mut b = buf("foo foobar foo_x foo| xfoo foo");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "foo foobar foo_x «foo» xfoo foo");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "foo foobar foo_x «foo» xfoo «foo»");
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "«foo» foobar foo_x «foo» xfoo «foo»", "wraps");
    assert!(
        !b.select_next_occurrence(),
        "foobar, foo_x and xfoo are other words"
    );
    check(&b);
}

#[test]
fn next_occurrence_from_a_selection_matches_substrings() {
    let mut b = buf("«foo» foobar foo_x xfoo");
    assert!(b.select_next_occurrence());
    assert!(b.select_next_occurrence());
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "«foo» «foo»bar «foo»_x x«foo»");
    assert!(!b.select_next_occurrence());
    check(&b);
}

#[test]
fn occurrence_matching_is_case_sensitive() {
    let mut b = buf("Foo foo FOO |Foo");
    assert!(b.select_next_occurrence());
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "«Foo» foo FOO «Foo»");
    assert!(!b.select_next_occurrence());
    assert_eq!(
        run("«Foo» foo FOO Foo", |b| b.select_all_occurrences()),
        "«Foo» foo FOO «Foo»"
    );
}

#[test]
fn occurrence_session_ends_when_the_text_or_selection_changes() {
    // a hand-made selection of the same text is a plain substring search
    let mut b = buf("foo| foobar");
    assert!(b.select_next_occurrence());
    b.set_selections(vec![Selection {
        id: 0,
        anchor: 0,
        head: 3,
        goal: None,
    }]);
    // still the same text and query: the session continues
    assert!(!b.select_next_occurrence(), "foobar is not the word foo");
    // after an edit the session is over, substring matching applies
    let mut b = buf("foo| foobar");
    assert!(b.select_next_occurrence());
    b.set_cursor(11);
    b.insert("!");
    b.set_selections(vec![Selection {
        id: 0,
        anchor: 0,
        head: 3,
        goal: None,
    }]);
    assert!(b.select_next_occurrence());
    assert_eq!(show(&b), "«foo» «foo»bar!");
}

#[test]
fn select_all_occurrences_whole_words_from_a_caret() {
    assert_eq!(
        run("a |ab ab abc xab ab", |b| b.select_all_occurrences()),
        "a «ab» «ab» abc xab «ab»"
    );
    // from a selection: substrings
    assert_eq!(
        run("a «ab» ab abc xab", |b| b.select_all_occurrences()),
        "a «ab» «ab» «ab»c x«ab»"
    );
    // ctrl+d then select-all keeps the whole-word session
    assert_eq!(
        run("ab| abc ab", |b| {
            b.select_next_occurrence();
            b.select_all_occurrences();
        }),
        "«ab» abc «ab»"
    );
}

#[test]
fn only_inserted_closers_are_typed_over() {
    assert_eq!(typed("|", &["(", ")"]), "()|");
    assert_eq!(typed("|", &["(", "a", ")"]), "(a)|");
    assert_eq!(typed("|", &["(", "(", ")", ")"]), "(())|");
    assert_eq!(typed("|", &["[", "{", "}", "]"]), "[{}]|");
    assert_eq!(typed("|", &["\"", "x", "\""]), "\"x\"|");
    // pre-existing closer
    assert_eq!(typed("f(|)", &[")"]), "f()|)");
    assert_eq!(typed("[|]", &["]"]), "[]|]");
    // our closer is skipped, the one that was already there is not
    assert_eq!(typed("(|)", &["(", ")", ")"]), "(())|)");
}

#[test]
fn type_over_ends_when_the_caret_leaves_the_pair() {
    let mut b = buf("|");
    b.insert("(");
    b.move_carets(Motion::Left, false);
    b.move_carets(Motion::Right, false);
    b.insert(")");
    assert_eq!(
        show(&b),
        "()|)",
        "caret left the pair: the closer is no longer ours"
    );
    // moving inside the pair keeps it
    let mut b = buf("|");
    b.insert("(");
    b.insert("a");
    b.insert("b");
    b.move_carets(Motion::Left, false);
    b.move_carets(Motion::Right, false);
    b.insert(")");
    assert_eq!(show(&b), "(ab)|");
    // undo forgets
    let mut b = buf("|");
    b.insert("(");
    b.insert("a");
    b.undo();
    b.redo();
    b.insert(")");
    assert_eq!(
        show(&b),
        "(a)|)",
        "after undo/redo the pair is no longer tracked"
    );
    // deleting the closer: nothing to skip, nothing breaks
    let mut b = buf("|");
    b.insert("(");
    b.delete();
    b.insert(")");
    assert_eq!(show(&b), "()|");
}

#[test]
fn type_over_with_many_carets() {
    assert_eq!(
        run("a|\nb|\nc|", |b| {
            b.insert("(");
            b.insert("x");
            b.insert(")");
        }),
        "a(x)|\nb(x)|\nc(x)|"
    );
    // only the caret whose closer we inserted skips
    assert_eq!(
        run("(|)\n|", |b| {
            b.insert("(");
            b.insert(")");
        }),
        "(()|)\n()|"
    );
}

#[test]
fn electric_outdent_realigns_a_closer() {
    // matching opener's line
    assert_eq!(
        run("fn f() {\n    if x {\n        y();\n        |", |b| {
            b.insert("}");
        }),
        "fn f() {\n    if x {\n        y();\n    }|"
    );
    assert_eq!(
        run("fn f() {\n    if x {\n        y();\n    }\n    |", |b| {
            b.insert("}");
        }),
        "fn f() {\n    if x {\n        y();\n    }\n}|"
    );
    assert_eq!(
        run("foo(\n    a,\n    |", |b| {
            b.insert(")");
        }),
        "foo(\n    a,\n)|"
    );
    assert_eq!(
        run("x = [\n    1,\n        |", |b| {
            b.insert("]");
        }),
        "x = [\n    1,\n]|"
    );
    // nested brackets of the same kind are counted
    assert_eq!(
        run("if a {\n    if b {\n    }\n        |", |b| {
            b.insert("}");
        }),
        "if a {\n    if b {\n    }\n}|"
    );
    // too shallow lines are aligned as well
    assert_eq!(
        run("    if a {\n|", |b| {
            b.insert("}");
        }),
        "    if a {\n    }|"
    );
    // no opener: one indent unit less
    assert_eq!(
        run("        |", |b| {
            b.insert("}");
        }),
        "    }|"
    );
    assert_eq!(
        run("\t\t|", |b| {
            b.insert(")");
        }),
        "\t)|"
    );
    // a closer after text, or with text after the caret, is not electric
    assert_eq!(
        run("{\n    a|", |b| {
            b.insert("}");
        }),
        "{\n    a}|"
    );
    assert_eq!(
        run("{\n    |x", |b| {
            b.insert("}");
        }),
        "{\n    }|x"
    );
    // already aligned: plain insert
    assert_eq!(
        run("{\n|", |b| {
            b.insert("}");
        }),
        "{\n}|"
    );
}

#[test]
fn electric_outdent_is_one_undo_step_and_respects_options() {
    let mut b = buf("fn f() {\n    x\n    |");
    b.insert("}");
    assert_eq!(show(&b), "fn f() {\n    x\n}|");
    b.undo();
    assert_eq!(show(&b), "fn f() {\n    x\n    |");
    let mut o = options();
    o.auto_indent = false;
    assert_eq!(
        run_with("{\n    |", o, |b| {
            b.insert("}");
        }),
        "{\n    }|"
    );
    // several carets, each on its own whitespace-only line
    assert_eq!(
        run("a {\n        |\n}\nb {\n            |\n}", |b| {
            b.insert("}");
        }),
        "a {\n}|\n}\nb {\n}|\n}"
    );
    // typed over an auto-inserted closer on a blank line
    assert_eq!(
        run("{\n    |", |b| {
            b.insert("(");
            b.insert(")");
        }),
        "{\n    ()|"
    );
}

#[test]
fn colon_indent_is_a_per_language_option() {
    let mut py = options();
    py.indent_after_colon = true;
    assert_eq!(
        run_with("def f():|", py.clone(), |b| {
            b.newline();
        }),
        "def f():\n    |"
    );
    assert_eq!(
        run_with("def f():|", py.clone(), |b| {
            b.newline_below();
        }),
        "def f():\n    |"
    );
    // braces still work and the default has no colon rule
    assert_eq!(
        run_with("if (x) {|", py, |b| {
            b.newline();
        }),
        "if (x) {\n    |"
    );
    assert_eq!(
        run("case x:|", |b| {
            b.newline();
        }),
        "case x:\n|"
    );
    assert_eq!(
        run("case x:|", |b| {
            b.newline_below();
        }),
        "case x:\n|"
    );
    assert!(!BufferOptions::default().indent_after_colon);
}

#[test]
fn add_caret_toggles_an_existing_caret() {
    let mut b = buf("ab|cd|ef");
    b.add_caret(2);
    assert_eq!(show(&b), "abcd|ef", "alt-click on a caret removes it");
    b.add_caret(2);
    assert_eq!(show(&b), "ab|cd|ef");
    // the last caret stays
    let mut b = buf("ab|cd");
    b.add_caret(2);
    assert_eq!(show(&b), "ab|cd");
    // clicks inside a multi-byte char hit the caret before it
    let mut b = buf("a|éb|");
    b.add_caret(2);
    assert_eq!(show(&b), "aéb|");
    // removing the newest caret keeps a valid primary
    let mut b = buf("a|b");
    b.add_caret(2);
    assert_eq!(show(&b), "a|b|");
    b.add_caret(2);
    assert_eq!(show(&b), "a|b");
    assert_eq!(b.newest_selection().head, 1);
    check(&b);
    // a selection is not toggled off by clicking at its edge
    let mut b = buf("a«bc»d|");
    b.add_caret(3);
    check(&b);
    assert_eq!(b.selections().len(), 2);
}

#[test]
fn replacing_a_selection_by_itself_is_not_an_edit() {
    let mut b = buf("hello «world»");
    let v = b.version();
    assert!(b.paste("world").is_empty());
    assert_eq!(b.version(), v);
    assert!(!b.is_dirty());
    assert_eq!(b.undo_depth(), 0);
    assert_eq!(
        show(&b),
        "hello world|",
        "the caret ends after the text as in an edit"
    );
    // typing the selected char over a selection
    let mut b = buf("x«a»y");
    assert!(b.insert("a").is_empty());
    assert!(!b.is_dirty() && !b.can_undo());
    assert_eq!(show(&b), "xa|y");
    // several selections, all identical
    let mut b = buf("«a» b «a»");
    assert!(b.paste("a").is_empty());
    assert!(!b.is_dirty());
    assert_eq!(show(&b), "a| b a|");
    // one of them differs: a real edit
    let mut b = buf("«a» «b»");
    assert!(!b.paste("a").is_empty());
    assert_eq!(b.text().to_string(), "a a");
    assert!(b.is_dirty());
    // an edit followed by an identical paste keeps the dirty state and history
    let mut b = buf("«ab» cd");
    b.insert("x");
    let depth = b.undo_depth();
    b.set_selections(vec![Selection {
        id: 0,
        anchor: 0,
        head: 1,
        goal: None,
    }]);
    assert!(b.paste("x").is_empty());
    assert_eq!(b.undo_depth(), depth);
    // after saving, an identical paste leaves the buffer clean
    let mut b = buf("«abc»");
    b.mark_saved();
    b.paste("abc");
    assert!(!b.is_dirty());
}

/// Proptest regression: two carets on one whitespace-only line, typing a closer.
/// Electric outdent of the end caret used to overlap the other caret's insert.
#[test]
fn electric_outdent_skips_rows_with_several_carets() {
    let mut b = Buffer::new(Rope::from_str("ae\u{301}\n  "), BufferOptions::default());
    b.set_selections(vec![Selection::caret(0, 6), Selection::caret(0, 7)]);
    b.insert(")");
    assert_eq!(b.text().to_string(), "ae\u{301}\n ) )");
    assert_eq!(b.selections().len(), 2);
}
