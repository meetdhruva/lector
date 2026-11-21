//! `lector_app::store`: every piece of state Lector keeps for itself, in one database
//! (ADR 0004).
//!
//! The database is `data_dir()/lector.redb` ([`Store::default_path`]). Each concern has its
//! own table, a schema version and a forward migration ([`Concern`], [`concerns`]):
//!
//! | concern | table | version | handle | value type |
//! |---|---|---|---|---|
//! | `recent_projects` | `recent` | 1 | [`RecentProjects`] | path → stamp |
//! | `window_sessions` | `sessions` | 1 | [`WindowSessions`] | [`Session`] |
//! | `terminal_sessions` | `terminal_sessions` | 1 | [`TerminalSessions`] | [`TerminalSession`] |
//! | `review_state` | `review` | 1 | [`ReviewMarks`] (+ [`Store::hunk_reviews`], `kv`) | mark |
//! | `notices` | `notices` | 1 | [`Store::notices`] | caller's type (`lector-ui-agent`) |
//! | `update_state` | `update_state` | 1 | [`Store::update_state`] | `lector_forge::releases::UpdateState` |
//!
//! Comments, the activity journal, `kv` and the content cache keep their own tables and API
//! on [`crate::db::Db`], reached through [`Store::db`].
//!
//! **Transactions.** Every write is one redb write transaction; [`Keyspace::update`] is a
//! read-modify-write inside one. redb serializes write transactions between threads and,
//! through its file lock, between processes, so concurrent updates never lose each other.
//!
//! **Versions.** See [`keyspace`]: a concern stored by an older build is migrated forward,
//! one stored by a newer build reads as absent and is never written.
//!
//! **Several processes, locks, corruption.** See [`open`]: two Lector processes share the
//! database; a process that cannot open it for writing runs with read-only state
//! ([`StoreMode::ReadOnly`]: in memory, never written anywhere) and says so; a corrupt file
//! is copied aside and replaced by a fresh one.
//!
//! **Import.** The JSON files of 0.3 to 0.7.1 are imported once and renamed to
//! `*.imported` ([`Store::import_legacy`], [`legacy`]).
//!
//! Logs and crash reports are not state of this kind: they stay files
//! (`crate::health`, `lector.log`) so that they work when the database does not.

pub mod keyspace;
pub mod legacy;
pub mod open;
mod recent;
mod review;
mod sessions;
mod terminal;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_import;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{de::DeserializeOwned, Serialize};

pub use keyspace::{Access, Concern, Keyspace, Migration, Single};
pub use legacy::{ImportReport, MAX_LEGACY_BYTES};
pub use recent::{RecentProjects, MAX_RECENT_PROJECTS};
pub use review::{ReviewMarks, MAX_REVIEW_MARKS};
pub use sessions::{
    session_key, Axis, PaneState, Session, TabKind, TabState, WindowBounds, WindowSessions,
};
pub use terminal::{terminal_key, SavedTab, TerminalSession, TerminalSessions};

use crate::db::{Db, DbError, DbMode};
use crate::review_queue::HunkReviewStore;

/// File name of the database in the data directory.
pub const DB_FILE_NAME: &str = "lector.redb";

/// The concerns of the store and their schema versions.
pub mod concerns {
    use super::Concern;

    /// Recently opened project roots.
    pub const RECENT_PROJECTS: Concern = Concern {
        name: "recent",
        version: 1,
        migrate: None,
    };
    /// Window sessions per project.
    pub const WINDOW_SESSIONS: Concern = Concern {
        name: "sessions",
        version: 1,
        migrate: None,
    };
    /// Terminal tabs per project.
    pub const TERMINAL_SESSIONS: Concern = Concern {
        name: "terminal_sessions",
        version: 1,
        migrate: None,
    };
    /// Viewed marks.
    pub const REVIEW_STATE: Concern = Concern {
        name: "review",
        version: 1,
        migrate: None,
    };
    /// One-time questions and notes of the update and health notices.
    pub const NOTICES: Concern = Concern {
        name: "notices",
        version: 1,
        migrate: None,
    };
    /// The update check's last request, `ETag` and release list.
    pub const UPDATE_STATE: Concern = Concern {
        name: "update_state",
        version: 1,
        migrate: None,
    };

    /// Concerns whose values are JSON bytes in a table of their own name.
    pub const BYTE_CONCERNS: &[&Concern] = &[
        &WINDOW_SESSIONS,
        &TERMINAL_SESSIONS,
        &REVIEW_STATE,
        &NOTICES,
        &UPDATE_STATE,
    ];
}

/// Errors of the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The concern was written by a newer Lector: it is left untouched.
    #[error("state `{concern}` was written by a newer Lector (schema {found}, this one knows {known}); left untouched")]
    NewerSchema {
        /// Concern name.
        concern: &'static str,
        /// Version in the database.
        found: u64,
        /// Version this build knows.
        known: u64,
    },
    /// A stored value could not be migrated forward.
    #[error("state `{concern}`: cannot migrate a value from schema {from}: {reason}")]
    Migration {
        /// Concern name.
        concern: &'static str,
        /// Version of the value.
        from: u64,
        /// Why.
        reason: String,
    },
    /// A value could not be (de)serialized.
    #[error("state `{concern}`: {source}")]
    Value {
        /// Concern name.
        concern: &'static str,
        /// The serde error.
        source: serde_json::Error,
    },
}

