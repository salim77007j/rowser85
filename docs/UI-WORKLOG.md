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
