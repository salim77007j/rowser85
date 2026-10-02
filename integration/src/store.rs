//! Profile persistence for the browser shell: settings, bookmarks, history,
//! permissions and session state, stored as compact JSON in the profile
//! directory. The engine owns cookies/cache/localStorage (redb); the shell
//! owns browser-level user data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Serializable mirror of the engine's `PrivacySettings` (the engine type
/// is not serializable; this DTO converts losslessly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacyMirror {
    /// Network-layer ad/tracker blocking.
    pub block_ads: bool,
    /// Upgrade http:// to https://.
    pub https_upgrade: bool,
    /// Reject third-party unpartitioned cookies.
    pub block_third_party_cookies: bool,
    /// Anti-fingerprinting spoofing.
    pub anti_fingerprinting: bool,
    /// WebRTC IP leak protection.
    pub webrtc_protection: bool,
    /// Local safe-browsing checks.
    pub safe_browsing: bool,
    /// Telemetry opt-in (engine ships zero telemetry).
    pub telemetry_opt_in: bool,
}

impl Default for PrivacyMirror {
    fn default() -> Self {
        PrivacyMirror::from(rowser_privacy::PrivacySettings::default())
    }
}

impl From<rowser_privacy::PrivacySettings> for PrivacyMirror {
    fn from(p: rowser_privacy::PrivacySettings) -> Self {
        PrivacyMirror {
            block_ads: p.block_ads,
            https_upgrade: p.https_upgrade,
            block_third_party_cookies: p.block_third_party_cookies,
            anti_fingerprinting: p.anti_fingerprinting,
            webrtc_protection: p.webrtc_protection,
            safe_browsing: p.safe_browsing,
            telemetry_opt_in: p.telemetry_opt_in,
        }
    }
}

impl From<PrivacyMirror> for rowser_privacy::PrivacySettings {
    fn from(m: PrivacyMirror) -> Self {
        rowser_privacy::PrivacySettings {
            block_ads: m.block_ads,
            https_upgrade: m.https_upgrade,
            block_third_party_cookies: m.block_third_party_cookies,
            anti_fingerprinting: m.anti_fingerprinting,
            webrtc_protection: m.webrtc_protection,
            safe_browsing: m.safe_browsing,
            telemetry_opt_in: m.telemetry_opt_in,
        }
    }
}

impl PrivacyMirror {
    /// The engine-side settings.
    pub fn to_engine(self) -> rowser_privacy::PrivacySettings {
        rowser_privacy::PrivacySettings::from(self)
    }
}

/// The root of all shell-side user data.
pub struct ProfileStore {
    /// Profile directory (created if missing).
    pub dir: PathBuf,
    /// Browser settings.
    pub settings: Settings,
    /// Bookmarks.
    pub bookmarks: Bookmarks,
    /// Browsing history.
    pub history: History,
    /// Per-site permission grants.
    pub permissions: Permissions,
    /// Last session's open tabs.
    pub session: SessionData,
}

impl ProfileStore {
    /// Loads (or initializes) the profile in `dir`.
    pub fn load(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        ProfileStore {
            settings: Settings::load(&dir),
            bookmarks: Bookmarks::load(&dir),
            history: History::load(&dir),
            permissions: Permissions::load(&dir),
            session: SessionData::load(&dir),
            dir,
        }
    }

    /// Persists every collection.
    pub fn save_all(&self) {
        self.settings.save(&self.dir);
        self.bookmarks.save(&self.dir);
        self.history.save(&self.dir);
        self.permissions.save(&self.dir);
        self.session.save(&self.dir);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path, default: fn() -> T) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(default)
}

fn save_json<T: Serialize>(path: &Path, value: &T) {
    if let Ok(bytes) = serde_json::to_vec_pretty(value) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// UI theme selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeMode {
    /// Light chrome.
    #[default]
    Light,
    /// Dark chrome.
    Dark,
    /// Follow the OS preference.
    System,
}

/// A search engine the omnibox can hand queries to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchEngine {
    /// Display name.
    pub name: String,
    /// Query URL template containing `{q}`.
    pub url: String,
}

