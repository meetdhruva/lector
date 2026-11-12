#!/usr/bin/env bash
# Instruction-count performance gates (Linux only; needs valgrind).
#
#   scripts/perf-gate.sh                      run the gates, compare with perf/baseline.json, fail on > 5% regression
#   scripts/perf-gate.sh --update             rewrite perf/baseline.json with the measured numbers
#   scripts/perf-gate.sh --threshold 10       allow 10% (or: PERF_GATE_THRESHOLD=10)
#   scripts/perf-gate.sh --only text/         run only matching benchmarks
#   scripts/perf-gate.sh --results out.json   also write the measured numbers (default: target/perf-results.json)
#
# The gates are small binaries (crates/lector-perf) that run fixed, generated workloads; the
# `perf-gate` runner executes each under `valgrind --tool=callgrind` and reads the instruction
# count. Counts depend on the compiler and its settings, so the build profile is pinned here
# (not taken from Cargo.toml) and the baseline records the rustc version: when it differs the
# gate is skipped with a warning. Update the baseline in the same commit as a toolchain bump.
# See docs/PERFORMANCE.md, "Instruction-count gates".
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Linux" ]; then
    echo "perf-gate: Linux only (valgrind's callgrind is the instruction counter)" >&2
    exit 2
fi
if ! command -v valgrind >/dev/null 2>&1; then
    echo "perf-gate: valgrind not found (Debian/Ubuntu: sudo apt-get install valgrind)" >&2
    exit 2
fi

# The fixed build profile: deterministic code generation, nothing from the user's environment.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_RELEASE_OPT_LEVEL=3
export CARGO_PROFILE_RELEASE_LTO=off
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_STRIP=none
export CARGO_PROFILE_RELEASE_PANIC=unwind
export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=false
export CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=false
export PERF_GATE_PROFILE="release opt-level=3 lto=off codegen-units=1 debug=0 strip=none panic=unwind"

TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# The runner must build; each gate is built on its own so that one that does not compile
# (an API in flux) is reported and the others still run.
cargo build --release -p lector-perf --no-default-features --bin perf-gate
missing=0
for gate in text buffer diff search syntax; do
    # Never run a stale binary from an earlier build when this one fails to compile.
    rm -f "$TARGET_DIR/release/gate_$gate"
    if ! cargo build --release -p lector-perf --no-default-features --features "$gate" --bin "gate_$gate"; then
        echo "perf-gate: gate_$gate failed to build" >&2
        missing=1
    fi
done

extra=()
if [ "$missing" = 1 ] && [ "${PERF_ALLOW_MISSING:-0}" = 1 ]; then
    extra+=(--allow-missing)
fi

exec "$TARGET_DIR/release/perf-gate" \
    --bin-dir "$TARGET_DIR/release" \
    --baseline perf/baseline.json \
    --results "$TARGET_DIR/perf-results.json" \
    "${extra[@]}" "$@"
