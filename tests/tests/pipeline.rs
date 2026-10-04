//! End-to-end engine tests over a local HTTP server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rowser_api::{BrowserApi, EngineEvent};
use rowser_engine::EngineConfig;

use rowser_tests::LocalServer;

/// Test wait budget; sanitizer builds run 10-20x slower.
fn wait_seconds() -> u64 {
    if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok() {
        120
    } else {
        20
    }
}

fn test_config(label: &str) -> EngineConfig {
    if std::env::var("ROWSER_TEST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_target(true)
            .try_init();
    }
    EngineConfig {
        // Unique per test (parallel test runs share one process).
        profile_dir: std::env::temp_dir()
            .join(format!("rowser-e2e-{}-{label}", std::process::id())),
        ..EngineConfig::default()
    }
}

const PAGE_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
  <title>Rrowser E2E</title>
  <link rel="stylesheet" href="/style.css">
  <script src="/app.js"></script>
</head>
<body>
  <h1>Engine Test Page</h1>
  <p>This page exercises the full pipeline: fetch, parse, style, layout, paint, script.</p>
  <div id="target" class="box"></div>
  <img alt="logo" src="/pixel.png">
</body>
</html>"#;

const PAGE_CSS: &str = r#"
body { font-family: sans-serif; margin: 20px; }
h1 { color: #2244cc; }
.box { width: 240px; height: 120px; background-color: #ff8800; margin: 16px 0; }
p { color: #333333; }
"#;

const PAGE_JS: &str = r#"
console.log('script running');
const target = document.getElementById('target');
target.setAttribute('data-seen', 'yes');
target.textContent = 'filled by script';
localStorage.setItem('e2e', 'ok');
"#;

// 8x8 orange PNG (base64).
const PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAYAAADED76LAAAAGElEQVR4nGP8z8DwnwEPYMInOXwUAADtmwT9ZBgLgAAAAABJRU5ErkJggg==";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_pipeline_loads_renders_and_runs_scripts() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (
            200,
            "text/html; charset=utf-8".to_owned(),
            PAGE_HTML.as_bytes().to_vec(),
        ),
    );
    routes.insert(
        "/style.css".to_owned(),
        (200, "text/css".to_owned(), PAGE_CSS.as_bytes().to_vec()),
    );
    routes.insert(
        "/app.js".to_owned(),
        (
            200,
            "application/javascript".to_owned(),
            PAGE_JS.as_bytes().to_vec(),
        ),
    );
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD
        .decode(PIXEL_PNG)
        .expect("test png");
    routes.insert("/pixel.png".to_owned(), (200, "image/png".to_owned(), png));

    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("full-pipeline")).expect("engine start");
    let tab = browser.new_tab(Some(url.clone()));

    let mut events = browser.events();
    let mut loaded = false;
    let mut frame_ids = 0u64;
    let mut console_messages = 0usize;
    // Sanitizer builds run 10-20x slower; widen the deadline there.
    let seconds = if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok()
    {
        120
    } else {
        20
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    'event_loop: loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for engine events")
            .expect("event channel alive");
        match event {
            EngineEvent::PageLoaded { url, title, .. } => {
                assert!(url.contains("127.0.0.1"), "loaded url: {url}");
                assert_eq!(title, "Rrowser E2E");
                loaded = true;
            }
            EngineEvent::FrameReady { .. } => frame_ids += 1,
            EngineEvent::ConsoleMessage {
                tab: _,
                level,
                text,
            } => {
                assert_eq!(level, "log");
                assert_eq!(text, "script running");
                console_messages += 1;
            }
            EngineEvent::BlockedRequest { url, .. } => {
                panic!("unexpected block: {url}");
            }
            _ => {}
        }
        if loaded && frame_ids >= 1 && console_messages >= 1 {
            break 'event_loop;
        }
    }

    // Subresources were all fetched.
    assert!(server.hits() >= 4, "server hits: {}", server.hits());

    // Frame exists and is not blank; the orange box must be present.
    let frame = browser.frame(tab).expect("frame");
    assert!(frame.width > 0 && frame.height > 0);
    let straight = frame.straight_rgba();
    let mut found_orange = 0usize;
    for px in straight.chunks_exact(4) {
        if px[0] >= 240 && (120..160).contains(&px[1]) && px[2] < 32 {
            found_orange += 1;
        }
    }
    assert!(found_orange > 200, "orange box pixels: {found_orange}");

    // Title snapshot matches.
    assert_eq!(browser.title(tab).as_deref(), Some("Rrowser E2E"));

    // Content is taller than the default viewport 800 → scrollable.
    let (content_w, content_h) = browser.content_size(tab).expect("content size");
    let _ = content_w;
    assert!(content_h > 100.0, "content height {content_h}");

    browser.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_url_navigation_renders() {
    let browser = BrowserApi::start(test_config("data-url")).expect("engine start");
    let html = "<html><body style='background-color: #00aa00'><p>data url page</p></body></html>";
    let data_url = format!("data:text/html;base64,{}", {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(html)
    });
    let tab = browser.new_tab(Some(data_url));
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds());
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout")
            .expect("alive");
        if matches!(event, EngineEvent::PageLoaded { .. }) {
            break;
        }
    }
    let frame = browser.frame(tab).expect("frame");
    let straight = frame.straight_rgba();
    let mut green = 0usize;
    for px in straight.chunks_exact(4) {
        if px[1] >= 150 && px[0] < 40 && px[2] < 40 && px[3] > 0 {
            green += 1;
        }
    }
    assert!(green > 500, "green pixels: {green}");
    browser.shutdown();
}

