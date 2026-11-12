//! Tests of the completion session model.

use serde_json::{json, Value};

use super::*;
use crate::completion::{parse_completion, Documentation};

fn pt(row: u32, column: u32) -> PointUtf16 {
    PointUtf16 { row, column }
}

/// Text with `|` marking the caret.
fn with_caret(s: &str) -> (Rope, usize) {
    let caret = s.find('|').expect("caret marker");
    (Rope::from_str(&s.replacen('|', "", 1)), caret)
}

fn response(items: Value, pos: PointUtf16) -> CompletionResponse {
    let (items, is_incomplete) = parse_completion(items, &[]).expect("items");
    CompletionResponse {
        items,
        is_incomplete,
        request_pos: pos,
        doc_version: 1,
        tag: None,
        resolve_supported: true,
    }
}

fn open(s: &str, items: Value, options: SessionOptions) -> (CompletionSession, Rope, usize) {
    let (rope, caret) = with_caret(s);
    let mut session = CompletionSession::open(&rope, caret, OpenReason::Typing, options);
    let pos = session.request().pos;
    assert!(session.set_response(&rope, caret, response(items, pos)));
    (session, rope, caret)
}

fn apply(text: &Rope, edits: &[ByteEdit]) -> String {
    let mut s = text.to_string();
    for e in edits.iter().rev() {
        s.replace_range(e.range.clone(), &e.new_text);
    }
    s
}

fn labels(session: &CompletionSession) -> Vec<String> {
    session
        .matches()
        .iter()
        .map(|m| session.items()[m.item].label.clone())
        .collect()
}

fn names(list: &[&str]) -> Value {
    Value::Array(list.iter().map(|l| json!({"label": l})).collect())
}

fn range16(sc: u32, ec: u32) -> Value {
    json!({"start": {"line": 0, "character": sc}, "end": {"line": 0, "character": ec}})
}

#[test]
fn word_before_the_caret_uses_language_word_chars() {
    let (rope, caret) = with_caret("let foo_bar|");
    let s = CompletionSession::open(&rope, caret, OpenReason::Typing, SessionOptions::default());
    assert_eq!((s.anchor(), s.typed()), (4, "foo_bar"));

    let (rope, caret) = with_caret("echo $HOM|");
    let plain =
        CompletionSession::open(&rope, caret, OpenReason::Typing, SessionOptions::default());
    assert_eq!(plain.typed(), "HOM");
    let opts = SessionOptions {
        word_chars: WordChars::for_language("Bash"),
        ..SessionOptions::default()
    };
    let s = CompletionSession::open(&rope, caret, OpenReason::Typing, opts);
    assert_eq!(s.typed(), "$HOM");

    let (rope, caret) = with_caret("a.|");
    let s = CompletionSession::open(
        &rope,
        caret,
        OpenReason::TriggerCharacter('.'),
        SessionOptions::default(),
    );
    assert_eq!((s.anchor(), s.typed()), (2, ""));
    assert_eq!(
        s.request().trigger,
        CompletionTrigger::TriggerCharacter('.')
    );
    assert_eq!(s.request().pos, pt(0, 2));

    let (rope, caret) = with_caret("x\n  été|");
    let s = CompletionSession::open(&rope, caret, OpenReason::Manual, SessionOptions::default());
    assert_eq!((s.anchor(), s.typed()), (4, "été"));
    assert_eq!(s.request().pos, pt(1, 5), "UTF-16 column");
    assert_eq!(s.request().trigger, CompletionTrigger::Invoked);
}

#[test]
fn label_only_item_replaces_the_typed_prefix() {
    let (s, rope, caret) = open(
        "x = pri|",
        names(&["println", "print"]),
        SessionOptions::default(),
    );
    let (idx, item) = s.selected_item().unwrap();
    assert_eq!(item.label, "print", "ties are ordered by sortText (label)");
    let c = s.commit(&rope, &[caret], 0, idx).unwrap();
    assert_eq!(apply(&rope, &c.edits), "x = print");
    assert_eq!(c.carets, vec![9]);
    assert_eq!((c.skipped_additional_edits, c.skipped_carets), (0, 0));
}

#[test]
fn insert_text_wins_over_label() {
    let items = json!([{"label": "foo(…)", "insertText": "foo()", "filterText": "foo"}]);
    let (s, rope, caret) = open("fo|", items, SessionOptions::default());
    assert_eq!(labels(&s), vec!["foo(…)"], "filtered by filterText");
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "foo()");
}

