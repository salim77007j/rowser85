//! Rrowser engine core.
//!
//! Architecture: one OS thread per tab ("page thread") owning that tab's
//! DOM, JS runtime, style, layout and painter — strong isolation with
//! zero-cost cross-tab communication. A single engine loop thread owns the
//! tab registry and routes commands; all I/O (fetches, timers, DNS) runs on
//! a shared multithreaded tokio runtime and is delivered back as messages.
//!
//! ```text
//!  UI (api) ──commands──▶ engine loop ──page messages──▶ page threads
//!      ▲                     │   ▲                           │
//!      └────── events ───────┘   └── network completions ────┘
//!                                 (tokio runtime + timer service)
//! ```

pub mod memory;
pub mod page;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rowser_js::{EngineEvent as JsIntoEvent, JsCommand, JsConfig};
use rowser_layout::Viewport;
use rowser_privacy::blocklist::{Blocklist, ResourceType as FilterResourceType};
use rowser_privacy::cname::CnameGuard;
use rowser_privacy::fingerprint::SpoofProfile;
pub use rowser_privacy::PrivacySettings;
use rowser_privacy::RequestVerdict;
use rowser_rendering::Frame;
use rowser_storage::Storage;

pub use page::SubresourceKind;

/// Tab identifier.
pub type TabId = u64;

/// Engine events broadcast to the UI layer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum EngineEvent {
    /// A tab was created.
    TabCreated(TabId),
    /// A tab was closed.
    TabClosed(TabId),
    /// Navigation started.
    NavigationStarted {
        /// Tab id.
        tab: TabId,
        /// Target URL.
        url: String,
    },
    /// The main document finished loading (DOM + first frame ready).
    PageLoaded {
        /// Tab id.
        tab: TabId,
        /// Final URL.
        url: String,
        /// Document title.
        title: String,
    },
    /// Load progress (0.0-1.0).
    LoadProgress {
        /// Tab id.
        tab: TabId,
        /// Fraction loaded.
        progress: f32,
    },
    /// A new frame is available in the tab snapshot.
    FrameReady {
        /// Tab id.
        tab: TabId,
        /// Frame id.
        frame: u64,
    },
    /// The document title changed.
    TitleChanged {
        /// Tab id.
        tab: TabId,
        /// New title.
        title: String,
    },
    /// A console message from JS.
    ConsoleMessage {
        /// Tab id.
        tab: TabId,
        /// `log`/`info`/`warn`/`error`.
        level: String,
        /// Message text.
        text: String,
    },
    /// A request was blocked by the privacy engine.
    BlockedRequest {
        /// Tab id.
        tab: TabId,
        /// Blocked URL.
        url: String,
        /// Reason.
        reason: String,
    },
    /// Result of a devtools-console JavaScript evaluation.
    JsResult {
        /// Tab id.
        tab: TabId,
        /// Whether evaluation succeeded.
        ok: bool,
        /// The serialized result (or error text).
        result: String,
    },
    /// Find-in-page match update.
    FindResult {
        /// Tab id.
        tab: TabId,
        /// Total matches for the query.
        matches: usize,
        /// 0-based index of the active match, if any.
        active: Option<usize>,
    },
    /// The page DOM was saved to disk.
    PageSaved {
        /// Tab id.
        tab: TabId,
        /// File path written.
        path: String,
    },
    /// What is under a document-space point (hover / status bar).
    HitTestResult {
        /// Tab id.
        tab: TabId,
        /// DOM node id (0 when nothing was hit).
        node: u64,
        /// Hit element tag name ("" when nothing was hit).
        tag: String,
        /// Href of the enclosing anchor, if any.
        href: Option<String>,
        /// Anchor text, when inside a link.
        text: Option<String>,
    },
    /// The tab was suspended (frozen, frame freed).
    TabSuspended(TabId),
    /// The tab resumed from suspension.
    TabResumed(TabId),
    /// Engine memory pressure actions.
    MemoryPressure {
        /// Total estimated tab memory.
        total_bytes: u64,
    },
}

/// Live tab snapshot, readable without touching the page thread.
#[derive(Debug, Default, Clone)]
pub struct TabSnapshot {
    /// Current frame (premultiplied RGBA).
    pub frame: Option<Arc<Frame>>,
    /// Document title.
    pub title: String,
    /// Current URL.
    pub url: String,
    /// Document content size (width, height) for scrolling.
    pub content_size: (f32, f32),
    /// Estimated memory usage in bytes.
    pub memory_bytes: u64,
    /// True while a navigation is in flight.
    pub loading: bool,
    /// True when session history has a back entry.
    pub can_go_back: bool,
    /// True when session history has a forward entry.
    pub can_go_forward: bool,
}