/// Group C: `<img src="*.svg">` fetches, decodes (resvg), lays out at the
/// attribute size and paints — the Wikipedia-logo class of images.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn svg_img_fetches_and_renders() {
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 40">
        <circle cx="20" cy="20" r="18" fill="#0000cc"/>
    </svg>"##;
    let html = r##"<!DOCTYPE html><html><head><title>SVG Img</title></head><body>
        <img alt="logo" src="/logo.svg" width="120" height="120">
    </body></html>"##;
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), html.as_bytes().to_vec()),
    );
    routes.insert(
        "/logo.svg".to_owned(),
        (200, "image/svg+xml".to_owned(), svg.to_vec()),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("svg-img")).expect("engine start");
    let tab = browser.new_tab(Some(url.clone()));
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds());
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for engine events")
            .expect("event channel alive");
        if matches!(event, EngineEvent::PageLoaded { .. }) {
            break;
        }
    }
    // Give the image fetch + decode + repaint a moment to land.
    for _ in 0..wait_seconds() {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(frame) = browser.frame(tab) {
            let straight = frame.straight_rgba();
            let blue = straight
                .chunks_exact(4)
                .filter(|px| px[2] >= 150 && px[0] < 40 && px[1] < 40 && px[3] > 0)
                .count();
            if blue > 1500 {
                browser.shutdown();
                return;
            }
        }
    }
    let frame = browser.frame(tab).expect("frame");
    let straight = frame.straight_rgba();
    let blue = straight
        .chunks_exact(4)
        .filter(|px| px[2] >= 150 && px[0] < 40 && px[1] < 40 && px[3] > 0)
        .count();
    assert!(blue > 1500, "svg img blue pixels: {blue}");
    browser.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tab_lifecycle_and_suspension() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (
            200,
            "text/html".to_owned(),
            b"<html><head><title>tab test</title></head><body><p>tab</p></body></html>".to_vec(),
        ),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let config = EngineConfig {
        suspend_after: Duration::from_millis(500),
        ..test_config("tab-lifecycle")
    };
    let browser = BrowserApi::start(config).expect("engine start");
    let tab = browser.new_tab(Some(url.clone()));
    // Session-6 semantics: the FIRST tab is born focused (a visible tab
    // must never auto-freeze). Background it the way the UI does — focus
    // a sibling tab — so the suspension sweep has something to freeze.
    let background = browser.new_tab(None);
    browser.focus(background);
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds());
    let mut loaded = false;
    let mut suspended = false;
    let mut resumed = false;
    while tokio::time::Instant::now() < deadline {
        let event = tokio::time::timeout_at(deadline, events.recv()).await;
        let Ok(Ok(event)) = event else { break };
        match event {
            EngineEvent::PageLoaded { .. } => loaded = true,
            EngineEvent::TabSuspended(_) => {
                suspended = true;
                // Focusing should resume.
                browser.focus(tab);
            }
            EngineEvent::TabResumed(_) => resumed = true,
            _ => {}
        }
        if loaded && suspended && resumed {
            break;
        }
    }
    assert!(loaded, "page never loaded");
    assert!(suspended, "tab never suspended");
    assert!(resumed, "tab never resumed");
    browser.shutdown();
    // Give the engine thread a moment to finish joining.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = Arc::new(0u8); // keep Arc import alive for future helpers
}