#[test]
fn text_edit_insert_versus_replace_mode() {
    let items = json!([{"label": "foobaz", "textEdit": {"newText": "foobaz",
        "insert": range16(0, 2), "replace": range16(0, 5)}}]);
    for (mode, want, caret_after) in [
        (InsertMode::Insert, "foobazbar baz", 6),
        (InsertMode::Replace, "foobaz baz", 6),
    ] {
        let opts = SessionOptions {
            insert_mode: mode,
            ..SessionOptions::default()
        };
        let (s, rope, caret) = open("fo|bar baz", items.clone(), opts);
        assert_eq!(s.typed(), "fo");
        let c = s.commit(&rope, &[caret], 0, 0).unwrap();
        assert_eq!(apply(&rope, &c.edits), want, "{mode:?}");
        assert_eq!(c.carets, vec![caret_after]);
    }
}

#[test]
fn no_text_edit_replace_mode_takes_the_rest_of_the_word() {
    let opts = SessionOptions {
        insert_mode: InsertMode::Replace,
        ..SessionOptions::default()
    };
    let (s, rope, caret) = open("pr|int(x)", names(&["println"]), opts);
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "println(x)");
    let (s, rope, caret) = open("pr|int(x)", names(&["println"]), SessionOptions::default());
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "printlnint(x)");
}

#[test]
fn item_defaults_edit_range_with_text_edit_text() {
    let items = json!({"isIncomplete": false,
        "itemDefaults": {"editRange": range16(4, 6)},
        "items": [{"label": "foo", "textEditText": "foo_bar"}, {"label": "fox"}]});
    let (s, rope, caret) = open("let fo|", items, SessionOptions::default());
    assert_eq!(labels(&s), vec!["foo", "fox"]);
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "let foo_bar");
    let c = s.commit(&rope, &[caret], 0, 1).unwrap();
    assert_eq!(apply(&rope, &c.edits), "let fox");
}

#[test]
fn more_typed_since_the_request_re_anchors_the_range() {
    let items =
        json!([{"label": "foo_bar", "textEdit": {"range": range16(4, 6), "newText": "foo_bar"}}]);
    let (mut s, _rope, _) = open("let fo|", items, SessionOptions::default());
    // The user typed "o" after the request.
    let (typed, caret) = with_caret("let foo|");
    assert_eq!(s.update(&typed, caret), Update::Continue);
    assert_eq!(s.typed(), "foo");
    assert_eq!(labels(&s), vec!["foo_bar"]);
    let c = s.commit(&typed, &[caret], 0, 0).unwrap();
    assert_eq!(
        c.edits,
        vec![ByteEdit {
            range: 4..7,
            new_text: "foo_bar".into()
        }]
    );
    assert_eq!(apply(&typed, &c.edits), "let foo_bar");
    // Backspace below the request position.
    let (back, caret) = with_caret("let f|");
    assert_eq!(s.update(&back, caret), Update::Continue);
    let c = s.commit(&back, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&back, &c.edits), "let foo_bar");
}

#[test]
fn utf16_positions_after_non_ascii_text() {
    // "𝄞" is 2 UTF-16 units / 4 bytes, "é" 1 unit / 2 bytes.
    let items = json!([{"label": "fo_x", "textEdit": {"range": range16(4, 6), "newText": "fo_x"}},
                       {"label": "fo𝐀x", "textEdit": {"range": range16(4, 6), "newText": "fo𝐀x"}}]);
    let (mut s, rope, caret) = open("𝄞é fo|", items, SessionOptions::default());
    assert_eq!(s.request().pos, pt(0, 6));
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(c.edits[0].range, 7..9);
    assert_eq!(apply(&rope, &c.edits), "𝄞é fo_x");
    // One more typed character: the end of the range moves by one UTF-16 unit.
    let (more, caret) = with_caret("𝄞é fox|");
    assert_eq!(s.update(&more, caret), Update::Continue);
    let c = s.commit(&more, &[caret], 0, 0).unwrap();
    assert_eq!(c.edits[0].range, 7..10);
    // A word character outside the BMP typed since: 2 UTF-16 units, 4 bytes.
    let (astral, caret) = with_caret("𝄞é fo𝐀|");
    assert_eq!(s.update(&astral, caret), Update::Continue);
    assert_eq!(s.typed(), "fo𝐀");
    let c = s.commit(&astral, &[caret], 0, 1).unwrap();
    assert_eq!(c.edits[0].range, 7..13);
    assert_eq!(apply(&astral, &c.edits), "𝄞é fo𝐀x");
    // An emoji is not a word character: the session ends.
    let (emoji, caret) = with_caret("𝄞é fo😀|");
    assert_eq!(
        s.update(&emoji, caret),
        Update::Close(CloseReason::LeftWord)
    );
}

