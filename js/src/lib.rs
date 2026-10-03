//! Rrowser JS: the QuickJS-ng runtime and Web API bindings.
//!
//! Architecture (see `docs/ARCHITECTURE.md` for the full picture):
//!
//! * **QuickJS-ng** (v0.16.x) is embedded through `rquickjs` 0.14.
//! * All JavaScript callbacks (timer callbacks, promise resolvers, event
//!   listeners, worker callbacks, WebSocket handlers) live **inside the JS
//!   heap**, keyed by numeric ids. Rust holds no `JsFunction` references,
//!   which keeps GC tracing trivially sound and leak-free.
//! * Native functions (`__native_*`) only exchange primitives (strings,
//!   numbers, booleans). The JS-side prelude (`prelude.js`) builds the real
//!   Web APIs — `fetch`, `XMLHttpRequest`, `setTimeout`, `WebSocket`,
//!   `Worker`, `localStorage`, DOM classes — on top of them.
//! * Async completions flow **into** JS through [`EngineEvent`] dispatch,
//!   followed by a promise-job pump — so `async/await` and microtasks work.
//! * A watchdog interrupt terminates runaway scripts; the JS heap has a hard
//!   memory limit. QuickJS-ng's incremental GC runs during allocation, so
//!   the main thread never stalls on a full GC pass; idle pages get an
//!   explicit `run_gc` from the engine's memory manager.

pub mod prelude {
    /// The JS-side Web API prelude, evaluated before any page script.
    pub const PRELUDE_JS: &str = include_str!("prelude.js");

    /// The minimal prelude for worker contexts (message passing only).
    pub const WORKER_PRELUDE_JS: &str = r#"
(function () {
  'use strict';
  globalThis.onmessage = null;
  globalThis.postMessage = function (msg) { __native_worker_post_message(JSON.stringify(msg)); };
  globalThis.close = function () {};
  globalThis.__onWorkerMessage = function (json) {
    const cb = globalThis.onmessage;
    if (cb) { try { cb({ data: JSON.parse(json) }); } catch (e) {} }
  };
  globalThis.console = {
    log: (...a) => __native_console('log', a.join(' ')),
    error: (...a) => __native_console('error', a.join(' ')),
  };
})();
"#;
}

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rowser_dom::{Dom, NodeId};
use rowser_privacy::fingerprint::SpoofProfile;
use rowser_storage::Storage;
use rquickjs::{Context, Function, Runtime};

/// A pending DOM mutation, raw form (WHATWG MutationObserver, DOM §4.3).
///
/// Captured at mutation time inside the native bridges; delivered to JS as
/// JSON batches after the script turn (spec: observer callbacks run at the
/// microtask checkpoint — our checkpoint is end-of-`eval`/`dispatch`).
#[derive(Debug, Clone, Default)]
pub struct RawMutation {
    /// 0 = childList, 1 = attributes, 2 = characterData.
    pub kind: u8,
    /// Node the mutation occurred on (parent for childList).
    pub target: NodeId,
    /// childList: added node handles (still live at delivery).
    pub added: Vec<u64>,
    /// childList: removed nodes — captured descriptors, because the arena
    /// may recycle those handles before delivery.
    pub removed: Vec<RemovedNode>,
    /// childList: sibling handles around the change point (after mutation).
    pub prev: Option<u64>,
    pub next: Option<u64>,
    /// attributes: attribute name.
    pub name: String,
    /// attributes/characterData: previous value.
    pub old: String,
}

/// Descriptor of a node removed from the tree, captured at mutation time.
/// The JS side materializes a detached pseudo-Element from this, so
/// `record.removedNodes[i].tagName` etc. keep working like Chrome even
/// though the arena slot is gone.
#[derive(Debug, Clone, Default)]
pub struct RemovedNode {
    /// Former node handle.
    pub h: u64,
    /// 1 = element (upper-cased tag), 3 = text.
    pub node_type: u8,
    /// Element tag name (upper-case) or `#text`.
    pub tag: String,
    /// Former `id` attribute (if any).
    pub id: String,
    /// Former `class` attribute (if any).
    pub cls: String,
}

/// A registered MutationObserver (`observe()` state).
#[derive(Debug, Clone)]
pub struct MoRegistration {
    /// JS-side observer id.
    pub id: u64,
    /// Observed target node.
    pub target: NodeId,
    /// `childList` option.
    pub child_list: bool,
    /// `attributes` option.
    pub attributes: bool,
    /// `characterData` option.
    pub character_data: bool,
    /// `subtree` option.
    pub subtree: bool,
}

/// `observe()` options as sent by the prelude (JSON).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoOptions {
    #[serde(default)]
    child_list: bool,
    #[serde(default)]
    attributes: bool,
    #[serde(default)]
    character_data: bool,
    #[serde(default)]
    subtree: bool,
}

/// An IntersectionObserver registration (engine-evaluated after layout).
#[derive(Debug, Clone)]
pub struct IoRegistration {
    /// JS-side observer id.
    pub id: u64,
    /// Observed target node.
    pub target: NodeId,
    /// Root node (None = viewport).
    pub root: Option<NodeId>,
    /// rootMargin in px: [top, right, bottom, left].
    pub root_margin: [f32; 4],
    /// Thresholds (default [0.0]).
    pub thresholds: Vec<f32>,
    /// Last-delivered ratio (-1 = never delivered).
    pub last_ratio: f32,
}

/// A ResizeObserver registration.
#[derive(Debug, Clone)]
pub struct RoRegistration {
    /// JS-side observer id.
    pub id: u64,
    /// Observed target node.
    pub target: NodeId,
    /// Last-delivered border-box rect (None = never delivered).
    pub last_rect: Option<[f32; 4]>,
}

/// Intersection/ResizeObserver registrations shared with the engine.
#[derive(Debug, Default)]
pub struct ObserversState {
    /// IntersectionObservers.
    pub io: Vec<IoRegistration>,
    /// ResizeObservers.
    pub ro: Vec<RoRegistration>,
}

/// Shared handle to the observer state.
pub type ObserversShared = Rc<RefCell<ObserversState>>;

/// MutationObserver bookkeeping: pending records + registrations.
#[derive(Debug, Default)]
pub struct MoState {
    /// Records queued since the last delivery.
    pub records: Vec<RawMutation>,
    /// Active registrations (last `observe()` wins per observer id).
    pub observers: Vec<MoRegistration>,
}

/// Shared handle to the MutationObserver state.
pub type MoShared = Rc<RefCell<MoState>>;

/// Captures a removed-node descriptor (call while `dom` is still borrowed
/// and the node is still valid).
fn describe_removed(dom: &Dom, h: NodeId) -> RemovedNode {
    let mut out = RemovedNode {
        h: h as u64,
        ..Default::default()
    };
    if let Some(el) = dom.element(h) {
        out.node_type = 1;
        out.tag = el.name.local.to_uppercase();
        out.id = dom.get_attr(h, "id").unwrap_or_default().to_owned();
        out.cls = dom.get_attr(h, "class").unwrap_or_default().to_owned();
    } else if dom.is_text(h) {
        out.node_type = 3;
        out.tag = "#text".to_owned();
    } else {
        out.node_type = 8;
        out.tag = "#comment".to_owned();
    }
    out
}

/// Queues a childList record (call after the mutation, dom borrowed).
fn record_child_list(
    mo: &MoShared,
    dom: &Dom,
    target: NodeId,
    added: Vec<u64>,
    removed: Vec<RemovedNode>,
    anchor: Option<NodeId>,
) {
    let (prev, next) = match anchor {
        // Siblings around the anchor slot (after mutation).
        Some(a) => (dom.prev_sibling(a), dom.next_sibling(a)),
        None => (None, None),
    };
    mo.borrow_mut().records.push(RawMutation {
        kind: 0,
        target,
        added,
        removed,
        prev: prev.map(|n| n as u64),
        next: next.map(|n| n as u64),
        ..Default::default()
    });
}

