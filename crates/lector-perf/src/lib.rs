//! Deterministic performance gates for Lector's hot paths.
//!
//! Wall-clock benchmarks are too noisy on shared CI runners, so the gates count
//! *instructions* instead: every gate binary (`gate_text`, `gate_buffer`, ...) runs one named
//! workload on fixed, seeded, generated inputs, and the [`runner`] (the `perf-gate` binary,
//! driven by `scripts/perf-gate.sh`) executes it under `valgrind --tool=callgrind`, reads the
//! instruction count of the measured window and compares it to `perf/baseline.json`.
//!
//! * [`harness`]: the `Bench` type and the `main` shared by the gate binaries. Setup (building
//!   inputs) is outside the measured window; only the closure it returns is counted.
//! * [`inputs`]: seeded generators for texts, paths and edits (no network, no real repositories).
//! * [`runner`]: valgrind invocation, callgrind output parsing, baseline file, comparison.
//!
//! See `docs/PERFORMANCE.md`, "Instruction-count gates".
#![forbid(unsafe_code)]

pub mod harness;
pub mod inputs;
pub mod runner;