#[test]
fn filtering_ranks_prefix_and_word_boundary_matches_first() {
    let items = names(&[
        "xfoo", "foo_bar", "fooBar", "format", "fo", "buffoon", "Foo",
    ]);
    let (s, ..) = open("fo|", items, SessionOptions::default());
    let got = labels(&s);
    assert_eq!(got.len(), 7);
    assert_eq!(got[0], "fo", "exact match first: {got:?}");
    let mut prefix: Vec<_> = got[..5].to_vec();
    prefix.sort();
    assert_eq!(prefix, ["Foo", "fo", "fooBar", "foo_bar", "format"]);
    assert_eq!(&got[5..], ["xfoo", "buffoon"], "{got:?}");
    // "fb" only matches word-boundary or later subsequences; boundaries rank first.
    let (s, ..) = open(
        "fb|",
        names(&["foo_bar", "fooBar", "fobar", "fxxbxx", "bf"]),
        SessionOptions::default(),
    );
    assert_eq!(labels(&s), vec!["foo_bar", "fooBar", "fobar", "fxxbxx"]);
}

#[test]
fn case_match_breaks_ties_and_is_not_required() {
    let (s, ..) = open(
        "vec|",
        names(&["Vec", "vec", "VEC_MAX"]),
        SessionOptions::default(),
    );
    assert_eq!(labels(&s)[0], "vec");
    let (s, ..) = open("Vec|", names(&["vec", "Vec"]), SessionOptions::default());
    assert_eq!(labels(&s), vec!["Vec", "vec"]);
}

#[test]
fn empty_query_orders_by_sort_text_then_stable() {
    let items = json!([
        {"label": "c", "sortText": "2"}, {"label": "a", "sortText": "3"},
        {"label": "b", "sortText": "1"}, {"label": "d", "sortText": "2"}, {"label": "e"}]);
    let (s, ..) = open("|", items, SessionOptions::default());
    // sortText "1","2","2","3"; the label-keyed "e" sorts by its label ("e" > "3").
    assert_eq!(labels(&s), vec!["b", "c", "d", "a", "e"]);
}

#[test]
fn deprecated_items_sink_but_stay() {
    let items = json!([{"label": "old_foo", "deprecated": true}, {"label": "foo_new"},
                       {"label": "foo", "tags": [1]}, {"label": "foobar"}]);
    let (s, ..) = open("foo|", items, SessionOptions::default());
    assert_eq!(labels(&s), vec!["foo_new", "foobar", "foo", "old_foo"]);
}

#[test]
fn preselect_sets_the_default_selection_among_the_best() {
    let items =
        json!([{"label": "alpha"}, {"label": "beta", "preselect": true}, {"label": "gamma"}]);
    let (s, ..) = open("|", items, SessionOptions::default());
    assert_eq!(s.selected_item().unwrap().1.label, "beta");
    // A preselected item that is not among the best matches does not jump the queue.
    let items = json!([{"label": "foo"}, {"label": "xxfoo", "preselect": true}]);
    let (s, ..) = open("fo|", items, SessionOptions::default());
    assert_eq!(s.selected_item().unwrap().1.label, "foo");
}

#[test]
fn selection_moves_wraps_and_survives_narrowing() {
    let (mut s, ..) = open(
        "|",
        names(&["aa", "ab", "ac", "bd"]),
        SessionOptions::default(),
    );
    s.select_previous();
    assert_eq!(s.selected_item().unwrap().1.label, "bd");
    s.select_next();
    assert_eq!(s.selected(), Some(0));
    s.select_page(10);
    assert_eq!(s.selected(), Some(3));
    s.select(1);
    let (rope, caret) = with_caret("a|");
    // `open` created the session on an empty word; moving the caret back to
    // the anchor and typing "a" keeps the explicitly selected item in view.
    assert_eq!(s.update(&rope, caret), Update::Continue);
    assert_eq!(labels(&s), vec!["aa", "ab", "ac"]);
    assert_eq!(s.selected_item().unwrap().1.label, "ab");
}

