#!/usr/bin/env bash
# rowser85 session capture v2 — launches the browser with the target URL as
# argv[1] (the startup-URL path, no synthetic typing: XTEST input could be
# dropped while a previous page's render is busy on the 2-core box), grabs
# the window with ffmpeg, crops the content viewport (~115px chrome), and
# waits for pixel stability with per-page settle floors (JS-heavy pages
# need their first multi-second render to land).
#
# Usage: tools/capture/rowser_session.sh <binary> <out_dir> [wait_secs]
set -u
BIN=$1
OUT=$2
WAIT=${3:-30}
HERE=$(cd "$(dirname "$0")" && pwd)
XDRIVER=$HERE/../../target/debug/xdriver
export DISPLAY=:99
export LD_LIBRARY_PATH="$HERE/../../debs-extracted/usr-lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# name|url|settle_floor_secs — heavy pages get a higher minimum settle.
# Floors scale with first-render cost (compute_styles dominates on
# JS-built pages: github ~19s, wikipedia ~12s at 2 cores).
PAGES=(
  "example|https://example.com|9"
  "wikipedia|https://en.wikipedia.org/wiki/Rust_(programming_language)|30"
  "github|https://github.com|40"
  "hackernews|https://news.ycombinator.com|12"
  "rustlang|https://www.rust-lang.org|18"
  "bing|https://www.bing.com/search?q=rust+programming+language|15"
)

mkdir -p "$OUT"

for entry in "${PAGES[@]}"; do
  IFS='|' read -r name url floor <<<"$entry"
  pkill -f "Xvfb :99" 2>/dev/null; sleep 0.3
  Xvfb :99 -screen 0 1360x860x24 >/dev/null 2>&1 &
  XVFB_PID=$!
  sleep 1.2
  # Fresh profile per page: no session restore, no cross-page interference.
  export HOME=$(mktemp -d /tmp/rowser-profile.XXXX)
  export XDG_DATA_HOME="$HOME/.local/share"
  "$BIN" "$url" > "/tmp/rowser-page-$name.log" 2>&1 &
  ROWSER_PID=$!
  sleep 5

  # Wait for the page to settle: probe the content viewport every 3s;
  # settle when two consecutive probes are pixel-identical AND at least
  # $floor seconds elapsed (first render of a heavy page takes 5-15s).
  waited=0
  prev_probe=""
  stable=0
  while [ "$waited" -lt "$WAIT" ]; do
    sleep 3
    waited=$((waited + 3))
    ffmpeg -y -f x11grab -video_size 1360x745 -i :99+0,115 -frames:v 1 "$OUT/.probe-$name.png" >/dev/null 2>&1
    verdict=$(python3 - "$OUT/.probe-$name.png" "$prev_probe" <<'PYEOF'
import sys
from PIL import Image, ImageChops
new, old = sys.argv[1], sys.argv[2]
if not old:
    print("no-prev")
else:
    a = Image.open(new).convert("L")
    b = Image.open(old).convert("L").resize(a.size)
    diff = ImageChops.difference(a, b).point(lambda p: 255 if p > 12 else 0)
    changed = sum(diff.histogram()[128:])
    total = a.size[0] * a.size[1]
    print("same" if changed * 100 < total * 0.5 else "moved")
PYEOF
)
    if [ "$verdict" = "same" ] && [ "$waited" -ge "$floor" ]; then
      stable=$((stable + 1))
      if [ "$stable" -ge 1 ]; then
        break
      fi
    else
      stable=0
    fi
    prev_probe="$OUT/.probe-$name.png"
  done
  ffmpeg -y -f x11grab -video_size 1360x860 -i :99 -frames:v 1 "$OUT/$name-full.png" >/dev/null 2>&1
  ffmpeg -y -i "$OUT/$name-full.png" -vf "crop=1360:745:0:115" "$OUT/$name.png" >/dev/null 2>&1
  echo "captured $name (waited ${waited}s)"
  kill $ROWSER_PID 2>/dev/null
  kill $XVFB_PID 2>/dev/null
  pkill -f "Xvfb :99" 2>/dev/null
  sleep 0.5
done
echo "done -> $OUT"
