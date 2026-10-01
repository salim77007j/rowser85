//! The cookie jar with CHIPS partitioning.
//!
//! CHIPS (Cookies Having Independent Partitioned State) stores third-party
//! cookies keyed by the **top-level site** partition, so embedded content on
//! `a.example` and `b.example` never share trackers. Unpartitioned
//! third-party cookies are rejected under the default policy (matching
//! 2026 browser norms).

use cookie::{Cookie, SameSite};
use jiff::Timestamp;
use redb::{Database, ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{StorageError, COOKIE_TABLE};

/// Third-party cookie policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThirdPartyPolicy {
    /// Block all third-party cookies (partitioned or not).
    BlockAll,
    /// Allow partitioned (CHIPS) third-party cookies only.
    #[default]
    Partitioned,
    /// Allow all third-party cookies (not recommended).
    AllowAll,
}

/// One stored cookie.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CookieRecord {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Host key the cookie belongs to (host or domain).
    pub domain: String,
    /// True when the `Domain` attribute was absent (host-only match).
    pub host_only: bool,
    /// Path scope.
    pub path: String,
    /// Expiry (unix seconds); `None` marks a session cookie.
    pub expires_at: Option<i64>,
    /// Secure-only.
    pub secure: bool,
    /// HttpOnly.
    pub http_only: bool,
    /// SameSite mode.
    pub same_site: SameSiteMode,
    /// CHIPS partition (top-level registrable domain), when set.
    pub partition: Option<String>,
}

/// SameSite values we persist.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SameSiteMode {
    /// `Strict`.
    Strict,
    /// `Lax` (the default).
    #[default]
    Lax,
    /// `None` (requires Secure).
    None,
}

impl From<SameSite> for SameSiteMode {
    fn from(mode: SameSite) -> Self {
        match mode {
            SameSite::Strict => SameSiteMode::Strict,
            SameSite::Lax => SameSiteMode::Lax,
            SameSite::None => SameSiteMode::None,
        }
    }
}

/// The cookie jar. All operations are single ACID redb transactions.
#[derive(Debug)]
pub struct CookieJar<'a> {
    db: &'a Database,
}

/// The outcome of setting a cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetCookieOutcome {
    /// The cookie was stored (created or replaced).
    Stored,
    /// The cookie was rejected by policy.
    Rejected(&'static str),
    /// The cookie expired and was removed.
    Expired,
}

impl<'a> CookieJar<'a> {
    /// Wraps the profile database.
    pub fn new(db: &'a Database) -> Self {
        CookieJar { db }
    }

