//! Laid-out single-character galleys, reused across frames.
//!
//! The grid paints one glyph per cell so every character lands on the cursor's
//! `col * cell_w` boundary — egui's own run layout drifts off it.  Going
//! through `Painter::text` for each one costs a `String`, a `LayoutJob`, and a
//! galley-cache probe per glyph *per frame*, which at a maximized window is
//! tens of thousands of allocations to redraw glyphs that mostly did not
//! change.  A galley is immutable and its colour can be replaced at paint time
//! with `TextShape::override_text_color`, so one galley per character and
//! style serves every cell that ever shows it.

use std::collections::HashMap;
use std::sync::Arc;

use egui::text::LayoutJob;
use egui::{Color32, Context, FontFamily, FontId, Galley};

use crate::fonts::{BOLD_FAMILY, BOLD_ITALIC_FAMILY, ITALIC_FAMILY};

/// Which of the four terminal faces a glyph is drawn with.  Cheaper to hash
/// than a `FontId`, whose `f32` size is not `Hash` anyway.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Face {
    Normal,
    Bold,
    Italic,
    BoldItalic,
}

impl Face {
    pub fn new(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (true, true) => Self::BoldItalic,
            (true, false) => Self::Bold,
            (false, true) => Self::Italic,
            (false, false) => Self::Normal,
        }
    }

    fn font_id(self, size: f32) -> FontId {
        match self {
            Self::Normal => FontId::monospace(size),
            Self::Bold => FontId::new(size, FontFamily::Name(BOLD_FAMILY.into())),
            Self::Italic => FontId::new(size, FontFamily::Name(ITALIC_FAMILY.into())),
            Self::BoldItalic => FontId::new(size, FontFamily::Name(BOLD_ITALIC_FAMILY.into())),
        }
    }
}

/// `layout_job` rather than `layout_no_wrap` so the character is laid out with
/// `PLACEHOLDER`, which `override_text_color` is defined against; a concrete
/// colour here would be the one egui reuses if the override is ever dropped.
fn glyph_job(ch: char, face: Face, size: f32) -> LayoutJob {
    let mut job = LayoutJob::single_section(
        ch.to_string(),
        egui::TextFormat::simple(face.font_id(size), Color32::PLACEHOLDER),
    );
    job.wrap.max_width = f32::INFINITY;
    job
}

/// The font atlas a set of galleys was laid out against.  A galley's mesh
/// stores atlas positions, so it only means anything while that atlas is the
/// one being sampled.
#[derive(Clone, Copy, PartialEq)]
struct AtlasState {
    pixels_per_point: f32,
    max_texture_side: usize,
    image_size: [usize; 2],
    fill_ratio: f32,
}

impl AtlasState {
    fn read(ctx: &Context) -> Self {
        ctx.fonts(|f| Self {
            pixels_per_point: f.pixels_per_point(),
            max_texture_side: f.max_texture_side(),
            image_size: f.font_image_size(),
            fill_ratio: f.font_atlas_fill_ratio(),
        })
    }

    /// Whether galleys laid out against `self` can still be painted now that
    /// the atlas looks like `now`.  Repacking is the case that matters, but
    /// growth moves nothing and is folded in anyway: it costs one relayout of
    /// the visible glyphs on a frame the atlas changed shape, and keeping the
    /// rule to "anything moved" leaves no repack unnoticed.
    fn outlived_by(self, now: Self) -> bool {
        self.pixels_per_point != now.pixels_per_point
            || self.max_texture_side != now.max_texture_side
            || self.image_size != now.image_size
            || now.fill_ratio < self.fill_ratio
    }
}

#[derive(Default)]
pub struct GlyphCache {
    /// Point size the cached galleys were laid out at.  A font-size change
    /// (zoom, config reload) invalidates every one of them.
    size: f32,
    /// The atlas the entries were laid out against, once a frame has observed
    /// one.  `None` before the first `begin_frame`, when there is nothing
    /// cached to invalidate.
    atlas: Option<AtlasState>,
    entries: HashMap<(char, Face), Arc<Galley>>,
}

