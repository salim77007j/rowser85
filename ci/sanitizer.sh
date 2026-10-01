#!/usr/bin/env bash
# Local sanitizer runner (mirrors the CI `sanitizer` job).
#
# Builds std from source with the same sanitizer flags (-Z build-std), so
# std-internal synchronisation (OnceLock futexes, mpmc channels) is
# instrumented and TSan sees the real happens-before edges.
#
# Usage:
#   ./ci/sanitizer.sh address   # ASan + leak detection
#   ./ci/sanitizer.sh thread    # TSan over the engine + e2e suites
set -euo pipefail

SAN="${1:-address}"
case "$SAN" in
  address|thread) ;;
  *) echo "usage: $0 [address|thread]"; exit 2 ;;
esac

if ! rustup toolchain list | grep -q nightly; then
  echo "nightly toolchain required (rustup toolchain install nightly)"
  exit 1
fi
rustup component add rust-src --toolchain nightly >/dev/null 2>&1 || true

export RUSTFLAGS="-Z sanitizer=${SAN}"
export ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=1}"
export TSAN_OPTIONS="${TSAN_OPTIONS:-halt_on_error=0}"

set -o pipefail
cargo +nightly test -Z build-std --workspace --exclude rowser-benchmarks \
  --lib --tests --target x86_64-unknown-linux-gnu -- --test-threads=2 2>&1 | tee /tmp/sanitized.log

if [ "$SAN" = "thread" ]; then
  python3 "$(dirname "$0")/tsan-filter.py" /tmp/sanitized.log
fi
