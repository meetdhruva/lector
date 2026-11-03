# Changelog

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

Known limitations: fuzzy finder misses its 50 ms gate at 1M paths on 2-core machines;
cold open of 1 GB files is disk-bound; no accessibility support yet (planned for 0.4).