impl SearchEngine {
    /// Built-in engines offered in settings.
    pub fn builtins() -> Vec<SearchEngine> {
        vec![
            SearchEngine {
                name: "Google".into(),
                url: "https://www.google.com/search?q={q}".into(),
            },
            SearchEngine {
                name: "DuckDuckGo".into(),
                url: "https://duckduckgo.com/?q={q}".into(),
            },
            SearchEngine {
                name: "Brave Search".into(),
                url: "https://search.brave.com/search?q={q}".into(),
            },
            SearchEngine {
                name: "Bing".into(),
                url: "https://www.bing.com/search?q={q}".into(),
            },
            SearchEngine {
                name: "Startpage".into(),
                url: "https://www.startpage.com/sp/search?query={q}".into(),
            },
            SearchEngine {
                name: "Wikipedia".into(),
                url: "https://en.wikipedia.org/w/index.php?search={q}".into(),
            },
        ]
    }

    /// Builds the query URL for `query`.
    pub fn query_url(&self, query: &str) -> String {
        self.url.replace(
            "{q}",
            &query
                .replace('%', "%25")
                .replace(' ', "%20")
                .replace('+', "%2B")
                .replace('&', "%26")
                .replace('?', "%3F")
                .replace('#', "%23"),
        )
    }
}

impl Default for SearchEngine {
    fn default() -> Self {
        // Google default: a "standard browser for any user" searches where
        // users expect; privacy-minded users switch to DDG/Brave in settings.
        SearchEngine {
            name: "Google".into(),
            url: "https://www.google.com/search?q={q}".into(),
        }
    }
}

/// What the browser shows on startup.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StartupMode {
    /// The new tab page.
    #[default]
    NewTab,
    /// The configured home page.
    Homepage,
    /// Restore the previous session.
    PreviousSession,
}

/// Browser settings (everything the settings page edits).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// UI theme.
    pub theme: ThemeMode,
    /// Accent color (hex, `RRGGBB`).
    pub accent: String,
    /// Active search engine.
    pub search_engine: SearchEngine,
    /// Home page URL ("rowser://newtab" for the start page).
    pub homepage: String,
    /// Startup behavior.
    pub startup: StartupMode,
    /// Restore the session after a crash/close.
    pub restore_session: bool,
    /// Default download directory.
    pub download_dir: PathBuf,
    /// Whether to ask where to save each download.
    pub ask_download_path: bool,
    /// Mirrored engine privacy settings (applied at start and on change).
    pub privacy: PrivacyMirror,
    /// Show the bookmarks bar.
    pub show_bookmarks_bar: bool,
    /// Default zoom factor for new tabs.
    pub default_zoom: f32,
    /// Window scale hint (0 = auto).
    pub ui_scale: f32,
}

impl Default for Settings {
    fn default() -> Self {
        let download_dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Settings {
            theme: ThemeMode::Light,
            accent: "1A73E8".into(),
            search_engine: SearchEngine::default(),
            homepage: "rowser://newtab".into(),
            startup: StartupMode::NewTab,
            restore_session: true,
            download_dir,
            ask_download_path: false,
            privacy: PrivacyMirror::default(),
            show_bookmarks_bar: true,
            default_zoom: 1.0,
            ui_scale: 0.0,
        }
    }
}

impl Settings {
    fn path(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    fn load(dir: &Path) -> Settings {
        load_json(&Self::path(dir), Settings::default)
    }

    pub fn save(&self, dir: &Path) {
        save_json(&Self::path(dir), self);
    }
}

// ---------------------------------------------------------------------------
// Bookmarks
// ---------------------------------------------------------------------------

/// One bookmark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Stable identifier.
    pub id: u64,
    /// Target URL.
    pub url: String,
    /// Title.
    pub title: String,
    /// Folder name ("Bookmarks bar" root when empty).
    pub folder: String,
    /// Creation time (ms since epoch).
    pub added: u64,
}

impl Bookmark {
    /// A short title for tabs and bars.
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.url
        } else {
            &self.title
        }
    }
}

