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

/// How to build an [`AppCore`].
#[derive(Debug, Clone, Default)]
pub struct CoreOptions {
    /// Database file; `None` = `lector_config::paths::data_dir()/lector.redb`.
    pub db_path: Option<PathBuf>,
    /// Use an in-memory database (tests, `--no-state`).
    pub in_memory_db: bool,
    /// Worker threads; `None` = the process-wide [`Scheduler::global`].
    pub workers: Option<usize>,
    /// Import the v0.3 JSON stores from this data directory on first open
    /// (`None` = `data_dir()` when `db_path` is `None`, otherwise no import).
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
    db: Db,
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
            .field("db", &self.inner.db)
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

    /// Builds a core. Never fails: a database that cannot be opened falls back
    /// to a temporary copy or memory (see [`Db::open_or_fallback`]).
    pub fn open(opts: CoreOptions) -> AppCore {
        let scheduler = match opts.workers {
            Some(n) => Scheduler::new(n),
            None => Scheduler::global(),
        };
        let db = if opts.in_memory_db {
            Db::in_memory()
        } else {
            let path = opts
                .db_path
                .clone()
                .unwrap_or_else(|| lector_config::paths::data_dir().join("lector.redb"));
            Db::open_or_fallback(&path)
        };
        let import_dir = opts.import_from.clone().or_else(|| {
            (opts.db_path.is_none() && !opts.in_memory_db).then(lector_config::paths::data_dir)
        });
        if let Some(dir) = import_dir {
            if let Err(err) = db.import_v03(&dir) {
                tracing::warn!("cannot import v0.3 state from {}: {err}", dir.display());
            }
        }
        let registry = CommandRegistry::new();
        crate::commands::register_builtin(&registry);
        let git = GitService::new(scheduler.clone());
        let core = AppCore {
            inner: Arc::new(Inner {
                scheduler,
                registry,
                db,
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

    /// The database.
    pub fn db(&self) -> &Db {
        &self.inner.db
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

    /// Viewed marks backed by the database (pass to `lector_workspace::review`).
    pub fn review_store(&self) -> lector_config::ReviewStore {
        self.inner.db.review_store()
    }
}

#[cold]
fn unreachable_extension<T>() -> T {
    // Keys are `TypeId::of::<T>()`, so the downcast cannot fail.
    panic!("extension slot holds a different type")
}
