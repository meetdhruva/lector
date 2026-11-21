//! Unit tests of the inline-edit model: hunks, accept/reject, edit mapping, prompts and availability.

use super::*;

fn apply(text: &str, edits: &[TextEdit]) -> String {
    let mut out = text.to_string();
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    for e in sorted {
        out.replace_range(e.range.clone(), &e.new_text);
    }
    out
}

fn running(old: &str, sel: Range<usize>) -> InlineEdit {
    let mut e = InlineEdit::new(InlineRequest::from_selection("a.rs", old, sel));
    e.turn_started("do it", TurnId(1));
    e
}

fn finished(old: &str, new: &str, sel: Range<usize>) -> InlineEdit {
    let mut e = running(old, sel);
    e.finish(FinishInput {
        state: TurnState::Completed("end_turn".into()),
        result_text: Some(new.into()),
        buffer_text: new.into(),
        changed_files: vec!["a.rs".into()],
    });
    e
}

#[test]
fn request_lines_from_selections() {
    let t = "one\ntwo\nthree\nfour\n";
    let r = |s: Range<usize>| InlineRequest::from_selection("f", t, s).lines;
    assert_eq!(r(5..5), 1..2, "caret = its line");
    assert_eq!(r(4..7), 1..2);
    assert_eq!(r(4..8), 1..2, "ending at the newline");
    assert_eq!(r(4..9), 1..3, "reaching into the next line");
    assert_eq!(
        r(0..t.len()),
        0..4,
        "ending at column 0 of the empty last line"
    );
    let req = InlineRequest::from_selection("f", t, 5..5);
    assert!(!req.from_selection);
    assert_eq!(req.selected_text(), "two\n");
    assert_eq!(req.range_label(), "line 2");
    assert_eq!(
        InlineRequest::from_selection("f", t, 0..10).range_label(),
        "lines 1-3"
    );
}

#[test]
fn hunks_of_a_simple_change() {
    let (old, new) = ("a\nb\nc\n", "a\nB\nc\nd\n");
    let hunks = compute_hunks(old, new);
    assert_eq!(hunks.len(), 2);
    assert_eq!(&old[hunks[0].old.clone()], "b\n");
    assert_eq!(&new[hunks[0].new.clone()], "B\n");
    assert_eq!(&old[hunks[1].old.clone()], "");
    assert_eq!(&new[hunks[1].new.clone()], "d\n");
    assert!(compute_hunks("same\n", "same\n").is_empty());
}

#[test]
fn missing_final_newline_is_a_change_of_the_last_line() {
    for (old, new) in [
        ("a\nb", "a\nb\n"),
        ("a\nb\n", "a\nb"),
        ("a\nb", "a\nb\nc"),
        ("", "\n"),
        ("\n", ""),
        ("a", "b"),
        ("a\n", "a\nb"),
    ] {
        let hunks = compute_hunks(old, new);
        let n = hunks.len();
        assert_eq!(
            splice(old, new, &hunks, &vec![false; n]),
            old,
            "{old:?} -> {new:?}"
        );
        assert_eq!(
            splice(old, new, &hunks, &vec![true; n]),
            new,
            "{old:?} -> {new:?}"
        );
    }
}

#[test]
fn every_combination_of_decisions_is_a_consistent_splice() {
    let old = "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n\nfn c() {}\n";
    let new = "fn a() {\n    uno();\n    extra();\n}\n\nfn b() {\n}\n\nfn c() {}\nfn d() {}\n";
    let hunks = compute_hunks(old, new);
    assert!(hunks.len() >= 3, "{hunks:?}");
    let n = hunks.len();
    for mask in 0..(1u32 << n) {
        let keep: Vec<bool> = (0..n).map(|i| mask & (1 << i) != 0).collect();
        let text = splice(old, new, &hunks, &keep);
        // Deciding through the model gives the same text.
        let mut e = finished(old, new, 0..old.len());
        let mut buffer = new.to_string();
        let ids: Vec<u32> = e.hunks().iter().map(|h| h.id).collect();
        for (id, keep) in ids.iter().zip(&keep) {
            if *keep {
                e.accept(*id).unwrap();
            } else {
                let edit = e.reject(*id).unwrap();
                buffer = apply(&buffer, &[edit]);
            }
        }
        assert_eq!(buffer, text, "mask {mask:b}");
        assert_eq!(e.current_text(), text);
        assert!(matches!(
            e.phase(),
            InlinePhase::Done(InlineOutcome::Resolved { .. })
        ));
    }
}

