# Library choices

Every third-party dependency in the engine, why it was chosen, and what was
rejected. Selection rules, in order:

1. **Best 2026 performer in its niche** — measured or benchmarked, not assumed.
2. **Mature, actively maintained, battle-tested** — no vaporware versions.
3. **Small and focused** — prefer composable crates over frameworks.

The engine language is **Rust** (non-negotiable: memory safety with zero GC
pause for the pipeline; the only GC in the system is QuickJS-ng's per-page
arena). The JS engine is **QuickJS-ng** (non-negotiable: full ES2023 support at
~1 MB footprint; V8 was rejected for its 30+ MB per-isolate cost, SpiderMonkey
for embedding complexity, and Boa/Javy-class engines for incomplete spec
coverage).

## Table of decisions

| Concern | Chosen | Version | Rejected | Why |
|---|---|---|---|---|
| HTML parsing | `html5ever` | 0.40 | `lol_html`, `scraper`, hand-written | The only complete WHATWG-compliant parser in Rust; lol_html is a streaming rewriter (no tree), scraper is DOM-heavy for our own arena. Servo lineage, actively maintained by the Servo org. |
| DOM representation | custom arena (`dom/`) | — | `scraper::Html`, `kuchiki`, ego trees | Needed generation-tagged slot ids for safe JS handles + free-list reuse (zero-fragmentation DOM GC). No off-the-shelf arena DOM exposes both. |
| CSS selector matching | `selectors` (Servo) | 0.41 | `css-selector`, lightningcss's internal matcher | The battle-tested engine from Servo with bloom-filter ancestor filters; shared `SelectorImpl` trait lets the arena DOM plug in directly. |
| CSS parsing | `lightningcss` | 1.0.0-alpha.72 | `cssparser` raw, `stylo` | lightningcss gives a typed AST, color spaces (OKLab/OKLCH), vendor-prefix normalization and media queries on top of the same cssparser core stylo uses. stylo itself is a mozilla research tree — enormous and not embeddable standalone. Alpha, but the API surface we use is stable and it is the Parcel production tool. |
| Layout | `taffy` | 0.14 | `morphorm`, `cassowary-rs`, `yoga` | 2026 layout shootout winner: block+flex+grid in one engine, flat data structures, `Send` tree. morphorm is flex-only; cassowary is constraint-solving (too slow); yoga binds C. |
| Text shaping | `cosmic-text` | 0.17 | `fontdue`, `allsorts`, `harfrust` direct | Complete stack (fontdb + rustybuzz/harfrust shaping + Unicode line-breaking + BIDI) with span-level attrs. fontdue is raster-only; allsorts is a research shaping engine; using harfrust directly would mean rebuilding line-breaking, BIDI and font fallback. |
| 2D rasterization | `tiny-skia` | 0.12 | `vello`, `wgpu` direct, `skia-safe` | CPU rasterizer with Skia-compatible path model, zero GPU dependency for v1 (CI parity, no driver variance). vello/wgpu are the planned GPU compositor path once the display list is stable; skia-safe is a C++ binding (build weight, licensing). |
| Glyph rasterization | `swash` (via cosmic-text) | — | `ab_glyph`, `fontdue` | Subpixel-positioned glyph masks + color emoji (COLR/sbix) content — required for correct text on all scripts. |
| Image decode | `image` | 0.25 | per-format crates | One dependency covering PNG/JPEG/GIF/WebP/AVIF/TIFF/QOI. Per-format crates would duplicate plumbing; `image` is the ecosystem standard and decodes in parallel. |
| Async runtime | `tokio` | 1.53 | `smol`, `monoio`, `glommio` | The whole network stack (hyper/quinn/hickory) is tokio-native; io_uring runtimes (monoio/glommio) would fork the ecosystem and gained no measurable advantage for browser traffic shapes. |
| HTTP client | `hyper` | 1.11 | `reqwest`, `ureq` | reqwest pulls a service tower + its own client (duplication on top of what we already build); ureq is sync-only. hyper 1.x with hyper-util is the foundation everything else uses. |
| HTTP/2 | `h2` | via hyper-util | — | Comes with the hyper stack. |
| HTTP/3 + QUIC | `quinn` + `h3`/`h3-quinn` | 0.11.12 / 0.0.8 | `s2n-quic` | quinn is the most-deployed Rust QUIC (Cloudflare/Inkl). s2n-quinc is AWS-tuned and heavier; the h3 crate is the protocol layer the IETF-interop tests exercise. |
| TLS | `rustls` (ring provider) | via hyper-rustls 0.27 | OpenSSL, `native-tls`, aws-lc-rs provider | Pure-Rust TLS 1.3, no C build chain, FIPS-trackable. **OpenSSL forbidden** by the task; aws-lc-rs was rejected because it drags cmake/nasm into CI (ring is cmake-free). |
| DNS | `hickory-resolver` | 0.25 | `trust-dns` (renamed), system getaddrinfo | Encrypted DNS (DoH/DoT/DoQ) in-process, with a system fallback. getaddrinfo leaks queries to the OS and cannot do DoH/DoT. |
| WebSocket | `tokio-tungstenite` | 0.30 | `fastwebsockets` | RFC6455 + extensions (permessage-deflate), tungstenite is the most battle-tested impl; fastwebsockets is server-oriented. |
| JS engine | `rquickjs` (bundling **QuickJS-ng**) | 0.14 (QuickJS-ng 0.16.2) | `quick-js`, direct FFI | rquickjs is the maintained binding generator with async support and class macros, and it vendors QuickJS-ng (verified in-tree). quick-js binds upstream quickjs (unmaintained). |
| Serialization | `serde` + `serde_json` | 1 | hand-rolled, `rkyv` | Headers/events/IDB records round-trip through serde; ecosystem standard, zero-cost derived code. rkyv's zero-copy would pay off only in the IDB hot path — deferred. |
| Storage engine | `redb` | 2.6 | `sled`, `rocksdb`, `sqlite` | Pure-Rust, ACID, single-file, lock-free reads. sled is still beta after years; rocksdb is a C++ build; sqlite is C and per-key typing is awkward for our four logical stores. |
| Cookies | `cookie` crate + custom CHIPS | 0.18 | `cookie_store` | Needed CHIPS (`Partitioned`) attribute handling and site-for-site partitioning the `cookie_store` design doesn't express. Core RFC6265 parsing reuses `cookie`. |
| Ad blocking | `adblock` (Brave) | 0.13 | `easylist` regex hand-rolled, `hosts`-file matching | The Brave ABP/filter-list engine: network + cosmetic filtering semantics, flat matcher. Regex re-implementations miss `domain=`/`~domain` and important options semantics. |
| Public suffix list | `psl` + `addr` | 2 / 0.16 | `publicsuffix` | Fresh PSL data snapshot + registrable-domain computation used for cookie partitioning and the CNAME gate. |
| Hashing (fast) | `blake3` + `rustc-hash` | 1.5 / 2 | `fnv`, `xxhash` | blake3 for keyed spoofing seeds & cache keys (fast + keyed); rustc-hash (FxHash) for DOM/class internals — same speed class as xxhash with no extra dep. |
| Crypto | `sha2`, RustCrypto primitives | 0.10 | OpenSSL bindings | Only hashes are needed (cache keys, integrity); RustCrypto is pure Rust and audited. |
| Date/time | `jiff` | 0.2 | `chrono`, `time` | 2026-era API (tzdb via bundled data, no `TZ` env dependence), correct DST arithmetic for cookie expiry; `time` 0.3 API is awkward for time zones. |
| Time in storage | `time` (via `cookie`) | 0.3 | — | Forced by the `cookie` crate; isolated to cookie parsing. |
| Logging/tracing | `tracing` + `tracing-subscriber` | 0.1 / 0.3 | `log` | Span-aware, per-target filtering (`ROWSER_TEST_LOG`/`RUST_LOG`), zero-cost when disabled. |
| Errors | `anyhow` (app) + `thiserror` (libs) | 1 / 2 | hand-written | thiserror for typed library errors (network, storage, JS), anyhow only at the API boundary. |
| System metrics | `sysinfo` | 0.36 | `procfs` | Cross-platform (CI runs Linux+Windows): RSS per page thread, total RAM for the memory manager. |
| Property testing | `proptest` | 1.11 | `quickcheck` | Composable strategies, shrinking, deterministic replay via seeds. |
| Benchmarks | `criterion` | 0.7 | `divan`, `iai` | Statistics (outlier detection), regression comparison vs baselines, HTML reports. |
| Base64 (tests) | `base64` | 0.23 | — | Test PNG/data-URL helpers. |

## Version policy

Dependencies were resolved in October 2026 and locked in `Cargo.lock`. Bumps
are deliberate: each upgrade re-runs the fuzz and e2e suites (`cargo test
--workspace`) plus the benchmark regression check
(`cargo bench -p rowser-benchmarks` against the committed baseline in
`benchmarks/results.md`).

## Notable non-choices

- **No V8, no SpiderMonkey** — mandated QuickJS-ng; enforced by never
  depending on any binding to them.
- **No OpenSSL anywhere** — the tree is pure-Rust on the TLS path; CI greps
  `Cargo.lock` for `openssl-sys` and fails the build if it appears.
- **No GPU dependency in v1** — tiny-skia keeps CI deterministic; the display
  list is GPU-ready (flat, ordered, no retained-mode state), so a vello/wgpu
  compositor is an additive change.
- **No UI framework** — the engine exposes `rowser-api` only; the shell is a
  separate project by design.
