#!/usr/bin/env bash
# restart-browser.sh — (re)start the browser under Xvfb cleanly.
# usage: ci/restart-browser.sh [fresh|keep]
set -u
cd "$(dirname "$0")/.."
export LD_LIBRARY_PATH="/home/z/debs/extracted/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# Kill and WAIT for the engine threads to release the profile lock.
pkill -f "target/debug/rowser" 2>/dev/null
for _ in $(seq 1 30); do
  pgrep -f "target/debug/rowser" >/dev/null || break
  sleep 0.3
done
pkill -9 -f "target/debug/rowser" 2>/dev/null
sleep 0.5

if [ "${1:-fresh}" = "fresh" ]; then
  rm -rf ~/.local/share/rowser85
fi

( DISPLAY=:99 ROWSER_UI_DEBUG="${ROWSER_UI_DEBUG:-}" \
    nohup ./target/debug/rowser > /tmp/rowser-run.log 2>&1 & )
# Wait for the window to map and the engine to boot.
for _ in $(seq 1 40); do
  sleep 0.5
  grep -q "panicked\|failed to start" /tmp/rowser-run.log 2>/dev/null && break
done
pgrep -f "target/debug/rowser" >/dev/null && echo "browser up" || { echo "browser failed"; tail -3 /tmp/rowser-run.log; exit 1; }
