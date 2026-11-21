# 0004 — One versioned state store

## Problem
App state is in one redb database plus at least seven JSON files written by different modules
with their own read-modify-write code (`review.json`, `review-state.json`, `sessions.json`,
`recent.json`, `notices.json`, `update-state.json`, terminal session files). 0.7.0 lost
terminal tabs to a race between two writers of one such file.

## Decision
1. **All Lector-owned state lives in the database**, behind `lector_app::store`: typed
   tables with a schema version each, transactions for read-modify-write, and one writer
   discipline (multi-process safe: redb file lock + retry; a second process that cannot get the
   lock falls back to read-only state and says so in the log, never to a private copy).
2. **`settings.toml` and `keymap.toml` stay as files** in the config dir: people edit them.
   Per-project `.lector/settings.toml` stays. Logs and crash reports stay as files.
3. **One-time import** on first run of 0.8: each old JSON file is read, written to its table,
   and renamed to `<name>.imported` (not deleted). Import is idempotent and tested with
   fixture files captured from 0.7.1.
4. Crates below `lector-app` (and UI crates) do not open files in the data dir. A module that
   needs persistence takes a typed store handle from `lector-app`. `lector-config` keeps only
   paths, settings and keymap parsing.
5. Corruption policy stays as today: a database that cannot be opened is copied aside (0600)
   and a fresh one started; the user is told.

## Rejected
- *One JSON file per concern with a shared helper*: still N formats, N migration stories, and
  no transactions across concerns.
- *SQLite*: redb is already in use, pure Rust, and sufficient.

## Checked by
`arch-check state`: no `data_dir()`/`state_dir()` path joins with a literal file name outside
`lector_app::store`, `lector-config::paths`, logging and crash reporting. Upgrade test from
0.7.1 fixtures.

## Clarifications at implementation
Recorded by the architect when reviewing the implementation, before merge.

- **Sharing is the normal case.** Processes open the file in redb's `MultiWriter` mode and
  share it read-write (0.7.1 included); write transactions are serialized across processes.
  Read-only state happens only when the file cannot be opened for writing: held exclusively,
  another lock protocol, written by a newer Lector, or not writable.
- **Read-only state keeps the session working, in memory.** Writes go to an in-memory copy
  (seeded from the file when it can be read) and are lost at exit; nothing is written back and
  no private file is made. This replaces "writes fail" so undo and review marks work for the
  session. It must stay visible for as long as it lasts: a persistent status bar item, the
  startup toast, a line in `lector health`, and one stderr line from `lector <subcommand>` /
  `lector mcp` at the first state write.
- **A newer file is never opened for writing.** The schema is read through a read-only handle
  first. Per-concern versions live in `store_meta`: older values migrate on read and the
  concern on its first write; a newer concern reads as absent and refuses writes.
- **Corruption recovery is cross-process safe.** A file counts as damaged only if it stays so
  over repeated opens (about 500 ms); recovery runs under an OS-held lock
  (`lector.redb.recovery-lock`) and first re-opens the file; the damaged file is renamed (not
  copied) to `lector.redb.corrupt-<secs>`, owner-only at every moment. A new database file is
  created 0600; an existing file's mode is left alone.
- **Import.** Per old file: values and an `import:<path>` marker in one transaction, then
  rename to `.imported`. Imported values never overwrite what the store holds; symbolic links
  are not followed; files over 8 MiB are not read; nothing is imported in read-only state.
- **Where types live.** The store owns names, versions and migrations. Value types for notices
  and update state stay in `lector-ui-agent` / `lector-forge` (no `lector-app` -> `lector-forge`
  dependency); session types moved to `lector_app::store`. Hunk-reviewed marks stay in the
  `kv` table.
- **Still files, by decision 2:** settings, keymap, themes, logs, crash reports, the shell
  integration scripts and the journal's shadow repositories.
- **The `state` rule** also resolves file names held in `const`/`static`, and `paths.rs` and
  `db/**` are no longer allowlisted.
