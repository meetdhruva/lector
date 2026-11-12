//! Spawning `git` through `lector-process` (process tree kill, cancellation, streaming).

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;

use lector_process::{
    CancelToken, Line, ProcessCommand, ProcessError, ProcessKind, RunOptions, RunOutput, StdinMode,
    Supervisor,
};

use crate::error::{Error, Result};

/// Options of network and branch operations: credentials environment (the app's askpass
/// server), cancellation and an optional timeout.
///
/// `Debug` prints environment variable names only (values may be tokens).
#[derive(Clone, Default)]
pub struct SyncOptions {
    /// Added to git's environment (e.g. `lector_process::askpass::AskpassServer::env`).
    /// Never put credentials in argv; `credential.helper` is never overridden.
    pub env: Vec<(OsString, OsString)>,
    /// Cancelling terminates git and everything it started (ssh, helpers).
    pub cancel: Option<CancelToken>,
    /// Overall limit; `None` = no limit.
    pub timeout: Option<Duration>,
    /// The supervisor to run under (default: [`Supervisor::global`]).
    pub supervisor: Option<Supervisor>,
}

impl std::fmt::Debug for SyncOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncOptions")
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("cancel", &self.cancel.is_some())
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl SyncOptions {
    /// Sets [`SyncOptions::cancel`].
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Sets [`SyncOptions::env`].
    pub fn with_env(mut self, env: Vec<(OsString, OsString)>) -> Self {
        self.env = env;
        self
    }
}

/// A `git -C <workdir>` invocation, terminal-like (the user's config, hooks, credential
/// helpers), non-interactive (no editor, no terminal prompt), messages in English so
/// failures can be classified.
pub(crate) fn git(workdir: &Path, args: &[&OsStr], label: &str) -> ProcessCommand {
    ProcessCommand::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["--no-pager", "-c", "core.quotepath=off"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OPTIONAL_LOCKS")
        .kind(ProcessKind::Git)
        .label(label)
}

/// Runs `cmd` with `opts`; returns the output whatever the exit status.
pub(crate) fn run(
    cmd: ProcessCommand,
    opts: &SyncOptions,
    what: &'static str,
    on_line: Option<&mut dyn FnMut(&Line)>,
) -> Result<RunOutput> {
    let cmd = cmd.envs(opts.env.iter().map(|(k, v)| (k, v)));
    let run_opts = RunOptions {
        cancel: opts.cancel.clone(),
        timeout: opts.timeout,
        ..RunOptions::default()
    };
    let sup = opts.supervisor.as_ref().unwrap_or(Supervisor::global());
    let res = match on_line {
        Some(cb) => sup.run_streaming(&cmd, &run_opts, cb),
        None => sup.run(&cmd, &run_opts),
    };
    res.map_err(|err| map_process_error(err, what))
}

/// Runs a read-only `git` command (`GIT_OPTIONAL_LOCKS=0`); stdout on success.
pub(crate) fn read(workdir: &Path, args: &[&str], what: &'static str) -> Result<Vec<u8>> {
    let os: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let cmd = git(workdir, &os, what).env("GIT_OPTIONAL_LOCKS", "0");
    let out = run(cmd, &SyncOptions::default(), what, None)?;
    if !out.success() {
        return Err(command_error(args.iter().map(OsStr::new), &out));
    }
    Ok(out.stdout)
}

/// Runs a local writing `git` command with stdin; output whatever the exit status.
pub(crate) fn write_raw(
    workdir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    what: &'static str,
) -> Result<RunOutput> {
    let os: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let mut cmd = git(workdir, &os, what);
    if let Some(input) = stdin {
        cmd = cmd.stdin(StdinMode::Bytes(input.to_vec()));
    }
    run(cmd, &SyncOptions::default(), what, None)
}

pub(crate) fn map_process_error(err: ProcessError, what: &'static str) -> Error {
    match err {
        ProcessError::NotFound { .. } => Error::GitNotInstalled(what),
        ProcessError::Cancelled => Error::Cancelled,
        ProcessError::TimedOut { .. } => Error::TimedOut,
        ProcessError::Spawn { source, .. } => Error::Io(source),
        ProcessError::Io(e) => Error::Io(e),
        ProcessError::ShuttingDown => Error::Io(std::io::Error::other("Lector is shutting down")),
    }
}

/// `Error::GitCommand` with git's output (stderr, plus stdout when git reports there).
pub(crate) fn command_error<'a>(
    args: impl IntoIterator<Item = &'a OsStr>,
    out: &RunOutput,
) -> Error {
    let stderr = crate::sync::redact_credentials(out.stderr_text().trim());
    let stdout = crate::sync::redact_credentials(out.stdout_text().trim());
    let message = match (stderr.is_empty(), stdout.is_empty()) {
        (false, true) => stderr,
        (true, false) => stdout,
        (false, false) => format!("{stderr}\n{stdout}"),
        (true, true) => String::new(),
    };
    Error::GitCommand {
        command: args
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        status: out.status.to_string(),
        stderr: message,
    }
}
