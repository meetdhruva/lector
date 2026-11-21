//! Named, registered owner threads (ADR 0003, decision 3).
//!
//! A thread that *is* a long-lived resource (a PTY reader, a language-server pipe, a file
//! watcher, a scheduler worker...) is created with [`spawn_named`]. It gets an OS-visible name
//! and an entry in a process-wide registry that [`threads`] reads for diagnostics. The list of
//! [`ThreadKind`]s is closed and mirrored in `arch/threads.allow` (a unit test keeps them equal);
//! adding a kind needs an ADR.
//!
//! This module is the only place that calls `std::thread::Builder::spawn`; the scheduler's own
//! workers are owner threads made here too. Short work does not get a thread: it runs on the
//! `lector-app` scheduler, and crates below it hand short tasks to their caller through a
//! [`TaskRunner`]. There is no join-all here: whoever owns the resource stops and joins its
//! threads; `lector_app::shutdown` reports the ones still alive at quit.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{Builder, JoinHandle};
use std::time::SystemTime;

/// A short task handed to a [`TaskRunner`].
pub type RunnerTask = Box<dyn FnOnce() + Send + 'static>;

/// Where a short blocking task of a crate below `lector-app` runs. Those crates do not
/// schedule (ADR 0003, decision 4): an owner thread that has short work to hand off (an askpass
/// connection, a language-server launch, a terminal's kill escalation, a request from an agent)
/// calls the runner its caller supplied, e.g. one that submits a job to the `lector-app`
/// scheduler.
///
/// **Contract: a task is never silently dropped.** A runner either takes the task (`Ok`: it
/// will run it, or, if the work is cancelled later, drop it, which for cleanup work still runs
/// it) or hands it back (`Err(task)`: it cannot take it, e.g. the scheduler has stopped), and the
/// caller then runs it itself, on its own thread. Callers write
/// `if let Err(task) = runner(task) { task() }`.
pub type TaskRunner = std::sync::Arc<dyn Fn(RunnerTask) -> Result<(), RunnerTask> + Send + Sync>;

/// The closed set of owner-thread kinds. [`as_str`](Self::as_str) spells each one exactly as
/// `arch/threads.allow` does (`kind:<name>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ThreadKind {
    /// Terminal: reads the PTY master.
    PtyReader,
    /// Terminal: writes input to the PTY master.
    PtyWriter,
    /// Terminal: waits for the shell process to exit.
    PtyWaiter,
    /// Terminal: parses PTY output into the screen grid.
    PtyParser,
    /// Language server: reads and dispatches its stdout.
    LspReader,
    /// Language server: writes requests to its stdin.
    LspWriter,
    /// Language server: request-timeout watchdog.
    LspWatchdog,
    /// Askpass: the accept loop of the credential socket.
    AskpassListener,
    /// File watcher: its debounce loop (registration runs as a scheduler job).
    FileWatcher,
    /// Fallback polling when no watcher could be registered.
    WatchPoll,
    /// A worker of the `lector-app` scheduler.
    SchedulerWorker,
    /// The scheduler's debounce timer.
    SchedulerTimer,
    /// Pumps a child's stdout or stderr (process supervisor, git, agent and server stderr).
    PipeReader,
    /// Pumps a child's stdin for as long as the input lasts, so large input cannot deadlock
    /// against full output pipes.
    PipeWriter,
    /// Agent client: reads the ACP connection.
    AcpReader,
    /// The file picker's matching thread.
    PickerWorker,
    /// The mock forge server used by tests (accept loop and connections).
    MockServer,
}

impl ThreadKind {
    /// Every kind, in declaration order.
    pub const ALL: [ThreadKind; 17] = [
        ThreadKind::PtyReader,
        ThreadKind::PtyWriter,
        ThreadKind::PtyWaiter,
        ThreadKind::PtyParser,
        ThreadKind::LspReader,
        ThreadKind::LspWriter,
        ThreadKind::LspWatchdog,
        ThreadKind::AskpassListener,
        ThreadKind::FileWatcher,
        ThreadKind::WatchPoll,
        ThreadKind::SchedulerWorker,
        ThreadKind::SchedulerTimer,
        ThreadKind::PipeReader,
        ThreadKind::PipeWriter,
        ThreadKind::AcpReader,
        ThreadKind::PickerWorker,
        ThreadKind::MockServer,
    ];

