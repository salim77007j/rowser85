#!/usr/bin/env python3
"""Side-by-side composite + pixel metrics: rowser85 | Chrome.

Usage: python3 tools/capture/side_by_side.py <chrome_dir> <rowser_dir> <out_dir> [names...]

Produces out_dir/side-by-side-<name>.png (Chrome left, rowser85 right,
scaled 50%) and prints a grayscale threshold-diff percentage per pair.
"""
import pathlib
import sys

from PIL import Image, ImageChops, ImageOps

SCALE = 0.5


def diff_pct(a: Image.Image, b: Image.Image) -> float:
    """Fraction of pixels that differ beyond a small tolerance (grayscale)."""
    if a.size != b.size:
        b = b.resize(a.size)
    ga, gb = ImageOps.grayscale(a), ImageOps.grayscale(b)
    diff = ImageChops.difference(ga, gb).point(lambda p: 255 if p > 24 else 0)
    hist = diff.histogram()
    changed = sum(hist[128:])
    total = a.size[0] * a.size[1]
    return 100.0 * changed / max(1, total)


def main() -> None:
    chrome_dir = pathlib.Path(sys.argv[1])
    rowser_dir = pathlib.Path(sys.argv[2])
    out_dir = pathlib.Path(sys.argv[3])
    names = sys.argv[4:] or sorted(
        p.stem for p in chrome_dir.glob("*.png") if (rowser_dir / p.name).exists()
    )
    out_dir.mkdir(parents=True, exist_ok=True)
    for name in names:
        ca = chrome_dir / f"{name}.png"
        rb = rowser_dir / f"{name}.png"
        if not ca.exists() or not rb.exists():
            print(f"{name}: MISSING ({ca.exists()=} {rb.exists()=})")
            continue
        a, b = Image.open(ca).convert("RGB"), Image.open(rb).convert("RGB")
        if a.size != b.size:
            b = b.resize(a.size)
        w = int(a.size[0] * SCALE)
        h = int(a.size[1] * SCALE)
        a, b = a.resize((w, h)), b.resize((w, h))
        canvas = Image.new("RGB", (w * 2 + 8, h), (20, 20, 20))
        canvas.paste(a, (0, 0))
        canvas.paste(b, (w + 8, 0))
        canvas.save(out_dir / f"side-by-side-{name}.png")
        print(f"{name}: {diff_pct(a.resize((w * 2, h * 2)), b.resize((w * 2, h * 2))):.1f}% diff")


if __name__ == "__main__":
    main()
