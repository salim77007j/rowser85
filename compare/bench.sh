#!/usr/bin/env bash
# compare/bench.sh — cross-browser benchmark harness (rowser vs Chrome).
#
# Metrics (identical rig for both browsers, same 5 sites):
#   cold-start  : process spawn → first non-baseline frame (video @20fps)
#   ram         : process-tree VmRSS + VmHWM after the sites settle
#   cpu-idle    : utime+stime delta over 15s with pages loaded
#   cpu-scroll  : utime+stime delta over 12s of XTEST scrolling
#   shutdown    : SIGTERM → process-tree exit
#
# Usage: compare/bench.sh rowser|chrome
set -u
cd "$(dirname "$0")/.."
WHO="${1:?rowser|chrome}"
export DISPLAY="${DISPLAY:-:99}"
export LD_LIBRARY_PATH="/home/z/debs/extracted/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
X="$(realpath ./target/debug/xdriver)"
RES="1360x860"
PROFILE="/tmp/cmp-profile-$WHO"
OUT="validation/bench-$WHO.txt"
mkdir -p validation
: > "$OUT"

SITES=("example.com" "en.wikipedia.org/wiki/Web_browser" "github.com/rust-lang/rust" "news.ycombinator.com" "www.rust-lang.org")

ensure_xvfb() {
  local current=""
  if pgrep -f "Xvfb :99" >/dev/null; then
    current=$(pgrep -af "Xvfb :99" | grep -oE '[0-9]+x[0-9]+x24' | head -1 | cut -dx -f1-2)
  fi
  if [ -z "$current" ] || [ "$current" != "$RES" ]; then
    pkill -f "Xvfb :99" 2>/dev/null; sleep 1
    rm -f /tmp/.X99-lock /tmp/.X11-unix/X99
    Xvfb :99 -screen 0 "${RES}x24" >/dev/null 2>&1 &
    sleep 1.5
  fi
}
ensure_xvfb

pids() { # all pids of the app (rowser: binary path; chrome: user-data-dir)
  if [ "$WHO" = "chrome" ]; then pgrep -f "user-data-dir=$PROFILE" || true
  else pgrep -f "dist/rowser" || true; fi
}

tree_rss() { # summed VmRSS (MB) over process tree
  local total=0 pid rss
  for pid in $(pids); do
    rss=$(awk '/VmRSS/ {print $2}' "/proc/$pid/status" 2>/dev/null)
    [ -n "${rss:-}" ] && total=$((total + rss))
  done
  awk -v t="$total" 'BEGIN{printf "%.1f", t/1024}'
}
tree_hwm() {
  local total=0 pid h
  for pid in $(pids); do
    h=$(awk '/VmHWM/ {print $2}' "/proc/$pid/status" 2>/dev/null)
    [ -n "${h:-}" ] && total=$((total + h))
  done
  awk -v t="$total" 'BEGIN{printf "%.1f", t/1024}'
}
tree_cpu_jiffies() { # summed utime+stime of process tree
  local total=0 pid u s
  for pid in $(pids); do
    read -r u s < <(awk '{print $14, $15}' "/proc/$pid/stat" 2>/dev/null)
    [ -n "${u:-}" ] && total=$((total + u + s))
  done
  echo "$total"
}
cpu_pct() { # cpu_pct <jiffies1> <jiffies2> <seconds>
  awk -v a="$1" -v b="$2" -v s="$3" 'BEGIN{printf "%.1f", (b-a)/s}'
}

killall_app() {
  local p
  p=$(pids | tr '\n' ' ')
  [ -n "$p" ] && kill $p 2>/dev/null
  for _ in $(seq 1 40); do [ -z "$(pids)" ] && break; sleep 0.25; done
  p=$(pids | tr '\n' ' '); [ -n "$p" ] && kill -9 $p 2>/dev/null
  sleep 0.5
}

launch() { # launch [urls...] — spawn under Xvfb with a clean profile
  if [ "$WHO" = "chrome" ]; then
    CHROME="$(realpath compare/chrome-linux64/chrome)"
    ( setsid nohup "$CHROME" --no-sandbox --no-first-run --no-default-browser-check \
        --disable-dev-shm-usage --user-data-dir="$PROFILE" \
        --window-size=1360,860 --window-position=0,0 "$@" \
        > /tmp/chrome-run.log 2>&1 < /dev/null & )
  else
    ( setsid nohup ./dist/rowser > /tmp/rowser-run.log 2>&1 < /dev/null & )
  fi
}

