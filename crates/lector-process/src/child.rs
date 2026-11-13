//! Spawning and the process-tree handle.

use std::io::{Read, Write as _};
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::Receiver;
use process_wrap::std::{StdChildWrapper, StdCommandWrap};

use crate::command::{OutputMode, ProcessCommand, ProcessKind, StdinMode};
use crate::error::{ProcessError, Result};
use crate::lines::{Line, LineSplitter, Stream};
use crate::supervisor::SupInner;

/// `SIGTERM`; the same number on every Unix Lector supports (Linux, macOS, BSDs).
#[cfg(unix)]
const SIGTERM: i32 = 15;

/// How long a hard-killed process may take to be reaped before a background thread
/// takes over (a process in uninterruptible sleep cannot die at once).
const REAP_WAIT: Duration = Duration::from_secs(2);

/// A snapshot of a supervised process for listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Supervisor-assigned id (unique per [`Supervisor`](crate::Supervisor), never reused).
    pub id: u64,
    /// OS process id of the main process (also its process group id on Unix).
    pub pid: u32,
    /// Human label.
    pub label: String,
    /// What it is.
    pub kind: ProcessKind,
    /// The program.
    pub program: String,
    /// When it was started.
    pub started: SystemTime,
    /// `false` once the main process has exited (its handle is still alive).
    pub running: bool,
}

struct State {
    child: Box<dyn StdChildWrapper>,
    status: Option<ExitStatus>,
    /// Kill what is left of the tree the moment the main process is reaped.
    sweep_on_exit: bool,
}

/// State shared by a [`ChildHandle`], the supervisor's registry (weakly) and reaper threads.
pub(crate) struct Shared {
    pub(crate) id: u64,
    pub(crate) pid: u32,
    pub(crate) label: String,
    pub(crate) kind: ProcessKind,
    pub(crate) program: String,
    pub(crate) started: SystemTime,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn info(&self) -> ProcessInfo {
        ProcessInfo {
            id: self.id,
            pid: self.pid,
            label: self.label.clone(),
            kind: self.kind,
            program: self.program.clone(),
            started: self.started,
            running: self.lock().status.is_none(),
        }
    }

    /// Non-blocking exit check. Once the main process is reaped no signal is ever sent to
    /// its group again (its id could be reused), except the sweep done right here.
    pub(crate) fn try_wait(&self) -> std::io::Result<Option<ExitStatus>> {
        let mut state = self.lock();
        if let Some(status) = state.status {
            return Ok(Some(status));
        }
        let status = state.child.try_wait()?;
        if let Some(status) = status {
            state.status = Some(status);
            if state.sweep_on_exit {
                // ESRCH (nothing left) is the normal case.
                let _ = state.child.start_kill();
            }
        }
        Ok(status)
    }

    fn exited(&self) -> bool {
        self.lock().status.is_some()
    }

    /// Asks the tree to stop: `SIGTERM` to the process group on Unix. Windows has no
    /// equivalent for console-less children; closing stdin (done by the caller) is the
    /// polite request there.
    fn signal_terminate(&self) {
        let mut state = self.lock();
        state.sweep_on_exit = true;
        // No early `return`: with the Unix block compiled out it would be a needless
        // trailing return (clippy on Windows).
        #[cfg(unix)]
        if state.status.is_none() {
            let _ = state.child.signal(SIGTERM);
        }
    }

    /// Hard-kills the whole tree (`SIGKILL` to the group / `TerminateJobObject`).
    pub(crate) fn kill_tree(&self) {
        let mut state = self.lock();
        state.sweep_on_exit = true;
        if state.status.is_none() {
            if let Err(err) = state.child.start_kill() {
                tracing::debug!("kill {} ({}): {err}", self.label, self.pid);
            }
        }
    }

    /// Polls until the main process is reaped or `limit` passes.
    pub(crate) fn wait_for(&self, limit: Duration) -> std::io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + limit;
        let mut pause = Duration::from_millis(1);
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(pause.min(deadline - now));
            pause = (pause * 2).min(Duration::from_millis(20));
        }
    }

    /// Graceful stop, then a hard kill of the tree after `grace`; reaps the main process.
    pub(crate) fn terminate(self: &Arc<Self>, grace: Duration) -> Option<ExitStatus> {
        self.signal_terminate();
        if let Ok(Some(status)) = self.wait_for(grace) {
            return Some(status);
        }
        self.kill_tree();
        self.reap()
    }

    /// Waits a little for a killed process; hands it to a reaper thread if it lingers,
    /// so no zombie is left behind.
    fn reap(self: &Arc<Self>) -> Option<ExitStatus> {
        match self.wait_for(REAP_WAIT) {
            Ok(Some(status)) => Some(status),
            _ => {
                let me = self.clone();
                let spawned = std::thread::Builder::new()
                    .name("lector-reaper".into())
                    .spawn(move || {
                        while !matches!(me.wait_for(Duration::from_secs(1)), Ok(Some(_)) | Err(_)) {
                        }
                    });
                if let Err(err) = spawned {
                    tracing::warn!("cannot start a reaper thread: {err}");
                }
                None
            }
        }
    }
}

