# Lector

**A fast, reading-first code viewer and git review tool, written in Rust.**

AI agents now write a large share of code; humans increasingly *read* and *review* it.
Today's editors are built for typing. Lector is built for reading: understanding an
unfamiliar codebase, following definitions, and reviewing changes — working-tree
edits, commits, branches, or whatever an agent just did.

Reading comes first, but you can act on what you read: stage and commit, make the small
fix, rename a symbol, resolve a conflict. Lector only writes when you explicitly ask it to
(save, stage, commit, discard, resolve) — browsing, diffing and blame never touch your files.

![Lector reading code](docs/screenshots/reader-dark.png)

## Features (v0.3)

**Reading**
- Native, GPU-rendered UI (GPUI) with embedded JetBrains Mono and Inter fonts
- Tree-sitter highlighting for 23 languages, folding, indent guides, minimap
- Sticky scroll headers, breadcrumbs, outline panel, symbol pickers
- Go to definition, find references and hover — via your installed language servers,
  with a tags-based fallback that works with zero setup
- Fuzzy file finder (nucleo), command palette, project search (ripgrep's engine)
- Markdown preview, image preview, binary detection, instant opening of 1 GB+ files
- Live reload when files change on disk (e.g. while an agent works)
- Material file icons, light/dark/high-contrast themes, colorblind-safe diffs
- Session restore, keyboard and mouse parity, context menus everywhere

**Git review**
- Changes panel: staged, unstaged, untracked, conflicts; branch vs base; commit ranges
- Side-by-side and unified diffs with word-level highlights and folded context
- Review tab with per-file "viewed" checkboxes that reset when a file changes again
- History with commit graph, ref labels and filters; commit detail view
- Branches, tags and stashes; blame gutter; file and line history
- Permalinks for GitHub, GitLab, Bitbucket, Gitea/Codeberg, sourcehut
- Stage, unstage and discard files, hunks or selected lines; commit composer with amend,
  sign-off and hook output
- Merge conflicts: inline accept actions, three-way merge editor, Continue/Abort for
  merges, rebases, cherry-picks and reverts

**Light editing**
- Multiple carets, IME, undo/redo, auto-indent, comment toggling, line moves, find & replace
- Saves are atomic and keep encoding, BOM, line endings and permissions; unsaved-change
  prompts; files changed or deleted on disk are handled without losing work
- Language servers: diagnostics, format (and format on save), code actions, rename
- Read-only mode per tab or globally (`editor.read_only = true`)

**Everywhere**
- Menu bar on every platform (in the title bar on Linux and Windows), welcome screen with
  recent projects, keyboard shortcut viewer — every command is reachable by mouse or keys

| | |
|---|---|
| ![Split diff](docs/screenshots/diff-split-dark.png) | ![History graph](docs/screenshots/history-graph-dark.png) |
| ![Review tab](docs/screenshots/review-tab-dark.png) | ![Outline](docs/screenshots/outline-light.png) |
| ![Commit composer](docs/screenshots/commit-composer-light.png) | ![Merge editor](docs/screenshots/merge-editor-dark.png) |
| ![Welcome](docs/screenshots/welcome-dark.png) | ![Diagnostics](docs/screenshots/diagnostics-dark.png) |

## Download

Get the latest build from [GitHub Releases](https://github.com/meetdhruva/lector/releases/latest):

| Platform | Installers | Portable |
|---|---|---|
| Windows x64 / ARM64 | `…-setup.exe` (per-user or all users), `.msi` | `…-portable.zip` |
| macOS 11+ (universal) | `.dmg` | `.zip` |
| Linux x86_64 / ARM64 | `.deb`, `.rpm` | `.AppImage`, `.tar.gz` |

Builds are not code-signed yet. On macOS, right-click Lector.app → *Open* on first launch;
on Windows, SmartScreen may ask you to confirm. Every release includes `SHA256SUMS.txt`.

**Portable mode:** if a `lector-data` folder exists next to the executable, Lector keeps all
settings and state there (the Windows portable zip ships with it).

## Install from source

Requires Rust 1.88+ and `git` on `PATH`.

```sh
git clone https://github.com/meetdhruva/lector
cd lector
cargo build --release -p lector
./target/release/lector path/to/repo        # or: lector file.rs:42
```

Linux build dependencies (Debian/Ubuntu):

```sh
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  libxcb1-dev libx11-xcb-dev libfontconfig-dev libfreetype-dev libssl-dev
```

macOS needs Xcode command line tools; Windows needs the MSVC toolchain.

## Usage

```
lector [PATH[:LINE[:COL]]] [--theme NAME]
```

| Action | macOS | Linux / Windows |
|---|---|---|
| File finder | `cmd-p` | `ctrl-p` |
| Command palette | `cmd-shift-p` | `ctrl-shift-p` |
| Project symbols / file outline | `cmd-t` / `cmd-shift-o` | `ctrl-t` / `ctrl-shift-o` |
| Project search | `cmd-shift-f` | `ctrl-shift-f` |
| Changes / History | `ctrl-shift-g` / `ctrl-shift-h` | same |
| Go to definition / references | `f12` / `shift-f12` | same |
| Back / forward | `ctrl--` / `ctrl-shift--` | `alt-left` / `alt-right` |
| Next / previous hunk (diff) | `alt-n` / `alt-p` | same |
| Mark file viewed (review) | `v` | `v` |
| Toggle blame | `cmd-alt-b` | `ctrl-alt-b` |

Settings live in `settings.toml` and `keymap.toml` in Lector's config directory
(run *Open Settings* from the command palette). A project can override settings in
`.lector/settings.toml`.

## Architecture

Lector is a Cargo workspace of small crates. Everything below the UI is UI-free,
synchronous, and reusable on its own:

| Crate | Purpose |
|---|---|
| `lector-text` | SumTree, rope, memory-mapped text, coordinate conversions |
| `lector-syntax` | tree-sitter language registry, highlights, outline, folds, tags |
| `lector-diff` | line diff (imara-diff histogram), word refinement, split/unified rows |
| `lector-search` | gitignore-aware walker, fuzzy picker (nucleo), project grep |
| `lector-git` | git reads via gitoxide; blame, history and all writes via the git CLI |
| `lector-lsp` | LSP client without an async runtime (navigation, diagnostics, edits) |
| `lector-buffer` | editing model: multi-cursor selections, edit operations, undo/redo |
| `lector-fs` | file loading, encoding and kind detection, debounced watcher |
| `lector-config` | settings, keymaps, paths, session and review state |
| `lector-theme`, `lector-icons` | theme model and built-in themes; file and UI icons |
| `lector-workspace` | UI-free app model: project, documents, navigation, review |
| `lector-ui`, `lector-ui-git` | GPUI shell, widgets and views |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for contracts and design rules,
[docs/PERFORMANCE.md](docs/PERFORMANCE.md) for release-gate measurements, and
[docs/RELEASING.md](docs/RELEASING.md) for how releases are built.

## Roadmap

| Release | Scope |
|---|---|
| ✅ 0.2 | Stage/unstage hunks, discard, commit composer, menus and welcome screen |
| ✅ 0.3 | Editing, multi-cursor, formatting, LSP code actions, conflict resolution |
| 0.4 | Full accessibility (screen readers via AccessKit) |
| 0.5 | Agent integration: ACP client, MCP server, headless CLI |
| 0.6+ | WASM plugins, PR integration, stacked changes, structural diff |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Bundled fonts, icons and grammars keep their own permissive licenses; see
[assets/THIRD_PARTY.md](assets/THIRD_PARTY.md), `crates/lector-icons/THIRD_PARTY.md`
and `crates/lector-syntax/GRAMMARS.md`. Release artifacts also include the full license
texts of all Rust dependencies (`third-party/CRATES.md`, generated with cargo-about).
