//! Process-wide git state: one [`RepoState`] per repository, shared by every
//! window (and the CLI, MCP, agents).
//!
//! A `RepoState` owns one `lector-fs` watcher on the work tree. Watcher events
//! (`GitStateChanged`, file events, `Rescan`) and explicit [`RepoState::refresh`]
//! calls are coalesced through the [`Scheduler`] (keyed debounce), so any number
//! of windows on one repository trigger **one** HEAD + status + conflict
//! computation. If the watcher cannot be started, a slow poll thread refreshes
//! instead. Results are published as immutable [`GitSnapshot`]s tagged with a
//! generation; subscribers are called (on a worker thread) only when something
//! changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use lector_git::{HeadInfo, Repo, StatusEntry};
use lector_workspace::conflicts::ConflictState;
use parking_lot::{Mutex, RwLock};

use crate::scheduler::{JobKey, Priority, Scheduler};

/// Delay that coalesces refresh requests.
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(40);
/// Poll interval when the watcher could not be started.
pub const FALLBACK_POLL: Duration = Duration::from_secs(5);

/// Immutable git state of a repository at one point in time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitSnapshot {
    /// `HEAD` (None until loaded or when unreadable).
    pub head: Option<HeadInfo>,
    /// `git status` entries.
    pub status: Arc<Vec<StatusEntry>>,
    /// Operation in progress and conflicted paths.
    pub conflicts: Arc<ConflictState>,
    /// Bumped on each observed change; 0 = not loaded yet.
    pub generation: u64,
    /// Last refresh error (status unreadable, ...).
    pub error: Option<String>,
}

impl GitSnapshot {
    /// Short branch name, or the detached short sha.
    pub fn branch_label(&self) -> Option<String> {
        let head = self.head.as_ref()?;
        match (&head.branch, head.oid) {
            (Some(b), _) => Some(b.clone()),
            (None, Some(oid)) => Some(oid.short()),
            _ => None,
        }
    }
}

type Callback = Arc<dyn Fn(&Arc<GitSnapshot>) + Send + Sync>;

struct Subscribers {
    next: u64,
    list: Vec<(u64, Callback)>,
}

/// Shared git state of one repository.
pub struct RepoState {
    repo: Repo,
    scheduler: Scheduler,
    snapshot: RwLock<Arc<GitSnapshot>>,
    subscribers: Mutex<Subscribers>,
    watcher: Mutex<Option<lector_fs::Watcher>>,
    watching: AtomicBool,
    computations: AtomicU64,
    force_notify: AtomicBool,
    me: Weak<RepoState>,
}

impl std::fmt::Debug for RepoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoState")
            .field("workdir", &self.repo.workdir())
            .field("generation", &self.snapshot.read().generation)
            .finish()
    }
}

/// Unsubscribes when dropped.
#[must_use = "dropping a Subscription unsubscribes"]
pub struct Subscription {
    state: Weak<RepoState>,
    id: u64,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .subscribers
                .lock()
                .list
                .retain(|(id, _)| *id != self.id);
        }
    }
}

impl RepoState {
    fn new(repo: Repo, scheduler: Scheduler, watch: bool) -> Arc<RepoState> {
        let state = Arc::new_cyclic(|me| RepoState {
            repo,
            scheduler,
            snapshot: RwLock::new(Arc::new(GitSnapshot::default())),
            subscribers: Mutex::new(Subscribers {
                next: 1,
                list: Vec::new(),
            }),
            watcher: Mutex::new(None),
            watching: AtomicBool::new(false),
            computations: AtomicU64::new(0),
            force_notify: AtomicBool::new(false),
            me: me.clone(),
        });
        if watch {
            // Registering recursive watches walks the tree: never on the caller's
            // (possibly UI) thread.
            let weak = Arc::downgrade(&state);
            let spawned = std::thread::Builder::new()
                .name("lector-git-watch".into())
                .spawn(move || {
                    if let Some(state) = weak.upgrade() {
                        state.start_watching();
                    }
                });
            if let Err(err) = spawned {
                tracing::error!("cannot start git watcher thread: {err}");
            }
        }
        state.refresh();
        state
    }

