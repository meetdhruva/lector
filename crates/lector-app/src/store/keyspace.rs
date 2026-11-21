//! Concerns, their schema versions, and typed keyspaces over them.
//!
//! Every concern records its schema version in the `store_meta` table
//! (`version:<concern>`; absent means 1, the shape every value had before versions were
//! recorded). Each read checks the stored version inside its read transaction and each
//! write inside its write transaction, so a concern upgraded by another (newer) process
//! while this one runs is noticed at the next access:
//!
//! * stored == known: plain access;
//! * stored <  known: reads migrate each value in memory; the first write migrates every
//!   row of the concern and records the new version, in the same transaction as the write;
//! * stored >  known: the concern was written by a newer Lector. Reads see nothing (logged
//!   once), writes fail with [`StoreError::NewerSchema`]; nothing is ever overwritten.

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::{Mutex, OnceLock};

use redb::{
    ReadTransaction, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
    WriteTransaction,
};
use serde::{de::DeserializeOwned, Serialize};

use super::{StoreError, StoreResult};
use crate::db::Db;

/// Versions and import markers of the store.
pub(crate) const STORE_META: TableDefinition<&str, u64> = TableDefinition::new("store_meta");

/// Rewrites one stored value from schema `from` to the concern's current version.
pub type Migration = fn(from: u64, value: &[u8]) -> Result<Vec<u8>, String>;

/// One concern of the store: its table, its schema version and its forward migration.
#[derive(Debug)]
pub struct Concern {
    /// Name; also the redb table that holds it.
    pub name: &'static str,
    /// Schema version this build writes.
    pub version: u64,
    /// Migration from older versions (`None`: there has been no other version).
    pub migrate: Option<Migration>,
}

/// How this build may use a concern, given the version stored in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The stored version is the one this build knows.
    Current,
    /// Stored by an older build: values are migrated on read, the concern on first write.
    Older(u64),
    /// Stored by a newer build: read as absent, never written.
    Newer(u64),
}

impl Concern {
    /// The concern's table, for byte-valued concerns.
    pub(crate) fn table(&self) -> TableDefinition<'static, &'static str, &'static [u8]> {
        TableDefinition::new(self.name)
    }

    fn version_key(&self) -> String {
        format!("version:{}", self.name)
    }

    fn access(&self, stored: u64) -> Access {
        match stored.cmp(&self.version) {
            std::cmp::Ordering::Equal => Access::Current,
            std::cmp::Ordering::Less => Access::Older(stored),
            std::cmp::Ordering::Greater => Access::Newer(stored),
        }
    }

    /// The current-version bytes of a value stored under `access` (`None`: newer, unreadable).
    pub(crate) fn decode_bytes(
        &'static self,
        access: Access,
        bytes: Vec<u8>,
    ) -> StoreResult<Option<Vec<u8>>> {
        match access {
            Access::Current => Ok(Some(bytes)),
            Access::Newer(found) => {
                warn_newer(self, found);
                Ok(None)
            }
            Access::Older(from) => {
                let migrate = self.migrate.ok_or(StoreError::Migration {
                    concern: self.name,
                    from,
                    reason: "no migration from this version".into(),
                })?;
                migrate(from, &bytes)
                    .map(Some)
                    .map_err(|reason| StoreError::Migration {
                        concern: self.name,
                        from,
                        reason,
                    })
            }
        }
    }
}

/// Logs once per concern and process that a newer Lector owns it.
fn warn_newer(concern: &Concern, found: u64) {
    static WARNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut warned = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if warned.insert(concern.name) {
        tracing::warn!(
            "state `{}` has schema {found}, newer than this Lector knows ({}): read as empty and left untouched",
            concern.name,
            concern.version
        );
    }
}

