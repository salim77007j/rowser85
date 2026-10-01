//! End-to-end engine tests over a local HTTP server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rowser_api::{BrowserApi, EngineEvent};
use rowser_engine::EngineConfig;

use rowser_tests::LocalServer;

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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
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
    let mut events = browser.events();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
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