    /// Stores a `Set-Cookie` header value for `request_url`.
    ///
    /// `top_site` is the top-level site (registrable domain) of the tab, used
    /// as the CHIPS partition key for third-party cookies.
    pub fn set_cookie(
        &self,
        raw: &str,
        request_url: &Url,
        top_site: Option<&str>,
        policy: ThirdPartyPolicy,
    ) -> Result<SetCookieOutcome, StorageError> {
        let raw = raw.trim();
        let partitioned_attr = has_partitioned_attribute(raw);
        // `cookie` takes the name=value part; attributes are read back below.
        let Ok(parsed) = Cookie::parse(raw.to_owned()) else {
            return Ok(SetCookieOutcome::Rejected("malformed"));
        };

        let host = request_url
            .host_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if host.is_empty() {
            return Ok(SetCookieOutcome::Rejected("no host"));
        }

        // Domain scoping.
        let (domain, host_only) = match parsed.domain() {
            Some(d) if !d.is_empty() => {
                let d = d.trim_start_matches('.').to_ascii_lowercase();
                if host == d || host.ends_with(&format!(".{d}")) {
                    (d, false)
                } else {
                    return Ok(SetCookieOutcome::Rejected("domain mismatch"));
                }
            }
            _ => (host.clone(), true),
        };

        // Secure enforcement.
        let secure = parsed.secure().unwrap_or(false);
        let is_https = request_url.scheme() == "https" || request_url.scheme() == "wss";
        if secure && !is_https {
            return Ok(SetCookieOutcome::Rejected("secure on insecure"));
        }

        // Third-party determination + CHIPS.
        let request_site = registrable_domain(&host);
        let top = top_site
            .map(str::to_owned)
            .unwrap_or_else(|| request_site.clone());
        let third_party = request_site != top;
        let partition = if partitioned_attr && third_party {
            Some(top.clone())
        } else if partitioned_attr {
            Some(request_site.clone())
        } else {
            None
        };

        if third_party {
            match policy {
                ThirdPartyPolicy::BlockAll => {
                    return Ok(SetCookieOutcome::Rejected("third-party blocked"));
                }
                ThirdPartyPolicy::Partitioned if partition.is_none() => {
                    return Ok(SetCookieOutcome::Rejected("third-party unpartitioned"));
                }
                ThirdPartyPolicy::Partitioned => {}
                ThirdPartyPolicy::AllowAll => {}
            }
        }

        // SameSite=None requires Secure.
        let same_site = parsed
            .same_site()
            .map(SameSiteMode::from)
            .unwrap_or_default();
        if same_site == SameSiteMode::None && !secure {
            return Ok(SetCookieOutcome::Rejected("SameSite=None without Secure"));
        }

        // Expiry: Max-Age wins, then Expires, else session cookie.
        let expires_at = if let Some(max_age) = parsed.max_age() {
            Some(Timestamp::now().as_second() + max_age.whole_seconds().max(0))
        } else {
            parsed.expires_datetime().map(|e| e.unix_timestamp())
        };
        if let Some(expiry) = expires_at {
            if expiry <= Timestamp::now().as_second() {
                self.remove(&domain, parsed.name(), partition.as_deref())?;
                return Ok(SetCookieOutcome::Expired);
            }
        }

        let record = CookieRecord {
            name: parsed.name().to_owned(),
            value: parsed.value().to_owned(),
            domain,
            host_only,
            path: parsed.path().unwrap_or("/").to_owned(),
            expires_at,
            secure,
            http_only: parsed.http_only().unwrap_or(false),
            same_site,
            partition,
        };
        let key = cookie_key(&record.domain, &record.name, record.partition.as_deref());
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COOKIE_TABLE)?;
            let raw = serde_json::to_string(&record).unwrap_or_default();
            table.insert(key.as_str(), raw.as_str())?;
        }
        txn.commit()?;
        Ok(SetCookieOutcome::Stored)
    }

    /// Builds the `Cookie` request header value for `url`.
    ///
    /// Partitioned cookies only match when the current top site equals the
    /// stored partition.
    pub fn cookie_header(
        &self,
        url: &Url,
        top_site: Option<&str>,
        policy: ThirdPartyPolicy,
    ) -> Result<Option<String>, StorageError> {
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if host.is_empty() {
            return Ok(None);
        }
        let request_site = registrable_domain(&host);
        let top = top_site
            .map(str::to_owned)
            .unwrap_or_else(|| request_site.clone());
        let third_party = request_site != top;
        let path = url.path();

        let now = Timestamp::now().as_second();
        let mut parts: Vec<String> = Vec::new();
        let mut expired: Vec<String> = Vec::new();

        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COOKIE_TABLE)?;
            let range = table
                .range::<&str>(..)
                .map_err(|e| StorageError::Backend(e.to_string()))?;
            for entry in range {
                let (key, value) = entry.map_err(|e| StorageError::Backend(e.to_string()))?;
                let Ok(record) = serde_json::from_str::<CookieRecord>(value.value()) else {
                    continue;
                };
                // Domain match.
                let domain_ok = if record.host_only {
                    host == record.domain
                } else {
                    host == record.domain || host.ends_with(&format!(".{}", record.domain))
                };
                if !domain_ok {
                    continue;
                }
                // Path match.
                if !path_matches(path, &record.path) {
                    continue;
                }
                // Partition match.
                if record.partition.is_some() {
                    if record.partition.as_deref() != Some(top.as_str()) {
                        continue;
                    }
                } else if third_party {
                    match policy {
                        ThirdPartyPolicy::BlockAll | ThirdPartyPolicy::Partitioned => continue,
                        ThirdPartyPolicy::AllowAll => {}
                    }
                }
                // SameSite (approximate: cross-site subresource requests only
                // carry SameSite=None; the engine classifies request context).
                if record.expires_at.map(|e| e <= now).unwrap_or(false) {
                    expired.push(key.value().to_owned());
                    continue;
                }
                parts.push(format!("{}={}", record.name, record.value));
            }
            for key in expired {
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        if parts.is_empty() {
            Ok(None)
        } else {
            Ok(Some(parts.join("; ")))
        }
    }

    /// Removes one cookie.
    pub fn remove(
        &self,
        domain: &str,
        name: &str,
        partition: Option<&str>,
    ) -> Result<(), StorageError> {
        let key = cookie_key(domain, name, partition);
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COOKIE_TABLE)?;
            table.remove(key.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// All cookies for a host (devtools / tests).
    pub fn cookies_for_host(&self, host: &str) -> Result<Vec<CookieRecord>, StorageError> {
        let host = host.to_ascii_lowercase();
        let txn = self.db.begin_read()?;
        let table = txn.open_table(COOKIE_TABLE)?;
        let mut out = Vec::new();
        for entry in table
            .range::<&str>(..)
            .map_err(|e| StorageError::Backend(e.to_string()))?
        {
            let (_, value) = entry.map_err(|e| StorageError::Backend(e.to_string()))?;
            if let Ok(record) = serde_json::from_str::<CookieRecord>(value.value()) {
                if record.domain == host {
                    out.push(record);
                }
            }
        }
        Ok(out)
    }

    /// Number of stored cookies.
    pub fn len(&self) -> Result<usize, StorageError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(COOKIE_TABLE)?;
        Ok(table.len()? as usize)
    }

    /// True when the jar is empty.
    pub fn is_empty(&self) -> Result<bool, StorageError> {
        Ok(self.len()? == 0)
    }

    /// Clears all cookies.
    pub fn clear(&self) -> Result<(), StorageError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COOKIE_TABLE)?;
            let keys: Vec<String> = table
                .range::<&str>(..)
                .map_err(|e| StorageError::Backend(e.to_string()))?
                .filter_map(|e| e.ok().map(|(k, _)| k.value().to_owned()))
                .collect();
            for key in keys {
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }
}

