# 0005 — One cancel token and one fuzzy matcher, in `lector-base`

## Problem
Three `CancelToken` types (`lector-process`, `lector-search`, `lector-lsp`) with bridging
helpers, and three fuzzy matchers (`lector-search` picker, `lector-lsp/src/fuzzy.rs`,
`lector-ui/src/fuzzy.rs`) with different ranking for the same query.

## Decision
A new crate `lector-base` with **no dependencies beyond std** (and `memchr` if the matcher
needs it), below every other crate:
- `CancelToken` (clonable flag, `is_cancelled`, `cancel`, child tokens, `check() -> Result`).
- `fuzzy`: one scorer (prefix, word-boundary/camelCase, case bonus, match positions) used by
  completion, symbol/recent/command pickers and as the scoring core of the file picker (the
  picker keeps its index, prefilter and top-k machinery).
- `thread::spawn_named` (ADR 0003).
The other definitions are deleted, not re-exported. Ranking differences are settled by a
shared table test of query → expected order.

## Rejected
- *Put them in `lector-text`*: process and search code would depend on the rope crate.

## Checked by
`arch-check dupes`: exactly one `struct CancelToken`; no `fn fuzzy`/matcher module outside
`lector-base` and the picker.

## Clarifications at implementation (step 5)
Recorded by the architect when reviewing the implementation, before merge.

- **The file picker keeps nucleo** as its scorer. Measured on 1M paths, the base scorer was
  1.4–5.8× slower per path and produced a different top-10 for 7 of 10 queries; the picker's
  pruning bounds and its property tests are built on nucleo's scores. The picker exposes no
  general fuzzy function, so there is still exactly one scorer other code can call.
- **Queries are whitespace-separated atoms, matched in any order** (every atom must match;
  scores add; positions are the union). A one-atom query scores exactly as before. The
  nucleo operator syntax (`^ $ ! '`), which only one picker had and no document described, is
  not carried over.
- **No ranking of large lists on the UI thread.** The decision is by work, not by count
  (cost is query length x candidate length): a ranking runs inline under a budget of 500,000
  work units (1.0-1.6 ms in the dev profile, `rank_by_budgeted`) and otherwise on the
  `lector-app` scheduler under one job key per picker, cancellable by work done
  (`rank_cancellable`), newest query wins (`lector-ui` `ListRanker`). Query atoms over 64
  characters are substring searches, at most 16 atoms count. Rows of an older query that are
  still on screen while a ranking is pending are never confirmed: an Enter waits for the new
  rows.
- **`CancelToken`** has `child()` (weakly held by the parent) and `on_cancel` hooks that run
  exactly once on the cancelling thread, never under the internal lock. Crates import
  `lector_base::CancelToken` directly; there are no aliases or re-exports.
- **`ThreadKind` lists 25 kinds today**, 16 owner threads and 9 short tasks that currently
  use a thread. Step 3 (ADR 0003) moves the short tasks to the scheduler and removes their
  kinds; only owner kinds remain.
