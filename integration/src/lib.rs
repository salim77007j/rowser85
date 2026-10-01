//! `rowser-shell` — the integration layer gluing the rowser85 engine to a
//! UI. Owns the engine handle, the event pump, browser-level state
//! (bookmarks, history, settings, permissions, session), the download
//! manager and print-to-PDF.
//!
//! The UI thread never blocks: it polls `poll_events()` once per frame and
//! calls engine commands through `browser()`. Background threads wake the
//! UI through the installed [`Waker`].

mod downloads;
mod print;
mod store;
mod suggest;

pub use downloads::{DownloadItem, DownloadPhase, Downloads};
pub use print::{print_to_pdf, render_pages, write_pdf, PrintPage, A4_H, A4_W};
pub use store::{
    top_sites, Bookmarks, History, HistoryEntry, PermissionState, Permissions, PrivacyMirror,
    ProfileStore, SearchEngine, Settings, StartupMode, ThemeMode, TopSite, PERMISSION_KINDS,
};
pub use suggest::{normalize_url, resolve_input, suggest, Suggestion, SuggestionKind};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rowser_api::{BrowserApi, EngineEvent, TabId};
use rowser_engine::EngineConfig;

/// A cross-thread repaint hook: the UI installs a closure, background
/// threads call `wake()`.
pub struct Waker {
    f: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Waker {
    /// Installs (or replaces) the wake closure.
    pub fn set(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.f.lock().unwrap() = Some(f);
    }

    /// Fires the wake closure, when installed.
    pub fn wake(&self) {
        if let Some(f) = self.f.lock().unwrap().as_ref() {
            f();
        }
    }
}

impl Default for Waker {
    fn default() -> Self {
        Waker {
            f: Mutex::new(None),
        }
    }
}

/// One devtools console line.
#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    /// Level: log/info/warn/error.
    pub level: String,
    /// Message text.
    pub text: String,
    /// Origin tab.
    pub tab: TabId,
}

/// Live privacy telemetry for the dashboard.
#[derive(Debug, Clone, Default)]
pub struct PrivacyStats {
    /// Requests blocked by the adblock engine.
    pub ads_blocked: u64,
    /// Requests blocked as trackers.
    pub trackers_blocked: u64,
    /// CNAME-cloaked trackers unmasked.
    pub cname_unmasked: u64,
    /// Per-tab blocked totals.
    pub per_tab: std::collections::HashMap<TabId, u64>,
}

impl PrivacyStats {
    /// Total blocked requests.
    pub fn total_blocked(&self) -> u64 {
        self.ads_blocked + self.trackers_blocked + self.cname_unmasked
    }

    /// Number of fingerprint protections currently active.
    pub fn fingerprint_protections(&self, privacy: &rowser_privacy::PrivacySettings) -> u8 {
        let mut n = 0;
        if privacy.anti_fingerprinting {
            n += 1;
        }
        if privacy.webrtc_protection {
            n += 1;
        }
        if privacy.block_third_party_cookies {
            n += 1;
        }
        n
    }
}

/// A recently closed tab (for reopen).
#[derive(Debug, Clone)]
pub struct ClosedTab {
    /// URL it had.
    pub url: String,
    /// Title it had.
    pub title: String,
}

/// The browser shell. Construct once at startup; it owns the engine.
pub struct Shell {
    browser: BrowserApi,
    store: ProfileStore,
    downloads: Downloads,
    waker: Arc<Waker>,
    events_rx: Mutex<std::sync::mpsc::Receiver<EngineEvent>>,
    stats: PrivacyStats,
    console: Vec<ConsoleEntry>,
    closed_tabs: Vec<ClosedTab>,
    tab_titles: Mutex<std::collections::HashMap<TabId, String>>,
    session_timer: std::time::Instant,
    history_dirty: std::time::Instant,
    startup: std::time::Instant,
    boot_ms: AtomicU64,
    rss_kb: AtomicU64,
    last_known_urls: Mutex<std::collections::HashMap<TabId, String>>,
    active_tab: std::sync::RwLock<Option<TabId>>,
}

