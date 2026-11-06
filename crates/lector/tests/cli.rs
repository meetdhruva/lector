//! Runs the CLI suite against the real `lector` binary: subcommands must be dispatched before
//! any GUI code starts (the test environment has no display), and `lector --help` must still
//! describe the GUI.
const BIN: &str = env!("CARGO_BIN_EXE_lector");

#[path = "../../lector-cli/tests/suite/mod.rs"]
mod suite;

#[test]
fn gui_help_is_unchanged_and_mentions_the_subcommands() {
    let out = std::process::Command::new(BIN)
        .arg("--help")
        .env_remove("RUST_LOG")
        .output()
        .expect("lector runs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("--theme") && text.contains("--welcome"),
        "{text}"
    );
    assert!(text.contains("lector status | changes | diff"), "{text}");
}