/// The version stored for `concern`, read in `tx`.
pub(crate) fn stored_version(tx: &ReadTransaction, concern: &Concern) -> StoreResult<u64> {
    match tx.open_table(STORE_META) {
        Ok(t) => Ok(t
            .get(concern.version_key().as_str())?
            .map(|g| g.value())
            .unwrap_or(1)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(1),
        Err(e) => Err(e.into()),
    }
}

/// Runs `f` in a read transaction with the concern's [`Access`].
pub(crate) fn read<R>(
    db: &Db,
    concern: &'static Concern,
    f: impl FnOnce(&ReadTransaction, Access) -> StoreResult<R>,
) -> StoreResult<R> {
    let tx = db.raw().begin_read().map_err(crate::db::DbError::from)?;
    let access = concern.access(stored_version(&tx, concern)?);
    f(&tx, access)
}

/// Opens a byte table for reading; `None` when it was never created.
pub(crate) fn open_read<'t>(
    tx: &ReadTransaction,
    def: TableDefinition<'t, &'static str, &'static [u8]>,
) -> StoreResult<Option<redb::ReadOnlyTable<&'static str, &'static [u8]>>> {
    match tx.open_table(def) {
        Ok(t) => Ok(Some(t)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Makes `concern` writable inside `tx`: refuses a newer schema, migrates an older one
/// (every row, in this transaction) and records the current version.
pub(crate) fn prepare(tx: &WriteTransaction, concern: &'static Concern) -> StoreResult<()> {
    let key = concern.version_key();
    let recorded = {
        let meta = tx.open_table(STORE_META)?;
        let v = meta.get(key.as_str())?.map(|g| g.value());
        v
    };
    let stored = recorded.unwrap_or(1);
    match concern.access(stored) {
        Access::Newer(found) => {
            warn_newer(concern, found);
            return Err(StoreError::NewerSchema {
                concern: concern.name,
                found,
                known: concern.version,
            });
        }
        Access::Older(from) => migrate_all(tx, concern, from)?,
        Access::Current => {}
    }
    if recorded != Some(concern.version) {
        tx.open_table(STORE_META)?
            .insert(key.as_str(), concern.version)?;
    }
    Ok(())
}

fn migrate_all(tx: &WriteTransaction, concern: &'static Concern, from: u64) -> StoreResult<()> {
    let mut table = tx.open_table(concern.table())?;
    if table.is_empty()? {
        return Ok(());
    }
    let mut rows = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        rows.push((k.value().to_string(), v.value().to_vec()));
    }
    for (k, v) in rows {
        let Some(new) = concern.decode_bytes(Access::Older(from), v)? else {
            continue;
        };
        table.insert(k.as_str(), new.as_slice())?;
    }
    tracing::info!(
        "state `{}` migrated from schema {from} to {}",
        concern.name,
        concern.version
    );
    Ok(())
}

/// Runs `f` in one write transaction on a prepared concern and commits.
pub(crate) fn write<R>(
    db: &Db,
    concern: &'static Concern,
    f: impl FnOnce(&WriteTransaction) -> StoreResult<R>,
) -> StoreResult<R> {
    let tx = db.begin_write().map_err(crate::db::DbError::from)?;
    prepare(&tx, concern)?;
    let out = f(&tx)?;
    tx.commit().map_err(crate::db::DbError::from)?;
    Ok(out)
}

/// The access a concern has right now (for diagnostics and tests).
pub(crate) fn access(db: &Db, concern: &'static Concern) -> StoreResult<Access> {
    read(db, concern, |_, a| Ok(a))
}

/// Typed values of one concern, by string key, stored as JSON.
pub struct Keyspace<T> {
    db: Db,
    concern: &'static Concern,
    _value: PhantomData<fn() -> T>,
}

impl<T> Clone for Keyspace<T> {
    fn clone(&self) -> Self {
        Keyspace {
            db: self.db.clone(),
            concern: self.concern,
            _value: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for Keyspace<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyspace")
            .field("concern", &self.concern.name)
            .finish()
    }
}

impl<T: Serialize + DeserializeOwned> Keyspace<T> {
    /// The keyspace of `concern` in `db`.
    pub fn new(db: Db, concern: &'static Concern) -> Self {
        Keyspace {
            db,
            concern,
            _value: PhantomData,
        }
    }

    /// The concern.
    pub fn concern(&self) -> &'static Concern {
        self.concern
    }

    /// How this build may use the concern right now.
    pub fn access(&self) -> StoreResult<Access> {
        access(&self.db, self.concern)
    }

    fn decode(&self, access: Access, bytes: Vec<u8>) -> StoreResult<Option<T>> {
        match self.concern.decode_bytes(access, bytes)? {
            Some(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|source| StoreError::Value {
                    concern: self.concern.name,
                    source,
                }),
            None => Ok(None),
        }
    }

    /// The value under `key` (`None`: absent, or the concern belongs to a newer Lector).
    pub fn get(&self, key: &str) -> StoreResult<Option<T>> {
        let c = self.concern;
        read(&self.db, c, |tx, access| {
            let Some(t) = open_read(tx, c.table())? else {
                return Ok(None);
            };
            let bytes = t.get(key)?.map(|g| g.value().to_vec());
            match bytes {
                Some(b) => self.decode(access, b),
                None => Ok(None),
            }
        })
    }

    /// Every readable `(key, value)`, sorted by key (unreadable values are skipped and logged).
    pub fn entries(&self) -> StoreResult<Vec<(String, T)>> {
        let c = self.concern;
        read(&self.db, c, |tx, access| {
            let Some(t) = open_read(tx, c.table())? else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for item in t.iter()? {
                let (k, v) = item?;
                match self.decode(access, v.value().to_vec()) {
                    Ok(Some(value)) => out.push((k.value().to_string(), value)),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("skipping unreadable state value: {e}"),
                }
            }
            Ok(out)
        })
    }

    /// Number of stored keys (0 when the concern belongs to a newer Lector).
    pub fn len(&self) -> StoreResult<u64> {
        let c = self.concern;
        read(&self.db, c, |tx, access| {
            if matches!(access, Access::Newer(_)) {
                return Ok(0);
            }
            match open_read(tx, c.table())? {
                Some(t) => Ok(t.len()?),
                None => Ok(0),
            }
        })
    }

    /// True when nothing is stored.
    pub fn is_empty(&self) -> StoreResult<bool> {
        Ok(self.len()? == 0)
    }

    /// Stores `value` under `key`.
    pub fn put(&self, key: &str, value: &T) -> StoreResult<()> {
        let bytes = serde_json::to_vec(value).map_err(|source| StoreError::Value {
            concern: self.concern.name,
            source,
        })?;
        write(&self.db, self.concern, |tx| {
            tx.open_table(self.concern.table())?
                .insert(key, bytes.as_slice())?;
            Ok(())
        })
    }

    /// Removes `key` (absent keys are fine).
    pub fn remove(&self, key: &str) -> StoreResult<()> {
        write(&self.db, self.concern, |tx| {
            tx.open_table(self.concern.table())?.remove(key)?;
            Ok(())
        })
    }

    /// Read-modify-write of `key` in ONE transaction: `f` sees the current value (`None`
    /// when absent; an unreadable value is logged and seen as `None`) and leaves the new
    /// one (`None` removes the key). Concurrent updates, from threads of this process or
    /// from another process, are serialized by the database's writer lock: none is lost.
    pub fn update<R>(&self, key: &str, f: impl FnOnce(&mut Option<T>) -> R) -> StoreResult<R> {
        let c = self.concern;
        write(&self.db, c, |tx| {
            let mut t = tx.open_table(c.table())?;
            let current = t.get(key)?.map(|g| g.value().to_vec());
            let mut value = match current {
                Some(bytes) => match serde_json::from_slice::<T>(&bytes) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!("state `{}`: replacing an unreadable value: {e}", c.name);
                        None
                    }
                },
                None => None,
            };
            let out = f(&mut value);
            match value {
                Some(v) => {
                    let bytes = serde_json::to_vec(&v).map_err(|source| StoreError::Value {
                        concern: c.name,
                        source,
                    })?;
                    t.insert(key, bytes.as_slice())?;
                }
                None => {
                    t.remove(key)?;
                }
            }
            Ok(out)
        })
    }
}

