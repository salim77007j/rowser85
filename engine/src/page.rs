//! The page actor: one OS thread per tab, owning the DOM, JS runtime,
//! styles, layout and painter.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use rowser_dom::{ns, Dom, NodeId};
use rowser_js::{
    prelude, EngineEvent as JsEngineEvent, JsCommand, JsConfig, JsRuntime, PageBridge,
};
use rowser_layout::{LayoutEngine, LayoutResult, Viewport};
use rowser_media::{
    MediaEvent as PipelineEvent, MediaIngressSender, MediaNotification, MediaPipeline,
};
use rowser_parsing::cascade::{compute_styles, StyleMap};
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
    /// A dynamically-inserted <script src> finished fetching; evaluate it
    /// and fire the element's `load` (or `error`) event.
    ScriptCodeFetched {
        /// DOM handle of the script element.
        node: u64,
        /// Script source (or an error message to console.error).
        code: String,
        /// Fetch success — `true` fires `load`, `false` fires `error`.
        ok: bool,
    },
    /// A dynamically-created image finished fetching; decode it into the
    /// image map and fire the element's `load` (or `error`) event.
    ImageFetched {
        /// DOM handle of the img element.
        node: u64,
        /// Image bytes.
        body: Vec<u8>,
        /// Fetch success — `true` fires `load`, `false` fires `error`.
        ok: bool,
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
    /// Sets the tab scroll offset (absolute, CSS px).
    SetScroll(f32),
    /// Scroll wheel at a document-space point: routes to the innermost
    /// scrollable element (overflow: scroll/auto) or the page.
    Wheel {
        /// Document-space x.
        x: f32,
        /// Document-space y.
        y: f32,
        /// Scroll delta (positive = down).
        delta: f32,
    },
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
    /// Web font (@font-face src url()).
    Font,
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
            SubresourceKind::Font => rowser_networking::ResourceKind::Font,
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
        // (dirty re-renders, memory reports). With rAF callbacks pending the
        // timeout shrinks to one 60 FPS frame slot so the frame clock runs.
        let focused = page
            .state
            .focused
            .load(std::sync::atomic::Ordering::Relaxed);
        let frame_pace = focused
            && !page.pending_raf.is_empty()
            && page
                .last_raf
                .map(|t| t.elapsed() >= Duration::from_millis(12))
                .unwrap_or(true);
        let anim_pace = page.animations_running();
        let timeout = if frame_pace {
            Duration::from_millis(4)
        } else if anim_pace {
            Duration::from_millis(16)
        } else if !page.pending_raf.is_empty() {
            Duration::from_millis(12)
        } else {
            Duration::from_millis(250)
        };
        match rx.recv_timeout(timeout) {
            Ok(message) => {
                if page.handle(message) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if frame_pace {
                    page.fire_raf();
                }
                if anim_pace {
                    page.tick_animations();
                }
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

/// Interpolates one keyframes rule at `frac` (0..1) into a style patch.
fn interpolate_keyframes(
    frames: &[rowser_parsing::css::KeyframeRaw],
    frac: f32,
) -> Option<rowser_parsing::cascade::ComputedStyle> {
    if frames.is_empty() {
        return None;
    }
    // Find the bracketing keyframes.
    let mut k0: Option<&rowser_parsing::css::KeyframeRaw> = None;
    let mut k1: Option<&rowser_parsing::css::KeyframeRaw> = None;
    let mut sorted: Vec<&rowser_parsing::css::KeyframeRaw> = frames.iter().collect();
    sorted.sort_by(|a, b| {
        a.offset
            .partial_cmp(&b.offset)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for f in &sorted {
        if f.offset <= frac {
            k0 = Some(f);
        }
        if f.offset >= frac && k1.is_none() {
            k1 = Some(f);
        }
    }
    let k0 = k0.or_else(|| sorted.first().copied())?;
    let k1 = k1.or_else(|| sorted.last().copied())?;
    let span = (k1.offset - k0.offset).max(1e-6);
    let t = ((frac - k0.offset) / span).clamp(0.0, 1.0);
    let mut style = rowser_parsing::cascade::ComputedStyle::default();
    // opacity interpolation.
    match (k0.props.opacity, k1.props.opacity) {
        (Some(a), Some(b)) => style.opacity = a + (b - a) * t,
        (Some(a), None) | (None, Some(a)) => style.opacity = a,
        _ => {}
    }
    // transform: interpolate op-by-op when structurally equal, else snap.
    match (&k0.props.transform, &k1.props.transform) {
        (Some(a), Some(b)) => {
            if a.len() == b.len() {
                let ops = a
                    .iter()
                    .zip(b.iter())
                    .map(|(x, y)| interp_transform(x, y, t))
                    .collect();
                style.transform = ops;
            } else {
                style.transform = (if t < 0.5 { a } else { b }).clone();
            }
        }
        (Some(a), None) | (None, Some(a)) => style.transform = a.clone(),
        _ => {}
    }
    Some(style)
}

/// Interpolates two transform ops (numeric lerp where kinds match).
fn interp_transform(
    a: &rowser_parsing::cascade::TransformOp,
    b: &rowser_parsing::cascade::TransformOp,
    t: f32,
) -> rowser_parsing::cascade::TransformOp {
    use rowser_parsing::cascade::TransformOp as T;
    match (a, b) {
        (
            T::Translate {
                px: (ax, ay),
                pct: (apx, apy),
            },
            T::Translate {
                px: (bx, by),
                pct: (bpx, bpy),
            },
        ) => T::Translate {
            px: (ax + (bx - ax) * t, ay + (by - ay) * t),
            pct: (apx + (bpx - apx) * t, apy + (bpy - apy) * t),
        },
        (T::Scale(ax, ay), T::Scale(bx, by)) => T::Scale(ax + (bx - ax) * t, ay + (by - ay) * t),
        (T::Rotate(a), T::Rotate(b)) => T::Rotate(a + (b - a) * t),
        (T::Skew(a1, a2), T::Skew(b1, b2)) => T::Skew(a1 + (b1 - a1) * t, a2 + (b2 - a2) * t),
        _ => {
            if t < 0.5 {
                *a
            } else {
                *b
            }
        }
    }
}

/// Applies a keyframe's declarations onto an override style.
fn apply_keyframe_props(
    style: &mut rowser_parsing::cascade::ComputedStyle,
    props: &rowser_parsing::cascade::StyleProps,
) {
    if let Some(opacity) = props.opacity {
        style.opacity = opacity.clamp(0.0, 1.0);
    }
    if let Some(ops) = &props.transform {
        style.transform = ops.clone();
    }
    if let Some(filters) = &props.filters {
        style.filters = filters.clone();
    }
}

/// One running CSS animation.
#[derive(Debug, Clone)]
struct AnimationEntry {
    /// Animated element.
    node: NodeId,
    /// The animation spec.
    spec: rowser_parsing::cascade::AnimationSpec,
    /// Start time.
    start: std::time::Instant,
}

/// One running transition.
#[derive(Debug, Clone)]
struct TransitionEntry {
    /// Transitioned element.
    node: NodeId,
    /// Property group being animated (paint-side subset).
    prop: TransitionProp,
    /// Start value.
    from: f32,
    /// End value.
    to: f32,
    /// Start time.
    start: std::time::Instant,
    /// Duration in seconds.
    duration: f32,
}

/// Paint-side transitionable properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionProp {
    /// opacity.
    Opacity,
}

/// One session-history entry: URL + serialized pushState state.
#[derive(Debug, Clone, Default)]
struct HistoryEntry {
    url: String,
    state: String,
}

/// True when two URLs share scheme+host+port (pushState same-origin rule).
fn origins_match(a: &str, b: &str) -> bool {
    let origin_of = |u: &str| {
        url::Url::parse(u)
            .ok()
            .map(|p| p.origin().ascii_serialization())
            .unwrap_or_default()
    };
    let (oa, ob) = (origin_of(a), origin_of(b));
    !oa.is_empty() && oa == ob
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
    layout: Option<std::sync::Arc<LayoutResult>>,
    display_list: Option<std::sync::Arc<rowser_rendering::DisplayList>>,
    /// Layout-inputs fingerprint (dom version + css fp + viewport +
    /// intrinsic sizes + font generation, WITHOUT interaction state) at
    /// the last pass through styles/layout (Group E). Equal ⇒ the round
    /// is paint-only and skips compute_styles + the taffy solve entirely
    /// (canvas draw loops, find highlights, settle ticks).
    layout_fp: Option<u64>,
    /// Interaction-state fingerprint (:hover/:active/:focus/:visited) at
    /// the last styles pass — styles depend on it while `dom.version`
    /// does not cover it.
    state_fp: u64,
    /// Display-list generation bookkeeping: `paint_gen` is bumped by every
    /// producer that changes display-list inputs (fresh layout, animation
    /// override ticks, element scroll, video frames, image arrivals, SVG
    /// raster refresh); `dl_gen` is the generation the cached list was
    /// built at. Equal ⇒ repaint reuses the cached list (page scroll and
    /// canvas draws reuse it by design).
    paint_gen: u64,
    dl_gen: Option<u64>,
    /// Cached merged image map (images + SVG rasters; canvases resolve
    /// lazily at raster) + the `image_map_gen` it was built at.
    image_map_cache: Option<ImageMap>,
    image_map_gen: Option<u64>,
    /// Canvas element node ids, cached per dom version (the intrinsic-size
    /// scan walks the whole document; canvas draw loops do not bump the
    /// version, so the cached list keeps that walk off the hot path).
    canvas_nodes: Option<(u64, Vec<NodeId>)>,
    images: ImageMap,
    /// Live canvas surfaces shared with the JS runtime (Group D).
    canvas_registry: rowser_rendering::canvas2d::CanvasRegistryShared,
    /// Mirror of `images` shared with the JS bridge (drawImage + naturalWidth).
    image_mirror: rowser_js::ImageMirrorShared,
    /// Decoded background-image layers per node (one slot per layer).
    background_images: rowser_rendering::display_list::BackgroundImageMap,
    /// URL → (node, layer) requests in flight for background images.
    pending_bg: HashMap<String, Vec<(NodeId, usize)>>,
    /// Background image URLs already requested (no re-request loops).
    bg_requested: std::collections::HashSet<String>,
    /// Inline `<svg>` rasters keyed by element, re-rendered when the layout
    /// rect or subtree signature changes (Group C: SVG at device-pixel size).
    svg_rasters: HashMap<NodeId, SvgRasterEntry>,
    /// Device pixel ratio for rasterization density (HiDPI; 1.0 default,
    /// `ROWSER_DPR` env override).
    device_pixel_ratio: f32,
    /// Per-element scroll offsets for overflow: scroll/auto containers.
    element_scroll: rowser_rendering::display_list::ElementScrollMap,
    /// @keyframes rules by animation name (refreshed per render).
    anim_keyframes: HashMap<String, Vec<rowser_parsing::css::KeyframeRaw>>,
    /// Active CSS animations: (node, spec, start).
    anim_active: Vec<AnimationEntry>,
    /// Animation identities already started (restart control).
    anim_seen: std::collections::HashSet<(NodeId, String, usize)>,
    /// Style overrides from running animations (applied at repaint).
    anim_overrides: HashMap<NodeId, rowser_parsing::cascade::ComputedStyle>,
    /// Previous computed styles (transition diffing base).
    prev_styles: Option<StyleMap>,
    /// Transitions in flight: (node, property, from, to, start, duration).
    transitions: Vec<TransitionEntry>,

    layout_engine: LayoutEngine,
    /// Parsed-stylesheet cache: (fingerprint of css_texts + media, sheets).
    /// Reparsing every stylesheet on every re-render is the dominant cost
    /// for JS-heavy pages that dirty the DOM continuously.
    css_cache: Option<(u64, Vec<ParsedStylesheet>)>,
    painter: Painter,
    url: String,
    pending: HashMap<String, SubresourceKind>,
    css_texts: Vec<String>,
    /// Base URL of each link-fetched stylesheet (parallel to css_texts) —
    /// resolves @font-face src URLs.
    css_bases: Vec<String>,
    /// Web-font URLs already registered (loaded or permanently failed).
    fonts_done: std::collections::HashSet<String>,
    /// Font URL → CSS family (populated when the fetch is issued, consumed
    /// when the bytes arrive).
    font_url_family: HashMap<String, String>,
    scripts: Vec<(Option<String>, String, NodeId)>,
    viewport: Viewport,
    scroll_y: f32,
    suspended: bool,
    dirty: bool,
    rendered_dom_version: u64,
    /// Consecutive dirty re-renders without a quiet period (sustained
    /// JS-driven mutation loops: rAF animations, observer reactions).
    dirty_streak: u32,
    /// Duration of the most recent full render; slow pages pace their
    /// re-renders instead of wedging the machine (see idle()).
    last_render_cost: std::time::Duration,
    /// Earliest time the next slow-page render may run (pacing floor).
    render_not_before: Option<std::time::Instant>,
    last_memory_report: std::time::Instant,
    navigating: bool,
    /// Session history (entries carry the pushState state blob).
    history: Vec<HistoryEntry>,
    /// Current position in the session history.
    history_pos: usize,
    /// A history traversal is in flight: fire popstate once scripts run.
    popstate_pending: bool,
    /// Find-in-page match rectangles (document coordinates).
    find_matches: Vec<rowser_rendering::Rect>,
    /// Index of the active find match.
    active_match: Option<usize>,
    /// The current find query ("" = no search).
    find_query: String,
    /// Script identities (URL or inline-N) already executed for this
    /// document — Chrome's "execute once" semantics.
    executed_scripts: std::collections::HashSet<String>,
    /// Watchdog: when a navigation's subresources have not all settled by
    /// this deadline, the page force-completes the load (render with what
    /// arrived) instead of spinning on the loading screen forever.
    load_deadline: Option<std::time::Instant>,
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
    /// MutationObserver state shared with the JS runtime.
    mo_state: rowser_js::MoShared,
    /// History mirror shared with the JS runtime.
    history_mirror: rowser_js::HistoryMirrorShared,
    /// Intersection/ResizeObserver registrations shared with the runtime.
    observers_state: rowser_js::ObserversShared,
    /// Per-tab sessionStorage shared with the runtime.
    session_store: Rc<RefCell<HashMap<String, String>>>,
    /// Pending rAF callback ids (fired on the frame clock).
    pending_raf: Vec<u64>,
    /// Last rAF frame time (pacing).
    last_raf: Option<std::time::Instant>,
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
            layout_fp: None,
            state_fp: 0,
            paint_gen: 0,
            dl_gen: None,
            image_map_cache: None,
            image_map_gen: None,
            canvas_nodes: None,
            images: ImageMap::new(),
            canvas_registry: rowser_rendering::canvas2d::new_registry(),
            image_mirror: Rc::new(RefCell::new(ImageMap::new())),
            svg_rasters: HashMap::new(),
            device_pixel_ratio: std::env::var("ROWSER_DPR")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|d| *d > 0.0 && *d <= 4.0)
                .unwrap_or(1.0),
            background_images: HashMap::new(),
            pending_bg: HashMap::new(),
            bg_requested: std::collections::HashSet::new(),
            element_scroll: HashMap::new(),
            anim_keyframes: HashMap::new(),
            anim_active: Vec::new(),
            anim_seen: std::collections::HashSet::new(),
            anim_overrides: HashMap::new(),
            prev_styles: None,
            transitions: Vec::new(),
            css_cache: None,
            layout_engine: LayoutEngine::new(),
            painter: Painter::new(),
            url: String::new(),
            pending: HashMap::new(),
            css_texts: Vec::new(),
            css_bases: Vec::new(),
            fonts_done: std::collections::HashSet::new(),
            font_url_family: HashMap::new(),
            scripts: Vec::new(),
            viewport: Viewport::default(),
            scroll_y: 0.0,
            suspended: false,
            dirty: false,
            rendered_dom_version: 0,
            dirty_streak: 0,
            last_render_cost: std::time::Duration::ZERO,
            render_not_before: None,
            last_memory_report: std::time::Instant::now(),
            navigating: false,
            history: Vec::new(),
            history_pos: 0,
            popstate_pending: false,
            find_matches: Vec::new(),
            active_match: None,
            find_query: String::new(),
            executed_scripts: std::collections::HashSet::new(),
            load_deadline: None,
            media_slots: HashMap::new(),
            mse_sources: HashMap::new(),
            sb_lanes: HashMap::new(),
            video_frames: ImageMap::new(),
            media_mirror: Rc::new(RefCell::new(HashMap::new())),
            rect_mirror: Rc::new(RefCell::new(HashMap::new())),
            viewport_mirror: Rc::new(RefCell::new((0.0, 0.0, 0.0))),
            mo_state: rowser_js::MoShared::default(),
            history_mirror: rowser_js::HistoryMirrorShared::default(),
            observers_state: rowser_js::ObserversShared::default(),
            session_store: Rc::new(RefCell::new(HashMap::new())),
            pending_raf: Vec::new(),
            last_raf: None,
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
                let kind = self.subresource_fetched(url, body, &headers);
                // The engine-side `pending` count in this message is racy for
                // the DOCUMENT completion: the page issues its subresource
                // batch only while handling this very message, so the engine
                // counted 0 in-flight at send time and reported "all done" —
                // painting the page before any stylesheet arrived (the
                // unstyled first paint). The page's own pending map is the
                // source of truth; the document kind self-manages completion
                // inside document_fetched.
                let _ = pending;
                let was_document = matches!(kind, Some(SubresourceKind::Document));
                if !was_document && self.pending.is_empty() && self.dom.is_some() {
                    self.subresources_complete();
                }
            }
            Message::SubresourceFailed { url, pending, .. } => {
                self.pending.remove(&url);
                let _ = pending;
                // A failed parser-inserted <script src> fires `error` on
                // the element (async error paths depend on it).
                if let Some((_, _, node)) = self
                    .scripts
                    .iter()
                    .find(|(src, code, _)| src.as_deref() == Some(url.as_str()) && code.is_empty())
                    .cloned()
                {
                    if node != 0 {
                        if let Some(js) = &self.js {
                            let _ = js.eval(
                                &format!(
                                    "globalThis.__fireElementEvent && __fireElementEvent({node}, 'error');"
                                ),
                                "script-error-event.js",
                            );
                        }
                    }
                }
                // Page-side truth (same race as above): only complete when
                // nothing we asked for is still outstanding. A failed
                // *document* leaves dom empty — subresources_complete ends
                // the loading state for the error page.
                if self.pending.is_empty() {
                    self.subresources_complete();
                }
            }
            Message::WorkerScriptFetched { worker, code } => {
                self.spawn_worker(worker, code);
            }
            Message::ScriptCodeFetched { node, code, ok } => {
                self.run_dynamic_script(node, code, ok);
            }
            Message::ImageFetched { node, body, ok } => {
                self.image_fetched(node, body, ok);
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
            Message::Wheel { x, y, delta } => self.wheel(x, y, delta),
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
            // NOTE: ScriptFetch must NOT be handled here — it falls into the
            // `other` arm below and is forwarded to the engine, whose
            // network task fetches the bytes and replies with
            // Message::ScriptCodeFetched.
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
            JsCommand::RafStart { id } => {
                if !self.pending_raf.contains(&id) {
                    self.pending_raf.push(id);
                }
            }
            JsCommand::RafClear { id } => {
                self.pending_raf.retain(|i| *i != id);
            }
            JsCommand::ScrollTo { x: _, y } => {
                // Programmatic scroll: clamp + repaint like the UI wheel path.
                let max = self
                    .layout
                    .as_ref()
                    .map(|l| (l.content_size.1 - self.viewport.height).max(0.0))
                    .unwrap_or(0.0);
                self.scroll_y = y.clamp(0.0, max);
                self.viewport_mirror.borrow_mut().0 = self.scroll_y;
                self.repaint();
                // Sticky/observer refresh runs on the next dirty render.
            }
            JsCommand::HistoryPush { state, url } => self.history_push(state, url),
            JsCommand::HistoryReplace { state, url } => self.history_replace(state, url),
            JsCommand::HistoryGo { delta } => self.go_history(delta),
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

    /// `history.pushState(state, title, url)` — same-document entry: no
    /// navigation, but the address bar, `location`, and the entry list
    /// all move. The runtime is kept (SPA state survives).
    fn history_push(&mut self, state: String, url: String) {
        let resolved = if url.is_empty() {
            self.url.clone()
        } else {
            url::Url::parse(&self.url)
                .and_then(|base| base.join(&url))
                .map(|joined| joined.to_string())
                .unwrap_or(url)
        };
        // Only same-origin, same-path-family URLs are honored like Chrome
        // (cross-origin pushState throws there; we keep the engine simple
        // and just ignore such URLs, keeping the current one).
        let same_origin = [(&resolved, &self.url)]
            .iter()
            .all(|(a, b)| origins_match(a, b));
        let new_url = if same_origin {
            resolved
        } else {
            self.url.clone()
        };
        self.history.truncate(self.history_pos + 1);
        self.history.push(HistoryEntry {
            url: new_url.clone(),
            state,
        });
        self.history_pos = self.history.len() - 1;
        self.url = new_url.clone();
        self.sync_history_mirror();
        self.snapshot(|snapshot| snapshot.url = new_url.clone());
        if let Some(js) = &self.js {
            js.dispatch(JsEngineEvent::LocationChanged { url: new_url });
        }
    }

    /// `history.replaceState(state, title, url)`.
    fn history_replace(&mut self, state: String, url: String) {
        let resolved = if url.is_empty() {
            self.url.clone()
        } else {
            url::Url::parse(&self.url)
                .and_then(|base| base.join(&url))
                .map(|joined| joined.to_string())
                .unwrap_or(url)
        };
        let same_origin = origins_match(&resolved, &self.url);
        let new_url = if same_origin {
            resolved
        } else {
            self.url.clone()
        };
        if let Some(entry) = self.history.get_mut(self.history_pos) {
            entry.url = new_url.clone();
            entry.state = state;
        } else {
            self.history.push(HistoryEntry {
                url: new_url.clone(),
                state,
            });
        }
        self.url = new_url.clone();
        self.sync_history_mirror();
        self.snapshot(|snapshot| snapshot.url = new_url.clone());
        if let Some(js) = &self.js {
            js.dispatch(JsEngineEvent::LocationChanged { url: new_url });
        }
    }

    fn navigate(&mut self, url: String) {
        self.start_navigation(url, true);
    }

    /// Starts a navigation, optionally pushing it onto the session history.
    fn start_navigation(&mut self, mut url: String, push_history: bool) {
        // Normalize through the url crate: root paths gain their trailing
        // slash, default ports drop, percent-encoding canonicalizes —
        // `location.href` then matches Chrome byte-for-byte.
        if let Ok(normalized) = url::Url::parse(&url) {
            url = normalized.to_string();
        }
        tracing::debug!(target: "rowser::engine", "tab {} navigating to {url}", self.state.tab);
        self.navigating = true;
        self.reset_page();
        self.url = url.clone();
        if push_history && !url.is_empty() {
            self.history.truncate(self.history_pos);
            self.history.push(HistoryEntry {
                url: url.clone(),
                state: "null".to_owned(),
            });
            self.history_pos = self.history.len() - 1;
        }
        self.sync_history_mirror();
        let (back, forward) = self.history_state();
        self.snapshot(|snapshot| {
            snapshot.url = url.clone();
            snapshot.loading = true;
            snapshot.can_go_back = back;
            snapshot.can_go_forward = forward;
        });
        self.pending.insert(url.clone(), SubresourceKind::Document);
        // Watchdog: if subresources have not settled by this point, render
        // with whatever arrived (bounded loading — matches the "render what
        // you have" behaviour of mainstream browsers on slow networks).
        self.load_deadline = Some(std::time::Instant::now() + Duration::from_secs(15));
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
        let url = self.history[target as usize].url.clone();
        self.popstate_pending = true;
        self.start_navigation(url, false);
    }

    /// Publishes (length, current state) to the JS-visible history mirror.
    fn sync_history_mirror(&self) {
        *self.history_mirror.borrow_mut() = rowser_js::HistoryMirror {
            len: self.history.len() as u32,
            state: self
                .history
                .get(self.history_pos)
                .map(|e| e.state.clone())
                .unwrap_or_else(|| "null".to_owned()),
        };
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
            // :focus state — focusable targets (links, form controls,
            // tabindex carriers) receive focus on click.
            let focusable = dom
                .element(node)
                .map(|el| {
                    matches!(
                        &*el.name.local,
                        "a" | "button" | "input" | "textarea" | "select" | "summary"
                    ) || dom.get_attr(node, "tabindex").is_some()
                })
                .unwrap_or(false);
            let changed = dom
                .interaction_state
                .borrow()
                .focus
                .map(|f| f != node)
                .unwrap_or(focusable);
            if focusable && changed {
                dom.interaction_state.borrow_mut().focus = Some(node);
                self.dirty = true;
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
        // :hover chain maintenance — the hit node plus its ancestors. Only
        // a CHANGED chain invalidates styles (recompute + repaint).
        if let Some(dom) = self.dom.as_ref() {
            let dom_rc = Rc::clone(dom);
            let dom = dom_rc.borrow();
            let chain: Vec<rowser_dom::NodeId> = match node {
                Some(node) => {
                    let mut chain = vec![node];
                    let mut walk = dom.parent_element(node);
                    while let Some(up) = walk {
                        chain.push(up);
                        walk = dom.parent_element(up);
                    }
                    chain
                }
                None => Vec::new(),
            };
            let changed = {
                let state = dom.interaction_state.borrow();
                state.hover != chain
            };
            if changed {
                dom.interaction_state.borrow_mut().hover = chain;
                drop(dom);
                self.dirty = true;
            }
        }
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

    /// Scroll-wheel routing: the innermost scrollable element under the
    /// point consumes the delta first; unconsumed delta bubbles to outer
    /// scrollables and finally the page.
    fn wheel(&mut self, x: f32, y: f32, delta: f32) {
        if delta == 0.0 {
            return;
        }
        let Some(layout) = self.layout.clone() else {
            return;
        };
        let Some(styles) = self.style_map.clone() else {
            return;
        };
        let Some(dom_rc) = self.dom.clone() else {
            return;
        };
        let chain: Vec<NodeId> = {
            let dom = dom_rc.borrow();
            let mut chain = Vec::new();
            let mut current = self.hit_node(x, y);
            while let Some(node) = current {
                chain.push(node);
                current = dom.parent_element(node);
            }
            chain
        };
        // Try each scrollable ancestor, innermost first.
        let mut remaining = delta;
        let dom = dom_rc.borrow();
        for node in &chain {
            let Some(style) = styles.get(*node) else {
                continue;
            };
            if !(style.overflow_x.scrollable() || style.overflow_y.scrollable()) {
                continue;
            }
            let Some(rect) = layout.rects.get(node) else {
                continue;
            };
            let b = &style.borders;
            let clip_h = (rect.h - b.top.width - b.bottom.width).max(1.0);
            // Content extent: max bottom over the subtree's rects.
            let mut content_bottom = rect.y + rect.h;
            for descendant in dom.descendants(*node) {
                if let Some(dr) = layout.rects.get(&descendant) {
                    content_bottom = content_bottom.max(dr.y + dr.h);
                }
            }
            let content_h = (content_bottom - rect.y).max(clip_h);
            let max_scroll = (content_h - clip_h).max(0.0);
            if max_scroll <= 0.0 {
                continue;
            }
            let current = self
                .element_scroll
                .get(node)
                .map(|(_, sy)| *sy)
                .unwrap_or(0.0);
            let next = (current + remaining).clamp(0.0, max_scroll);
            if (next - current).abs() < 0.5 {
                continue; // at the rail's end: bubble outward
            }
            let consumed = next - current;
            remaining -= consumed;
            self.element_scroll.entry(*node).or_insert((0.0, 0.0)).1 = next;
            // Element scroll offsets feed PushClip in the display list.
            self.paint_gen = self.paint_gen.wrapping_add(1);
            self.repaint();
            if remaining.abs() < 0.5 {
                return; // fully consumed
            }
        }
        // Page scroll fallback.
        let max = self
            .layout
            .as_ref()
            .map(|l| (l.content_size.1 - self.viewport.height).max(0.0))
            .unwrap_or(0.0);
        let next = (self.scroll_y + remaining).clamp(0.0, max);
        if (next - self.scroll_y).abs() > 0.5 {
            self.scroll_y = next;
            self.viewport_mirror.borrow_mut().0 = self.scroll_y;
            let _ = self.state.event_tx.send(EngineEvent::ScrollChanged {
                tab: self.state.tab,
                scroll_y: self.scroll_y,
            });
            self.repaint();
        }
    }

    /// Collects background-image URLs from computed styles and requests
    /// the not-yet-fetched ones (data: URLs decode synchronously).
    fn collect_background_images(&mut self, styles: &StyleMap) {
        let Some(dom_rc) = self.dom.clone() else {
            return;
        };
        let dom = dom_rc.borrow();
        // Pass 1 (immutable): decide which (node, layer) need fetching.
        let mut wanted: Vec<(NodeId, usize, String)> = Vec::new();
        for (node, style) in &styles.styles {
            if style.background_layers.is_empty() {
                continue;
            }
            if dom.element(*node).is_none() {
                continue;
            }
            let already = self.background_images.get(node);
            for (i, layer) in style.background_layers.iter().enumerate() {
                let rowser_parsing::cascade::BackgroundImageSpec::Url(url) = &layer.image else {
                    continue;
                };
                if url.is_empty() {
                    continue;
                }
                if already.is_some_and(|layers| layers.get(i).is_some_and(|img| img.is_some())) {
                    continue;
                }
                if self.bg_requested.contains(url) {
                    continue;
                }
                wanted.push((*node, i, url.clone()));
            }
        }
        if wanted.is_empty() {
            return;
        }
        // Pass 2 (mutable): request / decode.
        let mut requests: Vec<(String, SubresourceKind)> = Vec::new();
        let mut data_decodes: Vec<(NodeId, usize, String)> = Vec::new();
        for (node, i, url) in wanted {
            self.bg_requested.insert(url.clone());
            if url.starts_with("data:") {
                data_decodes.push((node, i, url));
                continue;
            }
            let resolved = self.resolve_url(&url);
            self.pending_bg
                .entry(resolved.clone())
                .or_default()
                .push((node, i));
            requests.push((resolved, SubresourceKind::Image));
        }
        for (node, i, url) in data_decodes {
            if let Some(image) = DecodedImage::decode(extract_data_payload(&url).as_bytes())
                .or_else(|| {
                    rowser_rendering::decode_svg_bytes(extract_data_payload(&url).as_bytes())
                })
            {
                let entry = self.background_images.entry(node).or_default();
                while entry.len() <= i {
                    entry.push(None);
                }
                entry[i] = Some(Arc::new(image));
                self.paint_gen = self.paint_gen.wrapping_add(1);
            }
        }
        if !requests.is_empty() {
            for (url, kind) in &requests {
                self.pending.insert(url.clone(), *kind);
            }
            self.request_subresources(&requests);
        }
    }

    fn reset_page(&mut self) {
        self.css_bases.clear();
        self.fonts_done.clear();
        self.font_url_family.clear();
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
        self.layout_fp = None;
        self.state_fp = 0;
        self.dl_gen = None;
        self.image_map_cache = None;
        self.image_map_gen = None;
        self.canvas_nodes = None;
        self.images.clear();
        self.svg_rasters.clear();
        self.background_images.clear();
        self.element_scroll.clear();
        // New document: every shape from the old document is dead weight.
        self.layout_engine.shape_cache.clear();
        self.paint_gen = self.paint_gen.wrapping_add(1);
        self.css_texts.clear();
        self.css_bases.clear();
        self.fonts_done.clear();
        self.font_url_family.clear();
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

    /// The CSS family a pending font URL was requested for.
    fn pending_font_family(&self, url: &str) -> Option<String> {
        self.font_url_family.get(url).cloned()
    }

    /// Issues fetches for every unloaded @font-face source in the parsed
    /// sheets. Called from render_pipeline once per (css set); a font URL
    /// is requested at most once per document (fonts_done).
    fn request_web_fonts(&mut self, sheets: &[ParsedStylesheet], css_base_of_sheet: &[String]) {
        let mut requests: Vec<(String, SubresourceKind)> = Vec::new();
        let doc_url = self.url.clone();
        for (index, sheet) in sheets.iter().enumerate() {
            if sheet.font_faces.is_empty() {
                continue;
            }
            let base = css_base_of_sheet
                .get(index)
                .cloned()
                .unwrap_or_else(|| doc_url.clone());
            for face in &sheet.font_faces {
                for source in &face.sources {
                    let resolved = self.resolve_url_against(&base, source);
                    if self.fonts_done.contains(&resolved)
                        || self.pending.contains_key(&resolved)
                        || self.font_url_family.contains_key(&resolved)
                    {
                        continue;
                    }
                    if source.starts_with("data:") {
                        // data: URLs decode synchronously.
                        if let Some(bytes) = decode_data_url_bytes(source) {
                            if crate::font_face::register_font_bytes(
                                &mut self.layout_engine.font_system,
                                &face.family,
                                &bytes,
                            ) {
                                self.dirty = true;
                            }
                        }
                        self.fonts_done.insert(resolved);
                        continue;
                    }
                    self.font_url_family
                        .insert(resolved.clone(), face.family.clone());
                    requests.push((resolved, SubresourceKind::Font));
                }
            }
        }
        if !requests.is_empty() {
            tracing::debug!(
                target: "rowser::engine",
                "requesting {} web fonts",
                requests.len()
            );
            // Track the fetches in `pending`: `subresource_fetched` drops
            // the body when the URL is not in `pending` (`pending.remove`
            // returns None → early return), so font bytes arriving for an
            // untracked URL were silently discarded and the page kept the
            // fallback font forever. This mirrors the DOM scan path, which
            // inserts every request before calling request_subresources.
            for (url, _) in &requests {
                self.pending.insert(url.clone(), SubresourceKind::Font);
            }
            self.request_subresources(&requests);
        }
    }

    /// Resolves `href` against an explicit `base` (absolute hrefs pass
    /// through; protocol-relative URLs resolve against the base).
    fn resolve_url_against(&self, base: &str, href: &str) -> String {
        if href.contains("://") || href.starts_with("data:") {
            return href.to_owned();
        }
        if let Ok(base) = url::Url::parse(base) {
            if let Ok(joined) = base.join(href) {
                return joined.to_string();
            }
        }
        self.resolve_url(href)
    }

    fn top_site(&self) -> Option<String> {
        url::Url::parse(&self.url)
            .ok()
            .and_then(|url| url.host_str().map(rowser_privacy::psl_registrable))
            .map(|site| site.to_string())
            .or_else(|| Some(self.url.clone()))
    }

    fn subresource_fetched(
        &mut self,
        url: String,
        body: Vec<u8>,
        headers: &str,
    ) -> Option<SubresourceKind> {
        let kind = self.pending.remove(&url)?;
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
                Some(SubresourceKind::Document)
            }
            SubresourceKind::Stylesheet => {
                let text = String::from_utf8_lossy(&body).into_owned();
                self.css_texts.push(text);
                self.css_bases.push(url.clone());
                Some(SubresourceKind::Stylesheet)
            }
            SubresourceKind::Font => {
                // Register the face (decode + fontdb + alias) and re-render:
                // fonts change metrics, so this is a layout-level update.
                // The CSS family is recoverable from the rule set.
                if let Some(family) = self.pending_font_family(&url) {
                    if crate::font_face::register_font_bytes(
                        &mut self.layout_engine.font_system,
                        &family,
                        &body,
                    ) {
                        self.style_map = None; // force re-style in render_pipeline
                        self.dirty = true;
                        self.render_pipeline();
                    }
                }
                self.fonts_done.insert(url.clone());
                Some(SubresourceKind::Font)
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
                Some(SubresourceKind::Script)
            }
            SubresourceKind::Image => {
                if let Some(image) = DecodedImage::decode(&body)
                    .or_else(|| rowser_rendering::decode_svg_bytes(&body))
                {
                    // Attach to the img node that referenced it. The fetch
                    // used the RESOLVED url (protocol-relative "//host/..."
                    // became "https://host/..."), so match resolution-side
                    // too: comparing against the raw attribute left every
                    // protocol-relative image undelivered.
                    let owner = self.dom.as_ref().and_then(|dom_rc| {
                        let dom = dom_rc.borrow();
                        let base = self.url.clone();
                        let mut found = None;
                        for node in dom.subtree_elements(dom.document()) {
                            let is_img =
                                dom.element(node).is_some_and(|el| &*el.name.local == "img");
                            if !is_img {
                                continue;
                            }
                            if let Some(src) = dom.get_attr(node, "src") {
                                let resolved = match url::Url::parse(src) {
                                    Ok(_) => src.to_owned(),
                                    Err(_) => url::Url::parse(&base)
                                        .and_then(|b| b.join(src))
                                        .map(|joined| joined.to_string())
                                        .unwrap_or_else(|_| src.to_owned()),
                                };
                                if resolved == url {
                                    found = Some(node);
                                    break;
                                }
                            }
                        }
                        found
                    });
                    if let Some(node) = owner {
                        self.images.insert(node, Arc::new(image));
                        self.paint_gen = self.paint_gen.wrapping_add(1);
                    }
                    self.sync_image_mirror();
                    self.dirty = true;
                }
                // Background-image layer delivery: the fetch URL matches
                // pending_bg entries recorded at style time.
                let layer_hits = self.pending_bg.remove(&url).unwrap_or_default();
                let mut delivered_bg = !layer_hits.is_empty();
                for (node, layer_index) in layer_hits {
                    if let Some(image) = DecodedImage::decode(&body)
                        .or_else(|| rowser_rendering::decode_svg_bytes(&body))
                    {
                        let entry = self.background_images.entry(node).or_default();
                        while entry.len() <= layer_index {
                            entry.push(None);
                        }
                        entry[layer_index] = Some(Arc::new(image));
                        delivered_bg = true;
                    }
                }
                if delivered_bg {
                    self.paint_gen = self.paint_gen.wrapping_add(1);
                    self.dirty = true;
                }
                Some(SubresourceKind::Image)
            }
            SubresourceKind::Media => {
                // Media never uses the buffered subresource path (it would
                // hold whole videos in memory); the streaming loader feeds
                // Message::MediaData instead. A late completion here means
                // the navigation moved on: drop the bytes.
                Some(SubresourceKind::Media)
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
                            )
                            .or_else(|| {
                                rowser_rendering::decode_svg_bytes(
                                    extract_data_payload(&src).as_bytes(),
                                )
                            }) {
                                self.images.insert(node, Arc::new(image));
                                self.paint_gen = self.paint_gen.wrapping_add(1);
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
        self.load_deadline = None;
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
        // History traversal (back/forward): popstate fires after the new
        // document's scripts have booted (SPA routers rely on this).
        if self.popstate_pending {
            self.popstate_pending = false;
            let state = self
                .history
                .get(self.history_pos)
                .map(|e| e.state.clone())
                .unwrap_or_else(|| "null".to_owned());
            if let Some(js) = &self.js {
                js.dispatch(JsEngineEvent::PopState { state });
            }
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
        // Base URL per entry of all_css: link sheets carry their own base
        // (for @font-face resolution); <style> elements resolve against the
        // document URL.
        let css_base_of_sheet: Vec<String> = {
            let link_count = self.css_texts.len();
            let doc_url = self.url.clone();
            let mut bases: Vec<String> = self.css_bases.clone();
            bases.resize(link_count, doc_url.clone());
            while bases.len() < all_css.len() {
                bases.push(doc_url.clone());
            }
            bases
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
        // @font-face: request (or synchronously register, for data: URLs)
        // every not-yet-loaded web font before styling/layout run.
        self.request_web_fonts(&sheets, &css_base_of_sheet);
        let stage_t0 = std::time::Instant::now();
        if trace {
            eprintln!(
                "[page-{}] css parsed: {} sheets, {} total bytes",
                self.state.tab,
                sheets.len(),
                self.css_texts.iter().map(|c| c.len()).sum::<usize>()
            );
        }
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
        // Decoded <img> natural sizes drive replaced-element sizing
        // (auto-sized images get their intrinsic box, Chrome-style).
        for (node, image) in &self.images {
            if image.width > 0 && image.height > 0 {
                intrinsic.insert(*node, (image.width as f32, image.height as f32));
            }
        }
        // <canvas> elements size from their width/height attributes
        // (default 300x150, the spec's intrinsic size). The canvas scan is
        // a full-document walk, so its node list is cached per dom version
        // (canvas draw loops never bump the version — the scan runs once).
        if let Some(dom_rc) = &self.dom {
            let dom = dom_rc.borrow();
            let version = dom.version;
            let canvas_nodes: Vec<NodeId> = match &self.canvas_nodes {
                Some((v, nodes)) if *v == version => nodes.clone(),
                _ => dom
                    .subtree_elements(dom.document())
                    .filter(|&node| {
                        dom.element(node)
                            .is_some_and(|el| &*el.name.local == "canvas")
                    })
                    .collect(),
            };
            if self.canvas_nodes.as_ref().map(|(v, _)| *v) != Some(version) {
                self.canvas_nodes = Some((version, canvas_nodes.clone()));
            }
            for node in canvas_nodes {
                let attr = |name: &str| -> f32 {
                    dom.get_attr(node, name)
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .filter(|v| *v >= 0.0)
                        .unwrap_or(match name {
                            "width" => 300.0,
                            _ => 150.0,
                        })
                };
                intrinsic.insert(node, (attr("width"), attr("height")));
            }
        }
        // ---- Group E layout gates ---------------------------------------
        // Fingerprint every input styles/layout depend on EXCEPT the
        // interaction state (hover/focus/:active/:visited — covered
        // separately below): DOM version, sheet set, viewport, intrinsic
        // sizes, web-font generation.
        let state_fp = {
            use std::hash::Hasher;
            let dom_ref = dom.borrow();
            let st = dom_ref.interaction_state.borrow();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for node in &st.hover {
                h.write_u32(*node);
            }
            if let Some(node) = st.active {
                h.write_u32(node);
            }
            if let Some(node) = st.focus {
                h.write_u32(node);
            }
            let mut visited: Vec<u32> = st.visited.iter().copied().collect();
            visited.sort_unstable();
            for node in visited {
                h.write_u32(node);
            }
            h.finish()
        };
        let inputs_fp = {
            use std::hash::Hasher;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            h.write_u64(dom.borrow().version);
            h.write_u64(fp);
            h.write_u32(self.viewport.width.to_bits());
            h.write_u32(self.viewport.height.to_bits());
            let mut entries: Vec<(NodeId, (f32, f32))> =
                intrinsic.iter().map(|(n, s)| (*n, *s)).collect();
            entries.sort_unstable_by(|a, b| {
                a.0.cmp(&b.0).then(a.1 .0.to_bits().cmp(&b.1 .0.to_bits()))
            });
            for (node, (w, ht)) in entries {
                h.write_u32(node);
                h.write_u32(w.to_bits());
                h.write_u32(ht.to_bits());
            }
            h.write_u64(rowser_layout::text::FONT_GEN.load(std::sync::atomic::Ordering::Relaxed));
            h.finish()
        };
        if self.layout_fp == Some(inputs_fp) && self.state_fp == state_fp {
            // Gate 1 — paint-only round: nothing that feeds styles or
            // layout changed since the last full pass (canvas draw loops,
            // find highlighting, settle ticks). Skip compute_styles AND
            // the taffy solve; repaint with the cached list.
            if trace {
                eprintln!(
                    "[page-{}] gate 1: paint-only round (dom v{}, {} intrinsic)",
                    self.state.tab,
                    dom.borrow().version,
                    intrinsic.len()
                );
            }
            self.dirty = false;
            self.repaint();
            self.deliver_observers();
            return;
        }
        let styles = {
            let t = std::time::Instant::now();
            let s = compute_styles(&dom.borrow(), &sheets, &media);
            if trace {
                eprintln!(
                    "[page-{}] compute_styles took {}ms",
                    self.state.tab,
                    t.elapsed().as_millis()
                );
            }
            s
        };
        if self.layout_fp == Some(inputs_fp)
            && self.state_fp != state_fp
            && self.style_map.as_ref().is_some_and(|prev| *prev == styles)
        {
            // Gate 2 — interaction-state churn with an unchanged DOM: the
            // re-computed styles are DEEP-EQUAL to the previous set (the
            // hovered element has no :hover rules, or the rules produce
            // the same values). Keep the existing layout; skip the solve.
            if trace {
                eprintln!(
                    "[page-{}] gate 2: state churn, styles identical — layout kept",
                    self.state.tab
                );
            }
            self.style_map = Some(styles);
            self.state_fp = state_fp;
            self.layout_fp = Some(inputs_fp);
            self.dirty = false;
            self.repaint();
            self.deliver_observers();
            return;
        }
        let layout = self
            .layout_engine
            .compute(&dom.borrow(), &styles, self.viewport, &intrinsic);
        // JS layout mirror refresh (getBoundingClientRect).
        {
            let mut rects = self.rect_mirror.borrow_mut();
            rects.clear();
            for (node, rect) in &layout.rects {
                rects.insert(u64::from(*node), [rect.x, rect.y, rect.w, rect.h]);
            }
        }
        if trace {
            eprintln!(
                "[page-{}] layout took {}ms ({} dom nodes)",
                self.state.tab,
                stage_t0.elapsed().as_millis(),
                dom.borrow().node_count()
            );
        }
        if trace {
            let (hits, misses, entries, bytes, lookups) = self.layout_engine.last_shape_stats;
            eprintln!(
                "[page-{}] shape cache: {} hits / {} misses ({} entries, ~{} KiB, {} lookups)",
                self.state.tab,
                hits,
                misses,
                entries,
                bytes / 1024,
                lookups
            );
        }
        // Background images: request any URL layers not yet fetched.
        self.collect_background_images(&styles);
        // CSS animations: refresh @keyframes + activate new animation
        // declarations (paint-side subset: transform/opacity/filters).
        self.refresh_animations(&styles, &sheets);
        self.style_map = Some(styles);
        self.layout = Some(std::sync::Arc::new(layout));
        self.rendered_dom_version = dom.borrow().version;
        self.layout_fp = Some(inputs_fp);
        self.state_fp = state_fp;
        // Fresh styles + layout: the display list must be rebuilt.
        self.paint_gen = self.paint_gen.wrapping_add(1);
        self.display_list = None;
        // NOTE: do NOT leave `dirty` set here. idle() clears dirty before
        // calling us; re-setting it made every tab re-render on every 250ms
        // idle tick forever (static pages, background tabs included). That
        // leak, multiplied across tabs, fed a FrameReady storm that
        // eventually wedged the UI thread (exponential event batching).
        self.dirty = false;
        self.repaint();
        // Observers: layout rects are fresh — evaluate Intersection and
        // Resize observers and deliver threshold crossings / size deltas.
        self.deliver_observers();
    }

    /// Computes Intersection/Resize observer entries from the fresh layout
    /// mirror and dispatches them into JS. IO fires on threshold crossings
    /// (or first evaluation); RO fires on border-box changes.
    fn deliver_observers(&mut self) {
        if self.observers_state.borrow().io.is_empty()
            && self.observers_state.borrow().ro.is_empty()
        {
            return;
        }
        let (scroll_y, vw, vh) = *self.viewport_mirror.borrow();
        let rects = self.rect_mirror.borrow().clone();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        let mut io_batches: Vec<(u64, String)> = Vec::new();
        {
            let mut obs = self.observers_state.borrow_mut();
            let mut pending: Vec<(u64, String)> = Vec::new();
            for reg in obs.io.iter_mut() {
                let Some(&target_rect) = rects.get(&(reg.target as u64)) else {
                    continue;
                };
                // Root: the viewport (document space) or the root element's
                // rect, expanded by rootMargin [top right bottom left].
                let root_rect = match reg.root {
                    Some(root) => rects
                        .get(&(root as u64))
                        .copied()
                        .unwrap_or([0.0, 0.0, vw, vh]),
                    None => [0.0, scroll_y, vw, vh],
                };
                let m = reg.root_margin;
                let root_box = (
                    root_rect[0] - m[3],
                    root_rect[1] - m[0],
                    root_rect[0] + root_rect[2] + m[1],
                    root_rect[1] + root_rect[3] + m[2],
                );
                let tb = (
                    target_rect[0],
                    target_rect[1],
                    target_rect[0] + target_rect[2],
                    target_rect[1] + target_rect[3],
                );
                let ix0 = tb.0.max(root_box.0);
                let iy0 = tb.1.max(root_box.1);
                let ix1 = tb.2.min(root_box.2);
                let iy1 = tb.3.min(root_box.3);
                let inter_w = (ix1 - ix0).max(0.0);
                let inter_h = (iy1 - iy0).max(0.0);
                let target_area = (target_rect[2] * target_rect[3]).max(0.0);
                let ratio = if target_area > 0.0 {
                    ((inter_w * inter_h) / target_area).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                // Fire when: first evaluation, entering/leaving visibility,
                // or crossing any threshold.
                let last = reg.last_ratio;
                let threshold_crossed = reg
                    .thresholds
                    .iter()
                    .any(|t| (ratio >= *t) != ((last >= *t) || (last < 0.0 && *t == 0.0 && false)));
                let visibility_flip = (ratio > 0.0) != (last > 0.0);
                let first = last < 0.0;
                if !first && !threshold_crossed && !visibility_flip {
                    continue;
                }
                reg.last_ratio = ratio;
                // Chrome's boundingClientRect is viewport-relative.
                let entry = format!(
                    r#"{{"target":{},"time":{:.3},"isIntersecting":{},"intersectionRatio":{:.4},"boundingClientRect":{{"x":{:.2},"y":{:.2},"width":{:.2},"height":{:.2},"top":{:.2},"right":{:.2},"bottom":{:.2},"left":{:.2}}},"rootBounds":{{"x":0,"y":0,"width":{:.2},"height":{:.2},"top":0,"right":{:.2},"bottom":{:.2},"left":0}},"intersectionRect":{{"x":{:.2},"y":{:.2},"width":{:.2},"height":{:.2},"top":{:.2},"right":{:.2},"bottom":{:.2},"left":{:.2}}}}}"#,
                    reg.target as u64,
                    now_ms,
                    ratio > 0.0,
                    ratio,
                    target_rect[0],
                    target_rect[1] - scroll_y,
                    target_rect[2],
                    target_rect[3],
                    target_rect[1] - scroll_y,
                    target_rect[0] + target_rect[2],
                    target_rect[1] + target_rect[3] - scroll_y,
                    target_rect[0],
                    vw,
                    vh,
                    vw,
                    vh,
                    ix0,
                    iy0 - scroll_y,
                    inter_w,
                    inter_h,
                    iy0 - scroll_y,
                    ix0 + inter_w,
                    iy0 + inter_h - scroll_y,
                    ix0
                );
                pending.push((reg.id, entry));
            }
            // Group entries per observer id.
            for (id, entry) in pending {
                if let Some(slot) = io_batches.iter_mut().find(|(bid, _)| *bid == id) {
                    slot.1.push(',');
                    slot.1.push_str(&entry);
                } else {
                    io_batches.push((id, format!("[{entry}]")));
                }
            }
        }
        // ResizeObserver: fire on border-box change (incl. first eval).
        let mut ro_batches: Vec<(u64, String)> = Vec::new();
        {
            let mut obs = self.observers_state.borrow_mut();
            for reg in obs.ro.iter_mut() {
                let Some(&rect) = rects.get(&(reg.target as u64)) else {
                    continue;
                };
                let changed = match reg.last_rect {
                    None => true,
                    Some(last) => {
                        (last[0] - rect[0]).abs() > 0.5
                            || (last[1] - rect[1]).abs() > 0.5
                            || (last[2] - rect[2]).abs() > 0.5
                            || (last[3] - rect[3]).abs() > 0.5
                    }
                };
                if !changed {
                    continue;
                }
                reg.last_rect = Some(rect);
                let entry = format!(
                    r#"{{"target":{},"contentRect":{{"x":{:.2},"y":{:.2},"width":{:.2},"height":{:.2},"top":{:.2},"right":{:.2},"bottom":{:.2},"left":{:.2}}}}}"#,
                    reg.target as u64,
                    rect[0],
                    rect[1] - scroll_y,
                    rect[2],
                    rect[3],
                    rect[1] - scroll_y,
                    rect[0] + rect[2],
                    rect[1] + rect[3] - scroll_y,
                    rect[0]
                );
                if let Some(slot) = ro_batches.iter_mut().find(|(bid, _)| *bid == reg.id) {
                    slot.1.push(',');
                    slot.1.push_str(&entry);
                } else {
                    ro_batches.push((reg.id, format!("[{entry}]")));
                }
            }
        }
        if let Some(js) = &self.js {
            for (id, json) in io_batches {
                js.dispatch(JsEngineEvent::IntersectFired { id, json });
            }
            for (id, json) in ro_batches {
                js.dispatch(JsEngineEvent::ResizeFired { id, json });
            }
        }
    }

    /// Refreshes @keyframes rules and activates newly-declared animations.
    fn refresh_animations(&mut self, styles: &StyleMap, sheets: &[ParsedStylesheet]) {
        self.anim_keyframes.clear();
        for sheet in sheets {
            for rule in &sheet.keyframes {
                self.anim_keyframes
                    .entry(rule.name.clone())
                    .or_insert_with(|| rule.frames.clone());
            }
        }
        let now = std::time::Instant::now();
        for (node, style) in &styles.styles {
            for (i, spec) in style.animations.iter().enumerate() {
                if spec.name.is_empty() || spec.paused {
                    continue;
                }
                let key = (*node, spec.name.clone(), i);
                if self.anim_seen.contains(&key) {
                    continue;
                }
                self.anim_seen.insert(key);
                self.anim_active.push(AnimationEntry {
                    node: *node,
                    spec: spec.clone(),
                    start: now,
                });
            }
        }
        // Transitions: diff prev vs new paint props.
        if let Some(prev) = &self.prev_styles {
            for (node, style) in &styles.styles {
                let Some(before) = prev.styles.get(node) else {
                    continue;
                };
                let relevant: Vec<&rowser_parsing::cascade::TransitionSpec> = style
                    .transitions
                    .iter()
                    .filter(|t| {
                        t.duration > 0.0 && (t.property == "all" || t.property == "opacity")
                    })
                    .collect();
                if relevant.is_empty() {
                    continue;
                }
                if (before.opacity - style.opacity).abs() > 1e-4 {
                    let duration = relevant
                        .iter()
                        .map(|t| t.duration)
                        .fold(f32::INFINITY, f32::min);
                    // Replace any running transition on this property.
                    self.transitions
                        .retain(|t| !(t.node == *node && t.prop == TransitionProp::Opacity));
                    self.transitions.push(TransitionEntry {
                        node: *node,
                        prop: TransitionProp::Opacity,
                        from: before.opacity,
                        to: style.opacity,
                        start: now,
                        duration,
                    });
                }
            }
        }
        self.prev_styles = Some(styles.clone());
    }

    /// True while animations or transitions are running (loop pacing).
    fn animations_running(&self) -> bool {
        !self.anim_active.is_empty() || !self.transitions.is_empty()
    }

    /// Advances animation time: interpolates keyframes/transitions into
    /// style overrides and repaints (no relayout — paint-side props only).
    fn tick_animations(&mut self) {
        if !self.animations_running() {
            return;
        }
        let now = std::time::Instant::now();
        // Animations: compute the override per node.
        let mut overrides: HashMap<NodeId, rowser_parsing::cascade::ComputedStyle> = HashMap::new();
        self.anim_active.retain(|entry| {
            let spec = &entry.spec;
            let t = now.duration_since(entry.start).as_secs_f32() - spec.delay;
            if t < 0.0 {
                return true; // still in delay
            }
            let total = if spec.duration > 0.0 {
                spec.duration
            } else {
                return true;
            };
            let finished = match spec.iteration_count {
                n if n.is_finite() => t >= total * n,
                _ => false,
            };
            if finished && !spec.paused {
                // Apply the final state (fill forwards approximated) and end.
                if let Some(frames) = self.anim_keyframes.get(&spec.name) {
                    if let Some(last) = frames.iter().find(|f| f.offset >= 1.0) {
                        let style = overrides.entry(entry.node).or_default();
                        apply_keyframe_props(style, &last.props);
                    }
                }
                return false;
            }
            // Map t into the current iteration + direction.
            let progress = if spec.iteration_count.is_finite() {
                t % (total * spec.iteration_count.max(1.0))
            } else {
                t % total
            };
            let mut frac = (progress / total).clamp(0.0, 1.0);
            let iteration = if total > 0.0 {
                (t / total).floor() as i64
            } else {
                0
            };
            use rowser_parsing::cascade::AnimationDirectionMode as D;
            let forward = match spec.direction {
                D::Normal => true,
                D::Reverse => false,
                D::Alternate => iteration % 2 == 0,
                D::AlternateReverse => iteration % 2 == 1,
            };
            if !forward {
                frac = 1.0 - frac;
            }
            if let Some(frames) = self.anim_keyframes.get(&spec.name) {
                if let Some(style) = interpolate_keyframes(frames, frac) {
                    let entry_style = overrides.entry(entry.node).or_default();
                    *entry_style = style;
                }
            }
            true
        });
        // Transitions: interpolate (easing: ease-out approximation).
        self.transitions.retain(|t| {
            let elapsed = now.duration_since(t.start).as_secs_f32() - 0.0;
            if elapsed >= t.duration {
                return false;
            }
            let raw = (elapsed / t.duration.max(1e-6)).clamp(0.0, 1.0);
            // ease: quadratic out.
            let eased = 1.0 - (1.0 - raw) * (1.0 - raw);
            let value = t.from + (t.to - t.from) * eased;
            let style = overrides.entry(t.node).or_default();
            match t.prop {
                TransitionProp::Opacity => style.opacity = value.clamp(0.0, 1.0),
            }
            true
        });
        self.anim_overrides = overrides;
        // Animation overrides patch the styles the display list is built
        // from — a changed override set means the list must be rebuilt.
        self.paint_gen = self.paint_gen.wrapping_add(1);
        self.repaint();
    }

    /// Builds the display list, applying animation overrides (paint-side
    /// properties only — no relayout needed). Returns None when styles are
    /// not yet computed.
    fn build_list(
        &self,
        dom: &Rc<RefCell<Dom>>,
        layout: &std::sync::Arc<LayoutResult>,
        images: &ImageMap,
        media_overlays: &rowser_rendering::display_list::MediaOverlays,
    ) -> Option<std::sync::Arc<rowser_rendering::DisplayList>> {
        let base_styles = self.style_map.as_ref()?;
        // Animation overrides patch transform/opacity/filters onto the
        // computed styles (a full StyleMap clone ONLY while animations run).
        let patched;
        let styles: &StyleMap = if self.anim_overrides.is_empty() {
            base_styles
        } else {
            let mut clone = base_styles.clone();
            for (node, override_style) in &self.anim_overrides {
                if let Some(target) = clone.styles.get_mut(node) {
                    if !override_style.transform.is_empty() {
                        target.transform = override_style.transform.clone();
                    }
                    if (override_style.opacity - 1.0).abs() > 1e-6 {
                        target.opacity = override_style.opacity;
                    }
                    if !override_style.filters.is_empty() {
                        target.filters = override_style.filters.clone();
                    }
                }
            }
            patched = clone;
            &patched
        };
        let inputs = rowser_rendering::display_list::PaintInputs {
            images,
            background_images: &self.background_images,
            video_frames: &self.video_frames,
            media: media_overlays,
            element_scroll: &self.element_scroll,
        };
        let list = build_display_list(&dom.borrow(), styles, layout, &inputs);
        Some(std::sync::Arc::new(list))
    }

    fn repaint(&mut self) {
        if self.suspended || self.dom.is_none() {
            return;
        }
        let trace = std::env::var("ROWSER_UI_TRACE").is_ok();
        let Some(layout) = self.layout.clone() else {
            return;
        };
        let Some(dom) = self.dom.clone() else { return };
        // Inline SVG raster pass (Group C): refresh device-pixel rasters
        // for every laid-out <svg> element, then overlay them onto the
        // <img> image map (the display list paints both via DrawCmd::Image).
        self.rasterize_inline_svgs(&dom.borrow(), &layout);
        let images = self.merged_images();
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
        // Display-list cache (Group E): producers that change list inputs
        // bump `paint_gen` (fresh layout, animation ticks, element scroll,
        // video frames, image arrivals, SVG raster refresh). Pure page
        // scroll and canvas draw loops do NOT — they reuse the list (the
        // list is in document coordinates; canvas pixels resolve lazily
        // at raster).
        let was_cached = self.dl_gen == Some(self.paint_gen) && self.display_list.is_some();
        let list: std::sync::Arc<rowser_rendering::DisplayList> =
            if self.dl_gen == Some(self.paint_gen) {
                match &self.display_list {
                    Some(cached) => std::sync::Arc::clone(cached),
                    None => self
                        .build_list(&dom, &layout, &images, &media_overlays)
                        .inspect(|built| {
                            self.display_list = Some(std::sync::Arc::clone(built));
                            self.dl_gen = Some(self.paint_gen);
                        })
                        .unwrap_or_default(),
                }
            } else {
                match self.build_list(&dom, &layout, &images, &media_overlays) {
                    Some(built) => {
                        self.display_list = Some(std::sync::Arc::clone(&built));
                        self.dl_gen = Some(self.paint_gen);
                        built
                    }
                    None => {
                        self.display_list = None;
                        self.dl_gen = None;
                        return;
                    }
                }
            };
        if trace {
            eprintln!(
                "[page-{}] display_list {} ({} cmds, gen {}, {}ms)",
                self.state.tab,
                if was_cached { "cached" } else { "rebuilt" },
                list.commands.len(),
                self.paint_gen,
                dl_t0.elapsed().as_millis()
            );
        }
        let background = match self.style_map.as_ref() {
            Some(styles) => page_background(&dom.borrow(), styles, &layout),
            None => rowser_parsing::cascade::Rgba::new_opaque(255, 255, 255),
        };
        let options = RenderOptions {
            viewport_width: self.viewport.width as u32,
            viewport_height: self.viewport.height as u32,
            scroll_y: self.scroll_y,
            scale: self.device_pixel_ratio,
            background,
            find_matches: self.find_matches.clone(),
            active_match: self.active_match,
        };
        let paint_t0 = std::time::Instant::now();
        if let Some(frame) = self.painter.render_ctx(
            &list,
            options,
            &mut self.layout_engine.font_system,
            Some(&self.canvas_registry),
        ) {
            if trace {
                eprintln!(
                    "[page-{}] paint took {}ms ({} blits, {} fulls)",
                    self.state.tab,
                    paint_t0.elapsed().as_millis(),
                    self.painter.scroll_stats().0,
                    self.painter.scroll_stats().1
                );
            }
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
            // Frame presented: rAF callbacks for this frame run now.
            self.fire_raf();
        }
    }

    /// Inline `<svg>` raster pass (Group C). Every svg element that has a
    /// layout rect is re-rasterized only when its size or subtree signature
    /// changed; results land in `svg_rasters` and are merged over the image
    /// map in [`Self::merged_images`]. Rasterizing at the *layout rect* in
    /// device pixels (rather than scaling a natural-size raster) is what
    /// makes logos crisp — the Phase-B giant-blurry-logo class of bugs.
    fn rasterize_inline_svgs(&mut self, dom: &Dom, layout: &LayoutResult) {
        // Roots: light tree + every shadow root (icon SVGs frequently live
        // inside component shadow trees).
        let mut roots = vec![dom.document()];
        roots.extend(dom.all_shadow_roots());
        let mut wanted: Vec<NodeId> = Vec::new();
        for root in roots {
            for node in dom.subtree_elements(root) {
                if dom
                    .element(node)
                    .is_some_and(|e| e.name.ns == ns!(svg) && &*e.name.local == "svg")
                {
                    wanted.push(node);
                }
            }
        }
        if wanted.is_empty() {
            if !self.svg_rasters.is_empty() {
                self.svg_rasters.clear();
                self.paint_gen = self.paint_gen.wrapping_add(1);
            }
            return;
        }
        // Pass 1 (immutable): size + signature per svg element.
        let mut targets: Vec<(NodeId, u32, u32, u64)> = Vec::new();
        for node in wanted {
            let Some(rect) = layout.rects.get(&node) else {
                continue;
            };
            if rect.w <= 0.5 || rect.h <= 0.5 {
                continue;
            }
            let w = (rect.w * self.device_pixel_ratio).round().max(1.0) as u32;
            let h = (rect.h * self.device_pixel_ratio).round().max(1.0) as u32;
            // Sanity cap: a layout explosion must not allocate a giant raster.
            if w > 8192 || h > 8192 {
                continue;
            }
            let sig = svg_subtree_signature(dom, node);
            targets.push((node, w, h, sig));
        }
        let hits: Vec<NodeId> = targets
            .iter()
            .filter(|(node, w, h, sig)| {
                self.svg_rasters
                    .get(node)
                    .is_some_and(|r| r.w == *w && r.h == *h && r.sig == *sig)
            })
            .map(|(node, ..)| *node)
            .collect();
        if hits.len() == self.svg_rasters.len() && targets.len() == hits.len() {
            return; // nothing changed, keep the cache
        }
        // Pass 2 (mutable): drop stale entries, rasterize misses.
        let before = self.svg_rasters.len();
        self.svg_rasters.retain(|node, r| {
            targets
                .iter()
                .any(|(t, w, h, sig)| t == node && r.w == *w && r.h == *h && r.sig == *sig)
        });
        if self.svg_rasters.len() != before {
            self.paint_gen = self.paint_gen.wrapping_add(1);
        }
        let t0 = std::time::Instant::now();
        let mut rasterized = 0usize;
        for (node, w, h, sig) in targets {
            if self
                .svg_rasters
                .get(&node)
                .is_some_and(|r| r.w == w && r.h == h && r.sig == sig)
            {
                continue;
            }
            let markup = dom.serialize_subtree(node);
            // Root size = the layout rect (device px): usvg maps the viewBox
            // into it honoring preserveAspectRatio — Chrome-equivalent.
            if let Some(image) = rowser_rendering::rasterize_svg(&markup, w, h) {
                self.svg_rasters.insert(
                    node,
                    SvgRasterEntry {
                        w,
                        h,
                        sig,
                        image: Arc::new(image),
                    },
                );
                self.paint_gen = self.paint_gen.wrapping_add(1);
                rasterized += 1;
            }
        }
        if rasterized > 0 && std::env::var("ROWSER_UI_TRACE").is_ok() {
            eprintln!(
                "[page-{}] svg raster pass: {} new, {} cached, {}ms",
                self.state.tab,
                rasterized,
                self.svg_rasters.len() - rasterized,
                t0.elapsed().as_millis()
            );
        }
    }

    /// The image map the display list sees: decoded `<img>`s overlaid with
    /// inline-SVG rasters (both paint through the same DrawCmd::Image path).
    fn merged_images(&mut self) -> ImageMap {
        // Cached (images + SVG rasters) map, rebuilt only when either
        // source changes. Canvases are NOT merged anymore: the display
        // list references them via DrawCmd::Canvas and the painter pulls
        // live pixels per raster (Group E — canvas draws no longer rebuild
        // the display list or re-clone the image map).
        if self.image_map_gen == Some(self.paint_gen) {
            if let Some(cached) = &self.image_map_cache {
                return cached.clone();
            }
        }
        let mut merged = self.images.clone();
        for (node, raster) in &self.svg_rasters {
            merged.insert(*node, Arc::clone(&raster.image));
        }
        self.image_map_cache = Some(merged.clone());
        self.image_map_gen = Some(self.paint_gen);
        merged
    }

    /// Dispatches pending rAF callbacks into JS. The page loop paces this
    /// at ~60 FPS while callbacks keep re-scheduling (the loop timeout
    /// shrinks to 16ms when `pending_raf` is non-empty).
    fn fire_raf(&mut self) {
        if self.pending_raf.is_empty() {
            return;
        }
        // Chrome pauses the frame clock in background tabs; the callbacks
        // stay queued and run when the tab regains focus.
        if !self
            .state
            .focused
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let ids = std::mem::take(&mut self.pending_raf);
        self.last_raf = Some(std::time::Instant::now());
        if let Some(js) = &self.js {
            for id in ids {
                js.dispatch(JsEngineEvent::RafFired(id));
            }
        }
        // NOTE: no mark_if_dirty here. JS callbacks that mutate the DOM
        // send MarkDirty through their natives; re-rendering on every
        // frame turned 60 FPS rAF pages into 60 FPS full re-layouts
        // (GitHub's RSS grew to 1.6 GB in seconds).
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
                mo: Rc::clone(&self.mo_state),
                history: Rc::clone(&self.history_mirror),
                observers: Rc::clone(&self.observers_state),
                session: Rc::clone(&self.session_store),
                canvases: Rc::clone(&self.canvas_registry),
                images: Rc::clone(&self.image_mirror),
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
                // HTML: the element's `load` event fires when the script has
                // executed. Sites gate async bootstraps on this (Wikipedia's
                // loader, analytics bundles, JSONP callbacks). Non-bubbling:
                // window 'load' listeners must not see per-script events.
                if *node != 0 {
                    let _ = js.eval(
                        &format!(
                            "globalThis.__fireElementEvent && __fireElementEvent({node}, 'load');"
                        ),
                        "script-load-event.js",
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

    /// A dynamically-inserted `<script src>` finished fetching: evaluate
    /// its code in the page runtime and fire the element's `load` (ok) or
    /// `error` (failure) event — the contract every async loader
    /// (`script.onload = boot`, JSONP, module preloads) depends on.
    fn run_dynamic_script(&mut self, node: u64, code: String, ok: bool) {
        let node_id = node as NodeId;
        if self
            .dom
            .as_ref()
            .is_none_or(|d| !d.borrow().is_valid(node_id))
        {
            return; // script was removed from the document while fetching
        }
        if let Some(js) = &self.js {
            if ok {
                let name = self
                    .dom
                    .as_ref()
                    .and_then(|d| d.borrow().get_attr(node_id, "src").map(str::to_owned))
                    .unwrap_or_else(|| "dynamic-script.js".to_owned());
                if node != 0 {
                    let _ = js.eval(
                        &format!("globalThis.__setCurrentScript && __setCurrentScript({node})"),
                        "current-script.js",
                    );
                }
                if let Err(err) = js.eval(&code, &name) {
                    let _ = self.state.event_tx.send(EngineEvent::ConsoleMessage {
                        tab: self.state.tab,
                        level: "error".to_owned(),
                        text: format!("{name}: {err}"),
                    });
                }
                if node != 0 {
                    let _ = js.eval(
                        "globalThis.__setCurrentScript && __setCurrentScript(0)",
                        "current-script-clear.js",
                    );
                }
            } else {
                // Fetch failure: `error` on the element, plus the console
                // diagnostic the engine-side task embedded in `code`.
                let _ = js.eval(&code, "script-fetch-error.js");
            }
            let event = if ok { "load" } else { "error" };
            if node != 0 {
                let _ = js.eval(
                    &format!(
                        "globalThis.__fireElementEvent && __fireElementEvent({node}, '{event}');"
                    ),
                    "script-load-event.js",
                );
            }
        }
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
            media_mirror: Rc::new(RefCell::new(HashMap::new())),
            rects: Rc::new(RefCell::new(HashMap::new())),
            viewport: Rc::new(RefCell::new((0.0, 0.0, 0.0))),
            mo: rowser_js::MoShared::default(),
            history: Default::default(),
            observers: Default::default(),
            session: Default::default(),
            canvases: rowser_rendering::canvas2d::new_registry(),
            images: Rc::new(RefCell::new(ImageMap::new())),
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

    /// A dynamically-created image (`new Image()` + `.src`) finished
    /// fetching: decode into the image map, fire load/error, refresh the
    /// JS-side mirror, repaint.
    fn image_fetched(&mut self, node: u64, body: Vec<u8>, ok: bool) {
        let node_id = NodeId::try_from(node).unwrap_or(0);
        if self
            .dom
            .as_ref()
            .is_none_or(|d| !d.borrow().is_valid(node_id))
        {
            return; // the element was removed while fetching
        }
        if ok {
            if let Some(image) =
                DecodedImage::decode(&body).or_else(|| rowser_rendering::decode_svg_bytes(&body))
            {
                self.images.insert(node_id, Arc::new(image));
                self.paint_gen = self.paint_gen.wrapping_add(1);
            }
        }
        // Refresh the JS-visible image mirror FIRST — onload handlers
        // read naturalWidth (the classic image-loader bootstrap).
        self.sync_image_mirror();
        // Then fire load/error on the element.
        if let Some(js) = &self.js {
            let event = if ok { "load" } else { "error" };
            js.dispatch(JsEngineEvent::DomEvent {
                node,
                event_type: event.to_owned(),
            });
        }
        self.dirty = true;
    }

    /// Refreshes the JS-visible mirror of decoded images.
    fn sync_image_mirror(&mut self) {
        *self.image_mirror.borrow_mut() = self.images.clone();
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
            // Video frames are embedded in the display list (DrawCmd::Image):
            // each new frame requires a rebuild + raster.
            self.paint_gen = self.paint_gen.wrapping_add(1);
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
                    .map(|dom| dom.borrow().get_attr(node as NodeId, "controls").is_some())
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
        // Load watchdog: a navigation whose subresources never all settle
        // must not spin on the loading screen forever (hung third-party
        // fetches, silent engine-side drops). Force-complete with what we
        // have — the stylesheet set that DID arrive is applied.
        if self.navigating
            && !self.pending.is_empty()
            && self
                .load_deadline
                .is_some_and(|deadline| std::time::Instant::now() > deadline)
        {
            let stuck: Vec<String> = self.pending.keys().cloned().collect();
            tracing::warn!(
                "load watchdog fired (tab {}): rendering without {} stuck subresource(s): {}",
                self.state.tab,
                stuck.len(),
                stuck.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            );
            self.pending.clear();
            self.load_deadline = None;
            self.subresources_complete();
        }
        if self.dirty && !self.suspended && !self.navigating && self.dom.is_some() {
            // Sustained-dirty pacing: pages whose every rAF/observer
            // reaction dirties the DOM re-render forever (marketing
            // animations). Chrome composites those at 60 FPS with
            // incremental layout; our full re-render costs seconds on
            // JS-heavy pages, and render→observer→mutate→render ground
            // the whole machine (1.6 GB RSS, starved UI). Pace slow pages
            // to at most one render per cost-scaled interval (1s floor,
            // 4s cap) — degraded animation, alive browser.
            let slow = self.last_render_cost > std::time::Duration::from_millis(800);
            if slow
                && self.dirty_streak > 4
                && self
                    .render_not_before
                    .is_some_and(|t| std::time::Instant::now() < t)
            {
                // Not yet: leave dirty set, retry on a later idle tick.
            } else {
                self.dirty = false;
                self.dirty_streak += 1;
                let t0 = std::time::Instant::now();
                self.render_pipeline();
                self.last_render_cost = t0.elapsed();
                if self.last_render_cost > std::time::Duration::from_millis(800) {
                    let gap = (self.last_render_cost / 2).clamp(
                        std::time::Duration::from_secs(1),
                        std::time::Duration::from_secs(4),
                    );
                    self.render_not_before = Some(std::time::Instant::now() + gap);
                }
            }
        } else if !self.dirty {
            // Quiet period: a genuinely idle page resets the streak so the
            // FIRST new interaction renders immediately.
            self.dirty_streak = 0;
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

/// A cached inline-SVG raster (see `Page::rasterize_inline_svgs`).
struct SvgRasterEntry {
    /// Raster width in device pixels.
    w: u32,
    /// Raster height in device pixels.
    h: u32,
    /// Subtree signature when rasterized (mutation detection).
    sig: u64,
    /// The raster itself.
    image: Arc<DecodedImage>,
}

/// Cheap content signature of an svg subtree: tag, attributes and text
/// bytes hashed depth-first. Detects DOM mutations that change the art.
fn svg_subtree_signature(dom: &Dom, node: NodeId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    fn hash_node(h: &mut std::collections::hash_map::DefaultHasher, dom: &Dom, node: NodeId) {
        match dom.kind(node) {
            rowser_dom::NodeKind::Element(el) => {
                el.name.local.hash(h);
                for attr in &el.attrs {
                    attr.name.hash(h);
                    attr.value.hash(h);
                }
                for child in dom.children(node) {
                    hash_node(h, dom, child);
                }
            }
            rowser_dom::NodeKind::Text(t) => t.hash(h),
            _ => {}
        }
    }
    hash_node(&mut h, dom, node);
    h.finish()
}

/// Decodes a `data:` URL (base64 or percent-encoded) into bytes.
fn decode_data_url_bytes(data_url: &str) -> Option<Vec<u8>> {
    let rest = data_url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let is_base64 = meta.split(';').any(|p| p.eq_ignore_ascii_case("base64"));
    if is_base64 {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()
    } else {
        percent_encoding::percent_decode_str(payload)
            .decode_utf8()
            .ok()
            .map(|s| s.into_owned().into_bytes())
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

fn page_background(
    dom: &rowser_dom::Dom,
    styles: &StyleMap,
    layout: &LayoutResult,
) -> rowser_parsing::cascade::Rgba {
    // CSS canvas background propagation, spec order: the root element's
    // (html) background wins; if it has none, body's is used. The previous
    // implementation returned the first opaque background of ANY element in
    // HashMap iteration order — a random banner/logo cell could (and did,
    // on Hacker News) paint the entire canvas orange.
    let mut html_bg = None;
    let mut body_bg = None;
    for node in dom.subtree_elements(dom.document()) {
        if let Some(el) = dom.element(node) {
            let bg = styles
                .get(node)
                .filter(|s| s.background_color.a > 0)
                .map(|s| s.background_color);
            match &*el.name.local {
                "html" => html_bg = bg,
                "body" => body_bg = bg.or(body_bg),
                _ => {}
            }
        }
    }
    let _ = layout;
    html_bg
        .or(body_bg)
        .unwrap_or_else(|| rowser_parsing::cascade::Rgba::new_opaque(255, 255, 255))
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
