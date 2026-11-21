//! The agent's registry commands declare their undo (ADR 0002, point 3 and 5).
//!
//! `lector-app`'s `tests/undo_roundtrip.rs` enumerates the built-in registry, but cannot reach
//! the commands this crate registers (lector-agent depends on lector-app, not the other way
//! round). This test enumerates them: every agent command that writes must declare
//! `NotUndoable(reason)`; one that becomes undoable must get an undo round-trip scenario
//! here (state → command → journal undo → state) before this test lets it through.

use lector_app::commands::UndoPolicy;
use lector_app::{AppCore, CoreOptions};

/// Agent write commands whose undo is exercised by a round-trip scenario in this file.
const UNDO_SCENARIOS: &[&str] = &[];

#[test]
fn every_agent_write_command_declares_its_undo() {
    let core = AppCore::open(CoreOptions::for_tests());
    lector_agent::commands::register(&core);
    let agent: Vec<_> = core
        .registry()
        .list()
        .into_iter()
        .filter(|c| c.spec().id.starts_with("agent."))
        .collect();
    assert!(!agent.is_empty(), "the agent commands are registered");
    for command in agent {
        let spec = command.spec();
        if !spec.writes {
            assert_eq!(spec.undo, UndoPolicy::NothingToUndo, "{}", spec.id);
            continue;
        }
        match spec.undo {
            UndoPolicy::NotUndoable(reason) => {
                assert!(!reason.trim().is_empty(), "`{}`: empty reason", spec.id);
                assert!(!spec.undoable, "`{}`: undoable flag disagrees", spec.id);
            }
            UndoPolicy::Undoable => assert!(
                UNDO_SCENARIOS.contains(&spec.id),
                "undoable agent command `{}` has no undo round-trip scenario",
                spec.id
            ),
            other => panic!(
                "agent write command `{}` declares no undo: {other:?}",
                spec.id
            ),
        }
    }
}
