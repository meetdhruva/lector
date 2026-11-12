//! Scripted user journeys, end to end through the headless core.
//!
//! The gaps users hit in early releases were whole-journey gaps, not single-feature bugs, so
//! these tests walk the main journeys the way the CLI (`lector run`), MCP and the GUI do: through
//! the typed command registry of `lector-app`, on fixtures built with the `git` CLI in a temp
//! folder. They run on Linux, macOS and Windows:
//!
//! * fixture files have portable names and `core.autocrlf` is off, text comparisons go through
//!   [`fixture::norm`] (CRLF-tolerant), temp folders are canonicalized with `dunce`;
//! * nothing reads the user's real state: the core uses an in-memory database and a temp data
//!   directory, git ignores global and system configuration.
//!
//! Journeys: [`project_flow`] (open, status, changes, diff, stage a hunk, commit, activity,
//! undo), [`documents`] (edit through the document store, save, status, discard, undo the
//! discard), [`review`] (queue order, reviewed hunks survive a new commit), [`comments`]
//! (comments stay anchored through unrelated edits and new commits), [`cli`] (the same journey
//! through the `lector-cli` binary with the stable `--json` schemas), [`sync`] (commit, push,
//! fetch and pull between two clones of a bare remote; rejected push; rebase pull conflicts).

mod cli;
mod comments;
mod documents;
mod fixture;
mod project_flow;
mod review;
mod sync;