    /// The kind's name as written in `arch/threads.allow`, e.g. `"pty-reader"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ThreadKind::PtyReader => "pty-reader",
            ThreadKind::PtyWriter => "pty-writer",
            ThreadKind::PtyWaiter => "pty-waiter",
            ThreadKind::PtyParser => "pty-parser",
            ThreadKind::LspReader => "lsp-reader",
            ThreadKind::LspWriter => "lsp-writer",
            ThreadKind::LspWatchdog => "lsp-watchdog",
            ThreadKind::AskpassListener => "askpass-listener",
            ThreadKind::FileWatcher => "file-watcher",
            ThreadKind::WatchPoll => "watch-poll",
            ThreadKind::SchedulerWorker => "scheduler-worker",
            ThreadKind::SchedulerTimer => "scheduler-timer",
            ThreadKind::PipeReader => "pipe-reader",
            ThreadKind::PipeWriter => "pipe-writer",
            ThreadKind::AcpReader => "acp-reader",
            ThreadKind::PickerWorker => "picker-worker",
            ThreadKind::MockServer => "mock-server",
        }
    }
}

impl fmt::Display for ThreadKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A snapshot of one registered thread, see [`threads`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInfo {
    /// Process-unique, increasing registration number.
    pub id: u64,
    /// What the thread is.
    pub kind: ThreadKind,
    /// The owner's name for it (a language, a session id, a path...). Not truncated.
    pub name: String,
    /// When [`spawn_named`] registered it.
    pub started_at: SystemTime,
    /// False once the closure has returned or panicked.
    pub alive: bool,
}

/// Registered threads that have finished are kept for diagnostics, newest last, up to this many.
const KEEP_FINISHED: usize = 64;

/// Longest OS thread name handed to `std` (macOS accepts 63 bytes, Linux 15; `std` shortens
/// further where the platform needs it, and [`ThreadInfo::name`] always keeps the full name).
const OS_NAME_MAX: usize = 63;

#[derive(Default)]
struct Registry {
    entries: VecDeque<ThreadInfo>,
}

impl Registry {
    fn add(&mut self, info: ThreadInfo) {
        self.entries.push_back(info);
        self.prune();
    }

    /// Forgets the oldest finished entries beyond [`KEEP_FINISHED`].
    fn prune(&mut self) {
        let finished = self.entries.iter().filter(|e| !e.alive).count();
        if finished > KEEP_FINISHED {
            let mut drop_n = finished - KEEP_FINISHED;
            self.entries.retain(|e| {
                if e.alive || drop_n == 0 {
                    true
                } else {
                    drop_n -= 1;
                    false
                }
            });
        }
    }
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    entries: VecDeque::new(),
});
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn registry() -> MutexGuard<'static, Registry> {
    // The registry holds plain data and the guards below never panic, but a poisoned lock must
    // not stop diagnostics or thread starts anyway.
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Clears the registry entry's `alive` flag when dropped: on return and while unwinding.
struct AliveGuard(u64);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        let mut reg = registry();
        if let Some(entry) = reg.entries.iter_mut().find(|e| e.id == self.0) {
            entry.alive = false;
        }
        reg.prune();
    }
}

/// `<kind>:<name>`, without NUL or control characters (`std` panics on NUL), cut to
/// [`OS_NAME_MAX`] bytes on a char boundary.
fn os_name(kind: ThreadKind, name: &str) -> String {
    let mut out = String::with_capacity(OS_NAME_MAX);
    out.push_str(kind.as_str());
    out.push(':');
    for c in name.chars() {
        let c = if c.is_control() { '_' } else { c };
        if out.len() + c.len_utf8() > OS_NAME_MAX {
            break;
        }
        out.push(c);
    }
    out
}

/// Starts a thread of `kind` running `f`.
///
/// The OS thread is named `<kind>:<name>` (control characters replaced, cut to 63 bytes on a
/// char boundary; the platform may cut further, the kind comes first for that reason) and the
/// thread is in [`threads`] from before this function returns until `f` returns or panics. A
/// panic in `f` is reported by `JoinHandle::join` as usual and leaves the registry intact.
///
/// # Errors
/// The OS refused to start a thread; nothing stays registered then.
pub fn spawn_named<T, F>(
    kind: ThreadKind,
    name: impl Into<String>,
    f: F,
) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let name = name.into();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let os = os_name(kind, &name);
    registry().add(ThreadInfo {
        id,
        kind,
        name,
        started_at: SystemTime::now(),
        alive: true,
    });
    let guard = AliveGuard(id);
    let spawned = Builder::new().name(os).spawn(move || {
        let _guard = guard;
        f()
    });
    if spawned.is_err() {
        registry().entries.retain(|e| e.id != id);
    }
    spawned
}

