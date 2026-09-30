//! HTTP response cache (Cache API core) with LRU eviction by byte budget.

use redb::{Database, ReadableTable};
use serde::{Deserialize, Serialize};

use crate::{CACHE_BODY_TABLE, CACHE_META_TABLE, StorageError};

/// Cached response metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntryMeta {
    /// Response status.
    pub status: u16,
    /// Response headers (name, value) pairs.
    pub headers: Vec<(String, String)>,
    /// MIME type of the body.
    pub content_type: String,
    /// Body length in bytes.
    pub len: u64,
    /// Last use (unix seconds) for LRU.
    pub last_used: i64,
    /// Stored at (unix seconds).
    pub stored_at: i64,
}

/// One cached response.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    /// Metadata.
    pub meta: CacheEntryMeta,
    /// Body bytes.
    pub body: Vec<u8>,
}

/// The HTTP cache.
#[derive(Debug)]
pub struct HttpCache<'a> {
    db: &'a Database,
    max_bytes: u64,
}

impl<'a> HttpCache<'a> {
    /// Wraps the profile database with an LRU byte budget.
    pub fn new(db: &'a Database, max_bytes: u64) -> Self {
        HttpCache { db, max_bytes }
    }

    /// Stores a response under `url`.
    pub fn put(
        &self,
        url: &str,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<(), StorageError> {
        let meta = CacheEntryMeta {
            status,
            headers,
            content_type: String::new(),
            len: body.len() as u64,
            last_used: jiff::Timestamp::now().as_second(),
            stored_at: jiff::Timestamp::now().as_second(),
        };
        let txn = self.db.begin_write()?;
        {
            let mut meta_table = txn.open_table(CACHE_META_TABLE)?;
            let mut body_table = txn.open_table(CACHE_BODY_TABLE)?;
            let meta_raw = serde_json::to_string(&meta).unwrap_or_default();
            meta_table.insert(url, meta_raw.as_str())?;
            body_table.insert(url, body.as_slice())?;
        }
        txn.commit()?;
        self.evict_to_budget()
    }

    /// Fetches a cached response, refreshing its LRU timestamp.
    pub fn get(&self, url: &str) -> Result<Option<CachedResponse>, StorageError> {
        let txn = self.db.begin_write()?;
        let mut result = None;
        {
            let mut meta_table = txn.open_table(CACHE_META_TABLE)?;
            let body_table = txn.open_table(CACHE_BODY_TABLE)?;
            let owned = meta_table.get(url)?.map(|raw| raw.value().to_owned());
            if let Some(owned) = owned {
                if let Ok(mut meta) = serde_json::from_str::<CacheEntryMeta>(&owned) {
                    let body = body_table
                        .get(url)
                        .ok()
                        .flatten()
                        .map(|b| b.value().to_vec())
                        .unwrap_or_default();
                    meta.last_used = jiff::Timestamp::now().as_second();
                    let serialized = serde_json::to_string(&meta).unwrap_or_default();
                    meta_table.insert(url, serialized.as_str())?;
                    result = Some(CachedResponse { meta, body });
                }
            }
        }
        txn.commit()?;
        Ok(result)
    }

    /// Removes one entry.
    pub fn delete(&self, url: &str) -> Result<(), StorageError> {
        let txn = self.db.begin_write()?;
        {
            let mut meta_table = txn.open_table(CACHE_META_TABLE)?;
            let mut body_table = txn.open_table(CACHE_BODY_TABLE)?;
            meta_table.remove(url)?;
            body_table.remove(url)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Total cached bytes.
    pub fn usage(&self) -> Result<u64, StorageError> {
        let txn = self.db.begin_read()?;
        let meta_table = txn.open_table(CACHE_META_TABLE)?;
        let mut total = 0u64;
        for entry in meta_table
            .iter()
            .map_err(|e| StorageError::Backend(e.to_string()))?
        {
            if let Ok((_, raw)) = entry {
                if let Ok(meta) = serde_json::from_str::<CacheEntryMeta>(raw.value()) {
                    total += meta.len;
                }
            }
        }
        Ok(total)
    }

    /// Evicts least-recently-used entries until under budget.
    fn evict_to_budget(&self) -> Result<(), StorageError> {
        let mut entries: Vec<(String, CacheEntryMeta)> = Vec::new();
        {
            let txn = self.db.begin_read()?;
            let meta_table = txn.open_table(CACHE_META_TABLE)?;
            for entry in meta_table
                .iter()
                .map_err(|e| StorageError::Backend(e.to_string()))?
                .flatten()
            {
                let (url, raw) = (entry.0, entry.1);
                if let Ok(meta) = serde_json::from_str::<CacheEntryMeta>(raw.value()) {
                    entries.push((url.value().to_owned(), meta));
                }
            }
        }
        let mut total: u64 = entries.iter().map(|(_, m)| m.len).sum();
        if total <= self.max_bytes {
            return Ok(());
        }
        entries.sort_by_key(|(_, m)| m.last_used);
        let txn = self.db.begin_write()?;
        {
            let mut meta_table = txn.open_table(CACHE_META_TABLE)?;
            let mut body_table = txn.open_table(CACHE_BODY_TABLE)?;
            for (url, meta) in entries {
                if total <= self.max_bytes {
                    break;
                }
                meta_table.remove(url.as_str())?;
                body_table.remove(url.as_str())?;
                total = total.saturating_sub(meta.len);
            }
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::Storage;

    #[test]
    fn cache_roundtrip_and_eviction() {
        let store = Storage::open(format!("/tmp/rowser-cache-{}-{}.redb", std::process::id(), line!())).unwrap();
        let cache = store.http_cache(1000);
        cache.delete("https://example.com/a").unwrap();
        cache.put("https://example.com/a", 200, vec![], vec![0u8; 400]).unwrap();
        cache.put("https://example.com/b", 200, vec![], vec![0u8; 400]).unwrap();
        cache.put("https://example.com/c", 200, vec![], vec![0u8; 400]).unwrap();
        // Budget 1000 → oldest evicted.
        assert!(cache.get("https://example.com/a").unwrap().is_none());
        assert!(cache.get("https://example.com/c").unwrap().is_some());
        assert!(cache.usage().unwrap() <= 1000);
    }
}
