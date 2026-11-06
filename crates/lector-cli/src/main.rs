//! `lector-cli`: the console build of Lector's command line.
//!
//! * `lector-cli <subcommand> ...` runs the headless CLI (same as `lector <subcommand>`).
//! * Anything else (`lector-cli .`, `lector-cli src/main.rs:12`) starts the GUI app that sits
//!   next to this executable and returns at once. That is what makes this binary usable as
//!   `lector.com` on Windows, where `lector` in a terminal resolves `.com` before `.exe`.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli_arg = lector_cli::is_cli_invocation(&args)
        || args
            .get(1)
            .is_some_and(|a| matches!(a.to_str(), Some("-h" | "--help" | "-V" | "--version")));
    if cli_arg {
        return lector_cli::run_args(args);
    }
    match lector_cli::launch_gui(&args[1..]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lector: {err}");
            ExitCode::from(1)
        }
    }
}