/// Commands accepted by the engine.
#[derive(Debug)]
pub enum Command {
    /// Create a tab with a caller-assigned id (optionally navigating).
    CreateTab(TabId, Option<String>),
    /// Navigate a tab.
    Navigate(TabId, String),
    /// Close a tab.
    CloseTab(TabId),
    /// Focus a tab (suspends others after the idle timeout).
    Focus(TabId),
    /// Set the tab viewport.
    SetViewport(TabId, f32, f32),
    /// Scroll a tab (re-renders with the offset without re-layout).
    Scroll(TabId, f32),
    /// Dispatch a UI event (click etc.) to a DOM node.
    UiEvent(TabId, u64, String),
    /// Update privacy settings (applies to future requests).
    SetPrivacy(PrivacySettings),
    /// Navigate back in the session history.
    GoBack(TabId),
    /// Navigate forward in the session history.
    GoForward(TabId),
    /// Reload the current document.
    Reload(TabId),
    /// Cancel the navigation in flight.
    Stop(TabId),
    /// Evaluate JavaScript in the page (devtools console).
    EvalJs(TabId, String),
    /// Set the find-in-page query (empty clears highlights).
    Find(TabId, String),
    /// Step the active find match (1 forward, −1 backward).
    FindStep(TabId, i32),
    /// Save the current DOM as HTML to a path.
    SavePage(TabId, std::path::PathBuf),
    /// Click at a document-space point (link navigation or JS event).
    ClickAt(TabId, f32, f32),
    /// Query what is at a document-space point (hover).
    HitTest(TabId, f32, f32),
    /// Shut the engine down.
    Shutdown,
}

/// Internal events routed by the engine loop.
#[derive(Debug)]
pub enum Internal {
    /// The page requests a batch of subresource fetches.
    FetchSubresources {
        /// Tab id.
        tab: TabId,
        /// Requests.
        requests: Vec<rowser_networking::FetchRequest>,
    },
    /// A command from a page's JS runtime (timer, fetch, ws, ...).
    PageCommand {
        /// Tab id.
        tab: TabId,
        /// Command.
        command: JsCommand,
    },
    /// A subresource fetch completed.
    FetchDone {
        /// Tab id.
        tab: TabId,
        /// URL.
        url: String,
        /// Status.
        status: u16,
        /// JSON header list.
        headers: String,
        /// Body.
        body: Vec<u8>,
    },
    /// A subresource fetch failed.
    FetchFailed {
        /// Tab id.
        tab: TabId,
        /// URL.
        url: String,
        /// Error.
        error: String,
    },
    /// A JS-initiated fetch completed.
    JsFetchDone {
        /// Tab id.
        tab: TabId,
        /// Fetch id.
        id: u64,
        /// Status.
        status: u16,
        /// JSON headers.
        headers: String,
        /// Base64 body.
        body_b64: String,
    },
    /// A JS-initiated fetch failed.
    JsFetchFailed {
        /// Tab id.
        tab: TabId,
        /// Fetch id.
        id: u64,
        /// Error.
        error: String,
    },
    /// A timer fired.
    TimerFired {
        /// Tab id.
        tab: TabId,
        /// Timer id.
        timer: u64,
    },
    /// A page thread reported its memory usage.
    MemoryReport {
        /// Tab id.
        tab: TabId,
        /// Estimated bytes.
        bytes: u64,
    },
    /// A WebSocket event.
    WsEvent {
        /// Tab id.
        tab: TabId,
        /// Socket id.
        socket: u64,
        /// Event kind.
        kind: String,
        /// Payload.
        data: String,
    },
    /// A page thread finished (tab closed).
    PageExited(TabId),
    /// Route a page message from an engine-side async task.
    PageMessage {
        /// Tab id.
        tab: TabId,
        /// Message.
        message: Box<page::Message>,
    },
    /// CNAME-cloaking verdict from the privacy gate thread.
    CnameVerdict {
        /// Tab id.
        tab: TabId,
        /// The gated request.
        request: Box<rowser_networking::FetchRequest>,
        /// True when the resolved host is a cloaked tracker.
        cloaked: bool,
        /// Human-readable resolution chain.
        detail: String,
    },
}

/// Engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Profile directory for storage.
    pub profile_dir: PathBuf,
    /// Privacy settings.
    pub privacy: PrivacySettings,
    /// JS runtime limits.
    pub js: JsConfig,
    /// Tab background suspension timeout.
    pub suspend_after: Duration,
    /// Memory management tick.
    pub memory_tick: Duration,
    /// Fraction of total RAM usable for tab memory before trimming.
    pub memory_budget_fraction: f32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            profile_dir: std::env::temp_dir().join("rowser-profile"),
            privacy: PrivacySettings::default(),
            js: JsConfig::default(),
            suspend_after: Duration::from_secs(60),
            memory_tick: Duration::from_secs(30),
            memory_budget_fraction: 0.35,
        }
    }
}

struct PageHandle {
    tx: std::sync::mpsc::Sender<page::Message>,
    thread: Option<JoinHandle<()>>,
    focused: bool,
    backgrounded_since: Option<std::time::Instant>,
    memory: u64,
    suspended: bool,
    pending_subresources: std::collections::HashSet<String>,
    /// WebSocket command channels for the tab.
    ws_senders: HashMap<u64, std::sync::mpsc::Sender<WsCommand>>,
}

/// A request queued for CNAME-cloaking verification.
#[derive(Debug)]
struct CnameCheck {
    /// Tab id.
    tab: TabId,
    /// The request to verify and (if clean) spawn.
    request: rowser_networking::FetchRequest,
}

/// Commands to a WebSocket task.
#[derive(Debug)]
enum WsCommand {
    /// Send a text frame.
    Send(String),
    /// Close the socket.
    Close,
}