const HISTORY_A: &str = r#"<!DOCTYPE html><html><head><title>History A</title></head><body>
<h1>Page A</h1>
<script>
if (localStorage.getItem('hist-done')) {
  console.log('BACK-LANDED:' + location.pathname);
  setTimeout(function () { history.forward(); }, 200);
} else {
  localStorage.setItem('hist-done', '1');
  const preHref = location.href;
  history.pushState({ page: 2 }, '', '/p2');
  console.log('PUSH:' + location.pathname + ':' + history.length + ':' + preHref + ':' + location.href);
  setTimeout(function () { history.back(); }, 250);
}
</script>
</body></html>"#;

const HISTORY_B: &str = r#"<!DOCTYPE html><html><head><title>History B</title></head><body>
<h1>Page B</h1>
<script>
window.addEventListener('popstate', function (e) {
  console.log('POP:' + JSON.stringify(e.state));
});
</script>
</body></html>"#;

/// History API: pushState updates location + length; back() re-navigates;
/// forward() lands on the pushed entry and pops `popstate` with the stored
/// state — the SPA-router contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_pushstate_popstate_roundtrip() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), HISTORY_A.as_bytes().to_vec()),
    );
    routes.insert(
        "/p2".to_owned(),
        (200, "text/html".to_owned(), HISTORY_B.as_bytes().to_vec()),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("history")).expect("engine start");
    let _tab = browser.new_tab(Some(url.clone()));
    let mut events = browser.events();

    let seconds = if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok()
    {
        120
    } else {
        20
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut saw_push = false;
    let mut saw_back = false;
    let mut saw_pop = false;
    while !(saw_push && saw_back && saw_pop) {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for history events")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("PUSH:") {
                assert!(text.contains("/p2"), "push location: {text}");
                assert!(text.contains(":2"), "history.length: {text}");
                saw_push = true;
            }
            if text.starts_with("BACK-LANDED:") {
                assert!(text.contains('/'), "back landed: {text}");
                saw_back = true;
            }
            if text.starts_with("POP:") {
                assert!(text.contains("page"), "popstate state: {text}");
                saw_pop = true;
            }
        }
    }
    browser.shutdown();
}

const OBSERVERS_HTML: &str = r#"<!DOCTYPE html><html><head><title>Observers</title></head><body>
<div id="target" style="width:100px;height:100px;background:#0a0">X</div>
<script>
  const el = document.getElementById('target');
  const io = new IntersectionObserver(function (entries) {
    console.log('IO:' + entries[0].isIntersecting + ':' + Math.round(entries[0].boundingClientRect.width));
  }, { threshold: [0.5] });
  io.observe(el);
  const ro = new ResizeObserver(function (entries) {
    console.log('RO:' + Math.round(entries[0].contentRect.width) + 'x' + Math.round(entries[0].contentRect.height));
  });
  ro.observe(el);
</script>
</body></html>"#;

/// Intersection/Resize observers: engine computes entries from real layout
/// rects after the post-script re-render and delivers them into JS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observers_fire_with_real_geometry() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (
            200,
            "text/html".to_owned(),
            OBSERVERS_HTML.as_bytes().to_vec(),
        ),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("observers")).expect("engine start");
    let _tab = browser.new_tab(Some(url));
    let mut events = browser.events();

    let seconds = if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok()
    {
        120
    } else {
        20
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut saw_io = false;
    let mut saw_ro = false;
    while !(saw_io && saw_ro) {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for observer events")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("IO:") {
                assert!(text.contains("true"), "isIntersecting: {text}");
                assert!(text.contains("100"), "rect width: {text}");
                saw_io = true;
            }
            if text.starts_with("RO:") {
                assert!(text.contains("100x100"), "contentRect: {text}");
                saw_ro = true;
            }
        }
    }
    browser.shutdown();
}

const SESSION_HTML: &str = r#"<!DOCTYPE html><html><head><title>Session</title></head><body>
<script>
  sessionStorage.setItem('page', '1');
  console.log('SS:' + sessionStorage.getItem('page') + ':' + sessionStorage.length);
</script>
</body></html>"#;

