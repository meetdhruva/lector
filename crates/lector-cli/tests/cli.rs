//! Runs the CLI suite against the console binary `lector-cli`.
const BIN: &str = env!("CARGO_BIN_EXE_lector-cli");

#[path = "suite/mod.rs"]
mod suite;
