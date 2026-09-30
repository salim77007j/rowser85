//! localStorage: origin-keyed string storage.

use redb::{Database, ReadableTable};
use url::Url;

use crate::{LS_TABLE, StorageError};

/// localStorage access for one profile.
#[derive(Debug)]
pub struct LocalStorage<'a> {
    db: &'a Database,
}

impl<'a> LocalStorage<'a> {
    /// Wraps the profile database.
    pub fn new(db: &'a Database) -> Self {
        LocalStorage { db }
    }

    fn origin(url: &Url) -> String {
        // Origin: scheme + host + port (default port elided).
        let mut origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
        if let Some(port) = url.port() {
            origin.push_str(&format!(":{port}"));
        }
        origin
    }

    /// `localStorage.getItem`.
    pub fn get(&self, url: &Url, key: &str) -> Result<Option<String>, StorageError> {
        let storage_key = format!("{}\u{0}{}", Self::origin(url), key);
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LS_TABLE)?;
        Ok(table.get(storage_key.as_str())?.map(|v| v.value().to_owned()))
    }

    /// `localStorage.setItem`.
    pub fn set(&self, url: &Url, key: &str, value: &str) -> Result<(), StorageError> {
        let storage_key = format!("{}\u{0}{}", Self::origin(url), key);
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(LS_TABLE)?;
            table.insert(storage_key.as_str(), value)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// `localStorage.removeItem`.
    pub fn remove(&self, url: &Url, key: &str) -> Result<(), StorageError> {
        let storage_key = format!("{}\u{0}{}", Self::origin(url), key);
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(LS_TABLE)?;
            table.remove(storage_key.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// `localStorage.clear` for one origin.
    pub fn clear_origin(&self, url: &Url) -> Result<usize, StorageError> {
        let prefix = format!("{}\u{0}", Self::origin(url));
        let txn = self.db.begin_write()?;
        let mut removed = 0;
        {
            let mut table = txn.open_table(LS_TABLE)?;
            let keys: Vec<String> = table
                .range::<&str>(prefix.as_str()..)
                .map_err(|e| StorageError::Backend(e.to_string()))?
                .filter_map(|e| e.ok().map(|(k, _)| k.value().to_owned()))
                .take_while(|k| k.starts_with(&prefix))
                .collect();
            for key in keys {
                table.remove(key.as_str())?;
                removed += 1;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// All key/value pairs for one origin (devtools).
    pub fn entries_for_origin(&self, url: &Url) -> Result<Vec<(String, String)>, StorageError> {
        let prefix = format!("{}\u{0}", Self::origin(url));
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LS_TABLE)?;
        let mut out = Vec::new();
        for entry in table
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    #[test]
    fn roundtrip_and_origin_isolation() {
        let store = Storage::open(format!("/tmp/rowser-ls-{}-{}.redb", std::process::id(), line!())).unwrap();
        let ls = store.local_storage();
        let a = Url::parse("https://a.example/").unwrap();
        let b = Url::parse("https://b.example/").unwrap();
        ls.set(&a, "theme", "dark").unwrap();
        assert_eq!(ls.get(&a, "theme").unwrap().as_deref(), Some("dark"));
        assert_eq!(ls.get(&b, "theme").unwrap(), None);
        ls.remove(&a, "theme").unwrap();
        assert_eq!(ls.get(&a, "theme").unwrap(), None);
    }
}
