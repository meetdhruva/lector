# 0001 — One stateful app layer; review analysis is a stateless crate

## Problem
`lector-workspace` (0.1) and `lector-app` (0.4) both describe themselves as the UI-free app
model. `lector-app` depends on `lector-workspace`; the UI crates reference each 72 times. Risk
scoring and structural diff live in one, the review queue that consumes them in the other. A
reader cannot tell where new state or a new operation belongs, and features were wired through
whichever layer the author found first.

Measured at 0.7.1: `lector-workspace` is 10,257 lines, of which about 8,000 are pure analysis
(`risk` 3,552, `review` 1,753, `structural` 1,750, `conflicts` 889) and about 2,300 are the old
stateful model (`project`, `document`, `cache`, `symbols`, `navigation`).

## Decision
1. **`lector-app` is the only stateful layer.** It owns the open project, documents, git state,
   commands, journal, scheduling and persistence. Anything that holds state between calls,
   starts work, or writes lives there.
2. **`lector-review` is a new stateless crate below `lector-app`** holding the pure review
   analysis from `lector-workspace`: changeset loading and keys, file diffs, risk facts and
   scoring, structural diff, conflict parsing. Rule: functions of their inputs only — no
   globals, no caches that outlive a call, no threads, no I/O other than reading through the
   `lector-git` / `lector-fs` handles passed in. Cancellation by token.
3. **The old stateful modules move into `lector-app`** (`project`, `symbols`, `navigation`),
   or are deleted where `lector_app::documents` already replaced them (`document`, `cache` —
   verify each use before deleting; migrate remaining callers to the document store).
4. **`lector-workspace` is removed.** No re-export shim is left behind.

Allowed edges afterwards: `ui* → app → review → {git, diff, syntax, fs, text, search, lsp, config}`.
UI crates may use `lector-review` **types** (a `FileDiff`, a `RiskReason`) but obtain them
through `lector-app`.

## Rejected
- *Merge everything into `lector-app`*: 27,000 lines in one crate, and the pure analysis (the
  part worth reusing and benchmarking on its own) would gain access to app state.
- *Keep both and document the line*: the line has been documented since 0.4 and was not held.

## Checked by
`arch-check deps`: `lector-workspace` absent; `lector-review` depends on no stateful crate;
`arch-check purity`: no `static`, `thread::spawn`, `OnceLock`/`Lazy` state or file writes in
`lector-review` (regex and tree-sitter query tables in `OnceLock` are allowed by name).
No behaviour change: all existing tests pass with only import paths edited.

## Clarifications at implementation (step 1)
Recorded by the architect when reviewing the implementation, before merge.

- **Threads scoped to one call are allowed** in `lector-review` (`std::thread::scope`, rayon):
  they do not outlive the call, so the crate stays a function of its inputs. Detached threads
  remain forbidden (ADR 0003).
- **The seam is `lector_review::content::DerivedCache`** (memoized syntax trees and line diffs),
  not a blob provider: that is what the old cache actually held. `lector-app` implements it
  with `ContentCache`; `NoCache` is the uncached implementation.
- **UI crates may depend on `lector-review` directly** for its types and pure functions. A
  blanket re-export through `lector-app` would be the shim this record forbids.
- **Extra edges accepted:** `lector-review → lector-buffer` (the `TextEdit` value type used by
  conflict resolution) and, temporarily, `→ lector-config`.
- **Known debt, with an owner step.** These contradict "stateless" and are not accepted as the
  end state:
  - `apply_hunk_action*` writes to the repository through the handle passed in → becomes a
    registry command in step 2 (ADR 0002).
  - `set_viewed_with` and the `ReviewStore` it writes (`review.json`) → move behind the state
    store in step 4 (ADR 0004); `lector-review` then takes viewed-state as an input and drops
    its `lector-config` dependency for it.
  - UI code calling `lector-review` loaders that do I/O (`load_changeset_with`,
    `load_file_diff_with`, `file_vs_revision_with`, `load_vs_workdir_with`, `default_base`,
    `file_summary`) → routed through `lector-app` in step 2.
  Step 2 adds a `purity` sub-rule so that writes through handles are caught, not only direct
  file writes.
