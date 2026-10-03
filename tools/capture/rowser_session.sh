#!/usr/bin/env bash
# rowser85 session capture — drives the real browser UI under Xvfb with
# XTEST synthetic input (xdriver), grabs the window with ffmpeg, and crops
# the content viewport (browser chrome is ~115px tall).
#
# Usage: tools/capture/rowser_session.sh <binary> <out_dir> [wait_secs]
#   <binary>   path to the rowser executable (target/debug/rowser)
#   <out_dir>  destination for <name>.png files
#   wait_secs  per-page settle time (default 9; JS-heavy sites need more)
#
# Pages are the same set as chrome_shot.py so composites line up.
set -u
BIN=$1
OUT=$2
WAIT=${3:-26}
HERE=$(cd "$(dirname "$0")" && pwd)
XDRIVER=$HERE/../../target/debug/xdriver
export DISPLAY=:99
# System X11 runtime libs the browser dlopen()s (xkbcommon-x11 etc).
export LD_LIBRARY_PATH="$HERE/../../debs-extracted/usr-lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

PAGES=(
  "example|https://example.com"
  "wikipedia|https://en.wikipedia.org/wiki/Rust_(programming_language)"
  "github|https://github.com"
  "hackernews|https://news.ycombinator.com"
  "rustlang|https://www.rust-lang.org"
  "bing|https://www.bing.com/search?q=rust+programming+language"
)

mkdir -p "$OUT"
pkill -f "Xvfb :99" 2>/dev/null; sleep 0.3
Xvfb :99 -screen 0 1360x860x24 >/dev/null 2>&1 &
XVFB_PID=$!
sleep 1.2
# Fresh profile per run: no session restore of previous capture tabs.
export HOME=$(mktemp -d /tmp/rowser-profile.XXXX)
export XDG_DATA_HOME="$HOME/.local/share"
"$BIN" >/tmp/rowser-session.log 2>&1 &
ROWSER_PID=$!
sleep 4

for entry in "${PAGES[@]}"; do
  name="${entry%%|*}"
  url="${entry#*|}"
  # Focus the address bar, clear it, type the URL, submit. Retry once if
  # the viewport doesn't change within 4s (input can be dropped while the
  # previous page's JS is busy).
  navigate_url() {
    "$XDRIVER" click 600 76
    sleep 0.25
    "$XDRIVER" ctrl a
    "$XDRIVER" type "$url"
    "$XDRIVER" key Return
  }
  ffmpeg -y -f x11grab -video_size 1360x745 -i :99+0,115 -frames:v 1 "$OUT/.before-$name.png" >/dev/null 2>&1
  navigate_url
  sleep 4
  ffmpeg -y -f x11grab -video_size 1360x745 -i :99+0,115 -frames:v 1 "$OUT/.after-$name.png" >/dev/null 2>&1
  moved=$(python3 - "$OUT/.before-$name.png" "$OUT/.after-$name.png" <<'PYEOF2'
import sys
from PIL import Image, ImageChops
a = Image.open(sys.argv[1]).convert("L")
b = Image.open(sys.argv[2]).convert("L").resize(a.size)
diff = ImageChops.difference(a, b).point(lambda p: 255 if p > 12 else 0)
changed = sum(diff.histogram()[128:])
total = a.size[0] * a.size[1]
print("moved" if changed * 100 >= total * 0.5 else "same")
PYEOF2
)
  if [ "$moved" = "same" ]; then
    echo "  retry: input appears dropped for $name"
    navigate_url
    sleep 4
  fi
  # Wait for the page to settle: probe the content viewport every 3s;
  # settle when two consecutive probes are pixel-identical AND at least
  # 9s elapsed (heavy JS sites: spinner + hydration + late images).
  waited=0
  prev_probe=""
  stable=0
  while [ "$waited" -lt "$WAIT" ]; do
    sleep 3
    waited=$((waited + 3))
    ffmpeg -y -f x11grab -video_size 1360x745 -i :99+0,115 -frames:v 1 "$OUT/.probe-$name.png" >/dev/null 2>&1
    verdict=$(python3 - "$OUT/.probe-$name.png" "$prev_probe" <<'PYEOF2'
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
PYEOF2
)
    if [ "$verdict" = "same" ] && [ "$waited" -ge 9 ]; then
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
  # Crop the content viewport below the browser chrome.
  ffmpeg -y -i "$OUT/$name-full.png" -vf "crop=1360:745:0:115" "$OUT/$name.png" >/dev/null 2>&1
  echo "captured $name (waited ${waited}s)"
done

kill $ROWSER_PID 2>/dev/null
kill $XVFB_PID 2>/dev/null
pkill -f "Xvfb :99" 2>/dev/null
echo "done -> $OUT"