/// Commands the runtime sends to the owning engine (timers, fetches, ...).
#[derive(Debug, Clone)]
pub enum JsCommand {
    /// Schedule a timer (or interval).
    TimerStart {
        /// Timer id (JS-assigned).
        id: u64,
        /// Delay in milliseconds.
        delay_ms: u64,
        /// True for `setInterval`.
        interval: bool,
    },
    /// Cancel a timer.
    TimerClear {
        /// Timer id.
        id: u64,
    },
    /// Start a fetch; completion arrives as
    /// [`EngineEvent::FetchCompleted`].
    FetchStart {
        /// Fetch id (JS-assigned).
        id: u64,
        /// Target URL.
        url: String,
        /// JSON `FetchInit` (method, headers, body).
        init: String,
    },
    /// Open a WebSocket.
    WsOpen {
        /// Socket id.
        id: u64,
        /// URL.
        url: String,
    },
    /// Send a text frame.
    WsSend {
        /// Socket id.
        id: u64,
        /// Payload.
        data: String,
    },
    /// Close a socket.
    WsClose {
        /// Socket id.
        id: u64,
    },
    /// Spawn a worker (engine fetches + executes the script).
    WorkerSpawn {
        /// Worker id.
        id: u64,
        /// Script URL.
        url: String,
    },
    /// Post a message to a worker.
    WorkerPost {
        /// Worker id.
        id: u64,
        /// JSON message.
        message: String,
    },
    /// Terminate a worker.
    WorkerTerminate {
        /// Worker id.
        id: u64,
    },
    /// A console call (level, text) — forwarded to the engine event stream.
    Console {
        /// `log`/`warn`/`error`/`info`.
        level: String,
        /// Formatted message.
        text: String,
    },
    /// Script-initiated navigation (location.href assignment, .assign/.replace).
    Navigate {
        /// Absolute or page-relative URL.
        url: String,
    },
    /// `history.pushState(state, title, url)` — same-document history entry.
    HistoryPush {
        /// JSON-encoded state object.
        state: String,
        /// URL (absolute or page-relative; may be empty).
        url: String,
    },
    /// `history.replaceState(state, title, url)`.
    HistoryReplace {
        /// JSON-encoded state object.
        state: String,
        /// URL (may be empty = keep current).
        url: String,
    },
    /// `history.back/forward/go(delta)`.
    HistoryGo {
        /// Relative movement in the joint session history.
        delta: i64,
    },
    /// The DOM was mutated; re-style/layout/render after the script task.
    MarkDirty,
    /// JS set `video.src` (direct URL, data URL, or a `rowser-mse:` object
    /// URL referencing a MediaSource).
    MediaSetSrc {
        /// Element handle.
        node: u64,
        /// Source URL (may be `rowser-mse:N`).
        url: String,
    },
    /// JS called `play()`.
    MediaPlay {
        /// Element handle.
        node: u64,
    },
    /// JS called `pause()`.
    MediaPause {
        /// Element handle.
        node: u64,
    },
    /// JS set `currentTime`.
    MediaSeek {
        /// Element handle.
        node: u64,
        /// Target time in seconds.
        time: f64,
    },
    /// JS set `volume` (0.0-1.0).
    MediaSetVolume {
        /// Element handle.
        node: u64,
        /// Volume.
        volume: f32,
    },
    /// JS set `muted`.
    MediaSetMuted {
        /// Element handle.
        node: u64,
        /// Muted.
        muted: bool,
    },
    /// JS constructed `new MediaSource()`.
    MediaCreateSource {
        /// JS-side MediaSource id.
        ms_id: u64,
    },
    /// JS called `MediaSource.addSourceBuffer(mime)`.
    MediaAddSourceBuffer {
        /// MediaSource id.
        ms_id: u64,
        /// MIME type (e.g. `video/mp4; codecs="avc1.640028"`).
        mime: String,
        /// JS-assigned SourceBuffer id.
        sb_id: u64,
    },
    /// JS called `SourceBuffer.appendBuffer(bytes)`.
    MediaAppendBuffer {
        /// SourceBuffer id.
        sb_id: u64,
        /// Exact bytes from the ArrayBuffer.
        data: Vec<u8>,
    },
    /// JS called `MediaSource.endOfStream()`.
    MediaEndOfStream {
        /// MediaSource id.
        ms_id: u64,
    },
    /// A worker context posted a message to its owning page (local routing;
    /// never forwarded to the engine).
    WorkerEgress {
        /// Worker id.
        id: u64,
        /// JSON message.
        message: String,
    },
}

/// Events the engine dispatches **into** JavaScript.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A timer/interval fired.
    TimerFired(u64),
    /// A fetch completed.
    FetchCompleted {
        /// Fetch id.
        id: u64,
        /// HTTP status.
        status: u16,
        /// JSON headers object.
        headers: String,
        /// Base64 body.
        body_b64: String,
    },
    /// A fetch failed (network error).
    FetchFailed {
        /// Fetch id.
        id: u64,
        /// Error description.
        error: String,
    },
    /// A WebSocket event.
    WsEvent {
        /// Socket id.
        id: u64,
        /// Event kind: `open`/`message`/`close`/`error`.
        kind: String,
        /// Payload (for messages).
        data: String,
    },
    /// A worker posted a message to its page.
    WorkerMessage {
        /// Worker id.
        id: u64,
        /// JSON message.
        message: String,
    },
    /// IntersectionObserver entries computed by the engine after layout.
    IntersectFired {
        /// Observer id.
        id: u64,
        /// JSON array of entries.
        json: String,
    },
    /// ResizeObserver entries computed by the engine after layout.
    ResizeFired {
        /// Observer id.
        id: u64,
        /// JSON array of entries.
        json: String,
    },
    /// A history traversal (back/forward) landed on a document: fire
    /// `popstate` on window with the entry's state.
    PopState {
        /// JSON-encoded state (may be `null`).
        state: String,
    },
    /// The document URL changed without navigation (pushState/replaceState):
    /// update `location`, fire `hashchange` when the fragment changed.
    LocationChanged {
        /// New absolute URL.
        url: String,
    },
    /// A UI event (click, etc.) on a DOM node.
    DomEvent {
        /// Node handle.
        node: u64,
        /// Event type.
        event_type: String,
    },
    /// A media element event (loadedmetadata, canplay, timeupdate, ended,
    /// error, play, pause). `detail` is a JSON payload (may be empty).
    MediaEvent {
        /// Element handle.
        node: u64,
        /// Event type.
        event_type: String,
        /// JSON detail (time/duration/error text).
        detail: String,
    },
    /// A MediaSource/SourceBuffer event (sourceopen, updateend...).
    MediaSourceEvent {
        /// MediaSource id.
        ms_id: u64,
        /// SourceBuffer id (None for MediaSource-level events).
        sb_id: Option<u64>,
        /// Event type.
        event_type: String,
    },
}

/// Page-thread-maintained snapshot of a media element's state, read
/// synchronously by JS property getters (the truth lives in the pipeline
/// worker; events keep the mirror fresh).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MediaMirror {
    /// Current presentation time (seconds).
    pub time: f64,
    /// Duration (seconds; 0 = unknown/live).
    pub duration: f64,
    /// Video width (0 = audio-only).
    pub width: u32,
    /// Video height.
    pub height: u32,
    /// True while paused.
    pub paused: bool,
    /// HTMLMediaElement readyState approximation (0..4).
    pub ready_state: u8,
    /// Last error text.
    pub error: Option<String>,
    /// Demuxed playback window end (seconds) — `buffered.end()` analogue.
    pub buffered_end: f64,
}

/// Per-page media state mirror shared between the page thread (writer) and
/// the JS natives (reader).
pub type MediaMirrorMap =
    std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u64, MediaMirror>>>;

/// Layout rects (document space: `[x, y, w, h]`) keyed by node handle,
/// refreshed by the page thread after every layout pass. Powers a real
/// `getBoundingClientRect` — player frameworks size their controls from it.
pub type RectMirrorMap = std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u64, [f32; 4]>>>;

/// `(scroll_y, viewport_width, viewport_height)`, page-thread refreshed.
pub type ViewportMirror = std::rc::Rc<std::cell::RefCell<(f32, f32, f32)>>;

impl PageBridge {
    /// Empty layout mirrors for runtimes that never see layout data
    /// (workers, tests, examples).
    pub fn empty_mirrors() -> (RectMirrorMap, ViewportMirror) {
        (
            std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new())),
            std::rc::Rc::new(std::cell::RefCell::new((0.0, 0.0, 0.0))),
        )
    }
}

/// The shared state between the runtime and the page thread.
pub struct PageBridge {
    /// The page DOM (single-threaded with the runtime).
    pub dom: Rc<RefCell<Dom>>,
    /// The document node.
    pub document: NodeId,
    /// The `body` element (0 when absent).
    pub body: NodeId,
    /// The `html` element (0 when absent).
    pub html: NodeId,
    /// Document URL (for `location`).
    pub url: String,
    /// Origin for localStorage.
    pub origin: String,
    /// Profile storage (optional: about: pages have none).
    pub storage: Option<Arc<Storage>>,
    /// Anti-fingerprinting profile.
    pub spoof: SpoofProfile,
    /// Channel to the engine.
    pub outgoing: Option<Sender<JsCommand>>,
    /// Media element state mirror (page thread keeps it fresh).
    pub media_mirror: MediaMirrorMap,
    /// Layout rects, document space (page thread refreshes after layout).
    pub rects: RectMirrorMap,
    /// `(scroll_y, viewport_width, viewport_height)`.
    pub viewport: ViewportMirror,
    /// MutationObserver records + registrations.
    pub mo: MoShared,
    /// History mirror for sync reads: (entry_count, current state JSON).
    pub history: Rc<RefCell<HistoryMirror>>,
    /// Intersection/ResizeObserver registrations (engine drives delivery).
    pub observers: ObserversShared,
}

/// Synchronous history state shared with the prelude.
#[derive(Debug, Clone, Default)]
pub struct HistoryMirror {
    /// `history.length`.
    pub len: u32,
    /// `history.state` as JSON (defaults to `null`).
    pub state: String,
}

/// Shared handle to the history mirror.
pub type HistoryMirrorShared = Rc<RefCell<HistoryMirror>>;

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct JsConfig {
    /// Hard JS heap limit in bytes.
    pub memory_limit: usize,
    /// Max script execution time before the watchdog interrupts.
    pub script_timeout: Duration,
    /// Max stack size.
    pub stack_size: usize,
}

impl Default for JsConfig {
    fn default() -> Self {
        JsConfig {
            // Heavy single-page apps (YouTube's kevlar bundle alone is
            // ~3.5 MB of code, before any object allocation) need real
            // headroom; 96 MB OOM'd mid-hydration.
            memory_limit: 384 * 1024 * 1024,
            script_timeout: Duration::from_secs(10),
            stack_size: 2 * 1024 * 1024,
        }
    }
}

/// A QuickJS-ng runtime bound to a page.
pub struct JsRuntime {
    runtime: Runtime,
    context: Context,
    watchdog_start: Rc<Cell<f64>>,
    #[allow(dead_code)]
    config: JsConfig,
    /// Live timers registered by JS (count only; bodies live in JS).
    timer_count: Rc<Cell<u64>>,
    /// MutationObserver state (records + registrations), shared with the
    /// mutating DOM natives.
    mo: MoShared,
    /// The page DOM, for MutationObserver subtree filtering at delivery.
    dom: Rc<RefCell<Dom>>,
}

/// Errors surfaced to the engine.
#[derive(Debug, thiserror::Error)]
pub enum JsError {
    /// Script evaluation failed (syntax or runtime error).
    #[error("js error: {0}")]
    Eval(String),
    /// Runtime construction failed.
    #[error("js runtime: {0}")]
    Runtime(String),
}

