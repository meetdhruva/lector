# Changelog

## 0.8.0 — unreleased

Upgrading: on its first run, 0.8 imports Lector's own state (recent projects, window and terminal
sessions, review marks, update and notice choices) from the separate files earlier versions wrote
into one database (`lector.redb` in the data folder). The old files are kept, renamed with an
`.imported` suffix. Going back to 0.7.1 afterwards keeps recent projects, window sessions and
review marks (0.7.1 reads them from the same database) but finds no terminal tabs, update-check
state or notice answers (it looks for the files, which are now `.imported`), so it asks its
first-run update question again.

Changes:

- **All of Lector's own state is in one versioned database.** Every save is a transaction, so two
  windows or two Lector processes saving at once can no longer undo each other's changes (this
  also covers the update and notice choices, which were rewritten as a whole file by whichever
  window saved last).
- **A second Lector that cannot open the database for writing says so** instead of silently
  working on a private copy whose changes were thrown away at exit: it runs with read-only state
  (changes made in it are kept until it quits) and shows a note. Two current Lector processes
  still share the database normally.
- **A damaged state database is set aside and replaced**, with a note naming the kept copy. 0.7.1
  kept trying to open the damaged file and silently ran without saving anything, on every start.
- **Background work has one scheduler with priorities.** What answers a keystroke (reparse, picker
  ranking, find as you type) always has a worker of its own, so it stays quick while large diffs,
  indexing or blame run; waits on the network, language servers, agents and credential prompts run
  on a separate bounded set of threads and can no longer hold up local work. Before, a fetch or
  push ran on the same few workers as everything else.
- **Closing a view, document or repository stops its background work**: a newer request replaces an
  older one, and results that are no longer wanted are dropped instead of being applied late. The
  project file scan and symbol indexing of a closed window, and diff and changeset loads that were
  superseded, now stop instead of running to the end.
- **A background job that panics is caught**: the worker keeps running, the panic is written to the
  crash log (`lector health`), and views that report errors (diffs, panels, commits) show it.
- **Quitting finishes your writes and stops the rest.** A save, a stage, a commit or an undo that is
  running or waiting when you quit now completes (up to 10 seconds; a push or a pull-request comment
  up to 2 seconds, since the network can hang) before Lector stops its child processes; everything
  else is cancelled at once. Before, Lector did not wait for any of them: a save still in progress
  was lost (never half-written: saves replace the file in one step), and a git command still running,
  such as a commit, was killed by the process stop that quitting does. A write that could not finish
  in time is named in the log and the crash log (`lector health`), with the command and the
  repository, and whether it was still running.
- **Closing a panel, tab or window no longer drops a write you asked for.** A stage, unstage,
  discard, commit, undo, save or pull-request comment that was still waiting for a worker when its
  view closed now runs (the result is simply not shown). Before, it was silently skipped.
- **Commits with slow hooks no longer hold up the editor.** Git writes (which run your hooks and
  may wait for a signing passphrase) run on threads of their own, one at a time per repository and
  in the order you made them, instead of on the workers that open, save and highlight files: two
  slow pre-commit hooks could stall opening a file. Terminal tab saves are kept in order too.
- **Undoing an agent turn finishes when you quit** (up to the 10-second bound) instead of being cut
  off after 2 seconds with files partly restored; stopping a turn keeps its short grace.
- Replacing a background request no longer runs its cleanup (such as telling a language server
  to drop the old request) while every other background job waits for it, and cleanup that starts
  more background work can no longer hang the app.
- **A credential prompt or an agent's question always appears**, even while many network operations
  or agent sessions are busy: what other work waits on no longer queues behind them.
- **The Changes panel's mode menu no longer runs git when it opens**, and Review Branch no longer
  runs git on the UI thread: the branch's base is looked up in the background.

Security:

- **An agent can no longer write into `.git` through a symlink in the project** (also in 0.7.1,
  fixed in 0.7.2). With a link such as `docs -> .git` or `cfg -> .git/config` committed in a
  repository, an agent's file writes through the link (`docs/hooks/pre-commit`, `docs/config`,
  `cfg`) were accepted, because only the spelled path was checked for `.git`. Every path an agent,
  an MCP client or a command argument names is now resolved with its links before it is checked,
  `.git` is recognised in every spelling (any case, trailing dots, Windows short names such as
  `GIT~1`) both as written and after resolving, the repository's real git directory is off limits
  wherever it is (a `.git` that is a link, or a `gitdir:` file of a worktree or submodule, and its
  shared directory), Lector's own data and settings folders are off limits, and writes never go
  through a symlink, even one swapped in while the write runs.