/// The running engine handle: cheap to clone, thread-safe.
#[derive(Clone)]
pub struct Engine {
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
    snapshots: Arc<Mutex<HashMap<TabId, TabSnapshot>>>,
    event_tx: tokio::sync::broadcast::Sender<EngineEvent>,
    next_tab: Arc<AtomicU64>,
}

enum Cmd {
    User(Command),
    Internal(Internal),
}

/// Shared state handed to the page thread.
pub(crate) struct PageState {
    /// Tab id.
    pub tab: TabId,
    /// Engine command channel.
    pub engine_tx: std::sync::mpsc::Sender<Cmd>,
    /// Profile storage.
    pub storage: Arc<Storage>,
    /// Network context (kept for future direct page-thread use).
    #[allow(dead_code)]
    pub network: Arc<rowser_networking::NetworkContext>,
    /// JS runtime limits.
    pub js_config: JsConfig,
    /// Anti-fingerprinting profile.
    pub spoof: SpoofProfile,
    /// Event broadcaster.
    pub event_tx: tokio::sync::broadcast::Sender<EngineEvent>,
    /// Snapshot writer.
    pub snapshot_tx: page::SnapshotWriter,
}

impl Engine {
    /// Boots the engine (spawns all threads) and returns the handle plus
    /// the event subscription.
    pub fn start(
        config: EngineConfig,
    ) -> anyhow::Result<(Engine, tokio::sync::broadcast::Receiver<EngineEvent>)> {
        std::fs::create_dir_all(&config.profile_dir)?;
        let storage = Arc::new(Storage::open(config.profile_dir.join("profile.redb"))?);
        let (event_tx, event_rx) = tokio::sync::broadcast::channel(256);
        let snapshots = Arc::new(Mutex::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();

        let engine = Engine {
            cmd_tx: cmd_tx.clone(),
            snapshots: Arc::clone(&snapshots),
            event_tx: event_tx.clone(),
            next_tab: Arc::new(AtomicU64::new(1)),
        };

        // The network context must be built inside the runtime. Build both the
        // runtime and the contexts on a dedicated bootstrap thread so callers
        // may themselves be running inside a tokio worker (driving a nested
        // runtime from a worker thread would panic).
        let storage_for_net = Arc::clone(&storage);
        let privacy_config = config.privacy.clone();
        let (runtime, network, resolver) = {
            std::thread::scope(|scope| {
                let handle = scope.spawn(move || -> anyhow::Result<_> {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()?;
                    let network = runtime.block_on(async {
                        rowser_networking::build_context(
                            storage_for_net,
                            privacy_config,
                            rowser_networking::HickoryDnsConfig::default(),
                            rowser_networking::H3Settings::default(),
                        )
                        .await
                    })?;
                    // CNAME-cloaking gate's dedicated resolver.
                    let resolver = runtime.block_on(async {
                        rowser_networking::client::build_resolver(
                            &rowser_networking::HickoryDnsConfig::default(),
                        )
                        .await
                    })?;
                    Ok((runtime, network, resolver))
                });
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("engine bootstrap thread panicked"))?
            })
        }?;

        // CNAME-cloaking gate thread: owns its own resolver + guard.
        let (cname_tx, cname_rx) = std::sync::mpsc::channel::<CnameCheck>();
        {
            let cmd_tx_clone = cmd_tx.clone();
            std::thread::Builder::new()
                .name("rowser-cname-gate".into())
                .spawn(move || {
                    cname_gate_thread(resolver, CnameGuard::new(), cname_rx, cmd_tx_clone);
                })?;
        }

        let state = Arc::new(EngineLoop {
            config,
            storage,
            network: Arc::new(network),
            runtime,
            blocklist: Mutex::new(Blocklist::with_builtin_rules()),
            cname_gate: cname_tx,
            cmd_tx: cmd_tx.clone(),
            event_tx,
            snapshots: Arc::clone(&snapshots),
            tabs: Mutex::new(HashMap::new()),
        });

        std::thread::Builder::new()
            .name("rowser-engine".into())
            .spawn(move || engine_loop(state, cmd_rx))?;

        Ok((engine, event_rx))
    }

    /// Sends a user command.
    pub fn send(&self, command: Command) {
        let _ = self.cmd_tx.send(Cmd::User(command));
    }

    /// Allocates a tab id.
    pub fn next_tab_id(&self) -> TabId {
        self.next_tab.fetch_add(1, Ordering::Relaxed)
    }

    /// Creates a tab (id allocated here, creation processed on the loop).
    pub fn new_tab(&self, url: Option<String>) -> TabId {
        let id = self.next_tab_id();
        let _ = self.cmd_tx.send(Cmd::User(Command::CreateTab(id, url)));
        id
    }

    /// Subscribes to engine events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        self.event_tx.subscribe()
    }

    /// Reads a tab snapshot (frame, title, url).
    pub fn snapshot(&self, tab: TabId) -> Option<TabSnapshot> {
        self.snapshots.lock().unwrap().get(&tab).cloned()
    }

    /// Lists live tab ids.
    pub fn tabs(&self) -> Vec<TabId> {
        self.snapshots.lock().unwrap().keys().copied().collect()
    }

    /// Shuts the engine down.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Cmd::User(Command::Shutdown));
    }
}

