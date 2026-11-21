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

## Clarifications at implementation
Recorded by the architect when reviewing the implementation, before merge.

- **Done as moves.** All 20 files over the limit at 0.7.1 are under it; the `size` baseline is
  empty, so the rule is now a plain limit for every file. Test counts per crate are identical
  before and after; no public path changed (parents re-export what moved).
- **Visibility.** An item that moved to a child module and is used by its siblings is
  `pub(super)` (or `pub(in <the old parent>)` one level deeper): the same reach it had as a
  private item of the old file, never wider.
- **Left whole on purpose:** the completion controller (1,300 lines) and the code view state
  (1,171). Each is one unit; shrinking them needs a design change, not a move, and gets its
  own record if it is done.
- **`shadow.rs` keeps every public `ShadowRepo` method** because `arch-check writes` derives
  the write API from the `pub fn`s of named files. Until the rule scans a module's directory
  as well, public write functions of `lector-git` stay in the files the rule names.
