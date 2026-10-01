//! Omnibox intelligence: URL detection, search fallback, history and
//! bookmark suggestions, ranked.

use crate::store::{Bookmarks, History, SearchEngine};

/// What a suggestion represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    /// A search-engine query.
    Search,
    /// A direct URL navigation.
    Url,
    /// A history hit.
    History,
    /// A bookmark hit.
    Bookmark,
    /// A top-site tile.
    TopSite,
}

impl SuggestionKind {
    /// Short label shown in the dropdown.
    pub fn label(self) -> &'static str {
        match self {
            SuggestionKind::Search => "Search",
            SuggestionKind::Url => "Site",
            SuggestionKind::History => "History",
            SuggestionKind::Bookmark => "Bookmark",
            SuggestionKind::TopSite => "Top site",
        }
    }
}

/// One dropdown row.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// Kind (for the badge).
    pub kind: SuggestionKind,
    /// Primary line.
    pub title: String,
    /// Where it navigates.
    pub url: String,
}

/// Decides whether `input` is a URL, and normalizes it.
pub fn normalize_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(' ') || !trimmed.contains('.') {
        return None;
    }
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("file://")
        || trimmed.starts_with("data:")
        || trimmed.starts_with("rowser:")
    {
        return Some(trimmed.to_owned());
    }
    if trimmed.starts_with("localhost") || trimmed.parse::<std::net::IpAddr>().is_ok() {
        return Some(format!("http://{trimmed}"));
    }
    // Looks like a domain: no spaces, at least one dot, valid-ish host chars.
    let host_part = trimmed.split('/').next().unwrap_or(trimmed);
    if host_part.contains('.')
        && host_part
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | ':' | '_' | '~'))
    {
        return Some(format!("https://{trimmed}"));
    }
    None
}

/// Resolves omnibox input into a navigation URL.
pub fn resolve_input(input: &str, engine: &SearchEngine) -> String {
    match normalize_url(input) {
        Some(url) => url,
        None => engine.query_url(input.trim()),
    }
}

/// Builds the suggestion list for the omnibox.
pub fn suggest(
    input: &str,
    engine: &SearchEngine,
    history: &History,
    bookmarks: &Bookmarks,
    top: &[(String, String)],
    limit: usize,
) -> Vec<Suggestion> {
    let trimmed = input.trim();
    let mut out = Vec::new();

    // 1. The primary action: URL or search.
    match normalize_url(trimmed) {
        Some(url) => out.push(Suggestion {
            kind: SuggestionKind::Url,
            title: trimmed.to_owned(),
            url,
        }),
        None if !trimmed.is_empty() => out.push(Suggestion {
            kind: SuggestionKind::Search,
            title: format!("{trimmed} — {}", engine.name),
            url: engine.query_url(trimmed),
        }),
        None => {}
    }

    // 2. Bookmarks (high trust, early).
    for b in bookmarks.search(trimmed, limit) {
        out.push(Suggestion {
            kind: SuggestionKind::Bookmark,
            title: b.display_title().to_owned(),
            url: b.url.clone(),
        });
    }

    // 3. History.
    for e in history.search(trimmed, limit * 2) {
        // Dedupe against what we already suggested.
        if out.iter().any(|s| s.url == e.url) {
            continue;
        }
        out.push(Suggestion {
            kind: SuggestionKind::History,
            title: if e.title.is_empty() { e.url.clone() } else { e.title.clone() },
            url: e.url.clone(),
        });
    }

    // 4. Top sites (only for empty input, like Chrome's drop-down).
    if trimmed.is_empty() {
        for (label, url) in top.iter().take(limit) {
            out.push(Suggestion {
                kind: SuggestionKind::TopSite,
                title: label.clone(),
                url: url.clone(),
            });
        }
    }

    out.truncate(limit.max(1));
    out
}
