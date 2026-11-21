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

Run `scripts/arch-check all` before pushing. It takes about three seconds once built (it reads `cargo metadata` for `deps` and `background`).
`cargo test -p lector-archcheck` runs the same check on this repository, so the baseline is
also protected by the test suite.

## What is in this directory

| File | What it is |
|---|---|
| `baseline.toml` | Today's violations: rule, then key, then count. Keys are source files, except `deps` (a crate edge such as `lector-ui-git -> lector-process`, or `crate <name>` for a crate an ADR removes) and `size` (file and its current line count). Written by `--update` only. |
| `layers.toml` | The crate layering (bottom to top), explicit `[[forbid]]` edges and `[[forbid_crate]]` entries from the ADRs, and the dependency-closure rules (no GPUI, window system or tokio under core crates; no GPUI under `lector-cli`/`lector-mcp`). |
| `threads.allow` | Files that may create threads (`thread::spawn`, `thread::Builder`, `spawn_scoped`), the closed list of `kind:` names for `spawn_named` (ADR 0003), and `test-only:` kinds with the `test-file:` globs that may spawn them. |
| `background.allow` | The one UI bridge file that may name or hold a GPUI executor (`BackgroundExecutor`, `ForegroundExecutor`) or call `background_spawn` / `background_executor` (ADR 0003). |
| `state.allow` | Single files that may create files under the data/config/state dirs (never directories), and `literal:` file names that stay files (`settings.toml`, `keymap.toml`) (ADR 0004). |
| `generated.allow` | Generated files exempt from the 1,500-line limit (ADR 0006). |
| `review-statics.allow` | `NAME: Type` pairs of the statics `lector-review` may declare (ADR 0001); name and type must both match. |
| `git-read.allow` | The classification of the public surface of `lector-git`: `handle:Type` (a type whose `pub fn`s can act on a repository), `value:Type` (data only), and every read as `Type::name`, `free::name` or a bare `name`. Everything not listed is a write (ADR 0002, ADR 0010 item 8). |
| `git-callers.allow` | Files outside `lector-git` that may call the write API: the `lector-app` command modules (`commands/**`). |
| `git-ambiguous.allow` | Write-API names that are also common method names (`push`); only flagged when the receiver's name contains `repo`, `git` or `shadow`. |
| `fs-writers.allow` | Files of the UI crates, `lector-mcp`, `lector-cli` and `lector-agent` that write files themselves. Today: the agent's `fs/write_text_file` (ADR 0010 "known gaps"). |
| `store-ambiguous.allow` | `lector-config` write-method names that are also common method names (`remove`, `set`, `clear`); `purity` flags them in `lector-review` only when the receiver's name contains `store`, `config`, `settings`, `session`, `recent` or `backend`. |
| `one-impl.toml` | The concepts that have one implementation only (`dupes`): name, ADR, owner globs, name patterns, reviewed `delegate` wrappers. |
| `unsafe.allow` | `crate:` names exempt from `#![forbid(unsafe_code)]` (only `lector-text`); every `unsafe` block there needs a `// SAFETY:` comment. |

## Lowering the baseline

When you fix violations the check reports `STALE` and prints
`baseline is stale: run arch-check --update`. Run `scripts/arch-check --update` and commit
`baseline.toml` in the same pull request. `--update` never raises a number and never adds an entry:
if it would, it lists them and exits non-zero. **Never raise the baseline.** Fix the code, or write
an ADR that supersedes the rule. (`--update --allow-increase "<reason>"` exists for the one-time
creation of the baseline and for a block of findings that a *stronger rule* brings to light in
unchanged code. Put such a block in its own commit with a comment above it, `RULE CHANGED, NOT
CODE`, as the `[writes]`, `[state]` and `[dupes]` blocks of step 8 do: it prints the reason and
every raised entry.)

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
**An allowance that exempts nothing is an error** (`stale allowance ...: it exempts nothing any
more; remove it`): delete it when the code it covered is gone. A file entry counts as used when it
silences at least one finding; a `kind:` entry when non-test code names the kind; a
`NAME: Type` entry when that static exists. Keep the lists short. If you find yourself adding a file to `threads.allow` to make a feature
work, the feature belongs on the scheduler instead.

