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
WAIT=${3:-9}
HERE=$(cd "$(dirname "$0")" && pwd)
XDRIVER=$HERE/../../target/debug/xdriver
export DISPLAY=:99

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
"$BIN" >/tmp/rowser-session.log 2>&1 &
ROWSER_PID=$!
sleep 4

for entry in "${PAGES[@]}"; do
  name="${entry%%|*}"
  url="${entry#*|}"
  # Focus the address bar, clear it, type the URL, submit.
  "$XDRIVER" click 600 76
  "$XDRIVER" ctrl a
  "$XDRIVER" type "$url"
  "$XDRIVER" key Return
  sleep "$WAIT"
  ffmpeg -y -f x11grab -video_size 1360x860 -i :99 -frames:v 1 "$OUT/$name-full.png" >/dev/null 2>&1
  # Crop the content viewport below the browser chrome.
  ffmpeg -y -i "$OUT/$name-full.png" -vf "crop=1360:745:0:115" "$OUT/$name.png" >/dev/null 2>&1
  echo "captured $name"
done

kill $ROWSER_PID 2>/dev/null
kill $XVFB_PID 2>/dev/null
pkill -f "Xvfb :99" 2>/dev/null
echo "done -> $OUT"
