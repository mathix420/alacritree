# Bundled assets

## `alacritree-symbols.ttf`

A subset of DejaVu 2.37 carrying only the glyphs alacritree paints itself,
so the sidebar renders on systems whose fonts lack them. Registered last in
each chrome font family, so an installed font that already has a glyph keeps
rendering it.

Built from two faces because neither covers the whole set: `DejaVuSans.ttf`
lacks `⌕` (U+2315), and `DejaVuSansMono.ttf` lacks `⠿` (U+283F) and `⬤`
(U+2B24).

The internal family name is `Alacritree Symbols`, not `DejaVu Sans` — the
artifact is a derivative and should not be mistaken for the real face.

### Regenerating

Needed whenever a glyph is added to `DEFAULT_ICON_GLYPHS` or
`CHROME_GLYPHS`. `fonts.rs`'s coverage test fails until this is done, and
names the missing codepoint.

Requires `fonttools` and DejaVu 2.37.

    D=/usr/share/fonts/truetype/dejavu
    U=U+002B,U+00B7,U+00D7,U+2014,U+2022,U+2026,U+2191,U+2193,U+21BB,U+21C5,U+2302,U+232B,U+258C,U+25AA,U+25B8,U+25BE,U+25C7,U+25CB,U+25CF,U+25D0,U+25EB,U+25EF,U+2713,U+283F,U+2B24
    python3 -m fontTools.subset $D/DejaVuSans.ttf     --unicodes="$U"     --output-file=sans.ttf --no-hinting --notdef-outline --drop-tables+=MATH
    python3 -m fontTools.subset $D/DejaVuSansMono.ttf --unicodes="U+2315" --output-file=mono.ttf --no-hinting --notdef-outline

The `MATH` table is dropped from the Sans subset because `fontTools.merge`
cannot combine it across faces; the symbol font has no use for math layout
data anyway.

Then merge with `fontTools.merge.Merger`, set name IDs 1/2/4/6 to
`Alacritree Symbols` / `Regular` / `Alacritree Symbols` /
`AlacritreeSymbols-Regular`, and save over `alacritree-symbols.ttf`.

## `FONT-LICENSE.txt`

The complete upstream DejaVu notice. It must accompany any distribution of
the binary, which is why `alacritree --licenses` prints it.

## `icon-256.png`

The window icon, embedded by `main.rs`.
