# 0008 — Structural rules are enforced by a ratchet in CI

## Problem
Every rule above was already written down in some form and drifted anyway.

## Decision
`scripts/arch-check` (a small Rust binary in `crates/lector-archcheck`, no dependencies beyond
`serde_json` for `cargo metadata`) runs in CI on every pull request. Rules: `deps`, `writes`,
`threads`, `background`, `state`, `dupes`, `size`, `purity`, `hygiene` (`forbid(unsafe_code)`
present; no `unwrap`/`expect` outside tests on the listed I/O paths), `graph`.

**Ratchet, not report-only:** existing violations at 0.7.1 are recorded in `arch/baseline.toml`
(rule → file → count). CI fails when a count rises or a new file appears; a count that falls
must be lowered in the baseline in the same pull request (`arch-check --update` only ever
lowers). Each consolidation step ends with its rule's baseline empty, after which the rule is
absolute. Allowlists (`arch/*.allow`) are short, named and reviewed.

Separately in CI: `cargo deny` (advisories, licences, duplicate major versions reported).

## Rejected
- *Clippy lints / custom dylint*: heavier toolchain coupling than text and metadata checks
  need; these rules are about crate edges and call sites, which `cargo metadata` plus a
  tokenizer-level scan (comments and strings stripped, `#[cfg(test)]` regions skipped) answer.
