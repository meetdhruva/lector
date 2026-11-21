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
