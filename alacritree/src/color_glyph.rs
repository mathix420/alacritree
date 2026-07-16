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
use std::sync::Arc;

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
    /// Font files behind the chain, read on first use.  A `None` marks a file
    /// that could not be read, so a broken font is not re-read every frame.
    files: HashMap<PathBuf, Option<Arc<Vec<u8>>>>,
    /// Which chain entry, if any, draws this character in colour.  `None` means
    /// egui's own glyph pipeline owns it.
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
        // A character already known to have no colour artwork costs one lookup;
        // the whole grid takes this path on every frame.
        if self.source.get(&c) == Some(&None) {
            return None;
        }

        // Only the claiming face is considered.  Looking further down the chain
        // would rasterize from a font egui had already passed over, so the two
        // renderers would disagree about which face owns the character.
        let index = self.claiming_index(c)?;
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
        for i in 0..self.chain.len() {
            let face = self.chain[i].clone();
            let Some(data) = load(&mut self.files, &face.path) else {
                continue;
            };
            let claims = FontRef::from_index(&data, face.face_index as usize)
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
        let font = FontRef::from_index(&data, face.face_index as usize)?;
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

/// Read a font file once and keep it, so the chain's larger faces are not
/// re-read on every cache miss.  A file that cannot be read is remembered as
/// unreadable rather than retried.
fn load(files: &mut HashMap<PathBuf, Option<Arc<Vec<u8>>>>, path: &Path) -> Option<Arc<Vec<u8>>> {
    files
        .entry(path.to_path_buf())
        .or_insert_with(|| match std::fs::read(path) {
            Ok(bytes) => Some(Arc::new(bytes)),
            Err(e) => {
                log::debug!("could not read colour font {}: {e}", path.display());
                None
            },
        })
        .clone()
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
    use crate::config::FontConfig;

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
        let chain = crate::fonts::install_terminal_fonts(ctx, &font, None);

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
        let chain = crate::fonts::install_terminal_fonts(&ctx, &FontConfig::default(), None);
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
}
