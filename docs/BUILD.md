# Build & development

## Requirements

- **Rust 1.98+** (2024-edition; `rustup default stable`).
- A C compiler for the two vendored C components (`ring`'s asm and
  QuickJS-ng): `gcc`/`clang` on Linux, MSVC on Windows. **No cmake, no nasm**
  (aws-lc-rs was rejected from the TLS path precisely to keep this true).
- ~1 GB free disk for `target/` (a clean debug build is ~600 MB; CI cleans
  between jobs).

## Building

```bash
cargo build --workspace              # debug
cargo build --release                # release (benchmarks, embedding)
cargo test --workspace               # 45 tests: unit + e2e + fuzz
cargo clippy --workspace --all-targets -- -D warnings   # zero-warning gate
cargo bench -p rowser-benchmarks     # criterion suite (release profile)
```

First clean build: ~3 min (2 cores). Incremental: seconds.

## Platform notes

**Linux** — everything above works out of the box. Font discovery uses
`cosmic-text`'s fontdb over fontconfig paths; headless servers should ensure
`/usr/share/fonts` exists (CI installs `fonts-dejavu`).

**Windows** — MSVC toolchain (`x86_64-pc-windows-msvc`). `ring` and
QuickJS-ng compile with cl.exe; no extra SDKs. The test suite skips
`/proc`-dependent checks (the idle reporter reports RSS via sysinfo's
Windows path). PowerShell equivalent of the compare harness is not provided
in v1 — run it under WSL.

**macOS** — untested in CI (no runner), expected to build via the standard
toolchain; the `ring` asm needs `clang`.

## Test suite layout

| Suite | File | What it proves |
|---|---|---|
| crate unit tests | `*/src/**` | cascade, cookies, IDB, privacy rules, JS bridge… |
| end-to-end | `tests/tests/pipeline.rs` | real HTTP server → fetch → parse → style → layout → paint → JS → events (asserts pixels, title, storage, console) |
| lifecycle | `tests/tests/pipeline.rs` | tab suspension + resume-on-focus, data URLs |
| fuzz/property | `tests/tests/fuzz.rs` | random HTML/CSS bytes and structured mutants never panic the pipeline |
| benchmarks | `benchmarks/` | criterion micro-benchmarks + browser comparison harness |

E2E logging: `ROWSER_TEST_LOG=1 RUST_LOG=rowser=debug cargo test -p
rowser-tests` to see the engine trace.

## Sanitizers

Memory-safety CI passes (GitHub Actions matrix, `.github/workflows/ci.yml`):

```bash
# AddressSanitizer (UB + leaks) on the test suite:
RUSTFLAGS="-Z sanitizer=address" cargo +nightly test --workspace --exclude rowser-benchmarks

# ThreadSanitizer on the engine crates (page-thread isolation):
RUSTFLAGS="-Z sanitizer=thread"  cargo +nightly test -p rowser-engine -p rowser-tests
```

Both are wired as optional CI jobs (`sanitizer: address`, `sanitizer: thread`
in the matrix) so they run on every push to `main`.

## CI

`.github/workflows/ci.yml` runs on every push and PR:

- **matrix**: `ubuntu-latest` × `windows-latest`, debug + release.
- steps: fmt check → clippy `-D warnings` → build → test → bench compile.
- a **sanitizer** job (Linux, nightly, ASan/TSan).
- a **lockfile hygiene** step: fails if `openssl-sys` or `aws-lc-rs` ever
  appears in `Cargo.lock` (TLS policy).

## Repository conventions

- Every crate is `#![warn(missing_docs)]`-clean at the item level; public
  items carry doc comments.
- No `unsafe` outside `rquickjs-sys` (vendored) and `ring` (audited).
- Tracing targets are `rowser::<crate>`; never `println!` in library code.
- Commits: `type(crate): summary` — `feat`, `fix`, `test`, `docs`, `ci`.
