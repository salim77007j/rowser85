# V2 Plan — Close Every Remaining Gap

**Author**: principal architect · **Date**: 2026-10-03
**Baseline**: commit `489bae0` (5-gap layout mission complete, all verdicts "Yes")
**Mission**: render ANY real-world website competitively with Chrome.

---

## 1. Audit — where the engine stands

### 1.1 What already works (verified side-by-side vs Chrome)

| Area | State | Evidence |
|------|-------|----------|
| Floats, CSS Grid, overflow:hidden clip stack, @font-face (WOFF1/2/TTF/OTF), colspan/rowspan | Yes | `screenshots/compare-step1..5/` |
| example.com, Hacker News, Bing | Chrome-equivalent | `screenshots/final-5gaps/` |
| Web Components (custom elements, shadow DOM, slots, templates) | Works | `screenshots/m4-webcomponents.png` |
| Media: MP4 direct, MSE, HLS fMP4 + TS, audio | Works | `screenshots/m1-m7*` |
| 114 engine tests, fmt, clippy -D warnings, CI green | Pass | GitHub Actions |

### 1.2 What the audit found still broken

The three "Mostly" sites (Wikipedia, GitHub, rust-lang.org) fail for a small
set of concrete reasons, confirmed by code audit + VLM side-by-side analysis:

1. **JS platform stubs** (Group A): `MutationObserver.observe()` is a no-op;
   `IntersectionObserver` fakes "always intersecting"; `ResizeObserver`
   missing; `requestAnimationFrame` is `setTimeout(16)`; `history.pushState/
   replaceState` are no-ops; `matchMedia()` always returns `matches:false`;
   `sessionStorage`/`EventSource` missing. Wikipedia's sidebar, GitHub's
   header chrome and rust-lang.org's animated sections are all JS-built.
2. **SVG sizing** (C1): no `viewBox`/`width`/`height`/`preserveAspectRatio`
   handling and no SVG rasterizer in the tree — rust-lang.org's logo renders
   mis-sized, GitHub's inline SVG Octocat logo is missing entirely.
3. **Typography fidelity**: `font-weight` 700–900 maps to the same face
   (hero headings render visibly lighter than Chrome on GitHub and
   rust-lang.org); `letter-spacing`/`word-spacing` not honored.
4. **Modern design language** (Group B): no `border-radius`, no
   `box-shadow`/`text-shadow`, no `transform`, no `transition`/`@keyframes`,
   no `calc()`/`min()`/`max()`/`clamp()`, no `position:fixed`/`sticky`
   (`PositionMode` enum has only Static/Relative/Absolute), background-image
   layers limited, no CSS columns, no writing modes.
5. **Platform APIs** (Group D): no Canvas 2D, no WebGL/WebGPU, no
   WebAssembly, no WebRTC, no EventSource.
6. **Performance** (Group E): whole-document re-layout on every dirty flag;
   no layer compositing.

### 1.3 Environment constraint (2 cores, 3 GB RAM)

Every engine iteration costs a 2–6 min rebuild. This is the binding
constraint on scope. The plan below is ordered so that each phase lands
maximum real-world impact per rebuild.

---

## 2. Library decisions (2026 crate survey)

| Need | Choice | Why |
|------|--------|-----|
| SVG rasterization | **resvg 0.47 + tiny-skia** (already a dep) | Reference-quality, pure-Rust, same backend as our painter; usvg handles viewBox/pAR precisely. |
| Canvas 2D | **custom on tiny-skia** | tiny-skia already in tree; no new heavyweight deps; full control of state machine. |
| Fonts | keep cosmic-text + fontdb | Fallback chains already correct; add weight→face mapping. |
| Layout | keep taffy 0.14 | Grid/flex/float native; sticky/fixed handled in our layer. |
| JS | keep QuickJS-ng via rquickjs | Observers/history/media are engine-side work, not runtime work. |
| WebAssembly | **wasmi** (interpreter) *deferred* | wasmtime compiles ~30+ min on 2 cores; wasmi is lightweight and enough for small WASM usage. Honest deferral if time runs out. |
| WebGL/WebGPU | **deferred, documented** | wgpu adds ~400 deps; compile cost prohibitive here; honest report instead. |

**Servo embedding question**: rejected as primary path. Servo's crates.io
surface does not expose a turnkey "embed HTML→pixels" pipeline; we would
import Stylo (huge build), and our layout/paint stack is already at parity
for the 20% of CSS that drives 95% of real pages. Better ROI: close the
gaps listed below in our own stack, which we control and can debug.
Tradeoff documented here per the autonomy mandate.

