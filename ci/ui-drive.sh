#!/usr/bin/env bash
# ui-drive.sh — drives the running browser under $DISPLAY through the real
# validation battery, capturing screenshots at each stage.
# Requires: ./target/debug/rowser running, ./target/debug/xdriver, ffmpeg.
set -u
DISPLAY_NUM="${DISPLAY:-:99}"
X="./target/debug/xdriver"
SHOT="screenshots"
mkdir -p "$SHOT"

snap() { ffmpeg -y -loglevel error -f x11grab -video_size 1360x860 -i "$DISPLAY_NUM" -frames:v 1 "$SHOT/$1.png"; }
type_url() { # type into omnibox and submit
  $X click 680 61; sleep 0.7
  $X type "$1"; sleep 0.4
  $X key Return; sleep "${2:-9}"
}

echo "== stage 1: example.com =="
type_url "example.com" 10
snap 02-example-com

echo "== stage 2: wikipedia =="
type_url "en.wikipedia.org/wiki/Web_browser" 14
snap 03-wikipedia

echo "== stage 3: new tab + github =="
$X ctrl t; sleep 1.5
type_url "github.com/rust-lang/rust" 14
snap 04-github

echo "== stage 4: hacker news =="
$X ctrl t; sleep 1.5
type_url "news.ycombinator.com" 12
snap 05-hackernews

echo "== stage 5: search from omnibox =="
$X ctrl t; sleep 1.5
type_url "rust programming language" 12
snap 06-search

echo "== stage 6: scroll HN page (tab back) =="
# switch to tab 1 (first tab)
$X ctrl 1; sleep 1
snap 06b-back-to-tab1

echo "== done =="
