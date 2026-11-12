//! A terminal session: PTY + child shell + reader/parser/writer threads.
//!
//! # Threading model
//!
//! ```text
//!  PTY master ──read──▶ [reader thread] ──bounded chunks──▶ [parser thread] ──▶ Emulator (Mutex)
//!                                                              │  ├─ events ──▶ channel ──▶ UI
//!  [waiter thread] ── child.wait() ──────────exit──────────────┘  └─ wake()  (coalesced)
//!  UI input ──▶ unbounded queue ──▶ [writer thread] ──write──▶ PTY master
//!                     ▲ emulator answers to queries (DA, CPR, colors) also go here
//! ```
//!
//! * The reader only reads (64 KiB at a time) and hands chunks to the parser
//!   through a bounded channel: under a flood the child blocks on its own
//!   output instead of Lector buffering it, so memory stays bounded by the
//!   channel (2 MiB) plus the scrollback limit.
//! * The parser thread owns the `Emulator` lock only while feeding one chunk;
//!   a UI snapshot waits at most for that chunk. It also wakes up when a
//!   synchronized update (`?2026`) expires, so a program that forgets to end
//!   one cannot freeze the screen.
//! * `Wakeup` is coalesced: after one is sent no other is sent until the owner
//!   calls [`Terminal::snapshot`]. The wake callback runs on the parser
//!   thread; it must only schedule work (e.g. notify a GPUI entity).
//! * Input never blocks the caller: bytes go to an unbounded queue drained by
//!   the writer thread, so pasting megabytes into a busy program is fine.
//!
//! portable-pty is used for the PTY on every platform (openpty, ConPTY);
//! alacritty's own `tty`/`event_loop` modules are not used because they own
//! the PTY and the read loop themselves and would duplicate portable-pty.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use parking_lot::{Mutex, MutexGuard};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::emulator::{Emulator, EmulatorConfig};
use crate::error::TerminalError;
use crate::geometry::{ScrollAmount, TermSize};
use crate::input::{Key, Modifiers, MouseEvent};
use crate::links::{resolve_link, LinkTarget};
use crate::shell::{
    env_is_scrubbed, install_shell_integration, plan_injection, terminal_env, SpawnOptions,
};
use crate::snapshot::Snapshot;
use crate::TerminalEvent;

/// Callback invoked (on a background thread) when the owner should look at the terminal.
pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

/// How the child process ended.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExitInfo {
    /// Exit code (on Unix, 1 when killed by a signal).
    pub code: u32,
    /// Signal name when killed by a signal (Unix).
    pub signal: Option<String>,
    /// Whether the process exited successfully.
    pub success: bool,
}

const READ_CHUNK: usize = 64 * 1024;
const CHANNEL_CHUNKS: usize = 32;
const EVENT_CAPACITY: usize = 4096;
/// How long the parser waits for the PTY to drain after the child exits.
const EXIT_DRAIN: Duration = Duration::from_millis(250);
/// Most bytes parsed per lock acquisition before giving the UI a chance.
const PARSE_BUDGET: usize = 512 * 1024;

enum Msg {
    Data(Vec<u8>),
    Eof,
    Exit(ExitInfo),
}

struct Shared {
    emulator: Mutex<Emulator>,
    wake_pending: AtomicBool,
    wake: Option<WakeFn>,
    events_tx: Sender<TerminalEvent>,
    exit: Mutex<Option<ExitInfo>>,
    exited: AtomicBool,
}

impl Shared {
    fn notify(&self) {
        if let Some(w) = &self.wake {
            w();
        }
    }

    fn send(&self, event: TerminalEvent) {
        // Lossy when the owner stops draining: never block the parser.
        let _ = self.events_tx.try_send(event);
    }

    fn content_changed(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            self.send(TerminalEvent::Wakeup);
            self.notify();
        }
    }
}

/// Locked access to a session's [`Emulator`]. Hold it briefly: the parser
/// thread waits while it is held.
pub struct EmulatorGuard<'a>(MutexGuard<'a, Emulator>);

impl std::ops::Deref for EmulatorGuard<'_> {
    type Target = Emulator;
    fn deref(&self) -> &Emulator {
        &self.0
    }
}

impl std::ops::DerefMut for EmulatorGuard<'_> {
    fn deref_mut(&mut self) -> &mut Emulator {
        &mut self.0
    }
}

