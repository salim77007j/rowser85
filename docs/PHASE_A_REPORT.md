# Phase A Report — JavaScript Platform (Group A)

**Date**: 2026-10-03/04 · **Branch**: `main` · **Commits**: `ad261bb`, `7fa8111`, `bf154c2`
**Suite**: 99 tests green (was 97), `cargo fmt` + `clippy --workspace --all-targets -D warnings` clean.
**Method**: Playwright Chromium ground truth (1360×745, networkidle+settle) → rowser85 render → side-by-side composite + grayscale threshold-diff → VLM structural verdict.

> Context: this session started from a full environment reset (fresh clone,
> toolchain + system libs re-provisioned). The last-known session state was
> mid-work on the script load-event dispatch fix, which was re-derived and
> landed here as `ad261bb`.

---

## 1. What was fixed in Group A (this phase)

### 1.1 Script load-event dispatch (`ad261bb`) — the in-flight fix, completed
Every async bootstrap on the web gates on `<script>` `load`/`error` events.
Before: those events never fired, and dynamically-inserted scripts
(`createElement` + `appendChild`) were never fetched at all.

- **Parser scripts** fire `load` on the element after execution
  (non-bubbling per HTML — window `load` listeners must not see per-script
  events); failed fetches fire `error` from the `SubresourceFailed` path.
- **Dynamic `src` scripts**: `afterInsert` detects `<script>` elements
  entering the document and requests a fetch through the new
  `JsCommand::ScriptFetch` → engine network task → page-thread eval (with
  `document.currentScript` set) → `load`/`error` on the element.
- **Inline dynamic scripts** execute synchronously (indirect eval, global
  scope) at insertion, then fire `load`.
- `HTMLScriptElement` gained real `src`/`async`/`defer`/`type`/`text`
  accessors; `s.src = url` and `setAttribute('src', …)` on a connected
  element prepare the script like a browser does.
- `Element.prototype` gained `on*` handler properties (50 event names)
  stored in the same `_ls` store `addEventListener` uses; window lifecycle
  events now also call `window.onload`-style inline handlers.
- En-route bug found and fixed: an early no-op match arm in the page
  thread's `handle_js_command` swallowed `ScriptFetch` before it could
  reach the engine's fetch task.

### 1.2 Viewport metrics JSON parse (`7fa8111`)
`__native_dom_viewport` returns a JSON string; the prelude read `.scrollY`
off the raw string (undefined). Net effect: `window.scrollY` always read 0,
`scrollBy` always scrolled from 0, `innerWidth`/`innerHeight` always fell
back to hardcoded constants instead of the live viewport. The page-thread
scroll mirror was correct all along; only the JS-side read was broken.
Fixed with `JSON.parse`; covered by a new E2E test.

### 1.3 GitHub/HN capture wedge — root-caused and mitigated (`bf154c2`)
Reproduced deterministically: github.com → RSS 282 MB → 1.6 GB in 30 s,
loading screen never clearing, typed navigation dropped.
Decomposed into:
1. **Sustained-dirty render loop**: pages whose every rAF/observer
   reaction dirties the DOM re-rendered forever; our full re-render costs
   5–7 s on JS-heavy pages → render→observer→mutate→render ground the
   2-core box. Mitigation (engine-side pacing): after a >800 ms render and
   a >4-render dirty streak, the next re-render waits cost/2 (1–4 s clamp).
   After the fix github renders its dark-theme hero, settles `dirty=false`,
   and the machine stays alive. Fast pages are unaffected.
2. **Capture rig reliability**: typed navigation collided with busy page
   threads. `rowser` now accepts **argv[1] as a startup URL** (standard
   desktop-browser behavior, also a product feature), and the capture rig
   launches a fresh browser + profile per page with per-site settle floors.
   No synthetic typing, no cross-page interference.

### 1.4 Group A API end-to-end verification (test coverage matrix)

