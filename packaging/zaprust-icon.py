#!/usr/bin/env python3
"""Generate the Zaprust app icon rasters from the master artwork.

The master artwork is packaging/icon.png: the app icon as a
square, transparent-background image (a green speech bubble with
a white ring, three white dots, and the green signal crescent).
This script downscales it with LANCZOS to the shipped rasters:

- packaging/zaprust.png (512x512), the icon the installers ship
- packaging/icons/zaprust-<size>.png for the standard sizes
  256, 128, 64, 48, 32, 24, and 16

Usage: python3 packaging/zaprust-icon.py
Requires: pillow (pip install pillow).
"""

from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
MASTER = HERE / "icon.png"
OUT = HERE / "zaprust.png"
SIZES_DIR = HERE / "icons"
SIZES = [256, 128, 64, 48, 32, 24, 16]


def main() -> None:
    master = Image.open(MASTER).convert("RGBA")
    if master.width != master.height:
        raise SystemExit(f"master artwork must be square, got {master.size}")
    master.resize((512, 512), Image.LANCZOS).save(OUT)
    SIZES_DIR.mkdir(exist_ok=True)
    for size in SIZES:
        master.resize((size, size), Image.LANCZOS).save(SIZES_DIR / f"zaprust-{size}.png")
    print(f"wrote {OUT} (512x512) and {len(SIZES)} sizes in {SIZES_DIR}")


if __name__ == "__main__":
    main()
