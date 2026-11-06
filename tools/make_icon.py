"""Build the app icon from the approved artwork (dev tool, not run by the build).

Usage: python tools/make_icon.py [preview.png]
Reads assets/icon-source.jpg (full-bleed square artwork, generated 2026-11-05 with
Gemini from the Cerberus Lab logo and approved by the maintainer), masks it to a
rounded square and writes assets/icon.png (256x256) and assets/icon.ico
(16/24/32/48/256).
"""
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ICO_SIZES = [16, 24, 32, 48, 256]
CORNER_FRACTION = 0.22  # corner radius relative to the edge length
SHARPEN_MAX = 32  # sizes up to this get an extra unsharp mask
SUPERSAMPLE = 4

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"


def rounded(img: Image.Image) -> Image.Image:
    n = img.width
    big = n * SUPERSAMPLE
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, big - 1, big - 1], radius=int(big * CORNER_FRACTION), fill=255
    )
    out = img.convert("RGBA")
    out.putalpha(mask.resize((n, n), Image.Resampling.LANCZOS))
    return out


def sized(base: Image.Image, n: int) -> Image.Image:
    out = base.resize((n, n), Image.Resampling.LANCZOS)
    if n <= SHARPEN_MAX:
        out = out.filter(ImageFilter.UnsharpMask(radius=1, percent=110, threshold=1))
    return out


def preview(base: Image.Image, path: Path) -> None:
    pad = 16
    width = sum(n + pad for n in ICO_SIZES) + pad
    sheet = Image.new("RGBA", (width, (256 + 2 * pad) * 2), (10, 20, 36, 255))
    ImageDraw.Draw(sheet).rectangle([0, 256 + 2 * pad, width, sheet.height], fill=(238, 241, 245, 255))
    for row in range(2):
        x, y0 = pad, row * (256 + 2 * pad)
        for n in ICO_SIZES:
            sheet.alpha_composite(sized(base, n), (x, y0 + pad + 256 - n))
            x += n + pad
    sheet.save(path)


def main() -> None:
    src = Image.open(ASSETS / "icon-source.jpg").convert("RGB")
    side = min(src.size)
    src = src.crop(((src.width - side) // 2, (src.height - side) // 2,
                    (src.width + side) // 2, (src.height + side) // 2))
    base = rounded(src.resize((256, 256), Image.Resampling.LANCZOS))
    base.save(ASSETS / "icon.png")
    frames = [sized(base, n) for n in ICO_SIZES]
    frames[-1].save(ASSETS / "icon.ico", sizes=[(n, n) for n in ICO_SIZES], append_images=frames[:-1])
    if len(sys.argv) > 1:
        preview(base, Path(sys.argv[1]))


if __name__ == "__main__":
    main()