Changing a layer, a `[[forbid]]` entry or a closure list in `layers.toml` is a change to the
architecture: it needs an ADR, not just a reviewer.

## The rules

All of `threads`, `writes`, `background`, `state` and `purity` resolve names the same way (see
below), so renaming an import does not hide a call. Each rule's module doc in
`crates/lector-archcheck/src/rules/` is the exact list; this is the summary.

- **`deps`**: crate edges against `layers.toml` and the dependency closure (no GPUI/tokio under core).
- **`threads`** (ADR 0003): `thread::spawn`, `thread::Builder`, `.spawn_scoped(` outside
  `threads.allow`, and `spawn_named(kind, ..)` with a kind not on the closed list. A `test-only:`
  kind (the mock forge server) may only be spawned from its `test-file:` globs.
- **`background`** (ADR 0003): a GPUI executor type *named or stored* (`BackgroundExecutor`,
  `ForegroundExecutor`: a handle that can spawn), `background_spawn(` / `background_executor(`
  in `lector-ui*` (`.timer(..)` is exempt), any path into another executor crate (`tokio`, `smol`,
  `async_std`, `async_executor`, `async_global_executor`, `blocking`, `futures_executor`,
  `threadpool`, `scoped_threadpool`), a `ThreadPool`/`LocalPool`/`LocalSpawner`, and a workspace
  crate with a direct (normal or build) dependency on an executor crate (read from
  `cargo metadata`). **rayon** is allowed as a dependency (`lector-text` uses it for data
  parallelism); `rayon::scope` and `par_iter` end before they return, but `rayon::spawn`,
  `spawn_fifo` and `ThreadPoolBuilder` are flagged (detached work, global pool). Only the bridge
  file in `background.allow` may hold GPUI executors.
- **`writes`** (ADR 0002, ADR 0010 item 8):
  - *The write API is everything in `lector-git` that is not a classified read.* All of
    `crates/lector-git/src/**` is scanned: every plain `pub fn` of a `handle:` type (methods,
    associated functions, trait impl methods) and every `pub fn` free function is a write unless
    `git-read.allow` lists it. A `pub fn` added to a new file is therefore a write until a
    reviewer reads it. An inherent `impl` with `pub fn`s on a type that is neither `handle:` nor
    `value:` is reported, so a new handle type cannot slip in. `writes --list-api` prints the
    derived API.
  - *Callers*: in every crate except `lector-git` itself, a call of the API (method, `Type::f`,
    `lector_git::f`, through `use .. as ..` and `type Alias = Repo;`) is reported unless the file is
    in `git-callers.allow`. `lector-app` command modules are the only entries: every function
    there is a registry command (ADR 0002), where sources, permission, journal and undo are
    decided. A plain `pub fn` of `lector-app` outside `commands/` that runs a write (directly or
    through a private helper of the same file) is a derived *wrapper* and counts as API
    (`apply_hunk_action`, `undo`, `undo_last` today).
  - *Processes*: `ProcessCommand`, `Supervisor`, `std::process::Command` in the UI crates.
  - *Files*: `fs::write`/`rename`/`copy`/`remove_*`/`create_dir*`/..., `File::create*`,
    `OpenOptions`, `lector_fs::write_atomic` and the confined write of `lector_fs::confine`
    (named by its `CreateDirs` argument) in the UI crates, `lector-mcp`, `lector-cli` and
    `lector-agent`, unless the file is in `fs-writers.allow`. The agent's entries are counted and
    printed so they stay visible.
  - *Report-only*: a plain `pub fn` outside `lector-git` returning `&Repo`, `Arc<Repo>`,
    `Result<Repo, ..>` (or the `ShadowRepo` forms) hands every caller the whole write API. Each is
    counted in the baseline with a `[report-only]` message; the fix is a read-only view type.
