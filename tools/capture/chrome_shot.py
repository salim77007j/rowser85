#!/usr/bin/env python3
"""Chrome ground-truth capture (Playwright Chromium).

Captures the pages in URLS as PNGs at the rowser85 content-viewport size
(1360x745 — the rowser window is 1360x860 with ~115px of browser chrome).

Usage: python3 tools/capture/chrome_shot.py [out_dir] [--full]
  --full  capture 1360x860 (window-equivalent) instead of the content crop.
"""
import asyncio
import pathlib
import sys

from playwright.async_api import async_playwright

URLS = [
    ("example", "https://example.com"),
    ("wikipedia", "https://en.wikipedia.org/wiki/Rust_(programming_language)"),
    ("github", "https://github.com"),
    ("hackernews", "https://news.ycombinator.com"),
    ("rustlang", "https://www.rust-lang.org"),
    ("bing", "https://www.bing.com/search?q=rust+programming+language"),
]

VIEWPORT = (1360, 745)


async def main() -> None:
    out_dir = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "shots/chrome")
    full = "--full" in sys.argv
    out_dir.mkdir(parents=True, exist_ok=True)
    w, h = (1360, 860) if full else VIEWPORT
    async with async_playwright() as pw:
        browser = await pw.chromium.launch(args=["--force-color-profile=srgb"])
        page = await browser.new_page(viewport={"width": w, "height": h})
        for name, url in URLS:
            try:
                await page.goto(url, wait_until="networkidle", timeout=45000)
            except Exception:
                pass  # settle for whatever loaded
            await page.wait_for_timeout(1500)
            await page.screenshot(path=str(out_dir / f"{name}.png"))
            print(f"captured {name}")
        await browser.close()


if __name__ == "__main__":
    asyncio.run(main())
