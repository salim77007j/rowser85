//! Rrowser public API: the UI-facing facade over the engine.
//!
//! This is the surface another agent's UI connects to:
//!
//! * Create/navigate/close tabs, set viewport, scroll, dispatch UI events.
//! * Await engine events (`PageLoaded`, `FrameReady`, `ConsoleMessage`, ...).
//! * Read the latest frame per tab as raw premultiplied RGBA pixels
//!   (present it through winit/skia/wgpu on the UI side).
//! * Tune privacy settings at runtime.
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use rowser_api::{BrowserApi, EngineConfig};
//!
//! let browser = BrowserApi::start(EngineConfig::default())?;
//! let tab = browser.new_tab(Some("https://example.com".into()));
//!
//! while let Some(event) = browser.next_event().await {
//!     if matches!(event, rowser_api::EngineEvent::FrameReady { .. }) {
//!         if let Some(frame) = browser.frame(tab) {
//!             // frame.width, frame.height, frame.straight_rgba()
//!         }
//!         break;
//!     }
//! }
//! browser.shutdown();
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use rowser_engine::{Command, Engine, EngineEvent as InnerEvent, TabSnapshot};
use rowser_privacy::PrivacySettings;
use rowser_rendering::Frame;

pub use rowser_engine::{EngineConfig, TabId};
pub use rowser_privacy::PrivacySettings as Privacy;

/// Re-exported engine events.
pub type EngineEvent = InnerEvent;

/// A view of the latest rendered frame for a tab.
#[derive(Clone)]
pub struct FrameView {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Premultiplied RGBA8 pixels (tiny-skia byte order).
    pub frame: Arc<Frame>,
}

impl FrameView {
    /// Saves the frame to a PNG file (straight RGBA).
    pub fn save_png(&self, path: &str) -> std::io::Result<()> {
        self.frame.save_png(path)
    }

    /// Straight (non-premultiplied) RGBA bytes.
    pub fn straight_rgba(&self) -> Vec<u8> {
        self.frame.to_straight_rgba()
    }
}

/// The browser facade.
#[derive(Clone)]
pub struct BrowserApi {
    engine: Engine,
}

impl BrowserApi {
    /// Boots the engine.
    pub fn start(config: EngineConfig) -> anyhow::Result<BrowserApi> {
        let (engine, _rx) = Engine::start(config)?;
        Ok(BrowserApi { engine })
    }

    /// Async-flavored start for UI runtimes.
    pub async fn start_async(config: EngineConfig) -> anyhow::Result<BrowserApi> {
        let config_clone = config.clone();
        let engine = tokio::task::spawn_blocking(move || Engine::start(config_clone))
            .await
            .map_err(|e| anyhow::anyhow!("engine start: {e}"))??;
        Ok(BrowserApi { engine: engine.0 })
    }

    /// Creates a new tab, optionally navigating to `url`.
    pub fn new_tab(&self, url: Option<String>) -> TabId {
        self.engine.new_tab(url)
    }

    /// Navigates a tab.
    pub fn navigate(&self, tab: TabId, url: impl Into<String>) {
        self.engine.send(Command::Navigate(tab, url.into()));
    }

    /// Closes a tab.
    pub fn close_tab(&self, tab: TabId) {
        self.engine.send(Command::CloseTab(tab));
    }

    /// Focuses a tab (others become suspending after the idle timeout).
    pub fn focus(&self, tab: TabId) {
        self.engine.send(Command::Focus(tab));
    }

    /// Sets the viewport (CSS pixels).
    pub fn set_viewport(&self, tab: TabId, width: f32, height: f32) {
        self.engine.send(Command::SetViewport(tab, width, height));
    }

    /// Scrolls a tab.
    pub fn scroll(&self, tab: TabId, y: f32) {
        self.engine.send(Command::Scroll(tab, y));
    }

    /// Dispatches a click event to a DOM node handle (from hit-testing).
    pub fn click(&self, tab: TabId, node: u64) {
        self.engine
            .send(Command::UiEvent(tab, node, "click".to_owned()));
    }

    /// Dispatches a UI event to a DOM node.
    pub fn ui_event(&self, tab: TabId, node: u64, event_type: impl Into<String>) {
        self.engine
            .send(Command::UiEvent(tab, node, event_type.into()));
    }

    /// Updates privacy settings.
    pub fn set_privacy(&self, settings: PrivacySettings) {
        self.engine.send(Command::SetPrivacy(settings));
    }

    /// Subscribes to engine events.
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        let inner = self.engine.subscribe();
        EventAdapter::wrap(inner)
    }

    /// Awaits the next event on a fresh subscription.
    pub async fn next_event(&self) -> Option<EngineEvent> {
        let mut events = self.events();
        loop {
            match events.recv().await {
                Ok(event) => return Some(event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// The latest frame for a tab.
    pub fn frame(&self, tab: TabId) -> Option<FrameView> {
        let snapshot = self.engine.snapshot(tab)?;
        let frame = snapshot.frame?;
        Some(FrameView {
            width: frame.width,
            height: frame.height,
            frame,
        })
    }

    /// The tab snapshot (title, url, content size, memory).
    pub fn snapshot(&self, tab: TabId) -> Option<TabSnapshot> {
        self.engine.snapshot(tab)
    }

    /// Live tabs.
    pub fn tabs(&self) -> Vec<TabId> {
        self.engine.tabs()
    }

    /// Document title for a tab.
    pub fn title(&self, tab: TabId) -> Option<String> {
        self.engine.snapshot(tab).map(|s| s.title)
    }

    /// Document content size (scroll extent) for a tab.
    pub fn content_size(&self, tab: TabId) -> Option<(f32, f32)> {
        self.engine.snapshot(tab).map(|s| s.content_size)
    }

    /// Shuts the engine down.
    pub fn shutdown(&self) {
        self.engine.shutdown();
    }
}

/// Adapts the engine's internal event stream into the public type.
struct EventAdapter;

impl EventAdapter {
    fn wrap(
        mut inner: tokio::sync::broadcast::Receiver<InnerEvent>,
    ) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        // The engine's broadcast channel is re-typed through a forwarding
        // task so both internal and public receivers can coexist.
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        tokio::spawn(async move {
            loop {
                match inner.recv().await {
                    Ok(event) => {
                        if tx.send(EngineEvent::from(event)).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }
}
