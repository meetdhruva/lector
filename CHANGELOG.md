# Changelog

## 0.3.0 — 2026-11-04

Light editing on top of the reader, language server edits and merge-conflict resolution.
Released together with 0.2.0.

- **Editing:** file tabs are editable (or read-only per tab / with `editor.read_only`):
  multiple carets (Ctrl/Cmd+D, Shift+Alt+Up/Down, Alt+click), IME, undo/redo, auto-indent
  and electric outdent, auto-closing brackets, comment toggling, line moves and
  duplication, whole-word occurrence selection, find and replace with regex groups.
- **Saving:** Save, Save As, Save All, Revert; atomic writes that keep encoding, BOM, line
  endings and file permissions; optional trim-trailing-whitespace and final newline;
  opt-in auto-save; prompts before closing or switching folders with unsaved changes;
  files changed or deleted on disk show Reload / Keep Mine / Compare instead of losing work.
- **Language servers:** diagnostics (squiggles, gutter icons, Problems tab, F8), Format and
  format on save, code actions with a lightbulb, inline rename (F2). Edits across several
  files open as unsaved tabs for review — nothing is written until you save.
- **Merge conflicts:** banner for merges, rebases, cherry-picks and reverts with Continue
  and Abort; inline Accept Current / Incoming / Both actions; three-way merge editor with
  an editable result; Mark Resolved saves and stages the file.
- Live Markdown preview of unsaved text; blame follows unsaved edits.

## 0.2.0 — 2026-11-04

Every command is reachable by mouse, and you can stage and commit.

- **Menus and window chrome:** one menu model shown natively on macOS and in the title
  bar on Linux and Windows (File, Edit, Selection, View, Go, Git, Window, Help); window
  controls; welcome screen with recent projects; Open File, Open Recent, New Window,
  Close Folder, Close All Tabs; keyboard shortcut viewer; About dialog; log file;
  toolbar buttons and a clickable status bar.
- **Staging:** stage, unstage and discard files, hunks or selected lines from the Changes
  panel and the diff view (discard always asks first; staging refuses if the file changed
  since the diff was shown).
- **Commit composer:** multi-line message with a 50/72 guide, amend, sign-off, identity
  check with setup help, hook output shown inline; Ctrl/Cmd+Enter commits.
- **Write policy:** Lector writes only on explicit actions; git writes go through the git
  CLI so hooks, filters and signing behave as in the terminal.

Known limitations (0.2/0.3): find (Ctrl+F) is not available inside diff tabs; a conflicted
file's diff still offers hunk staging; committing doesn't warn about unsaved files; auto-save
can save text mid-IME composition; Windows can maximize but not restore from the
ToggleMaximize command (the title bar button works); no code completion yet.

## 0.1.0 — 2026-11-03

First release: a complete read-only reader and git review tool.

- Reading: tree-sitter highlighting (23 languages), folding, sticky scroll,
  breadcrumbs, outline, symbol pickers, go to definition / references / hover
  (LSP with tags fallback), fuzzy file finder, command palette, project search,
  find in file, Markdown and image preview, 1 GB+ files via memory mapping,
  live reload, session restore, themes (including high contrast and colorblind diffs).
- Git: changes panel, split and unified diffs with word highlights, review tab
  with persisted viewed state, history graph, commit view, branches/tags/stashes,
  blame, file and line history, permalinks.
- Safety: never writes to user files or repositories; language servers are started
  with options that keep them from writing into the project (see docs/PERFORMANCE.md
  and the read-only regression test in `crates/lector-workspace/tests/release_audit.rs`).
- Distribution: GitHub releases for Windows (x64, ARM64: setup.exe, MSI, portable zip),
  macOS (universal .dmg and .zip) and Linux (x86_64, ARM64: .deb, .rpm, AppImage, tarball),
  with an app icon, desktop integration ("Open with Lector", file associations, PATH),
  portable mode, a welcome project for first launch, reopening the last project when
  started from a launcher, SHA-256 checksums and bundled third-party licenses.

Known limitations: fuzzy finder misses its 50 ms gate at 1M paths on 2-core machines;
cold open of 1 GB files is disk-bound; no accessibility support yet (planned for 0.4).
