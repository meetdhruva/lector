//! Child process supervision for Lector: spawn, stream, cancel and kill whole process
//! trees; askpass prompts.
//!
//! Child processes Lector runs (git, language servers, agents, helpers) go through a
//! [`Supervisor`] (integrated terminals do not: `lector-terminal` runs them in a PTY with
//! `portable-pty` and kills their trees itself):
//!
//! * [`ProcessCommand`] describes it: program, argv (never a shell), working directory,
//!   environment additions/removals, stdin ([`StdinMode`]), label and [`ProcessKind`].
//! * Each child gets its own process tree container: a new session / process group on Unix
//!   (`setsid`, so `killpg` reaches grandchildren and ssh cannot grab Lector's terminal), a
//!   Job Object on Windows (with `CREATE_NO_WINDOW`, so no console flashes). Implemented with
//!   the `process-wrap` crate; this crate has no `unsafe`.
//! * [`Supervisor::run`] / [`Supervisor::run_streaming`] run to completion: both pipes are
//!   drained concurrently on their own threads, lines (`\n`, `\r\n` and `\r` progress
//!   updates) are delivered as they arrive through a bounded channel, output is captured up
//!   to a limit, and a [`CancelToken`] or timeout terminates the tree (`SIGTERM`, then
//!   `SIGKILL` of the whole group after a grace period; the job is terminated on Windows).
//! * [`Supervisor::spawn`] returns a [`ChildHandle`] for long-lived processes (raw pipes or
//!   line events). Dropping the handle kills the tree; [`Supervisor::shutdown`] kills every
//!   tree on app exit; [`Supervisor::list`] shows what runs.
//! * [`askpass`]: a local IPC server and the helper-mode client that let git and ssh ask the
//!   GUI for credentials.
//!
//! No async runtime: everything is blocking and meant for background threads.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod askpass;
mod cancel;
mod child;
mod command;
mod error;
mod lines;
mod run;
mod supervisor;

pub use cancel::CancelToken;
pub use child::{ChildHandle, Killer, ProcessInfo};
pub use command::{OutputMode, ProcessCommand, ProcessKind, StdinMode};
pub use error::{ProcessError, Result};
pub use lines::{Line, LineEnd, LineSplitter, Stream, MAX_LINE_BYTES};
pub use run::{run, run_streaming, RunOptions, RunOutput};
pub use supervisor::Supervisor;
