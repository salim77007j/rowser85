# Rrowser85 — UI/UX Engineering Worklog

Multi-session log of the UI layer build (engine was completed by a prior agent).

---

## Session 2 (2026-10-01/02) — validation, perf hardening, benchmarking

**Task:** Build, validate, and benchmark the Rrowser85 UI on top of the completed
engine; run the 7-step mission (clone/understand → design → build UI → CI
artifacts → run+test → Chrome comparison → push /ui /integration /assets
/screenshots /validation).

### Recovered state
- Prior session had already built: UI (egui/eframe, 7 source files), integration
  shell (7 files), CI `browser` workflow (Linux+Windows artifacts) — commit fd4df16.
- Fixed `cargo fmt` + xdriver clippy lapses blocking the `ci` workflow (04acb2e).

### Verification of the release artifact
- CI release artifact RUNS under Xvfb (smoke test + VLM check: full chrome, NTP,
  speed dial).
- Audited prior screenshots: found byte-identical captures for different sites
  (github/HN/rust-lang) — the validation rig was silently failing.

### Rig hardening (tools/xdriver + ci/ui-validate.sh)
- Root cause of bad captures: zero-gap XTEST press/release pairs coalesce in the
  X server → lost releases → phantom Escape autorepeat floods (30 presses per
  tap) surrendered omnibox focus; also flaky 2-in-8 click registration.
  Fix: 25ms mandatory event separation in fake_key + button_click.
- Built hardened validation battery: per-stage capture verification (frame must
  CHANGE vs previous stage), whole-interaction retries, RSS sampling, phases
  (sites|features|restore|big|small), self-contained resolution-aware Xvfb
  lifecycle, kills stray debug/artifact browser instances.

### Real bugs found by driving the real app (all fixed)
1. **engine**: `render_pipeline` left `dirty=true` → every tab re-rendered every
   250ms forever → FrameReady storm (125+ events/frame, exponential batching)
   wedged the UI thread at 100% CPU. Fix: dirty=false after render.
2. **shell**: coalesce FrameReady to newest-per-tab in poll_events.
3. **engine**: Chrome-style background tab timer throttle (>5s backgrounded →
   ≤1 delivery/sec); suspend_after 300s→60s. Idle CPU with 5 sites: 110% → 10%.
4. **js**: bounded pump_jobs (50ms/10k jobs) + idle resume (self-perpetuating
   microtask chains wedged the page thread).
5. **dom**: hard 100k node cap with overflow sentinel (unbounded appendChild →
   millions of nodes within the watchdog window).