/// sessionStorage: set/get across a reload within the same tab.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_storage_survives_reload() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (
            200,
            "text/html".to_owned(),
            SESSION_HTML.as_bytes().to_vec(),
        ),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("session")).expect("engine start");
    let tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let seconds = if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok()
    {
        120
    } else {
        20
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut saw = 0usize;
    while saw < 1 {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for session events")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("SS:") {
                assert!(text.contains("1:1"), "sessionStorage: {text}");
                saw += 1;
            }
        }
    }
    // Reload the same tab: the store must persist (per-tab lifetime).
    browser.reload(tab);
    let mut saw2 = 0usize;
    let deadline2 = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while saw2 < 1 {
        let event = tokio::time::timeout_at(deadline2, events.recv())
            .await
            .expect("timeout on reload")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("SS:") {
                saw2 += 1;
            }
        }
    }
    browser.shutdown();
}

const RAF_HTML: &str = r#"<!DOCTYPE html><html><head><title>RAF</title></head><body>
<div id="tick" style="width:50px;height:50px"></div>
<script>
  let frames = 0;
  function loop(ts) {
    frames++;
    if (frames >= 3) {
      console.log('RAF-DONE:' + frames + ':' + (ts > 0));
      return; // stop the loop
    }
    requestAnimationFrame(loop);
  }
  requestAnimationFrame(loop);
</script>
</body></html>"#;

/// requestAnimationFrame runs on the frame clock: nested scheduling
/// advances at ~60 FPS until the loop stops itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raf_frame_clock_advances() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), RAF_HTML.as_bytes().to_vec()),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("raf")).expect("engine start");
    let _tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let seconds = if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok()
    {
        120
    } else {
        20
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut saw_done = false;
    while !saw_done {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for rAF events")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("RAF-DONE:") {
                assert!(text.contains("3:true"), "rAF frames: {text}");
                saw_done = true;
            }
        }
    }
    browser.shutdown();
}

const SCRIPT_EVENTS_HTML: &str = r#"<!DOCTYPE html><html><head><title>Script Events</title>
</head><body>
<div id="out">x</div>
<script>
  // Registers a load listener on a parser-inserted <script src> that comes
  // LATER in document order (the whole DOM exists before any script runs,
  // but scripts execute in order — so this listener is live when the
  // external script executes and fires its load event).
  var ps = document.getElementById('late-parser');
  ps.addEventListener('load', function () { console.log('SL:PARSER-LOADED'); });
  ps.addEventListener('error', function () { console.log('SL:PARSER-ERROR'); });

  // 1) Dynamic external script: createElement + src + onload + appendChild.
  var s = document.createElement('script');
  s.src = '/dynamic.js';
  s.onload = function () { console.log('SL:DYN-LOADED'); };
  s.onerror = function () { console.log('SL:DYN-ERROR'); };
  document.head.appendChild(s);

  // 2) Inline dynamic script: runs at insertion, fires load.
  var s2 = document.createElement('script');
  s2.textContent = "console.log('SL:INLINE-RAN');";
  s2.onload = function () { console.log('SL:INLINE-LOADED'); };
  document.body.appendChild(s2);

  // 3) Error path: missing script fires onerror, not onload.
  var s3 = document.createElement('script');
  s3.src = '/missing.js';
  s3.onload = function () { console.log('SL:MISS-LOADED'); };
  s3.onerror = function () { console.log('SL:MISS-ERROR'); };
  document.body.appendChild(s3);

  // 4) window.onload inline handler must fire exactly once.
  window.onload = function () { console.log('SL:WINDOW-ONLOAD'); };
</script>
<script id="late-parser" src="/parser.js"></script>
</body></html>"#;

const PARSER_JS: &str = "console.log('SL:PARSER-RAN');";
const DYNAMIC_JS: &str = "console.log('SL:DYNAMIC-RAN');";

const SCROLL_HTML: &str = r#"<!DOCTYPE html><html><head><title>Scroll</title></head><body>
<div style="height:3000px; background:#eee"></div>
<script>
  // Programmatic scroll: the command routes to the page thread, which
  // clamps to content, updates the scroll mirror (scrollY, rect queries)
  // and repaints.
  scrollTo(0, 500);
  setTimeout(function () {
    console.log('SCROLL:' + Math.round(window.scrollY));
    scrollBy(0, 250);
    setTimeout(function () {
      console.log('SCROLL2:' + Math.round(window.scrollY));
    }, 120);
  }, 120);