- **`state`** (ADR 0004): a file or directory *created* under a path derived from `data_dir()`,
  `config_dir()`, `state_dir()` or any other `PathBuf`-returning `pub fn` of
  `lector-config/src/paths.rs`, whatever the extension; the path is followed through `let`
  bindings, `push`, `join`, `with_file_name`, `with_extension`, `set_file_name` and `format!`
  names. Also a state-file extension (`.json`, `.redb`, `.toml`, `.ron`, `.bin`, ...) in a
  `.join(..)` next to such a path, and a per-key file name (`format!("{}.json", key)`).
  `state.allow` lists single files, not directories.
- **`dupes`** (ADR 0005, 0010): `one-impl.toml` declares concepts that have one implementation: a
  definition (`struct`/`enum`, `fn`, `mod`/file name) matching a name pattern outside the owner's
  files is reported. Concepts today: cancel token, fuzzy scorer, path confinement (`confine*`,
  `resolve_in_root`, `*_in_root`), credential redaction, debounce. A thin wrapper of the owner is
  listed as a `delegate` with a reason (the six wrappers around `lector_fs::Confinement` today).
  The rule passes once `lector_fs::confine` is the only implementation and its wrappers are listed.
- **`size`**, **`hygiene`**, **`graph`**: unchanged (1,500 lines; `forbid(unsafe_code)`, `// SAFETY:`;
  the committed crate graph).
- **`purity`** (ADR 0001, 0002): `crates/lector-review` is stateless and writes nothing; the rule is
  absolute (0). In its non-test code it flags every `static` (including `static mut`, `OnceLock`,
  `LazyLock`, `thread_local!`, `Mutex`, `RwLock`, atomics, `Cell`) unless `review-statics.allow`
  has the entry `NAME: Type` (both must match, so a different type under an allowed name is
  caught), detached threads, and four kinds of write: the `lector-git` write API (same
  name-and-argument-count matching as `writes`), any `lector-config` store handle or
  store-writing method, `std::process::Command`/`ProcessCommand`/`Supervisor`, and any file write
  or `OpenOptions`. Computing a patch is allowed; applying it is `lector-app`'s
  (`review_service::apply`).

## Name resolution and its noise

`crates/lector-archcheck/src/alias.rs` resolves, per file: `use` trees (`as`, groups, `self`,
nested groups, glob imports, `extern crate .. as ..`), `type X = Y;` aliases, names the file
defines itself, and a workspace-wide table of `pub use` re-exports and `pub type` aliases (three
passes, so a chain of up to three re-exports resolves). A path is matched against what it
resolves to, so `use std::thread as t; t::spawn(..)` is a `thread::spawn`.

