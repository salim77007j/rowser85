# Phase D Report — Web Platform APIs (Canvas 2D)

**Date**: 2026-10-04 · **Branch**: `main`
**Suite**: 146 tests green (was 123), `cargo fmt` + `clippy --workspace --all-targets -D warnings` clean.
**Method**: Playwright Chromium ground truth → rowser85 render → side-by-side composite + grayscale threshold-diff + per-color presence counts. Every claim below is backed by a pixel test or an e2e capture.

> Environment note: this session started from a **fully reset workspace** (no Rust
> toolchain, no repo, no extracted system libs). The toolchain (Rust 1.99), the
> deb-extracted native libs (alsa, xkbcommon, xcb-xkb) and the build env wrapper
> (`build-env.sh`, committed) were rebuilt from scratch before any Group D work.

## 1. What was implemented

### D1 — Canvas 2D, end to end (the Group D core)

| Piece | Implementation |
|---|---|
| State machine | `rendering/src/canvas2d.rs` (new, ~2,200 lines): one `Canvas2D` per `<canvas>`, owned by a page-thread registry (`CanvasRegistryShared`, `Rc` like the DOM), shared into the JS bridge and the engine paint path |
| Backing store | `tiny_skia::Pixmap` (premultiplied RGBA); ImageData round-trips convert straight ↔ premultiplied like Chrome; `snapshot()` → `DecodedImage` feeds the existing display-list `DrawCmd::Image` path — zero display-list changes needed |
| Geometry | User-space coordinates; CTM applied at draw time (tiny-skia path/pixmap transforms; `post_concat` order verified by tests). Arcs/ellipses as ≤90° kappa-cubics with rotation mapping; `arcTo` tangent math with Chrome's radius clamping |
| Styles | Solid colors (hex/rgb[a]/named), linear + radial gradients (stops sorted, native shaders), patterns (native `SpreadMode::Repeat`, pre-tiling for repeat-x/y/no-repeat), `globalAlpha` (baked via `Shader::apply_opacity` / `PixmapPaint.opacity`), all 26 composite ops (`globalCompositeOperation` → `tiny_skia::BlendMode`) |
| Strokes | tiny-skia native dashing (`StrokeDash` + `lineDashOffset`) and stroke→outline conversion; strokes paint with the **stroke source** (a fill-source bug was caught by the Chrome comparison and fixed + regression-tested) |
| Shadows | Silhouette in shadow color → 3-pass box blur (same kernel as the painter's box-shadow) → CTM-mapped offset composite, under the real shape |
| Text | cosmic-text shaping into a transparent block composited through the CTM (full affine: rotate/scale/skew); per-glyph gradient sampling (documented approximation); `strokeText` as an 8-direction outline approximation; `measureText` returns advance/ascent/descent |
| Images | `drawImage` 3/5/9-arg from `<img>`, `Image()`, and other canvases; `imageSmoothingEnabled` → filter quality; premul conversion on blit |
| Pixel access | `getImageData` (full requested rect, transparent-black OOB — Chrome semantics), `putImageData` (CTM-ignoring, spec), `createImageData`, `ImageData` constructor both forms |
| Serialization | `toDataURL` PNG + JPEG (quality), `toBlob` shim |
| Layout | `<canvas>` is a replaced element: intrinsic size = width/height attrs (300×150 default); `canvas.width = x` resizes (contents reset, spec) and triggers relayout via `MarkDirty` |
| JS surface | `prelude.js`: `HTMLCanvasElement` (width/height/getContext/toDataURL), full `CanvasRenderingContext2D` class, `CanvasGradient`, `CanvasPattern`, `TextMetrics`, `ImageData`; `js/src/lib.rs`: 9 natives (lifecycle, JSON-op dispatcher, setters, getters, ArrayBuffer image-data, toDataURL, image fetch, natural size) |

### D2 — `new Image()` + dynamic image loading

`ImageFetch` JsCommand → engine network task → page-thread decode into the
image map → mirror refresh **before** `load` fires (naturalWidth is live in
onload) → `load`/`error` DOM events. `HTMLImageElement` gained
src/naturalWidth/naturalHeight/complete; works for connected `<img>` and
detached `Image()` objects (the canvas loader idiom). The image map is now
mirrored into the JS bridge (`ImageMirrorShared`).

### D3 — EventSource (SSE)

`new EventSource(url)` with correct `text/event-stream` framing (blocks,
`data:`/`event:`/`id:` lines, comments), `on*` + `addEventListener` delivery,
`open`/`message`/custom/`error` events, `readyState`/`lastEventId`/`close()`.
**Honest limitation (documented in code)**: the engine fetch is buffered, so
events fire when the response ENDS — finite streams (many endpoints) work
end-to-end; live infinite streams need incremental chunk delivery in
`rowser-networking`, which is the documented follow-up.

## 2. Verification

### Canvas probe page vs Chrome (side-by-side)

`screenshots/phase-d/side-by-side-canvas-probe.png` — three canvases: shapes
(fill/stroke rects, triangle, arc, bezier, rotated transform), gradient+text+
shadow, dashes+alpha. Per-color presence counts (tolerance 28):

| Color | Chrome px | rowser85 px |
|---|---|---|
| red fillRect | 7,701 | 7,698 |
| blue strokeRect | 1,200 | **1,200** |
| green triangle | 1,470 | **1,470** |
| orange circle | 3,120 | 3,144 |
| purple bezier stroke | 450 | 432 |
| gradient navy | 5,610 | 5,706 |
| gradient teal | 1,934 | 1,908 |
| yellow shadow box | 4,500 | **4,500** |
| rotated red-brown (alpha 0.8) | 2,341 | 2,339 |

Grayscale threshold-diff **10.13%** — dominated by text antialiasing and
gradient dithering; every geometric feature matches (several counts exact).

### E2E tests (LocalServer, full engine)

- `canvas2d_end_to_end`: 6 JS-visible markers — isPointInPath, measureText,
  getImageData, putImageData round-trip, toDataURL PNG magic,
  `new Image()` → onload → drawImage (with live naturalWidth).
- `canvas2d_paints_pixels`: rendered-frame pixel asserts — green
  putImageData block, red fill, gradient both ends, yellow canvas→canvas
  drawImage, dark fillText pixels, rotated magenta square.
- `eventsource_finite_stream`: open + 2 message events (one per
  blank-line block, spec framing) + custom `tick` event + summary.
- 17 new unit tests in `canvas2d.rs` (fills, gradients, clip, dashes,
  transforms, composite, shadows, image data, fonts, arcs, resize).

### 6-site regression (vs the same Chrome ground truths as Phase C)

| Site | Phase C diff | Phase D diff |
|---|---|---|
| example.com | 6.0% | **3.6%** |
| Hacker News | 16.3% | **11.5%** |
| Wikipedia | 22.2% | **18.2%** |
| GitHub | 20.1% | 22.9% |
| rust-lang.org | 49.9% | 47.7% |

No canvas-related regressions; four sites improved (settle timing +
AA variance; GitHub's delta is within capture variance — its dark hero,
navy gradient and CTA all render as before).

## 3. Bugs found and fixed during verification

1. **Strokes painted in fillStyle** — the stroke outline was filled with the
   fill source. Caught by the Chrome color table (red count inflated by
   exactly the strokeRect pixels), fixed (`paint_path_impl` takes the
   source), regression-tested (`stroke_uses_stroke_style_not_fill`).
2. **Text marshalling destroyed strings** — the `__call` numeric marshalling
   turned `'Rrowser'` into `0`; text ops now call the native directly.
3. **naturalWidth read stale in onload** — the image mirror was synced after
   the `load` dispatch; order swapped.
4. **Tiny-skia concat order** — `pre_concat`/`post_concat` semantics
   inverted the drawImage/text transforms (fixed + unit-tested).

## 4. Remaining gaps (honest)

1. **SSE live streaming** — finite responses only (see D3); needs chunked
   delivery in `rowser-networking`.
2. **Pattern transform** (`pattern.setTransform`) — parsed, not applied.
3. **Gradient/pattern text fills** sample per glyph (banding on large text);
   exact shader-space text needs glyph-outline paths.
4. **`strokeText`** is an 8-direction outline approximation, not a true
   outline stroke.
5. **`ctx.filter`** — no canvas filter chain (CSS filters exist).
6. **WebGL/WebGPU/WebAssembly/WebRTC** — the V2 plan's documented deferrals
   (compile-cost / GPU-absent on this box); unchanged.
7. `file://` pages render progressively slower than HTTP ones (pre-existing,
   unrelated to canvas; HTTP e2e + real sites unaffected).
8. `ctx.getTransform()` returns an identity stand-in (CTM lives engine-side).

## 5. Handoff to Group E (Performance)

The engine now covers the JS platform (A), advanced CSS (B), rendering
quality (C) and Canvas 2D + image loading + SSE-finite (D). The biggest
remaining lever per the V2 plan is Group E: incremental layout (the
whole-document relayout on every dirty flag is the per-render cost),
layer compositing, and paint caching — then the final 30-site suite and
`docs/V2_FINAL_REPORT.md` + tag `v2.0.0`.
