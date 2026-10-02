# Media subsystem: architecture decision and design

**Status:** implemented and validated (session 4, 2026-10-02).
**Decision owner:** UI/engine session acting under the standing directive:
*"a standard browser for any user — search Google, browse the web, watch
videos — massive compatibility, high-performance resource usage, and do not
simply copy others."*

## 1. The decision

When the mission moved from "lightweight privacy shell" to "standard browser
that can watch videos and live streams", three directions were on the table:

| Option | What it means | Verdict |
|---|---|---|
| **A. Embed someone's engine** (WebView/CEF/Chromium shell) | Instant compatibility; the project stops being a browser engine and becomes a skin | **Rejected** — violates the "don't simply copy/imitate" constraint and abandons the 12-crate Rust engine identity, the 5.8× RAM advantage and the 1-process model |
| **B. In-process native codecs** (mp4 + openh264 + symphonia + cpal) | Self-contained single binary; decode code runs in the browser process | **Rejected as the only layer** — every codec would be hand-wired; HEVC/VP9/AV1/live protocols would each be new in-process code; a decoder bug takes down the browser |
| **C. Original media subsystem in our engine**: a byte-stream pipeline with pluggable decoders, MSE surface, HLS reader | The engine gains the *architecture* real players need; codecs plug in behind it | **Adopted** |

The adopted design is **C with B's primitives**: the pipeline, demuxer,
source model, JS surface and rendering integration are original work; the
leaf codecs are proven libraries (openh264 for H.264, symphonia for AAC,
cpal for audio output). This is the same layering every real browser uses
(Chromium bundles ffmpeg; we bundle openh264+symphonia) — the *architecture*
around them is ours and is not a copy of any existing browser.

## 2. The architecture

```text
                    ┌──────────────────────── engine media loader (tokio) ───────────┐
<video src>  ──────▶│ ranged GET chunks (2 MB)  or  fMP4-HLS playlist walker          │
MediaSource ────────┤ (page JS appendBuffer → JsCommand::MediaAppendBuffer)           │
                    └───────────────────────────────┬─────────────────────────────────┘
                                                    │ page::Message::MediaData (lane n)
                                                    ▼
   page thread ──▶ MediaPipeline (one worker thread per playing element)
                    ├─ lane demuxers: original streaming ISOBMFF parser
                    │    progressive sample tables (stts/stsz/stsc/stco/stss/ctts)
                    │    fragments (tfhd/tfdt/trun) — no seek anywhere
                    ├─ video: avcC → Annex-B → openh264 → YUV420(strides) → RGBA8
                    ├─ audio: esds/ASC → symphonia AAC → f32 → cpal ring (or silence)
                    ├─ clock: wall-clock master, LEAD 0.30 s scheduling window
                    └─ notify: FrameReady / media events → page thread
                                                    │
             ┌──────────────────────────────────────┘
             ▼
   video_frames[Node] ──▶ build_display_list ──▶ DrawCmd::Image (existing image blit)
   media events ──▶ mirror map ──▶ HTMLMediaElement JS bindings (play/pause/…)
```

Key properties:

* **Everything is a byte stream.** Direct `<video src>`, fMP4-HLS live
  playlists and MSE `appendBuffer` all feed the same sequential ingress —
  one demuxer model, no seek paths, bounded memory (consumed bytes are
  dropped; the buffer cap turns non-faststart files into an explicit error
  instead of unbounded buffering).
* **Lanes.** Each MSE `SourceBuffer` is a lane with its own demuxer
  instance (YouTube's player uses two: one fMP4 video track, one audio
  track). A multiplexed fMP4 (single buffer, both tracks) also parses.
* **Process-free isolation.** Decode runs on one worker thread per playing
  element; the decoder types are thread-confined (documented `unsafe impl
  Send`), the browser process never holds raw decode state on the UI side,
  and a poisoned pipeline can only fail its own element (error event),
  never the tab.
* **Resource discipline.** No video ever fully buffers: ranged 2 MB chunks
  (192 MB hard cap for direct sources), 48 MB demuxer window, 4-frame
  presentable queue, ~500 ms audio ring. A playing 10 fps 320×240 clip
  costs ~1 worker thread and negligible RSS (measured: +11 MB over an idle
  browser).
* **Capability detection, never hard failure.** No audio device (CI,
  servers) → silent fallback + wall clock. Non-MP4 bytes → `error` event
  to page JS. Codec missing from a track → track marked undecodable, other
  track still plays.

## 3. What works today (validated end-to-end in the real browser)

* `<video src>` progressive MP4 (H.264 + AAC): streaming playback with
  loop, autoplay, muted, duration/currentTime/readyState — screenshots
  `screenshots/m1-*.png`.
