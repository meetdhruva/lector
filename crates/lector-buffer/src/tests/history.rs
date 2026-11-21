//! Undo and redo, coalescing, dirty tracking, history bounds and the deltas of transactions.

use super::*;

fn coalescing(s: &str) -> Buffer {
    let mut b = buf(s);
    b.set_coalesce_window(Duration::from_secs(3600));
    b
}

#[test]
fn undo_redo_restores_text_and_selections() {
    let mut b = buf("a|b|c");
    b.insert("X");
    assert_eq!(show(&b), "aX|bX|c");
    let tx = b.undo();
    assert_eq!(show(&b), "a|b|c");
    assert_eq!(tx.deltas.len(), 2);
    let tx = b.redo();
    assert_eq!(show(&b), "aX|bX|c");
    assert_eq!(tx.deltas.len(), 2);
    assert!(
        b.undo().deltas.len() == 2 && b.undo().is_empty(),
        "nothing left to undo"
    );
    assert!(!b.can_undo());
    assert!(b.can_redo());
    b.redo();
    assert!(!b.can_redo());
    assert!(b.redo().is_empty());
}

#[test]
fn typing_coalesces() {
    let mut b = coalescing("|");
    for c in ["a", "b", "c"] {
        b.insert(c);
    }
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "|");
    b.redo();
    assert_eq!(show(&b), "abc|");
    // newline breaks the run (and is its own step)
    let mut b = coalescing("|");
    b.insert("a");
    b.newline();
    b.insert("b");
    b.insert("c");
    assert_eq!(b.undo_depth(), 3);
    b.undo();
    assert_eq!(show(&b), "a\n|");
    // a caret jump breaks the run
    let mut b = coalescing("|");
    b.insert("a");
    b.set_cursor(0);
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
    // moving the caret breaks the run
    let mut b = coalescing("|");
    b.insert("a");
    b.move_carets(Motion::Left, false);
    b.move_carets(Motion::Right, false);
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
    // paste breaks the run
    let mut b = coalescing("|");
    b.insert("a");
    b.paste("zz");
    b.insert("b");
    assert_eq!(b.undo_depth(), 3);
    // typing then backspace are different kinds
    let mut b = coalescing("|");
    b.insert("a");
    b.insert("b");
    b.backspace();
    b.backspace();
    assert_eq!(b.undo_depth(), 2);
    // window zero: nothing merges
    let mut b = buf("|");
    b.insert("a");
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
}

#[test]
fn backspace_and_delete_coalesce() {
    let mut b = coalescing("abc|");
    b.backspace();
    b.backspace();
    b.backspace();
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "abc|");
    let mut b = coalescing("|abc");
    b.delete();
    b.delete();
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "|abc");
}

#[test]
fn coalescing_respects_time_window() {
    let mut b = buf("|");
    b.set_coalesce_window(Duration::from_millis(50));
    b.insert("a");
    std::thread::sleep(Duration::from_millis(120));
    b.insert("b");
    assert_eq!(b.undo_depth(), 2);
    b.break_undo_group();
    b.set_coalesce_window(Duration::from_secs(10));
    b.insert("c");
    b.insert("d");
    b.break_undo_group();
    b.insert("e");
    assert_eq!(b.undo_depth(), 4);
}

#[test]
fn explicit_transactions_group_edits() {
    let mut b = buf("|");
    b.begin_transaction();
    b.insert("a");
    b.newline();
    b.insert("b");
    b.end_transaction();
    assert_eq!(b.undo_depth(), 1);
    b.undo();
    assert_eq!(show(&b), "|");
    b.redo();
    assert_eq!(show(&b), "a\nb|");
    // nested
    let mut b = buf("|");
    b.begin_transaction();
    b.insert("a");
    b.begin_transaction();
    b.insert("b");
    b.end_transaction();
    b.insert("c");
    b.end_transaction();
    b.insert("d");
    assert_eq!(b.undo_depth(), 2);
    // empty transaction creates nothing
    let mut b = buf("|");
    b.begin_transaction();
    b.end_transaction();
    assert_eq!(b.undo_depth(), 0);
    // a transaction does not merge into the previous coalescing run
    let mut b = coalescing("|");
    b.insert("a");
    b.begin_transaction();
    b.insert("b");
    b.end_transaction();
    b.insert("c");
    assert_eq!(b.undo_depth(), 2 + 1);
}

