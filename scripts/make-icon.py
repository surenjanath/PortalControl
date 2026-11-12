"""Draw the PortalControl icon and write the icon files in assets/.

The same design as web/icon.svg: a violet rounded square, a white portal ring
that is open at the top right, and a yellow dot in the middle.

  assets/icon.ico        Windows executable and tray icon (16 to 256 px)
  assets/icon.icns       macOS application bundle icon
  assets/tray.rgba       32x32 raw RGBA, tray fallback when the .ico is not embedded
  assets/tray-mac.rgba   44x44 raw RGBA, black ring only: a macOS menu-bar
                         "template" image that the system tints itself

Each size is drawn on its own (small sizes get heavier strokes) instead of
being scaled down from one big picture, so the 16 px tray icon stays readable.

Needs Pillow:  python scripts/make-icon.py [preview.png]
"""

import math
import sys
from pathlib import Path

from PIL import Image, ImageDraw

ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
TOP, BOTTOM = (167, 139, 250), (109, 40, 217)
WHITE, YELLOW, BLACK = (255, 255, 255, 255), (252, 212, 82, 255), (0, 0, 0, 255)
OVERSAMPLE = 8


def draw(size: int, pad: float = 0.0, template: bool = False) -> Image.Image:
    """One icon of `size` pixels. `pad` is the empty margin as a fraction of the
    size (macOS icons sit inside a margin). `template` draws only the ring and
    dot in black on a transparent background."""
    n = size * OVERSAMPLE
    inner = round(n * (1 - 2 * pad))
    heavy = size <= 24 or template
    img = Image.new("RGBA", (inner, inner), (0, 0, 0, 0))

    if not template:
        # Diagonal gradient, clipped to a rounded square.
        ramp = Image.linear_gradient("L").resize((inner * 2, inner * 2)).rotate(45, resample=Image.BICUBIC)
        ramp = ramp.crop((inner // 2, inner // 2, inner // 2 + inner, inner // 2 + inner))
        fill = Image.composite(Image.new("RGB", (inner, inner), BOTTOM), Image.new("RGB", (inner, inner), TOP), ramp)
        mask = Image.new("L", (inner, inner), 0)
        ImageDraw.Draw(mask).rounded_rectangle((0, 0, inner - 1, inner - 1), radius=inner * 15 / 64, fill=255)
        img.paste(fill, (0, 0), mask)

    d = ImageDraw.Draw(img)
    c = inner / 2
    unit = inner / (46 if template else 64)  # the template has no square, so the ring fills it
    radius = unit * (17.5 if heavy else 17)
    width = unit * (8.5 if heavy else 6.5)
    ring = BLACK if template else WHITE
    start, end = -10, 280  # degrees, clockwise from 3 o'clock
    outer = radius + width / 2
    d.arc((c - outer, c - outer, c + outer, c + outer), start, end, fill=ring, width=round(width))
    for angle in (start, end):  # round caps
        x = c + radius * math.cos(math.radians(angle))
        y = c + radius * math.sin(math.radians(angle))
        d.ellipse((x - width / 2, y - width / 2, x + width / 2, y + width / 2), fill=ring)
    dot = unit * (7.5 if heavy else 6.5)
    d.ellipse((c - dot, c - dot, c + dot, c + dot), fill=BLACK if template else YELLOW)

    canvas = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    canvas.alpha_composite(img, ((n - inner) // 2, (n - inner) // 2))
    return canvas.resize((size, size), Image.LANCZOS)


def main() -> None:
    assets = Path(__file__).resolve().parent.parent / "assets"
    assets.mkdir(exist_ok=True)

    ico = [draw(s) for s in ICO_SIZES]
    ico[-1].save(assets / "icon.ico", format="ICO", sizes=[(s, s) for s in ICO_SIZES], append_images=ico[:-1])
    draw(1024, pad=0.1).save(assets / "icon.icns", format="ICNS")
    (assets / "tray.rgba").write_bytes(draw(32).tobytes())
    mac = draw(44, pad=0.06, template=True)
    (assets / "tray-mac.rgba").write_bytes(mac.tobytes())
    for f in sorted(assets.iterdir()):
        print(f"{f.name:14} {f.stat().st_size:>8} bytes")

    if len(sys.argv) > 1:  # optional preview sheet
        shown = ico + [mac]
        sheet = Image.new("RGBA", (sum(i.width for i in shown) + 12 * (len(shown) + 1), 280), (40, 40, 40, 255))
        x = 12
        for im in shown:
            if im is mac:  # show the template the way a light menu bar would
                sheet.paste((235, 235, 235, 255), (x - 4, 8, x + im.width + 4, 16 + im.height))
            sheet.alpha_composite(im, (x, 12))
            x += im.width + 12
        sheet.save(sys.argv[1])


if __name__ == "__main__":
    main()
