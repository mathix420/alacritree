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

Add the codepoint to `build_symbols.py`, then run it against DejaVu 2.37. `--dejavu` defaults to `/usr/share/fonts/truetype/dejavu`:

    uv run alacritree/assets/build_symbols.py --dejavu <dir holding DejaVuSans.ttf and DejaVuSansMono.ttf>

The script subsets both faces, merges them, refits zellij's hexagon (U+2B21) to the box a capital M takes up in DejaVu Sans, sets the names above, and writes over `alacritree-symbols.ttf`, printing the hexagon's transform. DejaVu draws that hexagon past the ascender and below the baseline, so unfitted it does not line up with the text beside it. A `fonts.rs` test fails if a rebuild drops the refit.

## `FONT-LICENSE.txt`

The complete upstream DejaVu notice. It must accompany any distribution of
the binary, which is why `alacritree --licenses` prints it.

## `icon-256.png`

The window icon, embedded by `main.rs`.
