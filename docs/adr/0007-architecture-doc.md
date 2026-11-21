# 0007 — The architecture document describes the present and is checked

## Problem
`docs/ARCHITECTURE.md` is 3,300 lines accreted per release ("v0.4 … as implemented"); its
layering diagram omits six crates, including `lector-app`. It cannot be read in one sitting
and cannot be trusted.

## Decision
- `docs/ARCHITECTURE.md` is rewritten as a description of the system **as it is**: principles,
  the layer diagram, one short section per crate (purpose, what it owns, what it must not do),
  the cross-cutting flows (a write, a background job, a credential prompt, an agent turn), and
  the security rules. Target: about 600 lines. No version history.
- Per-crate API detail lives in rustdoc, next to the code.
- Decisions and their reasons live in `docs/adr/`.
- The crate dependency diagram is **generated** (`scripts/arch-check graph`) and CI fails when
  the committed diagram differs from the real graph.

## Checked by
`arch-check graph --check`.
