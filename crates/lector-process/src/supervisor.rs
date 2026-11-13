//! The registry of live child processes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::child::{self, ChildHandle, ProcessInfo, Shared};
use crate::command::ProcessCommand;
use crate::error::{ProcessError, Result};

pub(crate) struct SupInner {
    next_id: AtomicU64,
    live: Mutex<BTreeMap<u64, Weak<Shared>>>,
    shutting_down: AtomicBool,
}

impl SupInner {
    pub(crate) fn forget(&self, id: u64) {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    fn alive(&self) -> Vec<Arc<Shared>> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|_, w| w.strong_count() > 0);
        live.values().filter_map(Weak::upgrade).collect()
    }
}

/// Tracks the child processes Lector runs through it (git, language servers, agents,
/// helpers): spawns them in their own process group / Job Object, lists them, and kills
/// every tree on [`shutdown`](Self::shutdown). Integrated terminals are not among them:
/// they run in a PTY from `portable-pty` (`lector-terminal`), which kills their trees itself.
///
/// Cheap to clone (shared). Use [`Supervisor::global`] in the app; tests can make their own.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<SupInner>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("live", &self.inner.alive().len())
            .finish()
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// A new, empty supervisor.
    pub fn new() -> Self {
        Supervisor {
            inner: Arc::new(SupInner {
                next_id: AtomicU64::new(1),
                live: Mutex::new(BTreeMap::new()),
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    /// The process-wide supervisor. The app calls [`shutdown`](Self::shutdown) on it
    /// when it quits.
    pub fn global() -> &'static Supervisor {
        static GLOBAL: OnceLock<Supervisor> = OnceLock::new();
        GLOBAL.get_or_init(Supervisor::new)
    }

    /// Starts `cmd` and returns its handle (long-lived processes: language servers,
    /// agents). Use [`run`](Self::run) / [`run_streaming`](Self::run_streaming) for
    /// commands that run to completion.
    pub fn spawn(&self, cmd: &ProcessCommand) -> Result<ChildHandle> {
        if self.is_shutting_down() {
            return Err(ProcessError::ShuttingDown);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (handle, shared) = child::spawn(cmd, id, &self.inner)?;
        {
            let mut live = self.inner.live.lock().unwrap_or_else(|e| e.into_inner());
            live.insert(id, Arc::downgrade(&shared));
            // `shutdown` sets the flag before it takes this lock to list the live trees:
            // either it lists this child, or the flag is visible here.
            if !self.inner.shutting_down.load(Ordering::SeqCst) {
                return Ok(handle);
            }
            live.remove(&id);
        }
        // Shutdown started while this child was being spawned: dropping the handle kills
        // its tree (outside the lock; the drop also unregisters, harmlessly, again).
        drop(handle);
        Err(ProcessError::ShuttingDown)
    }

    /// Every process whose handle is alive, by id.
    pub fn list(&self) -> Vec<ProcessInfo> {
        self.inner.alive().iter().map(|s| s.info()).collect()
    }

    /// Hard-kills the tree of process `id`; `false` if it is not tracked.
    pub fn kill(&self, id: u64) -> bool {
        match self.inner.alive().into_iter().find(|s| s.id == id) {
            Some(s) => {
                s.kill_tree();
                true
            }
            None => false,
        }
    }

    /// Stops every tracked tree (graceful first, all in parallel; hard kill after `grace`)
    /// and refuses new spawns from now on. Blocking; call when the app quits.
    pub fn shutdown(&self, grace: Duration) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let alive = self.inner.alive();
        let threads: Vec<_> = alive
            .into_iter()
            .map(|s| std::thread::spawn(move || s.terminate(grace)))
            .collect();
        for t in threads {
            let _ = t.join();
        }
    }

    /// True after [`shutdown`](Self::shutdown).
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }
}
