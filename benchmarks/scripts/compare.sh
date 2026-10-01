#!/usr/bin/env bash
# Rrowser vs. Chrome / Firefox / Brave / Edge comparison harness.
#
# Measures, per browser (whichever are installed on this machine):
#   * idle RSS after opening N tabs and settling
#   * idle CPU time
#   * cold start time
#
# Usage:  ./compare.sh [tabs] [settle_seconds]
# Output: a Markdown table on stdout (also saved to results.md).
#
# Notes:
#   * Chromium-family browsers are launched headless (`--headless=new`)
#     opening N tabs to about:blank. Their RSS includes the full browser
#     process tree (measured via `ps` on the process group).
#   * Rrowser is engine-only (no UI shell), so its numbers are the engine
#     core. Add your UI's overhead on top when comparing like-for-like.

set -euo pipefail

TABS="${1:-8}"
SETTLE="${2:-15}"
OUT="$(mktemp)"

command -v python3 >/dev/null 2>&1 || { echo "python3 required"; exit 1; }

# ---------------------------------------------------------------- helpers --

measure_tree_rss_kb() {
    # $1 = pid. Sums RSS over the process and all descendants.
    python3 - "$1" <<'PY'
import sys, os
root = int(sys.argv[1])
pids = [root]
# Collect children by scanning /proc PPid links.
table = {}
for entry in os.listdir('/proc'):
    if entry.isdigit():
        try:
            with open(f'/proc/{entry}/stat') as f:
                fields = f.read().rsplit(')', 1)[1].split()
            ppid, rss_pages = int(fields[2]), int(fields[21])
            table[int(entry)] = (ppid, rss_pages)
        except (OSError, IndexError, ValueError):
            pass
tree, stack = set(), [root]
while stack:
    pid = stack.pop()
    if pid in tree:
        continue
    tree.add(pid)
    for pid2, (ppid, _) in table.items():
        if ppid == pid:
            stack.append(pid2)
kb = sum(pages for pid, (_, pages) in table.items() if pid in tree) * 4
print(kb)
PY
}

measure_tree_cpu_s() {
    # $1 = pid. Sums utime+stime over the process tree.
    python3 - "$1" <<'PY'
import sys, os
root = int(sys.argv[1])
table = {}
for entry in os.listdir('/proc'):
    if entry.isdigit():
        try:
            with open(f'/proc/{entry}/stat') as f:
                fields = f.read().rsplit(')', 1)[1].split()
            ppid, ut, st = int(fields[2]), int(fields[11]), int(fields[12])
            table[int(entry)] = (ppid, ut, st)
        except (OSError, IndexError, ValueError):
            pass
tree, stack = set(), [root]
while stack:
    pid = stack.pop()
    if pid in tree:
        continue
    tree.add(pid)
    for pid2, (ppid, _, _) in table.items():
        if ppid == pid:
            stack.append(pid2)
ticks = sum(ut + st for pid, (_, ut, st) in table.items() if pid in tree)
print(f"{ticks / 100.0:.3f}")
PY
}

row() {
    # name boot_ms rss_kb cpu_s
    printf '| %s | %s | %s | %s |\n' "$1" "$2" "$3" "$4" >> "$OUT"
}

# ---------------------------------------------------------------- rrowser --

echo "Measuring Rrowser engine (${TABS} tabs, ${SETTLE}s settle)..."
BENCH_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$BENCH_DIR/../target/release/examples/idle_report"
if [ ! -x "$BIN" ]; then
    (cd "$BENCH_DIR/.." && cargo build --release -p rowser-api --example idle_report)
fi
RESULT="$("$BIN" "$TABS" "$SETTLE")"
RSS="$(echo "$RESULT" | grep -o 'RSS_KB=[0-9]*' | cut -d= -f2)"
CPU="$(echo "$RESULT" | grep -o 'CPU_SECONDS=[0-9.]*' | cut -d= -f2)"
BOOT="$(echo "$RESULT" | grep -o 'BOOT_MS=[0-9]*' | cut -d= -f2)"
row "Rrowser (engine)" "$BOOT" "$RSS" "$CPU"

# ------------------------------------------------------- chromium family --

try_chromium() {
    local name="$1"; shift
    local bin
    bin="$(command -v "$name" 2>/dev/null)" || return 1
    echo "Measuring $name (${TABS} tabs)..."
    local args
    args=("$@" --headless=new --disable-gpu --no-first-run \
          --user-data-dir="$(mktemp -d)" "about:blank")
    for _ in $(seq 2 "$TABS"); do args+=("about:blank"); done
    local t0 t1 pid
    t0=$(date +%s%N)
    "$bin" "${args[@]}" >/dev/null 2>&1 &
    pid=$!
    t1=$(date +%s%N)
    sleep "$SETTLE"
    local rss cpu boot
    rss="$(measure_tree_rss_kb "$pid")"
    cpu="$(measure_tree_cpu_s "$pid")"
    boot=$(( (t1 - t0) / 1000000 ))
    row "$name (headless)" "$boot" "$rss" "$cpu"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

try_firefox() {
    local bin
    bin="$(command -v firefox 2>/dev/null)" || return 1
    echo "Measuring firefox (${TABS} tabs)..."
    local profile
    profile="$(mktemp -d)"
    local t0 t1 pid
    t0=$(date +%s%N)
    "$bin" --headless --profile "$profile" --new-window "about:blank" >/dev/null 2>&1 &
    pid=$!
    t1=$(date +%s%N)
    sleep "$SETTLE"
    local rss cpu boot
    rss="$(measure_tree_rss_kb "$pid")"
    cpu="$(measure_tree_cpu_s "$pid")"
    boot=$(( (t1 - t0) / 1000000 ))
    row "firefox (headless)" "$boot" "$rss" "$cpu"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

for b in google-chrome chromium chromium-browser brave-browser microsoft-edge; do
    try_chromium "$b" 2>/dev/null || true
done
try_firefox || true

# ------------------------------------------------------------------ table --

{
    echo "# Browser comparison (${TABS} tabs, ${SETTLE}s settle, $(uname -srm))"
    echo
    echo "| Browser | Cold start (ms) | Idle RSS (KB) | Idle CPU (s) |"
    echo "|---|---|---|---|"
    cat "$OUT"
    echo
    echo "> Rrowser is engine-only; Chromium-family numbers include their full"
    echo "> headless process trees. Re-run on target hardware for publication."
} > results.md
cat results.md
