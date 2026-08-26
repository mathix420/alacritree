//! Underlines and strikeouts rasterized into a texture, one tile per style.
//!
//! Drawing a decoration as a shape means the shader can only draw shapes it
//! knows how to describe, which is why a straight rule is all the arithmetic
//! ever produced.  Rasterizing the styles instead puts curls, dots and dashes
//! within reach at the same per-cell cost: the cell record carries a tile
//! index, and the fragment shader samples it.  Ghostty and kitty both take
//! this route, ghostty as sprite codepoints past the Unicode range and kitty
//! as its own sprite map.
//!
//! Every pattern repeats once per cell so a run of decorated cells tiles into
//! a continuous line with no seam at the cell boundary.

use egui::{Color32, ColorImage, Context, TextureHandle, TextureId, TextureOptions};

/// Underline styles, in the order their tiles sit in the strip.  Zero is the
/// undecorated cell, whose tile is never sampled: the vertex shader collapses
/// that quad rather than reading it.
pub const UNDERLINE_KINDS: u16 = 6;

pub const NONE: u16 = 0;
pub const STRAIGHT: u16 = 1;
pub const DOUBLE: u16 = 2;
pub const CURLY: u16 = 3;
pub const DOTTED: u16 = 4;
pub const DASHED: u16 = 5;

/// Tiles in the strip: every underline style, once plain and once struck
/// through.  A cell carries at most one of each, so the pair fits in one tile
/// and a cell with both still costs a single sample.
pub const TILES: u16 = UNDERLINE_KINDS * 2;

/// The tile a cell carrying `underline` and `strikeout` samples.
pub fn tile(underline: u16, strikeout: bool) -> u16 {
    underline + if strikeout { UNDERLINE_KINDS } else { 0 }
}

/// Where the lines sit and how thick they are, in physical pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Geometry {
    pub cell: [usize; 2],
    pub thickness: f32,
    /// Centre of the underline, measured down from the cell's top edge.
    pub underline_y: f32,
    pub strikeout_y: f32,
}

/// The strip, rebuilt whenever the cell it was drawn for changes size.
#[derive(Default)]
pub struct DecorationAtlas {
    texture: Option<TextureHandle>,
    drawn_for: Option<Geometry>,
}

impl DecorationAtlas {
    /// The strip's texture, rasterizing it first if `geometry` has moved.
    ///
    /// Returns `None` for a cell too small to hold a line, which is only
    /// reachable before the first layout has given the view a font.
    pub fn texture(&mut self, ctx: &Context, geometry: Geometry) -> Option<TextureId> {
        let [w, h] = geometry.cell;
        if w == 0 || h == 0 {
            return None;
        }
        if self.drawn_for != Some(geometry) {
            let image = rasterize(geometry);
            // `set` reuses the allocation when the size is unchanged, which a
            // font-size change is not, so this goes through `load_texture`.
            self.texture =
                Some(ctx.load_texture("grid_decorations", image, TextureOptions::LINEAR));
            self.drawn_for = Some(geometry);
        }
        self.texture.as_ref().map(TextureHandle::id)
    }
}

/// Vertical subsamples per pixel.  The curl is the only shape whose edge is
/// not axis-aligned, and four steps is where its stems stop looking ragged.
const SUBSAMPLES: usize = 4;

/// Draw every tile side by side into one row of cells.
fn rasterize(geometry: Geometry) -> ColorImage {
    let [w, h] = geometry.cell;
    let width = w * TILES as usize;
    let mut coverage = vec![0.0_f32; width * h];

    for underline in [STRAIGHT, DOUBLE, CURLY, DOTTED, DASHED] {
        for strikeout in [false, true] {
            let x0 = tile(underline, strikeout) as usize * w;
            draw_underline(&mut coverage, width, x0, underline, geometry);
            if strikeout {
                let y = geometry.strikeout_y;
                rect(&mut coverage, width, x0, w, y - geometry.thickness / 2.0, geometry.thickness);
            }
        }
    }
    // A struck cell with no underline is the one tile the loop above cannot
    // reach: it has no underline style to iterate over.
    let x0 = tile(NONE, true) as usize * w;
    rect(
        &mut coverage,
        width,
        x0,
        w,
        geometry.strikeout_y - geometry.thickness / 2.0,
        geometry.thickness,
    );

    let pixels = coverage
        .iter()
        .map(|&c| {
            let a = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            Color32::from_rgba_premultiplied(a, a, a, a)
        })
        .collect();
    ColorImage { size: [width, h], pixels }
}

