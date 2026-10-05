# Phase F Report — Part 1: Clone, Understand, Baseline Verification

**Date**: 2026-10-05 · **Branch**: `main` · **Base commit**: `a448c2c` (Group E perf)
**Verdict**: baseline is GREEN and reproduced from a fully reset environment.

---

## 1. Environment re-provisioning (fresh sandbox)

This session started from a fully reset sandbox (5th reset in project history).
Everything was rebuilt from zero before verification:

| Step | Result |
|---|---|
| Rust toolchain | rustup 1.99.0 stable (matches `rust-version = 1.87` floor) |
| Repository | cloned `salim77007j/rowser85` at `a448c2c` (33 MB) |
| Native libs | `libasound2-dev` + `libxkbcommon-dev` (+ runtime `libasound2t64`, `libxkbcommon0`, `libxkbcommon-x11-0`, `libxcb-xkb1`) downloaded via `apt-get download`, extracted to `/home/z/debs/extracted`, `debs-extracted/usr-lib` symlink restored |
| Capture rig | Xvfb, ffmpeg, python3 + Playwright + Pillow all present; Playwright Chromium 1200/1243 cached |

## 2. Baseline verification (the gate for every later phase)

| Gate | Expected | Measured | Result |
|---|---|---|---|
| `cargo build --workspace` | clean | clean, 4 m 03 s (2 cores, cold) | ✅ |
| `cargo test --workspace` | 155 green | **155 passed, 0 failed** | ✅ |
| `cargo fmt --all -- --check` | clean | clean | ✅ |
| `cargo clippy --workspace --all-targets -- -D warnings` | zero warnings | zero warnings (exit 0) | ✅ |
| Binary smoke launch | renders example.com | ✅ UI chrome + page text render, clean SIGTERM exit | ✅ |

Smoke-capture evidence: full window frame shows tab strip (New tab + Example
Domain), nav cluster, omnibox with security indicator, bookmarks/downloads/
privacy/actions icons, status bar `100% 4 MB page 194 MB RSS`, and the
example.com body paragraph rendered. Two known non-blockers visible:

1. The documented "×" glyph artifact (Phase B/C reports) is still present
   mid-page on example.com — tracked for the final report.
2. Font warnings for Tinos/NotoSansSC variable fonts (cosmic-text rejects
   two local faces; fallback chain handles it). Cosmetic; noted.

## 3. Documentation audit — discrepancies found (honest)

Read in full: `README.md`, `docs/BUILD.md`, `docs/V2_PLAN.md`,
`docs/PHASE_A_REPORT.md` … `docs/PHASE_E_REPORT.md`, `validation/VALIDATION.md`
(sessions 1–6), `.github/workflows/{ci,browser}.yml`.

| Briefing claim | Repo reality |
|---|---|
| "docs/WORKLOG.md" exists | **Did not exist** — only `docs/UI-WORKLOG.md`. Created by this phase. |
| "FINAL capability matrix COMPLETE, 60 features, 56 full / 4 partial" | **No capability-matrix document exists anywhere in the repo.** The matrix will be (re)built empirically during Phase F and included in `docs/V2_FINAL_REPORT.md`. |
| "WebGL (via the engine's own path)" | **NOT supported.** `js/src/prelude.js` `getContext('webgl'/'webgl2'/'webgpu')` returns `null` ("honest: no GPU context — documented deferral", V2_PLAN + PHASE_D_REPORT agree). |
| "WASM" | **NOT implemented.** No WebAssembly runtime in the prelude or any crate. |
| "EventSource" | Supported, **finite-stream only** (engine fetch is buffered; live SSE needs chunked delivery — documented). |
| README "no UI", "45 tests" | Stale — the engine has a full egui UI (validated 31 stages in `validation/VALIDATION.md`) and the suite is 155 tests. README refresh queued for the final phase. |

Implication for the remaining phases: the 30-site suite will include
threejs.org / webgpu.github.io etc., and the honest expectation is that
WebGL/WebGPU canvases will NOT render — that goes in the report as-is.
No capability will be claimed that the code does not have (hard requirement 4).

## 4. Phase 1 status

- [x] Clone + toolchain + native libs
- [x] All docs read and cross-checked
- [x] 155/155 tests, fmt clean, clippy clean
- [x] Binary smoke launch works end-to-end
- [x] `docs/WORKLOG.md` seeded
- Next: Phase 2 — 30+ site suite with Chrome ground truth → `screenshots/v2-final/`
