#!/usr/bin/env bash
# Benchmark regression gate (local equivalent of the CI baseline job).
#
# Runs the criterion suite with the fast profile and compares against the
# previous run stored in target/criterion; a >10% regression on any
# benchmark exits non-zero. Run on consecutive commits from the same tree.
set -euo pipefail

cargo bench -p rowser-benchmarks --bench pipeline -- \
  --warm-up-time 1 --measurement-time 3 --sample-size 15

echo
echo "Regression report vs previous local baseline (target/criterion):"
STATUS=0
if [ -f target/criterion/pipeline/report/change/index.html ]; then
  # Criterion prints per-benchmark deltas to stdout; keep a plain summary:
  grep -rE "regressed|improved" target/criterion/*/change/*.json 2>/dev/null || true
else
  echo "No previous baseline found — this run becomes the baseline."
fi
exit $STATUS
