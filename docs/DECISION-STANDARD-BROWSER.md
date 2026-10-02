# Decision: the road to a "standard browser"

Status: **decided** (session 6, after live gap measurement). Owner: UI/UX +
integration agent. This document records the direction chosen for the
user directive *"a standard browser any ordinary user can use — search
Google, browse the web, watch video or livestream; priorities: massive
compatibility + high performance; do not simply copy others."*

## Measured starting point (session 6 gap run, HEAD bee8e98)

| Capability | Status | Evidence |
|---|---|---|
| Google search end-to-end | **works** (layout rough) | omnibox → results page rendered, 246 MB RSS |
| General browsing | works (8+ sites validated, 29-stage battery) | `validation/VALIDATION.md` |
| `<video src>` MP4 (H.264+AAC) playback | works (engine-internal pages + fixtures) | `docs/MEDIA.md` m1–m3 |
| MSE (fMP4 append → play) | works (own test pages) | `docs/MEDIA.md` m2 |
| Top-level navigation to a media URL | **fails** — error page / raw text; media pipeline never engages | gap run 06–08, zero `[media] ingest` |
| HLS | fMP4 media playlists only; **master (variant) playlists broken** — child playlists pushed as media bytes | gap run 06 (Apple bipbop master) |
| HLS with MPEG-TS segments | **not supported** (no TS demuxer) | gap run 07 |
| YouTube watch page | page + full subresource chain loads (HTTP 200), player JS executes, **video never starts**; page skeleton renders with grey placeholders (lazy images never fire) | gap run 03–05 + debug log |
| YouTube search/browse UX | home renders skeleton; thumbnails missing (IntersectionObserver stub never fires) | gap run 03 |

## Decision

**Continue deep-extending our own engine. No Chromium/CEF/WebView embedding,
no wholesale runtime replacement.** Reasons:

1. The performance identity (single-process, ~250–450 MB RSS on the heaviest
   sites vs Chrome 1.7 GB, 127 ms cold start) is the product's core value.
   Embedding another engine deletes it.
2. "Don't copy others" is satisfied by implementing *specifications*
   (HTML, MSE, HLS/IETF drafts) with original Rust architecture — not by
   porting foreign code. Our ISOBMFF demuxer, MSE lane design and
   audio-master-clock pipeline are already original work; we extend that
   pattern.
3. The gap list is addressable incrementally, each item shippable and
   testable on its own. Nothing above requires a rewrite.

## Session-6 scope (this sprint)

1. **Media viewer for top-level media URLs** — navigating to `.mp4/.m4v/
   .webm/.mp3/.m4a/.aac/.ogg/.flv/.ts/.m3u8` (extension shortcut + content-
   type backstop) renders a built-in black viewer document hosting
   `<video|audio controls autoplay>`, which engages the existing pipeline
   (ranged streaming / HLS walker). No full-file buffering in the common
   path.
2. **HLS done properly** — master playlists (pick lowest-bandwidth variant),
   segment-type sniffing, live poll from `TARGETDURATION`, TS segments.
3. **Original MPEG-TS demuxer** (`media/src/mpegts.rs`) — PAT/PMT/PES →
   Annex-B H.264 + ADTS AAC samples feeding the existing pipeline lanes.
4. **Native media controls** — `controls` attribute renders a real control
   bar (play/pause, seek, progress, mute) in the display list, with click
   hit-testing on the page thread. Forward seek only in v1 (backward seek
   needs sample-table re-streaming — roadmap).
5. **Replaced-element layout** — `<video>` gets intrinsic sizing
   (300×150 default, videoWidth/Height + aspect-ratio once decoded) and
   letterboxed painting (aspect-preserving, like real browsers).
6. **`<source>` children** on video/audio when no `src` attribute.
7. **Compatibility surface**: IntersectionObserver/ResizeObserver actually
   fire (unblocks lazy images site-wide), real `getBoundingClientRect`
   via a layout-rect mirror (unblocks player sizing logic), window
   dimensions/scroll, JS error surfacing (`window.onerror` +
   dispatcher catch → console), console mirror to stderr behind
   `ROWSER_JS_TRACE` for field debugging.
8. Mute-site wiring (tab strip → pipeline) if budget allows.

## Explicitly deferred (roadmap, in priority order)

- VP9/AV1 + Opus/Vorbis decode and WebM demux (Wikipedia video, YouTube
  >1080p).
- WebAssembly interpreter (`wasmi`) — needed by a growing set of sites.
- Backward seek (progressive: sample-table offset re-stream; MSE: JS-driven
  append restart).
- Canvas 2D / WebGL.
- EME/DRM: **permanently out** (documented in `docs/MEDIA.md`).

## Honesty clause

YouTube playback depends on their player JavaScript maturing in QuickJS-ng.
We ship the platform features (MSE, sizing, rects, observers, error
surfacing) and measure; the validation report records the true state after
each sprint — no fake demo pages counted as site compatibility.