/// A concern that holds one value (the notices, the update check's state).
pub struct Single<T> {
    keyspace: Keyspace<T>,
}

impl<T> Clone for Single<T> {
    fn clone(&self) -> Self {
        Single {
            keyspace: self.keyspace.clone(),
        }
    }
}

impl<T> std::fmt::Debug for Single<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.keyspace.fmt(f)
    }
}

/// The key a [`Single`] stores its value under.
pub(crate) const SINGLE_KEY: &str = "value";

impl<T: Serialize + DeserializeOwned> Single<T> {
    /// The single value of `concern` in `db`.
    pub fn new(db: Db, concern: &'static Concern) -> Self {
        Single {
            keyspace: Keyspace::new(db, concern),
        }
    }

    /// The stored value.
    pub fn get(&self) -> StoreResult<Option<T>> {
        self.keyspace.get(SINGLE_KEY)
    }

    /// Replaces the value.
    pub fn put(&self, value: &T) -> StoreResult<()> {
        self.keyspace.put(SINGLE_KEY, value)
    }

    /// Read-modify-write in one transaction (see [`Keyspace::update`]).
    pub fn update<R>(&self, f: impl FnOnce(&mut Option<T>) -> R) -> StoreResult<R> {
        self.keyspace.update(SINGLE_KEY, f)
    }

    /// How this build may use the concern right now.
    pub fn access(&self) -> StoreResult<Access> {
        self.keyspace.access()
    }
}
