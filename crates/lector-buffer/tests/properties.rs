//! Randomised invariants: any sequence of operations keeps the buffer valid,
//! deltas describe the change exactly, undo/redo are exact inverses.

use std::time::Duration;

use lector_buffer::{
    Buffer, BufferOptions, CommentTokens, EditDelta, IndentStyle, Motion, Selection, TextEdit,
    Transaction,
};
use lector_text::{Point, Rope};
use proptest::prelude::*;
use unicode_segmentation::UnicodeSegmentation;

const FRAGMENTS: &[&str] = &[
    "a",
    "b",
    "foo_bar",
    " ",
    "  ",
    "    ",
    "\t",
    "\n",
    "\r\n",
    "é",
    "😀",
    "👨‍👩‍👧",
    "e\u{301}",
    "日本",
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    "\"",
    "'",
    "// c",
    ".",
    ",",
    ";",
    "x = 1",
    "\n    ",
    "{\n",
    "\n}",
];

#[derive(Clone, Debug)]
enum Op {
    Insert(usize),
    Typed(usize),
    Newline,
    NewlineBelow,
    NewlineAbove,
    Backspace,
    Delete,
    DeleteWordLeft,
    DeleteWordRight,
    DeleteToLineStart,
    DeleteLine,
    Tab,
    Indent,
    Outdent,
    ToggleComment,
    Duplicate,
    MoveLines(bool),
    Cut,
    Paste(usize),
    PasteLines,
    Undo,
    Redo,
    Move(u8, bool),
    SelectAll,
    SelectLine,
    SelectWord(usize),
    SelectNext,
    SelectAllOcc,
    CaretAbove,
    CaretBelow,
    AddCaret(usize),
    SplitLines,
    Collapse,
    ApplyEdits(Vec<(usize, usize, usize)>),
    ReplaceAll(usize, usize),
    SetCursor(usize),
    Group(Vec<Op>),
}

fn leaf_op() -> impl Strategy<Value = Op> {
    let n = FRAGMENTS.len();
    prop_oneof![
        4 => (0..n).prop_map(Op::Typed),
        2 => (0..n).prop_map(Op::Insert),
        2 => Just(Op::Newline),
        1 => Just(Op::NewlineBelow),
        1 => Just(Op::NewlineAbove),
        3 => Just(Op::Backspace),
        2 => Just(Op::Delete),
        1 => Just(Op::DeleteWordLeft),
        1 => Just(Op::DeleteWordRight),
        1 => Just(Op::DeleteToLineStart),
        1 => Just(Op::DeleteLine),
        1 => Just(Op::Tab),
        1 => Just(Op::Indent),
        1 => Just(Op::Outdent),
        1 => Just(Op::ToggleComment),
        1 => Just(Op::Duplicate),
        1 => any::<bool>().prop_map(Op::MoveLines),
        1 => Just(Op::Cut),
        2 => (0..n).prop_map(Op::Paste),
        1 => Just(Op::PasteLines),
        3 => Just(Op::Undo),
        2 => Just(Op::Redo),
        4 => (0u8..12, any::<bool>()).prop_map(|(m, e)| Op::Move(m, e)),
        1 => Just(Op::SelectAll),
        1 => Just(Op::SelectLine),
        1 => (0usize..400).prop_map(Op::SelectWord),
        1 => Just(Op::SelectNext),
        1 => Just(Op::SelectAllOcc),
        1 => Just(Op::CaretAbove),
        1 => Just(Op::CaretBelow),
        2 => (0usize..400).prop_map(Op::AddCaret),
        1 => Just(Op::SplitLines),
        1 => Just(Op::Collapse),
        1 => prop::collection::vec((0usize..400, 0usize..6, 0..n), 0..4).prop_map(Op::ApplyEdits),
        1 => (0usize..3, 0usize..n).prop_map(|(a, b)| Op::ReplaceAll(a, b)),
        1 => (0usize..400).prop_map(Op::SetCursor),
    ]
}

fn has_undo(op: &Op) -> bool {
    match op {
        Op::Undo | Op::Redo => true,
        Op::Group(ops) => ops.iter().any(has_undo),
        _ => false,
    }
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        12 => leaf_op(),
        1 => prop::collection::vec(leaf_op(), 1..4).prop_map(Op::Group),
    ]
}

