#!/usr/bin/env python3
"""Build the Twemoji color-emoji atlas + Rust codepoint table.

Downloads Twemoji 72×72 PNGs from jsdelivr's twitter/twemoji mirror,
resizes to 64×64, packs them into a 16-column PNG sprite atlas at
`app/static/twemoji.png`, and emits a sorted Rust lookup table at
`crates/bsky-render/src/emoji_table.rs`.

Run: python tools/build-twemoji.py

Requires `pillow` and `requests` (`pip install pillow requests`).
Internet access required for the jsdelivr fetches; about 500 small
downloads, ~30 seconds total.

Re-run only when you want to expand or change the emoji set —
generated files are checked into the repo so end users don't need
Python at build time.
"""

import io
import os
import sys
from pathlib import Path

import requests
from PIL import Image

# ─── Codepoint set ──────────────────────────────────────────────────────────
#
# We aim for ~512 well-known emoji. Codepoints are organized by category for
# readability; the script flattens, dedupes, sorts, and skips any codepoint
# Twemoji's CDN doesn't have a glyph for (404). Final atlas size depends on
# how many of these resolve.

RANGES: list[tuple[int, int]] = [
    # Faces & emoticons
    (0x1F600, 0x1F650),  # 80 — Emoticons block (smileys)
    (0x1F910, 0x1F930),  # 32 — Faces with hand, money mouth, etc.
    (0x1F970, 0x1F980),  # 16 — Smiling with hearts, partying, woozy, hot, cold
    (0x1F47B, 0x1F480),  # 5  — Ghost, alien, robot, skull, poo
    (0x1F636, 0x1F640),  # (overlap with emoticons; deduped)
    # Hands
    (0x1F44A, 0x1F450),  # 6  — Fist, OK, thumbs, etc.
    (0x1F590, 0x1F596),  # 6  — Hand variants
    (0x1F918, 0x1F920),  # 8  — Rock-on, vulcan, etc.
    (0x1F932, 0x1F94A),  # ~24 — Crossed fingers, palms, handshake, pinch
    # Hearts & kisses
    (0x1F491, 0x1F49F),  # 14 — Heart variants
    (0x1F90D, 0x1F910),  # 3  — White, brown, black hearts (color hearts)
    (0x2763, 0x2766),    # 3  — Heart exclamation, decorative
    # Animals
    (0x1F40F, 0x1F44A),  # 59 — Sheep, dog, cat, monkey, etc.
    (0x1F980, 0x1F9B0),  # 48 — Lobster, shrimp, scorpion, etc.
    # Food & drink
    (0x1F32D, 0x1F37F),  # 82 — Hot dog, taco, ramen, drinks, etc.
    (0x1F950, 0x1F970),  # 32 — Croissant, bagel, cut of meat, etc.
    # Plants & nature
    (0x1F330, 0x1F33F),  # 16 — Trees, flowers, etc.
    # Weather, sky, planets
    (0x1F300, 0x1F320),  # 32 — Cyclone, earth, moon phases
    (0x2600, 0x2620),    # 32 — Sun, cloud, comet, snowflake, peace
    # Transport
    (0x1F680, 0x1F6A0),  # 32 — Rocket, train, car, bike
    (0x1F6A2, 0x1F6B0),  # 14 — Ship, scooter, etc.
    # Activities & sports
    (0x1F3A0, 0x1F3D0),  # 48 — Carousel, ball games, performing arts
    (0x26BD, 0x26C0),    # 3  — Soccer, baseball, snowman
    # Misc objects
    (0x1F4A1, 0x1F4B0),  # 15 — Lightbulb, gem, money, etc.
    (0x1F4DA, 0x1F4F8),  # 30 — Books, computer, tools, gears
    (0x1F525, 0x1F526),  # 1  — 🔥
    (0x1F4AF, 0x1F4B0),  # 1  — 💯
    # Time
    (0x1F550, 0x1F568),  # 24 — Clocks
    # Symbols (popular ones)
    (0x2705, 0x2706),    # 1  — ✅
    (0x274C, 0x274F),    # 3  — ❌ ❎ ❏
    (0x2728, 0x2729),    # 1  — ✨
    (0x2B50, 0x2B51),    # 1  — ⭐
]

EXTRAS: list[int] = [
    0x2764,        # ❤
    0x1F308,       # 🌈
    0x1F31F,       # 🌟
    0x1F389, 0x1F38A,  # 🎉 🎊
    0x1F4A5, 0x1F4A8, 0x1F4AB, 0x1F4AC, 0x1F4AD,  # 💥 💨 💫 💬 💭
    0x26A1,        # ⚡
    0x2614,        # ☔
    0x26C5,        # ⛅
    0x2B55,        # ⭕
    0x261D,        # ☝
    0x1F44F,       # 👏
    0x1F44D, 0x1F44E,  # 👍 👎
    0x1F4F1,       # 📱
    0x1F4BB,       # 💻
    0x1F4F7,       # 📷
    0x1F30D, 0x1F30E, 0x1F30F,  # 🌍 🌎 🌏
]