struct EngineLoop {
    config: EngineConfig,
    storage: Arc<Storage>,
    network: Arc<rowser_networking::NetworkContext>,
    runtime: tokio::runtime::Runtime,
    /// Ad/tracker blocklist (engine-loop-thread only: the adblock engine
    /// contains non-Send `Rc`s).
    blocklist: Mutex<Blocklist>,
    /// Channel to the CNAME-cloaking gate thread.
    cname_gate: std::sync::mpsc::Sender<CnameCheck>,
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
    event_tx: tokio::sync::broadcast::Sender<EngineEvent>,
    snapshots: Arc<Mutex<HashMap<TabId, TabSnapshot>>>,
    tabs: Mutex<HashMap<TabId, PageHandle>>,
}

impl EngineLoop {
    fn broadcast(&self, event: EngineEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Total estimated tab memory.
    fn total_tab_memory(&self) -> u64 {
        self.tabs.lock().unwrap().values().map(|h| h.memory).sum()
    }

    /// Page handles accessor used by the memory manager.
    pub(crate) fn pages(&self) -> &Mutex<HashMap<TabId, PageHandle>> {
        &self.tabs
    }

    fn source_url(&self, tab: TabId) -> String {
        self.snapshots
            .lock()
            .unwrap()
            .get(&tab)
            .map(|snapshot| snapshot.url.clone())
            .unwrap_or_default()
    }
}

fn engine_loop(state: Arc<EngineLoop>, cmd_rx: std::sync::mpsc::Receiver<Cmd>) {
    let mut last_memory_tick = std::time::Instant::now();
    let mut next_suspension_check = std::time::Instant::now();
    loop {
        let timeout = Duration::from_millis(250);
        let msg = match cmd_rx.recv_timeout(timeout) {
            Ok(msg) => Some(msg),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let quit = match msg {
            Some(Cmd::User(command)) => handle_user(&state, command),
            Some(Cmd::Internal(internal)) => {
                handle_internal(&state, internal);
                false
            }
            None => false,
        };
        if quit {
            break;
        }

        if next_suspension_check.elapsed() > Duration::from_secs(1) {
            next_suspension_check = std::time::Instant::now();
            check_suspension(&state);
        }
        if last_memory_tick.elapsed() > state.config.memory_tick {
            last_memory_tick = std::time::Instant::now();
            memory::tick(&state);
        }
    }
    // Shutdown: join page threads.
    let mut tabs = state.tabs.lock().unwrap();
    for handle in tabs.values_mut() {
        let _ = handle.tx.send(page::Message::Shutdown);
    }
    for handle in tabs.values_mut() {
        if let Some(thread) = handle.thread.take() {
            let _ = thread.join();
        }
    }
    tabs.clear();
    state.snapshots.lock().unwrap().clear();
}

fn handle_user(state: &EngineLoop, command: Command) -> bool {
    match command {
        Command::Shutdown => return true,
        Command::CreateTab(tab, url) => create_tab(state, tab, url),
        Command::Navigate(tab, url) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::Navigate(url.clone()));
                let _ = state
                    .event_tx
                    .send(EngineEvent::NavigationStarted { tab, url });
            }
        }
        Command::CloseTab(tab) => close_tab(state, tab),
        Command::Focus(tab) => {
            let mut tabs = state.tabs.lock().unwrap();
            for (id, handle) in tabs.iter_mut() {
                if *id == tab {
                    handle.focused = true;
                    handle.backgrounded_since = None;
                    if handle.suspended {
                        handle.suspended = false;
                        let _ = handle.tx.send(page::Message::Resume);
                        let _ = state.event_tx.send(EngineEvent::TabResumed(tab));
                    }
                } else if handle.focused {
                    handle.focused = false;
                    handle.backgrounded_since = Some(std::time::Instant::now());
                }
            }
        }
        Command::SetViewport(tab, width, height) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle
                    .tx
                    .send(page::Message::SetViewport(Viewport { width, height }));
            }
        }
        Command::Scroll(tab, y) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::SetScroll(y));
            }
        }
        Command::UiEvent(tab, node, event_type) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::UiEvent(node, event_type));
            }
        }
        Command::SetPrivacy(privacy) => {
            *state.network.settings.write().unwrap() = privacy;
        }
        Command::GoBack(tab) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::GoBack);
            }
        }
        Command::GoForward(tab) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::GoForward);
            }
        }
        Command::Reload(tab) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::Reload);
            }
        }
        Command::Stop(tab) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::Stop);
            }
        }
        Command::EvalJs(tab, code) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::Eval(code));
            }
        }
        Command::Find(tab, query) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::Find(query));
            }
        }
        Command::FindStep(tab, delta) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::FindStep(delta));
            }
        }
        Command::SavePage(tab, path) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::SavePage(path));
            }
        }
        Command::ClickAt(tab, x, y) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::ClickAt(x, y));
            }
        }
        Command::HitTest(tab, x, y) => {
            if let Some(handle) = state.tabs.lock().unwrap().get(&tab) {
                let _ = handle.tx.send(page::Message::HitTest(x, y));
            }
        }
    }
    false
}

