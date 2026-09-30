//! The embedded Public Suffix List.
//!
//! Snapshot of <https://publicsuffix.org/list/public_suffix_list.dat>,
//! committed under `data/`. The engine computes registrable domains ("eTLD+1")
//! for cookie partitioning and third-party checks. A runtime update path is
//! provided via [`Psl::update_from`].

use std::collections::HashMap;

/// The PSL snapshot compiled into the binary.
pub const PSL_DATA: &str = include_str!("../../data/public_suffix_list.dat");

/// A public suffix list with fast lookup.
#[derive(Debug, Default)]
pub struct Psl {
    /// Exact rules: suffix → is_exception (`!` rules).
    exact: HashMap<String, bool>,
    /// Wildcard rules: parent label → true (`*.foo` matches `bar.foo`).
    wildcard: HashMap<String, ()>,
}

impl Psl {
    /// Builds the list from the embedded snapshot.
    pub fn new() -> Self {
        Psl::update_from(PSL_DATA)
    }

    /// Builds the list from raw PSL text (for runtime updates).
    pub fn update_from(data: &str) -> Self {
        let mut psl = Psl::default();
        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let rule = line.split_whitespace().next().unwrap_or("");
            if rule.is_empty() {
                continue;
            }
            if let Some(exception) = rule.strip_prefix('!') {
                psl.exact.insert(exception.to_ascii_lowercase(), true);
            } else if let Some(parent) = rule.strip_prefix("*.") {
                psl.wildcard.insert(parent.to_ascii_lowercase(), ());
                // The wildcard rule itself is also a public suffix.
                psl.exact.insert(parent.to_ascii_lowercase(), false);
            } else {
                psl.exact.insert(rule.to_ascii_lowercase(), false);
            }
        }
        psl
    }

    /// True when `domain` is itself a public suffix (e.g. `co.uk`).
    pub fn is_public_suffix(&self, domain: &str) -> bool {
        let domain = domain.trim_matches('.').to_ascii_lowercase();
        if domain.is_empty() {
            return false;
        }
        if let Some(&is_exception) = self.exact.get(&domain) {
            return !is_exception;
        }
        // Wildcard: any single label under a wildcard parent is a suffix.
        if let Some((parent, _)) = domain.rsplit_once('.') {
            if self.wildcard.contains_key(parent) {
                return true;
            }
        }
        false
    }

    /// The registrable domain (eTLD+1) of `host`, or `None` for
    /// public suffixes / IPs.
    pub fn registrable_domain(&self, host: &str) -> Option<String> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        if host.parse::<std::net::IpAddr>().is_ok() {
            return Some(host.to_owned());
        }
        if self.is_public_suffix(host) {
            return None;
        }
        // Walk up: find the shortest suffix that is public; the registrable
        // domain is the label one above it.
        let labels: Vec<&str> = host.split('.').collect();
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels[i..].join(".");
            let parent = labels[i + 1..].join(".");
            if self.is_public_suffix(&parent) {
                return Some(candidate);
            }
        }
        // No public suffix found: treat the last two labels as registrable.
        if labels.len() >= 2 {
            Some(labels[labels.len() - 2..].join("."))
        } else {
            Some(host.to_owned())
        }
    }

    /// True when two hosts share a registrable domain ("same-site").
    pub fn same_site(&self, a: &str, b: &str) -> bool {
        let ra = self.registrable_domain(a);
        let rb = self.registrable_domain(b);
        match (ra, rb) {
            (Some(a), Some(b)) => a == b,
            _ => a == b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrable_domains() {
        let psl = Psl::new();
        assert_eq!(
            psl.registrable_domain("www.example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(psl.registrable_domain("example.co.uk").as_deref(), Some("example.co.uk"));
        assert_eq!(psl.registrable_domain("deep.a.example.co.uk").as_deref(), Some("example.co.uk"));
        assert_eq!(psl.registrable_domain("co.uk"), None);
        assert!(psl.same_site("a.example.com", "b.example.com"));
        assert!(!psl.same_site("example.com", "example.org"));
        assert_eq!(psl.registrable_domain("192.168.1.4").as_deref(), Some("192.168.1.4"));
    }
}
