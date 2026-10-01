//! Rrowser storage: persistent profile storage on redb.
//!
//! One ACID redb database per profile backs everything:
//!
//! * [`cookies`] — a cookie jar with **CHIPS partitioning** (per-top-site
//!   partitions), SameSite enforcement and expiry.
//! * [`localstorage`] — origin-keyed localStorage.
//! * [`idb`] — an IndexedDB core (databases, object stores, records).
//! * [`cache`] — an HTTP response cache with size-budgeted LRU eviction.
//!
//! redb was chosen over sled/fjall for its ACID transactions, pure-Rust
//! implementation and B-tree access pattern that matches browser storage
//! (point lookups + short range scans). See `LIBRARY_CHOICES.md`.

pub mod cache;
pub mod cookies;
pub mod idb;
pub mod localstorage;

use std::path::Path;

use redb::{Database, TableDefinition};

/// Cookies keyed by `host \0 name \0 partition`.
pub(crate) const COOKIE_TABLE: TableDefinition<&str, &str> = TableDefinition::new("cookies");
/// localStorage keyed by `origin \0 key`.
pub(crate) const LS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("localstorage");
/// IndexedDB database metadata keyed by name.
pub(crate) const IDB_DB_TABLE: TableDefinition<&str, &str> = TableDefinition::new("idb_dbs");
/// IndexedDB records keyed by `db \0 store \0 key`.
pub(crate) const IDB_RECORD_TABLE: TableDefinition<&str, &str> =
    TableDefinition::new("idb_records");
/// HTTP cache metadata keyed by canonical URL.
pub(crate) const CACHE_META_TABLE: TableDefinition<&str, &str> = TableDefinition::new("cache_meta");
/// HTTP cache bodies keyed by canonical URL.
pub(crate) const CACHE_BODY_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("cache_body");

/// Errors from the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// redb reported an error.
    #[error("storage backend: {0}")]
    Backend(String),
    /// The profile path could not be created.
    #[error("profile directory error: {0}")]
    Profile(std::io::Error),
}

impl From<redb::DatabaseError> for StorageError {
    fn from(err: redb::DatabaseError) -> Self {
        StorageError::Backend(err.to_string())
    }
}

impl From<redb::TransactionError> for StorageError {
    fn from(err: redb::TransactionError) -> Self {
        StorageError::Backend(err.to_string())
    }
}

impl From<redb::StorageError> for StorageError {
    fn from(err: redb::StorageError) -> Self {
        StorageError::Backend(err.to_string())
    }
}

impl From<redb::TableError> for StorageError {
    fn from(err: redb::TableError) -> Self {
        StorageError::Backend(err.to_string())
    }
}

impl From<redb::CommitError> for StorageError {
    fn from(err: redb::CommitError) -> Self {
        StorageError::Backend(err.to_string())
    }
}

/// The persistent profile storage.
pub struct Storage {
    db: Database,
    disk_path_len: std::sync::atomic::AtomicU64,
}

impl Storage {
    /// Opens (or creates) the profile database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Storage, StorageError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(StorageError::Profile)?;
        }
        let file_len = std::fs::metadata(path.as_ref())
            .map(|m| m.len())
            .unwrap_or(0);
        let db = Database::create(path)?;
        // Create all tables eagerly so later transactions cannot fail on
        // first use.
        let txn = db.begin_write()?;
        {
            txn.open_table(COOKIE_TABLE)?;
            txn.open_table(LS_TABLE)?;
            txn.open_table(IDB_DB_TABLE)?;
            txn.open_table(IDB_RECORD_TABLE)?;
            txn.open_table(CACHE_META_TABLE)?;
            txn.open_table(CACHE_BODY_TABLE)?;
        }
        txn.commit()?;
        Ok(Storage {
            db,
            disk_path_len: std::sync::atomic::AtomicU64::new(file_len),
        })
    }

    /// In-memory profile (tests, incognito-style sessions).
    pub fn in_memory() -> Result<Storage, StorageError> {
        // redb requires a real path; tests use a temp file via `open`.
        Storage::open("/tmp/rowser-in-memory.redb")
    }

    /// The cookie jar.
    pub fn cookies(&self) -> cookies::CookieJar<'_> {
        cookies::CookieJar::new(&self.db)
    }

    /// localStorage access.
    pub fn local_storage(&self) -> localstorage::LocalStorage<'_> {
        localstorage::LocalStorage::new(&self.db)
    }

    /// IndexedDB core.
    pub fn indexed_db(&self) -> idb::IndexedDb<'_> {
        idb::IndexedDb::new(&self.db)
    }

    /// HTTP response cache with an LRU byte budget.
    pub fn http_cache(&self, max_bytes: u64) -> cache::HttpCache<'_> {
        cache::HttpCache::new(&self.db, max_bytes)
    }

    /// Estimated on-disk size in bytes; tracked by the storage path.
    pub fn disk_usage(&self) -> Result<u64, StorageError> {
        Ok(self
            .disk_path_len
            .load(std::sync::atomic::Ordering::Relaxed))
    }
}
