# Phase B Report — Advanced CSS (Group B)

**Date**: 2026-10-04 · **Branch**: `main` · **Commits**: `2fca627` (B1 paint stack), `ce5c3fd` (B3 pseudo/states/calc/animations)
**Suite**: 111 tests green (was 99), `cargo fmt` + `clippy --workspace --all-targets -D warnings` clean.
**Method**: Playwright Chromium ground truth (1360×745) → rowser85 render → side-by-side composite + grayscale threshold-diff + VLM structural verdicts. Static probes verify feature firing on live site CSS (github.com: 4508 rules, 30 @keyframes, 173 pseudo-rules parsed).

> Environment reset AGAIN at phase start (3rd time: repo + toolchain wiped).
> Re-provisioned rustup 1.99, re-cloned at Phase A HEAD, re-extracted system
> libs (fixed a broken libasound.so symlink), rebuilt release.

---

## 1. What was implemented (by sub-phase)

### B1 — Paint richness (commit 2fca627)
| Feature | Implementation |
|---|---|
| `border-radius` | 4 corners, px+percent; rounded fill, ring borders (uniform-color exact, mixed-color approximates corners), rounded overflow clips |
| linear/radial gradients | CSS angle convention, keyword directions, corner geometry, auto stop distribution, farthest-corner radial |
| `background-image` layers | gradients + url() with cover/contain/px sizing, position fractions, repeat; engine fetches + decodes per (node, layer), data: URLs synchronous |
| `box-shadow` | outset (below background) + inset (above); spread grows radius; **manual 3-pass separable box blur** (tiny-skia 0.12 has no mask-filter blur) |
| `text-shadow` | offset shadows + 5-tap blur approximation |
| 2D transforms | translate(px/pct)/rotate/scale/skew/matrix + transform-origin as affine groups; per-rect fast path when translation-only |
| group opacity | offscreen layer composite (fractional, was binary before) |
| CSS filters | blur (box blur), brightness/contrast/saturate/grayscale/sepia/hue-rotate/opacity (matrix pipeline) |
| `position: fixed` | layout absolute + paint-time scroll cancellation (stays viewport-anchored) |
| `position: sticky` | paint-time clamped offset vs scrollport + containing block (top/bottom insets) |
| `overflow: scroll/auto` | interactive element scrolling: wheel routing to innermost scrollable, scroll chaining, native scrollbar painting, per-element scroll offsets |
| New DrawCmd set | Gradient / BgImage / BoxShadow / PushOpacity / PushTransform / PushFilter / PushFixed / PushSticky; clips carry radius + scroll |
| API surface | `PaintInputs` struct (replaces 6-arg builder); `Browser::wheel`, `Command::Wheel`, `EngineEvent::ScrollChanged`; UI wheel routing rewired |

### B3 — Dynamic CSS (commit ce5c3fd)
| Feature | Implementation |
|---|---|
| `::before` / `::after` | full pipeline: selector splitting (comma lists split correctly), ForStatelessPseudoElement cascade, `content` mining (strings w/ escapes, `attr()`, none). **Two forms**: inline content joins the element's text as styled spans (icon prefixes); block-display pseudos get real taffy boxes (synthetic ids → layout rects → paint) |
| state pseudo-classes | `:hover/:active/:focus/:focus-visible/:focus-within/:visited/:link/:disabled/:enabled/:checked` — Servo-selector Parser wired to `Dom::interaction_state`; engine maintains the hover chain on hit-test (recompute only on change) and focus on click |
| `calc()` | full lightningcss Calc tree: `calc(P% + Xpx)` → `Length::Calc` resolved by a **two-pass layout** against actual containing blocks; `min()/max()/clamp()` exact when absolute; **vw/vh/vmin/vmax now resolve at compute time (previously dropped to 0!)** |
| `transition` | opacity transitions with eased interpolation, started on computed-style diffs |
| `@keyframes` + `animation` | rules collected; engine clock paces the page loop at 16 ms while animations run; iteration/delay/direction/fill; interpolation (opacity lerp, transform op-wise, structural mismatch snaps); **paint-side application — no relayout** (transform/opacity/filter subset) |

### B2 — merged into B1 (fixed/sticky/overflow scroll landed with the paint stack)

## 2. Pixel diffs vs Chrome (after Phase B)

| Site | Phase A | Phase B | Note |
|---|---|---|---|
| example.com | 6.0% | **6.0%** | Chrome-equivalent (unchanged) |
| Bing search | 9.7% | **10.6%** | Chrome-equivalent (live result variance) |
| Hacker News | 16.3% | **16.5%** | Near-equivalent (live feed drift) |
| GitHub | 20.1% | **24.7%** | See §3 — gradient + radius now render; delta driven by hero blobs (radial-gradient layers) + typography |
| Wikipedia | 22.2% | **25.3%** | Structure correct (VLM); one glyph artifact ("×" overlap) + content variance |
| rust-lang.org | 49.9% | **51.1%** | Unchanged driver: SVG logo sizing (Group C) + font weight |

Composites: `screenshots/phase-b/compare/side-by-side-<site>.png`.

**Capture-correctness note**: the first Phase B capture pass accidentally used
the stale pre-B release binary (rebuilt 00:35, before B1). All numbers above
are from the re-captured session with the current binary; VLM verified the
new features visible (GitHub hero gradient renders, structure matches).

## 3. VLM structural verdicts (per site, current binary)

- **GitHub**: header + hero + email input + green CTA all render; **background
  gradient now paints** (deep navy → black). Remaining gaps: purple/bokeh
  hero blobs (multi-layer radial gradients, partially rendered), header logo
  (SVG → Group C), hero typography scale/weight. 6 px corner radii exist but
  read as near-square at composite scale.
- **Wikipedia**: layout correct (search bar, contents sidebar, article,
  infobox); sharp text, blue links. One black "×" glyph artifact overlapping
  body text (investigate in B4); bottom truncation is viewport-bound.
- **rust-lang.org**: content-level; the giant-SVG-logo bug is the dominant
  gap (Group C).
- **example / Bing / HN**: unchanged from Phase A verdicts (equivalent).

## 4. Remaining gaps in Group B (honest)

1. **Vertical-align** (baseline alignment of inline content) — not implemented.
2. **border-collapse** (shared table borders) — tables still use separated
   borders via the table→grid mapping.
3. **CSS multi-columns** — not implemented.
4. **writing-mode** — not implemented (LTR horizontal only).
5. **backdrop-filter** — parsed but not painted (needs backdrop sampling).
6. **Animation subset**: only transform/opacity/filter animate (paint-side);
   layout-property animations and color transitions need the full recompute
   path. `:visited` state is per-DOM (no profile-global history).
7. **Wikipedia "×" artifact** — a text/pseudo glyph painting overlap to
   investigate next phase.
8. **Corner-case fidelity**: mixed-color rounded borders blend corners;
   text under rotation stays upright (billboard); radial gradients are
   circular (ellipse via transform not yet).

## 5. Handoff to Group C (Rendering Quality)

The top visual levers now are SVG sizing/layout (rust-lang.org logo,
example.com icon, Wikipedia logo), anti-aliasing quality, HiDPI, and color
management — all Group C scope. Group E (incremental layout) remains the
biggest lever for per-render cost (github full render: ~5–7 s on this box).
