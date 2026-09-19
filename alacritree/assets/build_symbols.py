# /// script
# requires-python = ">=3.10"
# dependencies = ["fonttools>=4.47"]
# ///
"""Build alacritree-symbols.ttf from DejaVu 2.37.

Subsets DejaVu Sans and DejaVu Sans Mono down to the glyphs alacritree
paints, merges them, fits the zellij hexagon to a capital M's box, and names
the result Alacritree Symbols.

    uv run alacritree/assets/build_symbols.py [--dejavu DIR] [--output FILE]
"""

import argparse
import tempfile
from pathlib import Path

from fontTools import subset
from fontTools.merge import Merger
from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont

# Every codepoint drawn from DejaVu Sans.
SANS = [
    0x002B, 0x00B7, 0x00D7, 0x2014, 0x2022, 0x2026, 0x2191, 0x2193, 0x21BB,
    0x21C5, 0x2302, 0x232B, 0x258C, 0x25AA, 0x25B8, 0x25BE, 0x25C7, 0x25CB,
    0x25CF, 0x25D0, 0x25EB, 0x25EF, 0x2713, 0x283F, 0x2B21, 0x2B24,
]
# DejaVu Sans lacks the magnifier, so it comes from the Mono face.
MONO = [0x2315]

# zellij's sidebar icon.  DejaVu draws it past the ascender and below the
# baseline, so it is refitted to the box a capital M takes up.
HEXAGON = 0x2B21

NAMES = {
    1: "Alacritree Symbols",
    2: "Regular",
    4: "Alacritree Symbols",
    6: "AlacritreeSymbols-Regular",
}


def subset_face(source: Path, unicodes: list[int], output: Path, drop_math: bool) -> None:
    options = subset.Options()
    options.hinting = False
    options.notdef_outline = True
    if drop_math:
        # fontTools.merge cannot combine MATH across faces, and the symbol
        # font has no use for math layout.
        options.drop_tables += ["MATH"]
    font = subset.load_font(str(source), options)
    subsetter = subset.Subsetter(options)
    subsetter.populate(unicodes=unicodes)
    subsetter.subset(font)
    subset.save_font(font, str(output), options)


def bounds(font: TTFont, glyph: str) -> tuple[float, float, float, float]:
    glyphs = font.getGlyphSet()
    pen = BoundsPen(glyphs)
    glyphs[glyph].draw(pen)
    return pen.bounds


def fit_to_capital_m(font: TTFont, reference: TTFont) -> None:
    """Scale the hexagon to M's height, sit it on the baseline, and center it
    in M's advance.  A regular hexagon is narrower than M, so the height sets
    the scale and the width follows from it."""
    glyph = font.getBestCmap()[HEXAGON]
    m = reference.getBestCmap()[ord("M")]
    x_min, y_min, x_max, y_max = bounds(font, glyph)
    _, m_bottom, _, m_top = bounds(reference, m)
    m_advance = reference["hmtx"][m][0]

    scale = (m_top - m_bottom) / (y_max - y_min)
    width = (x_max - x_min) * scale
    dx = (m_advance - width) / 2 - x_min * scale
    dy = m_bottom - y_min * scale

    glyphs = font.getGlyphSet()
    pen = TTGlyphPen(glyphs)
    glyphs[glyph].draw(TransformPen(pen, (scale, 0, 0, scale, dx, dy)))
    font["glyf"][glyph] = pen.glyph()
    font["glyf"][glyph].recalcBounds(font["glyf"])
    font["hmtx"][glyph] = (m_advance, font["glyf"][glyph].xMin)
    print(
        f"U+{HEXAGON:04X}: source bbox {(x_min, y_min, x_max, y_max)}, "
        f"M bbox {bounds(reference, m)} advance {m_advance}, "
        f"scale {scale:.5f}, dx {dx:.1f}, dy {dy:.1f}, "
        f"result bbox {bounds(font, glyph)}"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dejavu", type=Path, default=Path("/usr/share/fonts/truetype/dejavu"))
    parser.add_argument(
        "--output", type=Path, default=Path(__file__).with_name("alacritree-symbols.ttf")
    )
    args = parser.parse_args()

    with tempfile.TemporaryDirectory() as tmp:
        sans, mono = Path(tmp, "sans.ttf"), Path(tmp, "mono.ttf")
        subset_face(args.dejavu / "DejaVuSans.ttf", SANS, sans, drop_math=True)
        subset_face(args.dejavu / "DejaVuSansMono.ttf", MONO, mono, drop_math=False)
        font = Merger().merge([str(sans), str(mono)])

    fit_to_capital_m(font, TTFont(args.dejavu / "DejaVuSans.ttf"))
    for record in font["name"].names:
        if record.nameID in NAMES:
            record.string = NAMES[record.nameID]
    font.save(args.output)
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