6. **parsing**: selector-bucket infinite loop on `\`-escaped identifiers
   (scan_ident doesn't consume backslash; pos never advanced) — THE github.com
   wedge: page thread 100% CPU, zero allocation, mid-load. Reproduced offline
   (github.html + 24 real sheets/2.4MB): cascade 2.3s after fix; CPU 110%→10%.
7. **ui**: print dialog + bookmark editor + tab context menu now close on
   Escape (they were blocking keyboard shortcuts).
8. **engine**: parsed-stylesheet cache keyed by (css set, media) fingerprint.

### Validation battery — 26/26 green on the release artifact
Stages 01–22 (see validation/VALIDATION.md §2): 10 real websites (example,
wikipedia, github, HN, rust-lang, mozilla, cern, …), omnibox search +
suggestions, bookmark star + manager, history, privacy dashboard, dark theme,
find-in-page, zoom, devtools, real network download, print→PDF, tab context
menu, multi-tab switch, session restore (SIGTERM → relaunch), 1080p + 768p.
Every stage attested by frame-change; failures auto-retry the whole interaction.

### Benchmark vs Chrome 153 (identical Xvfb/llvmpipe rig, same 5 sites)
| Metric | rowser | Chrome | Delta |
|---|---|---|---|
| RAM (tree VmRSS) | 293 MB | 1700 MB | 5.8× less |
| Processes | 1 | 13 | — |
| Cold start (fresh profile) | 127 ms | 10,410 ms | 82× faster |
| CPU idle, 5 tabs | 10.1% | 8.1% | parity |
| CPU scrolling | 15.6% | 2.2% | Chrome wins (software raster) |
| Shutdown | 5 ms | 84 ms | 17× faster |
| Footprint | 37 MB | 393 MB | 10× less |

### Final artifact verification (HEAD 8d24059)
- Downloaded the fresh artifact from Actions run 36932678103 (Linux 15.6 MB zip /
  37 MB binary; Windows 13.4 MB also attached).
- sha256 identical to the battery-validated binary (a45c66d2…) — commits after
  83f1a83 are docs-only, so validation transfers 1:1.
- Independent final smoke on the fresh download: 256 ms spawn, 149.6 MB after
  boot, 178.5 MB after example.com + wikipedia.org, frame attestation PASS,
  SIGTERM→exit 8 ms. Frames archived as screenshots/23-, 24-.

### Deliverables pushed
- `/ui` (egui/eframe UI: chrome.rs, pages.rs, theme.rs, icons.rs, app.rs, main.rs)
- `/integration` (UI↔engine shell, event coalescing)
- `/assets` (themes.json — palette export for tooling)
- `/screenshots` (24 PNGs + validation-report.txt)
- `/validation` (VALIDATION.md report, bench-*.txt, bench screenshots)
- `ci/` (ui-validate.sh battery, bench scripts), `tools/xdriver`

### Verdict (full report: validation/VALIDATION.md)
Functional, dramatically lighter (5.8× RAM), instant cold start, full real
feature set — competitive as a lightweight privacy-first browser; JS-heavy page
fidelity and software-rendering scroll CPU are the honest remaining gaps.

---

## Session 4 (2026-10-02): the media pipeline

**Directive:** "a standard browser any user can use — search Google, browse,
watch videos or live streams. Reconsider and decide; don't stop to ask."

Decision: implement the media subsystem in our own engine (byte-stream
pipeline + original ISOBMFF demuxer + pluggable codecs; full rationale in
docs/MEDIA.md). No engine embedding, no direction change.

Work log (all driven through the real app; sandbox was wiped mid-session —
toolchain and repo re-provisioned from GitHub first):
- Re-provisioned sandbox (rustup, repo clone), audited engine hook points.
- Built `rowser-media`: isobmff.rs (~1.3k lines, progressive + fMP4, lanes,
  bounds-checked), decode.rs (avcC→Annex-B, stride-aware YUV→RGBA),
  audio.rs (symphonia AAC, cpal sink, ring buffer), pipeline.rs (worker,
  wall clock, pending queues, backpressure). 7 fixture tests (committed
  ffmpeg-generated clips) — iterated through 11 live-driven defects (see
  VALIDATION.md §10) until 7/7 green.
- Wired the engine: Media streaming loader (ranged chunks + HLS reader),
  page-side media registry + MSE state, frame blit in the display list,
  mirror + JS bindings (HTMLMediaElement, MediaSource/SourceBuffer,
  createObjectURL, byte-exact fetch bodies, element wrapping by tag).
- E2E in the real browser: `scripts/media-e2e.sh` → M1 direct playback,
  M2 MSE playback, M3 navigation cleanup — PASS; Big Buck Bunny over
  public HTTPS plays; YouTube skeleton + clean script run (player blocked
  on WebComponents, documented).
- Battery 29/29 (3 new media stages added to ci/ui-validate.sh); clippy
  clean; fmt clean; benchmarks on baseline.
- Environment notes for future sessions: ALSA headers via
  `apt-get download libasound2-dev libasound2t64 + dpkg -x` + patched .pc
  (no sudo here); xkbcommon/xcb runtime libs likewise under
  /home/z/my-project/debs/extracted; `[profile.dev] debug=0` added after
  disk exhaustion (9.9 GB sandbox cannot hold debuginfo builds).

---

## Session 5 (2026-10-02): the WebComponents stack + the bidi crash

**Directive (standing):** "a standard browser any user can use — search
Google, browse, watch videos or live streams; massive compatibility +
high performance; don't copy others; don't stop to ask."

Sandbox was wiped again — re-provisioned (rustup, deb libs for
alsa/xkbcommon/xcb, repo clone) before any work.

### The crash that killed the browser first

Baseline probing found a page-thread panic in cosmic-text 0.17.2
(`assert_eq!(line_rtl, rtl)` in the shaper): Unicode bidi class-B
characters U+001C/001D/001E are *not* `char::is_whitespace()`, so they
survive CSS whitespace collapsing, reach unicode-bidi, split the layout
line into multiple bidi *paragraphs*, and mixed directions assert.
Google/YouTube pages contain them (international metadata). Fix:
`sanitize_bidi_separators` maps the survivors 1:1 to spaces (spec-
correct per CSS Text) + `catch_unwind` at the shaping boundary so no
text content can ever abort a tab again. Regression tests in
`layout/src/text.rs` (7/7 green).

### WebComponents (see docs/WEBCOMPONENTS.md for the full design)

* **Identity map** (NodeId→wrapper): `getElementById` returns the same
  object, element listeners actually fire — the prerequisite for
  everything else.
* **customElements v1**: define/get/whenDefined/upgrade +
  polyfillWrapFlushCallback; ctor/connected/disconnected/attributeChanged
  driven from the mutation natives; Rust-side `findCustomTags` scan so
  connect callbacks stay cheap on large insertions.
* **Shadow DOM as flat tree**: host↔root maps on the Dom,
  `flat_children` with slot assignment (named/default/fallback), cascade
  + layout + display list all walk the flat tree, shadow children
  inherit from the host, shadow `<style>` collected per render.
* **Templates**: contents persist past parse; `template.content`;
  cloneNode; serializer/import keep content through innerHTML
  round-trips.
* **innerHTML** get/set (parse + import + replace + connect callbacks).
* **DOM surface**: 40+ HTML element classes, Node family, TreeWalker,
  DOMImplementation.createHTMLDocument (settable), matches/closest,
  classList, Range/Selection, Intl (+supportedLocalesOf — a YouTube
  player hard dependency), MessageChannel/MessagePort/postMessage,
  document.currentScript (scripts carry node ids), writable
  document.readyState.
* **JS limits raised**: 96→384 MB heap, 2 MB stack (kevlar hydration
  OOM'd at 96).
* **xdriver uppercase fix**: capital letters typed bare lowercase keys
  (shared keycode with the lowercase twin) — every YouTube video ID the
  rig typed was silently lowercased. Rig bug, not engine.

### Validation

* Battery: **30/30 stages green** (sites 10, features 13, media 4
  incl. the new `m4-webcomponents` frame-attested stage, restore 1) —
  zero regressions from the DOM/prelude surgery.
* `validation/media/wc.html`: 15 self-verifying WebComponents checks —
  15/15 through the real browser (report channel via fixture-server
  fetch, immune to console truncation).
* Unit: dom (7), layout (8 incl. bidi regression), parsing (3), js (8).
* clippy + fmt clean.

### YouTube: from "skeleton" to "hydration running"

Progress ladder this session (each step verified by full-stack console
traces — the 240→900 char trace bump made stacks readable):
1. Full page loads (1.19 MB document; bot-challenge roulette explained
   below), RSS ~465 MB, zero panics.
2. ShadyDOM: YouTube *always* forces ShadyDOM `{force:true,noPatch:true}`
   (unconditional inline script) — the polyfill path IS the target
   path. It now loads cleanly (createHTMLDocument had to be fully
   settable; Document/Node-family globals had to exist; readyState had
   to be writable).
3. 147 scripts execute; Intl/statics fixed the player bootstrap;
   hydration builds ~529 DOM nodes (from 300 skeleton nodes) and then
   stalls on two remaining errors: the Cast-extension loader
   (`indexOf` of an undefined ytcfg string, kevlar 29440) and an
   uberproxy URL check receiving undefined (pmY). Documented for the
   next session.

### Google bot-detection findings (measured, not guessed)

* A bare Chrome-claiming UA over our rustls TLS gets 3 KB challenge
  stubs; the Rrowser-branded UA receives the real page. UA reverted to
  the honest brand token (curl + the engine's own hyper/h2/h3 client
  get the full page either way — the fingerprint roulette is Google's
  risk engine, not our transport).
* The engine's networking (hyper + h2 + h3 + rustls, cookies, privacy
  pipeline) fetches the full page 10/10 in isolation — verified with
  `networking/examples/yt_probe.rs`.

### Environment notes for future sessions

* `--profile` on the binary is **not wired** — the data dir is always
  `~/.local/share/rowser85`; reset with `rm -rf` like the battery does.
* The report-channel test pattern (`scripts/report-server.py` + page
  fetch to `/report?r=...`) is the reliable way to read page state —
  console output only prints at WARN+ under trace and truncates.
* `scripts/yt-patient.sh`: single-navigation patient probe (the retry
  loop in earlier probes re-navigated and RESET hydration mid-flight —
  never trust a probe that resets what it measures).

---

## Session 6 (2026-10-02): the rendering overhaul — "jumbled text on a blank background"

**Directive:** the browser rendered real pages as jumbled, reversed,
overlapping text on wrongly-colored canvases. Mandate: make the engine
render like a standard browser — glyph positioning, font fallback, CSS,
advanced layout, layering, images.

### Root causes found (each verified with a probe before fixing)

1. **Glyph x doubling** (`layout/src/text.rs` `shape_at`): cosmic-text's
   `LayoutGlyph::physical(offset, scale)` already adds the glyph's own
   line-relative x; we passed `glyph.x` as the offset too — every glyph
   landed at ~2x its x. THE "jumbled reversed" text.
2. **Leaf origin** (`layout/src/lib.rs` `extract`): text leaves were
   shaped at the PARENT box origin, discarding the leaf's own taffy
   location — flex rows / grid tracks / table rows all overlapped at the
   container corner.
3. **Selector bucket case mismatch** (`parsing/src/selector_bucket.rs`):
   ids/classes were indexed case-sensitively but looked up lowercased;
   every camelCase id selector silently never matched (Wikipedia's
   `.mw-body #bodyContent{grid-area:content}`).
