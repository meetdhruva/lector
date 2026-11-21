//! Headless command line of Lector (v0.5): `lector status | changes | diff | commands | run |
//! activity | undo | mcp | health`.
//!
//! This crate has no GUI dependencies. It is used by two binaries:
//!
//! * `lector` (the GUI) hands every invocation whose first argument is a subcommand name to
//!   [`run_args`] before any GUI code is touched;
//! * `lector-cli` (this crate's own binary) is the console-subsystem build that Windows packages
//!   ship as `lector-cli.exe` and `lector.com`: the GUI-subsystem `lector.exe` has no console, and
//!   `cmd.exe` does not wait for it. See `docs/RELEASING.md`.
//!
//! Human output is compact; `--json` output is stable: fields are only ever added. Exit codes:
//! 0 ok, 1 error (`lector: error: ...` on stderr), 2 usage. A closed stdout (`| head`) exits 0.
//! Diagnostics go to stderr only when `RUST_LOG` is set.
//!
//! # Invocation
//!
//! [`is_cli_invocation`]: the first argument (after an optional leading `--color <when>`) is a
//! subcommand name; everything else starts the GUI (`lector ./status` opens a folder named
//! `status`). Subcommands: `status [PATH]` (gix only, no core or database), `changes
//! [--staged | --unstaged | --branch BASE | --range A..B] [--no-risk] [-C DIR]`, `diff
//! <changeset> [--file P] [--moved] [-U N]`, `commands [--category C]`, `run <id> [--args JSON |
//! @file | -] [--allow-writes]`, `activity [-n N] [--all]`, `undo <id> [--force]`, `mcp [PATH]
//! [--allow-writes]`, `health [-n N]`. Global `--color auto|always|never` (auto: stdout is a
//! terminal, `NO_COLOR` empty or unset, `TERM` not `dumb`); every subcommand except `mcp` has
//! `--json`. Changesets are review keys (`worktree`, `staged`, `unstaged`, `untracked`,
//! `commit:<rev>`, `range:<a>..<b>`, `branch:<base>...<branch>`), `A...B` (merge base), `A..B`,
//! or one revision (that commit against its first parent). `run` refuses a command with
//! `writes` unless `--allow-writes` (nothing runs, nothing is journaled); `undo` is itself the
//! explicit action. The journal records `Source::Cli` / `Source::Mcp`. The headless registry is
//! assembled in `session::open_core` (language servers disabled); commands of other crates that
//! should exist without a window are registered there.
//!
//! # `--json` formats (schema 1)
//!
//! Every document is an object with `"schema": 1` ([`JSON_SCHEMA_VERSION`], bumped only for a
//! breaking change). Paths are repository-relative, lines 0-based, ranges `[start, end)`; floats
//! are rounded to 3 decimals.
//!
//! ```text
//! status    {root, branch|null, detached, head|null, head_short|null, upstream|null, ahead, behind,
//!            operation: null | {kind: merge|rebase|cherry-pick|revert|am, description, interactive,
//!                               step: [n, total]|null},
//!            counts: {staged, unstaged, untracked, conflicted}, clean}
//! changes   {changeset, title, files: [{path, old_path?, kind: added|modified|deleted|renamed|copied|typechange,
//!             added?, removed?, binary?, viewed?, risk?: {score, level: low|medium|high,
//!             reasons: [{kind, weight, message, line?, dampening?}]}}],   // riskiest first; by path with --no-risk
//!            totals: {files, added, removed, high, medium, low}, risk_skipped?}  // > MAX_RISK_FILES or --no-risk
//! diff      {changeset, title, files: [<git.show_diff file object: path, kind, added, removed,
//!             hunks: [{header, old, new, lines: [" ctx" | "-old" | "+new"]}] | binary | image | too_large>,
//!             moved?: [{old, new, reindented}]],
//!            moves?: [{from: {path, lines}, to: {path, lines}, reindented}]}  // only with --moved
//! commands  {commands: [CommandSpec::to_json(): id, title, category, args (JSON Schema), writes?,
//!            destructive?, undoable?, not_undoable?, ui_action?, ui_flow?,
//!            sources: all|interactive|ui_only, network?, credentials?, steers_agent?,
//!            answers_for_user?]}   // only commands the CLI may run (`list_for(Source::Cli)`)
//! run       {ok, command, summary, data, paths, journal_id?, undoable?}
//! activity  {entries: [{id, time (RFC 3339 UTC), command, source: ui|cli|mcp|agent|internal, summary,
//!             repo, paths, args, undoable, undone, undo_of?, undone_by?}]}  // newest first; current repo unless --all
//! undo      {undone: <id>, entry: <the entry the undo recorded>}
//! health    {sessions, running, clean, abnormal, panics, crash_free_rate: 0..1|null,
//!            by_version: [{version, sessions, clean, abnormal, panics}], crash_reports, crash_dir,
//!            last_crashes: [{file, path, time, version, os, thread, location, message}],
//!            state: null|"<line>"}                                   // the state store's problem, if any
//! ```
//!
//! `risk.reasons[].kind` is the snake_case `lector_review::risk::RiskKind`. Moved blocks come from
//! `lector_diff::moves` (within a file and across the changeset, the latter computed over the
//! whole changeset even with `--file`); in human diffs moved lines start with `<` / `>`.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal as _, Write};
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::Parser as _;

mod args;
mod changes;
mod health;
mod journal;
mod mcp;
mod out;
mod registry;
mod session;
mod status;

pub use args::{Cli, ColorChoice, Command};
pub use changes::{parse_changeset, MAX_RISK_FILES};
pub use out::{Io, Style};

/// Version of the `--json` output formats, emitted as `"schema"` in every document.
pub const JSON_SCHEMA_VERSION: u32 = 1;