#[test]
fn accept_all_and_reject_all() {
    let (old, new) = ("a\nb\nc\nd\n", "a\nB\nc\nD\n");
    let mut e = finished(old, new, 1..2);
    assert_eq!(e.phase(), &InlinePhase::Reviewing);
    assert_eq!(e.pending_count(), 2);
    let edits = e.reject_all();
    assert_eq!(apply(new, &edits), old);
    assert_eq!(
        e.phase(),
        &InlinePhase::Done(InlineOutcome::Resolved { kept: 0, undone: 2 })
    );
    let mut e = finished(old, new, 1..2);
    assert_eq!(e.accept_all(), 2);
    assert_eq!(
        e.phase(),
        &InlinePhase::Done(InlineOutcome::Resolved { kept: 2, undone: 0 })
    );
    assert_eq!(e.current_text(), new);
    assert_eq!(e.accept(1), Err(InlineError::WrongPhase("finished")));
}

#[test]
fn hunks_move_with_edits_elsewhere_and_conflict_inside() {
    let old = "head\na\nb\nc\ntail\n";
    let new = "head\na\nB\nc\ntail\n";
    let mut e = finished(old, new, 5..6);
    let id = e.hunks()[0].id;
    assert_eq!(e.hunk_rows(&e.hunks()[0].clone()), 2..3);
    // Insert a line at the very top: the hunk is one row lower and its range shifted.
    let edited = format!("new\n{new}");
    let effect = e.user_edited(
        &[EditSpan {
            old: 0..0,
            new_len: 4,
        }],
        &edited,
    );
    assert_eq!(effect, EditEffect::None);
    assert_eq!(e.hunk_rows(&e.hunks()[0].clone()), 3..4);
    let edit = e.reject(id).unwrap();
    assert_eq!(apply(&edited, &[edit]), format!("new\n{old}"));

    // An edit inside a pending hunk ends the inline review.
    let mut e = finished(old, new, 5..6);
    let r = e.hunks()[0].new_range.clone();
    let edited = new.replacen('B', "BB", 1);
    let effect = e.user_edited(
        &[EditSpan {
            old: r.start..r.start + 1,
            new_len: 2,
        }],
        &edited,
    );
    assert_eq!(effect, EditEffect::Conflict(FallbackReason::EditedHunk));
    assert_eq!(
        e.phase(),
        &InlinePhase::Done(InlineOutcome::FellBack(FallbackReason::EditedHunk))
    );
}

#[test]
fn editing_the_selection_while_running_falls_back_at_the_end() {
    let old = "a\nb\nc\n";
    let mut e = running(old, 2..3);
    // Elsewhere: fine, but remembered.
    let effect = e.user_edited(
        &[EditSpan {
            old: 0..0,
            new_len: 1,
        }],
        "xa\nb\nc\n",
    );
    assert_eq!(effect, EditEffect::None);
    assert_eq!(e.working_rows(), 1..2);
    // Inside the selection.
    let effect = e.user_edited(
        &[EditSpan {
            old: 4..4,
            new_len: 1,
        }],
        "xa\nbz\nc\n",
    );
    assert_eq!(effect, EditEffect::SelectionEdited);
    let phase = e
        .finish(FinishInput {
            state: TurnState::Completed("end_turn".into()),
            result_text: Some("a\nB\nc\n".into()),
            buffer_text: "a\nB\nc\n".into(),
            changed_files: vec!["a.rs".into()],
        })
        .clone();
    assert_eq!(
        phase,
        InlinePhase::Done(InlineOutcome::FellBack(FallbackReason::EditedSelection))
    );
}

