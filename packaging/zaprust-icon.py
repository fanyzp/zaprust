#!/usr/bin/env python3
"""Generate the Zaprust app icon rasters from the SVG master.

The master artwork is packaging/icon.svg: a green rounded-square
tile with a white speech bubble (bottom-left tail) and three dots
inside the bubble in the tile green. This script renders it to
the shipped rasters:

- packaging/icon.png (1024x1024), the PNG master
- packaging/zaprust.png (512x512), the icon the installers ship
- packaging/icons/zaprust-<size>.png for the standard sizes
  256, 128, 64, 48, 32, 24, and 16

Each size is rendered directly from the SVG so small sizes stay
crisp. Usage: python3 packaging/zaprust-icon.py
Requires: cairosvg (pip install cairosvg) or rsvg-convert.
"""

import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
MASTER = HERE / "icon.svg"
PNG_MASTER = HERE / "icon.png"
OUT = HERE / "zaprust.png"
SIZES_DIR = HERE / "icons"
SIZES = [256, 128, 64, 48, 32, 24, 16]


def render(size: int, dest: Path) -> None:
    try:
        import cairosvg
    except ImportError:
        cairosvg = None
    if cairosvg is not None:
        cairosvg.svg2png(
            url=str(MASTER), write_to=str(dest), output_width=size, output_height=size
        )
        return
    subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), str(MASTER), "-o", str(dest)],
        check=True,
    )


def main() -> None:
    if not any_renderer_available():
        sys.exit(
            "no SVG renderer available: install cairosvg (pip install cairosvg) "
            "or rsvg-convert (librsvg)"
        )
    render(1024, PNG_MASTER)
    render(512, OUT)
    SIZES_DIR.mkdir(exist_ok=True)
    for size in SIZES:
        render(size, SIZES_DIR / f"zaprust-{size}.png")
    print(f"wrote {PNG_MASTER} (1024x1024), {OUT} (512x512) and {len(SIZES)} sizes in {SIZES_DIR}")


def any_renderer_available() -> bool:
    try:
        import cairosvg  # noqa: F401

        return True
    except ImportError:
        return subprocess.run(["which", "rsvg-convert"], capture_output=True).returncode == 0


if __name__ == "__main__":
    main()