#[test]
fn narrowing_equals_a_fresh_filter() {
    let items = names(&[
        "get_user",
        "getUser",
        "get_users_by_id",
        "set_user",
        "GetUserName",
        "gutter",
        "unused_gu",
        "global_user",
        "to_string",
        "toString",
        "to_owned",
    ]);
    for query in ["g", "gu", "gus", "gu_", "tos", "t", "zz"] {
        let (mut typed, ..) = open("|", items.clone(), SessionOptions::default());
        let mut text = String::new();
        let mut last = Update::Continue;
        for c in query.chars() {
            text.push(c);
            let rope = Rope::from_str(&text);
            last = typed.update(&rope, text.len());
        }
        let fresh_src = format!("{query}|");
        let (fresh, ..) = open(&fresh_src, items.clone(), SessionOptions::default());
        if fresh.matches().is_empty() {
            assert_eq!(last, Update::Close(CloseReason::NoMatches), "{query}");
        } else {
            assert_eq!(labels(&typed), labels(&fresh), "{query}");
            assert_eq!(typed.matches(), fresh.matches(), "{query}");
        }
    }
}

#[test]
fn highlights_are_byte_ranges_of_the_label() {
    let (s, ..) = open("fb|", names(&["foo_bar", "fäb"]), SessionOptions::default());
    assert_eq!(labels(&s), vec!["foo_bar", "fäb"]);
    assert_eq!(s.label_highlights(0), vec![0..1, 4..5]);
    assert_eq!(s.label_highlights(1), vec![0..1, 3..4], "ä is two bytes");
    assert!(s.label_highlights(9).is_empty());
    let (s, ..) = open("fo|", names(&["foo"]), SessionOptions::default());
    assert_eq!(s.label_highlights(0), vec![0..2], "adjacent chars merge");
    let (s, ..) = open("|", names(&["abc"]), SessionOptions::default());
    assert!(s.label_highlights(0).is_empty());
}

#[test]
fn incomplete_lists_requery_on_further_typing() {
    let items = json!({"isIncomplete": true, "items": [{"label": "foo"}, {"label": "foobar"}]});
    let (mut s, ..) = open("fo|", items, SessionOptions::default());
    assert!(s.is_incomplete());
    // Same text: nothing to ask.
    let (r, c) = with_caret("fo|");
    assert_eq!(s.update(&r, c), Update::Continue);
    // One more character: ask again with trigger kind 3 at the new caret.
    let (r, c) = with_caret("foo|");
    assert_eq!(
        s.update(&r, c),
        Update::Requery(CompletionRequest {
            pos: pt(0, 3),
            trigger: CompletionTrigger::Incomplete
        })
    );
    // Not again for the same text.
    assert_eq!(s.update(&r, c), Update::Continue);
    // The new answer replaces the items.
    let generation = s.generation();
    let again = json!({"isIncomplete": true, "items": [{"label": "foo"}, {"label": "foobaz"}]});
    assert!(s.set_response(&r, c, response(again, pt(0, 3))));
    assert_eq!(s.generation(), generation + 1);
    assert_eq!(labels(&s).len(), 2);
    // No local match at all while incomplete: ask instead of closing.
    let (r, c) = with_caret("foozz|");
    assert!(matches!(s.update(&r, c), Update::Requery(_)));
}

#[test]
fn a_complete_list_never_requeries_and_closes_without_matches() {
    let (mut s, ..) = open("f|", names(&["foo", "fa"]), SessionOptions::default());
    for text in ["fo", "foo"] {
        let rope = Rope::from_str(text);
        assert_eq!(s.update(&rope, text.len()), Update::Continue);
    }
    let rope = Rope::from_str("foox");
    assert_eq!(s.update(&rope, 4), Update::Close(CloseReason::NoMatches));
    assert!(s.is_closed());
    assert_eq!(
        s.update(&rope, 4),
        Update::Close(CloseReason::NoMatches),
        "stays closed"
    );
}

