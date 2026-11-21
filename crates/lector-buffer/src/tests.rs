//! Focused unit tests. Fixtures use a tiny notation: `|` is a caret, `«…»` a
//! forward selection and `‹…›` a reversed one (head at `‹`).

use std::time::Duration;

use lector_text::Rope;

use crate::*;

fn options() -> BufferOptions {
    BufferOptions {
        comment: CommentTokens {
            line: Some("//".into()),
            block: None,
        },
        ..BufferOptions::default()
    }
}

fn buf_with(s: &str, o: BufferOptions) -> Buffer {
    let mut text = String::new();
    let mut sels = Vec::new();
    let mut open: Vec<(usize, bool)> = Vec::new();
    for ch in s.chars() {
        match ch {
            '|' => sels.push(Selection::caret(0, text.len())),
            '«' => open.push((text.len(), false)),
            '‹' => open.push((text.len(), true)),
            '»' | '›' => {
                let (start, rev) = open.pop().expect("unbalanced selection marker");
                sels.push(if rev {
                    Selection {
                        id: 0,
                        anchor: text.len(),
                        head: start,
                        goal: None,
                    }
                } else {
                    Selection {
                        id: 0,
                        anchor: start,
                        head: text.len(),
                        goal: None,
                    }
                });
            }
            c => text.push(c),
        }
    }
    let mut b = Buffer::new(Rope::from_str(&text), o);
    b.set_coalesce_window(Duration::ZERO);
    if !sels.is_empty() {
        // `«»` pairs are pushed when closed; restore text order.
        sels.sort_by_key(|s| s.range().start);
        b.set_selections(sels);
    }
    b
}

fn buf(s: &str) -> Buffer {
    buf_with(s, options())
}

fn show(b: &Buffer) -> String {
    let mut text = b.text().to_string();
    let mut marks: Vec<(usize, char)> = Vec::new();
    for s in b.selections() {
        if s.is_empty() {
            marks.push((s.head, '|'));
        } else if s.reversed() {
            marks.push((s.head, '‹'));
            marks.push((s.anchor, '›'));
        } else {
            marks.push((s.anchor, '«'));
            marks.push((s.head, '»'));
        }
    }
    marks.sort_by_key(|(o, c)| (*o, matches!(c, '«' | '‹')));
    for (o, c) in marks.into_iter().rev() {
        text.insert(o, c);
    }
    text
}

/// Runs `f` and returns the resulting notation, checking invariants on the way.
fn run(start: &str, f: impl FnOnce(&mut Buffer)) -> String {
    let mut b = buf(start);
    f(&mut b);
    check(&b);
    show(&b)
}

fn run_with(start: &str, o: BufferOptions, f: impl FnOnce(&mut Buffer)) -> String {
    let mut b = buf_with(start, o);
    f(&mut b);
    check(&b);
    show(&b)
}

fn check(b: &Buffer) {
    let len = b.text().len();
    let sels = b.selections();
    assert!(!sels.is_empty());
    let mut ids = std::collections::HashSet::new();
    for w in sels.windows(2) {
        let (a, c) = (w[0].range(), w[1].range());
        assert!(a.end <= c.start, "overlap: {sels:?}");
        assert!(
            a.start < c.start || (a.start == c.start && a.end < c.end),
            "unsorted {sels:?}"
        );
        assert!(
            !(a.end == c.start && (a.is_empty() || c.is_empty())),
            "unmerged touch {sels:?}"
        );
    }
    for s in sels {
        assert!(ids.insert(s.id), "dup id");
        assert!(s.anchor <= len && s.head <= len);
        let text = b.text().to_string();
        assert!(text.is_char_boundary(s.anchor) && text.is_char_boundary(s.head));
        assert!(crate::text_util::is_grapheme_boundary(b.text(), s.anchor));
        assert!(crate::text_util::is_grapheme_boundary(b.text(), s.head));
    }
}

impl Buffer {
    /// Test helper: replaces text and selections without history.
    fn set_text_for_test(&mut self, text: &str, sels: Vec<Selection>) {
        self.text = Rope::from_str(text);
        self.history = crate::history::History::new();
        self.history.window = Duration::ZERO;
        self.set_selections(sels);
    }
}

mod clipboard;
mod comments;
mod deletion;
mod external;
mod history;
mod indent;
mod lines;
mod misc;
mod motions;
mod polish;
mod select;
mod typing;
