# Changelog

## 0.6.0 — 2026-11-06

Team review. Released together with 0.5.0.
Released together with 0.4.0.

- **Pull requests:** review GitHub and GitLab pull requests (incl. GitHub Enterprise and
  self-hosted GitLab) in Lector: Pull Requests panel (review requested / mine / all), the
  review tab with existing threads inline, draft comments, reply and resolve, and Submit
  review (Approve / Request changes / Comment; asks before posting). Pull requests are
  fetched into Lector's own shadow storage — never into your repository.
- **Sign-in without setup:** tokens come from `GITHUB_TOKEN`/`GH_TOKEN`/`GITLAB_TOKEN`, the
  `gh` / `glab` CLIs, or the OS keychain; they are never logged or stored in plain text.
- **Comments that survive change:** comment on any line of any changeset; threads follow the
  code through edits, moves and rebases, and become "outdated" (keeping the original
  snippet) only when the code is really gone. Copy as Markdown or send to an agent.

## 0.5.0 — 2026-11-06

Agents.

- **Agent panel:** drive Gemini CLI, Claude Code, Codex or any Agent Client Protocol agent.
  Streaming replies, plans, tool calls with mini-diffs, inline permission prompts (allow once
  / always for this session / reject), context chips for files, selections and diffs.
- **Every turn is a checkpoint:** review a turn's changes in the review tab, undo a turn
  (itself undoable), see which prompt changed a line by hovering it. Agent edits appear live
  in open tabs, are confined to the project (never `.git`, never outside through symlinks)
  and are journaled.
- **Feedback loop:** "Send to agent" turns review comments into a prompt with file and line
  anchors.
- **Command line:** `lector status | changes | diff | commands | run | activity | undo` with
  stable `--json` output (`lector-cli` console binary on Windows).
- **MCP server:** `lector mcp [path]` exposes Lector's commands as tools to MCP clients;
  read-only unless started with `--allow-writes`, and every write is journaled.

## 0.4.0 — 2026-11-06

Foundations: a headless core, undo instead of "are you sure?", calmer reviews.

- **Undo instead of confirm:** discards, merge resolutions, accept-all and other writes are
  backed up first (in Lector's own data folder, never your `.git`) and show an Undo toast.
  The **Activity log** lists everything Lector wrote, with Undo per entry.
- **Review queue:** files ordered by risk with the reason shown ("changes a public function
  signature", "removes error handling", "touches CI workflows"); generated and lock files
  grouped; "N of M hunks reviewed" progress; `]` jumps to the next unreviewed hunk; hunk
  "reviewed" marks survive new revisions and rebases.
- **Moved code:** blocks moved within or across files are shown dimmed with "Moved from
  file:line" instead of as deletions and additions; optional syntax-aware diff view.
- **Provenance:** hover a changed line to see the commit (or agent turn) behind it.
- **Missing language server?** The exact install command for your OS is shown.
- **Accessibility:** F6 / Shift+F6 cycle regions, Tab order in dialogs and panels, visible
  focus rings, Shift+F10 context menus, UI scale (Ctrl/Cmd+Alt+= / - / 0), reduced motion,
  text or glyph cues next to colors. Screen readers need a GPUI version with AccessKit —
  see docs/ACCESSIBILITY.md.
- **Under the hood:** a headless core shared by the UI, CLI, MCP and agents (typed command
  registry with JSON schemas, priority scheduler, one git state per repository shared by
  all windows, document store with versioned queries, embedded database), faster fuzzy
  finder, trigram search index, content-addressed caches.
- **Security hardening:** project settings can no longer change which programs run
  (language servers, agents); command paths are confined to the project.

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
