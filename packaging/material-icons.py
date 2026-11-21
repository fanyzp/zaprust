#!/usr/bin/env python3
"""Build the Material icon font bundled with the app.

Downloads Google's Material Symbols Rounded variable font from the
google/material-design-icons GitHub repo, pins one static instance
(FILL 0, GRAD 0, opsz 24, wght 500, the friendly outline look at a
medium weight), then subsets it to just the codepoints the app uses
and writes the result to crates/app/assets/fonts/material-symbols.ttf.

Every codepoint is resolved at run time from the font's own
.codepoints file shipped in the same repo, never from a hand written
table, so the map printed at the end is guaranteed to match the font.

If the Rounded (variable font) route fails for any reason, the
script falls back to the classic MaterialIcons-Regular.ttf and its
.codepoints file from the font/ directory of the same repo. Classic
icons have no "database" glyph, so that variant uses "storage"
there; the printed map shows exactly what was used.

Usage: python3 packaging/material-icons.py
Requires: fonttools (pip install fonttools) and network access.
"""

import io
import sys
import tempfile
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
OUT_PATH = REPO_ROOT / "crates" / "app" / "assets" / "fonts" / "material-symbols.ttf"

RAW = "https://raw.githubusercontent.com/google/material-design-icons/master"
ROUNDED_BASE = RAW + "/variablefont/MaterialSymbolsRounded%5BFILL%2CGRAD%2Copsz%2Cwght%5D"
CLASSIC_BASE = RAW + "/font/MaterialIcons-Regular"

# Icon enum variant -> candidate Material names, most preferred first.
# The first candidate present in the source font's codepoints file
# wins; the script reports which one it used.
WANTED = [
    ("Chat", ["chat_bubble"]),
    ("Megaphone", ["campaign"]),
    ("Users", ["group"]),
    ("Blocks", ["extension", "widgets"]),
    ("Sliders", ["tune"]),
    ("Search", ["search"]),
    ("Plus", ["add"]),
    ("Smile", ["sentiment_satisfied", "emoji_emotions"]),
    ("Mic", ["mic"]),
    ("MicOff", ["mic_off"]),
    ("Phone", ["call"]),
    ("Video", ["videocam"]),
    ("X", ["close"]),
    ("Contrast", ["contrast"]),
    ("Bell", ["notifications"]),
    ("Shield", ["shield"]),
    ("Database", ["database", "storage"]),
    ("Info", ["info"]),
    ("Check", ["check"]),
    ("ChevronLeft", ["chevron_left"]),
    ("ChevronRight", ["chevron_right"]),
    ("Sparkles", ["auto_awesome"]),
    ("Group", ["groups"]),
    ("Play", ["play_arrow"]),
    ("Pause", ["pause"]),
    ("ChevronDown", ["expand_more", "keyboard_arrow_down"]),
    ("Reply", ["reply"]),
    ("Copy", ["content_copy"]),
    ("Translate", ["translate"]),
    ("Transcribe", ["transcribe", "record_voice_over"]),
    ("Edit", ["edit"]),
    ("Delete", ["delete"]),
    ("Forward", ["forward"]),
    ("Star", ["star"]),
    ("Pin", ["push_pin"]),
    ("Download", ["download"]),
    ("Poll", ["poll"]),
    ("Summarize", ["summarize"]),
    ("Schedule", ["schedule"]),
    ("React", ["add_reaction", "sentiment_satisfied"]),
    ("Person", ["person"]),
    ("Archive", ["archive"]),
    ("MoreVert", ["more_vert"]),
    ("Menu", ["menu"]),
    ("Image", ["image"]),
    ("Document", ["description", "insert_drive_file"]),
    ("LocationOn", ["location_on"]),
    ("Stop", ["stop"]),
    ("LooksOne", ["looks_one"]),
    ("BellOff", ["notifications_off"]),
    ("Keyboard", ["keyboard"]),
    ("Status", ["donut_large", "trip_origin", "radio_button_checked"]),
    ("AddCall", ["add_call"]),
    ("CallMade", ["call_made"]),
    ("CallReceived", ["call_received"]),
    ("CallMissed", ["call_missed"]),
]

# Variable font axes pinned for the Rounded route.
AXES = {"FILL": 0, "GRAD": 0, "opsz": 24, "wght": 500}


