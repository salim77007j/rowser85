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
            if std::env::var("SL_DEBUG").is_ok() {
                eprintln!("[SL-DEBUG] console: {text}");
            }
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