# ─── Atlas / output config ─────────────────────────────────────────────────
ATLAS_COLS = 16
CELL_PX = 64
TWEMOJI_BASE = "https://cdn.jsdelivr.net/gh/twitter/twemoji@latest/assets/72x72"

# Resolve repo root from this script's location.
SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
OUT_PNG = REPO_ROOT / "app" / "static" / "twemoji.png"
OUT_RS = REPO_ROOT / "crates" / "bsky-render" / "src" / "emoji_table.rs"


def collect_codepoints() -> list[int]:
    """Flatten RANGES + EXTRAS, dedupe, sort ascending."""
    cps: set[int] = set()
    for start, end in RANGES:
        for cp in range(start, end):
            cps.add(cp)
    for cp in EXTRAS:
        cps.add(cp)
    return sorted(cps)


def fetch_glyph(session: requests.Session, codepoint: int) -> Image.Image | None:
    """Fetch the Twemoji 72×72 PNG for `codepoint`. Returns None on 404 or
    decode failure (the codepoint isn't in Twemoji's set)."""
    url = f"{TWEMOJI_BASE}/{codepoint:x}.png"
    try:
        r = session.get(url, timeout=10)
    except requests.RequestException as e:
        print(f"  {codepoint:#x}: network error: {e}", file=sys.stderr)
        return None
    if r.status_code == 404:
        return None
    if r.status_code != 200:
        print(f"  {codepoint:#x}: HTTP {r.status_code}", file=sys.stderr)
        return None
    try:
        return Image.open(io.BytesIO(r.content)).convert("RGBA")
    except Exception as e:
        print(f"  {codepoint:#x}: decode failed: {e}", file=sys.stderr)
        return None


def main() -> int:
    requested = collect_codepoints()
    print(f"Requested {len(requested)} codepoints; downloading…")

    session = requests.Session()
    glyphs: list[tuple[int, Image.Image]] = []  # (codepoint, glyph_image)
    misses: list[int] = []

    for i, cp in enumerate(requested, 1):
        if i % 50 == 0:
            print(f"  {i}/{len(requested)} fetched ({len(misses)} misses so far)")
        glyph = fetch_glyph(session, cp)
        if glyph is None:
            misses.append(cp)
            continue
        glyphs.append((cp, glyph.resize((CELL_PX, CELL_PX), Image.LANCZOS)))

    print(f"Got {len(glyphs)} glyphs ({len(misses)} not in Twemoji set).")
    if not glyphs:
        print("ERROR: zero glyphs fetched; check network / CDN URL.", file=sys.stderr)
        return 1

    # Build the atlas: 16 cols, ceil(N/16) rows.
    n = len(glyphs)
    rows = (n + ATLAS_COLS - 1) // ATLAS_COLS
    atlas = Image.new("RGBA", (ATLAS_COLS * CELL_PX, rows * CELL_PX))
    table: list[tuple[int, int, int]] = []  # (codepoint, col, row)

    for i, (cp, glyph) in enumerate(glyphs):
        col = i % ATLAS_COLS
        row = i // ATLAS_COLS
        atlas.paste(glyph, (col * CELL_PX, row * CELL_PX))
        table.append((cp, col, row))

    OUT_PNG.parent.mkdir(parents=True, exist_ok=True)
    atlas.save(OUT_PNG, optimize=True)
    print(f"Wrote {OUT_PNG} ({atlas.size[0]}×{atlas.size[1]}, {OUT_PNG.stat().st_size / 1024:.1f} KB)")

    # Emit Rust source. Codepoints are sorted ascending so consumers can
    # binary-search; col/row follow insertion order, not codepoint order.
    table.sort(key=lambda t: t[0])
    OUT_RS.parent.mkdir(parents=True, exist_ok=True)
    with OUT_RS.open("w") as f:
        f.write("// @generated by tools/build-twemoji.py — do not edit by hand.\n")
        f.write("// Codepoints sorted ascending for binary-search lookup.\n\n")
        f.write(f"pub const ATLAS_COLS: u16 = {ATLAS_COLS};\n")
        f.write(f"pub const ATLAS_ROWS: u16 = {rows};\n")
        f.write(f"pub const CELL_PX: u16 = {CELL_PX};\n\n")
        f.write("/// `(codepoint, atlas_col, atlas_row)`. Cell pixel coords are\n")
        f.write("/// `(col * CELL_PX, row * CELL_PX)` of the atlas PNG.\n")
        f.write("pub static EMOJI_TABLE: &[(u32, u16, u16)] = &[\n")
        for cp, col, row in table:
            f.write(f"    ({cp:#08x}, {col:>2}, {row:>2}),\n")
        f.write("];\n\n")
        f.write("/// Look up a codepoint's atlas cell. `None` if not in the bundled set.\n")
        f.write("pub fn lookup(codepoint: u32) -> Option<(u16, u16)> {\n")
        f.write("    EMOJI_TABLE\n")
        f.write("        .binary_search_by_key(&codepoint, |&(cp, _, _)| cp)\n")
        f.write("        .ok()\n")
        f.write("        .map(|i| (EMOJI_TABLE[i].1, EMOJI_TABLE[i].2))\n")
        f.write("}\n")
    print(f"Wrote {OUT_RS} ({len(table)} entries)")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