    fn start_watching(&self) {
        if self.repo.is_bare() {
            return;
        }
        let weak = self.me.clone();
        let result = lector_fs::watch(self.repo.workdir(), Duration::from_millis(100), move |ev| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            match ev {
                lector_fs::WatchEvent::Files(_)
                | lector_fs::WatchEvent::GitStateChanged
                | lector_fs::WatchEvent::Rescan => state.refresh(),
            }
        });
        match result {
            Ok(w) => {
                *self.watcher.lock() = Some(w);
                self.watching.store(true, Ordering::Release);
            }
            Err(err) => {
                tracing::warn!(
                    "cannot watch {} ({err}); polling git state every {:?}",
                    self.repo.workdir().display(),
                    FALLBACK_POLL
                );
                let weak = self.me.clone();
                let spawned = std::thread::Builder::new()
                    .name("lector-git-poll".into())
                    .spawn(move || loop {
                        std::thread::sleep(FALLBACK_POLL);
                        match weak.upgrade() {
                            Some(state) => state.refresh(),
                            None => return,
                        }
                    });
                if let Err(err) = spawned {
                    tracing::error!("cannot start git poll thread: {err}");
                }
            }
        }
    }

    /// The repository.
    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Repository work directory (the key in [`GitService`]).
    pub fn workdir(&self) -> &Path {
        self.repo.workdir()
    }

    /// Whether the file-system watcher runs (false: slow poll fallback / bare repo).
    pub fn is_watching(&self) -> bool {
        self.watching.load(Ordering::Acquire)
    }

    /// The latest snapshot (cheap).
    pub fn snapshot(&self) -> Arc<GitSnapshot> {
        self.snapshot.read().clone()
    }

    /// Number of HEAD + status computations so far (diagnostics, tests).
    pub fn computations(&self) -> u64 {
        self.computations.load(Ordering::Relaxed)
    }

    /// Calls `cb` with every new snapshot (on a worker thread). Drop the
    /// returned [`Subscription`] to stop.
    pub fn subscribe(
        &self,
        cb: impl Fn(&Arc<GitSnapshot>) + Send + Sync + 'static,
    ) -> Subscription {
        let mut subs = self.subscribers.lock();
        let id = subs.next;
        subs.next += 1;
        subs.list.push((id, Arc::new(cb)));
        Subscription {
            state: self.me.clone(),
            id,
        }
    }

    fn job_key(&self) -> JobKey {
        JobKey(format!("git-refresh:{}", self.repo.workdir().display()))
    }

    /// Requests a re-read of HEAD, status and conflicts (coalesced with other
    /// requests; runs on the scheduler).
    pub fn refresh(&self) {
        let weak = self.me.clone();
        self.scheduler.debounce(
            self.job_key(),
            REFRESH_DEBOUNCE,
            Priority::Background,
            move |cancel| {
                if let Some(state) = weak.upgrade() {
                    if !cancel.is_cancelled() {
                        state.compute();
                    }
                }
            },
        );
    }

    /// Like [`RepoState::refresh`], but subscribers are notified even if the
    /// state looks unchanged (after a write such as staging one more hunk).
    pub fn refresh_and_notify(&self) {
        self.force_notify.store(true, Ordering::Release);
        self.refresh();
    }

    /// Re-reads the state on the calling thread and returns the new snapshot
    /// (headless callers that need fresh state now).
    pub fn refresh_blocking(&self) -> Arc<GitSnapshot> {
        self.compute();
        self.snapshot()
    }

    fn compute(&self) {
        self.computations.fetch_add(1, Ordering::Relaxed);
        let head = self.repo.head();
        let status = self.repo.status();
        let conflicts = lector_workspace::conflicts::conflict_state(&self.repo);
        let error = status
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .or_else(|| head.as_ref().err().map(|e| e.to_string()));
        let force = self.force_notify.swap(false, Ordering::AcqRel);
        let new = {
            let mut guard = self.snapshot.write();
            let old = guard.clone();
            let head = head.ok();
            let status = match status {
                Ok(s) if *old.status == s => old.status.clone(),
                Ok(s) => Arc::new(s),
                Err(_) => old.status.clone(),
            };
            let conflicts = match conflicts {
                Ok(c) if *old.conflicts == c => old.conflicts.clone(),
                Ok(c) => Arc::new(c),
                Err(_) => old.conflicts.clone(),
            };
            let changed = head != old.head
                || !Arc::ptr_eq(&status, &old.status)
                || !Arc::ptr_eq(&conflicts, &old.conflicts)
                || error != old.error;
            if !(changed || force || old.generation == 0) {
                return;
            }
            let snap = Arc::new(GitSnapshot {
                head,
                status,
                conflicts,
                generation: old.generation + 1,
                error,
            });
            *guard = snap.clone();
            snap
        };
        let callbacks: Vec<Callback> = self
            .subscribers
            .lock()
            .list
            .iter()
            .map(|(_, cb)| cb.clone())
            .collect();
        for cb in callbacks {
            cb(&new);
        }
    }
}

/// Registry of [`RepoState`]s, one per repository work directory. States live
/// as long as someone holds them (windows, commands); the service keeps weak
/// references.
#[derive(Clone)]
pub struct GitService {
    scheduler: Scheduler,
    repos: Arc<Mutex<HashMap<PathBuf, Weak<RepoState>>>>,
    watch: bool,
}

