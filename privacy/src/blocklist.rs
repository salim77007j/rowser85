//! Request blocking through Brave's `adblock` engine.
//!
//! The engine runs at the network layer — before any connection is opened —
//! so blocked requests cost zero bytes and zero latency. The built-in
//! curated ruleset (data/tracker-rules.txt) covers the major tracker
//! networks; users can add full EasyList/EasyPrivacy-compatible lists.

use adblock::engine::Engine;
use adblock::lists::ParseOptions;
use adblock::request::Request as AdblockRequest;

use crate::RequestVerdict;

/// The bundled ruleset (compiled into the binary).
pub const BUILTIN_RULES: &str = include_str!("../../data/tracker-rules.txt");

/// Request type hints for filter matching (EasyList conventions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    /// Main frame navigation.
    Document,
    /// Sub-frame.
    SubDocument,
    /// Stylesheet.
    Stylesheet,
    /// Script.
    Script,
    /// Image.
    Image,
    /// Font.
    Font,
    /// XHR/fetch.
    Xhr,
    /// WebSocket.
    WebSocket,
    /// Media.
    Media,
    /// Other.
    Other,
}

impl ResourceType {
    fn as_str(self) -> &'static str {
        match self {
            ResourceType::Document => "document",
            ResourceType::SubDocument => "subdocument",
            ResourceType::Stylesheet => "stylesheet",
            ResourceType::Script => "script",
            ResourceType::Image => "image",
            ResourceType::Font => "font",
            ResourceType::Xhr => "xhr",
            ResourceType::WebSocket => "websocket",
            ResourceType::Media => "media",
            ResourceType::Other => "other",
        }
    }
}

/// The network filter engine.
pub struct Blocklist {
    engine: Engine,
    enabled: bool,
    blocked_count: u64,
}

impl Blocklist {
    /// Builds the engine from the built-in ruleset.
    pub fn with_builtin_rules() -> Self {
        let mut set = adblock::lists::FilterSet::new(false);
        set.add_filter_list(BUILTIN_RULES.to_owned(), ParseOptions::default());
        Blocklist {
            engine: Engine::new_with_filter_set(set),
            enabled: true,
            blocked_count: 0,
        }
    }

    /// Builds the engine with extra filter lists (EasyList etc.).
    pub fn with_extra_rules(rules: &[String]) -> Self {
        let mut set = adblock::lists::FilterSet::new(false);
        set.add_filter_list(BUILTIN_RULES.to_owned(), ParseOptions::default());
        for list in rules {
            set.add_filter_list(list.clone(), ParseOptions::default());
        }
        Blocklist {
            engine: Engine::new_with_filter_set(set),
            enabled: true,
            blocked_count: 0,
        }
    }

    /// Toggles blocking without dropping loaded rules.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// True when blocking is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Checks one request. `source_url` is the initiating document URL.
    pub fn check(
        &mut self,
        url: &str,
        source_url: &str,
        resource_type: ResourceType,
    ) -> RequestVerdict {
        if !self.enabled {
            return RequestVerdict::Allow;
        }
        // Main-frame documents are never blocked by network rules (the
        // engine surfaces interstitials instead).
        if resource_type == ResourceType::Document {
            return RequestVerdict::Allow;
        }
        let Ok(request) = AdblockRequest::new(url, source_url, resource_type.as_str(), "GET")
        else {
            return RequestVerdict::Allow;
        };
        let result = self.engine.check_network_request(&request);
        if result.should_block() {
            self.blocked_count += 1;
            return RequestVerdict::Block("tracker".to_owned());
        }
        RequestVerdict::Allow
    }

    /// Number of requests blocked since engine start.
    pub fn blocked_count(&self) -> u64 {
        self.blocked_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_known_trackers() {
        let mut list = Blocklist::with_builtin_rules();
        let v = list.check(
            "https://www.google-analytics.com/analytics.js",
            "https://news.example/",
            ResourceType::Script,
        );
        assert!(matches!(v, RequestVerdict::Block(_)));
        assert_eq!(list.blocked_count(), 1);
    }

    #[test]
    fn allows_first_party() {
        let mut list = Blocklist::with_builtin_rules();
        let v = list.check(
            "https://news.example/styles.css",
            "https://news.example/",
            ResourceType::Stylesheet,
        );
        assert_eq!(v, RequestVerdict::Allow);
    }

    #[test]
    fn documents_never_blocked() {
        let mut list = Blocklist::with_builtin_rules();
        let v = list.check(
            "https://doubleclick.net/",
            "https://news.example/",
            ResourceType::Document,
        );
        assert_eq!(v, RequestVerdict::Allow);
    }

    #[test]
    fn external_list_rules() {
        let mut list = Blocklist::with_extra_rules(&["||blocked-by-user.example^".to_owned()]);
        let v = list.check(
            "https://blocked-by-user.example/pixel",
            "https://news.example/",
            ResourceType::Image,
        );
        assert!(matches!(v, RequestVerdict::Block(_)));
    }
}