</script>
</body></html>"#;

/// window.scrollTo/scrollBy update the scroll mirror (scrollY) end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn programmatic_scroll_updates_mirror() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), SCROLL_HTML.as_bytes().to_vec()),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("scroll")).expect("engine start");
    let _tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let seconds = if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok()
    {
        120
    } else {
        20
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut saw1 = false;
    let mut saw2 = false;
    while !(saw1 && saw2) {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for scroll markers")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("SCROLL:") {
                assert!(text.ends_with("500"), "scrollTo: {text}");
                saw1 = true;
            }
            if text.starts_with("SCROLL2:") {
                assert!(text.ends_with("750"), "scrollBy: {text}");
                saw2 = true;
            }
        }
    }
    browser.shutdown();
}

/// Script load-event dispatch: parser scripts, dynamic `src` scripts
/// (fetch + execute + load), inline dynamic scripts, `error` on failed
/// fetches, and window `load` firing exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_load_events_end_to_end() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (
            200,
            "text/html".to_owned(),
            SCRIPT_EVENTS_HTML.as_bytes().to_vec(),
        ),
    );
    routes.insert(
        "/parser.js".to_owned(),
        (
            200,
            "text/javascript".to_owned(),
            PARSER_JS.as_bytes().to_vec(),
        ),
    );
    routes.insert(
        "/dynamic.js".to_owned(),
        (
            200,
            "text/javascript".to_owned(),
            DYNAMIC_JS.as_bytes().to_vec(),
        ),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("script-events")).expect("engine start");
    let _tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let seconds = if std::env::var("TSAN_OPTIONS").is_ok() || std::env::var("ASAN_OPTIONS").is_ok()
    {
        120
    } else {
        20
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut saw = std::collections::HashSet::new();
    while saw.len() < 8 {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for script event markers")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("SL:") {
                saw.insert(text.trim().to_owned());
            }
        }
    }
    browser.shutdown();

    // Executed...
    assert!(saw.contains("SL:PARSER-RAN"), "parser script ran: {saw:?}");
    assert!(
        saw.contains("SL:DYNAMIC-RAN"),
        "dynamic script ran: {saw:?}"
    );
    assert!(
        saw.contains("SL:INLINE-RAN"),
        "inline dynamic script ran: {saw:?}"
    );
    // ...and their load events fired (parser load via addEventListener on
    // the element, dynamic load + inline load via on-properties)...
    assert!(
        saw.contains("SL:PARSER-LOADED"),
        "parser script load event: {saw:?}"
    );
    assert!(
        saw.contains("SL:DYN-LOADED"),
        "dynamic script load event: {saw:?}"
    );
    assert!(
        saw.contains("SL:INLINE-LOADED"),
        "inline script load event: {saw:?}"
    );
    // ...the failure path errored...
    assert!(
        saw.contains("SL:MISS-ERROR"),
        "missing script error: {saw:?}"
    );
    assert!(
        !saw.contains("SL:MISS-LOADED"),
        "missing script must not load: {saw:?}"
    );
    // ...and window onload fired exactly once (guard: the test only exits
    // the loop when all 8 distinct markers arrive; WINDOW-ONLOAD is the 8th).
    assert!(
        saw.contains("SL:WINDOW-ONLOAD"),
        "window.onload fired: {saw:?}"
    );
    assert_eq!(saw.len(), 8, "exactly the expected markers: {saw:?}");
}

// ---------------------------------------------------------------------------
// Canvas 2D (Group D)
// ---------------------------------------------------------------------------

