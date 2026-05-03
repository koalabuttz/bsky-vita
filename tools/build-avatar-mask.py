#!/usr/bin/env python3
"""Generate `app/static/avatar_mask_96.png` — a 96×96 RGBA mask used to
fake circular avatars on Vita. The center is a transparent disk; the
four corners are filled with `theme::BACKGROUND` color. Compositing the
mask on top of a square-rendered avatar makes the corners disappear,
giving a circular appearance.

Re-run only if BACKGROUND theme color changes.

Requires Pillow (`pip install pillow`).
"""

from pathlib import Path

from PIL import Image, ImageDraw

# Matches `bsky_render::theme::BACKGROUND` (0x0F172A — dark slate).
BG_RGB = (0x0F, 0x17, 0x2A)

SIZE = 96  # base mask size; scales down to 48 for timeline cleanly via Lanczos.

OUT = Path(__file__).resolve().parent.parent / "app" / "static" / "avatar_mask_96.png"


def main() -> int:
    img = Image.new("RGBA", (SIZE, SIZE), BG_RGB + (255,))
    draw = ImageDraw.Draw(img)
    # Transparent inscribed circle. Pillow's antialiasing on `ellipse`
    # gives a soft edge that blends nicely with the avatar underneath.
    draw.ellipse([(0, 0), (SIZE, SIZE)], fill=(0, 0, 0, 0))
    OUT.parent.mkdir(parents=True, exist_ok=True)
    img.save(OUT, optimize=True)
    print(f"Wrote {OUT} ({OUT.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
