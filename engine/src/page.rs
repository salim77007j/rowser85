//! The page actor: one OS thread per tab, owning the DOM, JS runtime,
//! styles, layout and painter.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use rowser_dom::Dom;
use rowser_js::{
    prelude, EngineEvent as JsEngineEvent, JsCommand, JsConfig, JsRuntime, PageBridge,
};
use rowser_layout::{LayoutEngine, LayoutResult, Viewport};
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
    /// Terminate the page thread.
    Shutdown,
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
}

impl SubresourceKind {
    /// Maps to the networking resource classification.
    pub fn to_resource_kind(self) -> rowser_networking::ResourceKind {
        match self {
            SubresourceKind::Document => rowser_networking::ResourceKind::Document,
            SubresourceKind::Stylesheet => rowser_networking::ResourceKind::Stylesheet,
            SubresourceKind::Script => rowser_networking::ResourceKind::Script,
            SubresourceKind::Image => rowser_networking::ResourceKind::Image,
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
    painter: Painter,
    url: String,
    pending: HashMap<String, SubresourceKind>,
    css_texts: Vec<String>,
    scripts: Vec<(Option<String>, String)>,
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
        }
    }

    fn handle(&mut self, message: Message) -> bool {
        match message {
            Message::Shutdown => return true,
            Message::Navigate(url) => self.navigate(url),
            Message::SubresourceFetched {
                url, body, pending, ..
            } => {
                self.subresource_fetched(url, body);
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

    fn subresource_fetched(&mut self, url: String, body: Vec<u8>) {
        let Some(kind) = self.pending.remove(&url) else {
            return;
        };
        match kind {
            SubresourceKind::Document => self.document_fetched(url, body),
            SubresourceKind::Stylesheet => {
                let text = String::from_utf8_lossy(&body).into_owned();
                self.css_texts.push(text);
            }
            SubresourceKind::Script => {
                let text = String::from_utf8_lossy(&body).into_owned();
                self.scripts.push((Some(url), text));
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
        }
    }

    fn document_fetched(&mut self, url: String, body: Vec<u8>) {
        let mut document = parse_html(&body);
        document.url = Some(url);
        let dom = Rc::new(RefCell::new(std::mem::take(&mut document.dom)));
        self.document = Some(document);

        // Collect subresources: stylesheets, scripts, images.
        let dom_ref = dom.borrow();
        let mut requests: Vec<(String, SubresourceKind)> = Vec::new();
        let mut css_texts: Vec<String> = self.css_texts.clone();
        let mut scripts: Vec<(Option<String>, String)> = Vec::new();
        for node in dom_ref.subtree_elements(dom_ref.document()) {
            let Some(element) = dom_ref.element(node) else {
                continue;
            };
            let tag = element.local_name().to_string();
            match tag.as_str() {
                "style" => css_texts.push(dom_ref.text_content(node)),
                "script" => {
                    if let Some(src) = dom_ref.get_attr(node, "src") {
                        let src = self.resolve_url(src);
                        requests.push((src.clone(), SubresourceKind::Script));
                        scripts.push((Some(src), String::new()));
                    } else {
                        scripts.push((None, dom_ref.text_content(node)));
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
        if self.dom.is_none() {
            // Internal (rowser://) or unreachable pages: end the load state.
            self.navigating = false;
            self.snapshot(|snapshot| snapshot.loading = false);
            return;
        }
        tracing::debug!(target: "rowser::engine", "tab {} subresources complete → render", self.state.tab);
        self.render_pipeline();
        tracing::debug!(target: "rowser::engine", "tab {} render done → scripts", self.state.tab);
        self.run_scripts();
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
        let Some(dom) = self.dom.clone() else { return };
        // Stylesheets.
        let media = MediaContext {
            width: self.viewport.width,
            height: self.viewport.height,
            dark_mode: false,
        };
        let sheets: Vec<ParsedStylesheet> = self
            .css_texts
            .iter()
            .map(|css| parse_stylesheet(css, &media))
            .collect();
        let (styles, layout) =
            self.layout_engine
                .layout_document(&dom.borrow(), &sheets, &media, self.viewport);
        self.style_map = Some(styles);
        self.layout = Some(layout);
        self.rendered_dom_version = dom.borrow().version;
        self.display_list = None;
        self.dirty = true;
        self.repaint();
    }

    fn repaint(&mut self) {
        if self.suspended || self.dom.is_none() {
            return;
        }
        let Some(layout) = self.layout.clone() else {
            return;
        };
        let Some(styles) = self.style_map.clone() else {
            return;
        };
        let Some(dom) = self.dom.clone() else { return };
        let list = build_display_list(&dom.borrow(), &styles, &layout, &self.images);
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
        };
        match JsRuntime::new(self.state.js_config.clone(), bridge) {
            Ok(runtime) => self.js = Some(runtime),
            Err(err) => {
                tracing::warn!(target: "rowser::engine", "js init failed: {err}");
                return;
            }
        }
        let scripts = std::mem::take(&mut self.scripts);
        if let Some(js) = &self.js {
            for (i, (src, code)) in scripts.iter().enumerate() {
                let name = src.clone().unwrap_or_else(|| format!("inline-{i}.js"));
                if let Err(err) = js.eval(code, &name) {
                    let _ = self.state.event_tx.send(EngineEvent::ConsoleMessage {
                        tab: self.state.tab,
                        level: "error".to_owned(),
                        text: format!("{name}: {err}"),
                    });
                }
            }
        }
        self.scripts = scripts;
        self.mark_if_dirty();
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

    fn mark_if_dirty(&mut self) {
        if let Some(dom) = &self.dom {
            let version = dom.borrow().version;
            if version != self.rendered_dom_version {
                self.dirty = true;
            }
        }
    }

    fn idle(&mut self) {
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
        dom_bytes + js_bytes + worker_bytes.max(0) as u64 + frame_bytes + image_bytes
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
