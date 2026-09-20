# /// script
# requires-python = ">=3.10"
# dependencies = ["fonttools>=4.47", "skia-pathops>=0.8"]
# ///
"""Build alacritree-symbols.ttf from DejaVu 2.37 and herdr-ram.svg.

Subsets DejaVu Sans and DejaVu Sans Mono down to the glyphs alacritree
paints, merges them, adds the herdr ram, fits the zellij hexagon and the ram
to a capital M's box, and names the result Alacritree Symbols.

    uv run alacritree/assets/build_symbols.py [--dejavu DIR] [--output FILE]
"""

import argparse
import tempfile
from pathlib import Path

import pathops
from fontTools import subset
from fontTools.merge import Merger
from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.cu2quPen import Cu2QuPen
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.svgLib import SVGPath
from fontTools.ttLib import TTFont
from fontTools.ttLib.tables._c_m_a_p import CmapSubtable

# Every codepoint drawn from DejaVu Sans.
SANS = [
    0x002B, 0x00B7, 0x00D7, 0x2014, 0x2022, 0x2026, 0x2191, 0x2193, 0x21BB,
    0x21C5, 0x2302, 0x232B, 0x258C, 0x25AA, 0x25B8, 0x25BE, 0x25C7, 0x25CB,
    0x25CF, 0x25D0, 0x25EB, 0x25EF, 0x2713, 0x283F, 0x2B21, 0x2B24,
]
# DejaVu Sans lacks the magnifier, so it comes from the Mono face.
MONO = [0x2315]

# Public codepoint to the plane 16 one alacritree's defaults are spelled at,
# so no installed face can shadow them. Both stay in the cmap; see README.md.
PRIVATE = {0x25EB: 0x10FF00, 0x2B21: 0x10FF01}

# zellij's sidebar icon.  DejaVu draws it past the ascender and below the
# baseline, so it is refitted to the box a capital M takes up.
HEXAGON = PRIVATE[0x2B21]

# herdr's ram, drawn in herdr-ram.svg.  No Unicode character means it, so it
# exists only at its private codepoint.
RAM = 0x10FF02
RAM_SVG = Path(__file__).with_name("herdr-ram.svg")

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


def fit_to_capital_m(font: TTFont, reference: TTFont, codepoint: int) -> None:
    """Scale a glyph to fit M's height and advance, sit it on the baseline,
    and center it in M's advance.  Whichever of height or width runs out
    first sets the scale, so the shape keeps its proportions."""
    glyph = font.getBestCmap()[codepoint]
    m = reference.getBestCmap()[ord("M")]
    x_min, y_min, x_max, y_max = bounds(font, glyph)
    _, m_bottom, _, m_top = bounds(reference, m)
    m_advance = reference["hmtx"][m][0]

    scale = min((m_top - m_bottom) / (y_max - y_min), m_advance / (x_max - x_min))
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
        f"U+{codepoint:04X}: source bbox {(x_min, y_min, x_max, y_max)}, "
        f"M bbox {bounds(reference, m)} advance {m_advance}, "
        f"scale {scale:.5f}, dx {dx:.1f}, dy {dy:.1f}, "
        f"result bbox {bounds(font, glyph)}"
    )


def add_svg_glyph(font: TTFont, svg: Path, codepoint: int) -> str:
    """Add the SVG's paths as a glyph at their own size, for
    `fit_to_capital_m` to place.  SVG's y axis points down, so the outline is
    flipped, and the SVG's even-odd fill is rewritten as the nonzero winding
    TrueType fills by."""
    path = pathops.Path(fillType=pathops.FillType.EVEN_ODD)
    SVGPath(str(svg)).draw(TransformPen(path.getPen(), (1, 0, 0, -1, 0, 0)))
    path = pathops.simplify(path, clockwise=True)

    name = f"u{codepoint:04X}"
    pen = TTGlyphPen(None)
    path.draw(Cu2QuPen(pen, max_err=1.0, reverse_direction=False))
    font["glyf"][name] = pen.glyph()
    font["glyf"][name].recalcBounds(font["glyf"])
    font["hmtx"][name] = (0, font["glyf"][name].xMin)
    font.setGlyphOrder(font.getGlyphOrder() + [name])
    return name


def write_cmap(font: TTFont, mapping: dict[int, str]) -> None:
    """Replace the cmap with a BMP table plus a full one.  Format 4 cannot
    express a codepoint above U+FFFF, so the private spellings need format 12,
    and a face carrying only format 12 is unreadable to some rasterizers."""
    bmp = CmapSubtable.newSubtable(4)
    bmp.platformID, bmp.platEncID, bmp.language = 3, 1, 0
    bmp.cmap = {cp: name for cp, name in mapping.items() if cp <= 0xFFFF}

    full = CmapSubtable.newSubtable(12)
    full.platformID, full.platEncID, full.language = 3, 10, 0
    full.format, full.reserved, full.length, full.nGroups = 12, 0, 0, 0
    full.cmap = dict(mapping)

    font["cmap"].tableVersion = 0
    font["cmap"].tables = [bmp, full]


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

    mapping = dict(font.getBestCmap())
    mapping[RAM] = add_svg_glyph(font, RAM_SVG, RAM)
    for public, private in PRIVATE.items():
        mapping[private] = mapping[public]
    write_cmap(font, mapping)

    reference = TTFont(args.dejavu / "DejaVuSans.ttf")
    for codepoint in (HEXAGON, RAM):
        fit_to_capital_m(font, reference, codepoint)
    for record in font["name"].names:
        if record.nameID in NAMES:
            record.string = NAMES[record.nameID]
    font.save(args.output)
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
