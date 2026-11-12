//! What a gate binary is made of: a list of [`Bench`]es and [`gate_main`].
//!
//! Each bench has a `setup` that builds its (fixed, generated) input and returns the [`Work`]
//! to measure. [`gate_main`] runs the setup normally and the work inside [`gate_window`], the
//! one function `valgrind --toggle-collect` is pointed at, so setup costs nothing.
//!
//! Gate binaries accept: `--list` (`name<TAB>window|process`, one bench per line), `<name>` (run
//! one bench, once), `<name> --setup-only` (build the input, skip the work) and
//! `--native [<name>]` (run once or all benches without valgrind and print wall time, for a
//! local sanity check).
//!
//! Benchmarks should be single-threaded and independent of the clock and of the environment;
//! those are counted by `--toggle-collect` on [`gate_window`] (mode `window`). Callgrind's
//! collection toggle only follows the thread that called [`gate_window`], so a benchmark whose
//! work runs on other threads (the fuzzy picker) is marked [`Bench::threaded`] (mode `process`):
//! the runner then counts the whole process and subtracts a `--setup-only` run. Their counts
//! still depend a little on thread timing, so the runner takes the minimum of several runs.

use std::process::ExitCode;
use std::time::Instant;

/// The measured part of a benchmark.
pub type Work = Box<dyn FnOnce()>;

/// One named benchmark: `setup` builds the input and returns the work to count.
pub struct Bench {
    /// `group/name`, stable: it is the key in `perf/baseline.json`.
    pub name: &'static str,
    /// Builds the input; the returned closure is the measured work.
    pub setup: fn() -> Work,
    /// The work runs on threads other than the caller's (counted by process difference).
    pub threaded: bool,
}

impl Bench {
    /// A single-threaded benchmark.
    pub const fn new(name: &'static str, setup: fn() -> Work) -> Bench {
        Bench {
            name,
            setup,
            threaded: false,
        }
    }

    /// A benchmark whose work runs on other threads.
    pub const fn threaded(name: &'static str, setup: fn() -> Work) -> Bench {
        Bench {
            name,
            setup,
            threaded: true,
        }
    }
}

/// Runs `work`. This function's name is the toggle for callgrind's collection
/// (`--toggle-collect=*gate_window*`): only instructions executed inside it are counted.
#[inline(never)]
pub fn gate_window(work: Work) {
    work();
    std::hint::black_box(());
}

/// Entry point of a gate binary for `benches`.
pub fn gate_main(group: &str, benches: &[Bench]) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list") => {
            for b in benches {
                println!(
                    "{}\t{}",
                    b.name,
                    if b.threaded { "process" } else { "window" }
                );
            }
            ExitCode::SUCCESS
        }
        Some("--native") => {
            let only = args.get(1).map(String::as_str);
            for b in benches.iter().filter(|b| only.is_none_or(|n| n == b.name)) {
                let work = (b.setup)();
                let t = Instant::now();
                gate_window(work);
                println!("{:<32} {:>10.2?}", b.name, t.elapsed());
            }
            ExitCode::SUCCESS
        }
        Some(name) if benches.iter().any(|b| b.name == name) => {
            let setup_only = args.get(1).is_some_and(|a| a == "--setup-only");
            for b in benches.iter().filter(|b| b.name == name) {
                let work = (b.setup)();
                if setup_only {
                    drop(work);
                } else {
                    gate_window(work);
                }
            }
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: gate_{group} --list | --native [NAME] | NAME [--setup-only]");
            ExitCode::from(2)
        }
    }
}

/// Convenience for building a [`Work`] from a closure.
pub fn work(f: impl FnOnce() + 'static) -> Work {
    Box::new(f)
}