#[test]
fn new_edit_clears_redo() {
    let mut b = buf("|");
    b.insert("a");
    b.undo();
    assert!(b.can_redo());
    b.insert("b");
    assert!(!b.can_redo());
    assert_eq!(show(&b), "b|");
}

#[test]
fn undo_after_set_selections_restores_old_selection_state() {
    let mut b = buf("a|b");
    b.insert("X");
    b.set_cursor(0);
    b.undo();
    assert_eq!(show(&b), "a|b");
}

#[test]
fn dirty_tracking() {
    let mut b = buf("a|");
    assert!(!b.is_dirty());
    b.insert("b");
    assert!(b.is_dirty());
    b.undo();
    assert!(!b.is_dirty(), "undo back to the loaded state is clean");
    b.redo();
    b.mark_saved();
    assert!(!b.is_dirty());
    b.insert("c");
    assert!(b.is_dirty());
    b.undo();
    assert!(!b.is_dirty());
    b.undo();
    assert!(b.is_dirty(), "undo past the saved state is dirty");
    // selections only changes do not dirty
    let mut b = buf("abc|");
    b.move_carets(Motion::Left, false);
    b.select_all();
    assert!(!b.is_dirty());
}

#[test]
fn history_is_bounded() {
    let mut b = buf("|");
    for i in 0..10_050 {
        b.insert(if i % 2 == 0 { "a" } else { "b" });
    }
    assert_eq!(b.undo_depth(), 10_000);
    let mut n = 0;
    while b.can_undo() {
        b.undo();
        n += 1;
    }
    assert_eq!(n, 10_000);
    assert_eq!(b.text().len(), 50);
}

#[test]
fn history_memory_is_bounded_by_cost() {
    let mut b = buf("|");
    for _ in 0..200 {
        b.add_caret_below();
    }
    let many: Vec<Selection> = (0..2000).map(|i| Selection::caret(0, i)).collect();
    b.set_text_for_test(&"x".repeat(2000), many);
    for _ in 0..300 {
        b.insert("y");
    }
    assert!(
        b.undo_depth() < 300,
        "cost cap drops old steps with 2000 carets each"
    );
    assert!(b.undo_depth() > 20);
}

// ---- deltas -------------------------------------------------------------------------------------

#[test]
fn deltas_are_exact() {
    let mut b = buf("ab\ncd|\nef|");
    let tx = b.insert("é\n");
    // back to front: the later caret first
    assert_eq!(tx.deltas.len(), 2);
    let d0 = tx.deltas[0];
    assert_eq!(
        d0,
        EditDelta {
            start_byte: 8,
            old_end_byte: 8,
            new_end_byte: 8 + "é\n".len(),
            start: Point::new(2, 2),
            old_end: Point::new(2, 2),
            new_end: Point::new(3, 0),
        }
    );
    assert_eq!(tx.deltas[1].start, Point::new(1, 2));
    assert_eq!(tx.deltas[1].new_end, Point::new(2, 0));
}

#[test]
fn deltas_for_replacement_and_deletion() {
    let mut b = buf("a«b\nc»d");
    let tx = b.insert("xy");
    assert_eq!(
        tx.deltas,
        vec![EditDelta {
            start_byte: 1,
            old_end_byte: 4,
            new_end_byte: 3,
            start: Point::new(0, 1),
            old_end: Point::new(1, 1),
            new_end: Point::new(0, 3),
        }]
    );
    let tx = b.undo();
    assert_eq!(
        tx.deltas,
        vec![EditDelta {
            start_byte: 1,
            old_end_byte: 3,
            new_end_byte: 4,
            start: Point::new(0, 1),
            old_end: Point::new(0, 3),
            new_end: Point::new(1, 1),
        }]
    );
}

#[test]
fn no_change_means_empty_transaction() {
    let mut b = buf("|");
    let v = b.version();
    assert!(b.backspace().is_empty());
    assert!(b.delete().is_empty());
    assert!(b.outdent().is_empty());
    assert!(b.undo().is_empty());
    assert!(!b.is_dirty());
    assert_eq!(b.version(), v);
    assert_eq!(b.undo_depth(), 0);
}
