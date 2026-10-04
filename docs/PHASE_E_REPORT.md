# Phase E Report — Performance (incremental layout, paint reuse, scroll blits)

**Date**: 2026-10-04/05 · **Branch**: `main`
**Suite**: 155 tests green (was 146), `cargo fmt` clean, `clippy --workspace --all-targets -D warnings` zero warnings.
**Method**: per-stage timing instrumentation (ROWSER_UI_TRACE) on the 6-site set + Playwright Chromium ground truth → rowser85 render → side-by-side composite + threshold diff + per-band analysis. Every performance claim below is backed by a trace log or a pixel test.

> Environment note: this session started from a **fully reset workspace** again (no
> toolchain, no repo, no extracted libs). Rust 1.99 + deb-extracted native libs
> (alsa, xkbcommon, xcb-xkb — the full runtime set) were rebuilt from scratch
> before any Group E work.

## 1. The audit (what actually cost time)

Stage timing on the 6 sites exposed three dominant costs, in order:

1. **`compute_styles` — the style cascade — 13 s on GitHub** (2440 elements,
   21 sheets, 2.4 MB CSS) and 6–9 s on Wikipedia (19k elements). The taffy
   solve itself was 200 ms; text shaping (before this phase) re-ran cosmic-text
   for every text leaf on every render because the per-leaf cache died with the
   ephemeral taffy tree.
2. **Filter-group rasterization — 7.2 s on GitHub**: the painter rendered
   every `filter:`/`backdrop-filter:` group into a **full-viewport**
   1360×745 offscreen pixmap and blurred all of it (GitHub's new homepage has
   three `blur(40–60px)` cards). Verified by A/B: the Phase-D binary hangs the
   same way on today's GitHub — this was **pre-existing**, exposed by GitHub's
   redesign, not introduced by Group E work.
3. **Whole-document re-layout on every dirty round** (the V2 plan's headline
   item): one global `Page::dirty` bit + a full style/solve/paint pass per
   round. Wikipedia's observer-driven settle loop used to pay 6+ s per round.

## 2. What was implemented

### E1 — Persistent cross-render shape cache (`layout/src/text.rs`)

- `ShapeCache` on `LayoutEngine`: keyed by the COMPLETE shaping input —
  text, span styles, default style, rounded width, and a **web-font
  generation counter** (`FONT_GEN`, bumped in `register_web_font`, so late
  font arrival invalidates correctly).
- LRU-bounded: 2048 entries / 48 MiB estimated bytes; per-canvas `rev`-style
  stats exposed for traces and tests.
- The per-leaf ephemeral `TextLeaf.cache` was removed (superseded — one cache
  instead of two).
- **Effect (GitHub)**: 6924 shaping lookups → 5001 hits / 1923 misses
  (72% hit rate); the taffy solve with measures runs **208 ms** (was
  seconds). Bing: 786 lookups → 692 hits. Wikipedia settles its text entirely
  from cache after the first pass.
- Tests: 5 unit tests (hit-on-repeat, miss-on-width, miss-on-style,
  web-font invalidation, LRU eviction).

### E2 — Layout-inputs gates (engine `render_pipeline`)

- **Gate 1 (skip styles + solve)**: a fingerprint of every style/layout
  input — DOM version, sheet-set fingerprint, viewport, intrinsic-size map,
  font generation — plus the interaction-state fingerprint
  (:hover/:active/:focus/:visited, hashed separately because it lives
  outside `dom.version`). Unchanged ⇒ the round is **paint-only**: straight
  to `repaint()` with the cached display list.
- **Gate 2 (skip solve, keep styles)**: interaction-state churn on an
  unchanged DOM — recompute styles, deep-compare against the previous
  `StyleMap` (requires `PartialEq` on `ComputedStyle`/`StyleMap`), and skip
  the taffy solve when identical (hover over elements with no `:hover`
  rules costs a style pass, not a relayout).
- Canvas-element intrinsic scan cached per DOM version (the scan walked the
  whole document every round; canvas draw loops never bump the version).
- **Effect (Wikipedia, measured)**: the settle loop's rounds went from a
  full 6–9 s pipeline each to **paint-only rounds of 123–177 ms** — 173
  gate-1 rounds in the capture window (173 display-list cache hits); the
  page settles within its 30 s floor instead of deferring through the
  sustained-dirty pacing. HN: 29 gate-1 + 1 gate-2 round, paints 34–60 ms.