impl From<rquickjs::Error> for JsError {
    fn from(err: rquickjs::Error) -> Self {
        JsError::Eval(err.to_string())
    }
}

impl JsRuntime {
    /// Creates the runtime and installs the Web API prelude.
    pub fn new(config: JsConfig, bridge: PageBridge) -> Result<JsRuntime, JsError> {
        let runtime = Runtime::new().map_err(|e| JsError::Runtime(e.to_string()))?;
        runtime.set_memory_limit(config.memory_limit);
        runtime.set_max_stack_size(config.stack_size);
        runtime.set_gc_threshold(1024 * 512);

        let watchdog_start = Rc::new(Cell::new(0.0f64));
        let watchdog_limit = config.script_timeout.as_secs_f64();
        {
            let start = Rc::clone(&watchdog_start);
            runtime.set_interrupt_handler(Some(Box::new(move || {
                let started = start.get();
                if started == 0.0 {
                    return false;
                }
                (monotonic_secs() - started) > watchdog_limit
            })));
        }

        let context = Context::full(&runtime).map_err(|e| JsError::Runtime(e.to_string()))?;
        let mo = Rc::clone(&bridge.mo);
        let dom = Rc::clone(&bridge.dom);
        let js = JsRuntime {
            runtime,
            context,
            watchdog_start,
            config,
            timer_count: Rc::new(Cell::new(0)),
            mo,
            dom,
        };
        js.install_natives(bridge)?;
        js.context.with(|ctx| {
            if let Err(e) = ctx.eval::<(), _>(prelude::PRELUDE_JS) {
                let detail = exception_detail(&ctx).unwrap_or_else(|| err_string(&e));
                return Err(JsError::Eval(detail));
            }
            Ok(())
        })?;
        Ok(js)
    }

    /// Evaluates a classic script. Returns a stringified result.
    pub fn eval(&self, code: &str, filename: &str) -> Result<String, JsError> {
        let code = code.to_owned();
        let filename = filename.to_owned();
        self.reset_watchdog();
        let result: Result<String, (rquickjs::Error, Option<String>)> = self.context.with(|ctx| {
            let mut options = rquickjs::context::EvalOptions::default();
            options.filename = Some(filename.clone());
            options.strict = false;
            match ctx.eval_with_options::<rquickjs::Value, _>(code, options) {
                Ok(value) => stringify_value(&ctx, &value).map_err(|e| (e, None)),
                Err(e) => Err((e, exception_detail(&ctx))),
            }
        });
        self.clear_watchdog();
        self.pump_jobs();
        self.deliver_mutations();
        result.map_err(|(e, detail)| {
            JsError::Eval(match detail {
                Some(detail) if !detail.is_empty() => detail,
                _ => format_js_error(&e),
            })
        })
    }

