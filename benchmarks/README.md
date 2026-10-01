# Benchmarks

## Criterion pipeline suite

```bash
cargo bench -p rowser-benchmarks
```

Groups (see `benches/pipeline.rs`):

| Group | Sizes | What it isolates |
|---|---|---|
| `parse-html` | 200 / 2 000 / 20 000 nodes | html5ever tokenization + arena sink |
| `parse-css` | 24 / 420 / 4 000 rules | lightningcss + selector re-parse |
| `cascade+layout` | 200 / 2 000 / 20 000 nodes | warm-engine style + taffy + cosmic-text shaping |
| `engine-cold-init` | — | font system + engine construction |
| `display-list` | 2 000 nodes | doc-order item building |
| `paint` | 1280×800, 2 000 nodes | tiny-skia raster + swash glyph blit |
| `pipeline` | html→pixels, 2 000 nodes | the whole cold pipeline |

Results land in `target/criterion/` (HTML report in
`target/criterion/report/index.html`). Compare against a baseline with
`cargo bench -- --save-baseline main` then `--baseline main` later.

## Browser comparison harness

```bash
./scripts/compare.sh [tabs] [settle_seconds]
```

- Boots the Rrowser engine (`api/examples/idle_report`, release build) with
  N tabs, waits for suspension, and reads `/proc` for RSS/CPU.
- Launches every Chromium-family browser found on the machine
  (`google-chrome`, `chromium`, `brave-browser`, `microsoft-edge`) plus
  `firefox` in headless mode with the same tab count, and measures their
  **process trees** the same way.
- Emits a Markdown table to stdout and `benchmarks/results.md`.

The idle reporter is also directly usable:

```bash
cargo run --release -p rowser-api --example idle_report -- 8 20 about:blank
# → BOOT_MS=4 TABS=8 CPU_SECONDS=0.630 RSS_KB=91636
```

Honesty notes: Rrowser is engine-only (add your shell's overhead), while the
headless browsers include their full process trees; run on the target
hardware for publication. `results.md` from the CI reference runner is
committed as the baseline.