#[test]
fn closing_rules() {
    let make = || open("  fo|", names(&["foo"]), SessionOptions::default()).0;
    let check = |text: &str, want: CloseReason| {
        let (rope, caret) = with_caret(text);
        let mut s = make();
        assert_eq!(s.update(&rope, caret), Update::Close(want), "{text}");
    };
    check(" | fo", CloseReason::CaretMoved);
    check("  fo |", CloseReason::LeftWord);
    check("  fo(|", CloseReason::LeftWord);
    check("  fo\n|", CloseReason::CaretMoved);
    check("x fo|", CloseReason::TextChanged);
    let (rope, caret) = with_caret("  f|o");
    assert_eq!(
        make().update(&rope, caret),
        Update::Continue,
        "caret inside the word"
    );
    let mut s = make();
    s.close(CloseReason::Dismissed);
    let (rope, caret) = with_caret("  fo|");
    assert_eq!(
        s.update(&rope, caret),
        Update::Close(CloseReason::Dismissed)
    );
}

#[test]
fn updates_before_the_answer_only_track_the_caret() {
    let (rope, caret) = with_caret("fo|");
    let mut s =
        CompletionSession::open(&rope, caret, OpenReason::Typing, SessionOptions::default());
    let (r, c) = with_caret("foo|");
    assert_eq!(s.update(&r, c), Update::Continue);
    assert_eq!(s.typed(), "foo");
    assert!(!s.has_response() && s.matches().is_empty());
    // The answer was made for "fo": the range is re-anchored to "foo".
    assert!(s.set_response(&r, c, response(names(&["foobar"]), pt(0, 2))));
    assert_eq!(labels(&s), vec!["foobar"]);
    // An answer for another row is rejected.
    assert!(!s.set_response(&r, c, response(names(&["x"]), pt(3, 0))));
}

#[test]
fn commit_characters_need_a_selection_or_typed_text() {
    let items = json!([{"label": "foo", "commitCharacters": [".", "("]}, {"label": "fox"}]);
    let (mut s, ..) = open("|", items.clone(), SessionOptions::default());
    assert!(!s.is_commit_character('.'), "nothing typed, nothing chosen");
    s.select_next();
    assert!(!s.is_commit_character('.'), "selected 'fox' has none");
    s.select_previous();
    assert!(
        s.is_commit_character('.') && s.is_commit_character('(') && !s.is_commit_character(',')
    );
    let (s, ..) = open("fo|", items.clone(), SessionOptions::default());
    assert!(s.is_commit_character('.'));
    let opts = SessionOptions {
        commit_characters: false,
        ..SessionOptions::default()
    };
    let (s, ..) = open("fo|", items, opts);
    assert!(!s.is_commit_character('.'));
}

#[test]
fn multi_caret_applies_where_the_prefix_matches() {
    let (rope, _) = with_caret("fo|\nfo\nfa\nxfo\n");
    let items = names(&["foobar"]);
    let mut s = CompletionSession::open(&rope, 2, OpenReason::Typing, SessionOptions::default());
    assert!(s.set_response(&rope, 2, response(items, pt(0, 2))));
    // Carets after "fo" (row 0, row 1, within "xfo" row 3) and after "fa" (row 2).
    let carets = [2usize, 5, 8, 12];
    let c = s.commit(&rope, &carets, 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "foobar\nfoobar\nfa\nxfoobar\n");
    assert_eq!(c.skipped_carets, 1);
    // Carets after the edits: 6, 6+1+6, then "fa" shifts by the two growths (+8).
    assert_eq!(c.carets, vec![6, 13, 16, 24]);
    // The primary can be any caret.
    let c = s.commit(&rope, &carets, 1, 0);
    assert_eq!(
        c,
        Err(CommitError::TextChanged),
        "the session belongs to row 0"
    );
    assert_eq!(s.commit(&rope, &[], 0, 0), Err(CommitError::NoCaret));
    assert_eq!(s.commit(&rope, &carets, 0, 5), Err(CommitError::NoSuchItem));
}

#[test]
fn multi_caret_replace_mode_needs_equal_suffixes() {
    let opts = SessionOptions {
        insert_mode: InsertMode::Replace,
        ..SessionOptions::default()
    };
    let (rope, _) = with_caret("fo|ox\nfoox\nfooy\n");
    let mut s = CompletionSession::open(&rope, 2, OpenReason::Typing, opts);
    assert!(s.set_response(&rope, 2, response(names(&["foobar"]), pt(0, 2))));
    let c = s.commit(&rope, &[2, 7, 12], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "foobar\nfoobar\nfooy\n");
    assert_eq!(c.skipped_carets, 1, "the third word ends differently");
}

