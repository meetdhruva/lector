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
| Fuzzy file finder, 1M paths | < 50 ms per keystroke | 0.7 re-measure (2026-11-13, 21 runs per cell, `picker_gate`), p50 / p95 ms until the pass is settled (exact count, final ranking): **cold** 1-3 characters 8-41 / 10-49; **extend by one** 8-38 / 10-45; **backspace** 0.1-3.3 / 0.2-5.5. Six characters: cold 17-30 / 20-40 (uniform corpus), 15-59 / 16-69 (varied corpus), extend 6-30 / 8-36. First page exact: cold 5-39 p50 / 7-49 p95 in every cell. Store: 22 bytes per path on top of the text and the 4-byte item (63 MB for 1 M paths). Injecting 1 M paths: 93-130 ms | Pass for 18 of 20 queries (each judged on the worst p95 of cold, extend and backspace) and for the first page in all 20; the two misses are cold 6-character queries on the varied corpus (`parser` p95 55 ms, `src/ma` p95 69 ms, 71 and 106 ms of CPU on 2 cores) | Chunked arena store with per-path character masks, nucleo score upper bound pruning against the top-k heap (the first page is exact before the match count finishes, `Picker::progress().ranked`), a counting pass that skips scoring for items the mask proves to match, end-position incremental extension and exact replay for backspace, per-chunk cancellation. Typing is almost always extension or backspace; cold 6-character queries are pastes. Remaining: the bound is loose for queries whose letters are common word starts (`src/ma`: ~42 k survivors are scored by nucleo). The wall times of a 2-core VM that also builds other crates fluctuate by tens of ms; `cold cpu`/`ext cpu` in the bench are the reliable columns. Before 0.7: first (partial) results 1-1.7 ms but settled 43-137 ms, `cr` 324 ms. |
| Structural diff, 3 000-line file, ~50 changed lines | <= 150 ms, never worse than the line diff | 0.7 (`structural_bench`, parse excluded, median of 5): 512 lines **2.6 ms**, 3 002 lines **11-15 ms**, 10 007 lines **36-42 ms** (line diff 0.1 / 0.2 / 0.5 ms), 0 regions skipped, 1 900-2 500 states | Pass | Anchors (sibling-hash diff) skip unchanged subtrees, a hash per subtree, a per-region state budget (60 k) and a total budget (400 k states, 750 ms); an over-budget region is reported in `StructuralDiff::skipped` and marked from the line + word diff, so the result is never worse than the line diff; cancellation through `diff_cancellable`. Before 0.7 files of a few hundred lines with a small edit exceeded the budget after 150-250 ms (callers then showed the line diff). |
| Risk ordering, top-1 on the 60-case evaluation set | >= 7 of 10 | Original model 18 of 60 (30 %); new signals with hand weights 58 of 60; fitted weights 58 of 60 (96.7 %): tune 40 of 42, holdout 18 of 18; MRR 0.979 (table below) | Pass on the synthetic set; **not validated on real pull requests** | The set was written together with the signals (the holdout cases were visible while the signals were designed), so the holdout is a regression guard, not an unbiased estimate. A deterministic fit from a flat start (every raising weight 0.3, never seeing the holdout) reaches 41 of 42 tune and 15 of 18 (83 %) holdout. |

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
# PICKER_GATE_REPS / PICKER_GATE_PATHS set repetitions and corpus size.
cargo bench -p lector-search --bench picker_gate

# Structural diff at 500 / 3 000 / 10 000 lines (STRUCTURAL_BENCH_REPS, SHOW_SKIPPED=1).
cargo bench -p lector-workspace --bench structural_bench

