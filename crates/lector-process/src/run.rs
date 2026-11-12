//! Run-to-completion with streaming output, cancellation and timeouts.

use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;

use crate::cancel::CancelToken;
use crate::child::{spawn_reader, Capture};
use crate::command::{OutputMode, ProcessCommand};
use crate::error::{ProcessError, Result};
use crate::lines::{Line, Stream};
use crate::supervisor::Supervisor;

/// Options of [`Supervisor::run`] / [`Supervisor::run_streaming`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Cancelling it terminates the tree; the run returns [`ProcessError::Cancelled`].
    pub cancel: Option<CancelToken>,
    /// Overall time limit; exceeding it terminates the tree ([`ProcessError::TimedOut`]).
    pub timeout: Option<Duration>,
    /// Time between the graceful stop request and the hard kill.
    pub grace: Duration,
    /// Bytes kept per stream in [`RunOutput`]; more is dropped (flagged `*_truncated`)
    /// but still streamed.
    pub capture_limit: usize,
    /// After the main process exited, how long to keep reading pipes that something it
    /// left behind still holds open (only matters with `kill_tree_on_exit(false)`).
    pub drain_timeout: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            cancel: None,
            timeout: None,
            grace: Duration::from_secs(2),
            capture_limit: 32 * 1024 * 1024,
            drain_timeout: Duration::from_secs(2),
        }
    }
}

impl RunOptions {
    /// Sets [`RunOptions::cancel`].
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Sets [`RunOptions::timeout`].
    pub fn with_timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }
}

/// Result of a completed run.
#[derive(Debug, Clone)]
pub struct RunOutput {
    /// Exit status of the main process.
    pub status: ExitStatus,
    /// Captured stdout (raw bytes, up to the limit).
    pub stdout: Vec<u8>,
    /// Captured stderr (raw bytes, up to the limit).
    pub stderr: Vec<u8>,
    /// Stdout exceeded [`RunOptions::capture_limit`].
    pub stdout_truncated: bool,
    /// Stderr exceeded [`RunOptions::capture_limit`].
    pub stderr_truncated: bool,
}

impl RunOutput {
    /// Exit status 0.
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// The exit code (`None` when killed by a signal).
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    /// Stdout as text (lossy).
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr as text (lossy).
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

impl Supervisor {
    /// Runs `cmd` to completion and captures its output (both pipes drained concurrently).
    pub fn run(&self, cmd: &ProcessCommand, opts: &RunOptions) -> Result<RunOutput> {
        self.run_inner(cmd, opts, None)
    }

    /// Runs `cmd` to completion, calling `on_line` for every line of stdout and stderr as
    /// it arrives (in arrival order; `\r` progress updates included), on the calling thread.
    pub fn run_streaming(
        &self,
        cmd: &ProcessCommand,
        opts: &RunOptions,
        on_line: &mut dyn FnMut(&Line),
    ) -> Result<RunOutput> {
        self.run_inner(cmd, opts, Some(on_line))
    }

    fn run_inner(
        &self,
        cmd: &ProcessCommand,
        opts: &RunOptions,
        mut on_line: Option<&mut dyn FnMut(&Line)>,
    ) -> Result<RunOutput> {
        if opts.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(ProcessError::Cancelled);
        }
        let cmd = cmd
            .clone()
            .stdout(OutputMode::Piped)
            .stderr(OutputMode::Piped);
        let started = Instant::now();
        let mut handle = self.spawn(&cmd)?;
        let streaming = on_line.is_some();
        // Bounded: a slow consumer slows the child down instead of growing memory.
        let (tx, rx) = crossbeam_channel::bounded::<Line>(256);
        let out_cap = Arc::new(Mutex::new(Capture::new(opts.capture_limit)));
        let err_cap = Arc::new(Mutex::new(Capture::new(opts.capture_limit)));
        for (stream, cap) in [(Stream::Stdout, &out_cap), (Stream::Stderr, &err_cap)] {
            let tx = tx.clone();
            let deliver = move |line: Line| !streaming || tx.send(line).is_ok();
            match stream {
                Stream::Stdout => {
                    if let Some(pipe) = handle.take_stdout() {
                        spawn_reader(stream, pipe, Some(cap.clone()), deliver);
                    }
                }
                Stream::Stderr => {
                    if let Some(pipe) = handle.take_stderr() {
                        spawn_reader(stream, pipe, Some(cap.clone()), deliver);
                    }
                }
            }
        }
        // Only the readers hold senders now: the channel disconnects when both reach EOF.
        drop(tx);

        let deadline = opts.timeout.map(|t| started + t);
        let mut status: Option<ExitStatus> = None;
        let mut exited_at: Option<Instant> = None;
        let mut pause = Duration::from_millis(1);
        loop {
            let mut wait = pause;
            if let Some(d) = deadline {
                wait = wait.min(d.saturating_duration_since(Instant::now()));
            }
            let streams_open = match rx.recv_timeout(wait) {
                Ok(line) => {
                    if let Some(cb) = on_line.as_mut() {
                        cb(&line);
                    }
                    pause = Duration::from_millis(1);
                    true
                }
                Err(RecvTimeoutError::Timeout) => {
                    pause = (pause * 2).min(Duration::from_millis(20));
                    true
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Both pipes are closed; only the exit status is missing.
                    std::thread::sleep(wait);
                    pause = (pause * 2).min(Duration::from_millis(20));
                    false
                }
            };
            if opts.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                handle.terminate(opts.grace);
                return Err(ProcessError::Cancelled);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                handle.terminate(opts.grace);
                return Err(ProcessError::TimedOut {
                    after: opts.timeout.unwrap_or_default(),
                });
            }
            if status.is_none() {
                status = handle.try_wait()?;
                if status.is_some() {
                    exited_at = Some(Instant::now());
                }
            }
            if let Some(st) = status {
                let drained =
                    !streams_open || exited_at.is_some_and(|t| t.elapsed() >= opts.drain_timeout);
                if drained {
                    // Deliver what is already queued.
                    if let Some(cb) = on_line.as_mut() {
                        while let Ok(line) = rx.try_recv() {
                            cb(&line);
                        }
                    }
                    let take = |cap: &Arc<Mutex<Capture>>| {
                        let mut c = cap.lock().unwrap_or_else(|e| e.into_inner());
                        (std::mem::take(&mut c.bytes), c.truncated)
                    };
                    let (stdout, stdout_truncated) = take(&out_cap);
                    let (stderr, stderr_truncated) = take(&err_cap);
                    return Ok(RunOutput {
                        status: st,
                        stdout,
                        stderr,
                        stdout_truncated,
                        stderr_truncated,
                    });
                }
            }
        }
    }
}

/// [`Supervisor::run`] on the [global](Supervisor::global) supervisor.
pub fn run(cmd: &ProcessCommand, opts: &RunOptions) -> Result<RunOutput> {
    Supervisor::global().run(cmd, opts)
}

/// [`Supervisor::run_streaming`] on the [global](Supervisor::global) supervisor.
pub fn run_streaming(
    cmd: &ProcessCommand,
    opts: &RunOptions,
    on_line: &mut dyn FnMut(&Line),
) -> Result<RunOutput> {
    Supervisor::global().run_streaming(cmd, opts, on_line)
}