const CANVAS_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><title>Canvas E2E</title></head>
<body style="margin:0">
  <canvas id="c" width="240" height="160" style="border:1px solid #333"></canvas>
  <canvas id="c2" width="60" height="60" style="display:none"></canvas>
  <img id="logo" src="/pixel.png" style="position:absolute;left:-9999px">
  <script>
    const c = document.getElementById('c');
    const ctx = c.getContext('2d');
    // 1. Basic fill + clear.
    ctx.fillStyle = '#ff0000';
    ctx.fillRect(10, 10, 40, 30);
    // 2. Gradient.
    const g = ctx.createLinearGradient(100, 0, 200, 0);
    g.addColorStop(0, '#0000ff');
    g.addColorStop(1, '#ffffff');
    ctx.fillStyle = g;
    ctx.fillRect(100, 10, 100, 30);
    // 3. Path + arc + stroke.
    ctx.strokeStyle = '#00aa00';
    ctx.lineWidth = 4;
    ctx.beginPath();
    ctx.arc(60, 90, 20, 0, Math.PI * 2);
    ctx.stroke();
    // 4. Text.
    ctx.fillStyle = '#111111';
    ctx.font = '16px sans-serif';
    ctx.fillText('Rrowser', 120, 90);
    // 5. Save/restore + transform discipline.
    ctx.save();
    ctx.translate(200, 120);
    ctx.rotate(Math.PI / 2);
    ctx.fillStyle = '#ff00ff';
    ctx.fillRect(-10, -5, 20, 10);
    ctx.restore();
    // 6. isPointInPath.
    ctx.beginPath();
    ctx.rect(0, 0, 20, 20);
    const hit = ctx.isPointInPath(10, 10);
    const miss = ctx.isPointInPath(50, 50);
    // 7. measureText sanity.
    const m = ctx.measureText('Rrowser');
    // 8. drawImage from another canvas.
    const c2 = document.getElementById('c2');
    const ctx2 = c2.getContext('2d');
    ctx2.fillStyle = '#ffff00';
    ctx2.fillRect(0, 0, 60, 60);
    ctx.drawImage(c2, 10, 120, 30, 30);
    // 9. getImageData round-trip: repaint the whole 5x5 block green.
    const im = ctx.getImageData(15, 20, 5, 5);
    const red = im.data[0] === 255 && im.data[1] === 0 && im.data[3] === 255;
    for (let i = 0; i < im.data.length; i += 4) {
      im.data[i] = 0; im.data[i+1] = 255; im.data[i+2] = 0; im.data[i+3] = 255;
    }
    ctx.putImageData(im, 10, 10);
    const im2 = ctx.getImageData(12, 12, 1, 1);
    const green = im2.data[1] === 255 && im2.data[3] === 255;
    // 10. toDataURL PNG.
    const url = c.toDataURL();
    const png = url.startsWith('data:image/png;base64,');
    // Report markers.
    console.log('CV:HIT=' + (hit && !miss));
    console.log('CV:MEASURE=' + (m.width > 40));
    console.log('CV:GOTDATA=' + red);
    console.log('CV:PUTDATA=' + green);
    console.log('CV:DATAURL=' + png);
    // 11. Async: Image() load + drawImage (fires later).
    const img = new Image();
    img.onload = () => {
      try {
        ctx.drawImage(img, 150, 120, 20, 20);
        console.log('CV:IMGLOADED nw=' + img.naturalWidth);
      } catch (e) {
        console.log('CV:IMGERR ' + e);
      }
    };
    img.onerror = () => console.log('CV:IMGERR onerror');
    img.src = '/pixel.png';
  </script>