impl GlyphCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every galley egui's font atlas has outlived.  Call once per frame
    /// before any `get`.
    ///
    /// `epaint::Fonts::begin_pass` replaces the whole atlas — and drops egui's
    /// own galley cache with it — when the scale changes, the texture limit
    /// changes, or the atlas passes 80% full.  Glyphs are repacked into
    /// different positions, so a galley held across that boundary addresses
    /// whatever landed in its old slot and paints some other character.
    pub fn begin_frame(&mut self, ctx: &Context) {
        let now = AtlasState::read(ctx);
        if self.atlas.is_some_and(|prev| prev.outlived_by(now)) {
            self.entries.clear();
        }
        self.atlas = Some(now);
    }

    /// The galley for `ch` in `face`, laid out once and reused.  Colour is not
    /// baked in: callers override it per cell.
    pub fn get(&mut self, ctx: &Context, ch: char, face: Face, size: f32) -> Arc<Galley> {
        if self.size != size {
            self.entries.clear();
            self.size = size;
        }
        if let Some(galley) = self.entries.get(&(ch, face)) {
            return galley.clone();
        }
        let galley = ctx.fonts(|f| f.layout_job(glyph_job(ch, face, size)));
        self.entries.insert((ch, face), galley.clone());
        galley
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context with fonts available and the three named terminal families
    /// bound, as `fonts::install_terminal_fonts` leaves it in the app.  egui
    /// has no fonts at all until a frame has run, and panics on a family it
    /// was never given.
    fn ctx() -> Context {
        let ctx = Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let mono = fonts.families[&FontFamily::Monospace].clone();
        for name in [BOLD_FAMILY, ITALIC_FAMILY, BOLD_ITALIC_FAMILY] {
            fonts.families.insert(FontFamily::Name(name.into()), mono.clone());
        }
        ctx.set_fonts(fonts);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        ctx
    }

    /// The whole point: painting the same character again must not lay it out
    /// again, however many cells show it.
    #[test]
    fn a_repeated_character_is_laid_out_once() {
        let ctx = ctx();
        let mut cache = GlyphCache::new();

        for _ in 0..100 {
            cache.get(&ctx, 'a', Face::Normal, 14.0);
        }

        assert_eq!(cache.len(), 1);
    }

    /// Bold and italic are separate faces, so they cannot share a galley —
    /// reusing one would paint every bold cell in the regular face.
    #[test]
    fn each_face_gets_its_own_galley() {
        let ctx = ctx();
        let mut cache = GlyphCache::new();

        for face in [Face::Normal, Face::Bold, Face::Italic, Face::BoldItalic] {
            cache.get(&ctx, 'a', face, 14.0);
        }

        assert_eq!(cache.len(), 4);
    }

    /// Galleys carry their laid-out size, so a zoom step has to discard them —
    /// keeping them would paint the old size until the cache happened to miss.
    #[test]
    fn a_font_size_change_discards_the_cached_galleys() {
        let ctx = ctx();
        let mut cache = GlyphCache::new();
        cache.get(&ctx, 'a', Face::Normal, 14.0);

        cache.get(&ctx, 'b', Face::Normal, 20.0);

        assert_eq!(cache.len(), 1, "galleys laid out at the old size survived a size change");
    }

    /// The atlas position of the first vertex of a galley's mesh — where in
    /// the font texture painting this galley actually samples from.
    fn atlas_pos(galley: &Galley) -> egui::Pos2 {
        galley.rows[0].visuals.mesh.vertices[0].uv
    }

    /// egui repacks the whole font atlas when the scale changes, and drops its
    /// own galley cache doing it.  A galley kept across that boundary still
    /// addresses the slot it had in the discarded atlas, so it paints whatever
    /// character was repacked into that slot instead of its own.
    #[test]
    fn a_repacked_atlas_discards_the_cached_galleys() {
        let ctx = ctx();
        let mut cache = GlyphCache::new();
        cache.begin_frame(&ctx);
        let before = atlas_pos(&cache.get(&ctx, 'a', Face::Normal, 14.0));

        ctx.set_pixels_per_point(2.0);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        cache.begin_frame(&ctx);
        let served = atlas_pos(&cache.get(&ctx, 'a', Face::Normal, 14.0));

        let repacked = ctx.fonts(|f| f.layout_job(glyph_job('a', Face::Normal, 14.0)));
        assert_ne!(
            before,
            atlas_pos(&repacked),
            "the scale change did not move 'a' in the atlas, so this cannot detect a stale galley"
        );
        assert_eq!(
            served,
            atlas_pos(&repacked),
            "cache served a galley addressing the discarded atlas"
        );
    }

    #[test]
    fn face_maps_bold_and_italic_flags() {
        assert_eq!(Face::new(false, false), Face::Normal);
        assert_eq!(Face::new(true, false), Face::Bold);
        assert_eq!(Face::new(false, true), Face::Italic);
        assert_eq!(Face::new(true, true), Face::BoldItalic);
    }
}