fn cookie_key(domain: &str, name: &str, partition: Option<&str>) -> String {
    format!("{domain}\u{0}{name}\u{0}{}", partition.unwrap_or("-"))
}

fn has_partitioned_attribute(raw: &str) -> bool {
    raw.split(';')
        .skip(1)
        .map(str::trim)
        .any(|attr| attr.eq_ignore_ascii_case("partitioned"))
}

/// Registrable domain approximation: last two labels.
///
/// The engine upgrades this with the embedded full Public Suffix List
/// (see `privacy::psl`); the storage layer keeps a fallback so it can be
/// used in isolation.
pub fn registrable_domain(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() <= 2 {
        return host.to_owned();
    }
    // Common multi-part public suffixes.
    const MULTI: [&str; 6] = ["co.uk", "org.uk", "com.au", "co.jp", "com.br", "co.nz"];
    let last_two = parts[parts.len() - 2..].join(".");
    if MULTI.contains(&last_two.as_str()) {
        parts[parts.len() - 3..].join(".")
    } else {
        last_two
    }
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    request_path
        .strip_prefix(cookie_path)
        .is_some_and(|rest| cookie_path.ends_with('/') || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    fn storage() -> Storage {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Storage::open(format!(
            "/tmp/rowser-cookie-{}-{id}.redb",
            std::process::id()
        ))
        .unwrap()
    }

    #[test]
    fn set_and_get_first_party() {
        let store = storage();
        let jar = store.cookies();
        jar.clear().unwrap();
        let url = Url::parse("https://example.com/page").unwrap();
        let out = jar
            .set_cookie(
                "session=abc; Path=/",
                &url,
                None,
                ThirdPartyPolicy::default(),
            )
            .unwrap();
        assert_eq!(out, SetCookieOutcome::Stored);
        let header = jar
            .cookie_header(&url, None, ThirdPartyPolicy::default())
            .unwrap();
        assert_eq!(header.as_deref(), Some("session=abc"));
    }

    #[test]
    fn chips_partitioning() {
        let store = storage();
        let jar = store.cookies();
        jar.clear().unwrap();
        // Third-party cookie WITH Partitioned → stored under top site.
        let tracker = Url::parse("https://tracker.dev/pixel").unwrap();
        let out = jar
            .set_cookie(
                "id=123; Secure; Partitioned; SameSite=None",
                &tracker,
                Some("news.example"),
                ThirdPartyPolicy::default(),
            )
            .unwrap();
        assert_eq!(out, SetCookieOutcome::Stored);
        // Same tracker on a different top site → no cookie.
        let other = Url::parse("https://tracker.dev/pixel").unwrap();
        let header = jar
            .cookie_header(&other, Some("shop.other"), ThirdPartyPolicy::default())
            .unwrap();
        assert_eq!(header, None);
        // Matching partition → sent.
        let header = jar
            .cookie_header(&tracker, Some("news.example"), ThirdPartyPolicy::default())
            .unwrap();
        assert_eq!(header.as_deref(), Some("id=123"));
    }

    #[test]
    fn third_party_unpartitioned_blocked() {
        let store = storage();
        let jar = store.cookies();
        jar.clear().unwrap();
        let tracker = Url::parse("https://tracker.dev/pixel").unwrap();
        let out = jar
            .set_cookie(
                "x=1; Secure; SameSite=None",
                &tracker,
                Some("news.example"),
                ThirdPartyPolicy::default(),
            )
            .unwrap();
        assert_eq!(out, SetCookieOutcome::Rejected("third-party unpartitioned"));
    }

    #[test]
    fn domain_scoping_rejected() {
        let store = storage();
        let jar = store.cookies();
        jar.clear().unwrap();
        let url = Url::parse("https://evil.example/").unwrap();
        let out = jar
            .set_cookie(
                "hijack=1; Domain=bank.com",
                &url,
                None,
                ThirdPartyPolicy::default(),
            )
            .unwrap();
        assert_eq!(out, SetCookieOutcome::Rejected("domain mismatch"));
    }
}