#[test]
fn edits_elsewhere_while_running_add_a_warning() {
    let old = "a\nb\nc\n";
    let mut e = running(old, 2..3);
    e.user_edited(
        &[EditSpan {
            old: 0..0,
            new_len: 1,
        }],
        "xa\nb\nc\n",
    );
    e.finish(FinishInput {
        state: TurnState::Completed("end_turn".into()),
        result_text: Some("a\nB\nc\n".into()),
        buffer_text: "a\nB\nc\n".into(),
        changed_files: vec!["a.rs".into()],
    });
    assert_eq!(e.phase(), &InlinePhase::Reviewing);
    assert!(e.notes().contains(&InlineNote::EditedWhileWorking));
}

#[test]
fn buffer_typed_after_the_write_is_mapped_or_conflicts() {
    let old = "a\nb\nc\nd\n";
    let result = "a\nB\nc\nd\n";
    // The user typed on the last line after the agent's write: hunks stay put.
    let mut e = running(old, 2..3);
    e.finish(FinishInput {
        state: TurnState::Completed("end_turn".into()),
        result_text: Some(result.into()),
        buffer_text: "a\nB\nc\nd!\n".into(),
        changed_files: vec!["a.rs".into()],
    });
    assert_eq!(e.phase(), &InlinePhase::Reviewing);
    let edit = e.reject(e.hunks()[0].id).unwrap();
    assert_eq!(apply("a\nB\nc\nd!\n", &[edit]), "a\nb\nc\nd!\n");
    // Typed inside the changed line: conflict.
    let mut e = running(old, 2..3);
    let phase = e
        .finish(FinishInput {
            state: TurnState::Completed("end_turn".into()),
            result_text: Some(result.into()),
            buffer_text: "a\nB!\nc\nd\n".into(),
            changed_files: vec!["a.rs".into()],
        })
        .clone();
    assert_eq!(
        phase,
        InlinePhase::Done(InlineOutcome::FellBack(FallbackReason::BufferChanged))
    );
}

#[test]
fn other_files_and_outside_hunks_are_reported() {
    let old = (0..60).map(|i| format!("line {i}\n")).collect::<String>();
    let new = old
        .replace("line 2\n", "LINE 2\n")
        .replace("line 50\n", "LINE 50\n");
    let start = old.find("line 2\n").unwrap();
    let mut e = running(&old, start..start + 7);
    e.finish(FinishInput {
        state: TurnState::Completed("end_turn".into()),
        result_text: Some(new.clone()),
        buffer_text: new,
        changed_files: vec!["a.rs".into(), "b.rs".into(), "c/d.rs".into()],
    });
    assert_eq!(e.other_files(), ["b.rs".to_string(), "c/d.rs".to_string()]);
    assert_eq!(
        e.other_files_notice().as_deref(),
        Some("The agent also changed 2 other files")
    );
    assert_eq!(e.hunks().len(), 2);
    assert!(!e.hunks()[0].outside_range && e.hunks()[1].outside_range);
    assert!(e.notes().contains(&InlineNote::OutsideSelection(1)));
}

#[test]
fn no_changes_cancel_and_failure() {
    let old = "a\nb\n";
    let end = |state: TurnState, result: &str, changed: Vec<String>| {
        let mut e = running(old, 0..2);
        e.finish(FinishInput {
            state,
            result_text: Some(result.into()),
            buffer_text: result.into(),
            changed_files: changed,
        });
        e
    };
    let e = end(TurnState::Completed("end_turn".into()), old, vec![]);
    assert_eq!(e.phase(), &InlinePhase::Done(InlineOutcome::NoChanges));
    let e = end(TurnState::Cancelled, old, vec![]);
    assert_eq!(e.phase(), &InlinePhase::Cancelled);
    let e = end(TurnState::Failed("boom".into()), old, vec![]);
    assert_eq!(e.phase(), &InlinePhase::Failed("boom".into()));
    // Cancelled or failed after the agent already wrote: the change is shown, with a note.
    let e = end(TurnState::Cancelled, "a\nB\n", vec!["a.rs".into()]);
    assert_eq!(e.phase(), &InlinePhase::Reviewing);
    assert!(e.notes().contains(&InlineNote::CancelledWithChanges));
    let e = end(TurnState::Failed("x".into()), "a\nB\n", vec!["a.rs".into()]);
    assert!(matches!(e.notes()[0], InlineNote::FailedWithChanges(_)));
    // Changes in other files only: no hunks, the notice is kept.
    let e = end(
        TurnState::Completed("end_turn".into()),
        old,
        vec!["other.rs".into()],
    );
    assert_eq!(e.phase(), &InlinePhase::Done(InlineOutcome::NoChanges));
    assert_eq!(e.other_files().len(), 1);
}

