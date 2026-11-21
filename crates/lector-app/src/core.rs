//! [`AppCore`]: the clone-able handle to everything headless Lector is made of.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

use crate::commands::{ActivitySink, CommandRegistry};
use crate::db::Db;
use crate::git_service::GitService;
use crate::scheduler::Scheduler;
use crate::store::Store;

/// How to build an [`AppCore`].
#[derive(Debug, Clone, Default)]
pub struct CoreOptions {
    /// Database file; `None` = [`Store::default_path`].
    pub db_path: Option<PathBuf>,
    /// Use an in-memory database (tests, `--no-state`).
    pub in_memory_db: bool,
    /// Worker threads; `None` = the process-wide [`Scheduler::global`].
    pub workers: Option<usize>,
    /// Import the JSON state files of 0.3 to 0.7.1 from this data directory
    /// ([`Store::import_legacy`]; `None` = `data_dir()` when `db_path` is `None`, otherwise
    /// no import).
    pub import_from: Option<PathBuf>,
    /// Lector's data directory for the journal's shadow repositories
    /// (`None` = `data_dir()`, or a per-process temp dir with `in_memory_db`).
    pub data_dir: Option<PathBuf>,
}

impl CoreOptions {
    /// An isolated core for tests: in-memory db, own 2-thread pool.
    pub fn for_tests() -> Self {
        CoreOptions {
            in_memory_db: true,
            workers: Some(2),
            ..Default::default()
        }
    }
}

struct Inner {
    scheduler: Scheduler,
    registry: CommandRegistry,
    store: Store,
    git: GitService,
    activity: RwLock<Option<Arc<dyn ActivitySink>>>,
    extensions: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

/// Handle to the application core: scheduler, command registry, database, git
/// service, activity sink (journal) and typed extension slots (document store, ...).
/// Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct AppCore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AppCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCore")
            .field("commands", &self.inner.registry.len())
            .field("store", &self.inner.store)
            .finish()
    }
}

impl AppCore {
    /// The process-wide core (GUI): global scheduler, the user's database.
    pub fn global() -> AppCore {
        static GLOBAL: OnceLock<AppCore> = OnceLock::new();
        GLOBAL
            .get_or_init(|| AppCore::open(CoreOptions::default()))
            .clone()
    }

    /// Builds a core. Never fails: a database that cannot be opened for writing gives
    /// read-only state, a corrupt one is replaced (see [`Store::open`]).
    pub fn open(opts: CoreOptions) -> AppCore {
        let scheduler = match opts.workers {
            Some(n) => Scheduler::new(n),
            None => Scheduler::global(),
        };
        let store = if opts.in_memory_db {
            Store::in_memory()
        } else {
            let path = opts.db_path.clone().unwrap_or_else(Store::default_path);
            Store::open(&path)
        };
        let import_dir = opts.import_from.clone().or_else(|| {
            (opts.db_path.is_none() && !opts.in_memory_db).then(lector_config::paths::data_dir)
        });
        if let Some(dir) = import_dir {
            store.import_legacy(&dir);
        }
        let registry = CommandRegistry::new();
        crate::commands::register_builtin(&registry);
        let git = GitService::new(scheduler.clone());
        let core = AppCore {
            inner: Arc::new(Inner {
                scheduler,
                registry,
                store,
                git,
                activity: RwLock::new(None),
                extensions: RwLock::new(HashMap::new()),
            }),
        };
        // The activity journal records every write command.
        match (opts.data_dir, opts.in_memory_db) {
            (Some(dir), _) => {
                crate::journal::Journal::install_in(&core, dir);
            }
            (None, true) => {
                let dir = std::env::temp_dir().join(format!("lector-core-{}", std::process::id()));
                crate::journal::Journal::install_in(&core, dir);
            }
            (None, false) => {
                crate::journal::Journal::install(&core);
            }
        }
        core
    }

    /// The scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.inner.scheduler
    }

    /// The command registry.
    pub fn registry(&self) -> &CommandRegistry {
        &self.inner.registry
    }

    /// The state store (ADR 0004).
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The database under the store (comments, journal, `kv`, cache).
    pub fn db(&self) -> &Db {
        self.inner.store.db()
    }

    /// The git service (one shared state per repository).
    pub fn git(&self) -> &GitService {
        &self.inner.git
    }

    /// Plugs in the activity journal.
    pub fn set_activity_sink(&self, sink: Arc<dyn ActivitySink>) {
        *self.inner.activity.write() = Some(sink);
    }

    /// The activity journal, if plugged in.
    pub fn activity_sink(&self) -> Option<Arc<dyn ActivitySink>> {
        self.inner.activity.read().clone()
    }

    /// Stores a shared service by type (e.g. the document store, the journal).
    pub fn set_extension<T: Any + Send + Sync>(&self, value: Arc<T>) {
        self.inner
            .extensions
            .write()
            .insert(TypeId::of::<T>(), value);
    }

    /// A service stored with [`AppCore::set_extension`].
    pub fn extension<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let any = self
            .inner
            .extensions
            .read()
            .get(&TypeId::of::<T>())?
            .clone();
        any.downcast::<T>().ok()
    }

    /// The service of type `T`, created with `init` on first use.
    pub fn extension_or_init<T: Any + Send + Sync>(&self, init: impl FnOnce() -> Arc<T>) -> Arc<T> {
        if let Some(v) = self.extension::<T>() {
            return v;
        }
        let mut map = self.inner.extensions.write();
        let entry = map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| init() as Arc<dyn Any + Send + Sync>)
            .clone();
        drop(map);
        match entry.downcast::<T>() {
            Ok(v) => v,
            Err(_) => unreachable_extension(),
        }
    }

    /// Review data (changesets, file diffs, viewed marks) for views, the CLI and agents.
    pub fn review(&self) -> crate::review_service::ReviewService {
        crate::review_service::ReviewService::new(self.review_marks())
    }

    /// Viewed marks.
    pub fn review_marks(&self) -> crate::store::ReviewMarks {
        self.inner.store.review_marks()
    }
}

#[cold]
fn unreachable_extension<T>() -> T {
    // Keys are `TypeId::of::<T>()`, so the downcast cannot fail.
    panic!("extension slot holds a different type")
}
