//! Command-line definition (clap).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// When to colorize human output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Only when stdout is a terminal, `NO_COLOR` is unset and `TERM` is not `dumb`.
    Auto,
    /// Always.
    Always,
    /// Never.
    Never,
}

/// `lector <subcommand>`: Lector without the window.
#[derive(Parser, Debug)]
#[command(
    name = "lector",
    version,
    about = "Lector without the window: git status, risk-ranked changes, diffs, the command registry, \
             the activity journal and an MCP server.",
    long_about = "Lector without the window. Everything here runs headless (no GUI is started) \
and uses the same core as the app: the same commands, the same activity journal, the same undo.\n\n\
Human output is compact; add --json for stable machine-readable output. Exit codes: 0 ok, \
1 error, 2 usage error. Colors only on a terminal (never with NO_COLOR set).",
    disable_help_subcommand = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Colorize human output.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto, value_name = "WHEN")]
    pub color: ColorChoice,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Branch, ahead/behind, operation in progress and change counts of a repository.
    Status(StatusArgs),
    /// Changed files with line stats and a risk score (riskiest first).
    Changes(ChangesArgs),
    /// Hunks of a changeset; moved blocks can be marked.
    Diff(DiffArgs),
    /// The command registry: every command with its argument schema.
    Commands(CommandsArgs),
    /// Run one registry command.
    Run(RunArgs),
    /// The activity journal: what was written to files or the repository, and by whom.
    Activity(ActivityArgs),
    /// Undo a journal entry.
    Undo(UndoArgs),
    /// Model Context Protocol server on stdin/stdout (JSON-RPC), for AI agents and tools.
    Mcp(McpArgs),
    /// Local crash log and session health: sessions, crash-free rate and the latest crash reports.
    Health(HealthArgs),
}

/// `lector status`.
#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Repository folder (default: the current directory).
    pub path: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// `lector changes`.
#[derive(Args, Debug)]
pub struct ChangesArgs {
    /// Staged changes (HEAD to index).
    #[arg(long, conflicts_with_all = ["unstaged", "branch", "range"])]
    pub staged: bool,
    /// Unstaged changes (index to files).
    #[arg(long, conflicts_with_all = ["staged", "branch", "range"])]
    pub unstaged: bool,
    /// The current branch against the merge base with BASE (what a pull request would show).
    #[arg(long, value_name = "BASE", conflicts_with_all = ["staged", "unstaged", "range"])]
    pub branch: Option<String>,
    /// A revision range: A..B (two commits), or A...B (merge base of A and B to B).
    #[arg(long, value_name = "A..B", conflicts_with_all = ["staged", "unstaged", "branch"])]
    pub range: Option<String>,
    /// Do not compute risk scores (faster on huge changesets).
    #[arg(long)]
    pub no_risk: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// Run in this folder instead of the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub directory: Option<PathBuf>,
}

/// `lector diff`.
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// What to diff: worktree, staged, unstaged, untracked, commit:<sha>, range:<a>..<b>,
    /// branch:<base>...<branch>, or simply a revision (HEAD~1, a sha, a tag) or A..B / A...B.
    #[arg(value_name = "CHANGESET")]
    pub changeset: String,
    /// Only this file (repository-relative path).
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
    /// Mark blocks of lines that were moved (within a file or between files).
    #[arg(long)]
    pub moved: bool,
    /// Lines of context around each hunk.
    #[arg(short = 'U', long, value_name = "N", default_value_t = 3)]
    pub context: u32,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// Run in this folder instead of the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub directory: Option<PathBuf>,
}

/// `lector commands`.
#[derive(Args, Debug)]
pub struct CommandsArgs {
    /// Only this category (Git, Documents, Search, ...; case-insensitive).
    #[arg(long, value_name = "NAME")]
    pub category: Option<String>,
    /// Machine-readable output, including each command's argument JSON schema.
    #[arg(long)]
    pub json: bool,
}

/// `lector run`.
#[derive(Args, Debug)]
pub struct RunArgs {
    /// Command id, e.g. git.list_changes (see `lector commands`).
    #[arg(value_name = "COMMAND")]
    pub id: String,
    /// Arguments as a JSON object, e.g. '{"changeset":"staged"}'. Use @file.json to read a
    /// file, or - for stdin.
    #[arg(long, value_name = "JSON")]
    pub args: Option<String>,
    /// Allow commands that write to files or the repository (journaled, usually undoable).
    #[arg(long)]
    pub allow_writes: bool,
    /// Print the whole result as one JSON object.
    #[arg(long)]
    pub json: bool,
    /// Run in this folder instead of the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub directory: Option<PathBuf>,
}

/// `lector activity`.
#[derive(Args, Debug)]
pub struct ActivityArgs {
    /// How many entries to show (newest first).
    #[arg(short = 'n', long, value_name = "N", default_value_t = 20)]
    pub limit: usize,
    /// Entries of every repository (default: only the repository of the current directory,
    /// when there is one).
    #[arg(long)]
    pub all: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// Run in this folder instead of the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub directory: Option<PathBuf>,
}

/// `lector undo`.
#[derive(Args, Debug)]
pub struct UndoArgs {
    /// Journal entry id (see `lector activity`).
    pub id: u64,
    /// Restore even when the files changed since the operation (the current state is
    /// backed up first, so this is undoable too).
    #[arg(long)]
    pub force: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// `lector mcp`.
#[derive(Args, Debug)]
pub struct McpArgs {
    /// Project folder the tools work on (default: the current directory).
    pub path: Option<PathBuf>,
    /// Also expose commands that write to files or the repository. Every write is journaled
    /// with origin "mcp".
    #[arg(long)]
    pub allow_writes: bool,
}

/// `lector health`.
#[derive(Args, Debug)]
pub struct HealthArgs {
    /// How many crash reports to list (newest first).
    #[arg(short = 'n', long, value_name = "N", default_value_t = 5)]
    pub limit: usize,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}