/// Every thread registered by [`spawn_named`] that is alive, plus the most recently finished ones
/// (up to 64), oldest first.
pub fn threads() -> Vec<ThreadInfo> {
    registry().entries.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn info(id_of: &str, kind: ThreadKind) -> Option<ThreadInfo> {
        threads()
            .into_iter()
            .find(|t| t.kind == kind && t.name == id_of)
    }

    /// The registry is process-wide and forgets old finished entries: tests that start threads
    /// and read it back run one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_until(mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn registered_while_running_and_cleared_on_return() {
        let _serial = serial();
        let (release, wait) = mpsc::channel::<()>();
        let (ready_tx, ready) = mpsc::channel::<()>();
        let h = spawn_named(ThreadKind::PtyReader, "t-return", move || {
            ready_tx.send(()).unwrap();
            wait.recv().unwrap();
            7
        })
        .unwrap();
        ready.recv().unwrap();
        let i = info("t-return", ThreadKind::PtyReader).expect("registered");
        assert!(i.alive);
        assert!(i.started_at <= SystemTime::now());
        release.send(()).unwrap();
        assert_eq!(h.join().unwrap(), 7);
        wait_until(|| info("t-return", ThreadKind::PtyReader).is_some_and(|i| !i.alive));
    }

    #[test]
    fn cleared_when_the_closure_panics_and_the_registry_keeps_working() {
        let _serial = serial();
        let h = spawn_named(ThreadKind::LspReader, "t-panic", || {
            if true {
                panic!("expected test panic");
            }
        })
        .unwrap();
        assert!(h.join().is_err());
        wait_until(|| info("t-panic", ThreadKind::LspReader).is_some_and(|i| !i.alive));
        // Not poisoned: another thread can still be started and listed.
        let h = spawn_named(ThreadKind::LspReader, "t-after-panic", || ()).unwrap();
        h.join().unwrap();
        assert!(info("t-after-panic", ThreadKind::LspReader).is_some());
    }

    #[test]
    fn the_os_thread_carries_kind_and_name() {
        let _serial = serial();
        let h = spawn_named(ThreadKind::FileWatcher, "root", || {
            std::thread::current().name().map(str::to_owned)
        })
        .unwrap();
        assert_eq!(h.join().unwrap().as_deref(), Some("file-watcher:root"));
    }

    #[test]
    fn os_names_are_sanitised_and_cut_on_char_boundaries() {
        assert_eq!(
            os_name(ThreadKind::PtyWriter, "a\0b\nc"),
            "pty-writer:a_b_c"
        );
        let long = "é".repeat(100);
        let n = os_name(ThreadKind::AskpassListener, &long);
        assert!(n.len() <= OS_NAME_MAX);
        assert!(n.starts_with("askpass-listener:é"));
        assert!(std::str::from_utf8(n.as_bytes()).is_ok());
        // A NUL-bearing or huge name must not make spawning panic.
        let h = spawn_named(ThreadKind::PtyWriter, format!("x\0{long}"), || ()).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn full_name_is_kept_in_the_registry() {
        let _serial = serial();
        let long = "n".repeat(200);
        spawn_named(ThreadKind::SchedulerWorker, long.clone(), || ())
            .unwrap()
            .join()
            .unwrap();
        assert!(info(&long, ThreadKind::SchedulerWorker).is_some());
    }

    #[test]
    fn ids_increase_and_finished_entries_are_bounded() {
        let _serial = serial();
        let mut last = 0;
        for i in 0..(KEEP_FINISHED + 40) {
            spawn_named(ThreadKind::PipeWriter, format!("bulk-{i}"), || ())
                .unwrap()
                .join()
                .unwrap();
        }
        wait_until(|| {
            threads()
                .iter()
                .filter(|t| t.kind == ThreadKind::PipeWriter && t.name.starts_with("bulk-"))
                .all(|t| !t.alive)
        });
        let all = threads();
        for t in &all {
            assert!(t.id > last);
            last = t.id;
        }
        assert!(all.iter().filter(|t| !t.alive).count() <= KEEP_FINISHED);
    }

    #[test]
    fn kind_names_are_unique_kebab_case_and_listed_in_arch_threads_allow() {
        let allow = include_str!("../../../arch/threads.allow");
        let listed: Vec<String> = allow
            .lines()
            .filter_map(|l| l.trim().strip_prefix("kind:"))
            .map(|rest| {
                rest.split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
            .collect();
        let mut names: Vec<&str> = ThreadKind::ALL.iter().map(|k| k.as_str()).collect();
        for name in &names {
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{name}"
            );
            assert!(
                listed.iter().any(|l| l == name),
                "{name} not in threads.allow"
            );
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ThreadKind::ALL.len());
        assert_eq!(
            listed.len(),
            ThreadKind::ALL.len(),
            "extra kinds in threads.allow"
        );
        assert_eq!(ThreadKind::PtyReader.to_string(), "pty-reader");
    }
}