#[test]
fn prompting_transitions() {
    let mut e = InlineEdit::new(InlineRequest::from_selection("src/a.rs", "x\ny\n", 0..1));
    assert_eq!(e.phase(), &InlinePhase::Prompting);
    assert!(e.is_active());
    assert_eq!(e.submit("   "), Err(InlineError::EmptyInstruction));
    let p = e.submit("rename x to z").unwrap();
    assert!(p.text.contains("`src/a.rs`, line 1"));
    assert!(p.text.ends_with("Instruction:\nrename x to z"));
    assert!(matches!(
        p.context[0],
        PromptContext::Range {
            start: 0,
            end: 1,
            ..
        }
    ));
    assert_eq!(p.context.len(), 2, "selection + surroundings");
    assert!(e.cancel_prompt());
    assert_eq!(e.phase(), &InlinePhase::Cancelled);
    assert!(!e.is_active());
    assert_eq!(e.submit("again"), Err(InlineError::WrongPhase("cancelled")));
    assert!(!e.cancel_prompt());
    let mut e = running("x\n", 0..1);
    assert!(e.needs_turn_cancel());
    assert_eq!(e.reject(1), Err(InlineError::WrongPhase("running")));
    assert!(!e.cancel_prompt());
}

#[test]
fn follow_up_replaces_pending_hunks_and_keeps_decided_ones() {
    let old = "a\nb\nc\nd\n";
    let new = "A\nb\nc\nD\n";
    let mut e = finished(old, new, 0..8);
    let first = e.hunks()[0].id;
    e.accept(first).unwrap();
    let prompt = e.prepare_follow_up("make D lowercase again").unwrap();
    assert!(prompt
        .text
        .starts_with("Refine your previous inline edit of `a.rs`"));
    e.follow_up_started("make D lowercase again", TurnId(2));
    assert_eq!(e.phase(), &InlinePhase::Running { turn: TurnId(2) });
    assert_eq!(e.turns(), [TurnId(1), TurnId(2)]);
    // Baseline: the accepted hunk is part of it, the pending one reverted.
    let result = "A\nb\nc\nd2\n";
    e.finish(FinishInput {
        state: TurnState::Completed("end_turn".into()),
        result_text: Some(result.into()),
        buffer_text: result.into(),
        changed_files: vec!["a.rs".into()],
    });
    assert_eq!(e.phase(), &InlinePhase::Reviewing);
    let pending: Vec<_> = e.pending().collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].old_text, "d\n");
    let edits = e.reject_all();
    assert_eq!(apply(result, &edits), "A\nb\nc\nd\n");
    assert_eq!(
        e.prepare_follow_up("x"),
        Err(InlineError::WrongPhase("finished"))
    );
}