impl std::fmt::Debug for GitService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitService")
            .field("repos", &self.repos.lock().len())
            .finish()
    }
}

impl GitService {
    /// A service that refreshes on `scheduler` and watches work trees.
    pub fn new(scheduler: Scheduler) -> Self {
        GitService {
            scheduler,
            repos: Arc::new(Mutex::new(HashMap::new())),
            watch: true,
        }
    }

    /// A service without file-system watchers (refresh only on request).
    pub fn without_watching(scheduler: Scheduler) -> Self {
        GitService {
            watch: false,
            ..Self::new(scheduler)
        }
    }

    /// The shared state of the repository containing `path` (None outside a repository).
    pub fn repo(&self, path: &Path) -> Option<Arc<RepoState>> {
        let repo = Repo::discover(path).ok()?;
        Some(self.repo_for(&repo))
    }

    /// The shared state of an already discovered repository.
    pub fn repo_for(&self, repo: &Repo) -> Arc<RepoState> {
        let key =
            dunce::canonicalize(repo.workdir()).unwrap_or_else(|_| repo.workdir().to_path_buf());
        let mut repos = self.repos.lock();
        if let Some(state) = repos.get(&key).and_then(Weak::upgrade) {
            return state;
        }
        repos.retain(|_, w| w.strong_count() > 0);
        let state = RepoState::new(repo.clone(), self.scheduler.clone(), self.watch);
        repos.insert(key, Arc::downgrade(&state));
        state
    }

    /// States currently alive.
    pub fn live_repos(&self) -> Vec<Arc<RepoState>> {
        self.repos
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", out);
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    fn wait_for(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn one_state_per_repo_and_one_computation_for_many_windows() {
        let dir = fixture();
        let s = Scheduler::new(2);
        let svc = GitService::without_watching(s.clone());
        let a = svc.repo(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let b = svc.repo(&dir.path().join("sub")).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(s.wait_idle(Duration::from_secs(10)));
        assert_eq!(a.snapshot().generation, 1);
        assert_eq!(a.snapshot().branch_label().as_deref(), Some("main"));
        let before = a.computations();
        // Ten "windows" ask for a refresh at once: one computation.
        for _ in 0..10 {
            a.refresh();
            b.refresh();
        }
        assert!(s.wait_idle(Duration::from_secs(10)));
        assert_eq!(a.computations(), before + 1);
        assert!(svc
            .repo(&std::env::temp_dir().join("lector-no-repo-here"))
            .is_none());
    }

    #[test]
    fn subscribers_see_changes_with_generation() {
        let dir = fixture();
        let s = Scheduler::new(2);
        let svc = GitService::without_watching(s.clone());
        let state = svc.repo(dir.path()).unwrap();
        assert!(s.wait_idle(Duration::from_secs(10)));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sub = {
            let seen = seen.clone();
            state.subscribe(move |snap| seen.lock().push((snap.generation, snap.status.len())))
        };
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        state.refresh();
        assert!(s.wait_idle(Duration::from_secs(10)));
        assert_eq!(*seen.lock(), vec![(2, 1)]);
        // Unchanged: no notification, unless forced.
        state.refresh();
        assert!(s.wait_idle(Duration::from_secs(10)));
        assert_eq!(seen.lock().len(), 1);
        state.refresh_and_notify();
        assert!(s.wait_idle(Duration::from_secs(10)));
        assert_eq!(seen.lock().last(), Some(&(3, 1)));
        drop(sub);
        std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();
        assert_eq!(state.refresh_blocking().status.len(), 2);
        assert_eq!(seen.lock().len(), 2);
    }

    #[test]
    fn watcher_drives_refresh() {
        let dir = fixture();
        let s = Scheduler::new(2);
        let svc = GitService::new(s.clone());
        let state = svc.repo(dir.path()).unwrap();
        assert!(wait_for(|| state.is_watching()));
        assert!(wait_for(|| state.snapshot().generation >= 1));
        std::fs::write(dir.path().join("c.txt"), "x\n").unwrap();
        assert!(wait_for(|| state
            .snapshot()
            .status
            .iter()
            .any(|e| e.path == "c.txt")));
        git(dir.path(), &["add", "c.txt"]);
        assert!(wait_for(|| state
            .snapshot()
            .status
            .iter()
            .any(|e| e.path == "c.txt" && e.staged.is_some())));
    }

    #[test]
    fn state_is_dropped_with_last_holder() {
        let dir = fixture();
        let s = Scheduler::new(2);
        let svc = GitService::without_watching(s.clone());
        let a = svc.repo(dir.path()).unwrap();
        assert_eq!(svc.live_repos().len(), 1);
        drop(a);
        assert!(s.wait_idle(Duration::from_secs(10)));
        assert!(svc.live_repos().is_empty());
    }
}
