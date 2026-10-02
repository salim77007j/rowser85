//! The page actor: one OS thread per tab, owning the DOM, JS runtime,
//! styles, layout and painter.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use rowser_dom::{Dom, NodeId};
use rowser_js::{
    prelude, EngineEvent as JsEngineEvent, JsCommand, JsConfig, JsRuntime, PageBridge,
};
use rowser_layout::{LayoutEngine, LayoutResult, Viewport};
use rowser_media::{
    MediaEvent as PipelineEvent, MediaIngressSender, MediaNotification, MediaPipeline,
};
use rowser_parsing::cascade::StyleMap;
use rowser_parsing::css::{parse_stylesheet, MediaContext, ParsedStylesheet};
use rowser_parsing::html::{parse_html, Document};
use rowser_rendering::display_list::{build_display_list, ImageMap};
use rowser_rendering::painter::{Painter, RenderOptions};
use rowser_rendering::{DecodedImage, Frame};

use crate::{Cmd, EngineEvent, Internal, PageState, TabId, TabSnapshot};

/// Messages delivered to a page thread.
#[derive(Debug)]
pub enum Message {
    /// Navigate to a URL.
    Navigate(String),
    /// A subresource fetch completed (`pending` = requests still in flight).
    SubresourceFetched {
        /// URL.
        url: String,
        /// HTTP status.
        status: u16,
        /// JSON header list.
        headers: String,
        /// Body bytes.
        body: Vec<u8>,
        /// Requests still pending.
        pending: usize,
    },
    /// A subresource fetch failed.
    SubresourceFailed {
        /// URL.
        url: String,
        /// Error text.
        error: String,
        /// Requests still pending.
        pending: usize,
    },
    /// A worker script was fetched.
    WorkerScriptFetched {
        /// Worker id.
        worker: u64,
        /// Script source.
        code: String,
    },
    /// A command from JavaScript (timers, fetches, ...).
    JsCommand(JsCommand),
    /// An event to dispatch into JavaScript.
    JsEvent(JsEngineEvent),
    /// A worker context posted a message.
    WorkerEgress {
        /// Worker id.
        worker: u64,
        /// JSON message.
        message: String,
    },
    /// Viewport changed.
    SetViewport(Viewport),
    /// Scroll offset changed.
    SetScroll(f32),
    /// A UI event on a DOM node.
    UiEvent(u64, String),
    /// The DOM was mutated by script; re-render.
    MarkDirty,
    /// Freeze the tab (backgrounded).
    Suspend,
    /// Unfreeze the tab.
    Resume,
    /// Navigate back in the session history.
    GoBack,
    /// Navigate forward in the session history.
    GoForward,
    /// Reload the current document.
    Reload,
    /// Cancel the navigation in flight.
    Stop,
    /// Evaluate JavaScript in the page (devtools console).
    Eval(String),
    /// Set the find-in-page query (empty clears highlighting).
    Find(String),
    /// Step the active find match forward (1) or backward (-1).
    FindStep(i32),
    /// Save the current DOM as HTML to a path.
    SavePage(std::path::PathBuf),
    /// Click at a document-space point (hit-test + link navigation or JS event).
    ClickAt(f32, f32),
    /// Query what is at a document-space point (hover/status bar).
    HitTest(f32, f32),
    /// A chunk of media bytes for a media element (direct source stream or
    /// an appended MSE segment, already routed by lane).
    MediaData {
        /// Media element node handle.
        node: u64,
        /// Bytes for lane 0 (direct source).
        data: Vec<u8>,
    },
    /// The direct-source stream for a media element finished.
    MediaEof {
        /// Media element node handle.
        node: u64,
    },
    /// The media worker presented a new frame for an element.
    MediaFrameReady {
        /// Media element node handle.
        node: u64,
    },
    /// A media pipeline event (pre-formatted for JS dispatch).
    MediaEngineEvent {
        /// Media element node handle.
        node: u64,
        /// Event type (loadedmetadata, canplay, timeupdate, ended, error...).
        event: String,
        /// JSON detail payload.
        detail: String,
    },
    /// The navigation target is a media resource: render the built-in
    /// media viewer document instead of parsing fetched bytes.
    MediaDocument(String),
    /// Terminate the page thread.
    Shutdown,
}

/// One playing (or loaded) media element.
struct MediaSlot {
    pipeline: MediaPipeline,
    #[allow(dead_code)]
    ingress: MediaIngressSender,
    autoplay: bool,
    loop_playback: bool,
    started: bool,
    /// Mirrored mute state (the pipeline has no getter; native controls
    /// toggle from here).
    muted: bool,
}

/// Page-side MSE state: appended-but-unattached bytes per SourceBuffer.
#[derive(Default)]
struct MediaSourceState {
    source_buffers: HashMap<u64, String>,
    backlog: HashMap<u64, Vec<u8>>,
    attached_node: Option<u64>,
}

/// Kinds of subresources a page waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubresourceKind {
    /// Main HTML document.
    Document,
    /// External stylesheet.
    Stylesheet,
    /// External script.
    Script,
    /// Image.
    Image,
    /// Audio/video source (streamed to the media pipeline, never buffered).
    Media,
}

impl SubresourceKind {
    /// Maps to the networking resource classification.
    pub fn to_resource_kind(self) -> rowser_networking::ResourceKind {
        match self {
            SubresourceKind::Document => rowser_networking::ResourceKind::Document,
            SubresourceKind::Stylesheet => rowser_networking::ResourceKind::Stylesheet,
            SubresourceKind::Script => rowser_networking::ResourceKind::Script,
            SubresourceKind::Image => rowser_networking::ResourceKind::Image,
            SubresourceKind::Media => rowser_networking::ResourceKind::Media,
        }
    }
}

/// Writer for the shared tab snapshot.
pub struct SnapshotWriter {
    /// Tab id.
    pub tab: TabId,
    /// Shared snapshots.
    pub snapshots: Arc<std::sync::Mutex<HashMap<TabId, TabSnapshot>>>,
}

impl SnapshotWriter {
    fn update(&self, f: impl FnOnce(&mut TabSnapshot)) {
        let mut snapshots = self.snapshots.lock().unwrap();
        if let Some(snapshot) = snapshots.get_mut(&self.tab) {
            f(snapshot);
        }
    }

    fn set_frame(&self, frame: Option<Arc<Frame>>, content_size: (f32, f32)) {
        self.update(|snapshot| {
            snapshot.frame = frame;
            snapshot.content_size = content_size;
        });
    }
}