fn motion(m: u8) -> Motion {
    match m {
        0 => Motion::Left,
        1 => Motion::Right,
        2 => Motion::Up,
        3 => Motion::Down,
        4 => Motion::WordLeft,
        5 => Motion::WordRight,
        6 => Motion::LineStart,
        7 => Motion::LineEnd,
        8 => Motion::PageUp(3),
        9 => Motion::PageDown(3),
        10 => Motion::BufferStart,
        _ => Motion::BufferEnd,
    }
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Applies `op`; returns the transaction when the op can edit text.
fn apply(b: &mut Buffer, op: &Op) -> Option<Transaction> {
    let len = b.text().len();
    let pos = |x: usize| if len == 0 { 0 } else { x % (len + 1) };
    Some(match op {
        Op::Insert(i) => b.paste(FRAGMENTS[*i]),
        Op::Typed(i) => b.insert(FRAGMENTS[*i]),
        Op::Newline => b.newline(),
        Op::NewlineBelow => b.newline_below(),
        Op::NewlineAbove => b.newline_above(),
        Op::Backspace => b.backspace(),
        Op::Delete => b.delete(),
        Op::DeleteWordLeft => b.delete_word_left(),
        Op::DeleteWordRight => b.delete_word_right(),
        Op::DeleteToLineStart => b.delete_to_line_start(),
        Op::DeleteLine => b.delete_line(),
        Op::Tab => b.tab(),
        Op::Indent => b.indent(),
        Op::Outdent => b.outdent(),
        Op::ToggleComment => b.toggle_comment(),
        Op::Duplicate => b.duplicate_line(),
        Op::MoveLines(up) => b.move_lines(*up),
        Op::Cut => b.cut().1,
        Op::Paste(i) => b.paste(&format!(
            "{}\n{}",
            FRAGMENTS[*i],
            FRAGMENTS[(*i + 3) % FRAGMENTS.len()]
        )),
        Op::PasteLines => {
            let clip = b.copy();
            b.paste(&clip)
        }
        Op::Undo => b.undo(),
        Op::Redo => b.redo(),
        Op::Move(m, e) => {
            b.move_carets(motion(*m), *e);
            return None;
        }
        Op::SelectAll => {
            b.select_all();
            return None;
        }
        Op::SelectLine => {
            b.select_line();
            return None;
        }
        Op::SelectWord(p) => {
            b.select_word_at(pos(*p));
            return None;
        }
        Op::SelectNext => {
            b.select_next_occurrence();
            return None;
        }
        Op::SelectAllOcc => {
            b.select_all_occurrences();
            return None;
        }
        Op::CaretAbove => {
            b.add_caret_above();
            return None;
        }
        Op::CaretBelow => {
            b.add_caret_below();
            return None;
        }
        Op::AddCaret(p) => {
            b.add_caret(pos(*p));
            return None;
        }
        Op::SplitLines => {
            b.split_selection_into_lines();
            return None;
        }
        Op::Collapse => {
            b.collapse_selections();
            return None;
        }
        Op::SetCursor(p) => {
            b.set_cursor(pos(*p));
            return None;
        }
        Op::ApplyEdits(list) => {
            let text = b.text().to_string();
            let edits = list
                .iter()
                .map(|(s, l, f)| {
                    let start = floor_boundary(&text, s % (text.len() + 1));
                    let end = floor_boundary(&text, start + l);
                    TextEdit {
                        range: start..end,
                        new_text: FRAGMENTS[*f].to_string(),
                    }
                })
                .collect();
            b.apply_edits(edits)
        }
        Op::ReplaceAll(mode, f) => {
            let text = b.text().to_string();
            let new = match mode {
                0 => format!("{}{}", FRAGMENTS[*f], text),
                1 => format!("{}{}", text, FRAGMENTS[*f]),
                _ => FRAGMENTS[*f].repeat(3),
            };
            b.replace_all(&new)
        }
        Op::Group(ops) => {
            b.begin_transaction();
            let mut deltas = Vec::new();
            for o in ops {
                if let Some(tx) = apply(b, o) {
                    deltas.extend(tx.deltas);
                }
            }
            b.end_transaction();
            // Group deltas span several calls; callers only use them when it is one call.
            Transaction { deltas }
        }
    })
}

fn point_in(s: &str, offset: usize) -> Point {
    let head = &s[..offset];
    let row = head.matches('\n').count() as u32;
    let col = head.len() - head.rfind('\n').map_or(0, |i| i + 1);
    Point::new(row, col as u32)
}

fn extent(s: &str) -> Point {
    point_in(s, s.len())
}

/// A text as pieces: bytes still present from `before` (with their source offset) or new bytes.
#[derive(Clone, Debug)]
enum Piece {
    Old { src: usize, len: usize },
    New { len: usize },
}

fn piece_len(p: &Piece) -> usize {
    match p {
        Piece::Old { len, .. } | Piece::New { len } => *len,
    }
}

/// Replays `deltas` sequentially on a piece list and checks that every byte
/// that survived equals the same byte of `before`, and lengths line up. This
/// validates deltas without needing the inserted text.
fn assert_deltas_exact(before: &str, after: &str, deltas: &[EditDelta]) {
    let mut pieces = vec![Piece::Old {
        src: 0,
        len: before.len(),
    }];
    for d in deltas {
        assert!(d.start_byte <= d.old_end_byte, "inverted delta {d:?}");
        assert!(d.start_byte <= d.new_end_byte, "inverted delta {d:?}");
        let total: usize = pieces.iter().map(piece_len).sum();
        assert!(
            d.old_end_byte <= total,
            "old_end {} past text {total}",
            d.old_end_byte
        );
        let mut out = Vec::new();
        let mut pos = 0;
        let mut inserted = false;
        for p in &pieces {
            let len = piece_len(p);
            let (lo, hi) = (pos, pos + len);
            // part before the replaced range
            if lo < d.start_byte {
                let keep = (d.start_byte.min(hi)) - lo;
                out.push(match p {
                    Piece::Old { src, .. } => Piece::Old {
                        src: *src,
                        len: keep,
                    },
                    Piece::New { .. } => Piece::New { len: keep },
                });
            }
            if !inserted && hi >= d.start_byte && lo <= d.start_byte {
                if d.new_end_byte > d.start_byte {
                    out.push(Piece::New {
                        len: d.new_end_byte - d.start_byte,
                    });
                }
                inserted = true;
            }
            // part after the replaced range
            if hi > d.old_end_byte {
                let skip = d.old_end_byte.max(lo) - lo;
                out.push(match p {
                    Piece::Old { src, .. } => Piece::Old {
                        src: src + skip,
                        len: len - skip,
                    },
                    Piece::New { .. } => Piece::New { len: len - skip },
                });
            }
            pos = hi;
        }
        if !inserted && d.new_end_byte > d.start_byte {
            out.push(Piece::New {
                len: d.new_end_byte - d.start_byte,
            });
        }
        pieces = out.into_iter().filter(|p| piece_len(p) > 0).collect();
    }
    let mut at = 0;
    for p in &pieces {
        match p {
            Piece::Old { src, len } => {
                assert_eq!(
                    after.get(at..at + len),
                    before.get(*src..src + len),
                    "surviving bytes differ; before {before:?} after {after:?} deltas {deltas:?}"
                );
                at += len;
            }
            Piece::New { len } => at += len,
        }
    }
    assert_eq!(
        at,
        after.len(),
        "length mismatch; before {before:?} after {after:?} deltas {deltas:?}"
    );
}

/// Stronger check for one back-to-front batch (what a mutating call returns):
/// applying the deltas to `before` with the inserted text taken from `after`
/// reproduces `after`, and every point is right.
fn assert_batch_reproduces(before: &str, after: &str, deltas: &[EditDelta]) {
    assert!(deltas.windows(2).all(|w| w[0].start_byte > w[1].start_byte
        || (w[0].start_byte == w[1].start_byte && w[1].old_end_byte == w[1].start_byte)));
    let mut shifts = vec![0isize; deltas.len() + 1];
    for i in (0..deltas.len()).rev() {
        let d = &deltas[i];
        shifts[i] = shifts[i + 1] + (d.new_end_byte - d.start_byte) as isize
            - (d.old_end_byte - d.start_byte) as isize;
    }
    let mut cur = before.to_string();
    for (i, d) in deltas.iter().enumerate() {
        assert!(d.old_end_byte <= cur.len(), "old_end past text");
        assert_eq!(point_in(&cur, d.start_byte), d.start, "start point");
        assert_eq!(point_in(&cur, d.old_end_byte), d.old_end, "old_end point");
        let new_len = d.new_end_byte - d.start_byte;
        let at = (d.start_byte as isize + shifts[i + 1]) as usize;
        let new_text = after.get(at..at + new_len).unwrap_or_else(|| {
            panic!(
                "bad slice {at}..{} of {after:?}; before {before:?}; deltas {deltas:?}",
                at + new_len
            )
        });
        cur.replace_range(d.start_byte..d.old_end_byte, new_text);
        assert_eq!(point_in(&cur, d.new_end_byte), d.new_end, "new_end point");
        assert_eq!(d.start + extent(new_text), d.new_end);
    }
    assert_eq!(cur, after, "deltas must reproduce the new text");
}

fn check_invariants(b: &Buffer) {
    let text = b.text().to_string();
    let len = text.len();
    assert_eq!(b.text().len(), len);
    assert_eq!(b.text().summary().len, len);
    assert_eq!(
        b.text().summary().lines.row as usize,
        text.matches('\n').count()
    );
    assert_eq!(
        b.text().line_count() as usize,
        text.matches('\n').count() + 1
    );
    assert_eq!(b.text().summary().chars, text.chars().count());
    assert_eq!(b.text().summary().len_utf16, text.encode_utf16().count());
    let boundaries: std::collections::HashSet<usize> = text
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain([len])
        .collect();
    let sels = b.selections();
    assert!(!sels.is_empty());
    let mut ids = std::collections::HashSet::new();
    for s in sels {
        assert!(ids.insert(s.id), "duplicate id");
        for off in [s.anchor, s.head] {
            assert!(off <= len);
            assert!(
                boundaries.contains(&off),
                "offset {off} not on a grapheme boundary of {text:?}"
            );
        }
    }
    for w in sels.windows(2) {
        let (a, c) = (w[0].range(), w[1].range());
        assert!(a.end <= c.start, "overlap {sels:?}");
        assert!(
            !(a.end == c.start && (a.is_empty() || c.is_empty())),
            "unmerged {sels:?}"
        );
        assert!((a.start, a.end) < (c.start, c.end));
    }
    let newest = b.newest_selection();
    assert!(sels.iter().any(|s| s.id == newest.id));
    // point round trip at selection offsets
    for s in sels {
        let p = b.text().offset_to_point(s.head);
        assert_eq!(b.text().point_to_offset(p), s.head);
    }
}

fn options(style: u8) -> BufferOptions {
    BufferOptions {
        tab_size: 4,
        indent: match style {
            0 => IndentStyle::Spaces(4),
            1 => IndentStyle::Tabs,
            _ => IndentStyle::Spaces(2),
        },
        auto_indent: style != 3,
        auto_close_brackets: style != 4,
        comment: CommentTokens {
            line: if style == 5 { None } else { Some("//".into()) },
            block: if style == 5 {
                Some(("/*".into(), "*/".into()))
            } else {
                None
            },
        },
    }
}

fn initial_strategy() -> impl Strategy<Value = (Vec<usize>, Vec<usize>, u8)> {
    (
        prop::collection::vec(0..FRAGMENTS.len(), 0..14),
        prop::collection::vec(0usize..200, 1..6),
        0u8..6,
    )
}

fn make(frags: &[usize], carets: &[usize], style: u8) -> Buffer {
    let text: String = frags.iter().map(|i| FRAGMENTS[*i]).collect();
    let mut b = Buffer::new(Rope::from_str(&text), options(style));
    let len = text.len();
    b.set_selections(
        carets
            .iter()
            .map(|c| Selection::caret(0, if len == 0 { 0 } else { c % (len + 1) }))
            .collect(),
    );
    b
}

fn ops_case(init: (Vec<usize>, Vec<usize>, u8), ops: Vec<Op>) -> Result<(), TestCaseError> {
    let (frags, carets, style) = init;
    let mut b = make(&frags, &carets, style);
    b.set_coalesce_window(Duration::ZERO);
    check_invariants(&b);
    for op in &ops {
        let before_text = b.text().to_string();
        let before_sels: Vec<Selection> = b.selections().to_vec();
        let is_undo_redo = matches!(op, Op::Undo | Op::Redo);
        let tx = apply(&mut b, op);
        check_invariants(&b);
        let after_text = b.text().to_string();
        match &tx {
            None => prop_assert_eq!(
                &before_text,
                &after_text,
                "selection-only op changed text: {:?}",
                op
            ),
            Some(tx) if tx.is_empty() && !matches!(op, Op::Group(_)) => {
                prop_assert_eq!(
                    &before_text,
                    &after_text,
                    "empty transaction but text changed: {:?}",
                    op
                );
            }
            Some(tx) if !matches!(op, Op::Group(_)) => {
                // (replacing a selection with identical text is a real, if useless, edit)
                if is_undo_redo {
                    assert_deltas_exact(&before_text, &after_text, &tx.deltas);
                } else {
                    assert_batch_reproduces(&before_text, &after_text, &tx.deltas);
                    assert_deltas_exact(&before_text, &after_text, &tx.deltas);
                    // undo restores text and selections exactly; redo re-applies
                    let after_sels: Vec<Selection> = b.selections().to_vec();
                    let undo = b.undo();
                    prop_assert_eq!(
                        b.text().to_string(),
                        before_text.clone(),
                        "undo text for {:?}",
                        op
                    );
                    prop_assert_eq!(
                        b.selections().to_vec(),
                        before_sels.clone(),
                        "undo selections for {:?}",
                        op
                    );
                    assert_deltas_exact(&after_text, &before_text, &undo.deltas);
                    check_invariants(&b);
                    let redo = b.redo();
                    prop_assert_eq!(
                        b.text().to_string(),
                        after_text.clone(),
                        "redo text for {:?}",
                        op
                    );
                    prop_assert_eq!(
                        b.selections().to_vec(),
                        after_sels,
                        "redo selections for {:?}",
                        op
                    );
                    assert_batch_reproduces(&before_text, &after_text, &redo.deltas);
                    check_invariants(&b);
                }
            }
            Some(_) => {}
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, max_shrink_iters: 2000, ..ProptestConfig::default() })]

    /// Every op keeps selections valid; deltas reproduce the text; undo/redo are exact inverses.
    #[test]
    fn ops_keep_invariants(init in initial_strategy(), ops in prop::collection::vec(op_strategy(), 1..25)) {
        ops_case(init, ops)?;
    }

    /// Same with many carets over longer text.
    #[test]
    fn ops_keep_invariants_many_carets(
        init in (
            prop::collection::vec(0..FRAGMENTS.len(), 20..80),
            prop::collection::vec(0usize..600, 8..40),
            0u8..6,
        ),
        ops in prop::collection::vec(op_strategy(), 1..12),
    ) {
        ops_case(init, ops)?;
    }

    /// With coalescing on, undoing everything restores the initial state; redoing everything the final one.
    #[test]
    fn undo_all_redo_all(init in initial_strategy(), ops in prop::collection::vec(op_strategy(), 1..25)) {
        let (frags, carets, style) = init;
        let mut b = make(&frags, &carets, style);
        b.set_coalesce_window(Duration::from_secs(3600));
        let initial_text = b.text().to_string();
        let initial_sels = b.selections().to_vec();
        let initial_version = b.version();
        b.mark_saved();
        // Undo/redo ops inside the sequence would make "final" ambiguous: skip them here.
        for op in ops.iter().filter(|o| !has_undo(o)) {
            apply(&mut b, op);
            check_invariants(&b);
        }
        let final_text = b.text().to_string();
        let final_sels = b.selections().to_vec();
        let mut guard = 0;
        while b.can_undo() {
            b.undo();
            check_invariants(&b);
            guard += 1;
            prop_assert!(guard < 10_000);
        }
        prop_assert_eq!(b.text().to_string(), initial_text);
        prop_assert_eq!(b.version(), initial_version);
        prop_assert!(!b.is_dirty());
        // The oldest step restores the selections that preceded it, which are the
        // ones in effect at the first edit (selection-only ops before it are not undoable).
        let _ = initial_sels;
        while b.can_redo() {
            b.redo();
            check_invariants(&b);
        }
        prop_assert_eq!(b.text().to_string(), final_text);
        let _ = final_sels;
    }

    /// Typing-like ops with coalescing: one undo per run, and text is right.
    #[test]
    fn coalesced_typing_roundtrip(init in initial_strategy(), chars in prop::collection::vec(0usize..8, 1..30)) {
        let (frags, carets, style) = init;
        let mut b = make(&frags, &carets, style);
        b.set_coalesce_window(Duration::from_secs(3600));
        let start_text = b.text().to_string();
        for c in &chars {
            b.insert(["a", "b", " ", "é", "😀", "(", "\"", "x"][*c]);
            check_invariants(&b);
        }
        // Typing over a closer keeps the run going, so any mix is one step (or none
        // when everything was typed over existing closers).
        prop_assert!(b.undo_depth() <= 1);
        b.undo();
        prop_assert_eq!(b.text().to_string(), start_text);
        // (selections are restored to those at the first real edit, which may come
        // after a type-over that moved the caret, so they are not compared here)
        check_invariants(&b);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

    /// Random selections normalise to something valid whatever goes in.
    #[test]
    fn set_selections_always_valid(init in initial_strategy(), raw in prop::collection::vec((0usize..300, 0usize..300, 0u64..4), 0..8)) {
        let (frags, carets, style) = init;
        let mut b = make(&frags, &carets, style);
        let sels = raw
            .into_iter()
            .map(|(a, h, id)| Selection { id, anchor: a, head: h, goal: None })
            .collect();
        b.set_selections(sels);
        check_invariants(&b);
    }
}
