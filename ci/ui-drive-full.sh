#!/usr/bin/env bash
# ui-drive-full.sh — the complete validation battery: real sites, tabs,
# bookmarks, history, downloads, settings, privacy, find, zoom, devtools,
# print. Captures a screenshot per stage.
set -u
cd "$(dirname "$0")/.."
X="./target/debug/xdriver"
SHOT="screenshots"
mkdir -p "$SHOT"
snap() { ffmpeg -y -loglevel error -f x11grab -video_size 1360x860 -i :99 -frames:v 1 "$SHOT/$1.png"; }
nav() { # type into omnibox + Enter + wait
  $X click 680 61; sleep 0.8
  $X type "$1"; sleep 0.4
  $X key Return; sleep "${2:-10}"
}
internal() { # navigate to an internal page
  $X click 680 61; sleep 0.8
  $X type "$1"; sleep 0.4
  $X key Return; sleep 2
}

echo "== 1. real sites =="
nav "example.com" 8;                     snap 02-example-com
nav "en.wikipedia.org/wiki/Web_browser" 14; snap 03-wikipedia

echo "== 2. more tabs + sites =="
$X ctrl t; sleep 1.2; nav "github.com/rust-lang/rust" 13; snap 04-github
$X ctrl t; sleep 1.2; nav "news.ycombinator.com" 12;     snap 05-hackernews
$X ctrl t; sleep 1.2; nav "www.rust-lang.org" 12;        snap 05b-rustlang
$X ctrl t; sleep 1.2; nav "example.com" 6;               snap 05c-example-2

echo "== 3. omnibox search =="
$X ctrl t; sleep 1.2; nav "rust programming language" 11; snap 06-search

echo "== 4. omnibox suggestions dropdown =="
$X click 680 61; sleep 0.6
$X ctrl a; sleep 0.3
$X type "wiki"; sleep 1;                  snap 07-suggestions
$X key Escape; sleep 0.5

echo "== 5. bookmarks: add (star), bar, manager =="
$X click 1210 61; sleep 0.8              # star → bookmark current page
$X ctrl 1; sleep 1                        # first tab (example.com)
$X click 1210 61; sleep 0.8              # bookmark it
snap 08-bookmarked
internal "rowser://bookmarks"; sleep 2;  snap 09-bookmarks-manager
$X key Escape; sleep 0.3

echo "== 6. history =="
internal "rowser://history"; sleep 2;     snap 10-history

echo "== 7. privacy dashboard =="
internal "rowser://privacy"; sleep 2;     snap 11-privacy

echo "== 8. settings (appearance→dark) =="
internal "rowser://settings"; sleep 2
$X click 300 200; sleep 0.5              # nav: Appearance
$X click 640 262; sleep 0.3              # Dark theme radio
$X click 690 262; sleep 1.5              # (fallback position)
snap 12-settings-dark

echo "== 9. back to page, find in page =="
$X ctrl 1; sleep 1.5
$X ctrl f; sleep 0.8
$X type "domain"; sleep 2
snap 13-find-in-page
$X key Escape; sleep 0.5

echo "== 10. zoom in =="
$X ctrl equal; sleep 0.6
$X ctrl equal; sleep 1
snap 14-zoom-in
$X ctrl 0; sleep 0.8

echo "== 11. devtools =="
$X key F12; sleep 1.5
snap 15-devtools
$X key F12; sleep 0.8

echo "== 12. downloads (real file) =="
internal "rowser://downloads"; sleep 2
$X click 200 130; sleep 0.4              # focus URL field
$X type "https://raw.githubusercontent.com/rust-lang/rust/master/README.md"
$X key Return; sleep 8
snap 16-downloads

echo "== 13. print dialog =="
$X ctrl p; sleep 6
snap 17-print-dialog
$X key Escape; sleep 0.8

echo "== 14. many tabs state =="
$X ctrl 2; sleep 1.5; snap 18-multi-tabs
echo "== done =="