# Risk ordering: scores, per-category table, misses, explanations; the fit (deterministic).
cargo run -p lector-app --example risk_eval -- --misses
cargo run -p lector-app --example risk_eval -- --explain <case-id>
cargo run --release -p lector-app --example risk_fit
cargo test -p lector-app --test risk_eval
```

### Risk ordering evaluation (0.7)

`crates/lector-app/tests/risk_eval/cases/*.json`: 60 synthetic changesets of 3-5 files each (Rust, TypeScript, Python,
Go, config), labelled with the file a reviewer must see first, in 10 categories; the risky file is the smaller change
in at least 90 % of the cases and the decoys are big mechanical refactors, added public API, tests, docs, lockfiles,
generated files. The split is fixed (positions 2, 5 and 8 of every ten cases are the 18 holdout cases). Ordering is
`review_queue::analyze` (attention group by score, then generated/lock/negligible files).

| Category (cases) | Original model top-1 | New signals + fitted weights top-1 | MRR before -> after |
|---|---|---|---|
| api-break (8) | 6 | 8 | 0.875 -> 1.000 |
| error-handling (6) | 4 | 6 | 0.833 -> 1.000 |
| security (8) | 0 | 8 | 0.292 -> 1.000 |
| unsafe-exec (6) | 1 | 6 | 0.500 -> 1.000 |
| concurrency (5) | 0 | 5 | 0.300 -> 1.000 |
| migration (4) | 1 | 4 | 0.625 -> 1.000 |
| deps-ci (6) | 0 | 6 | 0.472 -> 1.000 |
| tests-removed (5) | 1 | 5 | 0.517 -> 1.000 |
| config-flip (4) | 2 | 4 | 0.667 -> 1.000 |
| hard-negative (8) | 3 | 6 | 0.604 -> 0.844 |
| **tune (42)** | 14 (33.3 %) | 40 (95.2 %) | 0.591 -> 0.970 |
| **holdout (18)** | 4 (22.2 %) | 18 (100 %) | 0.523 -> 1.000 |
| **all (60)** | 18 (30.0 %), top-3 75 % | 58 (96.7 %), top-3 98.3 % | 0.571 -> 0.979 |

Misses: a mechanical rename under an `auth/` path outranks a removed `except` branch, and a one-line limit change
(`"member" => 100` to `1000`) has no syntactic signal. `tests/risk_eval.rs` fails when the holdout top-1 falls below 16 of 18
(measured 18, margin 2), the tune top-1 below 37 of 42, the holdout MRR below 0.9, or any category loses its top-3.
Real pull requests: `scripts/risk-eval-import.sh OWNER/REPO PR --risky PATH` writes them to the git-ignored
`tests/risk_eval/local/`; none were scored for this table.

## Instruction-count gates (CI, Linux)

The measurements above are wall-clock and belong to a release audit on known hardware. Wall-clock
benchmarks are far too noisy on shared CI runners to gate every pull request, so the hot paths that
have budgets are also covered by **deterministic gates that count instructions** with
`valgrind --tool=callgrind`. The same code on the same compiler executes the same instructions, so a
5 % regression is a 5 % regression, not noise.

| Gate | Workload (seeded, generated, no network, no real repositories) |
|---|---|
| `text/rope_build_1mb` | `Rope::from_str` of 1 MB (22 k lines) |
| `text/edit_and_line_lookup` | 2000 scattered insert / delete / replace edits on that rope, each followed by offset→point, line range, point→offset |
| `text/scroll_lines` | 500 screenfuls (`lines(rows)`, 60 rows) at random scroll positions |
| `buffer/multi_caret_edit` | 500 carets on 2 k lines: type, move right and left, backspace, undo, redo |
| `buffer/typing_in_big_buffer` | 300 keystrokes in the middle of a 20 k-line buffer |
| `buffer/select_all_occurrences` | select every occurrence of a word (5 k matches) and type over them |
| `diff/line_diff_5k` | histogram line diff of two ~5 k-line files, ~6 % of the lines edited |
| `diff/word_refine` | `refine_inline` of every replaced line pair of that diff |
| `search/picker_100k_paths` | the fuzzy file picker: inject 100 k synthetic paths, type `mod12` key by key, settle each time |
| `syntax/parse_highlight_2k` | tree-sitter parse and highlight of a ~2 k-line Rust file (queries pre-compiled) |
| `syntax/reparse_after_edit` | incremental reparse after inserting a line, then highlight the 60 lines around it |

```sh
scripts/perf-gate.sh                    # build, run, compare with perf/baseline.json; exit 1 on a regression
scripts/perf-gate.sh --update           # rewrite perf/baseline.json with what was measured
scripts/perf-gate.sh --threshold 10     # allow 10 % instead of 5 % (or PERF_GATE_THRESHOLD=10)
scripts/perf-gate.sh --only diff/       # a subset (with --update: merged into the baseline)
```

Needs `valgrind` (`sudo apt-get install valgrind`) and runs in about a minute (the build dominates).
Output is a table of baseline, measured, change and verdict per gate. `target/perf-results.json` has
the measured numbers in the baseline's format.

**How it works.** `crates/lector-perf` (not shipped) holds one gate binary per crate under gate; the
workloads are in `src/bin/gate_*.rs`, their seeded input generators in `src/inputs.rs`. A binary given a
benchmark name builds its input (*setup*, not counted) and runs the work inside one function,
`gate_window`; callgrind is started with `--collect-atstart=no --toggle-collect=*gate_window*`, so only the
work is counted. Callgrind's toggle follows one thread, so a benchmark whose work runs on other threads
(the picker's worker pool) is marked `threaded`: the runner counts the whole process, subtracts a
`--setup-only` run, and keeps the minimum of three runs (its counts wobble by about 1 % with thread
timing; everything else is exact to a few instructions). Measured processes are pinned to one CPU
(`taskset`) so `available_parallelism()`, and with it the number of worker threads, is the same on every
machine. A gate is added by appending a `Bench` to its binary and recording it with `--update`.

**Why the build is pinned.** Instruction counts depend on the compiler and its settings, not only on the
code. `scripts/perf-gate.sh` ignores the profile in `Cargo.toml` and builds with fixed settings (opt-level 3,
no LTO, 1 codegen unit, no debug info, no `RUSTFLAGS`), builds each gate with only its own crate's feature
(feature unification would otherwise change what e.g. `regex` compiles to), and records `rustc`, host,
LLVM version and the profile name in `perf/baseline.json` together with the valgrind and libc versions.
When the baseline's rustc, host, LLVM or profile differ from the current ones, the gate **prints a warning
and is skipped (exit 0)**: it never fails on numbers it cannot compare. valgrind or libc differences are
only noted (they move counts by a fraction of a percent).

**Updating the baseline.** Run `scripts/perf-gate.sh --update` and commit `perf/baseline.json`
- in the same commit as a toolchain bump (`RUST_TOOLCHAIN` in `.github/workflows/ci.yml`, clippy fixes
  included), otherwise the gate is silently skipped from then on;
- when a change makes a path deliberately faster or slower (say so in the commit message; the table marks
  gains beyond the threshold as `improved (consider --update)`);
- when a new gate is added (new gates are listed as `new` and not enforced until recorded).

The baseline must be produced with the compiler CI uses (`RUST_TOOLCHAIN`, currently 1.97.0) on
`ubuntu-latest` (glibc 2.39). The checked-in baseline was recorded locally with rustc 1.97.0
(x86_64-unknown-linux-gnu, LLVM 22.1.6, valgrind 3.22.0, Ubuntu 24.04 / glibc 2.39), the same compiler and
OS image generation as CI; if the first CI run still reports differences, regenerate it on CI:
**Actions → CI → Run workflow** runs the `perf` job in update mode and uploads `perf/baseline.json` as the
`perf-baseline` artifact (every run also uploads `perf-results.json`); commit that file.

**CI.** The `perf` job in `.github/workflows/ci.yml` (Ubuntu only, pinned `RUST_TOOLCHAIN`, valgrind from
apt, `scripts/perf-gate.sh`) runs on pushes and pull requests next to the build matrix. A failure means some
gate needs more than 5 % extra instructions: look at the table, then run
`valgrind --tool=callgrind target/release/gate_<crate> <name>` and `callgrind_annotate` on the output to see
which functions grew.

## Crash log and session health

`lector health [--json]` prints what Lector recorded about itself on this machine: sessions started, clean
exits, abnormal exits (crash, kill, power loss), panics, the crash-free rate and the latest crash reports in
`<data dir>/crashes/`. Nothing is sent anywhere; the files are plain text and can be attached to a bug
report by hand. Design and privacy rules: `docs/ARCHITECTURE.md`, "Measurement (0.7)".