fn draw_underline(buf: &mut [f32], stride: usize, x0: usize, kind: u16, geometry: Geometry) {
    let [w, h] = geometry.cell;
    let t = geometry.thickness;
    // Styles taller than one stem are pulled up until they fit, so a thick
    // line on a short cell loses its lower half to the cell edge instead of
    // its shape.
    let fit = |extent: f32| geometry.underline_y.min(h as f32 - extent);
    match kind {
        STRAIGHT => rect(buf, stride, x0, w, fit(t / 2.0) - t / 2.0, t),
        // One stem above the single underline's position and one below, with a
        // stem's worth of gap: any less and the pair reads as one thick rule.
        DOUBLE => {
            let y = fit(t * 1.5);
            rect(buf, stride, x0, w, y - t * 1.5, t);
            rect(buf, stride, x0, w, y + t * 0.5, t);
        },
        CURLY => curl(buf, stride, x0, geometry),
        // Both patterns repeat a whole number of times per cell, which is what
        // makes a run of them read as one continuous dotted or dashed line.
        DOTTED => {
            let y = fit(t / 2.0) - t / 2.0;
            // Dots as wide as they are tall, with a gap to match, so the
            // pattern survives the filtering that samples the tile.
            let dots = (w as f32 / (2.0 * t)).round().max(1.0);
            let step = w as f32 / dots;
            for i in 0..dots as usize {
                rect_x(buf, stride, x0, i as f32 * step, step / 2.0, y, t);
            }
        },
        DASHED => rect_x(buf, stride, x0, 0.0, w as f32 * 0.6, fit(t / 2.0) - t / 2.0, t),
        _ => {},
    }
}

/// An axis-aligned bar spanning the tile's full width.
fn rect(buf: &mut [f32], stride: usize, x0: usize, w: usize, top: f32, height: f32) {
    rect_x(buf, stride, x0, 0.0, w as f32, top, height);
}

/// An axis-aligned bar, with vertical and horizontal edges antialiased by the
/// fraction of the pixel each covers.
fn rect_x(buf: &mut [f32], stride: usize, x0: usize, left: f32, width: f32, top: f32, height: f32) {
    let (right, bottom) = (left + width, top + height);
    for py in top.floor().max(0.0) as usize..(bottom.ceil().max(0.0) as usize) {
        let dy = (bottom.min(py as f32 + 1.0) - top.max(py as f32)).clamp(0.0, 1.0);
        if dy <= 0.0 {
            continue;
        }
        for px in left.floor().max(0.0) as usize..(right.ceil().max(0.0) as usize) {
            let dx = (right.min(px as f32 + 1.0) - left.max(px as f32)).clamp(0.0, 1.0);
            let at = py * stride + x0 + px;
            if dx > 0.0 && at < buf.len() {
                buf[at] = (buf[at] + dx * dy).min(1.0);
            }
        }
    }
}

