//! CNAME-cloaking protection.
//!
//! Trackers disguise themselves as first-party subdomains
//! (`metrics.example.com` → `tracker.cdn.com`) via DNS CNAME records.
//! The engine re-evaluates the *resolved* hostname against the blocklist
//! after DNS resolution, before the TLS connection is opened.

use adblock::engine::Engine;
use adblock::lists::ParseOptions;
use adblock::request::Request as AdblockRequest;

/// CNAME-cloaking detector: maps resolved hosts against tracker rules.
pub struct CnameGuard {
    engine: Engine,
}

/// Known CNAME-cloaking target domains (from published research on
/// CNAME-cloaked trackers; extended by the main ruleset).
const CNAME_TRACKERS: &str = "\
||analyticssvc.com^
||metrics.icloud.com^
||cdn.measure.sh^
||impactcdn.com^
||firstpartyphoenix.com^
||exp.shutterstock.com^
";

impl CnameGuard {
    /// Builds the guard.
    pub fn new() -> Self {
        let mut set = adblock::lists::FilterSet::new(false);
        set.add_filter_list(CNAME_TRACKERS.to_owned(), ParseOptions::default());
        CnameGuard {
            engine: Engine::new_with_filter_set(set),
        }
    }

    /// Checks whether the DNS-resolved target of a first-party hostname is a
    /// known tracker. `source_url` is the initiating document.
    pub fn is_cloaked(
        &mut self,
        original_host: &str,
        resolved_host: &str,
        source_url: &str,
    ) -> bool {
        // If the resolved host is the original, there is no CNAME.
        if original_host.eq_ignore_ascii_case(resolved_host) {
            return false;
        }
        let url = format!("https://{resolved_host}/");
        let Ok(request) = AdblockRequest::new(&url, source_url, "xhr", "GET") else {
            return false;
        };
        self.engine.check_network_request(&request).should_block()
    }
}

impl Default for CnameGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cloaking() {
        let mut guard = CnameGuard::new();
        assert!(guard.is_cloaked(
            "metrics.example.com",
            "tracker.analyticssvc.com",
            "https://example.com/"
        ));
        assert!(!guard.is_cloaked("cdn.example.com", "cdn.example.com", "https://example.com/"));
        assert!(!guard.is_cloaked(
            "metrics.example.com",
            "static.fastly.net",
            "https://example.com/"
        ));
    }
}
