# Architecture

## Design principles

1. **One pipeline, no hidden re-entrancy.** Each stage (parse → cascade →
   layout → display list → paint) is a pure function of its inputs plus
   caches. Stages never call later stages.
2. **Threads own data; channels own control flow.** Every tab is one page
   thread with exclusive access to its DOM/layout/painter. The engine loop
   owns the network context and blocklist. Nothing shared is mutated without
   a mutex that is held for bounded, small sections.
3. **Idle is the enemy.** Unfocused tabs are frozen (frames freed, timers
   gated, runtime interrupted); the memory manager drops caches when the
   process exceeds its RAM budget.
4. **Privacy is engine-level, not UI-level.** Blocking, partitioning, spoofing
   and upgrading happen below the page's visibility, so pages cannot detect
   or route around the shell.

## Crate graph

```
            ┌─────────── rowser-api (UI facade) ───────────┐
            │                                             │
        rowser-engine  ──────────────► rowser-memory      │
        │  │  │  │                                     (engine)
   dom │  │  │  └── rowser-js ──► rquickjs (QuickJS-ng)
       │  │  └───── rowser-networking ─► hyper, quinn/h3, rustls,
       │  │                              hickory, tungstenite
       │  └──────── rowser-privacy ──► adblock, psl/addr
       └─────────── rowser-storage ──► redb, cookie
            │
   parsing ─► layout ─► rendering
 (html5ever,   (taffy,      (tiny-skia,
  lightningcss)  cosmic-text)  swash)
```

Dependencies point downward only; `rowser-api` is the sole public surface.

## Threading model

```
UI thread(s)                      engine bootstrap (scoped, exits)
  BrowserApi ──cmd channel──►  engine loop thread ──┬─► page thread (per tab)
  ▲   │  ▲                        │  ▲              │      DOM + cascade +
  │   │  └── broadcast events ────┘  │              │      layout + paint + JS
  │   └──── snapshots (mutex) ◄──────┘              │
  └──────────── frame views ◄────────────────────────┘
        engine runtime (tokio, 2 workers): fetches, DNS, WS, timers
        cname gate thread: dedicated resolver + cloaking guard
```

- **Engine loop** (`rowser-engine`): a `std::thread` running a
  `recv_timeout(250ms)` loop over a single command channel. Owns the tab
  table, the blocklist (contains non-`Send` `Rc`s — confined to this thread),
  the CNAME gate handle and the tokio `Runtime` (built on a scoped bootstrap
  thread so callers may themselves be inside a tokio worker).
- **Page thread** (one per tab): owns the arena DOM, computed styles, layout,
  display list, painter and the QuickJS-ng runtime — no locks for the entire
  render path. Receives `Message`s (navigate, subresource done, JS event,
  suspend/resume, shutdown) and drives re-renders when the DOM version bumps.
- **Engine runtime**: a 2-worker tokio runtime for all network I/O, WebSocket
  pumps, JS fetch/XHR tasks and timers. Fetch results are funneled back to
  the engine loop via the command channel, then forwarded to the owning page
  thread — pages never await the network.
- **CNAME gate**: a dedicated thread with its own resolver; third-party
  requests are held until the DNS chain is proven non-cloaked, then released
  to the normal fetch path (or blocked).

## Page load pipeline

1. **Navigate** — the engine loop creates the tab snapshot and the page thread
   receives `Message::Navigate`. It resets all stage state and sends a
   `FetchSubresources` command for the document.
2. **Privacy gate** (engine loop, before any socket opens):
   `blocklist_check` (Brave `adblock` engine over the tracker ruleset),
   HTTPS-upgrade (http→https unless localhost), then third-party requests go
   through the CNAME gate.
3. **Fetch** — `rowser_networking::fetch`: scheme dispatch (data:, about:,
   http/https), CHIPS-aware cookie attach, transport (H3 if QUIC succeeds,
   H2/H1 otherwise), Set-Cookie partitioned persistence, redirect chain (≤20),
   HTTP-cache store for cacheable subresources.
4. **Parse** — `html5ever` drives a custom `TreeSink` that builds the arena
   DOM directly (slot ids, no `Rc` soup). Charset sniffing: BOM, then
   `<meta charset>` in the first 2 KiB. `lightningcss` parses stylesheets into
   `StyleRuleEntry` items whose selectors are re-parsed with the `selectors`
   crate (shared parser context, per-DOM caches reused across cascade passes).
5. **Cascade** — UA stylesheet (~60 rules) + author sheets + inline styles.
   Specificity ordering with a two-pass `!important` handling, inheritance,
   em/rem/% resolution, currentColor, and a rule bucket index (tag/id/class)
   so candidate sets stay O(small).
