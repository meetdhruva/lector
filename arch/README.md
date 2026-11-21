# arch/ — architecture guardrails

`scripts/arch-check` (crate `crates/lector-archcheck`) enforces the structural rules from
[`docs/adr/`](../docs/adr/README.md) in CI. Violations that exist today are recorded in
`baseline.toml`; the check fails when anything gets worse, and also when something got better
and the baseline was not lowered, so the baseline always says exactly what is left to fix.
Design: [ADR 0008](../docs/adr/0008-guardrails.md).

## Running it

```
scripts/arch-check all                 # every rule + the graph check; exit 0 ok, 1 violations, 2 error
scripts/arch-check <rule>              # deps | writes | threads | background | state | dupes | size | purity | hygiene
scripts/arch-check all --verbose       # every violation, not only the new ones: `rule  file:line  message`
scripts/arch-check writes --list-api   # the lector-git write API the `writes` rule derived
scripts/arch-check graph               # print the crate graph
scripts/arch-check graph --write       # put it into docs/ARCHITECTURE.md between the markers
scripts/arch-check graph --check       # fail when the committed graph is stale
scripts/arch-check --update            # lower the baseline; refuses to raise anything
scripts/arch-check --update --moved <old>=<new>   # a file was renamed: carry its entries over
```

Run `scripts/arch-check all` before pushing. It takes a couple of seconds once built.
`cargo test -p lector-archcheck` runs the same check on this repository, so the baseline is
also protected by the test suite.

## What is in this directory

| File | What it is |
|---|---|
| `baseline.toml` | Today's violations: rule, then key, then count. Keys are source files, except `deps` (a crate edge such as `lector-ui-git -> lector-process`, or `crate <name>` for a crate an ADR removes) and `size` (file and its current line count). Written by `--update` only. |
| `layers.toml` | The crate layering (bottom to top), explicit `[[forbid]]` edges and `[[forbid_crate]]` entries from the ADRs, and the dependency-closure rules (no GPUI, window system or tokio under core crates; no GPUI under `lector-cli`/`lector-mcp`). |
| `threads.allow` | Files that may create threads (`thread::spawn`, `thread::Builder`, `spawn_scoped`), and the closed list of `kind:` names for `spawn_named` (ADR 0003). |
| `background.allow` | The one UI bridge module that may call `background_spawn` / `background_executor` (ADR 0003). |
| `state.allow` | Modules that may build paths under the data/config/state dir, and `literal:` file names that stay files (`settings.toml`, `keymap.toml`) (ADR 0004). |
| `generated.allow` | Generated files exempt from the 1,500-line limit (ADR 0006). |
| `review-statics.allow` | Statics, by name, that `lector-review` may declare (ADR 0001). |
| `git-read.allow` | Public `lector-git` methods that only read. `writes` derives the write API from the source of `crates/lector-git/src/{write,sync,branch,conflict,shadow}.rs` and subtracts this list, so a new `pub fn` there counts as a write until it is listed here. |
| `git-ambiguous.allow` | Write-API names that are also common method names (`push`); only flagged when the receiver's name contains `repo`, `git` or `shadow`. |
| `store-ambiguous.allow` | `lector-config` write-method names that are also common method names (`remove`, `set`, `clear`); `purity` flags them in `lector-review` only when the receiver's name contains `store`, `config`, `settings`, `session`, `recent` or `backend`. |
| `unsafe.allow` | `crate:` names exempt from `#![forbid(unsafe_code)]` (only `lector-text`); every `unsafe` block there needs a `// SAFETY:` comment. |

## Lowering the baseline

When you fix violations the check reports `STALE` and prints
`baseline is stale: run arch-check --update`. Run `scripts/arch-check --update` and commit
`baseline.toml` in the same pull request. `--update` never raises a number and never adds an entry:
if it would, it lists them and exits non-zero. **Never raise the baseline.** Fix the code, or write
an ADR that supersedes the rule. (`--update --allow-increase "<reason>"` exists for the one-time
creation of the baseline; it prints the reason and every raised entry.)