def fetch(url):
    req = urllib.request.Request(url, headers={"User-Agent": "zaprust-icon-build"})
    with urllib.request.urlopen(req, timeout=300) as resp:
        return resp.read()


def parse_codepoints(text):
    """Parse a .codepoints file ('name hex' per line) into a dict."""
    table = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2:
            table[parts[0]] = int(parts[1], 16)
    return table


def resolve(table):
    """Map every wanted variant to (material name, codepoint)."""
    resolved = {}
    for variant, candidates in WANTED:
        for name in candidates:
            if name in table:
                resolved[variant] = (name, table[name])
                break
        else:
            raise KeyError(
                f"no candidate for {variant} found in codepoints file: {candidates}"
            )
    return resolved


def subset_font(font, codepoints):
    """Subset `font` in place to the given unicode codepoints."""
    from fontTools import subset

    opts = subset.Options()
    # Icons are addressed by codepoint, not by ligature text, so no
    # layout features are needed. Hinting is dropped too: egui's
    # rasterizer does not use TrueType instructions.
    opts.layout_features = []
    opts.hinting = False
    opts.desubroutinize = True
    opts.name_IDs = []
    opts.name_languages = []
    opts.notdef_outline = True
    subsetter = subset.Subsetter(options=opts)
    subsetter.populate(unicodes=sorted(codepoints))
    subsetter.subset(font)


def build_rounded(workdir):
    """Preferred route: Material Symbols Rounded, one pinned instance."""
    from fontTools import ttLib
    from fontTools.varLib import instancer

    table = parse_codepoints(fetch(ROUNDED_BASE + ".codepoints").decode("utf-8"))
    resolved = resolve(table)
    raw = fetch(ROUNDED_BASE + ".ttf")
    font = ttLib.TTFont(io.BytesIO(raw))
    instancer.instantiateVariableFont(font, AXES, inplace=True)
    subset_font(font, [cp for _, cp in resolved.values()])
    tmp = workdir / "rounded.ttf"
    font.save(tmp)
    return tmp, resolved, "Material Symbols Rounded (variable, pinned FILL=0 GRAD=0 opsz=24 wght=500)"


def build_classic(workdir):
    """Fallback route: classic MaterialIcons-Regular.ttf."""
    from fontTools import ttLib

    table = parse_codepoints(fetch(CLASSIC_BASE + ".codepoints").decode("utf-8"))
    resolved = resolve(table)
    raw = fetch(CLASSIC_BASE + ".ttf")
    font = ttLib.TTFont(io.BytesIO(raw))
    subset_font(font, [cp for _, cp in resolved.values()])
    tmp = workdir / "classic.ttf"
    font.save(tmp)
    return tmp, resolved, "Material Icons classic (MaterialIcons-Regular.ttf)"


def verify(path, resolved):
    """Reopen the written font and check every codepoint survived."""
    from fontTools import ttLib

    font = ttLib.TTFont(path)
    cmap = font.getBestCmap()
    missing = [
        f"{variant} U+{cp:04X}"
        for variant, (_, cp) in resolved.items()
        if cp not in cmap
    ]
    if missing:
        raise RuntimeError(f"codepoints missing from output font: {missing}")


def main():
    try:
        from fontTools import ttLib  # noqa: F401
    except ImportError:
        sys.exit("fonttools is required: pip install fonttools")

    with tempfile.TemporaryDirectory() as td:
        workdir = Path(td)
        try:
            built, resolved, source = build_rounded(workdir)
        except Exception as exc:
            print(f"Rounded route failed ({exc}); falling back to classic icons")
            built, resolved, source = build_classic(workdir)
        data = built.read_bytes()

    OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUT_PATH.write_bytes(data)
    verify(OUT_PATH, resolved)

    print(f"source font: {source}")
    print(f"output: {OUT_PATH} ({len(data)} bytes)")
    print("variant -> material name -> codepoint:")
    for variant, _ in WANTED:
        name, cp = resolved[variant]
        print(f"  {variant:<13} {name:<20} U+{cp:04X}")
    substituted = [
        f"{variant} uses {resolved[variant][0]}"
        for variant, candidates in WANTED
        if resolved[variant][0] != candidates[0]
    ]
    if substituted:
        print("substitutions: " + "; ".join(substituted))


if __name__ == "__main__":
    main()