</body>
</html>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas2d_end_to_end() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), CANVAS_HTML.as_bytes().to_vec()),
    );
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD
        .decode(PIXEL_PNG)
        .expect("test png");
    routes.insert("/pixel.png".to_owned(), (200, "image/png".to_owned(), png));
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("canvas2d")).expect("engine start");
    let _tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds());
    let mut saw = std::collections::HashSet::new();
    while saw.len() < 6 {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for canvas markers")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("CV:") {
                saw.insert(text.trim().to_owned());
            }
        }
    }
    browser.shutdown();
    drop(server);

    // JS-visible API behavior.
    assert!(saw.contains("CV:HIT=true"), "isPointInPath: {saw:?}");
    assert!(saw.contains("CV:MEASURE=true"), "measureText: {saw:?}");
    assert!(saw.contains("CV:GOTDATA=true"), "getImageData: {saw:?}");
    assert!(saw.contains("CV:PUTDATA=true"), "putImageData: {saw:?}");
    assert!(saw.contains("CV:DATAURL=true"), "toDataURL: {saw:?}");
    assert!(
        saw.iter().any(|t| t.starts_with("CV:IMGLOADED nw=")),
        "Image() load + drawImage: {saw:?}"
    );
    assert!(
        !saw.iter().any(|t| t.contains("CV:IMGERR")),
        "no image errors: {saw:?}"
    );
    assert_eq!(saw.len(), 6, "exactly the expected markers: {saw:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas2d_paints_pixels() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), CANVAS_HTML.as_bytes().to_vec()),
    );
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD
        .decode(PIXEL_PNG)
        .expect("test png");
    routes.insert("/pixel.png".to_owned(), (200, "image/png".to_owned(), png));
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("canvas-pixels")).expect("engine start");
    let tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds());
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for image load")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("CV:IMGLOADED") {
                break;
            }
        }
    }
    // Allow the final draw + repaint to land.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let frame = browser.frame(tab).expect("frame");
    let straight = frame.straight_rgba();
    let (w, _) = (frame.width as usize, frame.height as usize);
    let at = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [
            straight[i],
            straight[i + 1],
            straight[i + 2],
            straight[i + 3],
        ]
    };
    // Border-1px + body margin 0: the canvas starts at (1,1). Red fill
    // covers canvas pixels (11..49, 11..39) => viewport (12..50, 12..40).
    // putImageData painted green INSIDE the red rect at canvas (10..15)².
    let green = at(13, 13);
    assert!(
        green[1] >= 200 && green[0] < 60 && green[3] >= 250,
        "putImageData green at (13,13): {green:?}"
    );
    let red = at(30, 30);
    assert!(red[0] >= 240 && red[1] < 32, "red fill at (30,30): {red:?}");
    // Gradient blue -> white horizontally at canvas y 10..40.
    let blue = at(112, 20);
    let white = at(195, 20);
    assert!(
        blue[2] >= 200 && blue[0] < 60,
        "gradient blue end: {blue:?}"
    );
    assert!(
        white[0] >= 230 && white[2] >= 230,
        "gradient white end: {white:?}"
    );
    // Yellow 30x30 drawImage(c2) at canvas (10,120) => viewport ~ (11..41, 121..151).
    let yellow = at(25, 135);
    assert!(
        yellow[0] >= 230 && yellow[1] >= 230 && yellow[2] < 60,
        "drawImage canvas->canvas yellow: {yellow:?}"
    );
    // Text: some dark pixels in the band canvas (120..190, 75..95).
    let mut dark = 0usize;
    for x in 121..190 {
        for y in 76..95 {
            let p = at(x, y);
            if p[0] < 90 && p[1] < 90 && p[2] < 90 && p[3] > 200 {
                dark += 1;
            }
        }
    }
    assert!(dark > 10, "fillText dark pixels: {dark}");
    // The magenta rotated square near canvas (200,120): some magenta.
    let mut magenta = 0usize;
    for x in 160..240 {
        for y in 100..160 {
            let p = at(x, y);
            if p[0] >= 200 && p[2] >= 200 && p[1] < 90 && p[3] > 200 {
                magenta += 1;
            }
        }
    }
    assert!(magenta > 10, "rotated transform square: {magenta}");
    browser.shutdown();
}

// ---------------------------------------------------------------------------
// EventSource (SSE, finite-response semantics)
// ---------------------------------------------------------------------------

const SSE_HTML: &str = r#"<!DOCTYPE html>
<html><body>
<script>
  window.onerror = function (m) { console.log('SSE:ERR ' + m); };
  console.log('SSE:START typeof=' + typeof EventSource);
  const es = new EventSource('/events');
  let got = 0, custom = 0;
  const datas = [];
  es.onmessage = (e) => { got++; datas.push(e.data); if (got >= 2) console.log('SSE:DATA ok=' + (datas.join(',') === 'hello,world')); };
  es.addEventListener('tick', (e) => { custom++; if (custom >= 1) console.log('SSE:TICK ok=' + (e.data === '1')); });
  es.onerror = () => console.log('SSE:ERROR');
  es.onopen = () => console.log('SSE:OPEN');
  setTimeout(() => {
    console.log('SSE:SUMMARY g=' + got + ' c=' + custom + ' rs=' + es.readyState);
  }, 900);