* **MSE**: `new MediaSource()`, `URL.createObjectURL`,
  `addSourceBuffer`, `appendBuffer(ArrayBuffer/TypedArray)`,
  `endOfStream`, `sourceopen`/`update`/`updateend` events — fragmented MP4
  fetched by page JS plays through the pipeline: `screenshots/m2-*.png`.
  This is the surface hls.js, video.js, Plex, Twitch-class players build on.
* **fMP4-HLS** playlists (init segment + media segments, ENDLIST or
  re-polled live) via the engine's native HLS reader.
* **Remote playback over the public internet**: Big Buck Bunny (real
  480×270 H.264/AAC over HTTPS, ~30 fps) renders live:
  `screenshots/r1-*.png`.
* Real-world validation: full 29-stage battery green including 3 media
  stages; 7 in-crate fixture tests (progressive demux, fragmented demux,
  audio-only, rejection of non-MP4, full pipeline decode, frame-advance,
  duration-at-EOF).

## 4. Known limits (honest list)

* **YouTube**: the page parses, paints its skeleton and its scripts run
  clean, but the Polymer UI needs custom-element upgrade callbacks and
  shadow DOM before the player mounts; that work is the documented next
  epic and is *orthogonal to media* — it unlocks YouTube's JS app, while
  this subsystem already provides the MSE surface its player would drive.
  Many other sites (direct MP4 tags, MSE-based players, HLS players) play
  today.
* **Codecs**: H.264/AAC only for now (the web's dominant pair). VP9/AV1 =
  add a decoder implementation behind `VideoDecoder`; Opus-in-MP4 is
  parsed but marked unsupported pending symphonia opus-in-mp4 wiring.
* **MPEG-TS HLS** segments (legacy live TV) need a TS demuxer — fMP4 HLS
  (the modern profile) is what the engine reads.
* **Seek**: forward-only in v1 (skip + clock jump); backward seek needs
  the ranged window reader (v2) — MSE backward seeks re-append by design
  and already work through the same path.
* **EME/Widevine is permanently out** (licensing); Netflix et al. will not
  play — true for every independent browser.
* Replaced-element layout: `<video>` honors width/height attributes, but
  surrounding flow layout treats it as a plain block — text can overlap
  the box; the display-list blit itself is exact.

## Session 6 (standard-browser sprint)

Top-level media navigation + HLS breadth + platform fixes on top of the
session-4 pipeline:

* **Media viewer documents.** Navigating to a media URL (extension
  shortcut in the engine fetch dispatcher + content-type backstop in the
  page thread) renders a built-in black viewer page hosting
  `<video|audio controls autoplay>`, which engages the same streaming
  pipeline as site-embedded media. Validated live: direct MP4
  (w3schools mov_bbb), Apple bipbop-advanced fMP4, mux TS HLS (m5-m7
  battery stages; frame-hash change + VLM-verified content).
* **HLS done properly**: master playlists (lowest-bandwidth VIDEO variant
  — RESOLUTION attribute excludes audio-only renditions), `#EXT-X-MAP`
  with BYTERANGE, `#EXT-X-BYTERANGE` segments (ranged fetches, implicit
  continuation offsets), TARGETDURATION-driven live polling.
* **Original MPEG-TS demuxer** (`media/src/mpegts.rs`): PAT/PMT/PES →
  Annex-B H.264 + ADTS AAC samples, per-pid PTS normalization (broadcast
  streams start ~1.4 s in), ID3 tag skipping (HLS timed metadata rides in
  front of TS packets — the bipbop sniff failure), RAI/SPS keyframe
  detection, AudioSpecificConfig synthesized from the first ADTS header.
  10 media-crate tests incl. end-to-end TS playback.
* **Native media controls**: the `controls` attribute renders a real
  control bar (translucent strip, progress fill, play/pause + mute
  glyphs) in the display list, with click hit-testing on the page thread
  (play/pause toggle, seek, mute). Forward seek only in v1.
* **Replaced-element sizing**: video/audio default 300x150; video adopts
  the decoded aspect ratio when height is auto (intrinsic map). Known
  taffy 0.14 limitation: percent-width + aspect collapses to content
  size — the viewer page bakes viewport px instead; a real replaced
  measure function is the v2 fix.
* **Letterboxed video painting** (`object-fit: contain` semantics).
* **Media suspension override**: a tab with playing media is never
  auto-suspended (engine `Internal::MediaActive`, page-thread reported)
  — previously ANY tab froze after 60 s, visible or not. First tab is
  born focused.
* **`<source>` children** honored when video/audio has no `src`.

Known gaps: HLS audio-only renditions (EXT-X-MEDIA GROUP-URI lanes) not
fetched — bipbop-advanced plays video-only; backward seek; YouTube
player JS still does not start video (see VALIDATION.md).