#[test]
fn additional_edits_from_the_list_are_re_anchored() {
    let items = json!([{"label": "HashMap", "additionalTextEdits": [
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
         "newText": "use std::collections::HashMap;\n"}]}]);
    // The request was made at row 1, column 9; a character typed on that row
    // since must not disturb an edit on row 0.
    let (requested, caret) = with_caret("\nlet m: Ha|");
    let mut s = CompletionSession::open(
        &requested,
        caret,
        OpenReason::Typing,
        SessionOptions::default(),
    );
    assert!(s.set_response(&requested, caret, response(items, pt(1, 9))));
    let (rope, caret) = with_caret("\nlet m: Has|");
    assert_eq!(s.update(&rope, caret), Update::Continue);
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(c.skipped_additional_edits, 0);
    assert_eq!(
        apply(&rope, &c.edits),
        "use std::collections::HashMap;\n\nlet m: HashMap"
    );
    assert_eq!(c.carets, vec![caret + 31 + 4]);
}

#[test]
fn resolved_additional_edits_use_the_caret_of_their_document_version() {
    let (rope_a, caret_a) = with_caret("fn f() {\n  let m = Ha|\n}\n");
    let mut s = CompletionSession::open(
        &rope_a,
        caret_a,
        OpenReason::Typing,
        SessionOptions::default(),
    );
    assert!(s.set_response(&rope_a, caret_a, response(names(&["HashMap"]), pt(1, 12))));
    assert!(s.needs_resolve(0));
    // The user types "s"; that text version is synced and the resolve is sent for it.
    let (rope_b, caret_b) = with_caret("fn f() {\n  let m = Has|\n}\n");
    assert_eq!(s.update(&rope_b, caret_b), Update::Continue);
    // The server computed the edit for text B: a same-row edit after the caret.
    let resolved = ResolvedCompletion {
        documentation: Documentation::from_json(&json!("A map.")),
        additional_edits: vec![
            TextEdit {
                range: pt(0, 0)..pt(0, 0),
                new_text: "use std::collections::HashMap;\n".into(),
            },
            TextEdit {
                range: pt(1, 13)..pt(1, 13),
                new_text: "::new()".into(),
            },
        ],
        tag: Some(rope_b.version()),
        ..ResolvedCompletion::default()
    };
    // Late answers for an older list are ignored.
    assert!(!s.apply_resolved(s.generation() + 1, 0, resolved.clone()));
    assert!(s.apply_resolved(s.generation(), 0, resolved));
    assert!(s.items()[0].resolved && !s.needs_resolve(0));
    assert_eq!(s.items()[0].documentation.as_ref().unwrap().value, "A map.");
    // The user types one more character before accepting.
    let (rope_c, caret_c) = with_caret("fn f() {\n  let m = Hash|\n}\n");
    assert_eq!(s.update(&rope_c, caret_c), Update::Continue);
    let c = s.commit(&rope_c, &[caret_c], 0, 0).unwrap();
    assert_eq!(
        apply(&rope_c, &c.edits),
        "use std::collections::HashMap;\nfn f() {\n  let m = HashMap::new()\n}\n"
    );
    assert_eq!(c.skipped_additional_edits, 0);
}

#[test]
fn resolved_edits_with_an_unknown_document_version_are_skipped() {
    let (rope, caret) = with_caret("Ha|");
    let mut s =
        CompletionSession::open(&rope, caret, OpenReason::Typing, SessionOptions::default());
    assert!(s.set_response(&rope, caret, response(names(&["HashMap"]), pt(0, 2))));
    let resolved = ResolvedCompletion {
        additional_edits: vec![TextEdit {
            range: pt(0, 0)..pt(0, 0),
            new_text: "use x;\n".into(),
        }],
        tag: Some(u64::MAX),
        ..ResolvedCompletion::default()
    };
    assert!(s.apply_resolved(s.generation(), 0, resolved));
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "HashMap");
    assert_eq!(c.skipped_additional_edits, 1);
}