/// Runs the page thread.
pub(crate) fn run(state: Arc<PageState>, rx: Receiver<Message>) {
    let (js_tx, js_rx) = std::sync::mpsc::channel::<JsCommand>();
    let mut page = Page::new(Arc::clone(&state), js_tx, js_rx);
    let trace = std::env::var("ROWSER_UI_TRACE").is_ok();
    let mut ticks: u64 = 0;
    let mut renders: u64 = 0;
    let mut last_report = std::time::Instant::now();
    loop {
        // Drain JS-originated commands first (fast, non-blocking).
        while let Ok(command) = page.js_rx.try_recv() {
            page.handle_js_command(command);
        }
        // Block briefly on engine messages; the timeout lets us do idle work
        // (dirty re-renders, memory reports).
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(message) => {
                if page.handle(message) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                page.idle();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if trace {
            ticks += 1;
            if page.dirty {
                renders += 1;
            }
            if last_report.elapsed() > Duration::from_secs(2) {
                eprintln!(
                    "[page-{}] loop ticks={ticks}/2s dirty_now={} renders_signal={renders}",
                    state.tab, page.dirty,
                );
                ticks = 0;
                renders = 0;
                last_report = std::time::Instant::now();
            }
        }
    }
    page.shutdown_report();
}

struct Page {
    state: Arc<PageState>,
    engine_tx: Sender<Cmd>,
    js_tx: Sender<JsCommand>,
    js_rx: Receiver<JsCommand>,
    js: Option<JsRuntime>,
    workers: HashMap<u64, JsRuntime>,
    dom: Option<Rc<RefCell<Dom>>>,
    document: Option<Document>,
    style_map: Option<StyleMap>,
    layout: Option<LayoutResult>,
    display_list: Option<rowser_rendering::DisplayList>,
    images: ImageMap,
    layout_engine: LayoutEngine,
    /// Parsed-stylesheet cache: (fingerprint of css_texts + media, sheets).
    /// Reparsing every stylesheet on every re-render is the dominant cost
    /// for JS-heavy pages that dirty the DOM continuously.
    css_cache: Option<(u64, Vec<ParsedStylesheet>)>,
    painter: Painter,
    url: String,
    pending: HashMap<String, SubresourceKind>,
    css_texts: Vec<String>,
    scripts: Vec<(Option<String>, String, NodeId)>,
    viewport: Viewport,
    scroll_y: f32,
    suspended: bool,
    dirty: bool,
    rendered_dom_version: u64,
    last_memory_report: std::time::Instant,
    navigating: bool,
    /// Session history (visited URLs, oldest first).
    history: Vec<String>,
    /// Current position in the session history.
    history_pos: usize,
    /// Find-in-page match rectangles (document coordinates).
    find_matches: Vec<rowser_rendering::Rect>,
    /// Index of the active find match.
    active_match: Option<usize>,
    /// The current find query ("" = no search).
    find_query: String,
    /// Script identities (URL or inline-N) already executed for this
    /// document — Chrome's "execute once" semantics.
    executed_scripts: std::collections::HashSet<String>,
    /// Playing / loaded media elements keyed by node handle.
    media_slots: HashMap<u64, MediaSlot>,
    /// MSE MediaSource states keyed by MediaSource id.
    mse_sources: HashMap<u64, MediaSourceState>,
    /// SourceBuffer id → (node handle, lane).
    sb_lanes: HashMap<u64, (u64, u64)>,
    /// Latest decoded video frame per <video> node (blitted into the
    /// display list).
    video_frames: ImageMap,
    /// Media state mirror shared with the JS runtime natives.
    media_mirror: rowser_js::MediaMirrorMap,
    /// Layout rects shared with the JS natives (getBoundingClientRect).
    rect_mirror: rowser_js::RectMirrorMap,
    /// (scroll_y, viewport_width, viewport_height) shared with JS natives.
    viewport_mirror: rowser_js::ViewportMirror,
    /// Last-reported media activity (drives the suspension override).
    media_active: bool,
}

impl Page {
    fn new(state: Arc<PageState>, js_tx: Sender<JsCommand>, js_rx: Receiver<JsCommand>) -> Page {
        Page {
            engine_tx: state.engine_tx.clone(),
            state,
            js_tx,
            js_rx,
            js: None,
            workers: HashMap::new(),
            dom: None,
            document: None,
            style_map: None,
            layout: None,
            display_list: None,
            images: ImageMap::new(),
            css_cache: None,
            layout_engine: LayoutEngine::new(),
            painter: Painter::new(),
            url: String::new(),
            pending: HashMap::new(),
            css_texts: Vec::new(),
            scripts: Vec::new(),
            viewport: Viewport::default(),
            scroll_y: 0.0,
            suspended: false,
            dirty: false,
            rendered_dom_version: 0,
            last_memory_report: std::time::Instant::now(),
            navigating: false,
            history: Vec::new(),
            history_pos: 0,
            find_matches: Vec::new(),
            active_match: None,
            find_query: String::new(),
            executed_scripts: std::collections::HashSet::new(),
            media_slots: HashMap::new(),
            mse_sources: HashMap::new(),
            sb_lanes: HashMap::new(),
            video_frames: ImageMap::new(),
            media_mirror: Rc::new(RefCell::new(HashMap::new())),
            rect_mirror: Rc::new(RefCell::new(HashMap::new())),
            viewport_mirror: Rc::new(RefCell::new((0.0, 0.0, 0.0))),
            media_active: false,
        }
    }

    fn handle(&mut self, message: Message) -> bool {
        match message {
            Message::Shutdown => return true,
            Message::Navigate(url) => self.navigate(url),
            Message::SubresourceFetched {
                url,
                body,
                headers,
                pending,
                ..
            } => {
                self.subresource_fetched(url, body, &headers);
                if pending == 0 {
                    self.subresources_complete();
                }
            }
            Message::SubresourceFailed { url, pending, .. } => {
                self.pending.remove(&url);
                if pending == 0 {
                    self.subresources_complete();
                }
            }
            Message::WorkerScriptFetched { worker, code } => {
                self.spawn_worker(worker, code);
            }
            Message::JsEvent(event) => {
                if let Some(js) = &self.js {
                    js.dispatch(event);
                    self.mark_if_dirty();
                }
            }
            Message::WorkerEgress { worker, message } => {
                if let Some(js) = &self.js {
                    js.dispatch(JsEngineEvent::WorkerMessage {
                        id: worker,
                        message,
                    });
                }
            }
            Message::JsCommand(command) => {
                self.handle_js_command(command);
            }
            Message::SetViewport(viewport) => {
                self.viewport = viewport;
                self.dirty = true;
            }
            Message::SetScroll(y) => {
                let max = self
                    .layout
                    .as_ref()
                    .map(|l| (l.content_size.1 - self.viewport.height).max(0.0))
                    .unwrap_or(0.0);
                self.scroll_y = y.clamp(0.0, max);
                self.viewport_mirror.borrow_mut().0 = self.scroll_y;
                self.repaint();
            }
            Message::UiEvent(node, event_type) => {
                if let Some(js) = &self.js {
                    js.dispatch(JsEngineEvent::DomEvent { node, event_type });
                    self.mark_if_dirty();
                }
            }
            Message::MarkDirty => {
                self.dirty = true;
            }
            Message::Suspend => {
                self.suspended = true;
                self.display_list = None;
                self.layout = None;
                self.snapshot(|snapshot| snapshot.frame = None);
            }
            Message::Resume => {
                self.suspended = false;
                self.dirty = true;
            }
            Message::GoBack => self.go_history(-1),
            Message::GoForward => self.go_history(1),
            Message::Reload => self.reload(),
            Message::Stop => self.stop_loading(),
            Message::Eval(code) => self.eval_js(code),
            Message::Find(query) => self.find_set(&query),
            Message::FindStep(delta) => self.find_step(delta),
            Message::SavePage(path) => self.save_page(path),
            Message::ClickAt(x, y) => self.click_at(x, y),
            Message::HitTest(x, y) => self.hit_test(x, y),
            Message::MediaData { node, data } => self.media_data(node, &data),
            Message::MediaEof { node } => self.media_eof(node),
            Message::MediaFrameReady { node } => self.media_frame_ready(node),
            Message::MediaEngineEvent {
                node,
                event,
                detail,
            } => self.media_engine_event(node, &event, &detail),
            Message::MediaDocument(url) => self.media_viewer_document(url),
        }
        false
    }

    fn handle_js_command(&mut self, command: JsCommand) {
        match command {
            JsCommand::WorkerEgress { id, message } => {
                if let Some(js) = &self.js {
                    js.dispatch(JsEngineEvent::WorkerMessage { id, message });
                }
            }
            JsCommand::WorkerPost { id, message } => {
                if let Some(worker) = self.workers.get(&id) {
                    let json = serde_json::to_string(&message).unwrap_or_default();
                    // Escape as a JS string argument.
                    let arg = serde_json::to_string(&json).unwrap_or_default();
                    let _ = worker.eval(&format!("__onWorkerMessage({arg})"), "worker-msg.js");
                }
            }
            JsCommand::WorkerTerminate { id } => {
                self.workers.remove(&id);
            }
            JsCommand::MarkDirty => {
                self.dirty = true;
            }
            JsCommand::MediaSetSrc { node, url } => self.media_set_src(node, &url),
            JsCommand::MediaPlay { node } => self.media_play(node, true),
            JsCommand::MediaPause { node } => self.media_play(node, false),
            JsCommand::MediaSeek { node, time } => {
                if let Some(slot) = self.media_slots.get(&node) {
                    slot.pipeline.seek(time);
                }
            }
            JsCommand::MediaSetVolume { node, volume } => {
                if let Some(slot) = self.media_slots.get(&node) {
                    slot.pipeline.set_volume(volume);
                }
            }
            JsCommand::MediaSetMuted { node, muted } => {
                if let Some(slot) = self.media_slots.get(&node) {
                    slot.pipeline.set_muted(muted);
                }
            }
            JsCommand::MediaCreateSource { ms_id } => {
                self.mse_sources.entry(ms_id).or_default();
            }
            JsCommand::MediaAddSourceBuffer { ms_id, mime, sb_id } => {
                if let Some(source) = self.mse_sources.get_mut(&ms_id) {
                    source.source_buffers.insert(sb_id, mime);
                    source.backlog.entry(sb_id).or_default();
                }
            }
            JsCommand::MediaAppendBuffer { sb_id, data } => {
                self.mse_append(sb_id, &data);
            }
            JsCommand::MediaEndOfStream { ms_id } => {
                if let Some(source) = self.mse_sources.get(&ms_id) {
                    if let Some(node) = source.attached_node {
                        if let Some(slot) = self.media_slots.get(&node) {
                            for sb in source.source_buffers.keys() {
                                slot.ingress.close_lane(*sb);
                            }
                        }
                    }
                }
            }
            JsCommand::Navigate { url } => {
                // Script-initiated navigation (location.href = ...). Resolve
                // against the current page URL so relative links work.
                let absolute = url::Url::parse(&self.url)
                    .and_then(|base| base.join(&url))
                    .map(|joined| joined.to_string())
                    .unwrap_or(url);
                tracing::debug!(target: "rowser::engine", "tab {} JS navigation → {absolute}", self.state.tab);
                self.navigate(absolute);
            }
            other => {
                // Timers, fetches, websockets, console, worker spawns are
                // engine-side.
                let _ = self.engine_tx.send(Cmd::Internal(Internal::PageCommand {
                    tab: self.state.tab,
                    command: other,
                }));
            }
        }
    }

    fn navigate(&mut self, url: String) {
        self.start_navigation(url, true);
    }

    /// Starts a navigation, optionally pushing it onto the session history.
    fn start_navigation(&mut self, url: String, push_history: bool) {
        tracing::debug!(target: "rowser::engine", "tab {} navigating to {url}", self.state.tab);
        self.navigating = true;
        self.reset_page();
        self.url = url.clone();
        if push_history && !url.is_empty() {
            self.history.truncate(self.history_pos);
            self.history.push(url.clone());
            self.history_pos = self.history.len() - 1;
        }
        let (back, forward) = self.history_state();
        self.snapshot(|snapshot| {
            snapshot.url = url.clone();
            snapshot.loading = true;
            snapshot.can_go_back = back;
            snapshot.can_go_forward = forward;
        });
        self.pending.insert(url.clone(), SubresourceKind::Document);
        self.request_subresources(&[(url, SubresourceKind::Document)]);
    }

    /// `(can_go_back, can_go_forward)` for the current history position.
    fn history_state(&self) -> (bool, bool) {
        (
            self.history_pos > 0,
            self.history_pos + 1 < self.history.len(),
        )
    }

    /// Moves in the session history by `delta` (−1 back, +1 forward).
    fn go_history(&mut self, delta: i64) {
        let target = self.history_pos as i64 + delta;
        if target < 0 || target >= self.history.len() as i64 {
            return;
        }
        self.history_pos = target as usize;
        let url = self.history[target as usize].clone();
        self.start_navigation(url, false);
    }

    /// Reloads the current document (history position unchanged).
    fn reload(&mut self) {
        if !self.url.is_empty() {
            self.start_navigation(self.url.clone(), false);
        }
    }

    /// Cancels the navigation in flight: pending subresources are dropped so
    /// late responses no longer apply.
    fn stop_loading(&mut self) {
        self.navigating = false;
        self.pending.clear();
        self.snapshot(|snapshot| snapshot.loading = false);
    }

    /// Evaluates JavaScript in the page runtime and reports the result.
    fn eval_js(&mut self, code: String) {
        let (ok, result) = match &self.js {
            Some(js) => match js.eval(&code, "devtools-console.js") {
                Ok(value) => (true, value),
                Err(err) => (false, err.to_string()),
            },
            None => (false, "no JavaScript runtime on this page".to_owned()),
        };
        let _ = self.state.event_tx.send(EngineEvent::JsResult {
            tab: self.state.tab,
            ok,
            result,
        });
    }

    /// Sets the find query, recomputes matches and repaints with highlights.
    fn find_set(&mut self, query: &str) {
        self.find_query = query.to_owned();
        self.find_matches.clear();
        self.active_match = None;
        if !query.is_empty() {
            if let Some(layout) = self.layout.clone() {
                if let Some(dom) = self.dom.clone() {
                    let dom = dom.borrow();
                    let needle = query.to_lowercase();
                    for run in &layout.text {
                        let hay = dom.text_content(run.node).to_lowercase();
                        let mut start = 0;
                        while let Some(found) = hay[start..].find(&needle) {
                            let byte = start + found;
                            start = byte + needle.len();
                            if let Some(lr) = layout.rects.get(&run.node).copied() {
                                let rect = rowser_rendering::Rect {
                                    x: lr.x,
                                    y: lr.y,
                                    w: lr.w,
                                    h: (lr.h / 8.0).max(18.0).min(lr.h),
                                };
                                let rect = rowser_rendering::Rect {
                                    y: rect.y
                                        + (byte as f32 / hay.len().max(1) as f32) * lr.h.max(1.0),
                                    ..rect
                                };
                                self.find_matches.push(rect);
                            }
                        }
                    }
                }
            }
        }
        self.active_match = (!self.find_matches.is_empty()).then_some(0);
        if self.active_match.is_some() {
            self.scroll_to_match();
        }
        self.dirty = true;
        self.repaint();
        let _ = self.state.event_tx.send(EngineEvent::FindResult {
            tab: self.state.tab,
            matches: self.find_matches.len(),
            active: self.active_match,
        });
    }

    /// Steps the active find match and scrolls to it.
    fn find_step(&mut self, delta: i32) {
        if self.find_query.is_empty() || self.find_matches.is_empty() {
            return;
        }
        let current = self.active_match.unwrap_or(0) as i64;
        let next = (current + delta as i64).rem_euclid(self.find_matches.len() as i64) as usize;
        self.active_match = Some(next);
        self.scroll_to_match();
        self.dirty = true;
        self.repaint();
        let _ = self.state.event_tx.send(EngineEvent::FindResult {
            tab: self.state.tab,
            matches: self.find_matches.len(),
            active: self.active_match,
        });
    }

    /// Scrolls so the active match is comfortably in view.
    fn scroll_to_match(&mut self) {
        if let (Some(active), Some(layout)) = (self.active_match, self.layout.as_ref()) {
            if let Some(rect) = self.find_matches.get(active) {
                let max = (layout.content_size.1 - self.viewport.height).max(0.0);
                self.scroll_y = (rect.y - self.viewport.height / 3.0).clamp(0.0, max);
            }
        }
    }

    /// Saves the current DOM as an HTML file.
    fn save_page(&mut self, path: std::path::PathBuf) {
        let html = self
            .dom
            .as_ref()
            .map(|dom| serialize_dom(&dom.borrow()))
            .unwrap_or_default();
        let saved = std::fs::write(&path, html).is_ok();
        if saved {
            let _ = self.state.event_tx.send(EngineEvent::PageSaved {
                tab: self.state.tab,
                path: path.display().to_string(),
            });
        } else {
            let _ = self.state.event_tx.send(EngineEvent::ConsoleMessage {
                tab: self.state.tab,
                level: "error".to_owned(),
                text: format!("save page: cannot write {}", path.display()),
            });
        }
    }

    /// Clicks at a document-space point: follows links, otherwise dispatches
    /// a DOM click event on the hit element.
    fn click_at(&mut self, x: f32, y: f32) {
        let node = self.hit_node(x, y);
        let Some(node) = node else { return };
        // Native media controls: elements with the controls attribute own
        // the bottom strip (play / seek / mute).
        if self.media_controls_click(node, x, y) {
            return;
        }
        if let Some(dom) = self.dom.as_ref() {
            let dom = dom.borrow();
            if let Some((href, _)) = ancestor_link(&dom, node) {
                let href = self.resolve_url(&href);
                drop(dom);
                self.start_navigation(href, true);
                return;
            }
        }
        if let Some(js) = &self.js {
            js.dispatch(rowser_js::EngineEvent::DomEvent {
                node: node as u64,
                event_type: "click".to_owned(),
            });
            self.mark_if_dirty();
        }
    }

    /// Native media controls hit-testing for elements with the `controls`
    /// attribute. The bottom strip is consumed (play / mute / seek); a body
    /// click toggles playback and still propagates to page JS.
    fn media_controls_click(&mut self, node: NodeId, x: f32, y: f32) -> bool {
        let node_u = node as u64;
        let has_controls = self
            .dom
            .as_ref()
            .map(|dom| {
                let dom = dom.borrow();
                dom.element(node)
                    .map(|el| matches!(&*el.name.local, "video" | "audio"))
                    .unwrap_or(false)
                    && dom.get_attr(node, "controls").is_some()
            })
            .unwrap_or(false);
        if !has_controls || !self.media_slots.contains_key(&node_u) {
            return false;
        }
        let Some(rect) = self
            .layout
            .as_ref()
            .and_then(|layout| layout.rects.get(&node).copied())
        else {
            return false;
        };
        let bar_h = (rect.h * 0.16).clamp(26.0, 40.0);
        let in_bar = y >= rect.y + rect.h - bar_h
            && y <= rect.y + rect.h
            && x >= rect.x
            && x <= rect.x + rect.w;
        // Actions are computed under a shared borrow, applied after.
        enum Action {
            None,
            Toggle,
            MuteToggle,
            Seek(f64),
        }
        let action = {
            let Some(slot) = self.media_slots.get(&node_u) else {
                return false;
            };
            let info = slot.pipeline.info();
            if in_bar {
                let local = x - rect.x;
                if local < 44.0 {
                    Action::Toggle
                } else if local > rect.w - 44.0 {
                    Action::MuteToggle
                } else if info.duration > 0.0 && info.duration.is_finite() {
                    let track_w = (rect.w - 88.0).max(1.0);
                    let frac = ((local - 44.0) / track_w).clamp(0.0, 1.0);
                    Action::Seek(f64::from(frac) * info.duration)
                } else {
                    Action::None
                }
            } else {
                Action::Toggle
            }
        };
        match action {
            Action::None => {}
            Action::Toggle => {
                let playing = self
                    .media_slots
                    .get(&node_u)
                    .map(|slot| !slot.pipeline.is_paused())
                    .unwrap_or(false);
                self.media_play(node_u, !playing);
            }
            Action::MuteToggle => {
                let muted = self
                    .media_slots
                    .get(&node_u)
                    .map(|slot| slot.muted)
                    .unwrap_or(false);
                self.media_muted(node_u, !muted);
            }
            Action::Seek(time) => {
                // Forward-only in v1: the pipeline clamps backward seeks
                // (backward seek needs sample-table re-streaming).
                if let Some(slot) = self.media_slots.get(&node_u) {
                    slot.pipeline.seek(time);
                }
            }
        }
        if in_bar {
            self.dirty = true;
            return true; // consumed: no JS click for the control strip
        }
        false // body click: toggle applied, JS click still dispatches
    }

    /// Reports what is under a document-space point (hover/status bar).
    fn hit_test(&mut self, x: f32, y: f32) {
        let node = self.hit_node(x, y);
        let mut tag = String::new();
        let mut href = None;
        let mut text = None;
        let mut node_id = 0u64;
        if let Some(node) = node {
            if let Some(dom) = self.dom.as_ref() {
                let dom = dom.borrow();
                if let Some(element) = dom.element(node) {
                    node_id = node as u64;
                    tag = element.local_name().to_string();
                    if let Some((link_href, link_text)) = ancestor_link(&dom, node) {
                        href = Some(link_href);
                        text = Some(link_text);
                    }
                }
            }
        }
        let _ = self.state.event_tx.send(EngineEvent::HitTestResult {
            tab: self.state.tab,
            node: node_id,
            tag,
            href,
            text,
        });
    }

    /// Deepest element whose layout rect contains the point.
    fn hit_node(&self, x: f32, y: f32) -> Option<rowser_dom::NodeId> {
        let layout = self.layout.as_ref()?;
        let dom = self.dom.as_ref()?;
        let dom = dom.borrow();
        let mut best: Option<(rowser_dom::NodeId, f32, usize)> = None;
        for (node, rect) in &layout.rects {
            let contains =
                x >= rect.x && x <= rect.x + rect.w && y >= rect.y && y <= rect.y + rect.h;
            if !contains {
                continue;
            }
            if dom.element(*node).is_none() {
                continue;
            }
            // "Deepest": prefer the smallest rect, tie-broken by tree depth.
            let area = rect.w * rect.h;
            let mut depth = 0usize;
            let mut walk = dom.parent(*node);
            while let Some(up) = walk {
                depth += 1;
                walk = dom.parent(up);
            }
            match best {
                Some((_, best_area, best_depth)) => {
                    if area < best_area || (area == best_area && depth > best_depth) {
                        best = Some((*node, area, depth));
                    }
                }
                None => best = Some((*node, area, depth)),
            }
        }
        best.map(|(node, _, _)| node)
    }

    fn reset_page(&mut self) {
        self.find_matches.clear();
        self.active_match = None;
        self.find_query.clear();
        for slot in self.media_slots.values() {
            slot.pipeline.close();
        }
        self.media_slots.clear();
        self.mse_sources.clear();
        self.sb_lanes.clear();
        self.video_frames.clear();
        self.media_mirror.borrow_mut().clear();
        self.js = None;
        self.workers.clear();
        self.dom = None;
        self.document = None;
        self.style_map = None;
        self.layout = None;
        self.display_list = None;
        self.images.clear();
        self.css_texts.clear();
        self.scripts.clear();
        self.executed_scripts.clear();
        self.pending.clear();
        self.dirty = false;
        self.scroll_y = 0.0;
    }

    fn request_subresources(&self, requests: &[(String, SubresourceKind)]) {
        let fetch_requests: Vec<rowser_networking::FetchRequest> = requests
            .iter()
            .map(|(url, kind)| rowser_networking::FetchRequest {
                url: url.clone(),
                resource_type: kind.to_resource_kind(),
                source_url: self.url.clone(),
                top_site: self.top_site(),
                ..rowser_networking::FetchRequest::default()
            })
            .collect();
        if fetch_requests.is_empty() {
            return;
        }
        let _ = self
            .engine_tx
            .send(Cmd::Internal(Internal::FetchSubresources {
                tab: self.state.tab,
                requests: fetch_requests,
            }));
    }

    fn top_site(&self) -> Option<String> {
        url::Url::parse(&self.url)
            .ok()
            .and_then(|url| url.host_str().map(rowser_privacy::psl_registrable))
            .map(|site| site.to_string())
            .or_else(|| Some(self.url.clone()))
    }

    fn subresource_fetched(&mut self, url: String, body: Vec<u8>, headers: &str) {
        let Some(kind) = self.pending.remove(&url) else {
            return;
        };
        match kind {
            SubresourceKind::Document => {
                // Content-type backstop: an extension-less media URL still
                // lands in the viewer (the fetched bytes are dropped; the
                // pipeline re-streams in ranged chunks).
                if headers_look_like_media(headers) {
                    self.media_viewer_document(url);
                } else {
                    self.document_fetched(url, body);
                }
            }
            SubresourceKind::Stylesheet => {
                let text = String::from_utf8_lossy(&body).into_owned();
                self.css_texts.push(text);
            }
            SubresourceKind::Script => {
                let text = String::from_utf8_lossy(&body).into_owned();
                // The document pass already inserted an empty placeholder
                // for this src; REPLACE it in place. Appending a second
                // entry with the same name made the run-once dedupe pick
                // the empty placeholder and never execute the real body.
                let placeholder = self
                    .scripts
                    .iter_mut()
                    .find(|(src, code, _)| src.as_deref() == Some(url.as_str()) && code.is_empty());
                match placeholder {
                    Some(entry) => entry.1 = text,
                    None => self.scripts.push((Some(url), text, 0)),
                }
            }
            SubresourceKind::Image => {
                if let Some(image) = DecodedImage::decode(&body) {
                    // Attach to the img node that referenced it.
                    if let Some(dom) = &self.dom {
                        let dom = dom.borrow();
                        for node in dom.subtree_elements(dom.document()) {
                            if let Some(el) = dom.element(node) {
                                if &*el.name.local == "img"
                                    && dom.get_attr(node, "src") == Some(url.as_str())
                                {
                                    self.images.insert(node, Arc::new(image));
                                    break;
                                }
                            }
                        }
                    }
                    self.dirty = true;
                }
            }
            SubresourceKind::Media => {
                // Media never uses the buffered subresource path (it would
                // hold whole videos in memory); the streaming loader feeds
                // Message::MediaData instead. A late completion here means
                // the navigation moved on: drop the bytes.
            }
        }
    }

    fn document_fetched(&mut self, url: String, body: Vec<u8>) {
        let trace = std::env::var("ROWSER_UI_TRACE").is_ok();
        if trace {
            eprintln!(
                "[page-{}] document_fetched start ({} bytes)",
                self.state.tab,
                body.len()
            );
        }
        let mut document = parse_html(&body);
        document.url = Some(url);
        let dom = Rc::new(RefCell::new(std::mem::take(&mut document.dom)));
        self.document = Some(document);

        // Collect subresources: stylesheets, scripts, images.
        let dom_ref = dom.borrow();
        let mut requests: Vec<(String, SubresourceKind)> = Vec::new();
        let css_texts: Vec<String> = self.css_texts.clone();
        let mut scripts: Vec<(Option<String>, String, NodeId)> = Vec::new();
        for node in dom_ref.subtree_elements(dom_ref.document()) {
            let Some(element) = dom_ref.element(node) else {
                continue;
            };
            let tag = element.local_name().to_string();
            match tag.as_str() {
                "style" => {
                    // Style texts are collected per render from the live DOM
                    // (light tree + shadow roots) so JS-injected styles
                    // (Polymer, WebComponents) apply too.
                }
                "script" => {
                    if let Some(src) = dom_ref.get_attr(node, "src") {
                        let src = self.resolve_url(src);
                        requests.push((src.clone(), SubresourceKind::Script));
                        scripts.push((Some(src), String::new(), node));
                    } else {
                        scripts.push((None, dom_ref.text_content(node), node));
                    }
                }
                "link" => {
                    let rel = dom_ref.get_attr(node, "rel").unwrap_or_default();
                    if rel
                        .split_whitespace()
                        .any(|r| r.eq_ignore_ascii_case("stylesheet"))
                    {
                        if let Some(href) = dom_ref.get_attr(node, "href") {
                            let href = self.resolve_url(href);
                            requests.push((href, SubresourceKind::Stylesheet));
                        }
                    }
                }
                "img" => {
                    if let Some(src) = dom_ref.get_attr(node, "src") {
                        let src = self.resolve_url(src);
                        if src.starts_with("data:") {
                            if let Some(image) = rowser_rendering::DecodedImage::decode(
                                extract_data_payload(&src).as_bytes(),
                            ) {
                                self.images.insert(node, Arc::new(image));
                            }
                        } else {
                            requests.push((src, SubresourceKind::Image));
                        }
                    }
                }
                "video" | "audio" => {
                    let src = match dom_ref.get_attr(node, "src") {
                        Some(src) => Some(src.to_owned()),
                        None => dom_ref.flat_children(node).into_iter().find_map(|child| {
                            // <video><source src=...></video> without a
                            // direct src attribute.
                            dom_ref
                                .element(child)
                                .filter(|el| &*el.name.local == "source")
                                .and_then(|_| dom_ref.get_attr(child, "src"))
                                .map(str::to_owned)
                        }),
                    };
                    if let Some(src) = src {
                        let src = self.resolve_url(&src);
                        let autoplay = dom_ref.get_attr(node, "autoplay").is_some();
                        let muted = dom_ref.get_attr(node, "muted").is_some();
                        let looped = dom_ref.get_attr(node, "loop").is_some();
                        self.media_register(node, &src, autoplay, muted, looped);
                    }
                }
                _ => {}
            }
        }
        drop(dom_ref);
        self.css_texts = css_texts;
        self.scripts.extend(scripts);
        for (url, kind) in &requests {
            self.pending.insert(url.clone(), *kind);
        }
        self.dom = Some(dom);
        if requests.is_empty() {
            self.subresources_complete();
        } else {
            self.request_subresources(&requests);
        }
    }

    fn resolve_url(&self, href: &str) -> String {
        if href.starts_with("data:") || href.starts_with("about:") {
            return href.to_owned();
        }
        match url::Url::parse(href) {
            Ok(_) => href.to_owned(),
            Err(_) => url::Url::parse(&self.url)
                .and_then(|base| base.join(href))
                .map(|joined| joined.to_string())
                .unwrap_or_else(|_| href.to_owned()),
        }
    }

    fn subresources_complete(&mut self) {
        let trace = std::env::var("ROWSER_UI_TRACE").is_ok();
        if trace {
            eprintln!("[page-{}] subresources_complete → render", self.state.tab);
        }
        if self.dom.is_none() {
            // Internal (rowser://) or unreachable pages: end the load state.
            self.navigating = false;
            self.snapshot(|snapshot| snapshot.loading = false);
            return;
        }
        tracing::debug!(target: "rowser::engine", "tab {} subresources complete → render", self.state.tab);
        if trace {
            let t0 = std::time::Instant::now();
            self.render_pipeline();
            if trace {
                eprintln!(
                    "[page-{}] render_pipeline took {}ms",
                    self.state.tab,
                    t0.elapsed().as_millis()
                );
            }
        } else {
            self.render_pipeline();
        }
        tracing::debug!(target: "rowser::engine", "tab {} render done → scripts", self.state.tab);
        if trace {
            eprintln!("[page-{}] run_scripts start", self.state.tab);
        }
        self.run_scripts();
        // Lifecycle: fire DOMContentLoaded (scripts ready) then load — the
        // events every framework bootstraps on. Fired exactly once per
        // document (subresources_complete re-runs on late CSS, but the
        // runtime and listeners persist).
        if let Some(js) = &self.js {
            let _ = js.eval(
                "if (globalThis.__fireDocumentEvent && !globalThis.__domContentLoadedFired) {\
                 \x20globalThis.__domContentLoadedFired = true;\
                 \x20__fireDocumentEvent('DOMContentLoaded');\
                 \x20__fireDocumentEvent('load'); }",
                "lifecycle-events.js",
            );
        }
        tracing::debug!(target: "rowser::engine", "tab {} scripts done → PageLoaded", self.state.tab);
        self.navigating = false;
        let title = self
            .document
            .as_ref()
            .map(|doc| doc.title.clone())
            .unwrap_or_default();
        let url = self.url.clone();
        self.snapshot(|snapshot| {
            snapshot.loading = false;
            snapshot.title = title.clone();
        });
        let _ = self.state.event_tx.send(EngineEvent::PageLoaded {
            tab: self.state.tab,
            url,
            title,
        });
    }

    /// Style → layout → display list → paint.
    fn render_pipeline(&mut self) {
        tracing::debug!(target: "rowser::engine", "render pipeline start");
        let trace = std::env::var("ROWSER_UI_TRACE").is_ok();
        let Some(dom) = self.dom.clone() else { return };
        // Stylesheets — parsed once per (css set, media) combination.
        let media = MediaContext {
            width: self.viewport.width,
            height: self.viewport.height,
            dark_mode: false,
        };
        // Effective sheet texts: link-fetched sheets plus live <style>
        // elements from the light tree AND every shadow root (Polymer and
        // WebComponents inject styles at upgrade time — a parse-time-only
        // collection would never see them).
        let all_css: Vec<String> = {
            let dom_ref = dom.borrow();
            let mut all = self.css_texts.clone();
            let push_styles = |root: rowser_dom::NodeId, all: &mut Vec<String>| {
                for node in dom_ref.subtree_elements(root) {
                    if dom_ref
                        .element(node)
                        .is_some_and(|e| &*e.name.local == "style")
                    {
                        let t = dom_ref.text_content(node);
                        if !t.is_empty() {
                            all.push(t);
                        }
                    }
                }
            };
            push_styles(dom_ref.document(), &mut all);
            for root in dom_ref.all_shadow_roots() {
                push_styles(root, &mut all);
            }
            all
        };
        let fp = {
            use std::hash::{Hash, Hasher};
            let fp_t0 = std::time::Instant::now();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for css in all_css.iter() {
                css.len().hash(&mut h);
                css.hash(&mut h);
            }
            (media.width as u64).hash(&mut h);
            (media.height as u64).hash(&mut h);
            if trace {
                eprintln!(
                    "[page-{}] css fingerprint took {}ms ({} link sheets, {} live style elements)",
                    self.state.tab,
                    fp_t0.elapsed().as_millis(),
                    self.css_texts.len(),
                    all_css.len() - self.css_texts.len()
                );
            }
            h.finish()
        };
        let sheets: Vec<ParsedStylesheet> = if let Some((cached_fp, cached)) = self.css_cache.take()
        {
            if cached_fp == fp {
                cached
            } else {
                all_css
                    .iter()
                    .map(|css| parse_stylesheet(css, &media))
                    .collect()
            }
        } else {
            all_css
                .iter()
                .map(|css| parse_stylesheet(css, &media))
                .collect()
        };
        self.css_cache = Some((fp, sheets.clone()));
        let stage_t0 = std::time::Instant::now();
        if trace {
            eprintln!(
                "[page-{}] css parsed: {} sheets, {} total bytes",
                self.state.tab,
                sheets.len(),
                self.css_texts.iter().map(|c| c.len()).sum::<usize>()
            );
        }
        let (styles, layout) = {
            // Intrinsic video sizes: 300x150 default, real aspect once the
            // pipeline knows the dimensions (replaced-element layout).
            let mut intrinsic: HashMap<NodeId, (f32, f32)> = HashMap::new();
            for (node, slot) in &self.media_slots {
                let info = slot.pipeline.info();
                if info.width > 0 && info.height > 0 {
                    intrinsic.insert(
                        NodeId::try_from(*node).unwrap_or(0),
                        (info.width as f32, info.height as f32),
                    );
                }
            }
            let (styles, layout) =
                self.layout_engine.layout_document(&dom.borrow(), &sheets, &media, self.viewport, &intrinsic);
            // JS layout mirror refresh (getBoundingClientRect).
            {
                let mut rects = self.rect_mirror.borrow_mut();
                rects.clear();
                for (node, rect) in &layout.rects {
                    rects.insert(u64::from(*node), [rect.x, rect.y, rect.w, rect.h]);
                }
            }
            (styles, layout)
        };
        if trace {
            eprintln!(
                "[page-{}] layout took {}ms ({} dom nodes)",
                self.state.tab,
                stage_t0.elapsed().as_millis(),
                dom.borrow().node_count()
            );
        }
        let stage_t1 = std::time::Instant::now();
        let _ = stage_t1;
        self.style_map = Some(styles);
        self.layout = Some(layout);
        self.rendered_dom_version = dom.borrow().version;
        self.display_list = None;
        // NOTE: do NOT leave `dirty` set here. idle() clears dirty before
        // calling us; re-setting it made every tab re-render on every 250ms
        // idle tick forever (static pages, background tabs included). That
        // leak, multiplied across tabs, fed a FrameReady storm that
        // eventually wedged the UI thread (exponential event batching).
        self.dirty = false;
        self.repaint();
    }

    fn repaint(&mut self) {
        if self.suspended || self.dom.is_none() {
            return;
        }
        let trace = std::env::var("ROWSER_UI_TRACE").is_ok();
        let Some(layout) = self.layout.clone() else {
            return;
        };
        let Some(styles) = self.style_map.clone() else {
            return;
        };
        let Some(dom) = self.dom.clone() else { return };
        let dl_t0 = std::time::Instant::now();
        let mut media_overlays = rowser_rendering::display_list::MediaOverlays::new();
        for (node, slot) in &self.media_slots {
            let info = slot.pipeline.info();
            media_overlays.insert(
                NodeId::try_from(*node).unwrap_or(0),
                rowser_rendering::display_list::MediaOverlay {
                    time: slot.pipeline.current_time(),
                    duration: if info.duration.is_finite() {
                        info.duration
                    } else {
                        0.0
                    },
                    paused: slot.pipeline.is_paused(),
                    muted: slot.muted,
                },
            );
        }
        let list = build_display_list(
            &dom.borrow(),
            &styles,
            &layout,
            &self.images,
            &self.video_frames,
            &media_overlays,
        );
        if std::env::var("ROWSER_UI_TRACE").is_ok() {
            eprintln!(
                "[page-{}] display_list took {}ms ({} cmds)",
                self.state.tab,
                dl_t0.elapsed().as_millis(),
                list.commands.len()
            );
        }
        self.display_list = Some(list.clone());
        let background = page_background(&styles, &layout);
        let options = RenderOptions {
            viewport_width: self.viewport.width as u32,
            viewport_height: self.viewport.height as u32,
            scroll_y: self.scroll_y,
            background,
            find_matches: self.find_matches.clone(),
            active_match: self.active_match,
        };
        if let Some(frame) =
            self.painter
                .render(&list, options, &mut self.layout_engine.font_system)
        {
            if trace {
                eprintln!(
                    "[page-{}] painted frame id={} {}x{}",
                    self.state.tab, frame.id, frame.width, frame.height
                );
            }
            let frame = Arc::new(frame);
            let content_size = layout.content_size;
            self.state
                .snapshot_tx
                .set_frame(Some(Arc::clone(&frame)), content_size);
            let _ = self.state.event_tx.send(EngineEvent::FrameReady {
                tab: self.state.tab,
                frame: frame.id,
            });
        }
    }

    fn run_scripts(&mut self) {
        let Some(dom) = self.dom.clone() else { return };
        // ONE runtime per document. Re-creating it on every render round
        // destroyed all JS state and re-executed every script (double
        // bootstraps wedged JS-heavy sites like YouTube).
        if self.js.is_none() {
            let (body, html) = {
                let dom = dom.borrow();
                let find = |tag: &str| {
                    dom.subtree_elements(dom.document())
                        .find(|n| {
                            dom.element(*n)
                                .map(|e| &*e.name.local == tag)
                                .unwrap_or(false)
                        })
                        .unwrap_or(0)
                };
                (find("body"), find("html"))
            };
            let bridge = PageBridge {
                dom: Rc::clone(&dom),
                document: dom.borrow().document(),
                body,
                html,
                url: self.url.clone(),
                origin: url::Url::parse(&self.url)
                    .map(|u| u.origin().ascii_serialization())
                    .unwrap_or_default(),
                storage: Some(Arc::clone(&self.state.storage)),
                spoof: self.state.spoof.clone(),
                outgoing: Some(self.js_tx.clone()),
                media_mirror: Rc::clone(&self.media_mirror),
                rects: Rc::clone(&self.rect_mirror),
                viewport: Rc::clone(&self.viewport_mirror),
            };
            *self.viewport_mirror.borrow_mut() =
                (self.scroll_y, self.viewport.width, self.viewport.height);
            match JsRuntime::new(self.state.js_config.clone(), bridge) {
                Ok(runtime) => self.js = Some(runtime),
                Err(err) => {
                    tracing::warn!(target: "rowser::engine", "js init failed: {err}");
                    return;
                }
            }
        }
        let scripts = std::mem::take(&mut self.scripts);
        // Phase 1 (exclusive borrow): pick the not-yet-executed scripts.
        let mut to_run: Vec<(usize, String, String, NodeId)> = Vec::new();
        for (i, (src, code, node)) in scripts.iter().enumerate() {
            let name = src.clone().unwrap_or_else(|| format!("inline-{i}.js"));
            // External scripts arrive as empty placeholders and are filled
            // by subresource_fetched; executing the placeholder would burn
            // the run-once slot on an empty body, so wait for the bytes.
            if src.is_some() && code.is_empty() {
                continue;
            }
            // Chrome semantics: a script executes exactly once per document
            // load — never again on re-render.
            if self.executed_scripts.insert(name.clone()) {
                to_run.push((i, name, code.clone(), *node));
            }
        }
        // Phase 2 (shared borrow of the runtime): execute.
        if let Some(js) = &self.js {
            for (i, name, code, node) in &to_run {
                let t0 = std::time::Instant::now();
                if std::env::var("ROWSER_UI_TRACE").is_ok() {
                    eprintln!("[page-{}] script {} START {}", self.state.tab, i, name);
                }
                // document.currentScript during evaluation (loaders derive
                // their base URL from it).
                if *node != 0 {
                    let _ = js.eval(
                        &format!("globalThis.__setCurrentScript && __setCurrentScript({node})"),
                        "current-script.js",
                    );
                }
                if let Err(err) = js.eval(code, name) {
                    let _ = self.state.event_tx.send(EngineEvent::ConsoleMessage {
                        tab: self.state.tab,
                        level: "error".to_owned(),
                        text: format!("{name}: {err}"),
                    });
                }
                if *node != 0 {
                    let _ = js.eval(
                        "globalThis.__setCurrentScript && __setCurrentScript(0)",
                        "current-script-clear.js",
                    );
                }
                if std::env::var("ROWSER_UI_TRACE").is_ok()
                    && t0.elapsed() > std::time::Duration::from_millis(300)
                {
                    eprintln!(
                        "[page-{}] script {} ({}) took {}ms",
                        self.state.tab,
                        i,
                        name,
                        t0.elapsed().as_millis()
                    );
                }
            }
        }
        self.scripts = scripts;
        self.mark_if_dirty();
        if std::env::var("ROWSER_UI_TRACE").is_ok() {
            eprintln!("[page-{}] run_scripts done", self.state.tab);
        }
    }

    fn spawn_worker(&mut self, worker: u64, code: String) {
        let empty_dom = Rc::new(RefCell::new(Dom::new()));
        let bridge = PageBridge {
            dom: empty_dom,
            document: 0,
            body: 0,
            html: 0,
            url: self.url.clone(),
            origin: String::new(),
            storage: None,
            spoof: self.state.spoof.clone(),
            outgoing: Some(self.js_tx.clone()),
            media_mirror: Rc::new(RefCell::new(HashMap::new())),
            rects: Rc::new(RefCell::new(HashMap::new())),
            viewport: Rc::new(RefCell::new((0.0, 0.0, 0.0))),
        };
        if let Ok(runtime) = JsRuntime::new(JsConfig::default(), bridge) {
            runtime
                .set_worker_post_message(worker, self.js_tx.clone())
                .ok();
            let _ = runtime.eval(prelude::WORKER_PRELUDE_JS, "worker-prelude.js");
            let _ = runtime.eval(&code, "worker.js");
            self.workers.insert(worker, runtime);
        }
    }

    // ------------------------------------------------------------------
    // Media: HTMLMediaElement + MSE wiring. The decode pipeline lives in
    // the `rowser-media` crate; the page thread owns the handles, routes
    // bytes, blits frames and mirrors state into JS.
    // ------------------------------------------------------------------

    /// Opens a pipeline for a media element and asks the engine to stream
    /// its source (unless it is an MSE object URL).
    fn media_register(
        &mut self,
        node: NodeId,
        src: &str,
        autoplay: bool,
        muted: bool,
        looped: bool,
    ) {
        let node_u = node as u64;
        if self.media_slots.contains_key(&node_u) || src.is_empty() {
            return;
        }
        if src.starts_with("rowser-mse:") {
            // MSE attach happens through mse_attach once bytes arrive.
            let ms_id: u64 = src
                .strip_prefix("rowser-mse:")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if ms_id > 0 {
                self.mse_attach(ms_id, node, autoplay, muted, looped);
            }
            return;
        }
        let (pipeline, ingress) = rowser_media::open_pipeline(self.media_notify(node_u));
        self.media_slots.insert(
            node_u,
            MediaSlot {
                pipeline,
                ingress,
                autoplay,
                loop_playback: looped,
                started: false,
                muted: false,
            },
        );
        self.media_muted(node_u, muted);
        let _ = self.engine_tx.send(Cmd::Internal(Internal::MediaLoad {
            tab: self.state.tab,
            node: node_u,
            url: src.to_owned(),
        }));
    }

    /// Renders the built-in media viewer document for a top-level media
    /// navigation: a black page hosting `<video|audio controls autoplay>`,
    /// which engages the streaming pipeline exactly like site-embedded
    /// media (ranged chunks or the playlist walker).
    fn media_viewer_document(&mut self, url: String) {
        self.pending.remove(&url);
        let name = url
            .rsplit('/')
            .next()
            .and_then(|n| n.split(['?', '#']).next())
            .unwrap_or("media");
        let lower = name.to_ascii_lowercase();
        let audio = [".mp3", ".m4a", ".aac", ".ogg", ".opus", ".flac"]
            .iter()
            .any(|ext| lower.ends_with(ext));
        let title = html_escape(name);
        let src = html_escape(&url);
        // Viewport-baked pixel width + aspect-ratio height: the reliable
        // path in our taffy 0.14 (percent-width + aspect collapses — see
        // layout test notes). The page is ours, so baking the viewport
        // width is legitimate.
        let video_width = self.viewport.width.max(1.0) as i32;
        let html = if audio {
            format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
                 <style>html,body{{margin:0;background:#141418}}\
                 h1{{color:#ddd;font-family:sans-serif;font-size:18px;font-weight:normal}}</style></head>\
                 <body style=\"display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:640px\">\
                 <h1>{title}</h1><audio src=\"{src}\" controls autoplay style=\"width:800px;height:48px\"></audio></body></html>"
            )
        } else {
            format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
                 <style>html,body{{margin:0;background:#000}}\
                 video{{background:#000}}</style></head>\
                 <body><video src=\"{src}\" controls autoplay style=\"width:{video_width}px;height:auto\"></video></body></html>"
            )
        };
        self.document_fetched(url, html.into_bytes());
    }

    /// Notification closure handed to a pipeline worker: routes frame-ready
    /// and media events back into this page thread via the engine loop.
    fn media_notify(&self, node: u64) -> Arc<dyn Fn(MediaNotification) + Send + Sync> {
        let engine_tx = self.engine_tx.clone();
        let tab = self.state.tab;
        Arc::new(move |notification| match notification {
            MediaNotification::FrameReady => {
                let _ = engine_tx.send(Cmd::Internal(Internal::PageMessage {
                    tab,
                    message: Box::new(Message::MediaFrameReady { node }),
                }));
            }
            MediaNotification::Event(event) => {
                let (event, detail) = media_event_parts(&event);
                let _ = engine_tx.send(Cmd::Internal(Internal::PageMessage {
                    tab,
                    message: Box::new(Message::MediaEngineEvent {
                        node,
                        event,
                        detail,
                    }),
                }));
            }
        })
    }

    /// JS set `video.src`.
    fn media_set_src(&mut self, node: u64, url: &str) {
        let url = url.to_owned();
        let (autoplay, muted, looped) = self
            .dom
            .as_ref()
            .and_then(|dom| {
                let dom = dom.borrow();
                dom.element(node as NodeId).map(|_el| {
                    (
                        dom.get_attr(node as NodeId, "autoplay").is_some(),
                        dom.get_attr(node as NodeId, "muted").is_some(),
                        dom.get_attr(node as NodeId, "loop").is_some(),
                    )
                })
            })
            .unwrap_or((false, false, false));
        self.media_register(node as NodeId, &url, autoplay, muted, looped);
    }

    /// play()/pause() from JS. `play` true also starts autoplay tracking.
    fn media_play(&mut self, node: u64, play: bool) {
        if let Some(slot) = self.media_slots.get_mut(&node) {
            if play {
                slot.started = true;
                slot.pipeline.play();
            } else {
                slot.pipeline.pause();
            }
        }
        if let Some(mirror) = self.media_mirror.borrow_mut().get_mut(&node) {
            mirror.paused = !play;
        }
        self.sync_media_active();
    }

    /// Direct-source bytes arriving from the engine's streaming fetch.
    fn media_data(&mut self, node: u64, data: &[u8]) {
        if let Some(slot) = self.media_slots.get_mut(&node) {
            slot.ingress.push(0, data.to_vec());
            if slot.autoplay && !slot.started {
                slot.started = true;
                slot.pipeline.play();
            }
        }
    }

    /// Direct-source stream finished.
    fn media_eof(&mut self, node: u64) {
        if let Some(slot) = self.media_slots.get(&node) {
            slot.ingress.close_lane(0);
        }
    }

    /// A decoded video frame is presentable: blit it into the frame map.
    fn media_frame_ready(&mut self, node: u64) {
        let frame = self
            .media_slots
            .get(&node)
            .and_then(|slot| slot.pipeline.latest_frame());
        if std::env::var("ROWSER_MEDIA_TRACE").is_ok() {
            eprintln!(
                "[page-{}] media_frame_ready node={} have={}",
                self.state.tab,
                node,
                frame.is_some()
            );
        }
        if let Some(image) = frame {
            let image = DecodedImage {
                width: image.width,
                height: image.height,
                rgba: Arc::clone(&image.rgba),
            };
            self.video_frames.insert(node as NodeId, Arc::new(image));
            self.dirty = true;
            self.repaint();
        }
    }

    /// A pipeline event: refresh the JS mirror and dispatch the DOM event.
    fn media_engine_event(&mut self, node: u64, event: &str, detail: &str) {
        let Some(slot) = self.media_slots.get(&node) else {
            return;
        };
        // loadedmetadata changes intrinsic size (layout resize);
        // timeupdate drives the native controls' progress bar (audio has
        // no frame pump to repaint it).
        if event == "loadedmetadata"
            || (event == "timeupdate"
                && self
                    .dom
                    .as_ref()
                    .map(|dom| {
                        dom.borrow()
                            .get_attr(node as NodeId, "controls")
                            .is_some()
                    })
                    .unwrap_or(false))
        {
            self.dirty = true;
        }
        let info = slot.pipeline.info();
        let playing = !slot.pipeline.is_paused();
        {
            let mut mirror = self.media_mirror.borrow_mut();
            let entry = mirror.entry(node).or_default();
            entry.duration = info.duration;
            entry.width = info.width;
            entry.height = info.height;
            entry.paused = !playing;
            entry.error = slot.pipeline.error();
            match event {
                "timeupdate" => {
                    entry.time = detail
                        .strip_prefix("{\"time\":")
                        .and_then(|rest| rest.trim_end_matches('}').parse().ok())
                        .unwrap_or(entry.time);
                    if entry.ready_state < 1 {
                        entry.ready_state = 1;
                    }
                }
                "loadedmetadata" => {
                    if entry.ready_state < 1 {
                        entry.ready_state = 1;
                    }
                }
                "canplay" => entry.ready_state = 4,
                _ => {}
            }
            entry.buffered_end = slot.pipeline.buffered_end(0);
        }
        if let Some(js) = &self.js {
            js.dispatch(JsEngineEvent::MediaEvent {
                node,
                event_type: event.to_owned(),
                detail: detail.to_owned(),
            });
        }
        // Autoplay + loop handling.
        if event == "ended" {
            if std::env::var("ROWSER_MEDIA_TRACE").is_ok() {
                eprintln!(
                    "[page-{}] media ended node={} (loop check)",
                    self.state.tab, node
                );
            }
            // loop attribute: the byte stream is fully consumed by now, so
            // replay means re-streaming the source into a fresh pipeline.
            let should_loop = self
                .media_slots
                .get(&node)
                .map(|slot| slot.loop_playback)
                .unwrap_or(false);
            if should_loop {
                let src = self.dom.as_ref().and_then(|dom| {
                    let dom = dom.borrow();
                    dom.element(node as NodeId)
                        .and_then(|_| dom.get_attr(node as NodeId, "src"))
                        .map(|s| self.resolve_url(s))
                });
                if let Some(src) = src {
                    if let Some(slot) = self.media_slots.remove(&node) {
                        slot.pipeline.close();
                    }
                    self.video_frames.remove(&(node as NodeId));
                    // autoplay=true: the fresh stream starts playing as soon
                    // as its first bytes arrive (a loop never waits).
                    self.media_register(node as NodeId, &src, true, false, true);
                }
            }
        }
        if event == "ended" || event == "error" {
            self.sync_media_active();
        }
        self.mark_if_dirty();
    }

    /// Attaches an MSE MediaSource to a media element: creates the pipeline
    /// and flushes appended backlogs into per-SourceBuffer lanes.
    fn mse_attach(&mut self, ms_id: u64, node: NodeId, autoplay: bool, muted: bool, looped: bool) {
        let node_u = node as u64;
        if !self.mse_sources.contains_key(&ms_id) {
            return;
        }
        if !self.media_slots.contains_key(&node_u) {
            let (pipeline, ingress) = rowser_media::open_pipeline(self.media_notify(node_u));
            self.media_slots.insert(
                node_u,
                MediaSlot {
                    pipeline,
                    ingress,
                    autoplay,
                    loop_playback: looped,
                    started: false,
                    muted: false,
                },
            );
            self.media_muted(node_u, muted);
        }
        let mut flushes: Vec<(u64, Vec<u8>)> = Vec::new();
        if let Some(source) = self.mse_sources.get_mut(&ms_id) {
            source.attached_node = Some(node_u);
            for sb_id in source.source_buffers.keys() {
                if let Some(backlog) = source.backlog.get_mut(sb_id) {
                    if !backlog.is_empty() {
                        flushes.push((*sb_id, std::mem::take(backlog)));
                    }
                }
            }
        }
        for (sb_id, bytes) in flushes {
            self.sb_lanes.insert(sb_id, (node_u, sb_id));
            if let Some(slot) = self.media_slots.get(&node_u) {
                slot.ingress.push(sb_id, bytes);
            }
        }
        // Fire sourceopen so players start their append loops.
        if let Some(js) = &self.js {
            js.dispatch(JsEngineEvent::MediaSourceEvent {
                ms_id,
                sb_id: None,
                event_type: "sourceopen".to_owned(),
            });
        }
    }

    /// appendBuffer bytes: route to the attached pipeline lane or park in
    /// the backlog until the MediaSource is attached to an element.
    fn mse_append(&mut self, sb_id: u64, bytes: &[u8]) {
        if let Some((node, lane)) = self.sb_lanes.get(&sb_id).copied() {
            if let Some(slot) = self.media_slots.get(&node) {
                slot.ingress.push(lane, bytes.to_vec());
            }
            return;
        }
        // Not routed yet. A SourceBuffer created AFTER the MediaSource was
        // attached to an element routes immediately; otherwise it parks in
        // the backlog until attach.
        for source in self.mse_sources.values_mut() {
            if source.source_buffers.contains_key(&sb_id) {
                if let Some(node) = source.attached_node {
                    self.sb_lanes.insert(sb_id, (node, sb_id));
                    if let Some(slot) = self.media_slots.get(&node) {
                        slot.ingress.push(sb_id, bytes.to_vec());
                        return;
                    }
                }
                source
                    .backlog
                    .entry(sb_id)
                    .or_default()
                    .extend_from_slice(bytes);
                return;
            }
        }
    }

    fn media_muted(&mut self, node: u64, muted: bool) {
        if let Some(slot) = self.media_slots.get_mut(&node) {
            slot.muted = muted;
            slot.pipeline.set_muted(muted);
        }
    }

    /// Reports playing-media state to the engine (suspension override:
    /// a tab playing audio/video is never auto-frozen).
    fn sync_media_active(&mut self) {
        let active = self
            .media_slots
            .values()
            .any(|slot| !slot.pipeline.is_paused());
        if active != self.media_active {
            self.media_active = active;
            let _ = self.engine_tx.send(Cmd::Internal(Internal::MediaActive {
                tab: self.state.tab,
                active,
            }));
        }
    }

    fn mark_if_dirty(&mut self) {
        if let Some(dom) = &self.dom {
            let version = dom.borrow().version;
            if version != self.rendered_dom_version {
                self.dirty = true;
            }
        }
    }

    fn idle(&mut self) {
        // Resume any microtasks left queued by a bounded pump_jobs() yield.
        if let Some(js) = &self.js {
            js.pump_jobs();
        }
        if self.dirty && !self.suspended && !self.navigating && self.dom.is_some() {
            self.dirty = false;
            self.render_pipeline();
        }
        if self.last_memory_report.elapsed() > Duration::from_secs(5) {
            self.last_memory_report = std::time::Instant::now();
            let bytes = self.estimate_memory();
            self.snapshot(|snapshot| snapshot.memory_bytes = bytes);
            let _ = self.engine_tx.send(Cmd::Internal(Internal::MemoryReport {
                tab: self.state.tab,
                bytes,
            }));
        }
    }

    fn estimate_memory(&self) -> u64 {
        let dom_bytes = self
            .dom
            .as_ref()
            .map(|dom| (dom.borrow().node_count() as u64) * 220)
            .unwrap_or(0);
        let js_bytes = self
            .js
            .as_ref()
            .map(|j| j.memory_usage().max(0) as u64)
            .unwrap_or(0);
        let worker_bytes: i64 = self.workers.values().map(|w| w.memory_usage()).sum();
        let frame_bytes = self
            .display_list
            .as_ref()
            .map(|_| self.viewport.width as u64 * self.viewport.height as u64 * 4)
            .unwrap_or(0);
        let image_bytes = self
            .images
            .values()
            .map(|image| image.memory_usage() as u64)
            .sum::<u64>();
        let video_bytes = self
            .video_frames
            .values()
            .map(|image| image.memory_usage() as u64)
            .sum::<u64>();
        dom_bytes + js_bytes + worker_bytes.max(0) as u64 + frame_bytes + image_bytes + video_bytes
    }

    fn snapshot(&self, f: impl FnOnce(&mut TabSnapshot)) {
        self.state.snapshot_tx.update(f);
    }

    fn shutdown_report(&self) {
        let _ = self
            .engine_tx
            .send(Cmd::Internal(Internal::PageExited(self.state.tab)));
    }
}

/// Escapes a string for embedding in an HTML attribute / text node.
fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// True when response headers describe a media payload (content-type
/// backstop for extension-less media URLs).
fn headers_look_like_media(headers: &str) -> bool {
    let lower = headers.to_ascii_lowercase();
    lower.contains("video/")
        || lower.contains("audio/")
        || lower.contains("mpegurl")
        || lower.contains("mp2t")
        || lower.contains("dash+xml")
}

/// Formats a pipeline event as (DOM event name, JSON detail).
fn media_event_parts(event: &PipelineEvent) -> (String, String) {
    match event {
        PipelineEvent::LoadedMetadata {
            duration,
            width,
            height,
            has_video,
            has_audio,
        } => (
            "loadedmetadata".to_owned(),
            format!(
                "{{\"duration\":{duration},\"width\":{width},\"height\":{height},\"hasVideo\":{has_video},\"hasAudio\":{has_audio}}}"
            ),
        ),
        PipelineEvent::CanPlay => ("canplay".to_owned(), "{}".to_owned()),
        PipelineEvent::Playing => ("play".to_owned(), "{}".to_owned()),
        PipelineEvent::Paused => ("pause".to_owned(), "{}".to_owned()),
        PipelineEvent::Ended => ("ended".to_owned(), "{}".to_owned()),
        PipelineEvent::TimeUpdate { time } => (
            "timeupdate".to_owned(),
            format!("{{\"time\":{time}}}"),
        ),
        PipelineEvent::Error(message) => (
            "error".to_owned(),
            format!("{{\"message\":{}}}", serde_json::to_string(message).unwrap_or_default()),
        ),
    }
}

/// Walks up from `node` looking for the nearest anchor with an href;
/// returns `(href, anchor text)`.
fn ancestor_link(dom: &Dom, node: rowser_dom::NodeId) -> Option<(String, String)> {
    let mut current = Some(node);
    while let Some(id) = current {
        if let Some(element) = dom.element(id) {
            if &*element.name.local == "a" {
                if let Some(href) = dom.get_attr(id, "href") {
                    let text = dom.text_content(id);
                    return Some((href.to_owned(), text));
                }
            }
        }
        current = dom.parent(id);
    }
    None
}

/// Serializes the DOM back to HTML5.
fn serialize_dom(dom: &Dom) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn walk(dom: &Dom, id: rowser_dom::NodeId, out: &mut String) {
        match dom.kind(id) {
            rowser_dom::NodeKind::Text(text) => out.push_str(&escape(text)),
            rowser_dom::NodeKind::Element(_) => {
                let Some(element) = dom.element(id) else {
                    return;
                };
                let tag = element.local_name().to_string();
                out.push('<');
                out.push_str(&tag);
                for attr in &element.attrs {
                    out.push(' ');
                    out.push_str(&attr.name);
                    out.push_str("=\"");
                    out.push_str(&escape(&attr.value));
                    out.push('\"');
                }
                out.push('>');
                let mut child = dom.first_child(id);
                while let Some(child_id) = child {
                    walk(dom, child_id, out);
                    child = dom.next_sibling(child_id);
                }
                out.push_str("</");
                out.push_str(&tag);
                out.push('>');
            }
            rowser_dom::NodeKind::Document => {
                let mut child = dom.first_child(id);
                while let Some(child_id) = child {
                    walk(dom, child_id, out);
                    child = dom.next_sibling(child_id);
                }
            }
            rowser_dom::NodeKind::Doctype { .. } => out.push_str("<!DOCTYPE html>"),
            rowser_dom::NodeKind::Comment(_) => {}
        }
    }
    let mut html = String::from("<!DOCTYPE html>\n");
    walk(dom, dom.document(), &mut html);
    html
}

fn page_background(styles: &StyleMap, layout: &LayoutResult) -> rowser_parsing::cascade::Rgba {
    // Find the body element's background; default to white.
    for (node, style) in styles.styles.iter() {
        if style.background_color.a > 0 && layout.rects.contains_key(node) {
            let _ = node;
            return style.background_color;
        }
    }
    rowser_parsing::cascade::Rgba::new_opaque(255, 255, 255)
}

fn extract_data_payload(data_url: &str) -> String {
    let rest = data_url.strip_prefix("data:").unwrap_or("");
    match rest.split_once(',') {
        Some((meta, payload)) => {
            if meta.to_ascii_lowercase().ends_with(";base64") {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default()
            } else {
                percent_encoding::percent_decode_str(payload)
                    .decode_utf8()
                    .map(|cow| cow.into_owned())
                    .unwrap_or_default()
            }
        }
        None => String::new(),
    }
}
