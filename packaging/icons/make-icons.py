#!/usr/bin/env python3
"""Draws the placeholder application icon and writes every size.

The icon is a plain set of grey rectangles, the way a treemap looks, on a
dark rounded square. It is neutral on purpose: it is a stand-in until a real
icon exists, and it carries no logo or brand colour.

    python3 packaging/icons/make-icons.py

Writes bytescape.svg, bytescape-<size>.png and bytescape.ico next to this
file. Needs Pillow. The output is checked in, so building needs no Python.
"""
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SIZES = [16, 24, 32, 48, 64, 128, 256, 512]
BACKGROUND = (38, 40, 44)
# (x, y, w, h, grey) on a 100 x 100 grid, inside a 6 unit margin.
TILES = [
    (8, 8, 52, 54, 150),
    (63, 8, 29, 31, 118),
    (63, 42, 29, 20, 176),
    (8, 65, 31, 27, 132),
    (42, 65, 22, 27, 190),
    (67, 65, 25, 27, 104),
]
GAP = 1.5


def svg() -> str:
    parts = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        '<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 100 100">',
        f'  <rect width="100" height="100" rx="14" fill="rgb{BACKGROUND}"/>',
    ]
    for x, y, w, h, g in TILES:
        parts.append(
            f'  <rect x="{x + GAP / 2}" y="{y + GAP / 2}" width="{w - GAP}" '
            f'height="{h - GAP}" rx="2" fill="rgb({g},{g},{g})"/>'
        )
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def raster(size: int) -> Image.Image:
    scale = 4
    big = size * scale
    image = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    unit = big / 100
    draw.rounded_rectangle([0, 0, big - 1, big - 1], radius=14 * unit, fill=BACKGROUND + (255,))
    for x, y, w, h, g in TILES:
        draw.rounded_rectangle(
            [(x + GAP / 2) * unit, (y + GAP / 2) * unit, (x + w - GAP / 2) * unit - 1, (y + h - GAP / 2) * unit - 1],
            radius=2 * unit,
            fill=(g, g, g, 255),
        )
    return image.resize((size, size), Image.LANCZOS)


def main() -> None:
    (HERE / "bytescape.svg").write_text(svg())
    for size in SIZES:
        raster(size).save(HERE / f"bytescape-{size}.png", optimize=True)
    raster(256).save(
        HERE / "bytescape.ico",
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


if __name__ == "__main__":
    main()