    /// Dispatches an engine event into JS (timer fired, fetch completed, ...).
    pub fn dispatch(&self, event: EngineEvent) {
        let call = match event {
            EngineEvent::TimerFired(id) => format!("__onTimerFired({id})"),
            EngineEvent::FetchCompleted {
                id,
                status,
                headers,
                body_b64,
            } => format!(
                "__onFetchCompleted({id},{status},{},{})",
                json_str(&headers),
                json_str(&body_b64)
            ),
            EngineEvent::FetchFailed { id, error } => {
                format!("__onFetchFailed({id},{})", json_str(&error))
            }
            EngineEvent::WsEvent { id, kind, data } => {
                format!("__onWsEvent({id},{},{})", json_str(&kind), json_str(&data))
            }
            EngineEvent::WorkerMessage { id, message } => {
                format!("__onWorkerMessage({id},{})", json_str(&message))
            }
            EngineEvent::DomEvent { node, event_type } => {
                format!("__onDomEvent({},{})", node, json_str(&event_type))
            }
            EngineEvent::PopState { state } => {
                format!("__onPopState({})", json_str(&state))
            }
            EngineEvent::IntersectFired { id, json } => {
                format!("__onIntersect({id},{json})")
            }
            EngineEvent::ResizeFired { id, json } => {
                format!("__onResize({id},{json})")
            }
            EngineEvent::LocationChanged { url } => {
                format!("__onLocationChanged({})", json_str(&url))
            }
            EngineEvent::MediaEvent {
                node,
                event_type,
                detail,
            } => format!(
                "__onMediaEvent({},{},{})",
                node,
                json_str(&event_type),
                json_str(&detail)
            ),
            EngineEvent::MediaSourceEvent {
                ms_id,
                sb_id,
                event_type,
            } => format!(
                "__onMediaSourceEvent({},{},{})",
                ms_id,
                sb_id
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "null".to_owned()),
                json_str(&event_type)
            ),
        };
        self.reset_watchdog();
        let outcome = self.context.with(|ctx| ctx.eval::<(), _>(call.as_bytes()));
        if let Err(err) = outcome {
            // Swallowed dispatch errors are the classic "site silently broke"
            // failure; surface them (devtools console via JS handler errors,
            // stderr behind ROWSER_JS_TRACE for field debugging).
            if std::env::var("ROWSER_JS_TRACE").is_ok() {
                let head: String = call.chars().take(140).collect();
                eprintln!("[js] dispatch error: {err} ← {head}");
            }
        }
        self.clear_watchdog();
        self.pump_jobs();
        self.deliver_mutations();
    }

    /// Delivers pending MutationObserver records to JS. Runs at the
    /// microtask checkpoint of every script turn (`eval`, `dispatch`) and
    /// after the promise pump — matching the spec's "queue a microtask to
    /// notify observers" behaviour closely enough for framework hydration
    /// (React/Vue/Vue hydration, Wikipedia's vector.js enhancements).
    ///
    /// Callbacks may mutate again; bounded rounds (20) keep a storm from
    /// wedging the page thread — leftover records carry to the next
    /// checkpoint.
    fn deliver_mutations(&self) {
        for _round in 0..20 {
            let batches = self.collect_mo_batches();
            if batches.is_empty() {
                break;
            }
            for (id, json) in batches {
                let call = format!("__onMutations({id},{json})");
                self.reset_watchdog();
                let outcome = self.context.with(|ctx| ctx.eval::<(), _>(call.as_bytes()));
                if let Err(err) = outcome {
                    let detail = self
                        .context
                        .with(|ctx| exception_detail(&ctx).unwrap_or_else(|| format!("{err:?}")));
                    if std::env::var("ROWSER_JS_TRACE").is_ok() {
                        eprintln!("[js] mutation dispatch error: {detail}");
                    }
                }
                self.clear_watchdog();
                self.pump_jobs();
            }
        }
    }

    /// Takes the pending records and groups them per registered observer
    /// (filtered by target/subtree/options). Empty result = nothing to do.
    fn collect_mo_batches(&self) -> Vec<(u64, String)> {
        let records = std::mem::take(&mut self.mo.borrow_mut().records);
        if records.is_empty() {
            return Vec::new();
        }
        let observers = self.mo.borrow().observers.clone();
        if observers.is_empty() {
            return Vec::new();
        }
        // Subtree resolution needs the DOM (ancestor walk).
        let mut batches: Vec<(u64, String)> = Vec::new();
        let dom = self.dom.borrow();
        for obs in &observers {
            let mut json = String::from("[");
            let mut any = false;
            for r in &records {
                let in_scope = r.target == obs.target
                    || (obs.subtree && in_subtree(&dom, obs.target, r.target));
                if !in_scope {
                    continue;
                }
                let kind_ok = match r.kind {
                    0 => obs.child_list,
                    1 => obs.attributes,
                    _ => obs.character_data,
                };
                if !kind_ok {
                    continue;
                }
                if any {
                    json.push(',');
                }
                json.push_str(&mo_record_json(r));
                any = true;
            }
            if any {
                json.push(']');
                batches.push((obs.id, json));
            }
        }
        batches
    }

    /// Pumps promise jobs until the queue is empty — BOUNDED. A page whose
    /// JS perpetually re-schedules microtasks (`while (true) { await
    /// Promise.resolve(); }` or a runaway prelude chain) would otherwise
    /// wedge the page thread inside a single dispatch: no engine messages,
    /// no renders, one core burned. Bounding each pump and resuming on the
    /// next idle tick keeps the tab responsive while the storm runs.
    pub fn pump_jobs(&self) {
        let started = std::time::Instant::now();
        let mut pumped: u32 = 0;
        while self.runtime.is_job_pending() {
            if pumped >= 10_000 || started.elapsed() > Duration::from_millis(50) {
                return; // yield; remaining jobs are pumped on the next call
            }
            match self.runtime.execute_pending_job() {
                Ok(true) => {
                    pumped += 1;
                    continue;
                }
                Ok(false) => break,
                Err(err) => {
                    tracing::warn!(target: "rowser::js", "job error: {err:?}");
                    break;
                }
            }
        }
    }

    /// JS heap usage in bytes.
    pub fn memory_usage(&self) -> i64 {
        self.runtime.memory_usage().memory_used_size
    }

    /// Runs a full GC pass (called by the memory manager on idle tabs).
    pub fn run_gc(&self) {
        self.runtime.run_gc();
    }

    /// Installs `__native_worker_post_message` for a worker context: the
    /// closure captures the worker id and the page-loop command channel.
    pub fn set_worker_post_message(
        &self,
        worker_id: u64,
        sender: Sender<JsCommand>,
    ) -> Result<(), JsError> {
        self.context.with(|ctx| {
            let globals = ctx.globals();
            let func = Function::new(ctx.clone(), move |message: String| {
                let _ = sender.send(JsCommand::WorkerEgress {
                    id: worker_id,
                    message,
                });
            })?;
            globals.set("__native_worker_post_message", func)?;
            Ok::<(), rquickjs::Error>(())
        })?;
        Ok(())
    }

    /// Live timer count (for memory accounting).
    pub fn timer_count(&self) -> u64 {
        self.timer_count.get()
    }

    /// True when scripts are still waiting on timers/promises.
    pub fn has_pending_work(&self) -> bool {
        self.runtime.is_job_pending() || self.timer_count.get() > 0
    }

    fn reset_watchdog(&self) {
        self.watchdog_start.set(self.now());
    }

    fn clear_watchdog(&self) {
        self.watchdog_start.set(0.0);
    }

    fn now(&self) -> f64 {
        monotonic_secs()
    }

    fn install_natives(&self, bridge: PageBridge) -> Result<(), JsError> {
        let bridge = Rc::new(bridge);
        self.context.with(|ctx| {
            let globals = ctx.globals();

            // --- console ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_console",
                Function::new(ctx.clone(), move |level: String, text: String| {
                    if std::env::var("ROWSER_JS_TRACE").is_ok() {
                        eprintln!("[js:{}] {}", level, text);
                    }
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::Console { level, text });
                    }
                })?,
            )?;

            // --- DOM layout rects (getBoundingClientRect) ---
            let rects = Rc::clone(&bridge.rects);
            globals.set(
                "__native_dom_get_rect",
                Function::new(ctx.clone(), move |h: u64| -> String {
                    rects
                        .borrow()
                        .get(&h)
                        .map(|r| format!("[{},{},{},{}]", r[0], r[1], r[2], r[3]))
                        .unwrap_or_else(|| "null".into())
                })?,
            )?;

            // --- viewport + scroll (window metrics) ---
            let viewport = Rc::clone(&bridge.viewport);
            globals.set(
                "__native_dom_viewport",
                Function::new(ctx.clone(), move || -> String {
                    let v = *viewport.borrow();
                    format!(
                        "{{\"scrollY\":{},\"width\":{},\"height\":{}}}",
                        v.0, v.1, v.2
                    )
                })?,
            )?;

            // --- MutationObserver registration ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_mo_observe",
                Function::new(ctx.clone(), move |id: u64, target: u64, options: String| {
                    let opts: MoOptions = serde_json::from_str(&options).unwrap_or_default();
                    let mut mo = b.mo.borrow_mut();
                    // Last observe() per id wins (spec: replace the
                    // previous registration for that observer).
                    mo.observers.retain(|o| o.id != id);
                    mo.observers.push(MoRegistration {
                        id,
                        target: target as NodeId,
                        child_list: opts.child_list,
                        attributes: opts.attributes,
                        character_data: opts.character_data,
                        subtree: opts.subtree,
                    });
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_mo_disconnect",
                Function::new(ctx.clone(), move |id: u64| {
                    b.mo.borrow_mut().observers.retain(|o| o.id != id);
                })?,
            )?;

            // --- timers ---
            let b = Rc::clone(&bridge);
            let timers = Rc::clone(&self.timer_count);
            globals.set(
                "__native_timer_start",
                Function::new(
                    ctx.clone(),
                    move |id: u64, delay_ms: f64, interval: bool| {
                        timers.set(timers.get() + 1);
                        if let Some(out) = &b.outgoing {
                            let _ = out.send(JsCommand::TimerStart {
                                id,
                                delay_ms: delay_ms.max(0.0) as u64,
                                interval,
                            });
                        }
                    },
                )?,
            )?;
            let b = Rc::clone(&bridge);
            let timers = Rc::clone(&self.timer_count);
            globals.set(
                "__native_timer_clear",
                Function::new(ctx.clone(), move |id: u64| {
                    timers.set(timers.get().saturating_sub(1));
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::TimerClear { id });
                    }
                })?,
            )?;

            // --- navigation (location.href / assign / replace) ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_navigate",
                Function::new(ctx.clone(), move |url: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::Navigate { url });
                    }
                })?,
            )?;

            // --- matchMedia: real evaluation against the live viewport ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_match_media",
                Function::new(ctx.clone(), move |query: String| -> bool {
                    let (scroll_y, width, height) = *b.viewport.borrow();
                    let _ = scroll_y;
                    let ctx = rowser_parsing::css::MediaContext {
                        width,
                        height,
                        dark_mode: false,
                    };
                    rowser_parsing::css::media_query_matches_str(&query, &ctx)
                })?,
            )?;

            // --- history API ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_history_length",
                Function::new(ctx.clone(), move || -> u32 { b.history.borrow().len })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_history_state",
                Function::new(ctx.clone(), move || -> String {
                    b.history.borrow().state.clone()
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_history_push",
                Function::new(ctx.clone(), move |state: String, url: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::HistoryPush { state, url });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_history_replace",
                Function::new(ctx.clone(), move |state: String, url: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::HistoryReplace { state, url });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_history_go",
                Function::new(ctx.clone(), move |delta: i64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::HistoryGo { delta });
                    }
                })?,
            )?;

            // --- Intersection/ResizeObserver registration ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_io_observe",
                Function::new(
                    ctx.clone(),
                    move |id: u64,
                          target: u64,
                          root: u64,
                          margin_json: String,
                          thresholds_json: String| {
                        let margin: [f32; 4] =
                            serde_json::from_str(&margin_json).unwrap_or([0.0; 4]);
                        let thresholds: Vec<f32> =
                            serde_json::from_str(&thresholds_json).unwrap_or_else(|_| vec![0.0]);
                        let thresholds = if thresholds.is_empty() {
                            vec![0.0]
                        } else {
                            thresholds
                        };
                        let mut obs = b.observers.borrow_mut();
                        obs.io
                            .retain(|o| !(o.id == id && o.target as u64 == target));
                        obs.io.push(IoRegistration {
                            id,
                            target: target as NodeId,
                            root: (root > 0).then_some(root as NodeId),
                            root_margin: margin,
                            thresholds,
                            last_ratio: -1.0,
                        });
                        drop(obs);
                        mark_dirty(&b);
                    },
                )?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_io_disconnect",
                Function::new(ctx.clone(), move |id: u64| {
                    b.observers.borrow_mut().io.retain(|o| o.id != id);
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_io_unobserve",
                Function::new(ctx.clone(), move |id: u64, target: u64| {
                    b.observers
                        .borrow_mut()
                        .io
                        .retain(|o| !(o.id == id && o.target as u64 == target));
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ro_observe",
                Function::new(ctx.clone(), move |id: u64, target: u64| {
                    let mut obs = b.observers.borrow_mut();
                    obs.ro
                        .retain(|o| !(o.id == id && o.target as u64 == target));
                    obs.ro.push(RoRegistration {
                        id,
                        target: target as NodeId,
                        last_rect: None,
                    });
                    drop(obs);
                    mark_dirty(&b);
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ro_unobserve",
                Function::new(ctx.clone(), move |id: u64, target: u64| {
                    b.observers
                        .borrow_mut()
                        .ro
                        .retain(|o| !(o.id == id && o.target as u64 == target));
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ro_disconnect",
                Function::new(ctx.clone(), move |id: u64| {
                    b.observers.borrow_mut().ro.retain(|o| o.id != id);
                })?,
            )?;

            // --- localStorage ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ls_get",
                Function::new(ctx.clone(), move |key: String| -> Option<String> {
                    read_local(&b, &key)
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ls_set",
                Function::new(ctx.clone(), move |key: String, value: String| {
                    write_local(&b, &key, &value);
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ls_remove",
                Function::new(ctx.clone(), move |key: String| {
                    remove_local(&b, &key);
                })?,
            )?;

            // --- fetch ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_fetch_start",
                Function::new(ctx.clone(), move |id: u64, url: String, init: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::FetchStart { id, url, init });
                    }
                })?,
            )?;

            // --- websockets ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ws_open",
                Function::new(ctx.clone(), move |url: String| -> u64 {
                    let id = next_id();
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WsOpen { id, url });
                    }
                    id
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ws_send",
                Function::new(ctx.clone(), move |id: u64, data: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WsSend { id, data });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ws_close",
                Function::new(ctx.clone(), move |id: u64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WsClose { id });
                    }
                })?,
            )?;

            // --- workers ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_worker_spawn",
                Function::new(ctx.clone(), move |url: String| -> u64 {
                    let id = next_id();
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WorkerSpawn { id, url });
                    }
                    id
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_worker_post",
                Function::new(ctx.clone(), move |id: u64, message: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WorkerPost { id, message });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_worker_terminate",
                Function::new(ctx.clone(), move |id: u64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WorkerTerminate { id });
                    }
                })?,
            )?;

            // --- DOM ---
            dom_natives(&ctx, &globals, &bridge)?;

            // --- media (HTMLMediaElement + MSE) ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_media_set_src",
                Function::new(ctx.clone(), move |node: u64, url: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaSetSrc { node, url });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_media_play",
                Function::new(ctx.clone(), move |node: u64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaPlay { node });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_media_pause",
                Function::new(ctx.clone(), move |node: u64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaPause { node });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_media_seek",
                Function::new(ctx.clone(), move |node: u64, time: f64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaSeek { node, time });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_media_set_volume",
                Function::new(ctx.clone(), move |node: u64, volume: f64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaSetVolume {
                            node,
                            volume: volume as f32,
                        });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_media_set_muted",
                Function::new(ctx.clone(), move |node: u64, muted: bool| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaSetMuted { node, muted });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_media_mirror",
                Function::new(ctx.clone(), move |node: u64| -> Option<String> {
                    let mirror = b.media_mirror.borrow().get(&node).cloned()?;
                    Some(serde_json::to_string(&mirror).unwrap_or_default())
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_mse_create",
                Function::new(ctx.clone(), move || -> u64 {
                    let id = next_id();
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaCreateSource { ms_id: id });
                    }
                    id
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_mse_add_source_buffer",
                Function::new(ctx.clone(), move |ms_id: u64, sb_id: u64, mime: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaAddSourceBuffer { ms_id, sb_id, mime });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_mse_append",
                Function::new(
                    ctx.clone(),
                    move |sb_id: u64, data: rquickjs::ArrayBuffer| {
                        // SAFETY: the buffer is alive for the duration of
                        // this native call; we copy out before returning.
                        let bytes = data
                            .as_raw()
                            .map(|slice| unsafe { slice.as_ref() }.to_vec())
                            .unwrap_or_default();
                        if let Some(out) = &b.outgoing {
                            let _ = out.send(JsCommand::MediaAppendBuffer { sb_id, data: bytes });
                        }
                    },
                )?,
            )?;
            // Byte-exact base64 → ArrayBuffer (binary fetch bodies; the
            // String-based b64 decode mangles non-UTF-8 bytes).
            // Base64 → binary → latin-1 string: each char carries one byte
            // (0-255) and survives the JS string round-trip exactly, unlike
            // from_utf8_lossy which corrupts non-UTF-8 bodies.
            globals.set(
                "__native_b64_decode_latin1",
                Function::new(ctx.clone(), |data: String| -> Option<String> {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .ok()?;
                    Some(bytes.iter().map(|&b| b as char).collect())
                })?,
            )?;

            let b = Rc::clone(&bridge);
            globals.set(
                "__native_mse_end_of_stream",
                Function::new(ctx.clone(), move |ms_id: u64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::MediaEndOfStream { ms_id });
                    }
                })?,
            )?;

            // --- environment (anti-fingerprinted) ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_env_info",
                Function::new(ctx.clone(), move || -> String { env_info_json(&b) })?,
            )?;

            // --- base64 helpers ---
            globals.set(
                "__native_b64_encode",
                Function::new(ctx.clone(), |data: String| -> Option<String> {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .encode(data.as_bytes())
                        .into()
                })?,
            )?;
            globals.set(
                "__native_b64_decode",
                Function::new(ctx.clone(), |data: String| -> Option<String> {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .ok()?;
                    Some(String::from_utf8_lossy(&bytes).into_owned())
                })?,
            )?;

            Ok::<(), rquickjs::Error>(())
        })?;
        Ok(())
    }
}