4. **Random canvas color** (`engine/src/page.rs` `page_background`):
   returned the first opaque background in HashMap order — HN painted the
   whole canvas orange. Now html -> body -> white (CSS propagation).
5. **Unparsed `background:`/`border:` shorthands** (`parsing/src/css.rs`):
   sites write the shorthand 100:1 vs the longhand; pages had no
   backgrounds at all.
6. **Font stack = entry #1 only** (`layout/src/text.rs`): unknown families
   ("-apple-system") degraded to arbitrary faces. Full stack walk +
   metric-compatibility aliases (Arial->Liberation Sans, Times->
   Liberation Serif, ...) + fallback chain + installed-font probing via
   fontdb.
7. **No CSS grid** at all: grid-template-columns/rows, the
   grid-template shorthand, grid-template-areas and grid-area placement
   now flow through taffy (tracks: px/%/fr/minmax/min-max-content/
   repeat; named areas resolve to explicit line placements).
8. **Inline whitespace gluing** (`append_collapsed_text`): trailing
   separator spaces were trimmed per text node — "is <a>app</a> for"
   rendered "isappfor". Separators survive; trimmed once at leaf close.
9. **No presentational attributes**: width/height/bgcolor attributes now
   map to CSS when author CSS is silent (imgs were zero-size; HN's
   orange banner is a bgcolor).
