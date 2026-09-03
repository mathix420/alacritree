//! Rasterize emoji from a font's colour tables.
//!
//! egui draws `glyf`/`CFF` outlines and nothing else.  Colour emoji fonts keep
//! their artwork in COLR, CBDT, sbix or SVG tables; the ones that also carry
//! monochrome outlines (Segoe UI Emoji) come out as black-and-white silhouettes,
//! and the ones that don't (Twemoji, Noto Color Emoji) come out as blank cells,
//! because egui still claims every codepoint their cmap covers.  Upstream
//! alacritty has no such gap — crossfont loads glyphs through FreeType with
//! `FT_LOAD_COLOR` and uploads RGBA bitmaps — so this restores parity for the
//! egui renderer.
//!
//! Characters are resolved against the same fallback chain, in the same order,
//! that `fonts::install_terminal_fonts` handed to egui.  Resolving against a
//! different order would rasterize from a font egui never considered, which is
//! the sort of divergence that only shows up as one wrong-looking glyph months
//! later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use swash::FontRef;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};

use crate::builtin_font::Metrics;
use crate::fonts::ChainFace;

/// A rasterized colour glyph, already scaled and centred within its cell box.
/// Offsets and dimensions are device pixels, matching `builtin_font`.
pub struct CachedColorGlyph {
    pub texture: TextureHandle,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl CachedColorGlyph {
    fn bytes(&self) -> usize {
        self.width.max(0) as usize * self.height.max(0) as usize * 4
    }
}

pub struct ColorGlyphCache {
    /// The fallback chain in egui's own consultation order.
    chain: Vec<ChainFace>,
    /// Font files behind the chain, borrowed from the mappings `fonts` already
    /// holds.  A `None` marks a file that would not map, so a broken font is
    /// not retried on every cache miss.
    files: HashMap<PathBuf, Option<&'static [u8]>>,
    /// Which chain entry, if any, draws this character in colour.  `None` means
    /// egui's own glyph pipeline owns it.  The index is what a re-render after
    /// a budget eviction uses instead of walking the chain again.
    source: HashMap<char, Option<usize>>,
    entries: HashMap<char, CachedColorGlyph>,
    /// Monotonic tick per lookup, so eviction can pick the coldest entry
    /// without reordering anything on the hot path.
    used: HashMap<char, u64>,
    clock: u64,
    bytes: usize,
    budget: usize,
    cell_size: (u32, u32),
    scale: ScaleContext,
    /// Chain walks performed, so a test can prove a post-eviction re-render
    /// answers from `source` instead of re-parsing every earlier face's cmap.
    /// `usize` rather than an atomic because `claiming_index` takes `&mut self`.
    #[cfg(test)]
    chain_walks: usize,
}

impl ColorGlyphCache {
    pub fn new(chain: Vec<ChainFace>, budget_mb: usize) -> Self {
        Self {
            chain,
            files: HashMap::new(),
            source: HashMap::new(),
            entries: HashMap::new(),
            used: HashMap::new(),
            clock: 0,
            bytes: 0,
            budget: budget_mb.saturating_mul(1024 * 1024),
            cell_size: (0, 0),
            scale: ScaleContext::new(),
            #[cfg(test)]
            chain_walks: 0,
        }
    }