/// The bookmark collection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bookmarks {
    /// All bookmarks, in insertion order.
    pub items: Vec<Bookmark>,
    #[serde(skip)]
    next_id: u64,
}

impl Bookmarks {
    fn path(dir: &Path) -> PathBuf {
        dir.join("bookmarks.json")
    }

    fn load(dir: &Path) -> Bookmarks {
        let mut b: Bookmarks = load_json(&Self::path(dir), Bookmarks::default);
        if b.next_id == 0 {
            b.next_id = b.items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        }
        b
    }

    pub fn save(&self, dir: &Path) {
        save_json(&Self::path(dir), self);
    }

    /// Adds a bookmark and returns its id.
    pub fn add(&mut self, url: impl Into<String>, title: impl Into<String>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Bookmark {
            id,
            url: url.into(),
            title: title.into(),
            folder: String::new(),
            added: now_ms(),
        });
        id
    }

    /// Sets the folder of the most recently added bookmark.
    pub fn set_folder_last(&mut self, folder: String) {
        if let Some(last) = self.items.last_mut() {
            last.folder = folder;
        }
    }

    /// Removes by id.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|b| b.id != id);
        before != self.items.len()
    }

    /// Moves a bookmark into a folder.
    pub fn set_folder(&mut self, id: u64, folder: impl Into<String>) {
        if let Some(b) = self.items.iter_mut().find(|b| b.id == id) {
            b.folder = folder.into();
        }
    }

    /// The first bookmark matching a URL exactly.
    pub fn find_url(&self, url: &str) -> Option<&Bookmark> {
        self.items.iter().find(|b| b.url == url)
    }

    /// Prefix/substring suggestions for the omnibox.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Bookmark> {
        let q = query.to_lowercase();
        let mut hits: Vec<&Bookmark> = self
            .items
            .iter()
            .filter(|b| b.title.to_lowercase().contains(&q) || b.url.to_lowercase().contains(&q))
            .collect();
        hits.sort_by_key(|b| b.id);
        hits.truncate(limit);
        hits
    }

    /// Bookmarks-bar entries (empty folder).
    pub fn bar(&self) -> Vec<&Bookmark> {
        self.items.iter().filter(|b| b.folder.is_empty()).collect()
    }

    /// Distinct folder names.
    pub fn folders(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .items
            .iter()
            .filter(|b| !b.folder.is_empty())
            .map(|b| b.folder.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Serializes bookmarks to the Netscape bookmark-file format (folders
    /// are emitted as `<H3>` headers).
    pub fn export_html(&self) -> String {
        let mut out = String::from(
            "<!DOCTYPE NETSCAPE-Bookmark-file-1>\n<!-- Rrowser85 bookmarks export -->\n\
             <META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=UTF-8\">\n\
             <TITLE>Bookmarks</TITLE>\n<H1>Bookmarks</H1>\n<DL><p>\n",
        );
        for b in self.items.iter().filter(|b| b.folder.is_empty()) {
            out.push_str(&format!(
                "    <DT><A HREF=\"{}\" ADD_DATE=\"{}\">{}</A>\n",
                escape_html(&b.url),
                b.added / 1000,
                escape_html(b.display_title())
            ));
        }
        for folder in self.folders() {
            out.push_str(&format!(
                "    <DT><H3 ADD_DATE=\"{}\">{}</H3>\n    <DL><p>\n",
                now_ms() / 1000,
                escape_html(&folder)
            ));
            for b in self.items.iter().filter(|b| b.folder == folder) {
                out.push_str(&format!(
                    "        <DT><A HREF=\"{}\" ADD_DATE=\"{}\">{}</A>\n",
                    escape_html(&b.url),
                    b.added / 1000,
                    escape_html(b.display_title())
                ));
            }
            out.push_str("    </DL><p>\n");
        }
        out.push_str("</DL><p>\n");
        out
    }

    /// Parses the Netscape bookmark-file format (folders become names).
    pub fn import_html(&mut self, text: &str) -> usize {
        let start = self.items.len();
        let mut current_folder = String::new();
        for line in text.lines() {
            let trimmed = line.trim();
            let lowered = trimmed.to_lowercase();
            if lowered.starts_with("<h3") {
                current_folder = text_between(trimmed, "<h3", "</h3")
                    .map(|t| t.trim().to_string())
                    .unwrap_or_default();
                continue;
            }
            if lowered.starts_with("</dl") {
                current_folder = String::new();
                continue;
            }
            if !lowered.contains("<a ") {
                continue;
            }
            let Some(href) = extract_attr(trimmed, "href") else {
                continue;
            };
            let title = text_after_tag(trimmed);
            let id = self.next_id;
            self.next_id += 1;
            self.items.push(Bookmark {
                id,
                url: href,
                title,
                folder: current_folder.clone(),
                added: now_ms(),
            });
        }
        self.items.len() - start
    }
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let mut needle = String::with_capacity(name.len() + 2);
    needle.push_str(name);
    needle.push_str("=\"");
    let start = tag.to_lowercase().find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Text between the first `>` and the next `<` after the opening tag.
fn text_after_tag(tag: &str) -> String {
    match tag.find('>') {
        Some(open) => {
            let rest = &tag[open + 1..];
            let end = rest.find('<').unwrap_or(rest.len());
            rest[..end].trim().to_string()
        }
        None => String::new(),
    }
}

/// Text between two case-insensitive markers.
fn text_between(text: &str, open_marker: &str, close_marker: &str) -> Option<String> {
    let lowered = text.to_lowercase();
    let open = lowered.find(open_marker)? + open_marker.len();
    let close = lowered.find(close_marker).unwrap_or(text.len());
    let _ = close;
    // Take the text after the '>' of the opening tag.
    let rest = &text[open.min(text.len())..];
    let gt = rest.find('>')?;
    let tail = &rest[gt + 1..];
    let lt = tail.find('<').unwrap_or(tail.len());
    Some(tail[..lt].trim().to_string())
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// One history visit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Visited URL.
    pub url: String,
    /// Page title when known.
    pub title: String,
    /// Visit time (ms since epoch).
    pub visited: u64,
}

/// The browsing history (most recent first).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    /// Visits, newest first.
    pub entries: Vec<HistoryEntry>,
}

impl History {
    fn path(dir: &Path) -> PathBuf {
        dir.join("history.json")
    }

    fn load(dir: &Path) -> History {
        load_json(&Self::path(dir), History::default)
    }

    pub fn save(&self, dir: &Path) {
        save_json(&Self::path(dir), self);
    }

    /// Records a visit (dedupes consecutive visits to the same URL).
    pub fn visit(&mut self, url: impl Into<String>, title: impl Into<String>) {
        let url = url.into();
        let title = title.into();
        if let Some(first) = self.entries.first_mut() {
            if first.url == url {
                first.title = if title.is_empty() {
                    first.title.clone()
                } else {
                    title
                };
                first.visited = now_ms();
                return;
            }
        }
        self.entries.insert(
            0,
            HistoryEntry {
                url,
                title,
                visited: now_ms(),
            },
        );
        if self.entries.len() > 10_000 {
            self.entries.truncate(10_000);
        }
    }

    /// Search over URL and title.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&HistoryEntry> {
        if query.is_empty() {
            return self.entries.iter().take(limit).collect();
        }
        let q = query.to_lowercase();
        let mut hits: Vec<&HistoryEntry> = self
            .entries
            .iter()
            .filter(|e| e.url.to_lowercase().contains(&q) || e.title.to_lowercase().contains(&q))
            .collect();
        hits.truncate(limit);
        hits
    }

    /// Clears everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Clears entries newer than `hours` back (or all when `None`).
    pub fn clear_recent(&mut self, hours: Option<u64>) {
        match hours {
            None => self.clear(),
            Some(h) => {
                let cutoff = now_ms().saturating_sub(h * 3_600_000);
                self.entries.retain(|e| e.visited < cutoff);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// A permission state for a site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionState {
    /// Prompt when a site asks.
    #[default]
    Ask,
    /// Always allow.
    Allow,
    /// Always block.
    Block,
}

/// Permission kinds the manager knows.
pub const PERMISSION_KINDS: &[&str] = &[
    "camera",
    "microphone",
    "location",
    "notifications",
    "clipboard",
    "popups",
];

/// Per-site permission grants: site -> permission -> state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Grants.
    pub sites: HashMap<String, HashMap<String, PermissionState>>,
}

impl Permissions {
    fn path(dir: &Path) -> PathBuf {
        dir.join("permissions.json")
    }

    fn load(dir: &Path) -> Permissions {
        load_json(&Self::path(dir), Permissions::default)
    }

    pub fn save(&self, dir: &Path) {
        save_json(&Self::path(dir), self);
    }

    /// The state for (site, permission) — `Ask` by default.
    pub fn get(&self, site: &str, permission: &str) -> PermissionState {
        self.sites
            .get(site)
            .and_then(|map| map.get(permission))
            .copied()
            .unwrap_or(PermissionState::Ask)
    }

    /// Sets a grant.
    pub fn set(&mut self, site: impl Into<String>, permission: &str, state: PermissionState) {
        self.sites
            .entry(site.into())
            .or_default()
            .insert(permission.to_owned(), state);
    }

    /// Sites that have at least one non-default grant.
    pub fn configured_sites(&self) -> Vec<String> {
        let mut sites: Vec<String> = self.sites.keys().cloned().collect();
        sites.sort();
        sites
    }

    /// Drops a site's grants.
    pub fn reset_site(&mut self, site: &str) {
        self.sites.remove(site);
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// The saved session (open tabs in order).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionData {
    /// URLs of the open tabs, in order.
    pub tabs: Vec<String>,
    /// Index of the active tab.
    pub active: usize,
}

impl SessionData {
    fn path(dir: &Path) -> PathBuf {
        dir.join("session.json")
    }

    fn load(dir: &Path) -> SessionData {
        load_json(&Self::path(dir), SessionData::default)
    }

    pub fn save(&self, dir: &Path) {
        save_json(&Self::path(dir), self);
    }
}

// ---------------------------------------------------------------------------
// Top sites (speed dial)
// ---------------------------------------------------------------------------

/// A speed-dial tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopSite {
    /// Tile label.
    pub label: String,
    /// Navigated URL.
    pub url: String,
    /// Visit count.
    pub visits: u64,
}

/// Derives the top sites from history, ordered by frequency.
pub fn top_sites(history: &History, limit: usize) -> Vec<TopSite> {
    let mut counts: HashMap<String, (String, u64)> = HashMap::new();
    for entry in &history.entries {
        if entry.url.starts_with("rowser:") {
            continue;
        }
        let host = url::Url::parse(&entry.url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        if host.is_empty() {
            continue;
        }
        let site = registrable(&host);
        let label = site.clone();
        let entry_url = normalize_site_url(&entry.url);
        let slot = counts.entry(site).or_insert((label, 0));
        slot.1 += 1;
        let _ = entry_url;
    }
    let mut sites: Vec<TopSite> = counts
        .into_iter()
        .map(|(host, (label, visits))| TopSite {
            label: label_or_host(&label, &host),
            url: format!("https://{host}/"),
            visits,
        })
        .collect();
    sites.sort_by(|a, b| b.visits.cmp(&a.visits).then(a.label.cmp(&b.label)));
    sites.truncate(limit);
    sites
}

fn label_or_host(label: &str, host: &str) -> String {
    if !label.is_empty() && label != host {
        label.to_string()
    } else {
        host.trim_start_matches("www.").to_string()
    }
}

fn normalize_site_url(url: &str) -> String {
    url.to_string()
}

/// Crude registrable-domain extraction (last two labels), enough for tiles.
fn registrable(host: &str) -> String {
    let trimmed = host.trim_start_matches("www.");
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.len() >= 2 {
        parts[parts.len() - 2..].join(".")
    } else {
        trimmed.to_string()
    }
}
