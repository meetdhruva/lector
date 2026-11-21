//! `arch-check`: the architecture ratchet (see `docs/adr/0008-guardrails.md` and `arch/README.md`).
#![forbid(unsafe_code)]

use std::io::Write as _;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let code = lector_archcheck::run(&args, &mut stdout, &mut stderr);
    let _ = stdout.flush();
    let _ = stderr.flush();
    std::process::exit(code);
}
