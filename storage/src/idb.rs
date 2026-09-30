//! IndexedDB core: databases, object stores and records on redb.
//!
//! V1 implements the storage engine layer: databases with named object
//! stores, string keys and JSON values, all under ACID transactions. The
//! full W3C event/transaction model lives in the JS binding layer
//! (see `js` crate and `docs/ROADMAP`).

use redb::{Database, ReadableTable};
use serde::{Deserialize, Serialize};

use crate::{IDB_DB_TABLE, IDB_RECORD_TABLE, StorageError};

/// Metadata for one IndexedDB database.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IdbDatabaseMeta {
    /// Object store names.
    pub stores: Vec<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
}

/// IndexedDB core.
#[derive(Debug)]
pub struct IndexedDb<'a> {
    db: &'a Database,
}

impl<'a> IndexedDb<'a> {
    /// Wraps the profile database.
    pub fn new(db: &'a Database) -> Self {
        IndexedDb { db }
    }

    /// Opens (or creates) a database.
    pub fn open_database(&self, name: &str) -> Result<IdbDatabaseMeta, StorageError> {
        let txn = self.db.begin_write()?;
        let meta = {
            let mut table = txn.open_table(IDB_DB_TABLE)?;
            let meta = table
                .get(name)?
                .and_then(|v| serde_json::from_str::<IdbDatabaseMeta>(v.value()).ok())
                .unwrap_or(IdbDatabaseMeta {
                    stores: Vec::new(),
                    created_at: jiff::Timestamp::now().as_second(),
                });
            let raw = serde_json::to_string(&meta).unwrap_or_default();
            table.insert(name, raw.as_str())?;
            meta
        };
        txn.commit()?;
        Ok(meta)
    }

    /// Creates an object store (idempotent).
    pub fn create_store(&self, name: &str, store: &str) -> Result<(), StorageError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(IDB_DB_TABLE)?;
            let mut meta = table
                .get(name)?
                .and_then(|v| serde_json::from_str::<IdbDatabaseMeta>(v.value()).ok())
                .unwrap_or_default();
            if !meta.stores.iter().any(|s| s == store) {
                meta.stores.push(store.to_owned());
            }
            let raw = serde_json::to_string(&meta).unwrap_or_default();
            table.insert(name, raw.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Lists database names.
    pub fn database_names(&self) -> Result<Vec<String>, StorageError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(IDB_DB_TABLE)?;
        Ok(table
            .iter()
            .map_err(|e| StorageError::Backend(e.to_string()))?
            .filter_map(|e| e.ok().map(|(k, _)| k.value().to_owned()))
            .collect())
    }

    /// Deletes a whole database (stores + records).
    pub fn delete_database(&self, name: &str) -> Result<(), StorageError> {
        let txn = self.db.begin_write()?;
        {
            let mut dbs = txn.open_table(IDB_DB_TABLE)?;
            let _stores = dbs
                .get(name)?
                .and_then(|v| serde_json::from_str::<IdbDatabaseMeta>(v.value()).ok())
                .map(|meta| meta.stores)
                .unwrap_or_default();
            dbs.remove(name)?;
            let mut records = txn.open_table(IDB_RECORD_TABLE)?;
            let prefix = format!("{name}\u{0}");
            let keys: Vec<String> = records
                .range::<&str>(prefix.as_str()..)
                .map_err(|e| StorageError::Backend(e.to_string()))?
                .filter_map(|e| e.ok().map(|(k, _)| k.value().to_owned()))
                .take_while(|k| k.starts_with(&prefix))
                .collect();
            for key in keys {
                records.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// `put` a JSON value under a string key.
    pub fn put(&self, db: &str, store: &str, key: &str, value: &str) -> Result<(), StorageError> {
        let record_key = format!("{db}\u{0}{store}\u{0}{key}");
        let txn = self.db.begin_write()?;
        {
            let mut records = txn.open_table(IDB_RECORD_TABLE)?;
            records.insert(record_key.as_str(), value)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// `get` a JSON value.
    pub fn get(&self, db: &str, store: &str, key: &str) -> Result<Option<String>, StorageError> {
        let record_key = format!("{db}\u{0}{store}\u{0}{key}");
        let txn = self.db.begin_read()?;
        let records = txn.open_table(IDB_RECORD_TABLE)?;
        Ok(records.get(record_key.as_str())?.map(|v| v.value().to_owned()))
    }

    /// `delete` a record.
    pub fn delete(&self, db: &str, store: &str, key: &str) -> Result<(), StorageError> {
        let record_key = format!("{db}\u{0}{store}\u{0}{key}");
        let txn = self.db.begin_write()?;
        {
            let mut records = txn.open_table(IDB_RECORD_TABLE)?;
            records.remove(record_key.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// All records in a store as `(key, value)` pairs.
    pub fn get_all(&self, db: &str, store: &str) -> Result<Vec<(String, String)>, StorageError> {
        let prefix = format!("{db}\u{0}{store}\u{0}");
        let txn = self.db.begin_read()?;
        let records = txn.open_table(IDB_RECORD_TABLE)?;
        let mut out = Vec::new();
        for entry in records
            .range::<&str>(prefix.as_str()..)
            .map_err(|e| StorageError::Backend(e.to_string()))?
        {
            let Ok((k, v)) = entry else { continue };
            let key = k.value();
            if !key.starts_with(&prefix) {
                break;
            }
            let local = key.strip_prefix(&prefix).unwrap_or("").to_owned();
            out.push((local, v.value().to_owned()));
        }
        Ok(out)
    }

    /// Counts records in a store.
    pub fn count(&self, db: &str, store: &str) -> Result<usize, StorageError> {
        Ok(self.get_all(db, store)?.len())
    }
}

#[cfg(test)]
mod tests {
    use crate::Storage;

    #[test]
    fn idb_roundtrip() {
        let store = Storage::open(format!("/tmp/rowser-idb-{}-{}.redb", std::process::id(), line!())).unwrap();
        let idb = store.indexed_db();
        idb.open_database("testdb").unwrap();
        idb.create_store("testdb", "people").unwrap();
        idb.put("testdb", "people", "1", r#"{"name":"Ada"}"#).unwrap();
        idb.put("testdb", "people", "2", r#"{"name":"Alan"}"#).unwrap();
        assert_eq!(
            idb.get("testdb", "people", "1").unwrap().as_deref(),
            Some(r#"{"name":"Ada"}"#)
        );
        assert_eq!(idb.count("testdb", "people").unwrap(), 2);
        idb.delete("testdb", "people", "1").unwrap();
        assert_eq!(idb.count("testdb", "people").unwrap(), 1);
        idb.delete_database("testdb").unwrap();
        assert!(idb.database_names().unwrap().is_empty());
    }
}