fn create_tab(state: &EngineLoop, requested: TabId, url: Option<String>) {
    let tab = requested;
    let (page_tx, page_rx) = std::sync::mpsc::channel::<page::Message>();
    let seed = blake3::hash(format!("tab-{tab}").as_bytes());
    let spoof = SpoofProfile::from_seed(*seed.as_bytes());

    {
        let mut snapshots = state.snapshots.lock().unwrap();
        snapshots.entry(tab).or_default();
    }
    let page_state = Arc::new(PageState {
        tab,
        engine_tx: state.cmd_tx.clone(),
        storage: Arc::clone(&state.storage),
        network: Arc::clone(&state.network),
        js_config: state.config.js.clone(),
        spoof,
        event_tx: state.event_tx.clone(),
        snapshot_tx: page::SnapshotWriter {
            tab,
            snapshots: Arc::clone(&state.snapshots),
        },
    });

    let thread = std::thread::Builder::new()
        .name(format!("rowser-page-{tab}"))
        .spawn({
            let page_state = Arc::clone(&page_state);
            move || page::run(page_state, page_rx)
        })
        .expect("page thread");

    state.tabs.lock().unwrap().insert(
        tab,
        PageHandle {
            tx: page_tx,
            thread: Some(thread),
            focused: false,
            // Unfocused tabs count as backgrounded from creation: the
            // suspension sweep may freeze them after the idle timeout.
            backgrounded_since: Some(std::time::Instant::now()),
            memory: 0,
            suspended: false,
            pending_subresources: std::collections::HashSet::new(),
            ws_senders: HashMap::new(),
        },
    );
    state.broadcast(EngineEvent::TabCreated(tab));
    if let Some(url) = url {
        let tx = state.tabs.lock().unwrap().get(&tab).map(|h| h.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(page::Message::Navigate(url.clone()));
        }
        let _ = state
            .event_tx
            .send(EngineEvent::NavigationStarted { tab, url });
    }
}

fn close_tab(state: &EngineLoop, tab: TabId) {
    let handle = state.tabs.lock().unwrap().remove(&tab);
    if let Some(mut handle) = handle {
        let _ = handle.tx.send(page::Message::Shutdown);
        if let Some(thread) = handle.thread.take() {
            let _ = thread.join();
        }
    }
    state.snapshots.lock().unwrap().remove(&tab);
    state.broadcast(EngineEvent::TabClosed(tab));
}

fn handle_internal(state: &EngineLoop, internal: Internal) {
    match internal {
        Internal::FetchSubresources { tab, requests } => {
            tracing::debug!(target: "rowser::engine", "fetch subresources: tab {tab}, {} requests", requests.len());
            {
                let mut tabs = state.tabs.lock().unwrap();
                let Some(handle) = tabs.get_mut(&tab) else {
                    return;
                };
                for request in &requests {
                    handle.pending_subresources.insert(request.url.clone());
                }
            }
            for request in requests {
                gate_fetch(state, tab, request);
            }
        }
        Internal::PageCommand { tab, command } => handle_page_command(state, tab, command),
        Internal::FetchDone {
            tab,
            url,
            status,
            headers,
            body,
        } => {
            let mut tabs = state.tabs.lock().unwrap();
            let Some(handle) = tabs.get_mut(&tab) else {
                return;
            };
            handle.pending_subresources.remove(&url);
            let pending = handle.pending_subresources.len();
            let tx = handle.tx.clone();
            drop(tabs);
            let _ = tx.send(page::Message::SubresourceFetched {
                url,
                status,
                headers,
                body,
                pending,
            });
        }
        Internal::FetchFailed { tab, url, error } => {
            let mut tabs = state.tabs.lock().unwrap();
            let Some(handle) = tabs.get_mut(&tab) else {
                return;
            };
            handle.pending_subresources.remove(&url);
            let pending = handle.pending_subresources.len();
            let tx = handle.tx.clone();
            drop(tabs);
            let _ = tx.send(page::Message::SubresourceFailed {
                url,
                error,
                pending,
            });
        }
        Internal::JsFetchDone {
            tab,
            id,
            status,
            headers,
            body_b64,
        } => {
            send_page(&state.tabs, tab, |tx| {
                tx.send(page::Message::JsEvent(JsIntoEvent::FetchCompleted {
                    id,
                    status,
                    headers,
                    body_b64,
                }))
            });
        }
        Internal::JsFetchFailed { tab, id, error } => {
            send_page(&state.tabs, tab, |tx| {
                tx.send(page::Message::JsEvent(JsIntoEvent::FetchFailed {
                    id,
                    error,
                }))
            });
        }
        Internal::TimerFired { tab, timer } => {
            let suspended = state
                .tabs
                .lock()
                .unwrap()
                .get(&tab)
                .map(|h| h.suspended)
                .unwrap_or(true);
            if !suspended {
                send_page(&state.tabs, tab, |tx| {
                    tx.send(page::Message::JsEvent(JsIntoEvent::TimerFired(timer)))
                });
            }
        }
        Internal::MemoryReport { tab, bytes } => {
            if let Some(handle) = state.tabs.lock().unwrap().get_mut(&tab) {
                handle.memory = bytes;
            }
            if let Some(snapshot) = state.snapshots.lock().unwrap().get_mut(&tab) {
                snapshot.memory_bytes = bytes;
            }
        }
        Internal::WsEvent {
            tab,
            socket,
            kind,
            data,
        } => {
            send_page(&state.tabs, tab, |tx| {
                tx.send(page::Message::JsEvent(JsIntoEvent::WsEvent {
                    id: socket,
                    kind,
                    data,
                }))
            });
        }
        Internal::PageExited(tab) => {
            let handle = state.tabs.lock().unwrap().remove(&tab);
            if let Some(mut handle) = handle {
                if let Some(thread) = handle.thread.take() {
                    let _ = thread.join();
                }
            }
        }
        Internal::PageMessage { tab, message } => {
            send_page(&state.tabs, tab, |tx| tx.send(*message));
        }
        Internal::CnameVerdict {
            tab,
            request,
            cloaked,
            detail,
        } => {
            if cloaked {
                tracing::debug!(target: "rowser::engine", "CNAME cloaking blocked: {detail}");
                let _ = state.event_tx.send(EngineEvent::BlockedRequest {
                    tab,
                    url: request.url.clone(),
                    reason: "cname-cloaking".to_owned(),
                });
                let mut tabs = state.tabs.lock().unwrap();
                let url = request.url.clone();
                if let Some(handle) = tabs.get_mut(&tab) {
                    handle.pending_subresources.remove(&url);
                    let pending = handle.pending_subresources.len();
                    let tx = handle.tx.clone();
                    drop(tabs);
                    let _ = tx.send(page::Message::SubresourceFailed {
                        url,
                        error: "cname-cloaking".to_owned(),
                        pending,
                    });
                }
            } else {
                spawn_subresource_fetch(state, tab, *request);
            }
        }
    }
}