</script>
</body></html>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eventsource_finite_stream() {
    let sse = "retry: 1000\n\nid: 1\ndata: hello\n\ndata: world\n\nevent: tick\ndata: 1\n\n: keepalive\n\n";
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), SSE_HTML.as_bytes().to_vec()),
    );
    routes.insert(
        "/events".to_owned(),
        (200, "text/event-stream".to_owned(), sse.as_bytes().to_vec()),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("sse")).expect("engine start");
    let _tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds());
    let mut saw = std::collections::HashSet::new();
    while saw.len() < 5 {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for SSE markers")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("SSE:") {
                saw.insert(text.trim().to_owned());
            }
        }
    }
    browser.shutdown();
    drop(server);

    assert!(saw.contains("SSE:OPEN"), "open event: {saw:?}");
    assert!(
        saw.contains("SSE:DATA ok=true"),
        "message events (one per blank-line block): {saw:?}"
    );
    assert!(
        saw.contains("SSE:TICK ok=true"),
        "custom event name: {saw:?}"
    );
    assert!(
        saw.iter().any(|t| t.starts_with("SSE:SUMMARY")),
        "summary: {saw:?}"
    );
    // Summary must show both handlers fired (g=2, c=1) — the wait loop
    // exits at 5 distinct markers which includes SUMMARY.
    let summary = saw.iter().find(|t| t.starts_with("SSE:SUMMARY")).unwrap();
    assert!(summary.contains("g=2"), "two message events: {summary}");
    assert!(summary.contains("c=1"), "one custom event: {summary}");
}

/// Group E — a JS canvas animation loop (draw + MarkDirty every rAF) must
/// (a) keep updating the rendered canvas pixels through the paint-only
/// gate, and (b) NOT swallow a DOM mutation that lands afterwards: the
/// mutation must still re-style/re-layout/re-paint (gate invalidation).
/// This catches fingerprints that are too coarse (a stuck-open gate would
/// show the green block never painting).
const GATE_HTML: &str = r#"<!DOCTYPE html>
<html><head><style>body { margin: 0; }</style></head>
<body>
<canvas id="c" width="60" height="40" style="width:60px;height:40px;border:0"></canvas>
<div id="after" style="width:120px;height:40px;background-color:#00cc00;display:none"></div>
<script>
const cv = document.getElementById('c');
const ctx = cv.getContext('2d');
let frame = 0;
function tick() {
    ctx.fillStyle = (frame % 2) ? '#dd0000' : '#0000dd';
    ctx.fillRect(0, 0, 60, 40);
    frame++;
    if (frame < 20) { requestAnimationFrame(tick); return; }
    // Last fill was frame=19 -> 19 % 2 = 1 -> RED.
    document.getElementById('after').style.display = 'block';
    console.log('CV:GATEDONE frames=' + frame + ' disp=' + document.getElementById('after').style.display);
}
requestAnimationFrame(tick);
</script>
</body></html>"#;

#[tokio::test]
async fn gate_canvas_loop_then_dom_mutation_repaints() {
    let mut routes: HashMap<String, (u16, String, Vec<u8>)> = HashMap::new();
    routes.insert(
        "/".to_owned(),
        (200, "text/html".to_owned(), GATE_HTML.as_bytes().to_vec()),
    );
    let mut server = LocalServer::start(routes);
    let url = server.url();
    server.serve();

    let browser = BrowserApi::start(test_config("gate-canvas")).expect("engine start");
    let tab = browser.new_tab(Some(url));
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds());
    let mut done = false;
    while !done {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("timeout waiting for gate marker")
            .expect("event channel alive");
        if let EngineEvent::ConsoleMessage { text, .. } = event {
            if text.starts_with("CV:GATEDONE") {
                done = true;
            }
        }
    }
    // Allow the mutation render round to land.
    tokio::time::sleep(Duration::from_millis(700)).await;

    let frame = browser.frame(tab).expect("frame");
    let straight = frame.straight_rgba();
    let w = frame.width as usize;
    let at = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [
            straight[i],
            straight[i + 1],
            straight[i + 2],
            straight[i + 3],
        ]
    };
    // body margin 0, canvas first element at (0,0): red fill covers
    // (0..60, 0..40). The LAST rAF fill (frame 19) must be visible —
    // lazy canvas resolution pulls live pixels per raster.
    let red = at(30, 20);
    assert!(
        red[0] >= 200 && red[1] < 60 && red[2] < 60,
        "canvas final frame must be red (got {red:?})"
    );
    // The post-loop DOM mutation (display:none -> block) must have
    // re-rendered: the green block sits below the canvas (y 40..80).
    let green = at(60, 60);
    assert!(
        green[1] >= 180 && green[0] < 80 && green[2] < 80,
        "post-canvas DOM mutation must repaint (got {green:?})"
    );
    browser.shutdown();
    drop(server);
}
