# API reference — `rowser-api`

`rowser-api` is the only crate a UI should import. It is synchronous on the
caller side (all commands are channel sends) except for the event stream,
which is a tokio broadcast receiver.

```toml
[dependencies]
rowser-api = { path = "../api" }
rowser-engine = { path = "../engine" }   # only for EngineConfig
```

## `BrowserApi`

```rust
pub fn start(config: EngineConfig) -> anyhow::Result<BrowserApi>;
pub async fn start_async(config: EngineConfig) -> anyhow::Result<BrowserApi>;
```

Boots the engine (page threads spawn lazily per tab). Safe to call from
inside a tokio runtime — network bootstrap happens on a dedicated thread.

| Method | Effect |
|---|---|
| `new_tab(url: Option<String>) -> TabId` | Creates a tab and navigates if a URL is given. |
| `navigate(tab, url)` | Resets the tab and navigates. |
| `close_tab(tab)` | Joins the tab's page thread, drops its snapshot. |
| `focus(tab)` | Marks a tab active; all others become suspension candidates. Resumes a suspended tab. |
| `set_viewport(tab, width, height)` | CSS-pixel viewport (reflows on next frame). |
| `scroll(tab, y)` | Document-space vertical scroll for the next paint. |
| `click(tab, node)` / `ui_event(tab, node, "click"\|"input"\|…)` | Dispatches a UI event to a DOM node handle (from hit-testing). |
| `set_privacy(settings: PrivacySettings)` | Hot-swaps privacy settings (blocking, upgrade, spoofing…). |
| `events() -> broadcast::Receiver<EngineEvent>` | New subscription to the engine event stream. |
| `frame(tab) -> Option<FrameView>` | The latest rendered frame (RGBA8). |
| `snapshot(tab) -> Option<TabSnapshot>` | Url, title, loading, memory estimate. |
| `tabs() -> Vec<TabId>` | Live tab ids. |
| `title(tab) -> Option<String>` | Document title. |
| `content_size(tab) -> Option<(f32, f32)>` | Scrollable content size. |
| `shutdown()` | Freezes tabs, joins page threads, flushes storage, drops the runtime. |

`BrowserApi` is `Clone` (it is a handle over channels) — share it freely
across UI threads. Call `shutdown()` exactly once before dropping.

## `EngineConfig`

| Field | Default | Meaning |
|---|---|---|
| `profile_dir` | temp/`rowser-profile` | redb profile location (cookies, LS, IDB, cache). |
| `privacy` | see below | Privacy toggles. |
| `js.memory_limit` | 96 MiB | QuickJS-ng heap ceiling per page. |
| `js.script_timeout` | 10 s | Watchdog interrupt budget per script. |
| `js.stack_size` | 1 MiB | JS stack ceiling. |
| `suspend_after` | 300 s | Background idle time before a tab freezes. |
| `memory_tick` | 30 s | Memory manager sampling interval. |
| `memory_budget_fraction` | 0.35 | Fraction of total RAM usable by tabs. |

## `PrivacySettings` (all default **on** except telemetry)

| Flag | Effect |
|---|---|
| `block_ads` | Network-layer ad/tracker blocking (Brave engine). |
| `https_upgrade` | http→https upgrade (localhost exempt). |
| `block_third_party_cookies` | Reject unpartitioned third-party cookies (CHIPS stays). |
| `anti_fingerprinting` | Per-tab consistent spoofing (canvas/audio/navigator/screen). |
| `webrtc_protection` | WebRTC candidate filtering. |
| `safe_browsing` | Local hash-prefix checks; no URLs leave the device. |
| `telemetry_opt_in` | **Off.** There is no telemetry collection to enable by default. |

## `EngineEvent`

Emitted on the broadcast stream; `Lagged` is recoverable (the UI re-reads
snapshots), `Closed` means engine shutdown.

```text
TabCreated(TabId) / TabClosed(TabId)
NavigationStarted { tab, url }
PageLoaded { tab, url, title }          // first frame + scripts done
LoadProgress { tab, progress: f32 }
FrameReady { tab, frame: u64 }          // new frame available in the snapshot
TitleChanged { tab, title }
ConsoleMessage { tab, level, text }     // "log" | "info" | "warn" | "error"
BlockedRequest { tab, url, reason }     // privacy engine verdict
TabSuspended(TabId) / TabResumed(TabId)
MemoryPressure { total_bytes }
```

## `FrameView`

| Method | Meaning |
|---|---|
| `width() / height() -> u32` | Pixel dimensions. |
| `straight_rgba() -> Vec<u8>` | Straight (non-premultiplied) RGBA8 for UI blitting. |
| `save_png(path)` | Writes the frame to disk (screenshots, tests, CI artifacts). |

## Event-loop recipe

```rust
let browser = BrowserApi::start(config)?;
let tab = browser.new_tab(Some("https://example.org".into()));
let mut events = browser.events();
loop {
    match events.recv().await {
        Ok(EngineEvent::FrameReady { tab, .. }) => {
            let frame = browser.frame(tab).unwrap();
            present(&frame);
        }
        Ok(EngineEvent::PageLoaded { tab, title, .. }) => { /* update tab bar */ }
        Ok(EngineEvent::TabSuspended(t)) => { /* dim tab */ }
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
        Err(_) => break, // engine gone
    }
}
```

Hit-testing: the v1 API exposes node handles via the DOM node id (`u64`);
frame→node hit-testing helpers land with the retained-mode display list in
v2. Until then, use `ui_event` with node ids obtained from your own
hit-testing against `content_size`.

## Guarantees and non-guarantees

Guaranteed: no page stalls the UI (page threads are isolated), no frame
delivers torn state (snapshots are swapped atomically), suspension never
loses the page (resume re-renders from the retained DOM), and every command
returns immediately (channel sends).

Not yet guaranteed: per-site process isolation (shell's sandbox layer),
layout stability across versions (v1 inline model is documented in
`docs/ARCHITECTURE.md`), and Web API completeness (see `js/` coverage tables
in `docs/API.md#web-api-coverage`).

## Web API coverage (JS surface)

Implemented: `document.querySelector(All)`, `getElementById`,
`createElement`*, `setAttribute/removeAttribute`, `textContent`,
`Node.parentNode/childNodes`*, `window.*`, `console.*`,
`setTimeout/clearTimeout/setInterval/clearInterval`, `localStorage`,
`fetch` (headers, methods, body, response text), `XMLHttpRequest` (open,
send, setRequestHeader, onreadystatechange), `WebSocket` (open/message/close,
send), `Worker` (dedicated, `postMessage`/`onmessage`), `navigator` spoofed
subset, `location` read, `performance.now`*, `structuredClone`*.

(\* partial — semantics documented in the `js/` crate docs.)

Not implemented: Service Workers, history/pushState, CSSOM mutation,
WebGL/Canvas 2D (canvas returns spoofed noise data), WebAudio rendering
(metadata only), WebRTC peer connections (candidates filtered).