**Renaming or moving a file** that has baseline entries: run
`scripts/arch-check --update --moved crates/a/src/old.rs=crates/a/src/new.rs` (repeatable, one pair
per file, forward slashes). In every rule that ran, the entry of `old` becomes the entry of `new`
with the same count, and the usual comparison follows: if `new` has more violations (or, for
`size`, more lines) than `old` had, the update is refused like any other raise; if it has fewer,
the entry is lowered (and dropped at zero). An `old` with no entry in the rules that ran, or a
`new` that already has one, is an error (exit 2), so a typo cannot silently pass. Without
`--moved`, a rename looks like a new file and `--update` refuses it. The step that moves a file
and the step that fixes its violations stay separate.

When a rule's section is empty the rule is absolute; delete nothing, the empty section stays.

## Adding an allowance

An allowance is a deliberate exception that a reviewer must accept. Put it in the matching
`*.allow` file with a reason, either inline or in a comment line directly above it:

```
# The scheduler's own worker threads (ADR 0003).
crates/lector-app/src/scheduler.rs

crates/lector-ui/src/background.rs   # the one GPUI bridge (ADR 0003)
```

An entry without a reason is a configuration error (exit 2). Entries are path globs
(`*` stays inside a directory, `**` crosses directories) or item names, as the file says; typed
entries are written `tag:value` (`kind:pty-reader`, `literal:settings.toml`, `crate:lector-text`).
Keep the lists short. If you find yourself adding a file to `threads.allow` to make a feature
work, the feature belongs on the scheduler instead.

Changing a layer, a `[[forbid]]` entry or a closure list in `layers.toml` is a change to the
architecture: it needs an ADR, not just a reviewer.

## `purity`

`crates/lector-review` is stateless and writes nothing (ADR 0001, 0002), with no baseline: the
rule is absolute (0). In its non-test code it flags `static`/lazy state and detached threads, and
four kinds of write: (a) a call of the `lector-git` write API (the list `writes --list-api`
prints, same name-and-argument-count matching), (b) any `lector-config` store handle
(`ReviewStore`, `SessionStore`, `RecentProjects`, `ReviewBackend`: types derived from
`crates/lector-config/src` that have a writing method) or a call of a derived store-writing method
(`&mut self`, or named `save*`, `write*`, `set*`, `remove*`, `clear*`, `delete*`, `put*`,
`insert*`, `persist*`, `record*`, `add*`), (c) `std::process::Command`, `ProcessCommand`,
`Supervisor`, (d) `fs::write`/`remove_*`/`rename`/`copy`/`create_dir*`/`hard_link`/`symlink`/
`set_permissions` (as call, import or value), `File::create`/`create_new`/`options` and any
`OpenOptions`. Computing a patch is allowed; applying it is `lector-app`'s
(`review_service::apply`).

## What the rules can and cannot see

Rules read source through a small lexer: comments, strings and char literals are never matched,
and test code is skipped (`#[cfg(test)]` items, `#[test]` / `#[gpui::test]` functions, files under
`tests/`, `benches/`, `examples/`, `*_tests.rs`, `tests.rs`, `test_support*`, and files that a
`#[cfg(test)] mod x;` declaration points at). It does not expand macros or follow `#[path]`.
`writes`, `state`, `threads` and `background` are heuristics; each module's doc comment lists
what it cannot see. They are tuned to avoid false positives, so a clean result is evidence, not
proof.

## Adding a rule

A rule is `crates/lector-archcheck/src/rules/<name>.rs` with a pure `scan_file(path, text)`-style
core and unit tests on in-memory sources, a `check(...) -> RuleOutput`, an entry in `RULES`
(`src/lib.rs`) and in `RULE_ORDER` (`src/baseline.rs`). Create its baseline section with
`scripts/arch-check <rule> --update --allow-increase "initial baseline for <rule> (ADR nnnn)"`.