    /// Get or rasterize the colour glyph for `c`.  `None` means no font in the
    /// chain has colour artwork for it, and egui should paint it as usual.
    ///
    /// `cells` is the character's width in terminal cells, so a double-width
    /// emoji is fitted to the two cells it actually occupies.
    pub fn get(
        &mut self,
        ctx: &Context,
        c: char,
        metrics: &Metrics,
        cells: u32,
    ) -> Option<&CachedColorGlyph> {
        let cell = (metrics.average_advance.round() as u32, metrics.line_height.round() as u32);
        if self.cell_size != cell {
            self.entries.clear();
            self.used.clear();
            self.bytes = 0;
            self.cell_size = cell;
        }
        if cell.0 == 0 || cell.1 == 0 {
            return None;
        }

        self.clock += 1;
        let now = self.clock;

        if self.entries.contains_key(&c) {
            self.used.insert(c, now);
            return self.entries.get(&c);
        }
        // Only the claiming face is considered.  Looking further down the chain
        // would rasterize from a font egui had already passed over, so the two
        // renderers would disagree about which face owns the character.
        let index = match self.source.get(&c) {
            // Known monochrome: egui's own glyph pipeline draws it.  The whole
            // grid takes this path on every frame, so it costs one lookup.
            Some(None) => return None,
            // Re-render after a budget eviction.  The chain is fixed at
            // construction, so the recorded index still names the same face.
            Some(Some(i)) => *i,
            None => match self.claiming_index(c) {
                Some(i) => i,
                None => {
                    self.source.insert(c, None);
                    return None;
                },
            },
        };
        let face = self.chain[index].clone();
        let glyph = self.rasterize(ctx, c, &face, cell, cells.max(1));

        let Some(glyph) = glyph else {
            // The claiming face is an ordinary text font; egui draws it.
            self.source.insert(c, None);
            return None;
        };

        self.source.insert(c, Some(index));
        self.bytes += glyph.bytes();
        self.entries.insert(c, glyph);
        self.used.insert(c, now);
        self.evict_to_budget(c);

        self.entries.get(&c)
    }

    /// Index of the first face whose cmap claims `c` — the same face egui picks.
    fn claiming_index(&mut self, c: char) -> Option<usize> {
        #[cfg(test)]
        {
            self.chain_walks += 1;
        }
        for i in 0..self.chain.len() {
            let face = self.chain[i].clone();
            let Some(data) = load(&mut self.files, &face.path) else {
                continue;
            };
            let claims = FontRef::from_index(data, face.face_index as usize)
                .is_some_and(|font| font.charmap().map(c) != 0);
            if claims {
                return Some(i);
            }
        }
        None
    }

    /// The face egui resolves `c` to, colour or not.  Exists so the no-blank-cell
    /// invariant can be stated over the same face egui would have used.
    #[cfg(test)]
    fn resolve_claiming_face(&mut self, c: char) -> Option<ChainFace> {
        self.claiming_index(c).map(|i| self.chain[i].clone())
    }

    /// Rasterize `c` from `face`, scaled and centred into its `cells`-wide cell
    /// box.  `None` when the face has no colour artwork for the character, which
    /// is the signal to leave it to egui.
    fn render(
        &mut self,
        c: char,
        face: &ChainFace,
        cell: (u32, u32),
        cells: u32,
    ) -> Option<(ColorImage, i32, i32)> {
        let data = load(&mut self.files, &face.path)?;
        let font = FontRef::from_index(data, face.face_index as usize)?;
        let glyph = font.charmap().map(c);

        // Ask for the glyph at the cell's height.  Outline-backed colour glyphs
        // (COLR) honour this exactly; bitmap strikes (CBDT/sbix) come back at
        // whatever fixed size the font ships, so both paths are rescaled below.
        let mut scaler = self.scale.builder(font).size(cell.1 as f32).hint(false).build();
        let image = Render::new(COLOR_SOURCES).render(&mut scaler, glyph)?;
        if image.content != Content::Color {
            return None;
        }

        let (src_w, src_h) = (image.placement.width, image.placement.height);
        if src_w == 0 || src_h == 0 {
            return None;
        }

        let box_w = cell.0 * cells;
        let box_h = cell.1;
        let fit = (box_w as f32 / src_w as f32).min(box_h as f32 / src_h as f32);
        let dst_w = ((src_w as f32 * fit).round() as u32).max(1);
        let dst_h = ((src_h as f32 * fit).round() as u32).max(1);

        let pixels = scale_rgba(&image.data, src_w, src_h, dst_w, dst_h);
        let color_image =
            ColorImage::from_rgba_unmultiplied([dst_w as usize, dst_h as usize], &pixels);

        let left = (box_w.saturating_sub(dst_w) / 2) as i32;
        let top = (box_h.saturating_sub(dst_h) / 2) as i32;
        Some((color_image, left, top))
    }

