#!/usr/bin/env python3
"""Generate the Zaprust app icon rasters from the SVG masters.

The master artwork is packaging/icon.svg: the M2 Filled
Monochrome mark, a solid black natural chat bubble with a tail
and three white typing dots as evenodd cutouts.
packaging/icon-white.svg is its exact inverse (white bubble,
the same cutouts) for dark grounds. This script renders both
masters to the shipped rasters:

- packaging/icon.png (1024x1024), the PNG master
- packaging/zaprust.png (512x512), the icon the installers ship
- packaging/icons/zaprust-<size>.png for the standard sizes
  256, 128, 64, 48, 32, 24, and 16 (bare black mark)
- packaging/icons/zaprust-white-<size>.png at the same sizes
  plus 512 (bare white mark on transparency, for in-app dark
  surfaces)
- packaging/icons/zaprust-plate-<size>.png at the same sizes
  plus 512: the white mark centered on a dark rounded plate
  (#282A2C), for OS-facing assets (window icon, desktop and
  dock install, tray) whose panel color the app cannot know.

The plate is a rounded square filling the canvas. Its corner
radius is 20 at the 256 reference size and scales with the
raster size, and the mark sits at 80 percent of the canvas,
centered, so the plate reads as a tile rather than a frame
around a full-bleed mark. The plate SVG is composed from the
white master's own path, so the mark geometry can never drift
between the bare and plated sets.

Each size is rendered directly from the SVG so small sizes stay
crisp. Usage: python3 packaging/zaprust-icon.py
Requires: cairosvg (pip install cairosvg) or rsvg-convert.
"""

import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
MASTER = HERE / "icon.svg"
WHITE_MASTER = HERE / "icon-white.svg"
PNG_MASTER = HERE / "icon.png"
OUT = HERE / "zaprust.png"
SIZES_DIR = HERE / "icons"
SIZES = [256, 128, 64, 48, 32, 24, 16]
EXTRA_SIZES = [512]

PLATE_FILL = "#282A2C"
PLATE_RADIUS_AT_256 = 20.0
MARK_SCALE = 0.8


def render(svg: Path, size: int, dest: Path) -> None:
    try:
        import cairosvg
    except ImportError:
        cairosvg = None
    if cairosvg is not None:
        cairosvg.svg2png(
            url=str(svg), write_to=str(dest), output_width=size, output_height=size
        )
        return
    subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size), str(svg), "-o", str(dest)],
        check=True,
    )


def plate_svg() -> str:
    """Compose the plate artwork from the white master's path.

    The viewBox is the masters' 1000-unit grid: the plate rect
    fills it, its radius is the 256-size radius expressed in
    grid units, and the mark path is scaled about the center.
    """
    text = WHITE_MASTER.read_text(encoding="utf-8")
    match = re.search(r'<path fill="#[0-9A-Fa-f]+" fill-rule="evenodd" d="([^"]+)"/>', text)
    if match is None:
        sys.exit("icon-white.svg: mark path not found in the expected shape")
    radius = PLATE_RADIUS_AT_256 / 256.0 * 1000.0
    offset = (1.0 - MARK_SCALE) / 2.0 * 1000.0
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 1000">\n'
        f'  <rect width="1000" height="1000" rx="{radius:.3f}" fill="{PLATE_FILL}"/>\n'
        f'  <g transform="translate({offset:.3f} {offset:.3f}) scale({MARK_SCALE})">\n'
        f'    <path fill="#FFFFFF" fill-rule="evenodd" d="{match.group(1)}"/>\n'
        "  </g>\n"
        "</svg>\n"
    )


def main() -> None:
    if not any_renderer_available():
        sys.exit(
            "no SVG renderer available: install cairosvg (pip install cairosvg) "
            "or rsvg-convert (librsvg)"
        )
    render(MASTER, 1024, PNG_MASTER)
    render(MASTER, 512, OUT)
    SIZES_DIR.mkdir(exist_ok=True)
    for size in SIZES:
        render(MASTER, size, SIZES_DIR / f"zaprust-{size}.png")
    for size in EXTRA_SIZES + SIZES:
        render(WHITE_MASTER, size, SIZES_DIR / f"zaprust-white-{size}.png")
    plate = HERE / ".zaprust-plate-tmp.svg"
    try:
        plate.write_text(plate_svg(), encoding="utf-8")
        for size in EXTRA_SIZES + SIZES:
            render(plate, size, SIZES_DIR / f"zaprust-plate-{size}.png")
    finally:
        plate.unlink(missing_ok=True)
    total = len(EXTRA_SIZES + SIZES)
    print(
        f"wrote {PNG_MASTER} (1024x1024), {OUT} (512x512), {len(SIZES)} black, "
        f"{total} white and {total} plate sizes in {SIZES_DIR}"
    )


def any_renderer_available() -> bool:
    try:
        import cairosvg  # noqa: F401

        return True
    except ImportError:
        return subprocess.run(["which", "rsvg-convert"], capture_output=True).returncode == 0


if __name__ == "__main__":
    main()
