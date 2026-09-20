# Bundled assets

## `alacritree-symbols.ttf`

A subset of DejaVu 2.37 carrying only the glyphs alacritree paints itself, so the sidebar renders on systems whose fonts lack them. Registered last in each chrome font family, so an installed font that already has a glyph keeps rendering it.

Built from two faces because neither covers the whole set: `DejaVuSans.ttf` lacks `⌕` (U+2315), and `DejaVuSansMono.ttf` lacks `⠿` (U+283F) and `⬤` (U+2B24).

The internal family name is `Alacritree Symbols`, not `DejaVu Sans`. The artifact is a derivative and should not be mistaken for the real face.

### Why the multiplexer icons live in plane 16

Registering last is right for a last resort and wrong for an icon this build fits to the row. A Nerd Font maps every private codepoint below U+F1AF0 and most of the geometric shapes besides, so a default spelled as the real character is drawn by whichever `[font] fallback` entry claims it first, at that font's metrics, and the fitting done here is never seen.

Plane 16 is claimed by nothing. Spelling the defaults at U+10FF00 (herdr's ram) and U+10FF01 (zellij's hexagon) leaves this face the only candidate, so it wins from last place with no reordering and no work at the paint sites.

Both are drawn here rather than borrowed from DejaVu. A bisected square was tried for herdr and it reads as a missing-glyph box at row size, however correctly it renders, and U+2B21's hairline stroke closes up once the hexagon is scaled down to a row.

Setting `[integrations.herdr] icon` to an ordinary character is therefore how you opt back into your own fonts for it. The schema publishes the private defaults, so they read as tofu in an editor; each field's documentation names the shape it draws.

A codepoint above U+FFFF needs a format 12 cmap, which `write_cmap` builds beside the format 4 one. `fonts.rs` resolves every private codepoint through `ab_glyph`, the same lookup epaint makes, so a rebuild that dropped format 12 fails the suite instead of the sidebar.

### Regenerating

Needed whenever a glyph is added to `DEFAULT_ICON_GLYPHS` or `CHROME_GLYPHS`, or one of the SVGs beside the font changes. `fonts.rs`'s coverage test fails until this is done, and names the missing codepoint.

Add the codepoint to `build_symbols.py`, then run it against DejaVu 2.37. `--dejavu` defaults to `/usr/share/fonts/truetype/dejavu`:

    uv run alacritree/assets/build_symbols.py --dejavu <dir holding DejaVuSans.ttf and DejaVuSansMono.ttf>

Use Debian's or Kali's DejaVu, which is what the committed font was built from. The copy Windows ships also calls itself 2.37 but draws `◇` differently.

The script subsets both faces, merges them, adds each SVG in `DRAWN` at its plane 16 codepoint, fits it to the box a capital M takes up in DejaVu Sans, sets the names above, and writes over `alacritree-symbols.ttf`, printing each fitted glyph's transform. An SVG carries no font metrics, so unfitted it does not line up with the text beside it. A `fonts.rs` test fails if a rebuild drops the fit.

## `herdr-ram.svg` and `zellij-hexagon.svg`

The sources of the glyphs `[integrations.herdr] icon` and `[integrations.zellij] icon` default to, at U+10FF00 and U+10FF01. Edit either in any vector editor and rebuild the font. The build reads every path, fills it even-odd, and scales it to fit, so the SVG's size and position do not matter, and a shape meant to read as an outline is drawn as two subpaths rather than a stroke.

## `FONT-LICENSE.txt`

The complete upstream DejaVu notice. It must accompany any distribution of
the binary, which is why `alacritree --licenses` prints it.

## `icon-256.png`

The window icon, embedded by `main.rs`.