/// The subcommand names (first argument of `lector <subcommand>`).
pub const SUBCOMMANDS: &[&str] = &[
    "status", "changes", "diff", "commands", "run", "activity", "undo", "mcp", "health",
];

/// Whether `arg` (the first argument after the program name) selects a CLI subcommand
/// rather than a path for the GUI.
pub fn is_subcommand(arg: &OsStr) -> bool {
    arg.to_str().is_some_and(|a| SUBCOMMANDS.contains(&a))
}

/// Whether the command line (program name first) is a CLI invocation: its first argument,
/// after an optional leading `--color <when>`, is a subcommand name.
pub fn is_cli_invocation(args: &[OsString]) -> bool {
    let mut rest = args.iter().skip(1);
    let mut first = rest.next();
    if let Some(a) = first {
        match a.to_str() {
            Some("--color") => {
                rest.next(); // the value
                first = rest.next();
            }
            Some(s) if s.starts_with("--color=") => first = rest.next(),
            _ => {}
        }
    }
    first.is_some_and(|a| is_subcommand(a))
}

/// Askpass helper mode: when git or ssh run Lector's askpass helper program (the Unix shim
/// passes the hidden `--lector-askpass-helper` flag first; on Windows the hard link is named
/// `lector-askpass.exe`), forwards the prompt to the running app's askpass server, prints the
/// answer and returns the exit code. The environment alone never selects this mode, so a git
/// hook that runs `lector <path>` during a sync opens Lector as usual.
/// `None` for every other invocation. Both binaries call it first thing in `main`, before
/// [`is_cli_invocation`]; it never touches GUI code. See `lector_process::askpass`.
pub fn run_askpass_helper(args: &[OsString]) -> Option<ExitCode> {
    let prompt = lector_process::askpass::askpass_prompt_from_args(args)?;
    let code = lector_process::askpass::run_askpass_client(&prompt);
    Some(ExitCode::from(u8::try_from(code).unwrap_or(2)))
}

/// Runs the CLI with the process's real streams. `args` includes the program name.
pub fn run_args<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    // No `lock()`: the MCP server writes from its own thread and the lock is not shared.
    // Crash reports for the headless paths too (CLI and `lector mcp`): local files only.
    // Not in `run`, which tests call in-process.
    lector_app::health::install_panic_hook();
    let mut out = io::stdout();
    let tty = out.is_terminal();
    let mut err = io::stderr();
    let code = run(args, &mut out, &mut err, tty);
    let _ = out.flush();
    ExitCode::from(code)
}

/// Runs the CLI against explicit streams (tests). Returns the exit code: 0 ok, 1 error,
/// 2 usage error.
///
/// `lector mcp` ignores `out`: it speaks on the process's stdin and stdout.
pub fn run<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write, stdout_is_tty: bool) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    init_logging();
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let code = match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                // `lector` / `lector diff` without what it needs: usage help on stderr.
                _ => 2,
            };
            let text = e.render().to_string();
            let _ = if code == 0 {
                write!(out, "{text}")
            } else {
                write!(err, "{text}")
            };
            return code;
        }
    };
    let mut io = Io {
        out,
        err,
        style: Style::resolve(cli.color, stdout_is_tty),
    };
    let result = match &cli.command {
        Command::Status(a) => status::run(a, &mut io),
        Command::Changes(a) => changes::run_changes(a, &mut io),
        Command::Diff(a) => changes::run_diff(a, &mut io),
        Command::Commands(a) => registry::run_commands(a, &mut io),
        Command::Run(a) => registry::run_command(a, &mut io),
        Command::Activity(a) => journal::run_activity(a, &mut io),
        Command::Undo(a) => journal::run_undo(a, &mut io),
        Command::Mcp(a) => mcp::run(a),
        Command::Health(a) => health::run(a, &mut io),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            // `lector changes | head`: the reader went away; that is not an error.
            if e.chain().any(|c| {
                c.downcast_ref::<io::Error>()
                    .is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
            }) {
                return 0;
            }
            let _ = writeln!(io.err, "lector: error: {e:#}");
            1
        }
    }
}

/// Diagnostics go to stderr, and only when asked for (`RUST_LOG=lector=debug`): stdout is
/// reserved for results (and, for `lector mcp`, for protocol messages).
fn init_logging() {
    if std::env::var_os("RUST_LOG").is_none() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Starts the GUI binary that sits next to the running executable with `args` (everything
/// after the program name) and returns without waiting. Used by the `lector-cli` console
/// binary (shipped as `lector.com` on Windows) for `lector .` / `lector path/to/file:12`.
pub fn launch_gui(args: &[OsString]) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot locate the executable's folder",
        )
    })?;
    let gui = dir.join(format!("lector{}", std::env::consts::EXE_SUFFIX));
    if !gui.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "the Lector app ({}) was not found next to this program",
                gui.display()
            ),
        ));
    }
    let mut cmd = std::process::Command::new(gui);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: the window outlives this console.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn cli_invocations() {
        assert!(is_cli_invocation(&args(&["lector", "status"])));
        assert!(is_cli_invocation(&args(&[
            "lector", "--color", "never", "changes", "--json"
        ])));
        assert!(is_cli_invocation(&args(&[
            "lector",
            "--color=always",
            "diff",
            "HEAD"
        ])));
        assert!(!is_cli_invocation(&args(&["lector"])));
        assert!(!is_cli_invocation(&args(&["lector", "."])));
        assert!(!is_cli_invocation(&args(&["lector", "./status"])));
        assert!(!is_cli_invocation(&args(&[
            "lector",
            "--theme",
            "Lector Light"
        ])));
        assert!(!is_cli_invocation(&args(&["lector", "--color"])));
        assert!(!is_cli_invocation(&args(&["lector", "src/main.rs:12"])));
    }
}