/// A running terminal session. Dropping it kills the child's process tree.
pub struct Terminal {
    shared: Arc<Shared>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer_tx: Sender<Vec<u8>>,
    events_rx: Receiver<TerminalEvent>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    scroll_to_bottom_on_input: bool,
    initial_cwd: PathBuf,
    name: String,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("name", &self.name)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

fn pty_size(size: TermSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.cols.saturating_mul(size.cell_width),
        pixel_height: size.rows.saturating_mul(size.cell_height),
    }
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), TerminalError> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .map(|_| ())
        .map_err(TerminalError::Thread)
}

impl Terminal {
    /// Spawns `options.profile` in a new PTY.
    ///
    /// `wake` is called from a background thread whenever there is something
    /// new (content, title, exit...); it should schedule a redraw, not do work.
    pub fn spawn(options: SpawnOptions, wake: Option<WakeFn>) -> Result<Terminal, TerminalError> {
        let size = options.size.clamped();
        let cwd = match options.cwd.clone() {
            Some(c) => c,
            None => std::env::current_dir()
                .map_err(|_| TerminalError::InvalidCwd(PathBuf::from(".")))?,
        };
        if !cwd.is_dir() {
            return Err(TerminalError::InvalidCwd(cwd));
        }
        let pair = native_pty_system()
            .openpty(pty_size(size))
            .map_err(|e| TerminalError::OpenPty(e.to_string()))?;

        let profile = &options.profile;
        let mut cmd = CommandBuilder::new(&profile.program);
        cmd.args(&profile.args);
        cmd.cwd(&cwd);
        for (key, _) in std::env::vars_os() {
            if let Some(k) = key.to_str() {
                if env_is_scrubbed(k, &options.env_scrub) {
                    cmd.env_remove(&key);
                }
            }
        }
        for (k, v) in terminal_env() {
            cmd.env(k, v);
        }
        if let Some(dir) = &options.shell_integration {
            match install_shell_integration(dir) {
                Ok(()) => {
                    if let Some(injection) = plan_injection(profile, dir) {
                        cmd.args(&injection.args);
                        for (k, v) in injection.env {
                            cmd.env(k, v);
                        }
                    }
                }
                Err(e) => tracing::warn!("shell integration disabled: {e}"),
            }
        }
        for (k, v) in profile.env.iter().chain(options.env.iter()) {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TerminalError::Spawn {
                program: profile.program.clone(),
                message: e.to_string(),
            })?;
        // The parent must not keep the slave open, or the reader never sees EOF.
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TerminalError::OpenPty(e.to_string()))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::OpenPty(e.to_string()))?;

        let emulator = Emulator::new(EmulatorConfig {
            size,
            scrollback: options.scrollback,
            initial_cwd: Some(cwd.clone()),
            ..EmulatorConfig::default()
        });
        let (events_tx, events_rx) = bounded(EVENT_CAPACITY);
        let shared = Arc::new(Shared {
            emulator: Mutex::new(emulator),
            wake_pending: AtomicBool::new(false),
            wake,
            events_tx,
            exit: Mutex::new(None),
            exited: AtomicBool::new(false),
        });

        let (data_tx, data_rx) = bounded::<Msg>(CHANNEL_CHUNKS);
        let (writer_tx, writer_rx) = unbounded::<Vec<u8>>();

        // Writer.
        spawn_thread("lector-term-writer", move || {
            while let Ok(bytes) = writer_rx.recv() {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        })?;

        // Reader.
        let reader_tx = data_tx.clone();
        spawn_thread("lector-term-reader", move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if reader_tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = reader_tx.send(Msg::Eof);
        })?;

        // Waiter.
        let waiter_tx = data_tx;
        spawn_thread("lector-term-waiter", move || {
            let info = match child.wait() {
                Ok(status) => ExitInfo {
                    code: status.exit_code(),
                    signal: status.signal().map(str::to_owned),
                    success: status.success(),
                },
                Err(_) => ExitInfo {
                    code: 1,
                    signal: None,
                    success: false,
                },
            };
            let _ = waiter_tx.send(Msg::Exit(info));
        })?;

        // Parser.
        let parser_shared = shared.clone();
        let parser_writer = writer_tx.clone();
        spawn_thread("lector-term-parser", move || {
            parser_loop(&parser_shared, &data_rx, &parser_writer)
        })?;

        Ok(Terminal {
            shared,
            master: Mutex::new(pair.master),
            writer_tx,
            events_rx,
            killer: Mutex::new(killer),
            pid,
            scroll_to_bottom_on_input: options.scroll_to_bottom_on_input,
            initial_cwd: cwd,
            name: options.profile.name.clone(),
        })
    }

    /// Profile name the session was started with.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Process id of the child (the shell).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Next pending event, without blocking.
    pub fn try_event(&self) -> Option<TerminalEvent> {
        self.events_rx.try_recv().ok()
    }

    /// All pending events.
    pub fn drain_events(&self) -> Vec<TerminalEvent> {
        self.events_rx.try_iter().collect()
    }

    /// Waits up to `timeout` for the next event.
    pub fn recv_event_timeout(&self, timeout: Duration) -> Option<TerminalEvent> {
        self.events_rx.recv_timeout(timeout).ok()
    }

    /// Locks the emulator (selection, search, links, command marks, ...).
    pub fn lock(&self) -> EmulatorGuard<'_> {
        EmulatorGuard(self.shared.emulator.lock())
    }

    /// Takes the frame snapshot (see [`Emulator::snapshot`]) and re-arms `Wakeup`.
    pub fn snapshot(&self) -> Snapshot {
        self.shared.wake_pending.store(false, Ordering::Release);
        self.shared.emulator.lock().snapshot()
    }

    fn before_input(&self) {
        if self.scroll_to_bottom_on_input {
            let mut em = self.shared.emulator.lock();
            if em.display_offset() != 0 {
                em.scroll(ScrollAmount::Bottom);
                drop(em);
                self.shared.content_changed();
            }
        }
    }

    /// Sends raw bytes to the program (never blocks).
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        self.before_input();
        let _ = self.writer_tx.send(bytes);
    }

    /// Encodes and sends a key press; returns false when the key produces no input.
    pub fn send_key(&self, key: Key, mods: Modifiers) -> bool {
        let bytes = self.shared.emulator.lock().encode_key(key, mods);
        match bytes {
            Some(b) => {
                self.write(b);
                true
            }
            None => false,
        }
    }

    /// Sends typed text (IME commits, dead keys) as-is.
    pub fn send_text(&self, text: &str) {
        self.write(text.as_bytes().to_vec());
    }

    /// Pastes text (bracketed and sanitised when the program enabled bracketed paste).
    pub fn paste(&self, text: &str) {
        let bytes = self.shared.emulator.lock().encode_paste(text);
        self.write(bytes);
    }

    /// Reports a mouse event to the program if its mouse mode wants it.
    /// Returns false when the UI should handle the event itself (selection, scrolling).
    pub fn send_mouse(&self, event: MouseEvent) -> bool {
        let bytes = self.shared.emulator.lock().encode_mouse(event);
        match bytes {
            Some(b) => {
                let _ = self.writer_tx.send(b);
                true
            }
            None => false,
        }
    }

    /// Tells the program about focus changes (when it asked for them).
    pub fn set_focused(&self, focused: bool) {
        let bytes = self.shared.emulator.lock().encode_focus(focused);
        if let Some(b) = bytes {
            let _ = self.writer_tx.send(b);
        }
    }

    /// Scrolls the view over scrollback.
    pub fn scroll(&self, amount: ScrollAmount) {
        self.shared.emulator.lock().scroll(amount);
        self.shared.content_changed();
    }

    /// Resizes the grid and the PTY (the child gets `SIGWINCH`).
    pub fn resize(&self, size: TermSize) -> Result<(), TerminalError> {
        let size = size.clamped();
        self.shared.emulator.lock().resize(size);
        let result = self
            .master
            .lock()
            .resize(pty_size(size))
            .map_err(|e| TerminalError::Resize(e.to_string()));
        self.shared.content_changed();
        result
    }

    /// Current working directory: last OSC 7 report, else the starting directory.
    pub fn cwd(&self) -> PathBuf {
        self.shared
            .emulator
            .lock()
            .cwd()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.initial_cwd.clone())
    }

    /// Resolves a link target against the session's cwd (see [`resolve_link`]).
    pub fn resolve_link(&self, target: &LinkTarget, cancel: &AtomicBool) -> Option<LinkTarget> {
        let cwd = self.cwd();
        resolve_link(target, &cwd, cancel)
    }

    /// Exit status once the child has exited.
    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.shared.exit.lock().clone()
    }

    /// Whether the child is still running.
    pub fn is_running(&self) -> bool {
        !self.shared.exited.load(Ordering::Acquire)
    }

    /// Kills the child's process tree now (the session stays readable).
    pub fn kill(&self) {
        if self.is_running() {
            self.kill_tree(Duration::from_millis(300));
        }
    }

    fn kill_tree(&self, grace: Duration) {
        #[cfg(unix)]
        {
            use rustix::process::Signal;
            let Some(shell) = self.pid.and_then(|p| i32::try_from(p).ok()) else {
                let _ = self.killer.lock().kill();
                return;
            };
            let foreground = self.master.lock().process_group_leader();
            // Collect while the shell is alive: its children are re-parented once it exits.
            let victims = crate::process_tree::Victims::collect(shell, foreground);
            victims.signal(Signal::HUP);
            victims.signal(Signal::CONT);
            let shared = self.shared.clone();
            let mut killer = self.killer.lock().clone_killer();
            let _ = spawn_thread("lector-term-reaper", move || {
                let deadline = Instant::now() + grace;
                while Instant::now() < deadline {
                    if !victims.any_alive() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                victims.kill_survivors();
                if !shared.exited.load(Ordering::Acquire) {
                    let _ = killer.kill();
                }
            });
        }
        #[cfg(windows)]
        {
            let _ = grace;
            let pid = self.pid;
            let mut killer = self.killer.lock().clone_killer();
            let _ = spawn_thread("lector-term-reaper", move || {
                if let Some(pid) = pid {
                    use std::os::windows::process::CommandExt;
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    let _ = std::process::Command::new("taskkill")
                        .args(["/T", "/F", "/PID", &pid.to_string()])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .creation_flags(CREATE_NO_WINDOW)
                        .status();
                }
                let _ = killer.kill();
            });
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = grace;
            let _ = self.killer.lock().kill();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.is_running() {
            self.kill_tree(Duration::from_millis(500));
        }
    }
}

fn parser_loop(shared: &Shared, rx: &Receiver<Msg>, writer: &Sender<Vec<u8>>) {
    let mut eof = false;
    let mut pending_exit: Option<(ExitInfo, Instant)> = None;
    let mut exit_sent = false;
    loop {
        let sync_deadline = shared.emulator.lock().sync_deadline();
        let exit_deadline = pending_exit.as_ref().map(|(_, d)| *d);
        let deadline = match (sync_deadline, exit_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let msg = match deadline {
            Some(d) => match rx.recv_deadline(d) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        let mut changed = false;
        match msg {
            None => {
                let mut em = shared.emulator.lock();
                if em.sync_deadline().is_some_and(|d| d <= Instant::now()) {
                    em.flush_expired_sync();
                    changed = true;
                }
            }
            Some(Msg::Data(first)) => {
                // Parse everything that is already queued, in bounded batches.
                let mut next = Some(first);
                while let Some(data) = next.take() {
                    let mut em = shared.emulator.lock();
                    let mut budget = 0usize;
                    let mut data = data;
                    loop {
                        em.feed(&data);
                        budget += data.len();
                        if budget >= PARSE_BUDGET {
                            break;
                        }
                        match rx.try_recv() {
                            Ok(Msg::Data(d)) => data = d,
                            Ok(Msg::Eof) => {
                                eof = true;
                                break;
                            }
                            Ok(Msg::Exit(info)) => {
                                pending_exit = Some((info, Instant::now() + EXIT_DRAIN));
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    forward(shared, &mut em, writer);
                    drop(em);
                    changed = true;
                    if budget >= PARSE_BUDGET {
                        if let Ok(m) = rx.try_recv() {
                            match m {
                                Msg::Data(d) => next = Some(d),
                                Msg::Eof => eof = true,
                                Msg::Exit(info) => {
                                    pending_exit = Some((info, Instant::now() + EXIT_DRAIN))
                                }
                            }
                        }
                        // Let a waiting snapshot in and tell the UI there is something to draw.
                        shared.content_changed();
                    }
                }
            }
            Some(Msg::Eof) => eof = true,
            Some(Msg::Exit(info)) => pending_exit = Some((info, Instant::now() + EXIT_DRAIN)),
        }
        if changed {
            shared.content_changed();
        }
        if let Some((info, deadline)) = pending_exit.clone() {
            if !exit_sent && (eof || Instant::now() >= deadline) {
                *shared.exit.lock() = Some(info.clone());
                shared.exited.store(true, Ordering::Release);
                shared.send(TerminalEvent::Exited(info));
                shared.notify();
                exit_sent = true;
                pending_exit = None;
            }
        }
        if eof && exit_sent {
            break;
        }
    }
}

fn forward(shared: &Shared, em: &mut Emulator, writer: &Sender<Vec<u8>>) {
    let responses = em.take_responses();
    if !responses.is_empty() {
        let _ = writer.send(responses);
    }
    let events = em.take_events();
    if !events.is_empty() {
        for e in events {
            shared.send(e);
        }
        shared.notify();
    }
}