| API | Evidence (test) | Status |
|---|---|---|
| MutationObserver (childList/attrs/charData/subtree, microtask batching, disconnect) | 3 unit tests + MO re-delivery | ✅ |
| IntersectionObserver (real layout geometry, thresholds) | `observers_fire_with_real_geometry` (E2E) | ✅ |
| ResizeObserver (layout box snapshots) | `observers_fire_with_real_geometry` (E2E) | ✅ |
| history (pushState/replaceState/back/forward/go, popstate, hashchange) | unit + `history_pushstate_popstate_roundtrip` (E2E) | ✅ |
| matchMedia (viewport query evaluation) | `match_media_evaluates_viewport_queries` | ✅ |
| sessionStorage (per-tab, survives reload) | `session_storage_survives_reload` (E2E) | ✅ |
| localStorage | `localstorage_roundtrip` | ✅ |
| requestAnimationFrame (frame clock, focused-tab gating) | `raf_frame_clock_advances` (E2E) | ✅ |
| scrollTo/scrollBy/scroll + scrollY mirror | `programmatic_scroll_updates_mirror` (E2E, new) | ✅ |
| Script load/error events (parser/dynamic/inline, window.onload once) | `script_load_events_end_to_end` (E2E, new) | ✅ |

---

## 2. Before/after pixel diffs (Chrome ground truth, grayscale threshold diff)

| Site | Diff before (session start) | Diff after | Note |
|---|---|---|---|
| example.com | 6.0% | **6.0%** | Chrome-equivalent (unchanged) |
| Bing search | 9.3% | **9.7%** | Chrome-equivalent (live result variance) |
| Wikipedia | 22.2% | **22.2%** | Content-level (gaps = C1 SVG logo + typography, not JS) |
| rust-lang.org | 51.1% | **49.9%** | Partial (gaps = C1 SVG sizing + font weight, not JS) |
| GitHub | **97.6%** (stuck on loading screen, 269 MB→1.6 GB RSS) | **20.1%** | Renders dark-theme hero + nav; biggest fix of the phase |
| Hacker News | **96.4%** (navigation input dropped → previous page shown) | **16.3%** | Real HN feed renders; residual = live-feed content drift |

Side-by-side composites: `screenshots/phase-a/compare/side-by-side-<site>.png`
(raw captures in `screenshots/phase-a/{chrome,rowser}/`).

## 3. Which sites now render Chrome-equivalent?

VLM structural verdicts against the composites:

| Site | Verdict | Biggest visual gap |
|---|---|---|
| example.com | **Yes (Chrome-equivalent)** | Missing the small document icon (SVG) — C1 |
| Bing | **Mostly (Chrome-equivalent)** | Live result-set variance; layout matches |
| Hacker News | **Mostly (near-equivalent)** | Live feed content drift; structure/format match |
| GitHub | **Mostly** | Hero typography scale/weight; background gradient cloud missing (Group B filters/gradients) |
| Wikipedia | **Mostly (content-level)** | Logo (SVG) missing; sidebar/typography deltas |
| rust-lang.org | **Different (partial)** | Language selector renders expanded (media-query default state); font-weight mapping; SVG logo sizing — all C1/Group B |

## 4. Remaining gaps in Group A (honest)

1. **MediaQueryList change events** — `matchMedia().addEventListener('change')`
   never fires (no resize re-evaluation loop). Sites reading `matches` at
   boot work; sites reacting to live resizes do not.
2. **ES Modules (A7)** — `<script type="module">` / dynamic `import()` not
   implemented: module scripts are currently skipped (treated as classic
   scripts with inline semantics where harmless). Modern bundle-free sites
   won't boot; bundled sites (the majority of the top-30 list) are fine.
3. **IndexedDB (A8 tail)** — not implemented (localStorage/sessionStorage
   cover the persistence most sites need; IndexedDB is app-specific).
4. **WAAPI `element.animate()` (A9)** — not implemented (motion is mostly
   CSS-driven on the tested sites).
5. **EventSource (A10 tail)** — WebSocket is live; SSE not yet.
6. **`document.cookie`** read/write is a stub (cookie jar exists
   network-side; the JS surface isn't wired).
7. **Per-render cost** (feeds every JS-driven page): one full github render
   costs 5.7 s layout + ~1.3 GB high-water on this 2-core box. Pacing keeps
   the browser alive, but the real fix is Group E incremental layout. This
   is the single biggest lever for the remaining GitHub/Wikipedia deltas.

## 5. Handoff to Phase B (Advanced CSS)

Everything in this phase is committed and pushed; the suite is green.
Phase B scope (per the phase plan): sticky/fixed, transforms,
transitions/animations, filters, box/text-shadow, border-radius (full),
background-image (full), calc/min/max/clamp, custom properties (runtime),
pseudo-elements/states, columns, writing modes, table completeness,
overflow scroll/auto, grid completeness.