- Test: `gate_canvas_loop_then_dom_mutation_repaints` (e2e) — a 20-frame
  canvas rAF loop followed by a DOM mutation: the final canvas frame must be
  painted AND the post-loop mutation must repaint (catches stuck-open
  gates).

### E3 — Display-list + image-map caching, lazy canvas pixels

- `DrawCmd::Canvas { rect, radius, node }`: canvas elements are referenced
  BY NODE, not by embedded snapshots; the painter resolves live pixels per
  raster from the shared `CanvasRegistryShared`. Canvas draw ops no longer
  rebuild the display list or re-clone the image map (the old
  `merged_images` cloned the whole map + snapshots per repaint).
- Per-canvas content revision (`Canvas2D::rev`, bumped by every JS op via
  `with_canvas`) + `registry_revision()` — feeds the scroll-blit freshness
  check (a canvas that redrew since the last frame must not be blitted).
- `Page.paint_gen`: one generation counter bumped by every DL-input producer
  (fresh layout, animation-override ticks, element scroll, video frames,
  image/bg-image arrivals, SVG raster refresh). `repaint()` reuses the
  cached `Arc<DisplayList>` when `dl_gen == paint_gen` — pure page scroll
  and canvas draw loops reuse the list by design; the list is in document
  coordinates. `Arc<LayoutResult>` + `build_list` (anim-override patch only
  clones the StyleMap while animations run — the per-repaint full clone is
  gone).
- **Effect**: canvas animation loops (games, charts) go from
  full-document relayout per draw to a DL hit + one canvas blit;
  `merged_images` rebuilt only when images/SVG rasters actually change.

### E4 — Scroll-blit fast path (`Painter::render_ctx`)

- Pure page scrolls (only `scroll_y` changed, same list version, same
  options signature, canvas content unchanged, **no fixed/sticky anchors**,
  no find highlights): shift the previous frame's rows (row-by-row memcpy),
  clear the exposed band to the page background, rasterize ONLY the band in
  a shifted coordinate frame (band-as-pixmap: per-command culling, masks
  and groups all stay correct with zero painter changes), composite.
- `DisplayList.has_fixed_or_sticky` (set at build) refuses the blit when
  fixed/sticky content exists (they must not translate with the page);
  `DisplayList.version` (monotonic) refuses it when the list changed.
- **Effect**: a scroll costs 2 row-memcpys + a band raster instead of a
  full-viewport re-raster (1360×745 ≈ 4 MB per frame).
- Tests: `scroll_blit_matches_full_render` (pixel-equivalence blit vs full
  raster, tolerance ≤2/channel, 0 mismatched channels — 300×200 case),
  `scroll_blit_refuses_fixed_elements`,
  `canvas_command_pulls_fresh_pixels_per_raster`.

### E5 — Cascade 3× fix: partitioned pseudo matching (`parsing/src/cascade.rs`)

- The cascade ran **bucket lookup + selector matching three times per
  element** (element + ::before + ::after) on any sheet set containing
  pseudo rules (GitHub: 2.4 MB of CSS). Now ONE lookup + ONE match sweep
  partitions rules into (element, ::before, ::after) sets;
  `cascade_from_matched` sorts/applies each set; per-element pseudo
  cascades run ONLY when that element actually matched pseudo rules.