---

## 3. Priority order (impact-ranked, not alphabetical)

### Phase 1 — JS platform completeness (Group A core) — *every modern site*
- A1 MutationObserver: real childList/attributes/characterData/subtree
  records, delivered as microtask batch after each script turn.
- A1 IntersectionObserver: real geometry from the layout tree (rootMargin,
  thresholds); drives lazy-loading correctly.
- A1 ResizeObserver: layout-box snapshots, fires on relayout diff.
- A3 History: pushState/replaceState/back/forward/go + state; popstate +
  hashchange events; address bar sync.
- A4 matchMedia: real evaluation against the media context (width
  breakpoints) + change events; window.innerWidth/innerHeight/scrollX/Y;
  scrollTo/scrollBy/scroll.
- A2 rAF: driven from the frame clock (one callback batch per presented
  frame, nested rAF scheduling); requestIdleCallback on idle ticks.
- A8 sessionStorage (per-tab); A10 EventSource (SSE over existing fetch).

### Phase 2 — SVG + typography (C1 + font weight) — *logos and headings everywhere*
- Inline SVG: serialize + resvg rasterization at correct CSS box size;
  viewBox/width/height/preserveAspectRatio discipline per SVG2.
- `<img src="*.svg">`: decode through the same resvg path.
- font-weight: map 100–900 to actual faces (select from family variants).

### Phase 3 — Positioning + scroll containers (B1/B14)
- position: fixed (viewport-anchored, painted above scrolled content).
- position: sticky (offset math against scroll container per spec).
- overflow: scroll/auto containers with real scroll offsets feeding
  IntersectionObserver/sticky.

### Phase 4 — Modern design language (B5/B6/B8/B7)
- border-radius (4 corners, elliptical, % radii, clip of bg/borders).
- box-shadow (multiple, inset, spread, blur) + text-shadow.
- calc()/min()/max()/clamp() at parse time (lightningcss evaluation).
- background-image layers: size cover/contain/length/%, position
  keywords+4-value, repeat modes, origin/clip.

### Phase 5 — Motion (B2/B3, A9)
- transform 2D (translate/rotate/scale/skew/matrix) + transform-origin.
- transition + @keyframes animation, cubic-bezier/steps easing, driven
  from the frame clock; element.animate() (WAAPI) on the same core.

### Phase 6 — Canvas 2D (D1)
- Full state machine on tiny-skia: paths, arcs, curves, text, images,
  gradients, patterns, clip, composite ops, shadows, filters,
  getImageData/putImageData, toDataURL.

### Phase 7 — 30-site real-world suite + V2_FINAL_REPORT.md
- Side-by-side (rowser85 | Chrome) for every site in the mandated list.
- Verdict table Yes/Mostly/No, console-error counts, capabilities matrix,
- performance numbers, honest verdict, v3 recommendations.

### Deferred with justification (documented in final report)
- WebGL2/WebGPU (wgpu compile cost vs 2-core budget; GPU absent in CI).
- WebAssembly (wasmi candidate; only if Phase 1–6 land early).
- WebRTC (webrtc-rs is a multi-session effort on its own).
- IndexedDB (localStorage/sessionStorage cover the persistence most
  sites need; IndexedDB is app-specific).
- WebTransport, writing modes, CSS columns, subgrid (tail-end).

Each phase ends with: Chrome ground truth capture → implement →
brows12 render → side-by-side compare → iterate → **commit + push**.

---

## 4. Method (unchanged, non-negotiable)

1. Playwright Chromium ground truth (1360×860, networkidle + settle).
2. Implement the fix.
3. Render the same URL in rowser85 (xvfb + xdriver + ffmpeg grab).
4. Side-by-side composite; pixel band scan + VLM structural judgement.
5. Iterate until match or honestly document the delta.
6. One fix per commit; push immediately.

## 5. Risk register

| Risk | Mitigation |
|------|-----------|
| Rebuild cost (2 cores) | batch related changes per crate; use `cargo build -p` for quick cycles |
| taffy sticky not native | implement sticky in our own layout pass (offset recomputation) |
| resvg pulls heavy deps | it reuses tiny-skia; watch lockfile growth |
| JS observer re-entrancy | deliver records as microtasks after the script turn; no re-entrant layout |
| Media/test regressions | full `cargo test --workspace` gate before every push |
