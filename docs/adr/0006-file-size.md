# 0006 — Source files stay under 1,500 lines

## Problem
Four hand-written UI files are 2,000–2,700 lines (`code_view/mod.rs`, `code_view/element.rs`,
`editor/completion.rs`, `diff_view.rs`); several more are over 1,500. Reviews of these files
miss interactions, and parallel work collides in them.

## Decision
Hand-written `.rs` files are at most 1,500 lines (tests included when inline; generated files
exempt by path). Splits follow existing seams, not line counts: **model** (state and
transitions, no GPUI where possible) / **input** (actions, key and mouse handling) /
**render** (elements and painting). A split is a move: no behaviour change, no API change
outside the module.

## Checked by
`arch-check size` with a ratchet: files over the limit at 0.7.1 are listed with their current
size and may only shrink; new files may not exceed the limit.