6. **Layout** — a taffy tree is built from computed styles (block/flex/grid
   mapped), inline content is flattened into rich text runs with per-span
   attributes, and cosmic-text shapes them inside taffy's measure callback
   (width-keyed shape cache).
7. **Display list** — document-order items: backgrounds, borders, images, text
   runs. Flat and GPU-ready.
8. **Paint** — tiny-skia rasterizes to a premultiplied RGBA `Frame`; swash
   provides subpixel glyph masks and color-emoji content, blitted with
   manual premultiplied src-over. `Frame::save_png` / `straight_rgba()` for
   the embedder.
9. **Scripts** — the page thread constructs a `JsRuntime` (QuickJS-ng) with a
   `PageBridge` (DOM access into the same arena, timers, storage, fetch/XHR
   proxied to the engine runtime). External scripts are fetched like any
   subresource and evaluated when the set completes; `PageLoaded` fires once
   the first frame + scripts are done.
10. **Events** — `EngineEvent`s are broadcast (tokio broadcast, 256 slots);
    the API re-types them for the embedder.

## Suspension & memory

- A tab that is neither focused nor interacting for `suspend_after`
  (default 300 s, e2e-tested at 500 ms) is **suspended**: its page thread
  freezes (no message processing except `Resume`/`Shutdown`), its frame
  snapshot is dropped, its QuickJS runtime is interrupted at the next
  interrupt-check, and timers stop firing. `focus(tab)` resumes it.
- The memory manager (`engine/src/memory.rs`) samples per-tab RSS (sysinfo)
  every `memory_tick`; when total tab memory exceeds
  `memory_budget_fraction` (default 35%) of RAM, it first drops painter
  caches, then suspends the most expensive background tabs. `MemoryPressure`
  events let the UI surface the state.

## Storage

One redb file per profile, four logical tables:

- **cookies** — CHIPS-partitioned (key: `host ∥ partition site`), with
  third-party policy enforcement at write and read time.
- **localStorage** — per-origin key/value with size accounting.
- **IndexedDB** — record-per-entry object stores with cursor-friendly
  ordering; the JS layer maps the Web API onto it.
- **http cache** — URL-keyed bodies with LRU trimming against a byte budget
  (default 32 MB) and `no-store` respect.

## JS runtime

`rquickjs` bundles **QuickJS-ng 0.16.2** (vendored and verified). Per page:

- Heap limit 96 MB (arena-allocated; freed wholesale on navigation), stack
  1 MB, script watchdog 10 s via the interrupt handler.
- The `PageBridge` implements: `document` (querySelector, getElementById,
  setAttribute/textContent into the arena DOM), `window`, `console`,
  `setTimeout/clearTimeout/Interval`, `localStorage`, `fetch` + `XMLHttpRequest`
  (proxied to the engine runtime, results routed back as `JsEvent`s),
  `WebSocket` (event-pump on the runtime, commands from the page thread), and
  `Worker` (a second QuickJS runtime per worker with `postMessage` channels).
- Fingerprint spoofing: each tab derives a per-tab `SpoofProfile` from a
  blake3 seed — canvas noise, `navigator.*`, screen geometry, timezone and
  audio context are perturbed consistently for the tab's lifetime.

## Privacy gates (in order of a request's life)

1. Tracker/ad blocklist (Brave engine + curated ruleset, network + cosmetic
   option semantics).
2. HTTPS upgrade.
3. Third-party classification via PSL registrable domains.
4. CNAME-cloaking gate (resolve chain; if any hop lands on a different
   registrable domain than the visible host, block).
5. CHIPS cookie partitioning + third-party cookie rejection.
6. Anti-fingerprinting at the JS boundary (spoofed `navigator`, canvas/audio
   noise, font/screen normalization).
7. Local-only safe browsing (hash-prefix match against a local set; no URL
   leaves the device).
8. WebRTC leak protection (candidate filtering at the JS bridge).

## Known limitations (v1)

- Inline layout flattens inline boxes to styled text runs (no
  inline-block borders; no baseline-aligned mixed content).
- `display:contents` is approximated.
- Lab()/LCH() colors are approximated to sRGB (OKLab/OKLCH are exact).
- Borders paint as rects: no radii, no miters/joins.
- HTTP/3 is opportunistic: first QUIC handshake failure silently falls back
  to H2/H1.
- The sandbox layer (process isolation per site) is the UI shell's
  responsibility in v1; the engine's threat model assumes semi-trusted
  content until it ships.
