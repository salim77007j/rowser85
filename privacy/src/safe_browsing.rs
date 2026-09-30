//! Privacy-preserving safe browsing.
//!
//! Threat model: the engine must protect users from known phishing/malware
//! URLs **without sending browsing history to any server**. We use the
//! standard hash-prefix approach in local-only mode:
//!
//! * A local database stores SHA-256 prefixes of canonicalized bad URLs.
//! * Navigation checks hash the URL locally; no URL or hash ever leaves the
//!   device in v1 (a future update protocol can fetch full hash-prefix
//!   sets, which still reveals nothing about visited URLs — the same
//!   property Google Safe Browsing v5 provides).
//! * The list ships empty and is populated via [`SafeBrowsing::add_prefix`]
//!   / [`SafeBrowsing::update_from`].

use std::collections::HashSet;

use sha2::{Digest, Sha256};

/// Local safe-browsing store.
#[derive(Debug, Default)]
pub struct SafeBrowsing {
    prefixes: HashSet<[u8; 4]>,
    enabled: bool,
}

/// Verdict for a navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeBrowsingVerdict {
    /// No match; proceed.
    Ok,
    /// The URL matches a known-bad prefix; the UI should show an
    /// interstitial. The engine still never sends the URL anywhere.
    Match,
}

impl SafeBrowsing {
    /// Creates an empty store.
    pub fn new() -> Self {
        SafeBrowsing {
            prefixes: HashSet::new(),
            enabled: true,
        }
    }

    /// Enables or disables checks.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Adds a 4-byte SHA-256 prefix.
    pub fn add_prefix(&mut self, prefix: [u8; 4]) {
        self.prefixes.insert(prefix);
    }

    /// Adds bad URLs (full URLs; their 4-byte prefixes are stored).
    pub fn update_from_urls(&mut self, bad_urls: &[String]) {
        for url in bad_urls {
            let prefix = url_prefix(url);
            self.prefixes.insert(prefix);
        }
    }

    /// Number of stored prefixes.
    pub fn len(&self) -> usize {
        self.prefixes.len()
    }

    /// True when no prefixes are loaded.
    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// Checks a navigation URL.
    pub fn check(&self, url: &str) -> SafeBrowsingVerdict {
        if !self.enabled {
            return SafeBrowsingVerdict::Ok;
        }
        let prefix = url_prefix(url);
        if self.prefixes.contains(&prefix) {
            SafeBrowsingVerdict::Match
        } else {
            SafeBrowsingVerdict::Ok
        }
    }
}

/// 4-byte SHA-256 prefix of a lightly-canonicalized URL.
fn url_prefix(url: &str) -> [u8; 4] {
    let canonical = url.trim().to_ascii_lowercase();
    let hash = Sha256::digest(canonical.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matching() {
        let mut sb = SafeBrowsing::new();
        sb.update_from_urls(&["https://phishing.example/login".to_owned()]);
        assert_eq!(sb.check("https://phishing.example/login"), SafeBrowsingVerdict::Match);
        assert_eq!(sb.check("https://good.example/"), SafeBrowsingVerdict::Ok);
        // Case-insensitive canonicalization.
        assert_eq!(
            sb.check("https://PHISHING.example/login"),
            SafeBrowsingVerdict::Match
        );
        assert_eq!(sb.len(), 1);
    }
}
