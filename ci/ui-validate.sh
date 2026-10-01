#!/usr/bin/env bash
# ui-validate.sh — hardened validation battery with per-stage capture
# verification. Every stage performs a real interaction, then waits until the
# frame CHANGES (byte-compared). Stages whose capture never changes are
# reported as FAIL — no silent duplicates. RSS is sampled per stage.
#
# Usage:  BROWSER=./dist/rowser ci/ui-validate.sh [phase]
# Phases: sites | features | restore | big | small   (default: all)
set -u
cd "$(dirname "$0")/.."
BROWSER="$(realpath "${BROWSER:-./target/debug/rowser}")"
X="$(realpath ./target/debug/xdriver)"
SHOT="${SHOT:-screenshots}"
RES="${RES:-1360x860}"
PHASE="${1:-all}"
export DISPLAY="${DISPLAY:-:99}"
export LD_LIBRARY_PATH="/home/z/debs/extracted/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
mkdir -p "$SHOT"
REPORT="$SHOT/validation-report.txt"
: > "$REPORT"

last_hash=""
fails=0

rss() { # peak/current RSS of the browser process, MB
  local pid
  pid=$(pgrep -f "$BROWSER" | head -1)
  [ -n "$pid" ] || { echo "?"; return; }
  awk '/VmRSS/ {printf "%.1f", $2/1024}' "/proc/$pid/status" 2>/dev/null || echo "?"
}

# snap <name> [wait_seconds] — capture, verify frame changed vs previous
# stage, retry capture until change or timeout; record PASS/FAIL + RSS.
snap() {
  local name="$1" secs="${2:-8}" hash tries=0
  local out="$SHOT/$name.png"
  while [ $tries -le $((secs / 2)) ]; do
    ffmpeg -y -loglevel error -f x11grab -video_size "$RES" -i :99 \
      -frames:v 1 "$out" 2>/dev/null
    hash=$(md5sum "$out" 2>/dev/null | cut -d' ' -f1)
    if [ -n "$hash" ] && [ "$hash" != "$last_hash" ]; then
      echo "PASS $name  rss=$(rss)MB" | tee -a "$REPORT"
      last_hash="$hash"
      return 0
    fi
    tries=$((tries + 1)); sleep 2
  done
  echo "FAIL $name (frame unchanged vs previous stage)" | tee -a "$REPORT"
  fails=$((fails + 1))
  return 1
}

# force a baseline so the very first stage can differ from it.
reset_baseline() {
  ffmpeg -y -loglevel error -f x11grab -video_size "$RES" -i :99 \
    -frames:v 1 "$SHOT/.baseline.png" 2>/dev/null
  last_hash=$(md5sum "$SHOT/.baseline.png" 2>/dev/null | cut -d' ' -f1)
}

nav() { # type into omnibox + Enter (real navigation)
  $X key Escape; sleep 0.4
  $X click 680 61; sleep 0.8
  $X type "$1"; sleep 0.4
  $X key Return
}

internal() { # navigate to an internal page
  $X key Escape; sleep 0.3
  $X click 680 61; sleep 0.8
  $X type "$1"; sleep 0.4
  $X key Return
}

newtab_nav() { # Ctrl+T then navigate
  $X key Escape; sleep 0.3
  $X ctrl t; sleep 1.2
  $X click 680 61; sleep 0.8
  $X type "$1"; sleep 0.4
  $X key Return
}

stop_browser() {
  pkill -f "$BROWSER" 2>/dev/null
  for _ in $(seq 1 30); do
    pgrep -f "$BROWSER" >/dev/null || break
    sleep 0.3
  done
  pkill -9 -f "$BROWSER" 2>/dev/null
  sleep 0.5
}

start_browser() { # start_browser [fresh|keep]
  stop_browser
  [ "${1:-fresh}" = "fresh" ] && rm -rf ~/.local/share/rowser85
  ( DISPLAY=:99 ROWSER_UI_DEBUG="${ROWSER_UI_DEBUG:-}" \
    nohup "$BROWSER" > /tmp/rowser-run.log 2>&1 & )
  for _ in $(seq 1 40); do
    sleep 0.5
    grep -q "panicked\|failed to start" /tmp/rowser-run.log 2>/dev/null && break
    pgrep -f "$BROWSER" >/dev/null || continue
    sleep 2; break
  done
  pgrep -f "$BROWSER" >/dev/null && echo "browser up ($(rss)MB)" || {
    echo "browser failed"; tail -3 /tmp/rowser-run.log; exit 1; }
}

ensure_xvfb() { # the harness reaps background procs at command end;
  # each run must bring up its own display server.
  pgrep -f "Xvfb :99" >/dev/null || {
    rm -f /tmp/.X99-lock /tmp/.X11-unix/X99
    Xvfb :99 -screen 0 "${XVFB_RES:-1360x860}x24" >/dev/null 2>&1 &
    sleep 1.5
  }
}
ensure_xvfb