/// One full sine wave across the cell, so consecutive cells join up.
///
/// Coverage is the vertical distance to the curve corrected by its slope: an
/// uncorrected test thins the stroke wherever the wave is steep, which is
/// exactly where the eye reads a curl's shape.
fn curl(buf: &mut [f32], stride: usize, x0: usize, geometry: Geometry) {
    let [w, h] = geometry.cell;
    let t = geometry.thickness;
    // The wave hangs from the underline position rather than sitting on it:
    // anchoring the lowest ink there and claiming the room above is what keeps
    // the amplitude off zero on a cell whose underline sits near the bottom,
    // where a slope-limited curl degrades into a straight line.
    let bottom = (geometry.underline_y + t).min(h as f32 - 1.0);
    let top = (bottom - 3.0 * t).max(0.0);
    let amplitude = ((bottom - top - t) / 2.0).max(0.5);
    let centre = (top + bottom) / 2.0;
    let two_pi = std::f32::consts::TAU;

    for px in 0..w {
        for py in 0..h {
            let mut hits = 0.0_f32;
            for sy in 0..SUBSAMPLES {
                for sx in 0..SUBSAMPLES {
                    let x = px as f32 + (sx as f32 + 0.5) / SUBSAMPLES as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SUBSAMPLES as f32;
                    let phase = two_pi * x / w as f32;
                    let curve = centre + amplitude * phase.sin();
                    let slope = amplitude * two_pi / w as f32 * phase.cos();
                    let distance = (y - curve).abs() / (1.0 + slope * slope).sqrt();
                    if distance <= t / 2.0 {
                        hits += 1.0;
                    }
                }
            }
            let at = py * stride + x0 + px;
            if hits > 0.0 && at < buf.len() {
                buf[at] = (buf[at] + hits / (SUBSAMPLES * SUBSAMPLES) as f32).min(1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry { cell: [10, 20], thickness: 2.0, underline_y: 17.0, strikeout_y: 10.0 }
    }

    fn alpha(image: &ColorImage, tile_index: u16, x: usize, y: usize) -> u8 {
        let w = geometry().cell[0];
        image.pixels[y * image.size[0] + tile_index as usize * w + x].a()
    }

    /// Each style gets its own tile, and the strip is as wide as it declares.
    #[test]
    fn the_strip_holds_one_cell_per_tile() {
        let image = rasterize(geometry());
        assert_eq!(image.size, [geometry().cell[0] * TILES as usize, geometry().cell[1]]);
    }

    /// Nothing samples the undecorated tile, and leaving ink there would show
    /// the moment an index went wrong, so it stays empty.
    #[test]
    fn the_undecorated_tile_is_blank() {
        let image = rasterize(geometry());
        for x in 0..geometry().cell[0] {
            for y in 0..geometry().cell[1] {
                assert_eq!(alpha(&image, NONE, x, y), 0, "ink at {x},{y} of the blank tile");
            }
        }
    }

    /// Every style has to put ink somewhere, or a cell asking for it paints
    /// nothing and the decoration silently disappears.
    #[test]
    fn every_style_draws_something() {
        let image = rasterize(geometry());
        for underline in [STRAIGHT, DOUBLE, CURLY, DOTTED, DASHED] {
            for strikeout in [false, true] {
                let index = tile(underline, strikeout);
                let ink: u32 = (0..geometry().cell[0])
                    .flat_map(|x| (0..geometry().cell[1]).map(move |y| (x, y)))
                    .map(|(x, y)| alpha(&image, index, x, y) as u32)
                    .sum();
                assert!(ink > 0, "tile {index} is empty");
            }
        }
    }

    /// A straight underline lands on the row it was asked for and leaves the
    /// rest of the cell alone.
    #[test]
    fn a_straight_underline_sits_at_its_position() {
        let image = rasterize(geometry());
        assert_eq!(alpha(&image, STRAIGHT, 5, 17), 255);
        assert_eq!(alpha(&image, STRAIGHT, 5, 5), 0);
    }

    /// A struck cell with no underline is the one tile the style loop cannot
    /// produce, so it is drawn separately and has to come out non-empty.
    #[test]
    fn strikeout_alone_has_its_own_tile() {
        let image = rasterize(geometry());
        assert_eq!(alpha(&image, tile(NONE, true), 5, 10), 255);
        assert_eq!(alpha(&image, tile(NONE, true), 5, 17), 0, "it drew an underline too");
    }

    /// A dotted underline alternates ink and gap along the row it sits on;
    /// a solid row there would mean the pattern collapsed.
    #[test]
    fn a_dotted_underline_has_gaps() {
        let image = rasterize(geometry());
        let row: Vec<u8> = (0..geometry().cell[0]).map(|x| alpha(&image, DOTTED, x, 17)).collect();
        assert!(row.iter().any(|&a| a > 128), "no dots: {row:?}");
        assert!(row.iter().any(|&a| a < 128), "no gaps: {row:?}");
    }

    /// The two stems have to be separated by a blank row, or the pair reads as
    /// one thick rule and the style is indistinguishable from a straight one.
    #[test]
    fn a_double_underline_has_two_separated_stems() {
        let image = rasterize(geometry());
        let column: Vec<bool> =
            (0..geometry().cell[1]).map(|y| alpha(&image, DOUBLE, 5, y) > 128).collect();
        let stems = column.windows(2).filter(|w| !w[0] && w[1]).count();
        assert_eq!(stems, 2, "not two stems: {column:?}");
    }

    /// A curl leaves ink on more than one row, which is what separates it from
    /// the straight rule it would otherwise degrade into.
    #[test]
    fn a_curl_spans_several_rows() {
        let image = rasterize(geometry());
        let rows = (0..geometry().cell[1])
            .filter(|&y| (0..geometry().cell[0]).any(|x| alpha(&image, CURLY, x, y) > 32))
            .count();
        assert!(rows > 2, "the curl is flat: {rows} rows");
    }

    /// A cell can carry an underline and a strikeout at once, and its tile has
    /// to hold both rather than whichever was drawn last.
    #[test]
    fn a_combined_tile_carries_both_lines() {
        let image = rasterize(geometry());
        let index = tile(STRAIGHT, true);
        assert_eq!(alpha(&image, index, 5, 17), 255, "underline missing");
        assert_eq!(alpha(&image, index, 5, 10), 255, "strikeout missing");
    }
}