# ------------------------------------------------------ cold-start x5 ------
cold_start() {
  echo "cold-start (5 runs, first-paint at 20fps):" >> "$OUT"
  local i t0 frames idx ms
  for i in 1 2 3 4 5; do
    killall_app
    rm -rf "$PROFILE"; [ "$WHO" = "rowser" ] && rm -rf ~/.local/share/rowser85
    sleep 1
    # baseline = bare root
    ffmpeg -y -loglevel error -f x11grab -video_size "$RES" -i :99 \
      -frames:v 1 /tmp/bench-base.png 2>/dev/null
    local base; base=$(md5sum /tmp/bench-base.png | cut -d' ' -f1)
    # record 15s @20fps in background
    ffmpeg -y -loglevel error -f x11grab -framerate 20 -video_size "$RES" \
      -i :99 -t 15 /tmp/bench-cold.mkv 2>/dev/null &
    local rec=$!
    sleep 0.5
    t0=$(date +%s%N)
    launch
    wait $rec
    # extract downscaled frames, find the first that differs from baseline
    rm -rf /tmp/bench-frames; mkdir -p /tmp/bench-frames
    ffmpeg -y -loglevel error -i /tmp/bench-cold.mkv -vf scale=80:50 \
      /tmp/bench-frames/f%04d.png 2>/dev/null
    idx=$(python3 - "$base" <<'EOF'
import sys, glob, hashlib
base = sys.argv[1]
best = None
for i, f in enumerate(sorted(glob.glob('/tmp/bench-frames/f*.png')), 1):
    h = hashlib.md5(open(f,'rb').read()).hexdigest()
    if h != base:
        best = i; break
print(best or -1)
EOF
)
    if [ "$idx" = "-1" ]; then
      echo "  run$i: FAILED (no frame change in 15s)" >> "$OUT"
    else
      ms=$(( (1000 * (idx - 1)) / 20 + 500 ))
      echo "  run$i: ${ms}ms (first change at frame $idx)" >> "$OUT"
    fi
  done
}

# ------------------------------------------------------- page-load RAM ----
load_sites() {
  killall_app
  rm -rf "$PROFILE"; [ "$WHO" = "rowser" ] && rm -rf ~/.local/share/rowser85
  sleep 1
  if [ "$WHO" = "chrome" ]; then
    launch "${SITES[@]/#/https://}"
    sleep 35                     # let all tabs load and settle
  else
    launch; sleep 6
    local s
    for s in "${SITES[@]}"; do
      $X key Escape; sleep 0.3
      $X ctrl t; sleep 1.0
      $X click 680 61; sleep 0.9
      $X type "$s"; sleep 0.5
      $X key Return; sleep 10
    done
    sleep 10
  fi
  echo "ram-after-load (same 5 sites):" >> "$OUT"
  echo "  VmRSS(tree)=$(tree_rss)MB  VmHWM(tree)=$(tree_hwm)MB  procs=$(pids | wc -w)" >> "$OUT"
  # screenshot evidence
  ffmpeg -y -loglevel error -f x11grab -video_size "$RES" -i :99 \
    -frames:v 1 "validation/bench-$WHO-loaded.png" 2>/dev/null
}

cpu_workloads() {
  local j1 j2 i
  sleep 2; j1=$(tree_cpu_jiffies); sleep 15; j2=$(tree_cpu_jiffies)
  echo "cpu-idle: $(cpu_pct "$j1" "$j2" 15)% (15s, 5 tabs loaded)" >> "$OUT"
  # scroll the front tab content for 12s
  $X move 700 400 2>/dev/null
  j1=$(tree_cpu_jiffies)
  for i in $(seq 1 24); do $X scroll 0 5 2>/dev/null; sleep 0.25; done
  for i in $(seq 1 24); do $X scroll 0 -5 2>/dev/null; sleep 0.25; done
  j2=$(tree_cpu_jiffies)
  echo "cpu-scroll: $(cpu_pct "$j1" "$j2" 12)% (12s of XTEST scrolling)" >> "$OUT"
}

shutdown_time() {
  local t0 t1 p
  p=$(pids | tr '\n' ' ')
  [ -z "$p" ] && return
  t0=$(date +%s%N)
  kill $p 2>/dev/null
  for _ in $(seq 1 100); do [ -z "$(pids)" ] && break; sleep 0.1; done
  t1=$(date +%s%N)
  echo "shutdown: $(( (t1 - t0) / 1000000 ))ms (SIGTERM)" >> "$OUT"
  pkill -9 -f "user-data-dir=$PROFILE" 2>/dev/null
  pkill -9 -f "dist/rowser" 2>/dev/null
  sleep 1
}

echo "== bench: $WHO ==" >> "$OUT"
cold_start
load_sites
cpu_workloads
shutdown_time
echo "done" >> "$OUT"
cat "$OUT"
