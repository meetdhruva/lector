//! Parse throughput of the headless emulator (no PTY).
//!
//! Run with `cargo bench -p lector-terminal`. Feeds ~64 MB of mixed output
//! (plain lines, SGR-colored lines, wide characters, shell-integration marks)
//! in 64 KiB chunks and reports MB/s. Target: >= 50 MB/s in release.

use std::time::Instant;

use lector_terminal::{Emulator, EmulatorConfig, TermSize};

fn corpus() -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0u64;
    while out.len() < 4 * 1024 * 1024 {
        match i % 8 {
            0 => out.extend_from_slice(format!("\x1b[32m{i:>8}\x1b[0m \x1b[1;34msrc/main.rs\x1b[0m:{}:5 warning: unused variable `x`\r\n", i % 900).as_bytes()),
            1 => out.extend_from_slice("plain ascii output line with some words in it and a number 1234567890\r\n".as_bytes()),
            2 => out.extend_from_slice("\u{4e2d}\u{6587}\u{8f93}\u{51fa} mixed width \u{65e5}\u{672c}\u{8a9e} line\r\n".as_bytes()),
            3 => out.extend_from_slice(format!("\x1b[38;5;{}m256 color\x1b[38;2;10;{};200m truecolor\x1b[0m\r\n", i % 256, i % 256).as_bytes()),
            4 => out.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07cargo build\r\n\x1b]133;C\x07"),
            5 => out.extend_from_slice(b"   Compiling lector-terminal v0.7.0 (/home/user/lector/crates/lector-terminal)\r\n"),
            6 => out.extend_from_slice(b"\x1b]133;D;0\x07\x1b]7;file:///home/user/lector\x07"),
            _ => out.extend_from_slice(b"\ttab\tseparated\tcolumns\there\r\n"),
        }
        i += 1;
    }
    out
}

fn plain() -> Vec<u8> {
    let line =
        b"plain ascii output line with some words in it and a number 1234567890 and more\r\n";
    line.iter().copied().cycle().take(4 * 1024 * 1024).collect()
}

fn main() {
    measure("plain", &plain());
    measure("mixed", &corpus());
}

fn measure(name: &str, data: &[u8]) {
    let rounds = 16;
    let mut emulator = Emulator::new(EmulatorConfig {
        size: TermSize::new(160, 50),
        ..EmulatorConfig::default()
    });
    // Warm up (fills the scrollback so the measured part includes trimming).
    for chunk in data.chunks(64 * 1024) {
        emulator.feed(chunk);
    }
    let start = Instant::now();
    for _ in 0..rounds {
        for chunk in data.chunks(64 * 1024) {
            emulator.feed(chunk);
        }
        let _ = emulator.snapshot();
    }
    let secs = start.elapsed().as_secs_f64();
    let mb = (data.len() * rounds) as f64 / (1024.0 * 1024.0);
    println!(
        "parse_throughput[{name}]: {mb:.0} MiB in {secs:.2} s = {:.1} MiB/s",
        mb / secs
    );
    println!(
        "scrollback {} lines, {} command blocks",
        emulator.history_size(),
        emulator.command_blocks().len()
    );
}
