#!/usr/bin/env python3
"""Generate the JchTools app icon (PNG + multi-size ICO).

The art matches the in-app logo badge: rounded square with the indigo -> cyan
gradient and a white geometric J. Drawn at 4x and downsampled so the 16 px
taskbar size stays crisp. Run from the repository root:

    python scripts/make-icon.py
"""

from __future__ import annotations

import sys
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
RESOURCES = ROOT / "resources"
SUPERSAMPLE = 4
BASE = 256
ACCENT = (79, 70, 229)  # #4f46e5
ACCENT_2 = (14, 165, 233)  # #0ea5e9


def gradient(size: int) -> Image.Image:
    image = Image.new("RGB", (size, size), ACCENT)
    draw = ImageDraw.Draw(image)
    for offset in range(2 * size):
        factor = offset / (2 * size - 1)
        color = tuple(round(a + (b - a) * factor) for a, b in zip(ACCENT, ACCENT_2, strict=False))
        draw.line([(offset, 0), (0, offset)], fill=color, width=2)
    return image


def rounded_mask(size: int) -> Image.Image:
    mask = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(mask)
    draw.rounded_rectangle([0, 0, size - 1, size - 1], radius=round(size * 0.22), fill=255)
    return mask


def glyph(size: int) -> Image.Image:
    """White geometric J, sized in the same coordinate system as `size`."""
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(layer)
    s = size / 256
    left, right = 74 * s, 182 * s
    top, bottom = 74 * s, 182 * s
    stroke = 27 * s
    # 块状 J：右侧竖笔从顶到底，底部横笔向左延伸（与旧 M 共用同一描边宽度与包围盒）。
    stem_left = right - stroke
    hook_left = left
    hook_top = bottom - stroke
    points = [
        (stem_left, top),
        (right, top),
        (right, bottom),
        (hook_left, bottom),
        (hook_left, hook_top),
        (stem_left, hook_top),
    ]
    draw.polygon(points, fill=(255, 255, 255, 255))
    return layer


def render(size: int) -> Image.Image:
    work = BASE * SUPERSAMPLE
    image = Image.new("RGBA", (work, work), (0, 0, 0, 0))
    image.paste(gradient(work), (0, 0), rounded_mask(work))
    image.alpha_composite(glyph(work))
    return image.resize((size, size), Image.LANCZOS)


def main() -> int:
    RESOURCES.mkdir(exist_ok=True)
    sizes = [256, 128, 64, 48, 32, 24, 16]
    images = {size: render(size) for size in sizes}
    png_path = RESOURCES / "app-icon.png"
    images[256].save(png_path)
    ico_path = RESOURCES / "app.ico"
    images[256].save(ico_path, format="ICO", sizes=[(size, size) for size in sizes])
    print(f"wrote {png_path} ({png_path.stat().st_size} bytes)")
    print(f"wrote {ico_path} ({ico_path.stat().st_size} bytes, sizes {sizes})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