# ---------------------------------------------------------------- sites ----
if [ "$PHASE" = "all" ] || [ "$PHASE" = "sites" ]; then
  echo "== phase: sites ==" | tee -a "$REPORT"
  stop_browser
  reset_baseline                 # bare Xvfb root = baseline
  start_browser fresh
  sleep 2
  snap 01-newtab 6                                  # start page / quick dial

  nav "example.com"; sleep 6;             snap 02-example-com 10
  nav "en.wikipedia.org/wiki/Web_browser"; sleep 10; snap 03-wikipedia 12

  newtab_nav "github.com/rust-lang/rust"; sleep 12;  snap 04-github 14
  newtab_nav "news.ycombinator.com";      sleep 10;  snap 05-hackernews 12
  newtab_nav "www.rust-lang.org";         sleep 10;  snap 05b-rustlang 12
  newtab_nav "example.com";               sleep 5;   snap 05c-example-2 8
  newtab_nav "www.mozilla.org";           sleep 10;  snap 05d-mozilla 12
  newtab_nav "info.cern.ch";              sleep 6;   snap 05e-cern 8

  newtab_nav "rust programming language"; sleep 9;   snap 06-search 12
fi

# ------------------------------------------------------------- features ----
if [ "$PHASE" = "all" ] || [ "$PHASE" = "features" ]; then
  echo "== phase: features ==" | tee -a "$REPORT"
  if [ "$PHASE" = "features" ]; then
    reset_baseline                 # standalone run: current frame = baseline
  fi
  # suggestions dropdown
  $X key Escape; sleep 0.3
  $X click 680 61; sleep 0.6
  $X ctrl a; sleep 0.3
  $X type "wiki"; sleep 1.5; snap 07-suggestions 6
  $X key Escape; sleep 0.5

  # bookmark via star (current tab: search results) then first tab
  $X click 1210 61; sleep 1.0
  $X ctrl 1; sleep 1.2
  $X click 1210 61; sleep 1.0; snap 08-bookmarked 6

  internal "rowser://bookmarks"; sleep 2;  snap 09-bookmarks-manager 6
  $X key Escape; sleep 0.3

  internal "rowser://history";  sleep 2;   snap 10-history 6
  $X key Escape; sleep 0.3

  internal "rowser://privacy";  sleep 2;   snap 11-privacy 6
  $X key Escape; sleep 0.3

  # settings → appearance → dark
  internal "rowser://settings"; sleep 2
  $X click 300 200; sleep 0.6
  $X click 640 262; sleep 0.4
  $X click 690 262; sleep 1.5;  snap 12-settings-dark 6
  $X key Escape; sleep 0.3

  # find in page on example.com tab
  $X ctrl 1; sleep 1.5
  $X ctrl f; sleep 0.8
  $X type "domain"; sleep 1.5;  snap 13-find-in-page 6
  $X key Escape; sleep 0.5

  # zoom
  $X ctrl equal; sleep 0.6
  $X ctrl equal; sleep 0.8;      snap 14-zoom-in 6
  $X ctrl 0; sleep 0.8

  # devtools
  $X key F12; sleep 1.5;        snap 15-devtools 6
  $X key F12; sleep 0.8

  # downloads: real file over the network
  internal "rowser://downloads"; sleep 2
  $X click 200 130; sleep 0.5
  $X type "https://raw.githubusercontent.com/rust-lang/rust/master/README.md"
  $X key Return; sleep 8;       snap 16-downloads 10
  $X key Escape; sleep 0.3

  # print dialog
  $X ctrl p; sleep 5;           snap 17-print-dialog 8
  $X key Escape; sleep 0.8

  # tab context menu (mute/pin/…)
  $X right-click 110 19; sleep 1.0; snap 18-tab-context-menu 6
  $X key Escape; sleep 0.4

  # many-tabs overview
  $X ctrl 2; sleep 1.5;         snap 19-multi-tabs 6
fi

# -------------------------------------------------------------- restore ----
if [ "$PHASE" = "all" ] || [ "$PHASE" = "restore" ]; then
  echo "== phase: session restore ==" | tee -a "$REPORT"
  stop_browser
  start_browser keep             # profile kept → tabs must restore
  sleep 3
  snap 20-session-restore 10
fi

# ------------------------------------------------------------------ size ----
big() {
  stop_browser
  pkill Xvfb; sleep 1
  rm -f /tmp/.X99-lock /tmp/.X11-unix/X99
  Xvfb :99 -screen 0 1920x1080x24 >/dev/null 2>&1 & sleep 1.5
  RES="1920x1080"
  reset_baseline                 # bare root before launch
  start_browser keep; sleep 2
  snap 21-window-1080p 8
}
small() {
  stop_browser
  pkill Xvfb; sleep 1
  rm -f /tmp/.X99-lock /tmp/.X11-unix/X99
  Xvfb :99 -screen 0 1024x768x24 >/dev/null 2>&1 & sleep 1.5
  RES="1024x768"
  reset_baseline                 # bare root before launch
  start_browser keep; sleep 2
  snap 22-window-768p 8
}
if [ "$PHASE" = "all" ] || [ "$PHASE" = "big" ];   then big;   fi
if [ "$PHASE" = "all" ] || [ "$PHASE" = "small" ]; then small; fi

echo "== summary ==" | tee -a "$REPORT"
echo "failed stages: $fails" | tee -a "$REPORT"
grep -c "^PASS" "$REPORT" | xargs -I{} echo "passed stages: {}" | tee -a "$REPORT"
rm -f "$SHOT/.baseline.png"
exit $([ "$fails" -eq 0 ] && echo 0 || echo 1)