#[test]
fn availability_checks_in_order() {
    let ok = AvailabilityInput {
        is_text_file: true,
        agent_configured: true,
        ..Default::default()
    };
    assert_eq!(availability(&ok), Ok(()));
    let bad = |f: fn(&mut AvailabilityInput)| {
        let mut i = ok;
        f(&mut i);
        availability(&i)
    };
    assert_eq!(
        bad(|i| i.is_text_file = false),
        Err(Unavailable::NotEditable)
    );
    assert_eq!(bad(|i| i.read_only = true), Err(Unavailable::ReadOnly));
    assert_eq!(
        bad(|i| i.agent_configured = false),
        Err(Unavailable::NoAgent)
    );
    assert_eq!(
        bad(|i| i.has_unresolved_edit = true),
        Err(Unavailable::Unresolved)
    );
    assert_eq!(
        bad(|i| i.session_busy = true),
        Err(Unavailable::SessionBusy)
    );
    // A read-only tab is reported before a missing agent.
    let mut i = ok;
    i.read_only = true;
    i.agent_configured = false;
    assert_eq!(availability(&i), Err(Unavailable::ReadOnly));
    for u in [
        Unavailable::NotEditable,
        Unavailable::ReadOnly,
        Unavailable::NoAgent,
        Unavailable::Unresolved,
        Unavailable::SessionBusy,
    ] {
        assert!(!u.reason().is_empty());
    }
}

#[test]
fn status_line_prefers_permissions_then_tools_then_text() {
    use crate::model::{AgentMessage, MessageKind, ToolCall};
    let mut t = Turn::new(TurnId(1), "p".into(), vec![]);
    assert_eq!(status_line(&t), "Taking a checkpoint…");
    t.state = TurnState::Running;
    assert_eq!(status_line(&t), "Working…");
    t.messages.push(AgentMessage {
        kind: MessageKind::Agent,
        text: "Looking at it\nRenaming the variable".into(),
    });
    assert_eq!(status_line(&t), "Renaming the variable");
    t.tool_calls.push(ToolCall {
        title: "Edit src/a.rs".into(),
        ..Default::default()
    });
    assert_eq!(status_line(&t), "Edit src/a.rs");
}

mod props {
    use super::*;
    use proptest::prelude::*;

    fn text() -> impl Strategy<Value = String> {
        // Few distinct lines so the texts share structure; optional final newline.
        (
            proptest::collection::vec(
                prop::sample::select(vec!["a", "b", "c", "", "dd", "a b"]),
                0..9,
            ),
            any::<bool>(),
        )
            .prop_map(|(lines, nl)| {
                let mut s = lines.join("\n");
                if nl && !lines.is_empty() {
                    s.push('\n');
                }
                s
            })
    }

    proptest! {
        #[test]
        fn all_accepted_is_new_all_rejected_is_old_any_mix_is_a_splice(
            old in text(), new in text(), mask in any::<u16>()
        ) {
            let hunks = compute_hunks(&old, &new);
            let n = hunks.len();
            prop_assert_eq!(splice(&old, &new, &hunks, &vec![true; n]), new.clone());
            prop_assert_eq!(splice(&old, &new, &hunks, &vec![false; n]), old.clone());
            // Gaps between hunks are identical in both texts.
            let (mut o_at, mut n_at) = (0, 0);
            for h in &hunks {
                prop_assert_eq!(&old[o_at..h.old.start], &new[n_at..h.new.start]);
                o_at = h.old.end;
                n_at = h.new.end;
            }
            prop_assert_eq!(&old[o_at..], &new[n_at..]);
            // The model gives the same text for an arbitrary mix of decisions.
            if n > 0 {
                let keep: Vec<bool> = (0..n).map(|i| mask & (1 << (i % 16)) != 0).collect();
                let expect = splice(&old, &new, &hunks, &keep);
                let mut e = finished(&old, &new, 0..old.len());
                let mut buffer = new.clone();
                let ids: Vec<u32> = e.hunks().iter().map(|h| h.id).collect();
                for (id, keep) in ids.iter().zip(&keep) {
                    if *keep {
                        e.accept(*id).unwrap();
                    } else {
                        let edit = e.reject(*id).unwrap();
                        buffer = apply(&buffer, &[edit]);
                    }
                }
                prop_assert_eq!(buffer, expect);
            }
        }

        #[test]
        fn reject_all_edits_restore_the_old_text(old in text(), new in text()) {
            let mut e = finished(&old, &new, 0..old.len());
            if old != new {
                let edits = e.reject_all();
                prop_assert_eq!(apply(&new, &edits), old);
            }
        }
    }
}