fn send_page(
    tabs: &Mutex<HashMap<TabId, PageHandle>>,
    tab: TabId,
    f: impl FnOnce(
        &std::sync::mpsc::Sender<page::Message>,
    ) -> Result<(), std::sync::mpsc::SendError<page::Message>>,
) {
    let guard = tabs.lock().unwrap();
    if let Some(handle) = guard.get(&tab) {
        if f(&handle.tx).is_err() {
            tracing::debug!(target: "rowser::engine", "page {tab} channel closed");
        }
    }
}

fn handle_page_command(state: &EngineLoop, tab: TabId, command: JsCommand) {
    match command {
        JsCommand::TimerStart {
            id,
            delay_ms,
            interval,
        } => {
            let cmd_tx = state.cmd_tx.clone();
            state.runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(delay_ms.max(1))).await;
                    let _ = cmd_tx.send(Cmd::Internal(Internal::TimerFired { tab, timer: id }));
                    if !interval {
                        break;
                    }
                }
            });
        }
        JsCommand::TimerClear { .. } => {
            // Intervals check tab liveness on each fire; cleared one-shot
            // timers are ignored by the JS prelude.
        }
        JsCommand::FetchStart { id, url, init } => {
            let request = js_fetch_request(state, tab, url, init);
            if let RequestVerdict::Block(reason) = blocklist_check(state, &request) {
                let _ = state.event_tx.send(EngineEvent::BlockedRequest {
                    tab,
                    url: request.url,
                    reason,
                });
                let _ = state.cmd_tx.send(Cmd::Internal(Internal::JsFetchFailed {
                    tab,
                    id,
                    error: "blocked".to_owned(),
                }));
                return;
            }
            spawn_js_fetch(state, tab, id, request);
        }
        JsCommand::Console { level, text } => {
            let _ = state
                .event_tx
                .send(EngineEvent::ConsoleMessage { tab, level, text });
        }
        JsCommand::MarkDirty => {
            send_page(&state.tabs, tab, |tx| tx.send(page::Message::MarkDirty));
        }
        JsCommand::WsOpen { id, url } => {
            spawn_ws_task(state, tab, id, url);
        }
        JsCommand::WsSend { id, data } => {
            let sender = {
                let tabs = state.tabs.lock().unwrap();
                tabs.get(&tab).and_then(|h| h.ws_senders.get(&id)).cloned()
            };
            if let Some(sender) = sender {
                let _ = sender.send(WsCommand::Send(data));
            }
        }
        JsCommand::WsClose { id } => {
            let sender = {
                let mut tabs = state.tabs.lock().unwrap();
                tabs.get_mut(&tab).and_then(|h| h.ws_senders.remove(&id))
            };
            if let Some(sender) = sender {
                let _ = sender.send(WsCommand::Close);
            }
        }
        JsCommand::WorkerSpawn { id, url } => {
            // Fetch the worker script, then hand it to the page thread.
            let request = rowser_networking::FetchRequest {
                url: url.clone(),
                resource_type: rowser_networking::ResourceKind::Script,
                source_url: state.source_url(tab),
                top_site: None,
                ..rowser_networking::FetchRequest::default()
            };
            let network = Arc::clone(&state.network);
            let cmd_tx = state.cmd_tx.clone();
            state.runtime.spawn(async move {
                let outcome = rowser_networking::fetch(&network, request).await;
                let code = match outcome {
                    Ok(response) if response.is_success() => {
                        String::from_utf8_lossy(&response.body).into_owned()
                    }
                    Ok(response) => format!(
                        "console.error('worker script failed: HTTP {}');",
                        response.status
                    ),
                    Err(err) => format!("console.error('worker fetch failed: {err}');"),
                };
                send_page_direct(
                    cmd_tx,
                    tab,
                    page::Message::WorkerScriptFetched { worker: id, code },
                );
            });
        }
        JsCommand::WorkerPost { .. } | JsCommand::WorkerTerminate { .. } => {
            // Handled locally by the page thread.
        }
        JsCommand::WorkerEgress { .. } => {
            // Local routing only.
        }
    }
}

