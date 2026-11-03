# Lector — contributor guide (humans and agents)

Lector is a reading-first, read-only (v0.1.0) code viewer and git review tool in Rust.
Read `docs/ARCHITECTURE.md` first: it defines crate boundaries and public API contracts.

## Rules
- v0.1.0 never writes to the user's files or repository. Lector's own state lives in
  `lector_config::paths::data_dir()` / `config_dir()`.
- Core crates (`crates/lector-*` except `lector-ui`) must not depend on GPUI, a window
  system, or tokio. Keep dependencies minimal; each crate should be reusable on its own.
- Respect the layering in `docs/ARCHITECTURE.md`. Never add a dependency on a higher layer.
- Public API changes to a contract in `docs/ARCHITECTURE.md` must update that doc.
- No `unwrap()`/`expect()` on user data or I/O. `thiserror` in libraries, `anyhow` in the app.
- `#![forbid(unsafe_code)]` in every crate except `lector-text` (mmap) — and document each `unsafe`.
- Tests live next to code (`#[cfg(test)]`) plus `tests/` for integration. Use `proptest`
  for invariants, `insta` for snapshots, `tempfile` for fixtures. Git fixtures are
  built with the `git` CLI inside tests.

## Building
- Machine is small (2 cores): build/test only your crate: `cargo test -p lector-text`.
  Never `cargo clean`. Avoid `cargo build --workspace` unless you are integrating.
- The workspace shares one `target/`; if cargo says "Blocking waiting for file lock", just wait.
- Add crate-specific dependencies to that crate's own `Cargo.toml` with explicit versions;
  shared ones are in `[workspace.dependencies]` (`foo.workspace = true`).
- `vendor/xattr` patches a libc incompatibility in GPUI's dependency tree. Keep it.
- Run the app headless for screenshots: `scripts/screenshot.sh <path> <out.png>`.

## Style
- `cargo fmt`; `cargo clippy -p <crate> -- -D warnings` should pass.
- Prefer clear names over comments; doc-comment every public item.
- Line numbers in APIs are 0-based `u32`; byte offsets are `usize`.
