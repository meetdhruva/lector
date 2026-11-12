//! `perf-gate`: runs the gate binaries under callgrind and compares with `perf/baseline.json`.
//! Use it through `scripts/perf-gate.sh`, which builds the gates with the fixed profile first.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env_threshold = std::env::var("PERF_GATE_THRESHOLD").ok();
    let options = match lector_perf::runner::parse_args(&args, env_threshold.as_deref()) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{}", lector_perf::runner::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("perf-gate: {e}\n\n{}", lector_perf::runner::USAGE);
            return ExitCode::from(2);
        }
    };
    match lector_perf::runner::run(&options) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("perf-gate: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