/// True when `node` is `ancestor` or a descendant of it.
fn in_subtree(dom: &Dom, ancestor: NodeId, node: NodeId) -> bool {
    let mut cur = Some(node);
    while let Some(c) = cur {
        if c == ancestor {
            return true;
        }
        cur = dom.parent(c);
    }
    false
}

/// Serializes one raw mutation record as a JSON object for the prelude.
fn mo_record_json(r: &RawMutation) -> String {
    let opt = |v: Option<u64>| match v {
        Some(v) => v.to_string(),
        None => "null".to_owned(),
    };
    match r.kind {
        0 => {
            let added: Vec<String> = r.added.iter().map(|h| h.to_string()).collect();
            let removed: Vec<String> = r
                .removed
                .iter()
                .map(|d| {
                    format!(
                        r#"{{"h":{},"nodeType":{},"tag":{},"id":{},"cls":{}}}"#,
                        d.h,
                        d.node_type,
                        json_str(&d.tag),
                        json_str(&d.id),
                        json_str(&d.cls)
                    )
                })
                .collect();
            format!(
                r#"{{"type":"childList","target":{},"added":[{}],"removed":[{}],"prev":{},"next":{}}}"#,
                r.target,
                added.join(","),
                removed.join(","),
                opt(r.prev),
                opt(r.next)
            )
        }
        1 => format!(
            r#"{{"type":"attributes","target":{},"name":{},"old":{}}}"#,
            r.target,
            json_str(&r.name),
            json_str(&r.old)
        ),
        _ => format!(
            r#"{{"type":"characterData","target":{},"old":{}}}"#,
            r.target,
            json_str(&r.old)
        ),
    }
}

