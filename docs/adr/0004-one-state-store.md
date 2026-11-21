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