/// A running (or exited) supervised process tree.
///
/// Dropping the handle kills the tree if the main process still runs (and, when the
/// command's `kill_tree_on_exit` is set, whatever it left behind). The process is listed by
/// its [`Supervisor`](crate::Supervisor) while the handle lives.
pub struct ChildHandle {
    shared: Arc<Shared>,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    registry: Weak<SupInner>,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("id", &self.shared.id)
            .field("pid", &self.shared.pid)
            .field("label", &self.shared.label)
            .finish_non_exhaustive()
    }
}

/// A cloneable way to stop a process from another thread without owning its handle.
/// Holds the process weakly: once the handle is gone, it does nothing.
#[derive(Clone)]
pub struct Killer {
    shared: Weak<Shared>,
}

impl std::fmt::Debug for Killer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Killer")
    }
}

impl Killer {
    /// Hard-kills the tree. Returns `false` if the handle is already gone.
    pub fn kill(&self) -> bool {
        match self.shared.upgrade() {
            Some(s) => {
                s.kill_tree();
                true
            }
            None => false,
        }
    }

    /// Graceful stop, hard kill after `grace` (blocking).
    pub fn terminate(&self, grace: Duration) -> bool {
        match self.shared.upgrade() {
            Some(s) => {
                s.terminate(grace);
                true
            }
            None => false,
        }
    }
}

impl ChildHandle {
    /// Supervisor id.
    pub fn id(&self) -> u64 {
        self.shared.id
    }

    /// OS process id of the main process.
    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// Label given at spawn.
    pub fn label(&self) -> &str {
        &self.shared.label
    }

    /// Kind given at spawn.
    pub fn kind(&self) -> ProcessKind {
        self.shared.kind
    }

    /// Listing snapshot.
    pub fn info(&self) -> ProcessInfo {
        self.shared.info()
    }