fn send_page_direct(cmd_tx: std::sync::mpsc::Sender<Cmd>, tab: TabId, message: page::Message) {
    let _ = cmd_tx.send(Cmd::Internal(Internal::PageMessage {
        tab,
        message: Box::new(message),
    }));
}

fn js_fetch_request(
    state: &EngineLoop,
    tab: TabId,
    url: String,
    init: String,
) -> rowser_networking::FetchRequest {
    let mut request = rowser_networking::FetchRequest {
        url,
        resource_type: rowser_networking::ResourceKind::Xhr,
        source_url: state.source_url(tab),
        top_site: None,
        ..rowser_networking::FetchRequest::default()
    };
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&init) {
        if let Some(method) = parsed.get("method").and_then(|m| m.as_str()) {
            request.method = method.to_owned();
        }
        if let Some(headers) = parsed.get("headers").and_then(|h| h.as_object()) {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    request.headers.push((name.clone(), value.to_owned()));
                }
            }
        }
        if let Some(body) = parsed.get("body").and_then(|b| b.as_str()) {
            request.body = Some(bytes::Bytes::copy_from_slice(body.as_bytes()));
        }
    }
    request
}

fn spawn_js_fetch(
    state: &EngineLoop,
    tab: TabId,
    id: u64,
    request: rowser_networking::FetchRequest,
) {
    let network = Arc::clone(&state.network);
    let cmd_tx = state.cmd_tx.clone();
    state.runtime.spawn(async move {
        match rowser_networking::fetch(&network, request).await {
            Ok(response) => {
                use base64::Engine;
                let body_b64 =
                    base64::engine::general_purpose::STANDARD.encode(response.body.as_ref());
                let headers =
                    serde_json::to_string(&response.headers).unwrap_or_else(|_| "[]".to_owned());
                let _ = cmd_tx.send(Cmd::Internal(Internal::JsFetchDone {
                    tab,
                    id,
                    status: response.status,
                    headers,
                    body_b64,
                }));
            }
            Err(err) => {
                let _ = cmd_tx.send(Cmd::Internal(Internal::JsFetchFailed {
                    tab,
                    id,
                    error: err.to_string(),
                }));
            }
        }
    });
}

fn spawn_subresource_fetch(
    state: &EngineLoop,
    tab: TabId,
    request: rowser_networking::FetchRequest,
) {
    let network = Arc::clone(&state.network);
    let cmd_tx = state.cmd_tx.clone();
    let request_url = request.url.clone();
    tracing::debug!(target: "rowser::engine", "spawning fetch task for {request_url}");
    state.runtime.spawn(async move {
        tracing::debug!(target: "rowser::engine", "fetch task running for {request_url}");
        match rowser_networking::fetch(&network, request).await {
            Ok(response) => {
                tracing::debug!(target: "rowser::engine", "fetch completed: {request_url} status {}", response.status);
                let headers = serde_json::to_string(
                    &response.headers,
                )
                .unwrap_or_else(|_| "[]".to_owned());
                let _ = cmd_tx.send(Cmd::Internal(Internal::FetchDone {
                    tab,
                    url: request_url,
                    status: response.status,
                    headers,
                    body: response.body.to_vec(),
                }));
            }
            Err(err) => {
                tracing::debug!(target: "rowser::engine", "fetch failed: {request_url}: {err}");
                let _ = cmd_tx.send(Cmd::Internal(Internal::FetchFailed {
                    tab,
                    url: request_url,
                    error: err.to_string(),
                }));
            }
        }
    });
}

fn spawn_ws_task(state: &EngineLoop, tab: TabId, socket: u64, url: String) {
    let cmd_tx = state.cmd_tx.clone();
    let (ws_tx, ws_rx) = std::sync::mpsc::channel::<WsCommand>();
    {
        let mut tabs = state.tabs.lock().unwrap();
        if let Some(handle) = tabs.get_mut(&tab) {
            handle.ws_senders.insert(socket, ws_tx);
        }
    }
    state.runtime.spawn(async move {
        match rowser_networking::ws::connect(&url).await {
            Ok(mut socket_conn) => {
                let _ = cmd_tx.send(Cmd::Internal(Internal::WsEvent {
                    tab,
                    socket,
                    kind: "open".to_owned(),
                    data: String::new(),
                }));
                // Serve events and commands until close.
                loop {
                    // Non-blocking command poll.
                    match ws_rx.try_recv() {
                        Ok(WsCommand::Send(data)) => {
                            if socket_conn.send_text(&data).await.is_err() {
                                break;
                            }
                        }
                        Ok(WsCommand::Close) => {
                            let _ = socket_conn.close().await;
                            break;
                        }
                        Err(_) => {}
                    }
                    match socket_conn.recv().await {
                        Ok(rowser_networking::ws::WsEvent::Text(text)) => {
                            let _ = cmd_tx.send(Cmd::Internal(Internal::WsEvent {
                                tab,
                                socket,
                                kind: "message".to_owned(),
                                data: text,
                            }));
                        }
                        Ok(rowser_networking::ws::WsEvent::Binary(_)) => continue,
                        Ok(rowser_networking::ws::WsEvent::Closed(reason)) => {
                            let _ = cmd_tx.send(Cmd::Internal(Internal::WsEvent {
                                tab,
                                socket,
                                kind: "close".to_owned(),
                                data: reason,
                            }));
                            break;
                        }
                        Ok(rowser_networking::ws::WsEvent::Continue) => continue,
                        Err(_) => break,
                    }
                }
            }
            Err(err) => {
                let _ = cmd_tx.send(Cmd::Internal(Internal::WsEvent {
                    tab,
                    socket,
                    kind: "error".to_owned(),
                    data: err.to_string(),
                }));
            }
        }
    });
}

