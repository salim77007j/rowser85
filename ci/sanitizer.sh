#!/usr/bin/env bash
# Local sanitizer runner (mirrors the CI `sanitizer` job).
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

export RUSTFLAGS="-Z sanitizer=${SAN}"
export ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=1}"
export TSAN_OPTIONS="${TSAN_OPTIONS:-halt_on_error=1}"

cargo +nightly test --workspace --exclude rowser-benchmarks \
  --target x86_64-unknown-linux-gnu -- --test-threads=2