#[test]
fn overlapping_additional_edits_are_dropped() {
    let items = json!([{"label": "foo", "textEdit": {"range": range16(0, 2), "newText": "foo"},
        "additionalTextEdits": [
            {"range": range16(1, 2), "newText": "!"},
            {"range": range16(0, 0), "newText": "// "}]}]);
    let (s, rope, caret) = open("fo|", items, SessionOptions::default());
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(c.skipped_additional_edits, 1, "the overlapping one");
    assert_eq!(
        apply(&rope, &c.edits),
        "// foo",
        "an insertion at the start of the range is fine"
    );
    assert_eq!(c.carets, vec![6]);
}

#[test]
fn snippets_are_flattened_unless_enabled() {
    let items = json!([{"label": "fn", "insertTextFormat": 2,
        "insertText": "fn ${1:name}($2) {\n\t$0\n}"}]);
    let (s, rope, caret) = open("f|", items.clone(), SessionOptions::default());
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "fn name() {\n\t\n}");
    assert!(c.tab_stops.is_empty());
    assert_eq!(c.carets, vec![15]);

    let opts = SessionOptions {
        snippets: true,
        ..SessionOptions::default()
    };
    let (s, rope, caret) = open("  f|", items, opts);
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    let text = apply(&rope, &c.edits);
    // Lines after the first get the line's indentation.
    assert_eq!(text, "  fn name() {\n  \t\n  }");
    let first = c.tab_stops[0].ranges[0].clone();
    assert_eq!(&text[first.clone()], "name");
    assert_eq!(c.carets, vec![first.start]);
    assert_eq!(
        c.tab_stops.iter().map(|t| t.index).collect::<Vec<_>>(),
        vec![1, 2, 0]
    );
    let last = c.tab_stops[2].ranges[0].clone();
    assert_eq!(&text[..last.start], "  fn name() {\n  \t");
}

#[test]
fn multi_line_text_adjusts_indentation_and_line_endings() {
    let items = json!([
        {"label": "if", "insertText": "if x {\n    y\n}"},
        {"label": "raw", "insertText": "if x {\n    y\n}", "insertTextMode": 1}]);
    let (s, rope, caret) = open("\t\ti|", items.clone(), SessionOptions::default());
    let adjust = s
        .matches()
        .iter()
        .find(|m| s.items()[m.item].label == "if")
        .unwrap()
        .item;
    let c = s.commit(&rope, &[caret], 0, adjust).unwrap();
    assert_eq!(apply(&rope, &c.edits), "\t\tif x {\n\t\t    y\n\t\t}");
    assert!(
        s.matches().iter().all(|m| s.items()[m.item].label != "raw"),
        "'raw' does not match 'i'"
    );
    let (s, rope, caret) = open("\t\tra|", items, SessionOptions::default());
    let c = s.commit(&rope, &[caret], 0, 1).unwrap();
    assert_eq!(apply(&rope, &c.edits), "\t\tif x {\n    y\n}");

    // Documents with CRLF get CRLF.
    let (rope, caret) = with_caret("a\r\n  i|\r\n");
    let mut s =
        CompletionSession::open(&rope, caret, OpenReason::Typing, SessionOptions::default());
    assert!(s.set_response(
        &rope,
        caret,
        response(
            json!([{"label": "if", "insertText": "if {\n  y\n}"}]),
            pt(1, 3)
        )
    ));
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "a\r\n  if {\r\n    y\r\n  }\r\n");
}

#[test]
fn command_is_passed_on() {
    let items = json!([{"label": "len", "command": {"title": "", "command": "editor.action.triggerParameterHints"}}]);
    let (s, rope, caret) = open("le|", items, SessionOptions::default());
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert!(c.triggers_signature_help());
}

#[test]
fn text_edit_starting_before_the_word_gets_its_own_filter_text() {
    // A server whose edit covers the trigger character: ".foo" replaces ".fo".
    let items = json!([
        {"label": "foo", "filterText": ".foo", "textEdit": {"range": range16(1, 4), "newText": "[foo]"}},
        {"label": "bar", "filterText": "bar"}]);
    let (rope, caret) = with_caret("a.fo|");
    let mut s =
        CompletionSession::open(&rope, caret, OpenReason::Typing, SessionOptions::default());
    assert!(s.set_response(&rope, caret, response(items, pt(0, 4))));
    assert_eq!(
        labels(&s),
        vec!["foo"],
        "'.fo' matches '.foo'; 'fo' does not match 'bar'"
    );
    let c = s.commit(&rope, &[caret], 0, 0).unwrap();
    assert_eq!(apply(&rope, &c.edits), "a[foo]");
}