When a path *cannot* be resolved, may come from a glob import of an internal or std module, and
its last segment looks like the target (`spawn`, `Command`, `write_atomic`, `OpenOptions`, an
executor type), the rule **fails closed**: it reports a guess (the message says "name cannot be
resolved ... treated as a hit"). Guesses need a first segment that is a target name, or a lowercase
module followed by a target last segment, and a bare name must be called, so a local variable named
`spawn` is not a hit. On this repository the guesses report **0 findings** (before the refinements
the first version reported 10, all of them local `spawn` parameters and local `Terminal::spawn` /
`Builder` types).

## How a rule can be fooled

The rules read tokens, not types. A clean result is evidence, not proof. Known ways around them:

- **Macros that expand to a call**: `macro_rules! go { () => { std::thread::spawn(..) } }` is
  matched where the macro is *defined* (its body is tokens), not where it is invoked, and a macro
  that builds the path from pieces is not seen at all.
- **`include!` and generated code** (`OUT_DIR`, build scripts, derive output): the included text is
  not scanned.
- **Function pointers and stored paths**: `let f = std::thread::spawn;` is seen (a path), but
  `let f = other_crate::reexported_spawn;` or a pointer passed through a generic parameter is not;
  `let w = std::fs::write; w(p, b)` is seen at the first line only.
- **A path passed to a helper**: `state` and `writes` flag the sink they can see. A helper
  `fn ensure_file(path, text)` that writes its parameter is flagged in its own file; the caller that
  chose `data_dir().join("x.json")` is only flagged if the helper is not exempt and the path is
  built in the same function as the sink.
- **A re-export chain longer than three hops**, or a re-export that renames through a macro.
- **Writes that are not file calls**: `DocumentHandle::save`, `.write_all` on a file opened
  elsewhere, `tracing_appender` choosing its own file, a library persisting by itself. The
  agent's save goes through `lector_fs::confine`'s write, which is found by `CreateDirs`.
- **Traits and generics**: a write through a trait object or a generic bound, a handle hidden
  behind a wrapper of another crate that forwards under another name (only `lector-app` wrappers are
  derived), and a wrapper of a wrapper in another file.
- **Names**: `dupes` and the write API match by name; an implementation with an unrelated name
  (`score_subsequence`) or inside the owner's own files is not seen. A function that happens to
  have the name and argument count of a write is flagged (`writes` limits the common ones to
  receivers named like a repository).
- **`ShadowRepo::open`** is classified a read (it opens Lector's own shadow store under the data
  dir; `restore_*`, `snapshot_*`, `fetch*` and `prune*` stay writes). If `open` ever changes the
  user's repository, move it out of `git-read.allow`.
- **Test regions** are detected from `#[cfg(test)]`, `#[test]` and file names, and fail closed (an
  unclear region counts as non-test), but a `#[cfg(feature = "test-support")]` item in a library is
  production code to the rules (it needs an allowance, like the `lector-ui` bridge's test
  scheduler).

## How to add a classification

A new `pub fn` in `lector-git` fails `writes` for every caller outside `commands/` until it is
classified. Read its body. If it does not change files, the index, refs, stashes, config or
remotes of the user's repository, add it to `git-read.allow` (`Repo::name`, `ShadowRepo::name` or
`free::name`, with a reason comment). If it writes, leave it out: it is a write, and callers go
through a `lector-app` command. A new type with inherent `pub fn`s needs `handle:Type` (it can
act on a repository) or `value:Type` (it only computes from its own fields). An entry for a
function that no longer exists is an error.

To add a concept to `dupes`, add a `[[concept]]` to `one-impl.toml` (see the header there). To
add a reviewed wrapper, add a `delegate = [{ file, fn, why }]` entry; to exempt a caller of the
write API, a file in `git-callers.allow`; to exempt a direct file writer, `fs-writers.allow` with
a reason (the agent's entries cite ADR 0010 "known gaps").

## What the rules can and cannot see

Rules read source through a small lexer: comments, strings and char literals are never matched,
and test code is skipped (`#[cfg(test)]` items, `#[test]` / `#[gpui::test]` functions, files under
`tests/`, `benches/`, `examples/`, `*_tests.rs`, `tests.rs`, `test_support*`, and files that a
`#[cfg(test)] mod x;` declaration points at). It does not expand macros or follow `#[path]`. See
"How a rule can be fooled" above.

## Adding a rule

A rule is `crates/lector-archcheck/src/rules/<name>.rs` with a pure `scan_file(path, text)`-style
core and unit tests on in-memory sources, a `check(...) -> RuleOutput`, an entry in `RULES`
(`src/lib.rs`) and in `RULE_ORDER` (`src/baseline.rs`). Create its baseline section with
`scripts/arch-check <rule> --update --allow-increase "initial baseline for <rule> (ADR nnnn)"`.