macro_rules! store_from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                StoreError::Db(DbError::from(e))
            }
        }
    )*};
}
store_from_redb!(
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError
);

/// `Result` with [`StoreError`].
pub type StoreResult<T> = Result<T, StoreError>;

/// Whether this process persists state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreMode {
    /// Changes are stored in the database (shared with other Lector processes), or in an
    /// in-memory database the caller asked for (tests, `--no-state`).
    ReadWrite,
    /// The database could not be opened for writing: state was read once (when the file
    /// could be read) and changes made in this process are kept in memory only and lost on
    /// exit. Nothing is written to the database or anywhere else.
    ReadOnly {
        /// Why (for the log and the UI).
        reason: String,
    },
}

#[derive(Debug, Default)]
pub(crate) struct StoreInfo {
    pub(crate) recovered_from: Option<PathBuf>,
}

/// The state store. Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct Store {
    db: Db,
    info: Arc<StoreInfo>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("db", &self.db)
            .field("recovered_from", &self.info.recovered_from)
            .finish()
    }
}

impl Store {
    /// `lector_config::paths::data_dir()/lector.redb`.
    pub fn default_path() -> PathBuf {
        lector_config::paths::data_dir().join("lector.redb")
    }

    /// Opens the database at `path` by the policy in [`open`]. Never fails: see
    /// [`Store::mode`] for what came of it.
    pub fn open(path: &Path) -> Store {
        open::open(path)
    }

    /// A store in memory (tests, `--no-state`).
    pub fn in_memory() -> Store {
        let store = Store::from_db(Db::in_memory());
        if let Err(e) = store.create_tables() {
            tracing::warn!("cannot create the state tables: {e}");
        }
        store
    }

    /// A store over an already opened database.
    pub fn from_db(db: Db) -> Store {
        Store::with_info(db, StoreInfo::default())
    }

    pub(crate) fn with_info(db: Db, info: StoreInfo) -> Store {
        Store {
            db,
            info: Arc::new(info),
        }
    }

    /// Creates the tables of every concern (so that readers in other processes and
    /// read-only snapshots find them).
    pub(crate) fn create_tables(&self) -> StoreResult<()> {
        let tx = self.db.raw().begin_write().map_err(DbError::from)?;
        tx.open_table(keyspace::STORE_META)?;
        tx.open_table(crate::db::META)?;
        tx.open_table(crate::db::RECENT)?;
        tx.open_table(crate::db::KV)?;
        tx.open_table(crate::db::CACHE)?;
        tx.open_table(crate::db::COMMENTS)?;
        tx.open_table(crate::db::JOURNAL)?;
        for c in concerns::BYTE_CONCERNS {
            tx.open_table(c.table())?;
        }
        tx.commit().map_err(DbError::from)?;
        Ok(())
    }

    /// Whether this process persists state.
    pub fn mode(&self) -> StoreMode {
        match self.db.mode() {
            DbMode::ReadOnly { reason, .. } => StoreMode::ReadOnly {
                reason: reason.clone(),
            },
            DbMode::File(_) | DbMode::Memory => StoreMode::ReadWrite,
        }
    }

    /// The copy of a damaged database that was set aside when this store started fresh.
    pub fn recovered_from(&self) -> Option<&Path> {
        self.info.recovered_from.as_deref()
    }

    /// The database (comments, journal, `kv`, cache).
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Recently opened projects.
    pub fn recent_projects(&self) -> RecentProjects {
        RecentProjects::new(self.db.clone())
    }

    /// Window sessions per project.
    pub fn window_sessions(&self) -> WindowSessions {
        WindowSessions::new(self.db.clone())
    }

    /// Terminal tabs per project.
    pub fn terminal_sessions(&self) -> TerminalSessions {
        TerminalSessions::new(self.db.clone())
    }

    /// Viewed marks.
    pub fn review_marks(&self) -> ReviewMarks {
        ReviewMarks::new(self.db.clone())
    }

    /// Reviewed hunks (part of `review_state`; stored in `kv`).
    pub fn hunk_reviews(&self) -> HunkReviewStore {
        HunkReviewStore::new(&self.db)
    }

    /// The notices' state. The value type belongs to the UI crate that shows the notices;
    /// the concern (name, version, migration) belongs to the store.
    pub fn notices<T: Serialize + DeserializeOwned>(&self) -> Single<T> {
        Single::new(self.db.clone(), &concerns::NOTICES)
    }

    /// The update check's state (`lector_forge::releases::UpdateState`, which stays a plain
    /// value type of the forge crate).
    pub fn update_state<T: Serialize + DeserializeOwned>(&self) -> Single<T> {
        Single::new(self.db.clone(), &concerns::UPDATE_STATE)
    }

    /// Imports the JSON state files of 0.3 to 0.7.1 found in `data_dir`, once, and renames
    /// them to `*.imported` ([`legacy`]). Does nothing when the state is read-only.
    pub fn import_legacy(&self, data_dir: &Path) -> ImportReport {
        legacy::import(self, data_dir)
    }
}