    fn rasterize(
        &mut self,
        ctx: &Context,
        c: char,
        face: &ChainFace,
        cell: (u32, u32),
        cells: u32,
    ) -> Option<CachedColorGlyph> {
        let (image, left, top) = self.render(c, face, cell, cells)?;
        let [width, height] = image.size;
        let texture =
            ctx.load_texture(format!("color_glyph_{:x}", c as u32), image, TextureOptions::LINEAR);

        Some(CachedColorGlyph { texture, left, top, width: width as i32, height: height as i32 })
    }

    /// Drop the coldest glyphs until the cache fits its budget.  `keep` is the
    /// glyph just inserted; evicting it would leave the caller holding nothing.
    fn evict_to_budget(&mut self, keep: char) {
        while self.bytes > self.budget && self.entries.len() > 1 {
            let coldest = self
                .used
                .iter()
                .filter(|(c, _)| **c != keep)
                .min_by_key(|(_, tick)| **tick)
                .map(|(c, _)| *c);
            let Some(coldest) = coldest else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&coldest) {
                self.bytes = self.bytes.saturating_sub(evicted.bytes());
            }
            self.used.remove(&coldest);
        }
    }
}

/// COLR first, then bitmap strikes.  `Source::Outline` is deliberately absent:
/// a monochrome outline is exactly the case we want to hand back to egui.
const COLOR_SOURCES: &[Source] =
    &[Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit)];

/// Borrow the mapping `fonts::map_font_file` already holds for the face rather
/// than reading the file again: the chain's faces are handed to egui as
/// mappings, and a second owned copy of a 792 MB collection is 792 MB of
/// private memory that nothing evicts.
fn load(files: &mut HashMap<PathBuf, Option<&'static [u8]>>, path: &Path) -> Option<&'static [u8]> {
    *files.entry(path.to_path_buf()).or_insert_with(|| match crate::fonts::map_font_file(path) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            // Every face in the chain mapped during install and `FONT_MAPS`
            // never evicts, so arriving here means the two have diverged.
            log::warn!("colour font {} is in the chain but will not map: {e}", path.display());
            None
        },
    })
}