impl Shell {
    /// Boots the engine and the shell. `profile_dir` holds all user data.
    pub fn start(profile_dir: impl Into<std::path::PathBuf>) -> anyhow::Result<Shell> {
        let startup = std::time::Instant::now();
        let store = ProfileStore::load(profile_dir);
        let waker = Arc::new(Waker::default());

        let config = EngineConfig {
            privacy: store.settings.privacy.to_engine(),
            ..EngineConfig::default()
        };
        let browser = BrowserApi::start(config)?;

        // Event pump: the engine's broadcast stream is async; forward it to
        // a plain channel the UI can poll.
        let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        let pump_browser = browser.clone();
        let pump_waker = Arc::clone(&waker);
        std::thread::Builder::new()
            .name("rowser-shell-pump".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("shell pump runtime");
                runtime.block_on(async move {
                    let mut events = pump_browser.events();
                    loop {
                        match events.recv().await {
                            Ok(event) => {
                                if tx.send(event).is_err() {
                                    break;
                                }
                                pump_waker.wake();
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
            })?;

        let downloads = Downloads::new(&store.dir, Arc::clone(&waker))?;

        let shell = Shell {
            browser,
            store,
            downloads,
            waker,
            events_rx: Mutex::new(rx),
            stats: PrivacyStats::default(),
            console: Vec::new(),
            closed_tabs: Vec::new(),
            tab_titles: Mutex::new(std::collections::HashMap::new()),
            session_timer: std::time::Instant::now(),
            history_dirty: std::time::Instant::now(),
            startup,
            boot_ms: AtomicU64::new(0),
            rss_kb: AtomicU64::new(0),
            last_known_urls: Mutex::new(std::collections::HashMap::new()),
            active_tab: std::sync::RwLock::new(None),
        };
        shell.apply_startup();
        Ok(shell)
    }

    /// Applies the startup mode from settings (NTP / homepage / session).
    fn apply_startup(&self) {
        use crate::store::StartupMode::*;
        match &self.store.settings.startup {
            NewTab => {
                self.browser.new_tab(None);
            }
            Homepage if self.store.settings.homepage != "rowser://newtab" => {
                self.browser
                    .new_tab(Some(self.store.settings.homepage.clone()));
            }
            _ => {
                self.browser.new_tab(None);
            }
        }
        // Session restore always reopens the previous tabs (after the first).
        if self.store.settings.restore_session && !self.store.session.tabs.is_empty() {
            for url in self.store.session.tabs.clone() {
                if url.is_empty() {
                    continue;
                }
                self.browser.new_tab(Some(url));
            }
            let tabs = self.browser.tabs();
            if let Some(active) =
                tabs.get(self.store.session.active.min(tabs.len().saturating_sub(1)))
            {
                self.browser.focus(*active);
            }
        }
    }

    /// The engine facade.
    pub fn browser(&self) -> &BrowserApi {
        &self.browser
    }

    /// The profile store (settings, bookmarks, history, permissions).
    pub fn store(&self) -> &ProfileStore {
        &self.store
    }

    /// Mutable store access (the UI edits settings/bookmarks through this).
    pub fn store_mut(&mut self) -> &mut ProfileStore {
        &mut self.store
    }

    /// The download manager.
    pub fn downloads(&self) -> &Downloads {
        &self.downloads
    }

    /// Live privacy stats.
    pub fn stats(&self) -> &PrivacyStats {
        &self.stats
    }

    /// Devtools console log.
    pub fn console(&self) -> &[ConsoleEntry] {
        &self.console
    }

    /// Appends a devtools console entry (e.g. a JS eval result).
    pub fn push_console(&mut self, tab: TabId, level: &str, text: String) {
        self.console.push(ConsoleEntry {
            level: level.to_owned(),
            text,
            tab,
        });
        if self.console.len() > 500 {
            let overflow = self.console.len() - 500;
            self.console.drain(0..overflow);
        }
    }

    /// Clears the devtools console.
    pub fn clear_console(&mut self) {
        self.console.clear();
    }

    /// Recently closed tabs (most recent last).
    pub fn closed_tabs(&self) -> &[ClosedTab] {
        &self.closed_tabs
    }

    /// Engine boot time in milliseconds (engine-only, before UI paint).
    pub fn boot_ms(&self) -> u64 {
        self.boot_ms.load(Ordering::Relaxed)
    }

    /// Process RSS in KiB, sampled with the event loop.
    pub fn rss_kb(&self) -> u64 {
        self.rss_kb.load(Ordering::Relaxed)
    }

    /// Installs the UI wake callback.
    pub fn set_waker(&self, f: Box<dyn Fn() + Send + Sync>) {
        self.waker.set(f);
    }

    /// Drains pending engine events, updating shell state. Call once per UI
    /// frame.
    pub fn poll_events(&mut self) -> Vec<EngineEvent> {
        let mut drained = Vec::new();
        {
            let rx = self.events_rx.lock().unwrap();
            while let Ok(event) = rx.try_recv() {
                drained.push(event);
            }
        }
        for event in &drained {
            self.observe(event);
        }
        // Periodic housekeeping.
        if self.session_timer.elapsed() > Duration::from_secs(15) {
            self.session_timer = std::time::Instant::now();
            self.save_session();
        }
        if self.history_dirty.elapsed() > Duration::from_secs(5) {
            self.history_dirty = std::time::Instant::now();
            self.store.history.save(&self.store.dir);
        }
        drained
    }

    /// Internal state updates driven by engine events.
    fn observe(&mut self, event: &EngineEvent) {
        match event {
            EngineEvent::PageLoaded { tab, url, title } => {
                self.store.history.visit(url.clone(), title.clone());
                self.tab_titles.lock().unwrap().insert(*tab, title.clone());
                if self.boot_ms.load(Ordering::Relaxed) == 0 {
                    self.boot_ms
                        .store(self.startup.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
                self.sample_rss();
            }
            EngineEvent::TitleChanged { tab, title } => {
                self.tab_titles.lock().unwrap().insert(*tab, title.clone());
            }
            EngineEvent::ConsoleMessage { tab, level, text } => {
                self.console.push(ConsoleEntry {
                    level: level.clone(),
                    text: text.clone(),
                    tab: *tab,
                });
                if self.console.len() > 500 {
                    let overflow = self.console.len() - 500;
                    self.console.drain(0..overflow);
                }
            }
            EngineEvent::BlockedRequest { tab, url, reason } => {
                let _ = url;
                match reason.as_str() {
                    "cname-cloaking" => self.stats.cname_unmasked += 1,
                    r if r.contains("ad") => self.stats.ads_blocked += 1,
                    _ => self.stats.trackers_blocked += 1,
                }
                *self.stats.per_tab.entry(*tab).or_insert(0) += 1;
            }
            EngineEvent::TabClosed(tab) => {
                let title = self
                    .tab_titles
                    .lock()
                    .unwrap()
                    .get(tab)
                    .cloned()
                    .unwrap_or_default();
                // Capture the URL from the last snapshot before it is gone.
                let snapshot_url = self.last_known_urls.lock().unwrap().get(tab).cloned();
                if let Some(url) = snapshot_url.filter(|u| !u.is_empty()) {
                    self.closed_tabs.push(ClosedTab { url, title });
                    if self.closed_tabs.len() > 25 {
                        self.closed_tabs.remove(0);
                    }
                }
                self.tab_titles.lock().unwrap().remove(tab);
                self.last_known_urls.lock().unwrap().remove(tab);
                self.save_session();
            }
            EngineEvent::NavigationStarted { tab, url } => {
                self.last_known_urls
                    .lock()
                    .unwrap()
                    .insert(*tab, url.clone());
            }
            EngineEvent::MemoryPressure { .. } => {
                self.sample_rss();
            }
            _ => {}
        }
    }

    /// Reopens the most recently closed tab.
    pub fn reopen_closed_tab(&mut self) -> Option<TabId> {
        let closed = self.closed_tabs.pop()?;
        if closed.url.is_empty() {
            return None;
        }
        Some(self.browser.new_tab(Some(closed.url)))
    }

    /// Saves the current session (open tab URLs + active index).
    pub fn save_session(&mut self) {
        let tabs = self.browser.tabs();
        let mut urls = Vec::with_capacity(tabs.len());
        let known = self.last_known_urls.lock().unwrap();
        for tab in &tabs {
            let url = self
                .browser
                .snapshot(*tab)
                .map(|s| s.url.clone())
                .or_else(|| known.get(tab).cloned())
                .unwrap_or_default();
            urls.push(url);
        }
        drop(known);
        self.store.session.tabs = urls;
        let active = self.active_tab();
        self.store.session.active = tabs.iter().position(|t| Some(*t) == active).unwrap_or(0);
        self.store.session.save(&self.store.dir);
    }

    /// Persists everything now (call on exit).
    pub fn persist(&mut self) {
        self.save_session();
        self.store.save_all();
    }

    /// Applies new privacy settings to the engine and persists them.
    pub fn set_privacy(&mut self, privacy: rowser_privacy::PrivacySettings) {
        self.store.settings.privacy = crate::store::PrivacyMirror::from(privacy.clone());
        self.browser.set_privacy(privacy);
        self.store.settings.save(&self.store.dir);
    }

    /// Records the UI's notion of the active tab (for session save).
    pub fn set_active_tab(&mut self, tab: Option<TabId>) {
        *self.active_tab.write().unwrap() = tab;
    }

    /// The UI's active tab.
    pub fn active_tab(&self) -> Option<TabId> {
        *self.active_tab.read().unwrap()
    }

    fn sample_rss(&self) {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb: u64 = rest
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    self.rss_kb.store(kb, Ordering::Relaxed);
                }
            }
        }
    }

    /// Shuts the engine down cleanly.
    pub fn shutdown(&mut self) {
        self.persist();
        self.browser.shutdown();
    }
}
