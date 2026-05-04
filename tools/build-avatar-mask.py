#!/usr/bin/env python3
"""Generate avatar circular-mask PNGs. Two flavors:
- `avatar_mask_96.png`       — BACKGROUND-colored corners (default rows)
- `avatar_mask_field_96.png` — FIELD_BG-colored corners (selected rows)

The center is a transparent disk in both. Compositing the appropriate
mask on top of a square-rendered avatar makes the corners disappear,
giving a circular appearance — matched to the row's background tint.

Re-run if either theme color changes.
Requires Pillow (`pip install pillow`).
"""

from pathlib import Path

from PIL import Image, ImageDraw

# Matches `bsky_render::theme::BACKGROUND` (dark slate).
BG_RGB = (0x0F, 0x17, 0x2A)
# Matches `bsky_render::theme::FIELD_BG` (slightly lighter slate).
FIELD_BG_RGB = (0x1E, 0x29, 0x40)

SIZE = 96  # base mask size; scales down to 48 for timeline cleanly via Lanczos.

OUT_DIR = Path(__file__).resolve().parent.parent / "app" / "static"


def make_mask(corner_rgb):
    img = Image.new("RGBA", (SIZE, SIZE), corner_rgb + (255,))
    draw = ImageDraw.Draw(img)
    draw.ellipse([(0, 0), (SIZE, SIZE)], fill=(0, 0, 0, 0))
    return img


def main() -> int:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    bg_path = OUT_DIR / "avatar_mask_96.png"
    field_path = OUT_DIR / "avatar_mask_field_96.png"
    make_mask(BG_RGB).save(bg_path, optimize=True)
    print(f"Wrote {bg_path} ({bg_path.stat().st_size} bytes)")
    make_mask(FIELD_BG_RGB).save(field_path, optimize=True)
    print(f"Wrote {field_path} ({field_path.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