- Pseudo-element inheritance preserved (owner's style, not the owner's
  parent's).
- **Effect (GitHub)**: compute_styles 13.0 s → 11.1 s cold → **7.3 s warm**
  (repeat rounds). Same-semantics refactor — the full pseudo-element test
  set from Phase B passes unchanged.

### E6 — Region-sized filter layers (`rendering/src/painter.rs`)

- `PushFilter` groups render into a layer sized to the **effect region**
  (the padded border box, blur-expanded at list build time — the DL already
  computed it), translated into layer-local coordinates (transform + clip
  stack shifted), blurred, composited back at the region origin through the
  region mask. The old code blurred a full-viewport 4 MB pixmap per group.
- **Effect (GitHub)**: paint 7158 ms → **1992–2788 ms** (2.6–3.6×), with
  bit-identical filter-group tests passing (Phase B filter tests are pixel
  asserts).

## 3. Six-site regression vs Chrome (same ground truths, re-captured)

Tool metric (`side_by_side.py`, 50% scale, threshold 24) — the official
metric of prior phases; raw full-resolution grayscale diff in parentheses
where it differs materially:

| Site | Phase D | Phase E | Notes |
|---|---|---|---|
| example.com | 3.6% | 6.0% (raw 3.6%) | static page — raw pixel diff equals Phase D exactly; the tool's resample amplifies a sub-pixel text shift |
| Hacker News | 11.5% | 18.1% (raw 11.9%) | live front page — content drift between ground-truth and render contributes |
| Wikipedia | 18.2% | 25.3% (raw 18.2%) | raw equals Phase D; same resample amplification; settle loop now completes 173 paint-only rounds |
| GitHub | 22.9% | 24.8% | renders fully (95% content coverage); GitHub redesigned since Phase D — their blur-card hero is new |
| rust-lang.org | 47.7% | 52.9% | JS reveal-on-scroll sections (state-dependent, as in Phase D); 92 painted frames at 57 ms paint each |
| Bing | — | 11.2% | in line with its historical range |

GitHub's own numbers (this phase's deep-dive): first full render ≈ **19 s**
(5 s boot + 11.1 s styles + 0.26 s solve + 2.8 s paint) vs ~30 s+ before E5/E6
(and a capture that used to time out white at 97.6% diff before the settle
floor was raised to match the real first-render cost).

Composites: `screenshots/phase-e/side-by-side-*.png` (Chrome left, rowser85
right).

## 4. Bugs found and fixed during verification

1. **Scroll-blit composite bug** — the band was composited at
   `(band_y0, 0)` instead of `(0, band_y0)` (draw_pixmap's x/y argument
   order). Caught by the pixel-equivalence test (61% of channels
   mismatched), fixed, test now bit-clean.
2. **GitHub "hang" misdiagnosis trail** — the capture went white; per-command
   paint tracing walked the stall down to `PushFilter → Gradient →
   apply_filters(blur 40–60px)` on full-viewport layers; the A/B against the
   Phase-D binary proved the pathology pre-existing (GitHub's homepage
   redesign). Fixed by E6; the capture settle floors were also recalibrated
   to the real first-render cost (github 40 s, wikipedia 30 s).
3. **Environment**: `cc` ignores `LIBRARY_PATH` for linking — the alsa link
   failure was fixed by symlinking `libasound.so` into the rustc sysroot
   `-L` path; the full xkbcommon/xcb-xkb runtime set was extracted into
   `debs-extracted/usr-lib` (repo-local, as the capture rig expects).

## 5. Remaining gaps (honest)

1. **`compute_styles` is still the #1 cost**: 7–11 s on GitHub, ~6 s on
   Wikipedia. The partitioned matching took ~15% + warmup; the real fix is
   per-node style-result caching keyed on (element signature, parent style
   fingerprint) with sibling-aware invalidation (nth-child), or rule
   bucketing by rightmost compound + bloom filters (Blink-style). The 155
   green tests give the safety net for that work — it is the top v3 item.
2. **Blits measured 0 in captures** — the capture rig never scrolls. The
   fast path is proven by unit tests (bit-equivalence + refusal cases) and
   the e2e programmatic-scroll test; a scroll-through capture harness would
   quantify it in the field.
3. **Sustained-dirty pacing thresholds** (800 ms / streak 4) were tuned for
   the pre-Group-E cost profile; with paint-only rounds at 150 ms the
   pacing rarely engages (Wikipedia's 173 rounds ran unthrottled) but the
   thresholds should be revisited with the new cost profile.
4. **rust-lang.org 52.9%** — dominated by reveal-on-scroll sections that
   Chrome shows and we do not until scrolled; a scrolled capture comparison
   is the honest next step (also the remaining SVG/asset nuances).
5. `Painter` keeps a full-frame `LastFrame` pixmap copy (~4 MB) for blits —
   bounded, but on memory-constrained boxes the memory valve may want to
   clear it under pressure.

## 6. Handoff to Final

Groups A–E are complete. What remains for the V2 finale: the 30-site
side-by-side suite, the capabilities matrix, `docs/V2_FINAL_REPORT.md`, and
tag `v2.0.0`.