    /// A [`Killer`] for other threads.
    pub fn killer(&self) -> Killer {
        Killer {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// The stdin pipe ([`StdinMode::Piped`] only; once).
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// The raw stdout pipe ([`OutputMode::Piped`]; once). Read it continuously, or use
    /// [`lines`](Self::lines), or the child blocks when the pipe buffer fills.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// The raw stderr pipe (once).
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// Line events of every pipe not taken yet (both drained concurrently on their own
    /// threads), through a channel bounded to `capacity` lines. The channel disconnects
    /// when all streams reached EOF.
    pub fn lines(&mut self, capacity: usize) -> Receiver<Line> {
        let (tx, rx) = crossbeam_channel::bounded(capacity.max(1));
        if let Some(out) = self.stdout.take() {
            let tx = tx.clone();
            spawn_reader(Stream::Stdout, out, None, move |line| tx.send(line).is_ok());
        }
        if let Some(err) = self.stderr.take() {
            spawn_reader(Stream::Stderr, err, None, move |line| tx.send(line).is_ok());
        }
        rx
    }

    /// Non-blocking: the exit status once the main process has exited.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.shared.try_wait()?)
    }

    /// Blocks until the main process exits.
    pub fn wait(&mut self) -> Result<ExitStatus> {
        loop {
            if let Some(status) = self.shared.wait_for(Duration::from_secs(1))? {
                return Ok(status);
            }
        }
    }

    /// Blocks up to `limit`; `None` if it still runs.
    pub fn wait_timeout(&mut self, limit: Duration) -> Result<Option<ExitStatus>> {
        Ok(self.shared.wait_for(limit)?)
    }

    /// Graceful stop (close stdin; `SIGTERM` to the group on Unix), then a hard kill of the
    /// whole tree after `grace`. Blocking. `None` if the process could not be reaped yet.
    pub fn terminate(&mut self, grace: Duration) -> Option<ExitStatus> {
        drop(self.stdin.take());
        self.shared.terminate(grace)
    }

    /// Hard-kills the whole tree at once and reaps the main process.
    pub fn kill(&mut self) -> Option<ExitStatus> {
        self.shared.kill_tree();
        self.shared.reap()
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if !self.shared.exited() {
            self.shared.kill_tree();
            self.shared.reap();
        }
        if let Some(reg) = self.registry.upgrade() {
            reg.forget(self.shared.id);
        }
    }
}

/// Spawns `cmd` under `registry`. Stdio follows the command (stdout/stderr may be
/// overridden to piped by runs).
pub(crate) fn spawn(
    cmd: &ProcessCommand,
    id: u64,
    registry: &Arc<SupInner>,
) -> Result<(ChildHandle, Arc<Shared>)> {
    let mut std_cmd = cmd.to_std();
    std_cmd.stdin(match cmd.stdin {
        StdinMode::Null => Stdio::null(),
        StdinMode::Piped | StdinMode::Bytes(_) => Stdio::piped(),
    });
    std_cmd.stdout(stdio(cmd.stdout));
    std_cmd.stderr(stdio(cmd.stderr));

    let mut wrap = StdCommandWrap::from(std_cmd);
    #[cfg(unix)]
    {
        if cmd.new_session {
            wrap.wrap(process_wrap::std::ProcessSession);
        } else {
            wrap.wrap(process_wrap::std::ProcessGroup::leader());
        }
    }
    #[cfg(windows)]
    {
        // `CreationFlags` must be added before `JobObject` (process-wrap merges them).
        if !cmd.console_window {
            wrap.wrap(process_wrap::std::CreationFlags(
                windows::Win32::System::Threading::CREATE_NO_WINDOW,
            ));
        }
        wrap.wrap(process_wrap::std::JobObject);
    }
    let program = cmd.program.to_string_lossy().into_owned();
    let mut child = wrap.spawn().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ProcessError::NotFound {
                program: program.clone(),
            }
        } else {
            ProcessError::Spawn {
                program: program.clone(),
                source,
            }
        }
    })?;
    let stdin = child.stdin().take();
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();
    let pid = child.id();
    let shared = Arc::new(Shared {
        id,
        pid,
        label: cmd.label.clone(),
        kind: cmd.kind,
        program,
        started: SystemTime::now(),
        state: Mutex::new(State {
            child,
            status: None,
            sweep_on_exit: cmd.kill_tree_on_exit,
        }),
    });
    let mut handle = ChildHandle {
        shared: shared.clone(),
        stdin,
        stdout,
        stderr,
        registry: Arc::downgrade(registry),
    };
    if let StdinMode::Bytes(bytes) = &cmd.stdin {
        if let Some(mut pipe) = handle.stdin.take() {
            let bytes = bytes.clone();
            // A helper thread, so large input cannot deadlock against full output pipes.
            // A broken pipe means the child exited early; its status explains why.
            let spawned = std::thread::Builder::new()
                .name("lector-stdin".into())
                .spawn(move || {
                    let _ = pipe.write_all(&bytes);
                });
            if let Err(err) = spawned {
                return Err(ProcessError::Io(err));
            }
        }
    }
    tracing::debug!(id, pid, kind = cmd.kind.as_str(), "spawned {}", cmd.label);
    Ok((handle, shared))
}

fn stdio(mode: OutputMode) -> Stdio {
    match mode {
        OutputMode::Piped => Stdio::piped(),
        OutputMode::Null => Stdio::null(),
        OutputMode::Inherit => Stdio::inherit(),
    }
}

/// Bytes kept from one stream, up to a limit.
#[derive(Debug, Default)]
pub(crate) struct Capture {
    pub(crate) bytes: Vec<u8>,
    pub(crate) limit: usize,
    pub(crate) truncated: bool,
}

impl Capture {
    pub(crate) fn new(limit: usize) -> Self {
        Capture {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let room = self.limit.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes
            .extend_from_slice(&chunk[..room.min(chunk.len())]);
    }
}

/// Reads `pipe` to EOF on its own thread: raw bytes into `capture` (if any), lines to
/// `deliver` (`None` = capture only). `deliver` returning `false` stops line delivery
/// (the receiver is gone) but the pipe is still drained so the child never blocks.
pub(crate) fn spawn_reader<R: Read + Send + 'static>(
    stream: Stream,
    mut pipe: R,
    capture: Option<Arc<Mutex<Capture>>>,
    mut deliver: impl FnMut(Line) -> bool + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name(match stream {
            Stream::Stdout => "lector-stdout".into(),
            Stream::Stderr => "lector-stderr".into(),
        })
        .spawn(move || {
            let mut splitter = LineSplitter::default();
            let mut buf = vec![0u8; 16 * 1024];
            let mut delivering = true;
            loop {
                let n = match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if let Some(cap) = &capture {
                    cap.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(&buf[..n]);
                }
                if delivering {
                    splitter.feed(&buf[..n], &mut |bytes, end| {
                        if delivering {
                            delivering = deliver(Line { stream, bytes, end });
                        }
                    });
                }
            }
            if delivering {
                splitter.finish(&mut |bytes, end| {
                    if delivering {
                        delivering = deliver(Line { stream, bytes, end });
                    }
                });
            }
        });
    if let Err(err) = spawned {
        tracing::warn!("cannot start an output reader thread: {err}");
    }
}
