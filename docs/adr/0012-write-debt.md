# 0012 — The write violations the stronger guardrails exposed

## Problem
After ADR 0010 the `writes` rule sees every crate and resolves aliases. It found 20 existing
violations the old rule could not see; 16 remain in `arch/baseline.toml` and 1 in `dupes`:
undo performed in `journal/undo.rs` and called directly from the UI, the CLI and the agent
(8); two directory/file creations in UI code (3); a patch apply in `review_service/apply.rs`
(1); two agent checkpoint calls (2 of which one is a handle accessor); three more accessors
that hand out a `Repo` handle; a second `debounce` in the file watcher. A baseline that never
reaches zero stops being read.

## Decision
1. **Undo is a registry command.** `journal.undo` (entry id, mode check/force) and
   `journal.undo_last` (scope) are commands with `writes`, a declared undo (an undo is itself
   undoable: the journal records the redo) and sources `All`. The Activity log, the Undo toast,
   `lector undo`, `agent.undo_turn` and MCP all invoke them; nothing outside `lector-app`
   calls `Journal::undo` (it becomes `pub(crate)`). The undo engine (`journal/undo.rs`) is the
   implementation of those commands and is listed in `arch/git-callers.allow` with that reason.
2. **UI code creates no files.** Creating the user's `settings.toml` / `keymap.toml` on "Open
   settings" and the no-folder workspace directory move to `lector-app` (a small
   `lector_app::user_files` service: explicit user action, Lector's own directories, private
   modes via `lector_base::private`). The UI calls it.
3. **Hunk apply belongs to its command.** `review_service/apply.rs` is only reachable from
   `git.stage_hunk` / `unstage_hunk` / `discard_hunk`; it moves under `commands/` (or its
   functions become `pub(super)` of the command module). No allowance.
4. **`lector-agent` calls no git write API.** The turn checkpoints move behind
   `lector_app::commands::agent_backup` (begin/finish turn already live there); the
   `shadow_repo()` accessor is removed.
5. **Repository handles stay read-write, on the record.** `Project::repo()`,
   `RepoState::repo()`, `GitStore::repo()` return `&Repo`, whose type also has the write
   methods. A read-only view type was considered and rejected for now (about 60 call sites and
   every read signature in `lector-git` would change, for a property the `writes` rule already
   enforces at each call site, fail-closed). The three accessors are listed in a new
   `arch/git-handles.allow` with this reason instead of sitting in the baseline; a fourth
   accessor is a violation.
6. **The watcher's coalescing is not a debounce duplicate.** `lector_fs::watch` batches raw
   events inside its owner thread before anything is scheduled; it is renamed for what it does
   and listed in `arch/one-impl.toml` as a reviewed exception with that reason.
7. After this record `writes` and `dupes` are 0 in the baseline.

## Rejected
- *Leave the entries in the baseline*: see Problem.
- *A `RepoRead` view type now*: see 5; it gets its own record if the rule ever proves
  insufficient.

## Checked by
`arch-check writes` and `dupes` at 0; the registry enumeration tests (`undo_roundtrip`,
`read_only`, `command_sources`) cover the two new commands; a test that MCP without
`--allow-writes` cannot undo; `lector undo` journey tests unchanged.

## Clarifications at implementation
Recorded by the architect when reviewing the implementation, before merge.

- **The undo commands record themselves.** The journal's undo engine writes the entry that
  carries the redo, so `CommandSpec::records_itself` tells `invoke` not to add a second entry.
  Only `journal.undo` and `journal.undo_last` set it. Their declaration is `Undoable`.
- **Sandboxed sources undo only their own project.** For MCP (and any sandboxed source) the
  entry's repository must be the context's, every path in the entry and its record must pass
  the write confinement, `undo_last` skips entries that fail, and `scope: "all"` is refused.
- **Force** skips only the changed-files check; a missing backup is never overridden.
- **Scheduling is unchanged:** UI undos run on the repository's serial git-write lane, agent
  turn undos as `AgentUndo`; both drain at quit.
- **`arch/one-impl.toml` has an `exception` entry kind** (file, function, reason; stale entries
  fail) beside `delegate`, and the debounce concept also matches `coalesce_*`, so renaming a
  debounce cannot hide it. The watcher's `coalesce_events` is the one exception.
- **New `settings.toml` / `keymap.toml` are created `0600`** (they go through
  `lector_base::private` like the rest of Lector's own files).
- `writes` and `dupes` are 0 in the baseline. What remains in the baseline is `hygiene` (6: the
  `lector-perf` gate binaries).