/// Bilinear resample of an RGBA buffer.  Colour bitmap strikes arrive at the
/// size the font shipped them (often 136px), which is far larger than a cell.
fn scale_rgba(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if (src_w, src_h) == (dst_w, dst_h) {
        return src.to_vec();
    }

    let mut out = vec![0u8; dst_w as usize * dst_h as usize * 4];
    let x_ratio = src_w as f32 / dst_w as f32;
    let y_ratio = src_h as f32 / dst_h as f32;

    for y in 0..dst_h {
        let sy = ((y as f32 + 0.5) * y_ratio - 0.5).max(0.0);
        let y0 = sy.floor() as u32;
        let y1 = (y0 + 1).min(src_h - 1);
        let wy = sy - y0 as f32;

        for x in 0..dst_w {
            let sx = ((x as f32 + 0.5) * x_ratio - 0.5).max(0.0);
            let x0 = sx.floor() as u32;
            let x1 = (x0 + 1).min(src_w - 1);
            let wx = sx - x0 as f32;

            let texel = |px: u32, py: u32, channel: usize| -> f32 {
                let offset = ((py * src_w + px) * 4) as usize + channel;
                src.get(offset).copied().unwrap_or(0) as f32
            };

            for channel in 0..4 {
                let top = texel(x0, y0, channel) * (1.0 - wx) + texel(x1, y0, channel) * wx;
                let bottom = texel(x0, y1, channel) * (1.0 - wx) + texel(x1, y1, channel) * wx;
                let value = top * (1.0 - wy) + bottom * wy;
                out[((y * dst_w + x) * 4) as usize + channel] =
                    value.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FontConfig, UiFont};

    /// The crate's own baked face, written to a unique path per test.
    /// `fonts::FONT_MAPS` is global and never cleared, so a test that asserts
    /// something *about* mapping has to own the path it asserts on.
    const FIXTURE: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/alacritree-symbols.ttf"));

    fn metrics() -> Metrics {
        Metrics { average_advance: 9.0, line_height: 20.0, descent: -4.0 }
    }

    /// Build the real chain the app would install, with whichever colour emoji
    /// fonts the machine actually has.  `None` when none carry emoji artwork the
    /// renderer can rasterize — the one case these tests have nothing to say about.
    ///
    /// Renderability is decided by reading the claiming face's colour tables
    /// directly, never by asking the renderer under test: a guard that called
    /// `get` would skip silently the moment the renderer broke, which is exactly
    /// when it needs to fail.  But `COLOR_SOURCES` only rasterizes bitmap strikes
    /// (CBDT/sbix) and COLR *version 0* layers; it has no COLRv1 paint-graph or
    /// SVG path, so a face whose only artwork for the glyph is COLRv1 (what modern
    /// Noto Color Emoji ships) or SVG produces nothing.  Counting those as
    /// renderable is what wedged CI on runners that carry a COLRv1 emoji font.
    ///
    /// Only the first face that claims U+1F600 is inspected, because that is the
    /// one face the renderer resolves the glyph to (see `claiming_index`); a
    /// renderable face further down the chain would never be consulted.
    fn chain_with_color_fonts(ctx: &Context) -> Option<Vec<ChainFace>> {
        let font = FontConfig {
            fallback: [
                "Twemoji Mozilla",
                "Noto Color Emoji",
                "Segoe UI Emoji",
                "Apple Color Emoji",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            ..FontConfig::default()
        };
        let (chain, _) = crate::fonts::install_terminal_fonts(ctx, &font, &UiFont::default());

        let renders_emoji = chain.iter().find_map(|face| {
            let data = std::fs::read(&face.path).ok()?;
            let parsed = ttf_parser::Face::parse(&data, face.face_index).ok()?;
            let glyph = parsed.glyph_index('😀')?;
            let tables = parsed.tables();
            let bitmap = tables.cbdt.is_some() || tables.sbix.is_some();
            let colr_v0 = tables.colr.is_some_and(|colr| colr.is_simple() && colr.contains(glyph));
            Some(bitmap || colr_v0)
        });

        renders_emoji.unwrap_or(false).then_some(chain)
    }

    /// The defect this module exists for: a face may not claim a codepoint it
    /// cannot draw.  egui picks the first face in the chain whose cmap has the
    /// character and never reconsiders, so if that face has neither an outline
    /// for it nor colour artwork, the cell paints blank.
    #[test]
    fn no_chain_face_claims_a_glyph_it_cannot_draw() {
        let ctx = Context::default();
        let Some(chain) = chain_with_color_fonts(&ctx) else {
            log::warn!("no colour emoji font installed; nothing to assert");
            return;
        };
        let mut cache = ColorGlyphCache::new(chain, 10);

        for c in ['😀', '✅', '❌', '🔴', '📁', '⭐'] {
            let drawn_in_color = cache.get(&ctx, c, &metrics(), 2).is_some();
            let drawn_by_egui = cache
                .resolve_claiming_face(c)
                .is_none_or(|face| crate::fonts::face_outlines_char(&face, c));
            assert!(
                drawn_in_color || drawn_by_egui,
                "U+{:04X} {c} is claimed by a face that can draw neither an outline nor \
                 colour artwork for it, so the cell renders blank",
                c as u32
            );
        }
    }

    /// Proves we drew artwork rather than a silhouette: a monochrome glyph is
    /// one hue at varying alpha, so counting distinct opaque RGB triples
    /// separates real colour from an outline egui could already have drawn.
    #[test]
    fn color_emoji_rasterizes_with_more_than_one_hue() {
        let ctx = Context::default();
        let Some(chain) = chain_with_color_fonts(&ctx) else {
            return;
        };
        let mut cache = ColorGlyphCache::new(chain.clone(), 10);
        let face = cache.resolve_claiming_face('😀').expect("no face claims U+1F600");
        let (image, _, _) = cache.render('😀', &face, (9, 20), 2).expect("emoji did not rasterize");

        let hues: std::collections::HashSet<(u8, u8, u8)> = image
            .pixels
            .iter()
            .filter(|px| px.a() > 128)
            .map(|px| (px.r(), px.g(), px.b()))
            .collect();
        assert!(
            hues.len() > 1,
            "U+1F600 rasterized to {} distinct hue(s); that is a silhouette, not colour artwork",
            hues.len()
        );
    }

    /// The cell box, not the glyph's own bitmap size, decides the placement:
    /// a CBDT strike ships at a fixed size (often 136px) and must be scaled
    /// down to fit, never blitted at native size over its neighbours.
    #[test]
    fn a_rasterized_emoji_fits_inside_its_cells() {
        let ctx = Context::default();
        let Some(chain) = chain_with_color_fonts(&ctx) else {
            return;
        };
        let mut cache = ColorGlyphCache::new(chain, 10);
        let glyph = cache.get(&ctx, '😀', &metrics(), 2).expect("emoji did not rasterize");

        let (cell_w, cell_h) = (9, 20);
        assert!(glyph.width <= cell_w * 2, "{} wider than its two cells", glyph.width);
        assert!(glyph.height <= cell_h, "{} taller than the line", glyph.height);
        assert!(glyph.left >= 0 && glyph.top >= 0);
        assert!(glyph.left + glyph.width <= cell_w * 2);
        assert!(glyph.top + glyph.height <= cell_h);
    }

    /// Ordinary text must not be diverted through the colour path — the whole
    /// grid would go through a texture blit per cell.
    #[test]
    fn plain_text_is_left_to_egui() {
        let ctx = Context::default();
        let (chain, _) =
            crate::fonts::install_terminal_fonts(&ctx, &FontConfig::default(), &UiFont::default());
        let mut cache = ColorGlyphCache::new(chain, 10);
        for c in ['A', 'z', '0', '─', '│'] {
            assert!(cache.get(&ctx, c, &metrics(), 1).is_none(), "{c} took the colour path");
        }
    }

    /// A cell-size change (font resize, DPI change) invalidates every raster.
    #[test]
    fn resizing_the_cell_clears_the_cache() {
        let ctx = Context::default();
        let Some(chain) = chain_with_color_fonts(&ctx) else {
            return;
        };
        let mut cache = ColorGlyphCache::new(chain, 10);

        cache.get(&ctx, '😀', &metrics(), 2).unwrap();
        let small = cache.entries[&'😀'].height;

        let bigger = Metrics { average_advance: 18.0, line_height: 40.0, descent: -8.0 };
        cache.get(&ctx, '😀', &bigger, 2).unwrap();

        assert_eq!(cache.entries.len(), 1, "stale rasters survived the resize");
        assert!(cache.entries[&'😀'].height > small);
    }

    #[test]
    fn the_cache_evicts_down_to_its_budget() {
        let ctx = Context::default();
        let Some(chain) = chain_with_color_fonts(&ctx) else {
            return;
        };
        // One byte of budget: every insert must immediately evict everything
        // except the glyph just handed to the caller.
        let mut cache = ColorGlyphCache { budget: 1, ..ColorGlyphCache::new(chain, 0) };

        for c in ['😀', '✅', '❌', '🔴'] {
            cache.get(&ctx, c, &metrics(), 2);
        }

        assert_eq!(cache.entries.len(), 1, "budget was not enforced");
        assert!(cache.bytes > 0);
        assert_eq!(cache.entries.len(), cache.used.len(), "eviction leaked LRU bookkeeping");
    }

    /// Every chain face is already memory-mapped by `fonts::map_font_file`.
    /// Reading it again cost 960 MB of private memory on a chain whose primary
    /// is a 792 MB collection, so pointer identity — not equal contents — is
    /// what this has to assert.
    #[test]
    fn load_returns_the_mapping_rather_than_a_copy() {
        let path = crate::test_util::scratch_dir().join("color_glyph_load.ttf");
        std::fs::write(&path, FIXTURE).unwrap();

        let mapped = crate::fonts::map_font_file(&path).expect("the fixture maps");
        let mut files = HashMap::new();
        let loaded = load(&mut files, &path).expect("the fixture loads");

        assert!(
            std::ptr::eq(mapped.as_ptr(), loaded.as_ptr()),
            "load copied the file instead of borrowing the mapping"
        );
    }

    /// A character no face in the chain claims must be recorded as egui's, or
    /// the whole chain is re-walked for it on every frame it is on screen.
    #[test]
    fn an_unclaimed_character_is_memoized() {
        let ctx = Context::default();
        let path = crate::test_util::scratch_dir().join("unclaimed_memo.ttf");
        std::fs::write(&path, FIXTURE).unwrap();
        let chain = vec![ChainFace { path, face_index: 0, color_only: false }];
        let mut cache = ColorGlyphCache::new(chain, 10);

        // The baked symbols face carries box drawing and chrome glyphs only.
        let unclaimed = '\u{4E00}';
        assert!(
            cache.resolve_claiming_face(unclaimed).is_none(),
            "the fixture claims U+4E00; this test would prove nothing"
        );

        assert!(cache.get(&ctx, unclaimed, &metrics(), 1).is_none());
        assert_eq!(
            cache.source.get(&unclaimed),
            Some(&None),
            "an unclaimed character was not recorded, so every frame re-walks the chain"
        );
    }

    /// After an eviction the glyph must be re-rasterized from the chain index
    /// already recorded for it, not rediscovered by re-parsing the chain.
    #[test]
    fn a_post_eviction_rerender_skips_the_chain_walk() {
        let ctx = Context::default();
        let Some(chain) = chain_with_color_fonts(&ctx) else {
            log::warn!("no colour emoji font installed; nothing to assert");
            return;
        };

        // `chain_with_color_fonts` proves renderability for U+1F600 alone, so
        // the second glyph is found rather than assumed.  A throwaway cache
        // keeps the probe out of the cache under test.
        let mut probe = ColorGlyphCache::new(chain.clone(), 10);
        let renderable: Vec<char> = ['\u{1F600}', '\u{1F601}', '\u{2764}', '\u{1F44D}']
            .into_iter()
            .filter(|c| probe.get(&ctx, *c, &metrics(), 2).is_some())
            .collect();
        if renderable.len() < 2 {
            log::warn!("fewer than two renderable colour glyphs here; nothing to assert");
            return;
        }
        let (first, second) = (renderable[0], renderable[1]);

        // One byte of budget: each insert evicts everything but itself.
        let mut cache = ColorGlyphCache { budget: 1, ..ColorGlyphCache::new(chain, 0) };
        assert!(cache.get(&ctx, first, &metrics(), 2).is_some(), "first glyph did not rasterize");
        assert!(cache.get(&ctx, second, &metrics(), 2).is_some(), "second glyph did not rasterize");
        assert!(!cache.entries.contains_key(&first), "the first glyph was not evicted");

        let walks = cache.chain_walks;
        assert!(cache.get(&ctx, first, &metrics(), 2).is_some(), "re-render after eviction failed");
        assert_eq!(cache.chain_walks, walks, "the chain was re-walked after an eviction");
    }
}