- **Your GitHub or GitLab token is no longer sent to look-alike hosts** (also in 0.7.1, fixed in
  0.7.2). A repository whose remote was on any host whose name starts with `github.` or `gitlab.`
  (for example `github.evil.example`) was treated as GitHub or GitLab, and the token from
  `GITHUB_TOKEN` / `GH_TOKEN` or `GITLAB_TOKEN` was sent to it, in API calls and when fetching a
  pull request. Now only `github.com`, `gitlab.com` and the hosts you list in `[forge]` are code
  hosts; `GITHUB_TOKEN` / `GH_TOKEN` and `GITLAB_TOKEN` go only to `github.com` / `gitlab.com`, and a
  host you list needs its own token (`LECTOR_FORGE_TOKEN_<HOST>`, `gh` / `glab` signed in to that
  host, or its keychain entry). Tokens travel only over HTTPS and are dropped on redirects.
- **MCP clients can no longer fetch, pull, push or drive agents.** With `lector mcp --allow-writes`
  an MCP client could run `git.push` (including with `force_with_lease`), `git.fetch`, `git.pull`,
  start and prompt agents and answer an agent's permission request in your name. Commands that use
  the network or your credentials, or start or steer an agent, now run only from Lector's window or
  `lector run`; answering a permission request only from the window.
- **An agent write that cannot be undone is refused.** If the backup that makes it undoable fails,
  the write is refused instead of happening without an undo.
- **Agents work in folders that are not git repositories, with undo.** Each file an agent writes
  there is backed up on its first touch in the turn into Lector's own data folder (never a `.git`
  in your folder; nothing is created until the first write), and Undo turn (the Agent panel, the
  Activity log, `lector undo`) restores exactly the files the turn touched, including removing the
  ones it created. Only those files are read: a turn costs the same in your home folder as in an
  empty one. Agents can only write files: naming a folder (or a device) as the file to write is
  refused before anything is read. A file over 64 MiB is not backed up there, so a write to it, or
  of more than that, is refused with the reason, and one turn backs up at most 1 GiB (further
  writes are refused until the next turn). A file that grew past 64 MiB through another program
  during the turn is still part of the turn: Undo restores the state before it (removing it if
  the turn created it) and says it cannot be redone; Undo never overwrites such a file if it
  changed after the turn, because that change could not be backed up. The Activity log and Undo
  Last Operation work in such folders too. In 0.7.1 such writes had no undo.
- **Undoing an agent turn restores git-ignored files it overwrote** (such as `.env`), not only
  tracked and untracked ones; the turn's changes list them too.
- **Backups of deleted or renamed folders are cleaned up** once they are 30 days old, like every
  other backup.
- **Lector's data folder is private.** The backups Lector keeps (which can include git-ignored
  files such as `.env`), logs, crash reports and session records were created readable by other
  users of the machine under the usual umask. Everything Lector creates there is now owner-only,
  and an existing data folder that others could read is narrowed when Lector starts.
- **Undo is a command like every other write.** `journal.undo` (one activity entry, `check` or
  `force`) and `journal.undo_last` are in the command list, so `lector run` and MCP clients started
  with `lector mcp --allow-writes` can undo too; an MCP client can only undo entries of the project
  it was given, never another project's. The Undo toast, the Activity log (with "Restore anyway"
  and Redo), Undo last operation, `lector undo` (same output and `--json`) and Undo turn work as
  before.
- **A new `settings.toml` or `keymap.toml` is created private.** When Open Settings or Open Keymap
  creates the file, it is now readable only by you (and its folder too, if it is new); an existing
  file is never rewritten and keeps its permissions.

## 0.7.2 — 2026-11-25

Security fixes.

Security:

- **An agent can no longer write into `.git` through a symlink in the project.** With a link such as
  `docs -> .git` or `cfg -> .git/config` committed in a repository, an agent's file writes through
  the link (`docs/hooks/pre-commit`, `docs/config`, `docs/newdir/x`, `cfg`) were accepted, because
  only the spelled path was checked for `.git`; a hook planted this way runs at the next commit.
  Every path an agent, an MCP client or a command argument names is now resolved with its links
  before it is checked, `.git` is recognised in every spelling (any case, trailing dots, Windows
  short names such as `GIT~1`) both as written and after resolving, the repository's real git
  directory is off limits wherever it is (a `.git` that is a link, or a `gitdir:` file of a
  worktree or submodule, and its shared directory), Lector's own data and settings folders are off
  limits, and writes never go through a symlink, even one swapped in while the write runs.
- **Your GitHub or GitLab token is no longer sent to look-alike hosts.** A repository whose remote
  was on any host whose name starts with `github.` or `gitlab.` (for example
  `github.evil.example`) was treated as GitHub or GitLab, and the token from `GITHUB_TOKEN` /
  `GH_TOKEN` or `GITLAB_TOKEN` was sent to it, in API calls and when fetching a pull request. Now
  only `github.com`, `gitlab.com` and the hosts you list in `[forge]` are code hosts.
  `GITHUB_TOKEN` / `GH_TOKEN` and `GITLAB_TOKEN` go only to `github.com` / `gitlab.com`; a host you
  list needs its own token: `LECTOR_FORGE_TOKEN_<HOST>` (for example
  `LECTOR_FORGE_TOKEN_CODE_EXAMPLE_COM`), `gh` / `glab` signed in to that host, or its keychain
  entry. If you used `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN` or `GITLAB_TOKEN` for a self-hosted
  server, set its `LECTOR_FORGE_TOKEN_<HOST>` instead. Tokens travel only over HTTPS and are dropped
  on redirects.

## 0.7.1 — 2026-11-19

Fixes for problems found in review of 0.7.0.

Fixes:

- **Undo turn acts on the right agent session.** Turn numbers start at 1 in every agent session, so
  "Undo turn" in the review tab of an older session could restore the files of the newest session's
  first turn. Review tabs, the turn buttons in the Agent panel, the file menu and inline edits now stay
  bound to the session their turn came from; the button is disabled, with the reason, when that turn
  cannot be undone any more.
- **Refining an inline edit after starting a new agent session no longer sticks on "Working".** The
  refinement runs on the session of the pending edit while it is alive, otherwise on the current one,
  and its result (or its failure) is shown.
- **Files opened from a terminal link outside the project stay read-only.** They became editable as
  soon as they finished loading. They are now locked read-only from the start (also after a reload,
  a change on disk and a restart); "Toggle Read-Only" explains why instead of unlocking. Files you
  open yourself (File > Open and the like) stay editable.
- **Typing right after accepting a completion keeps the completion.** When the language server was
  slow to resolve the item, a key pressed in the meantime could drop the acceptance and leave the list
  stuck. The key (or the commit character) is now typed right after the completion, Escape cancels
  the acceptance cleanly, and no typed text is lost.
- **Saved terminal tabs of two windows can no longer overwrite each other.** Each project's terminal
  session is stored in a file of its own, written atomically. Sessions saved by 0.7.0 are still read.
- **Undoing a stash restores what was staged** (not just the working tree).
- **Undoing a pull keeps staged changes in other files** that were not part of the pull.
- **Undo after applying a stash also covers files renamed since the stash was made.**
- **A stash apply that partly fails no longer leaves changes without an Undo.**
- **Windows installer debug files (`.wixpdb`) are no longer published** with releases or included in
  `SHA256SUMS.txt`.

## 0.7.0 — 2026-11-13

Sync, terminal, completion, inline agent edits, and installs you can check.

- **Sync with the remote:** fetch, pull (fast-forward only, or rebase) and push from the Git menu, the
  status bar and the palette, with progress, throughput and a Cancel button; a "Sync" action that
  does the obvious next step; Publish Branch for branches without an upstream. Pulls never create an
  implicit merge commit; a pull or stash pop that hits conflicts takes you to the Changes panel.
  Force push exists only as force-with-lease, behind a confirmation. Credential prompts (passwords,
  tokens, SSH key passphrases) and unknown-host-key questions appear in Lector, with Cancel as the
  default for host keys; nothing is stored by Lector. Optional background fetch is off by default.