/// The CNAME gate thread: resolves third-party hosts and rejects
/// cloaked trackers before the fetch is spawned.
fn cname_gate_thread(
    resolver: hickory_resolver::TokioResolver,
    mut guard: CnameGuard,
    rx: std::sync::mpsc::Receiver<CnameCheck>,
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    for check in rx {
        let host = url::Url::parse(&check.request.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let chain = runtime.block_on(rowser_networking::client::cname_chain(&resolver, &host));
        let resolved = chain.last().cloned().unwrap_or_else(|| host.clone());
        let source = if check.request.source_url.is_empty() {
            check.request.url.clone()
        } else {
            check.request.source_url.clone()
        };
        let cloaked = guard.is_cloaked(&host, &resolved, &source);
        let detail = if chain.is_empty() {
            resolved
        } else {
            chain.join(" -> ")
        };
        let _ = cmd_tx.send(Cmd::Internal(Internal::CnameVerdict {
            tab: check.tab,
            request: Box::new(check.request),
            cloaked,
            detail,
        }));
    }
}

/// Runs the sync blocklist gate; returns the verdict.
fn blocklist_check(
    state: &EngineLoop,
    request: &rowser_networking::FetchRequest,
) -> RequestVerdict {
    if !state
        .network
        .settings
        .read()
        .map(|s| s.block_ads)
        .unwrap_or(true)
    {
        return RequestVerdict::Allow;
    }
    let source = if request.source_url.is_empty() {
        request.url.clone()
    } else {
        request.source_url.clone()
    };
    let resource = match request.resource_type {
        rowser_networking::ResourceKind::Document => FilterResourceType::Document,
        rowser_networking::ResourceKind::SubDocument => FilterResourceType::SubDocument,
        rowser_networking::ResourceKind::Stylesheet => FilterResourceType::Stylesheet,
        rowser_networking::ResourceKind::Script => FilterResourceType::Script,
        rowser_networking::ResourceKind::Image => FilterResourceType::Image,
        rowser_networking::ResourceKind::Font => FilterResourceType::Font,
        rowser_networking::ResourceKind::Xhr => FilterResourceType::Xhr,
        rowser_networking::ResourceKind::WebSocket => FilterResourceType::WebSocket,
        rowser_networking::ResourceKind::Media => FilterResourceType::Media,
        rowser_networking::ResourceKind::Other => FilterResourceType::Other,
    };
    state
        .blocklist
        .lock()
        .unwrap()
        .check(&request.url, &source, resource)
}

/// Full gate: blocklist (sync) + CNAME (async gate thread for third-party).
/// Returns true when the request was blocked/queued (caller does nothing).
fn gate_fetch(state: &EngineLoop, tab: TabId, request: rowser_networking::FetchRequest) -> bool {
    if let RequestVerdict::Block(reason) = blocklist_check(state, &request) {
        let _ = state.event_tx.send(EngineEvent::BlockedRequest {
            tab,
            url: request.url.clone(),
            reason,
        });
        // Deliver the failure to the waiting page/subresource counter.
        let mut tabs = state.tabs.lock().unwrap();
        let blocked_url = request.url.clone();
        if let Some(handle) = tabs.get_mut(&tab) {
            handle.pending_subresources.remove(&blocked_url);
            let pending = handle.pending_subresources.len();
            let tx = handle.tx.clone();
            drop(tabs);
            let _ = tx.send(page::Message::SubresourceFailed {
                url: blocked_url,
                error: "blocked".to_owned(),
                pending,
            });
        }
        return true;
    }
    // Third-party requests go through the CNAME gate; first-party spawn now.
    let source_site = registrable(&request.source_url);
    let target_site = registrable(&request.url);
    if !source_site.is_empty() && source_site != target_site {
        let _ = state.cname_gate.send(CnameCheck { tab, request });
        return true;
    }
    spawn_subresource_fetch(state, tab, request);
    false
}

fn registrable(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(rowser_privacy::psl_registrable))
        .unwrap_or_default()
}

fn check_suspension(state: &EngineLoop) {
    let suspend_after = state.config.suspend_after;
    let mut to_suspend: Vec<TabId> = Vec::new();
    {
        let tabs = state.tabs.lock().unwrap();
        for (tab, handle) in tabs.iter() {
            if !handle.focused
                && !handle.suspended
                && handle
                    .backgrounded_since
                    .map(|t| t.elapsed() > suspend_after)
                    .unwrap_or(false)
            {
                to_suspend.push(*tab);
            }
        }
    }
    for tab in to_suspend {
        let mut tabs = state.tabs.lock().unwrap();
        if let Some(handle) = tabs.get_mut(&tab) {
            handle.suspended = true;
            let _ = handle.tx.send(page::Message::Suspend);
            let _ = state.event_tx.send(EngineEvent::TabSuspended(tab));
        }
    }
}
