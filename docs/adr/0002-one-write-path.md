# 0002 — Every write is a registry command

## Problem
There are two ways to change the user's repository: typed commands in the `lector-app`
registry (journaled, undoable, visible to CLI/MCP) and direct calls from UI code.
`lector-ui-git` references `lector_git` 57 times and `lector_process` 22 times. The two paths
produced real defects in 0.7: duplicate palette entries, "Drop Stash" running without
confirmation, and writes whose undo record did not match what was changed.

## Decision
1. Every operation that changes the user's files, index, refs, stashes, remotes or config is a
   **registry command**. The command is the only code that calls the `lector-git` write API,
   takes the backup, writes the journal entry and declares its undo.
2. **UI crates may not** call `lector_git::write`/sync/branch/stash functions, construct a
   `ProcessCommand`, or depend on `lector-process`. They may use `lector-git` *read types*
   (`Oid`, `Status`, `Commit`, …) received from `lector-app`.
3. Each command declares, in its spec: `writes: bool`, `destructive: bool`,
   `undo: Undoable | NotUndoable(reason)`, and `ui_flow` (the UI action that fronts it, if
   any). The palette and menus are generated from the specs; a command is listed once.
4. Credential prompts and progress stay as they are (`SyncHost`, `AskpassServer`) but are owned
   by `lector-app`; the UI only supplies the prompt handler and subscribes to progress.
5. **Undo round-trip test for every write command** (`tests/undo_roundtrip.rs`): state =
   worktree bytes + index entries + refs + stash list; `state → command → undo → state'`,
   assert equal, or the command is declared `NotUndoable`. New write commands cannot merge
   without one (the test enumerates the registry).

## Rejected
- *Lint only the known bad call sites*: the next feature adds new ones.
- *Let UI call read-only git functions that shell out*: reads that spawn processes go through
  the app's git service too, so they are cached, cancellable and scheduled.

## Checked by
`arch-check deps`: no `lector-ui*` → `lector-process`. `arch-check writes`: no call to a
function on the write-API list from `crates/lector-ui*`. `undo_roundtrip` enumerates commands.
