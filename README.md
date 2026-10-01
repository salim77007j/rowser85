# Rrowser85 — a lean, private, fast browser engine

Rrowser85 is an independent browser **engine core** written in Rust. It is not a
browser: it has no UI. It exposes a small, documented API (`rowser-api`) that a
shell/embedder drives to open tabs, load pages, run scripts, and receive
frames. The design goal is to beat the incumbent browsers on the two axes they
neglect: **idle resource usage** and **privacy-by-default**, while keeping a
competitive page pipeline.

```
 URL ──► networking ──► parsing ──► cascade ──► layout ──► display list ──► paint ──► Frame
         (h1/h2/h3,     (html5ever,  (lightningcss   (taffy +      (doc-order     (tiny-skia +
          QUIC/TLS,      charset      + selectors)     cosmic-text)   items)         swash glyphs)
          DoH/DoT)       sniffing)
              │                                                        ▲
              └── privacy gate (adblock, CNAME cloaking, CHIPS) ────────┘
                                        │
                          js (QuickJS-ng + Web APIs) ◄── events
```

## Why it is different

| Axis | Incumbent browsers | Rrowser85 |
|---|---|---|
| Idle RAM (8 tabs, blank) | hundreds of MB | **~90 MB engine-wide** (measured in CI: `BOOT_MS=4 RSS_KB≈91 000`) |
| Cold start | 300 ms – 2 s | **~4 ms** (engine boot, no UI) |
| Background tabs | throttled at best | **frozen**: frames freed, JS timers gated, resumed on focus |
| JS engine | V8 / SpiderMonkey (~30 MB per isolate) | **QuickJS-ng** (~1 MB footprint, arena-allocated) |
| Telemetry | on by default | **zero, and the setting is an opt-in flag** |
| Tracking protection | extension/site-list | **network-layer** adblock + CNAME-cloaking gate + CHIPS partitioned cookies + per-tab fingerprint spoofing |

The engine is privacy-first: every privacy feature is on by default and each
one is enforced in the engine core, not in a UI layer that a site can dodge.

## Quick start

```bash
git clone https://github.com/salim77007j/rowser85
cd rowser85
cargo build --workspace            # ~3 min from clean on 2 cores
cargo test --workspace             # 45 tests incl. end-to-end + fuzz
cargo bench -p rowser-benchmarks   # pipeline micro-benchmarks
```

Embedding (the whole surface a UI needs):

```rust
use rowser_api::BrowserApi;
use rowser_engine::EngineConfig;

let browser = BrowserApi::start(EngineConfig {
    profile_dir: dirs::data_local_dir().unwrap().join("rowser-profile"),
    ..EngineConfig::default()
})?;

let tab = browser.new_tab(Some("https://example.org".into()));
let mut events = browser.events(); // tokio broadcast channel

// In your UI loop:
while let Ok(event) = events.recv().await {
    if let rowser_api::EngineEvent::FrameReady { tab, .. } = event {
        let frame = browser.frame(tab).unwrap(); // RGBA, saveable as PNG
        blit_to_window(frame.width(), frame.height(), &frame.straight_rgba());
    }
}
```

`api/examples/idle_report.rs` is a complete runnable embedder — it boots the
engine, opens tabs, and prints `BOOT_MS / RSS_KB / CPU_SECONDS` for benchmark
comparisons.

## Repository layout

| Crate | Path | Role |
|---|---|---|
| `rowser-dom` | `dom/` | Arena DOM (free-list + generations), selector engine integration |
| `rowser-parsing` | `parsing/` | html5ever TreeSink → arena DOM, lightningcss → style rules, cascade, UA stylesheet |
| `rowser-layout` | `layout/` | taffy box/flex/grid layout, cosmic-text rich-text shaping |
| `rowser-rendering` | `rendering/` | Display list, tiny-skia rasterizer, swash glyph blitting, Frame |
| `rowser-js` | `js/` | QuickJS-ng runtime, Web APIs (DOM, Fetch, XHR, WebSocket, Workers, timers, storage) |
| `rowser-networking` | `networking/` | HTTP/1.1 + HTTP/2 (hyper) + HTTP/3 (quinn/h3), rustls TLS 1.3, DoH/DoT/DoQ DNS, WebSocket, HTTPS upgrade |
| `rowser-storage` | `storage/` | redb profile: CHIPS cookies, localStorage, IndexedDB, HTTP cache |
| `rowser-privacy` | `privacy/` | Brave-derived adblock, PSL, CNAME-cloaking guard, anti-fingerprint spoofing, local safe browsing |
| `rowser-engine` | `engine/` | Multi-threaded page orchestration, navigation, suspension, memory manager |
| `rowser-api` | `api/` | The documented UI-facing facade (this is the only crate a UI imports) |
| `rowser-tests` | `tests/` | End-to-end, integration and fuzz/property suites |
| `rowser-benchmarks` | `benchmarks/` | Criterion micro-benchmarks + browser comparison harness |

Docs: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) ·
[`docs/API.md`](docs/API.md) · [`docs/BUILD.md`](docs/BUILD.md) ·
[`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) ·
[`LIBRARY_CHOICES.md`](LIBRARY_CHOICES.md)

## Status (v1)

Working today: the full HTML→pixels pipeline, scripting with storage, QUIC-era
networking with privacy gates, aggressive tab suspension, and the embedding
API. Known v1 limitations are listed in
[`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) (inline layout is flattened to
text runs; border radii and miters are not painted; HTTP/3 pool is
opportunistic). The engine is not yet hardened for hostile content — do not
expose it to the open internet without the sandbox layer of a full browser.

## License

MIT (per-crate `license.workspace`).