10. **Images downloaded then dropped**: fetch matched the RESOLVED url
    against the RAW src attribute — protocol-relative images never
    attached. Both sides resolve now.
11. **No inset / z-index / stacking**: top/left/right/bottom parse +
    taffy inset mapping; display list paints positioned subtrees after
    in-flow, ordered by (z-index, DOM order).

### Verification
- `rendering/examples/visual_probe.rs`: 6 synthetic pages (grid,
  flex-row, font-stack, positioned, table, padding) — 6/6 PASS by
  vision-model inspection, all before/after.
- Live xvfb-run browser battery (`scripts/final-render-validation.sh`):
  Wikipedia renders Vector 2022 (3-column grid, titlebar, serif H1, TOC
  sidebar, Tools rail, readable paragraphs, fetched images); HN renders
  orange banner + beige canvas + columnar rows.
- Workspace tests green; clippy clean; committed as ab23e76.

### Remaining known gaps
- SVG images (Wikipedia logo) not rendered (no resvg integration yet).
- Google serves a CAPTCHA from this datacenter IP — search results
  unreachable from this environment (not a rendering issue).
- inline-block approximated as inline; borders collapsed at table
  edges; no border-radius/box-shadow/transforms.
- `client-js` class toggles (Wikipedia's JS dropdowns) need the JS DOM
  class APIs to fire for full Vector 2022 chrome.