- **Branches and stash:** create, switch, rename and delete branches, check out a remote branch; switching
  refuses to carry a dirty tree over without asking. Stash, apply, pop and drop, each with the same
  Undo and Activity log as other writes (a dropped stash can be brought back).
- **Integrated terminal:** a panel with tabs, horizontal and vertical splits, profiles, and per-tab titles
  that stay unique; "Open in Terminal" from the file tree and editor tabs; clickable URLs and
  `file:line` references (confirmed before they open); find in scrollback; command marks (prompt to
  prompt navigation, failed commands marked) where the shell supports integration; copy, paste with a
  confirmation for multi-line text, drag selection, mouse reporting, IME; settings for the shell, font,
  scrollback and shell integration. Sessions come back after a restart as placeholders and start only
  when their tab is first shown. Agents get no terminal access: the terminal is for you.
- **Code completion and signature help:** from your language servers only, no AI and no network of Lector's
  own (Ctrl+Space, Ctrl+Shift+Space); accepting a completion applies the server's edits; snippets are off
  by default (`editor.completion.snippets`). This replaces the "no code completion yet" limitation
  listed under 0.2/0.3.
- **Inline agent edit:** select code, press Ctrl/Cmd+I, describe the change, and review the agent's edit as
  a diff inside the file's own tab: accept or reject per hunk or all at once, follow up, or open the full
  review. Nothing is written to disk until you save. Turns also get a plain-language summary.
- **Installs you can verify (groundwork):** the release workflow is signing-ready (macOS Developer ID and
  notarization, Windows Authenticode, GPG for Linux packages, a minisign signature of `SHA256SUMS.txt`),
  with signing and publishing jobs separated from the jobs that run downloaded tools. **Release builds
  are signed only once the maintainer adds the certificates; none are configured yet, and the release
  notes say what each release actually carries.** Manifests for Homebrew, winget and Flatpak are
  included in the repository and attached to releases; they are not published to those package
  managers yet. Help → Check for Updates is **notify-only** (Lector never downloads or installs
  anything) and the automatic check is off until you enable it.
- **Faster and tidier review:** the file finder is faster on very large trees; structural diff scales to
  much larger files; risk ordering was tuned on a 60-case set of changes. That set is synthetic (written
  for this purpose, not collected from real projects), so treat the improvement as a regression guard,
  not as proof of accuracy on your code.
- **Under the hood:** one process supervisor starts git, language servers and terminals in their own
  process trees and kills the whole tree on cancel or quit; a local crash log and `lector health` show
  whether sessions ended cleanly (nothing is sent anywhere); instruction-count performance gates in CI
  (`scripts/perf-gate.sh`); scripted end-to-end journeys.
- **Security hardening:** terminal titles and working-directory names are sanitized, OSC 52 clipboard reads
  are denied, pasted text cannot end bracketed paste early, and Lector's own environment variables are
  removed from shells; credential prompts run over a private socket with a per-process token, and prompt
  text from servers is treated as untrusted; the Update dialog opens only
  `github.com/meetdhruva/lector` pages and falls back to the release page otherwise; palette commands
  without a dialog of their own (destructive or writing ones) ask before they run.
- **Fixes found in review:** the command palette listed some Git and Agent commands twice and the duplicates
  skipped their dialogs (Drop Stash ran at once); Ctrl+Shift+5 (split terminal) now works on X11; on macOS
  Cmd+K clears the terminal instead of waiting for a chord, and Ctrl+K reaches your shell on Linux and
  Windows; "Check for Updates" answers in the window that asked; Open in Terminal focuses the terminal;
  pressing Ctrl+Shift+W repeatedly never closes the window; informational and partial-failure messages
  no longer show the success check; quitting saves the session first and records a clean exit last.

Known limitations (0.7): Sync has not been tested against real HTTPS or SSH remotes with credential
prompts beyond a scripted helper. The Windows (ConPTY) and macOS terminal paths were verified only by CI
tests, not on those systems. Inline agent edit has not yet been exercised with a real agent in a window.
Shell integration (command marks, working-directory tracking) is unavailable for macOS login bash,
PowerShell and cmd. The terminal scrollbar cannot be dragged. The file picker misses its 50 ms gate for
some cold 6-character queries at 1M paths. There is no in-place updater: updates are downloaded by hand.
Screen readers still need a GPUI version with AccessKit.

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
