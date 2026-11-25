# 0011 — Agent edits in a folder without git are backed up and undoable

## Problem
ADR 0010 refuses an agent's file write when it cannot be backed up. In a folder that is not a
git repository there is no shadow repository, so every agent write is refused there. 0.7.1
allowed those writes with no undo. The owner decided: agents keep working in such folders,
with undo, before 0.8 ships.

## Decision
1. **Same guarantee as in a repository:** every agent write is backed up before it happens, a
   turn that changed files is one journal entry, and undoing it restores exactly the files the
   turn touched to their state before the turn (including "did not exist").
2. **Reuse the shadow repository rather than a second backup store.** For a folder without git,
   Lector keeps a shadow repository under its data directory keyed by the folder's canonical
   path, with the folder as work tree, exactly as for a repository but without alternates to a
   user repository. Backups are *path-scoped*: the file about to be written is snapshotted on
   first touch in the turn (`snapshot_paths`), never the whole folder (the folder may be a home
   directory). The turn's undo record lists the touched paths with their before-snapshots; the
   journal's existing check / force / redo logic applies unchanged.
3. **Bounds.** A single file larger than 64 MiB is not backed up and the write is refused with
   that reason. Pruning, owner-only permissions and the 1 GiB / 30 days policy are those of
   every shadow repository.
4. **Nothing else changes for such folders:** no status, staging or history appears; the
   shadow repository is Lector's own data, never visible as a `.git` in the folder; opening
   the folder creates nothing (the shadow is created at the first agent write).
5. **In a repository, the same path-scoped rule applies to files git ignores** if today's
   turn checkpoint does not cover them (verify; a `.env` edited by an agent must be restored
   by undo).

## Rejected
- *A separate per-turn copy store with its own undo record type*: a second backup format,
  pruning policy and undo path beside the journal's.
- *`git init` in the user's folder*: writes into the user's folder without being asked.
- *Allow without undo, with a warning*: the owner chose undo.

## Checked by
Agent tests with the fake agent in a folder without git: write, overwrite, create in a new
directory, delete-by-overwrite cases; undo restores bytes, modes and absence; undo refuses
when a file changed since (check) and restores anyway on force while backing up the present;
the folder contains no `.git` afterwards; a 65 MiB file is refused; the ignored-file case in
a repository.
