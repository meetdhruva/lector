# Lector v0.1.0 — release-gate measurements

Targets come from `docs/ARCHITECTURE.md` ("Performance budgets"). Measured on
2026-11-03 for the v0.1.0 release audit.

**Machine:** cloud VM, 2 vCPU Intel Xeon @ 2.10 GHz, 7 GB RAM, virtio disk,
Linux 6.18, rustc 1.97.0, `--release` (thin LTO). No GPU: the UI runs under
Xvfb with Mesa **lavapipe** (software Vulkan), which inflates anything that
includes rendering/presenting. A typical 8+ core developer machine with a GPU
should be noticeably faster on every row.

| Gate | Target | Measured | Verdict | Notes |
|---|---|---|---|---|
| Cold start → first paint | < 150 ms | **101–134 ms** warm (5 runs, termcolor clone; 101–122 ms on the Lector repo); **218–227 ms** with the page cache dropped | Pass (warm) / miss (cold) | Process start → first `WorkspaceRoot` paint. Breakdown (warm): GPUI app + X11 init and project open ≈ 44–58 ms, `lector_ui::init` + `lector_ui_git::init` ≈ 2 ms, window + renderer (lavapipe) + first layout/paint ≈ 55–75 ms. The cold number is dominated by paging in the 250 MB binary. |
| Scroll | 120 fps, p99 frame < 8 ms | Code view CPU work per frame: layout median 0.97 ms / p99 7.3 ms, paint median 1.0 ms / p99 8.1 ms. Whole frame (draw + lavapipe present): median 43 ms, p99 88 ms | Inconclusive here | The code view's own work fits the budget on median; the full-frame number is software rasterization of a 1600×1000 window and says nothing about GPU frame times. Needs re-measuring on real hardware. |
| Open 1 GB file → first paint | < 500 ms | `Project::load_document` + first 80 lines + highlights: **77–128 ms** warm, **607 ms** cold (page cache dropped); jump to last line 40 µs; 10.8 M lines. `lector-text` alone (`Text::open_mapped` + 80 lines): 91 ms warm | Pass (warm) / miss (cold) | `MappedText` builds the exact line index eagerly, so a cold open is bound by reading 1 GB from disk (~1.7 GB/s here). A background index (first screen from the first pages only) would fix the cold case — recommended for 0.1.1. |
| git status, Linux-kernel-sized repo | < 300 ms | 85 k files: **224–289 ms** median (first call 284–317 ms; `git status` 137–160 ms). 50 k files: 92 ms median (first 136 ms; `git status` 101 ms) | Pass (barely, 85 k) | 5 modified + 3 untracked files, index older than the files. `Repo::changes(WorkingTree)` on 85 k: 145–151 ms (measured after `git status` refreshed the index). |
| Fuzzy file finder, 1M paths, first results | < 50 ms | v0.4 top-k scorer, `crate mod12` typed key by key: first (partial) results **1.0–1.7 ms** on every keystroke; fully settled 43–137 ms, `cr` 324 ms and `sr` 288 ms (2-char fuzzy over ~875 k candidates); `src/mod999` settles in 26 ms. Backspace settles 42–106 ms (282 ms for `cr`). Injecting 1 M paths: 98–118 ms | Pass (first results) / settled misses for 1–2 char queries on this 2-core VM | Replaced the full-sort pass by a worker thread with a top-k heap (best 1000), progressive publishing every 12 ms and narrowing from the previous queries' match sets (history of 16). Partial results are the ranking of the items scanned so far, so "first results" are exact only once `tick` reports settled. Remaining: cheaper scoring for 1–2 char queries. |

## How to reproduce

All commands from the repository root. Release-mode test builds compile the
dev-dependencies once (a few minutes on 2 cores).

```sh
# Startup: logs "first paint at N ms" (and other milestones) to stderr.
cargo build --release -p lector
LECTOR_TRACE_STARTUP=1 target/release/lector /path/to/repo
# Headless (as measured): Xvfb :99 + lavapipe, see scripts/screenshot.sh for the env;
# under Xvfb the "first frame presented" milestone only fires after the event loop
# is poked (scripts/x11_poke.py), so use "first paint".

# Scroll frame times: GPUI prints "frame duration" (draw + present) per frame,
# the code view logs its own layout/paint time.
ZED_MEASUREMENTS=1 RUST_LOG=warn,lector_ui::perf=trace target/release/lector some/large/file.rs

# 1 GB file through the workspace model (add LECTOR_PERF_DROP_CACHES=1 as root for a cold run 0).
LECTOR_PERF_DIR=/tmp cargo test -p lector-workspace --release --test perf -- --ignored --nocapture
# Same gate on lector-text alone:
cargo test -p lector-text --release --test large_file -- --ignored --nocapture

# git status (default 50 k files; LECTOR_PERF_STATUS_DIRS=850 for 85 k) and log on 10 k commits.
LECTOR_PERF_STATUS_DIRS=850 cargo test -p lector-git --release --test perf -- --ignored --nocapture

# Fuzzy finder over 1 M synthetic paths.
cargo test -p lector-search --release --test picker release_gate -- --ignored --nocapture
# Per-keystroke table with first/settled/backspace timings (what the row above quotes).
cargo bench -p lector-search --bench picker_gate
```
