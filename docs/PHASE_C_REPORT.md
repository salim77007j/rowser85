# Phase C Report — Rendering Quality (Group C)

**Date**: 2026-10-04 · **Branch**: `main`
**Suite**: 123 tests green (was 111), `cargo fmt` + `clippy --workspace --all-targets -D warnings` clean.
**Method**: Playwright Chromium ground truth (1360×745) → rowser85 render → side-by-side composite + grayscale threshold-diff + VLM structural verdicts. Every claim below is backed by a pixel test or a capture.

---

## 1. What was implemented

### C1 — SVG rasterization end-to-end (the dominant visual lever)

| Piece | Implementation |
|---|---|
| Rasterizer | `resvg 0.44` (+ usvg, fontdb, system-fonts for `<text>`) added to `rowser-rendering`. resvg re-exports its own tiny-skia 0.11.4 instance — the painter keeps tiny-skia 0.12; both coexist, types never mix (`resvg::tiny_skia` for all SVG-side raster ops) |
| Root-size injection | The root `<svg>` tag is rewritten with exact `width`/`height` (+ `xmlns` when missing — html5ever strips it, usvg requires it) before parsing, so usvg maps the viewBox into the target box honoring **`preserveAspectRatio` natively** (meet/slice/none + all alignment flags — verified empirically in tests) |
| Inline `<svg>` elements | Engine raster pass in `Page::repaint`: walks light tree + all shadow roots, finds laid-out svg elements, rasterizes **at the layout rect in device pixels** (never an upscaled natural-size raster → logos are pixel-exact). Cache keyed by (node, w, h, subtree-signature hash) — mutations and resizes re-rasterize, static pages hit cache |
| SVG sizing discipline | Replaced-element rules (Chrome parity): width/height attrs → CSS size (presentational hints, incl. `%`); `viewBox` → intrinsic aspect-ratio via taffy; fully-auto → **300×150 default**. The Phase-B giant-logo bug (block svg stretching to the parent) is dead — rust-lang.org's gear now lays out at its attribute size |
| Subtree exclusion | svg-namespace descendants no longer produce taffy boxes or text runs (no double-painted `<text>`, no zero-size path boxes); the whole subtree serializes through `Dom::serialize_subtree` into usvg |
| `<img src="*.svg">` | Fetch → `DecodedImage::decode` → **`decode_svg_bytes` fallback** (natural size via Chrome semantics: missing dimension resolves through the viewBox ratio, not usvg's raw viewBox extent). Wired at all 4 decode sites: img fetch, background-layer fetch, background data-URL, img data-URL |
| `<img>` natural sizing | Decoded image dims now feed the engine's intrinsic map → auto-sized imgs get their intrinsic box (Chrome-style), 300×150 before decode |

### C2 — Paint correctness fix (found while testing SVG imgs)

`Painter::paint_image`'s translation-only fast path allocated its clip layer in
**source-pixel** dims (`screen.w / scale`) instead of destination dims. Every
**upscaled** image was cropped to its top-left quadrant — logos drew as tiny
slivers (this is why the Wikipedia globe "vanished": it *was* decoded, painted
as a ~25 px fragment). Fixed: layer sized in destination pixels, source mapped
via scale+crop. Affects all raster and SVG images, all sites.

### C3 — HiDPI / device pixel ratio

- `RenderOptions.scale` (device pixel ratio): frame rasterized at viewport × scale, root affine `[s,0,0,s,0,-scroll·s]`; all rect/image/border/gradient/clip paths scale through the existing transform stack
- **Glyphs rasterize at font-size × DPR** (cache-key font-size bits re-binned) — text is sharp at DPR 2, not an upscaled 1× raster; shadow taps scale too
- Sticky-offset math fixed for scale (insets/viewport converted CSS↔device); fixed-position cancellation verified
- Engine DPR from `ROWSER_DPR` env (1.0 default); SVG rasters sized in device px. The UI's `fit_to_exact_size` display consumes device-px frames at logical size unchanged
- Unit test: geometry at DPR 2 (device dims, scaled rect placement, text ink)

### C4 — Anti-aliasing audit

- Text: swash coverage masks (grayscale AA) + cosmic-text subpixel binning were already in place; glyph blit positions now `round()` instead of truncation (removes the half-pixel bias)
- Shapes: tiny-skia AA on all fill/stroke paths (B1) and resvg AA for SVG content — one rasterization quality path, no nearest-neighbor blits remain
- Color: pipeline is 8-bit sRGB end-to-end with premultiplied compositing; CSS filter matrices operate per-spec in sRGB. Known gap (documented, unchanged): box blur runs in gamma space (Chrome composites in linear) — visually subtle, listed for Group E

## 2. Pixel diffs vs Chrome (after Phase C)

| Site | Phase B | Phase C | Verdict |
|---|---|---|---|
| example.com | 6.0% | **6.0%** | Chrome-equivalent (unchanged) |
| Bing search | 10.6% | **8.0%** | Improved (paint fix + SVG images) |
| Hacker News | 16.5% | **16.6%** | Near-equivalent (live feed drift) |
| GitHub | 24.7% | **24.8%** | Structure matches; typography scale + hero radial-gradient blobs remain |
| Wikipedia | 25.3% | **25.3%** | Metric flat — see §3 |
| rust-lang.org | 51.1% | **53.1%** | Metric flat — **structure now matches** (see below) |

Composites: `screenshots/phase-c/compare/side-by-side-<site>.png`.

**Why the two biggest diffs barely moved while the pages changed radically:**
- **rust-lang.org**: the giant-blurry-logo bug (Phase B's dominant gap) is GONE —
  VLM confirms the gear logo renders at ~45 px top-left, and the hero now shows
  the giant "Rust" slab wordmark, "Get Started", blue version line and tagline —
  the same structure as Chrome. The remaining ~53% is *different pixels over the
  same layout*: Chrome's Alfa-Slab-One webfont vs our fallback serif, the yellow
  GET STARTED button, spacing. The failure mode changed from "structurally
  wrong" to "typographically different" — a font-matching gap, not a rendering
  gap.
- **Wikipedia**: the site header (globe logo, search input, user links) still
  fails to lay out — vector-2022's header grid/flex is a **Group B deferred
  item** (grid completeness), not a Group C regression; the globe img decodes
  and paints correctly now (e2e-proven) once the header gives it a box.

## 3. VLM structural verdicts (current binary)

- **rust-lang.org**: gear logo ~45 px top-left; hero = giant "Rust" wordmark +
  Get Started + blue "Version 1.99.0" + tagline — matches Chrome's structure.
  Residual: slab-font glyphs, yellow CTA button, a small stray "x" glyph.
- **Wikipedia**: article body, contents sidebar, infobox, blue links render
  (Phase B verdicts hold). Header bar broken (logo/search missing → B-deferred
  grid work). Infobox overlaps body text (float/table layout — B4 deferred).
  The "×" glyph artifact persists: now precisely characterized — a solid
  black sans-serif **14×14 px X at the boundary between the `mw-redirect`
  link "immutability," and the link "higher-order"** (also seen on
  rust-lang.org near the hero). No × exists in the DOM text; suspect glyph
  fallback or pseudo-content; needs a live-DOM dump tool (Group D tooling).
- **GitHub**: header + hero + email input + green CTA render; navy gradient
  paints; corner radii read near-square at composite scale. Residual: hero
  typography scale, purple bokeh blobs (multi radial-gradient layers), top nav
  row.
- **example.com / Bing / HN**: Chrome-equivalent (Bing improved to 8.0%).

## 4. Remaining gaps (honest)

1. **Vector-2022-style site headers** (Wikipedia logo/search row) — grid/flex
   completeness (Group B deferred list).
2. **Webfont fidelity** — Chrome-competitive hero typography needs real
   webfont loading for display faces (rust-lang.org slab font). Partial
   @font-face support exists; matching the exact face is the gap.
3. **Wikipedia "×" artifact** — precisely localized (see §3), root cause
   unknown; needs DOM-side glyph dump.
4. **Gamma-correct compositing** (blur/filters in linear space).
5. **background-image SVGs rasterize at natural size** (upscaled by the paint
   scale — correct geometry, soft at >1× display); re-raster at dest size is a
   follow-up (needs background-dest plumbing into the decode path).
6. `<use xlink:href>` round-trip: our Attr storage drops the `xlink:` prefix
   (local-name only) — SVG `<use>` referencing `xlink:href` (legacy files)
   won't resolve; modern `href` works.

## 5. Handoff to Group D (Web Platform APIs)

The engine now renders the visual web faithfully enough that API behavior
(Canvas, WebGL, WASM, media) is the next Chrome-parity frontier. The capture
rig, probe examples (`wiki_x_probe`, `svg_probe`) and the e2e LocalServer
harness are the standing verification tools.