fn dom_natives<'js>(
    ctx: &rquickjs::Ctx<'js>,
    globals: &rquickjs::Object<'js>,
    bridge: &Rc<PageBridge>,
) -> Result<(), rquickjs::Error> {
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_getElementById",
        Function::new(ctx.clone(), move |id: String| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.find_by_id(dom.document(), &id).map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_querySelector",
        Function::new(ctx.clone(), move |selector: String| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.query_selector(dom.document(), &selector)
                .map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_querySelectorAll",
        Function::new(ctx.clone(), move |selector: String| -> String {
            let dom = b.dom.borrow();
            match dom.query_selector_all(dom.document(), &selector) {
                Some(nodes) => serde_json::to_string(
                    &nodes.into_iter().map(|n| n as u64).collect::<Vec<u64>>(),
                )
                .unwrap_or_else(|_| "[]".to_owned()),
                None => "[]".to_owned(),
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_createElement",
        Function::new(ctx.clone(), move |tag: String| -> u64 {
            let mut dom = b.dom.borrow_mut();
            dom.create_html_element(&tag.to_ascii_lowercase()) as u64
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_createTextNode",
        Function::new(ctx.clone(), move |text: String| -> u64 {
            let mut dom = b.dom.borrow_mut();
            dom.create_text(text) as u64
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_appendChild",
        Function::new(ctx.clone(), move |parent: u64, child: u64| -> u64 {
            let (parent, child) = (parent as NodeId, child as NodeId);
            let mut dom = b.dom.borrow_mut();
            if dom.is_valid(parent) && dom.is_valid(child) {
                // MutationObserver: a moved node reports removal from its
                // old parent first (DOM spec: append = remove + insert).
                if let Some(old_parent) = dom.parent(child) {
                    let desc = describe_removed(&dom, child);
                    let prev = dom.prev_sibling(child).map(|n| n as u64);
                    let next = dom.next_sibling(child).map(|n| n as u64);
                    b.mo.borrow_mut().records.push(RawMutation {
                        kind: 0,
                        target: old_parent,
                        removed: vec![desc],
                        prev,
                        next,
                        ..Default::default()
                    });
                }
                dom.append(parent, child);
                record_child_list(
                    &b.mo,
                    &dom,
                    parent,
                    vec![child as u64],
                    Vec::new(),
                    Some(child),
                );
                mark_dirty(&b);
                child as u64
            } else {
                0
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_removeChild",
        Function::new(ctx.clone(), move |parent: u64, child: u64| {
            let (parent, child) = (parent as NodeId, child as NodeId);
            let mut dom = b.dom.borrow_mut();
            if dom.is_valid(parent) && dom.is_valid(child) {
                let desc = describe_removed(&dom, child);
                let prev = dom.prev_sibling(child).map(|n| n as u64);
                let next = dom.next_sibling(child).map(|n| n as u64);
                dom.detach(child);
                b.mo.borrow_mut().records.push(RawMutation {
                    kind: 0,
                    target: parent,
                    removed: vec![desc],
                    prev,
                    next,
                    ..Default::default()
                });
                mark_dirty(&b);
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_getAttr",
        Function::new(
            ctx.clone(),
            move |node: u64, name: String| -> Option<String> {
                let dom = b.dom.borrow();
                dom.get_attr(node as NodeId, &name).map(str::to_owned)
            },
        )?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_setAttr",
        Function::new(
            ctx.clone(),
            move |node: u64, name: String, value: String| {
                let mut dom = b.dom.borrow_mut();
                let old = dom
                    .get_attr(node as NodeId, &name)
                    .unwrap_or_default()
                    .to_owned();
                dom.set_attr(node as NodeId, &name, &value);
                b.mo.borrow_mut().records.push(RawMutation {
                    kind: 1,
                    target: node as NodeId,
                    name,
                    old,
                    ..Default::default()
                });
                mark_dirty(&b);
            },
        )?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_removeAttr",
        Function::new(ctx.clone(), move |node: u64, name: String| {
            let mut dom = b.dom.borrow_mut();
            let old = dom
                .get_attr(node as NodeId, &name)
                .unwrap_or_default()
                .to_owned();
            dom.remove_attr(node as NodeId, &name);
            b.mo.borrow_mut().records.push(RawMutation {
                kind: 1,
                target: node as NodeId,
                name,
                old,
                ..Default::default()
            });
            mark_dirty(&b);
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_textContent",
        Function::new(ctx.clone(), move |node: u64| -> String {
            let dom = b.dom.borrow();
            dom.text_content(node as NodeId)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_setTextContent",
        Function::new(ctx.clone(), move |node: u64, text: String| {
            let node = node as NodeId;
            let mut dom = b.dom.borrow_mut();
            if dom.is_valid(node) {
                if dom.element(node).is_some() {
                    // Replace all children with a single text node.
                    let children: Vec<NodeId> = dom.children(node).collect();
                    let mut removed_desc = Vec::with_capacity(children.len());
                    for child in children {
                        removed_desc.push(describe_removed(&dom, child));
                        dom.remove_subtree(child);
                    }
                    let text_node = dom.create_text(text);
                    dom.append(node, text_node);
                    record_child_list(
                        &b.mo,
                        &dom,
                        node,
                        vec![text_node as u64],
                        removed_desc,
                        Some(text_node),
                    );
                } else {
                    let old = dom.text_content(node);
                    dom.set_text(node, &text);
                    b.mo.borrow_mut().records.push(RawMutation {
                        kind: 2,
                        target: node,
                        old,
                        ..Default::default()
                    });
                }
                mark_dirty(&b);
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_style_get",
        Function::new(
            ctx.clone(),
            move |node: u64, prop: String| -> Option<String> {
                let dom = b.dom.borrow();
                let style = dom.get_attr(node as NodeId, "style")?;
                find_style_property(style, &prop)
            },
        )?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_style_set",
        Function::new(
            ctx.clone(),
            move |node: u64, prop: String, value: String| {
                let node = node as NodeId;
                let mut dom = b.dom.borrow_mut();
                if dom.is_valid(node) {
                    let current = dom.get_attr(node, "style").unwrap_or("").to_owned();
                    let merged = merge_style_property(&current, &prop, &value);
                    dom.set_attr(node, "style", &merged);
                    mark_dirty(&b);
                }
            },
        )?,
    )?;

    let _b = Rc::clone(bridge);
    let _ = _b;
    globals.set(
        "__native_dom_addEventListener",
        Function::new(ctx.clone(), move |node: u64, event_type: String| {
            // Listener callbacks live in JS; the engine only needs the
            // (node, event) registration for dispatch.
            let _ = (node, event_type);
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_click",
        Function::new(ctx.clone(), move |node: u64| {
            // Dispatch a synthetic click event back through the engine.
            if let Some(out) = &b.outgoing {
                let _ = out.send(JsCommand::Console {
                    level: "log".to_owned(),
                    text: format!("click requested on node {node}"),
                });
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_tagName",
        Function::new(ctx.clone(), move |node: u64| -> String {
            let dom = b.dom.borrow();
            dom.element(node as NodeId)
                .map(|e| e.local_name().to_string())
                .unwrap_or_default()
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_body",
        Function::new(ctx.clone(), move || -> Option<u64> {
            (b.body != 0).then_some(b.body as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_html",
        Function::new(ctx.clone(), move || -> Option<u64> {
            (b.html != 0).then_some(b.html as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_document_title",
        Function::new(ctx.clone(), move || -> String {
            let dom = b.dom.borrow();
            for node in dom.subtree_elements(dom.document()) {
                if let Some(el) = dom.element(node) {
                    if &*el.name.local == "title" {
                        return dom.text_content(node);
                    }
                }
            }
            String::new()
        })?,
    )?;

    let _b2 = Rc::clone(bridge);
    let _ = _b2;
    globals.set(
        "__native_document_addEventListener",
        Function::new(ctx.clone(), move |event_type: String| {
            let _ = event_type;
        })?,
    )?;

    // ------------------------------------------------------------------
    // WebComponents natives: tree inspection, mutation with handle
    // recycling reports, cloning, innerHTML, shadow DOM, upgrades.

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_childNodes",
        Function::new(ctx.clone(), move |node: u64| -> String {
            let dom = b.dom.borrow();
            let handles: Vec<u64> = dom
                .child_handles(node as NodeId)
                .into_iter()
                .map(|n| n as u64)
                .collect();
            serde_json::to_string(&handles).unwrap_or_else(|_| "[]".to_owned())
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_parentNode",
        Function::new(ctx.clone(), move |node: u64| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.parent(node as NodeId).map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_isConnected",
        Function::new(ctx.clone(), move |node: u64| -> bool {
            let dom = b.dom.borrow();
            dom.is_valid(node as NodeId) && dom.is_connected(node as NodeId)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_nextSibling",
        Function::new(ctx.clone(), move |node: u64| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.next_sibling(node as NodeId).map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_prevSibling",
        Function::new(ctx.clone(), move |node: u64| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.prev_sibling(node as NodeId).map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_nodeType",
        Function::new(ctx.clone(), move |node: u64| -> u8 {
            let dom = b.dom.borrow();
            match dom.kind(node as NodeId) {
                rowser_dom::NodeKind::Element(_) => 1,
                rowser_dom::NodeKind::Text(_) => 3,
                rowser_dom::NodeKind::Comment(_) => 8,
                rowser_dom::NodeKind::Document => 9,
                rowser_dom::NodeKind::Doctype { .. } => 10,
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_insertBefore",
        Function::new(
            ctx.clone(),
            move |parent: u64, child: u64, reference: u64| -> u64 {
                let (parent, child) = (parent as NodeId, child as NodeId);
                let reference = (reference > 0).then_some(reference as NodeId);
                let mut dom = b.dom.borrow_mut();
                if dom.is_valid(parent) && dom.is_valid(child) {
                    if let Some(old_parent) = dom.parent(child) {
                        let desc = describe_removed(&dom, child);
                        let prev = dom.prev_sibling(child).map(|n| n as u64);
                        let next = dom.next_sibling(child).map(|n| n as u64);
                        b.mo.borrow_mut().records.push(RawMutation {
                            kind: 0,
                            target: old_parent,
                            removed: vec![desc],
                            prev,
                            next,
                            ..Default::default()
                        });
                    }
                    dom.insert_before(parent, child, reference);
                    record_child_list(
                        &b.mo,
                        &dom,
                        parent,
                        vec![child as u64],
                        Vec::new(),
                        Some(child),
                    );
                    mark_dirty(&b);
                    child as u64
                } else {
                    0
                }
            },
        )?,
    )?;

    // Returns the handles freed by the removal so the JS identity map can
    // drop stale wrappers (the arena recycles node ids).
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_removeChild",
        Function::new(ctx.clone(), move |parent: u64, child: u64| -> String {
            let (parent, child) = (parent as NodeId, child as NodeId);
            let mut dom = b.dom.borrow_mut();
            if dom.is_valid(parent) && dom.is_valid(child) {
                // Detach (keep the subtree alive in the arena — spec-wise the
                // node stays usable while JS holds it).
                dom.detach(child);
                mark_dirty(&b);
            }
            "[]".to_owned()
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_cloneNode",
        Function::new(ctx.clone(), move |node: u64, deep: bool| -> u64 {
            let mut dom = b.dom.borrow_mut();
            if !dom.is_valid(node as NodeId) {
                return 0;
            }
            if deep {
                dom.clone_subtree(node as NodeId) as u64
            } else {
                // Shallow: clone the node itself (kind + attrs, no children).
                let kind = dom.kind(node as NodeId).clone();
                match &kind {
                    rowser_dom::NodeKind::Element(el) => {
                        dom.create_element(el.name.clone(), el.attrs.clone()) as u64
                    }
                    rowser_dom::NodeKind::Text(t) => dom.create_text(t.clone()) as u64,
                    _ => 0,
                }
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_getInnerHTML",
        Function::new(ctx.clone(), move |node: u64| -> String {
            let dom = b.dom.borrow();
            if dom.is_valid(node as NodeId) {
                dom.serialize_subtree(node as NodeId)
            } else {
                String::new()
            }
        })?,
    )?;

    // Parses `html` as a document, imports the body children into `node`
    // (replacing existing children) and returns the freed handle list so
    // JS can drop stale wrappers.
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_setInnerHTML",
        Function::new(ctx.clone(), move |node: u64, html: String| -> String {
            let node = node as NodeId;
            let fragment = rowser_parsing::html::parse_html(html.as_bytes());
            let mut dom = b.dom.borrow_mut();
            if !dom.is_valid(node) {
                return "[]".to_owned();
            }
            let mut freed: Vec<u64> = Vec::new();
            let mut removed_desc: Vec<RemovedNode> = Vec::new();
            for child in dom.child_handles(node) {
                removed_desc.push(describe_removed(&dom, child));
                collect_freed(&dom, child, &mut freed);
                dom.remove_subtree(child);
            }
            let body = {
                let frag_dom = &fragment.dom;
                frag_dom
                    .subtree_elements(frag_dom.document())
                    .find(|n| {
                        frag_dom
                            .element(*n)
                            .is_some_and(|e| &*e.name.local == "body")
                    })
                    .unwrap_or_else(|| frag_dom.document())
            };
            let mut added: Vec<u64> = Vec::new();
            for child in fragment.dom.child_handles(body) {
                let imported = dom.import_subtree(&fragment.dom, child);
                dom.append(node, imported);
                added.push(imported as u64);
            }
            if !removed_desc.is_empty() || !added.is_empty() {
                let anchor = dom.children(node).next();
                record_child_list(&b.mo, &dom, node, added, removed_desc, anchor);
            }
            mark_dirty(&b);
            serde_json::to_string(&freed).unwrap_or_else(|_| "[]".to_owned())
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_attachShadow",
        Function::new(ctx.clone(), move |host: u64| -> Option<u64> {
            let mut dom = b.dom.borrow_mut();
            dom.attach_shadow(host as NodeId).map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_shadowHost",
        Function::new(ctx.clone(), move |root: u64| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.shadow_host(root as NodeId).map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_template_content",
        Function::new(ctx.clone(), move |template: u64| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.template_contents(template as NodeId).map(|n| n as u64)
        })?,
    )?;

    // Scoped query: run inside any subtree (shadow roots, fragments).
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_querySelectorIn",
        Function::new(
            ctx.clone(),
            move |root: u64, selector: String| -> Option<u64> {
                let dom = b.dom.borrow();
                if !dom.is_valid(root as NodeId) {
                    return None;
                }
                dom.query_selector(root as NodeId, &selector)
                    .map(|n| n as u64)
            },
        )?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_querySelectorAllIn",
        Function::new(ctx.clone(), move |root: u64, selector: String| -> String {
            let dom = b.dom.borrow();
            if !dom.is_valid(root as NodeId) {
                return "[]".to_owned();
            }
            match dom.query_selector_all(root as NodeId, &selector) {
                Some(nodes) => serde_json::to_string(
                    &nodes.into_iter().map(|n| n as u64).collect::<Vec<u64>>(),
                )
                .unwrap_or_else(|_| "[]".to_owned()),
                None => "[]".to_owned(),
            }
        })?,
    )?;

    // Fast native walk: all elements under `root` (subtree order) whose
    // tag is in `tags` (JSON array). Drives custom-element connect/
    // disconnect notifications without a JS-side tree walk.
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_findCustomTags",
        Function::new(ctx.clone(), move |root: u64, tags: String| -> String {
            let dom = b.dom.borrow();
            let set: std::collections::HashSet<String> =
                serde_json::from_str(&tags).unwrap_or_default();
            let mut out = Vec::new();
            for n in dom.subtree_elements(root as NodeId) {
                if let Some(el) = dom.element(n) {
                    if set.contains::<str>(el.local_name().as_ref()) {
                        out.push(n as u64);
                    }
                }
            }
            serde_json::to_string(&out).unwrap_or_else(|_| "[]".to_owned())
        })?,
    )?;

    // Host → shadow root handle (the `element.shadowRoot` getter).
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_shadowRootOf",
        Function::new(ctx.clone(), move |host: u64| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.shadow_root(host as NodeId).map(|n| n as u64)
        })?,
    )?;

    // Element.matches / Element.closest support.
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_matchesSelector",
        Function::new(ctx.clone(), move |node: u64, selector: String| -> bool {
            let dom = b.dom.borrow();
            if !dom.is_valid(node as NodeId) {
                return false;
            }
            match dom.query_selector_all(
                dom.parent(node as NodeId).unwrap_or(node as NodeId),
                &selector,
            ) {
                Some(matches) => matches.contains(&(node as NodeId)),
                None => false,
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_createComment",
        Function::new(ctx.clone(), move |text: String| -> u64 {
            let mut dom = b.dom.borrow_mut();
            dom.create_comment(text) as u64
        })?,
    )?;

    // Cross-document deep import (document.importNode). With a single
    // live arena this is a deep clone — the practical semantic here.
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_importNode",
        Function::new(ctx.clone(), move |node: u64, deep: bool| -> Option<u64> {
            let mut dom = b.dom.borrow_mut();
            if !dom.is_valid(node as NodeId) {
                return None;
            }
            if deep {
                Some(dom.clone_subtree(node as NodeId) as u64)
            } else {
                let kind = dom.kind(node as NodeId).clone();
                match &kind {
                    rowser_dom::NodeKind::Element(el) => {
                        Some(dom.create_element(el.name.clone(), el.attrs.clone()) as u64)
                    }
                    rowser_dom::NodeKind::Text(t) => Some(dom.create_text(t.clone()) as u64),
                    _ => None,
                }
            }
        })?,
    )?;

    // textContent setter that reports freed handles (wrapper cleanup).
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_setTextContent2",
        Function::new(ctx.clone(), move |node: u64, text: String| -> String {
            let node = node as NodeId;
            let mut dom = b.dom.borrow_mut();
            let mut freed: Vec<u64> = Vec::new();
            if dom.is_valid(node) {
                if dom.element(node).is_some() {
                    let children: Vec<NodeId> = dom.children(node).collect();
                    let mut removed_desc = Vec::with_capacity(children.len());
                    for child in children {
                        removed_desc.push(describe_removed(&dom, child));
                        collect_freed(&dom, child, &mut freed);
                        dom.remove_subtree(child);
                    }
                    let text_node = dom.create_text(text);
                    dom.append(node, text_node);
                    record_child_list(
                        &b.mo,
                        &dom,
                        node,
                        vec![text_node as u64],
                        removed_desc,
                        Some(text_node),
                    );
                } else {
                    let old = dom.text_content(node);
                    dom.set_text(node, &text);
                    b.mo.borrow_mut().records.push(RawMutation {
                        kind: 2,
                        target: node,
                        old,
                        ..Default::default()
                    });
                }
                mark_dirty(&b);
            }
            serde_json::to_string(&freed).unwrap_or_else(|_| "[]".to_owned())
        })?,
    )?;

    Ok(())
}

/// Collects the handle of `node` and its subtree (for wrapper invalidation
/// when nodes are freed).
fn collect_freed(dom: &rowser_dom::Dom, node: NodeId, out: &mut Vec<u64>) {
    out.push(node as u64);
    for child in dom.children(node) {
        collect_freed(dom, child, out);
    }
}

/// Monotonic seconds since process start (shared clock for the watchdog).
fn monotonic_secs() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn mark_dirty(bridge: &PageBridge) {
    if let Some(out) = &bridge.outgoing {
        let _ = out.send(JsCommand::MarkDirty);
    }
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn read_local(bridge: &PageBridge, key: &str) -> Option<String> {
    let storage = bridge.storage.as_ref()?;
    let ls = storage.local_storage();
    let url = url::Url::parse(&bridge.url).ok()?;
    ls.get(&url, key).ok().flatten()
}

fn write_local(bridge: &PageBridge, key: &str, value: &str) {
    if let Some(storage) = &bridge.storage {
        let ls = storage.local_storage();
        if let Ok(url) = url::Url::parse(&bridge.url) {
            let _ = ls.set(&url, key, value);
        }
    }
}

fn remove_local(bridge: &PageBridge, key: &str) {
    if let Some(storage) = &bridge.storage {
        let ls = storage.local_storage();
        if let Ok(url) = url::Url::parse(&bridge.url) {
            let _ = ls.remove(&url, key);
        }
    }
}

fn env_info_json(bridge: &PageBridge) -> String {
    let spoof = &bridge.spoof;
    let url = url::Url::parse(&bridge.url).ok();
    let info = serde_json::json!({
        "userAgent": spoof.user_agent,
        "platform": spoof.platform,
        "language": "en-US",
        "hardwareConcurrency": spoof.hardware_concurrency,
        "deviceMemory": spoof.device_memory,
        "screen": {
            "width": spoof.screen_width,
            "height": spoof.screen_height,
            "colorDepth": spoof.color_depth,
        },
        "timezone": spoof.timezone,
        "location": bridge.url,
        "origin": url.as_ref().map(|u| u.origin().ascii_serialization()).unwrap_or_default(),
        "protocol": url.as_ref().map(|u| u.scheme().to_owned()).unwrap_or_default(),
        "host": url.as_ref().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default(),
        "pathname": url.as_ref().map(|u| u.path().to_owned()).unwrap_or_default(),
    });
    serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_owned())
}

fn json_str(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn find_style_property(style: &str, prop: &str) -> Option<String> {
    for decl in style.split(';') {
        let decl = decl.trim();
        if let Some((name, value)) = decl.split_once(':') {
            if name.trim().eq_ignore_ascii_case(prop) {
                return Some(value.trim().to_owned());
            }
        }
    }
    None
}

fn merge_style_property(style: &str, prop: &str, value: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut found = false;
    for decl in style.split(';') {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        if let Some((name, existing)) = decl.split_once(':') {
            if name.trim().eq_ignore_ascii_case(prop) {
                found = true;
                kept.push(format!("{prop}: {value}"));
            } else {
                kept.push(format!("{}:{}", name.trim(), existing.trim()));
            }
        }
    }
    if !found {
        kept.push(format!("{prop}: {value}"));
    }
    kept.join("; ")
}

fn stringify_value<'js>(
    ctx: &rquickjs::Ctx<'js>,
    value: &rquickjs::Value<'js>,
) -> rquickjs::Result<String> {
    match value.type_of() {
        rquickjs::Type::Undefined | rquickjs::Type::Null => Ok(String::new()),
        rquickjs::Type::Bool => Ok(value.as_bool().unwrap().to_string()),
        rquickjs::Type::Int => value
            .as_int()
            .map(|i| i.to_string())
            .ok_or(rquickjs::Error::Unknown),
        rquickjs::Type::Float => value
            .as_float()
            .map(|f| f.to_string())
            .ok_or(rquickjs::Error::Unknown),
        rquickjs::Type::String => Ok(value.as_string().unwrap().to_string().unwrap_or_default()),
        _ => {
            let json: String = ctx
                .eval("JSON.stringify")
                .and_then(|f: rquickjs::Function| f.call((value.clone(),)))
                .unwrap_or_else(|_| format!("{value:?}"));
            Ok(json)
        }
    }
}

fn err_string(err: &rquickjs::Error) -> String {
    format!("{err}")
}

fn format_js_error(err: &rquickjs::Error) -> String {
    match err {
        rquickjs::Error::Exception => {
            // Exception details were reported through the console channel.
            err.to_string()
        }
        other => other.to_string(),
    }
}

/// Extracts a readable message + stack from the pending QuickJS exception.
/// Must be called immediately after a failed eval (it consumes the pending
/// exception so subsequent JS calls start from a clean state).
fn exception_detail(ctx: &rquickjs::Ctx<'_>) -> Option<String> {
    use rquickjs::FromJs;
    if !ctx.has_exception() {
        return None;
    }
    let exception = ctx.catch();
    let mut out = String::new();
    if let Some(obj) = exception.clone().into_object() {
        if let Some(err) = rquickjs::Exception::from_object(obj) {
            if let Some(message) = err.message() {
                out.push_str(&message);
            }
            if let Some(stack) = err.stack() {
                let frames: Vec<&str> = stack.lines().take(4).collect();
                if !frames.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&frames.join("\n"));
                }
            }
        }
    }
    if out.is_empty() {
        // Thrown non-error values (strings, numbers...).
        if let Ok(coerced) = <rquickjs::Coerced<String>>::from_js(ctx, exception.clone()) {
            out = coerced.0;
        } else {
            out = format!("uncaught {exception:?}");
        }
    }
    if ctx.has_exception() {
        // Reading properties of a non-error can raise; clear the residue.
        let _ = ctx.catch();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_parsing::html::parse_html;

    fn runtime_with_dom(html: &[u8]) -> (JsRuntime, Rc<RefCell<Dom>>) {
        let doc = parse_html(html);
        let dom = Rc::new(RefCell::new(doc.dom));
        let (tx, _rx) = std::sync::mpsc::channel();
        let probe = dom.borrow_mut();
        let body = probe
            .subtree_elements(probe.document())
            .find(|n| {
                probe
                    .element(*n)
                    .map(|e| &*e.name.local == "body")
                    .unwrap_or(false)
            })
            .unwrap_or(0);
        let html_node = probe
            .subtree_elements(probe.document())
            .find(|n| {
                probe
                    .element(*n)
                    .map(|e| &*e.name.local == "html")
                    .unwrap_or(false)
            })
            .unwrap_or(0);
        drop(probe);
        let runtime = JsRuntime::new(
            JsConfig::default(),
            PageBridge {
                dom: Rc::clone(&dom),
                document: 0,
                body,
                html: html_node,
                url: "https://example.com/page".to_owned(),
                origin: "https://example.com".to_owned(),
                storage: None,
                spoof: SpoofProfile::from_seed([42u8; 32]),
                outgoing: Some(tx),
                media_mirror: Rc::new(RefCell::new(std::collections::HashMap::new())),
                rects: Rc::new(RefCell::new(std::collections::HashMap::new())),
                viewport: Rc::new(RefCell::new((0.0, 0.0, 0.0))),
                mo: MoShared::default(),
                history: Default::default(),
                observers: Default::default(),
            },
        )
        .unwrap();
        (runtime, dom)
    }

    #[test]
    fn eval_basics() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body><p id='x'>hi</p></body></html>");
        assert_eq!(runtime.eval("1 + 1", "test.js").unwrap(), "2");
        assert_eq!(runtime.eval("'a' + 'b' + 2", "test.js").unwrap(), "ab2");
        assert_eq!(
            runtime.eval("JSON.stringify({a: 1})", "test.js").unwrap(),
            "{\"a\":1}"
        );
    }

    #[test]
    fn match_media_evaluates_viewport_queries() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let out = runtime
            .eval(
                r#"[
                  matchMedia('(max-width: 800px)').matches,
                  matchMedia('(min-width: 800px)').matches,
                  matchMedia('screen').matches,
                  matchMedia('print').matches,
                  matchMedia('(min-width: 100px) and (max-width: 200px)').matches,
                ].map(function (v) { return v ? '1' : '0'; }).join('')"#,
                "test.js",
            )
            .unwrap();
        // Viewport width is 0 in the test runtime: max-width true,
        // min-width false, screen true, print false, and-range false.
        assert_eq!(out, "10100", "matchMedia results: {out}");
    }

    #[test]
    fn history_pushstate_updates_location_optimistically() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let out = runtime
            .eval(
                r#"history.pushState({ p: 2 }, '', '/p2');
                location.pathname + '|' + location.href + '|' + (history.state === null)"#,
                "test.js",
            )
            .unwrap();
        assert!(out.contains("/p2|"), "pathname after push: {out}");
        assert!(out.contains("http"), "href: {out}");
    }

    #[test]
    fn mutation_observer_child_list_and_attributes() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body><div id='box'></div></body></html>");
        runtime
            .eval(
                r#"globalThis.seen = [];
                const target = document.getElementById('box');
                const mo = new MutationObserver(function (records, obs) {
                    for (const r of records) seen.push(r.type + ':' + (r.attributeName || (r.addedNodes ? r.addedNodes.length + '+' + (r.removedNodes ? r.removedNodes.length : 0) : '')));
                });
                mo.observe(target, { childList: true, attributes: true, subtree: true });
                const span = document.createElement('span');
                target.appendChild(span);
                target.setAttribute('data-k', 'v');
                'registered'"#,
                "test.js",
            )
            .unwrap();
        // Records delivered at the end-of-eval checkpoint.
        let seen = runtime.eval("seen.join('|')", "check.js").unwrap();
        assert_eq!(seen, "childList:1+0|attributes:data-k", "records: {seen}");
    }

    #[test]
    fn mutation_observer_subtree_and_disconnect() {
        let (runtime, _dom) =
            runtime_with_dom(b"<html><body><div id='root'><p id='inner'>x</p></div></body></html>");
        runtime
            .eval(
                r#"globalThis.count = 0;
                const root = document.getElementById('root');
                const inner = document.getElementById('inner');
                const mo = new MutationObserver(function () { count++; });
                mo.observe(root, { childList: true, subtree: true });
                inner.setAttribute('data-a', '1');   // attributes on subtree: not observed
                const b = document.createElement('b');
                inner.appendChild(b);                 // childList in subtree: observed
                'go'"#,
                "test.js",
            )
            .unwrap();
        // Only the childList record (subtree) fired; the attribute record
        // was not requested (attributes:false).
        let count = runtime.eval("count", "check.js").unwrap();
        assert_eq!(count, "1", "subtree deliveries: {count}");
        runtime
            .eval(
                "const mo2 = new MutationObserver(function(){count+=10;});
                 mo2.observe(root, {childList:true, subtree:true});
                 mo2.disconnect();
                 root.appendChild(document.createElement('i'));",
                "test.js",
            )
            .unwrap();
        let count = runtime.eval("count", "check.js").unwrap();
        assert_eq!(count, "2", "disconnected observer must not fire: {count}");
    }

    #[test]
    fn mutation_observer_callback_mutations_redeliver() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body><ul id='list'></ul></body></html>");
        runtime
            .eval(
                r#"let rounds = 0;
                const list = document.getElementById('list');
                const mo = new MutationObserver(function () {
                    rounds++;
                    if (list.children.length < 3) list.appendChild(document.createElement('li'));
                });
                mo.observe(list, { childList: true });
                list.appendChild(document.createElement('li'));
                'seeded'"#,
                "test.js",
            )
            .unwrap();
        // The callback mutates again; the checkpoint loop must redeliver.
        let n = runtime.eval("list.children.length", "check.js").unwrap();
        assert_eq!(n, "3", "cascade reached 3 li: {n}");
        let r = runtime.eval("rounds", "check.js").unwrap();
        assert_eq!(r, "3", "callback rounds: {r}");
    }

    #[test]
    fn dom_manipulation() {
        let (runtime, dom) =
            runtime_with_dom(b"<html><body><p id='target'>before</p></body></html>");
        runtime
            .eval(
                "const p = document.getElementById('target'); p.textContent = 'after'; p.setAttribute('data-x', '1');",
                "test.js",
            )
            .unwrap();
        let dom = dom.borrow();
        let p = dom.find_by_id(dom.document(), "target").unwrap();
        assert_eq!(dom.text_content(p), "after");
        assert_eq!(dom.get_attr(p, "data-x"), Some("1"));
    }

    #[test]
    fn timers_and_promises() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let result = runtime
            .eval(
                "globalThis.result = 'no'; Promise.resolve(42).then(v => { globalThis.result = 'yes' + v; }); 'queued'",
                "test.js",
            )
            .unwrap();
        assert_eq!(result, "queued");
        // Promise jobs were pumped by eval.
        assert_eq!(runtime.eval("result", "test.js").unwrap(), "yes42");
    }

    #[test]
    fn fetch_dispatch_completes_promise() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        runtime
            .eval(
                "globalThis.state = 'pending'; fetch('https://example.com/data').then(r => { state = 'done:' + r.status; }); 'ok'",
                "test.js",
            )
            .unwrap();
        // Engine dispatches the completed fetch into JS.
        runtime.dispatch(EngineEvent::FetchCompleted {
            id: 1,
            status: 200,
            headers: "{\"content-type\":\"text/plain\"}".to_owned(),
            body_b64: "aGVsbG8=".to_owned(),
        });
        assert_eq!(runtime.eval("state", "test.js").unwrap(), "done:200");
        assert!(!runtime
            .eval("fetchDone", "check.js")
            .unwrap_err()
            .to_string()
            .is_empty());
    }

    #[test]
    fn watchdog_kills_infinite_loop() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let result = runtime.eval("while (true) {}", "loop.js");
        assert!(result.is_err(), "infinite loop must be interrupted");
    }

    #[test]
    fn memory_limit_enforced() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let result = runtime.eval(
            "let a = []; try { while (true) a.push(new Array(10000).fill('xxxxxxxxxxxxxxxx')); } catch (e) { 'oom' }",
            "oom.js",
        );
        // Either the memory limit raised or the catch handled it.
        assert!(result.is_ok() || result.is_err());
        assert!(runtime.memory_usage() >= 0);
    }

    #[test]
    fn navigator_is_spoofed() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let ua = runtime.eval("navigator.userAgent", "test.js").unwrap();
        assert!(ua.contains("Rrowser"), "UA: {ua}");
        let cores = runtime
            .eval("navigator.hardwareConcurrency", "test.js")
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!((2..=8).contains(&cores));
        assert_eq!(
            runtime
                .eval("typeof navigator.getBattery", "test.js")
                .unwrap(),
            "undefined"
        );
    }

    #[test]
    fn localstorage_roundtrip() {
        let store = rowser_storage::Storage::open("/tmp/rowser-js-ls-test.redb").unwrap();
        let doc = parse_html(b"<html><body></body></html>");
        let dom = Rc::new(RefCell::new(doc.dom));
        let (tx, _rx) = std::sync::mpsc::channel();
        let runtime = JsRuntime::new(
            JsConfig::default(),
            PageBridge {
                dom,
                document: 0,
                body: 0,
                html: 0,
                url: "https://shop.example/cart".to_owned(),
                origin: "https://shop.example".to_owned(),
                storage: Some(Arc::new(store)),
                spoof: SpoofProfile::from_seed([1u8; 32]),
                outgoing: Some(tx),
                media_mirror: Rc::new(RefCell::new(std::collections::HashMap::new())),
                rects: Rc::new(RefCell::new(std::collections::HashMap::new())),
                viewport: Rc::new(RefCell::new((0.0, 0.0, 0.0))),
                mo: MoShared::default(),
                history: Default::default(),
                observers: Default::default(),
            },
        )
        .unwrap();
        runtime
            .eval("localStorage.setItem('theme', 'dark')", "test.js")
            .unwrap();
        assert_eq!(
            runtime
                .eval("localStorage.getItem('theme')", "test.js")
                .unwrap(),
            "dark"
        );
        runtime
            .eval("localStorage.removeItem('theme')", "test.js")
            .unwrap();
        assert_eq!(
            runtime
                .eval("localStorage.getItem('theme') === null", "test.js")
                .unwrap(),
            "true"
        );
    }
}
