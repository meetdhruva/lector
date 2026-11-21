use std::collections::HashSet;

use lector_diff::DiffOptions;
use lector_git::{ChangeKind, FileChange, Side};
use lector_review::review::diff_bytes;
use lector_syntax::LanguageRegistry;

use super::*;

fn diff(old: &str, new: &str) -> FileDiff {
    let change = FileChange {
        old_path: Some("a.rs".into()),
        new_path: Some("a.rs".into()),
        kind: ChangeKind::Modified,
        old: Side::None,
        new: Side::None,
        binary: None,
    };
    diff_bytes(
        change,
        Some(old.as_bytes().to_vec()),
        Some(new.as_bytes().to_vec()),
        &LanguageRegistry::new(),
        &DiffOptions::default(),
        lector_app::content_cache::ContentCache::global(),
    )
}

fn thread(side: DiffSide, line: u32, state: ThreadState) -> Thread {
    let anchor = Anchor {
        path: "a.rs".into(),
        side,
        line,
        ..Default::default()
    };
    Thread {
        id: line as u64 + 1,
        anchor: anchor.clone(),
        original: anchor,
        state,
        comments: vec![lector_app::comments::Comment {
            body: "Why?".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn text(n: usize) -> String {
    (0..n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn gaps_follow_anchored_rows_in_both_layouts() {
    let old = text(40);
    let new = old.replace("line 20\n", "line twenty\n");
    let d = diff(&old, &new);
    let comments = DiffComments {
        threads: Rc::new(vec![
            thread(DiffSide::New, 20, ThreadState::Open),
            thread(DiffSide::Old, 20, ThreadState::Resolved),
            thread(DiffSide::New, 2, ThreadState::Open),
        ]),
        ..Default::default()
    };
    for unified in [false, true] {
        let segments = if unified {
            d.unified_segments(3)
        } else {
            d.split_segments(3)
        };
        // Line 2 is folded away: its fold opens.
        let open = comments.folds_with_threads(&segments, &d, unified);
        assert_eq!(open.len(), 1, "unified={unified}");
        let rows = crate::diff_source::display_rows(&segments, &open, false);
        let (rows, gaps) = comments.insert_gaps(rows, &d, unified, px(20.));
        assert_eq!(gaps.len(), 3);
        for g in &gaps {
            // Stacked threads of one row: walk back over gap rows.
            let anchor_row = rows[..g.first_row as usize]
                .iter()
                .rev()
                .find(|r| !matches!(r, DisplayRow::Gap(_)))
                .expect("a row before the gap");
            let t = &comments.threads[g.thread];
            let (side, line) = row_side_line(
                &d,
                anchor_row,
                match (unified, t.anchor.side) {
                    (true, _) => DiffColumn::Unified,
                    (false, DiffSide::Old) => DiffColumn::Old,
                    (false, DiffSide::New) => DiffColumn::New,
                },
            )
            .expect("anchored on a line");
            assert_eq!((side, line), (t.anchor.side, t.anchor.line));
            assert!(rows[g.first_row as usize..(g.first_row + g.rows) as usize]
                .iter()
                .all(|r| matches!(r, DisplayRow::Gap(_))));
        }
        // Collapsed (resolved) threads take less room than open ones.
        let resolved = gaps.iter().find(|g| g.thread == 1).map(|g| g.rows);
        let open_rows = gaps.iter().find(|g| g.thread == 0).map(|g| g.rows);
        assert!(resolved < open_rows);
        // Hunk starts ignore gaps.
        let starts = crate::diff_source::hunk_display_starts(&d, &rows, unified);
        assert_eq!(starts.len(), 1);
    }
    let none = DiffComments::default();
    let segments = d.split_segments(3);
    let rows = crate::diff_source::display_rows(&segments, &HashSet::new(), false);
    let n = rows.len();
    assert_eq!(none.insert_gaps(rows, &d, false, px(20.)).0.len(), n);
}

#[test]
fn long_bodies_need_more_rows() {
    let mut t = thread(DiffSide::New, 1, ThreadState::Open);
    let short = thread_rows(&t, true, px(20.), px(600.));
    t.comments[0].body = "word ".repeat(400);
    assert!(thread_rows(&t, true, px(20.), px(600.)) > short + 5);
    assert!(thread_rows(&t, false, px(20.), px(600.)) <= 2);
}
