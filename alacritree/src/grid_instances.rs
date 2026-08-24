//! Per-cell instance records for the GPU grid path.
//!
//! The mesh path writes four 20-byte vertices per cell, and almost all of that
//! is position arithmetic a vertex shader does for free.  One 16-byte record
//! per cell carries the same information, so the CPU writes a fifth of the
//! bytes and no geometry at all.
//!
//! Records are laid out at a fixed `cols` stride with a blank slot for empty
//! cells, so row `r` always occupies `[r * cols, (r + 1) * cols)`.  That is
//! what lets a frame rebuild and upload only the rows the terminal reported
//! damaged instead of the whole grid.

use egui::{Color32, Galley};

use crate::glyph_cache::Face;

/// Slot 0 is reserved for a cell with nothing to draw.  Its size is zero, so
/// the vertex shader collapses the quad and the rasterizer discards it.
pub const BLANK_SLOT: u16 = 0;

/// Where one character's artwork sits in egui's font atlas and where it is
/// drawn relative to its cell.  Read off a galley epaint laid out, so the
/// atlas stays epaint's and nothing here rasterizes a glyph.
///
/// Lives in the glyph table rather than in the per-cell record: a screen shows
/// tens of thousands of cells drawn from a few hundred distinct characters.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
#[repr(C)]
pub struct GlyphSlot {
    /// Atlas rectangle in texels, as the galley's vertices carry it.
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    /// Offset from the cell's top-left corner, in points.
    pub offset: [f32; 2],
    /// Drawn size in points.
    pub size: [f32; 2],
}

/// Decorations the fragment shader draws itself, so an underlined run costs no
/// extra geometry.  The mesh path emits a `Shape::LineSegment` per run for
/// these, which epaint tessellates as a feathered path.
pub mod cell_flags {
    pub const UNDERLINE: u16 = 1 << 0;
    pub const STRIKEOUT: u16 = 1 << 1;
}

/// One cell, glyph and background together. Twelve bytes against the mesh
/// path's eighty.
///
/// It carries no coordinates: records sit at a fixed row stride, so the cell a
/// record belongs to is its own index, which the vertex shader reads from
/// `gl_InstanceID`.  That also leaves every blank cell holding the same twelve
/// bytes as every other, which is what lets a row be cleared with a fill.
///
/// The background lives here rather than in a buffer of its own, so a frame is
/// one upload rather than two and a cell's colours share a cache line.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct GlyphInstance {
    pub slot: u16,
    pub flags: u16,
    /// Premultiplied sRGB, the same convention epaint's vertices use.
    pub fg: [u8; 4],
    pub bg: [u8; 4],
}

/// The four corners of a single-character galley, as the atlas holds them.
///
/// egui lays a one-character galley out as exactly one quad; anything else
/// (a character with no ink, a fallback that produced nothing) has no slot and
/// draws as blank.
pub fn slot_from_galley(galley: &Galley) -> Option<GlyphSlot> {
    let row = galley.rows.first()?;
    let v = &row.visuals.mesh.vertices;
    if v.len() != 4 {
        return None;
    }
    Some(GlyphSlot {
        uv_min: [v[0].uv.x, v[0].uv.y],
        uv_max: [v[3].uv.x, v[3].uv.y],
        offset: [v[0].pos.x, v[0].pos.y],
        size: [v[3].pos.x - v[0].pos.x, v[3].pos.y - v[0].pos.y],
    })
}

/// Character-and-face to slot index, plus the slot table itself.
///
/// The index is a two-level table rather than a map: a terminal asks tens of
/// thousands of times a frame, and two array loads cost a fraction of a hash
/// and a probe.  A flat array over every codepoint and face would be megabytes
/// of mostly-untouched memory, so the low byte of the character picks an entry
/// within a page and the rest picks the page, which is allocated the first
/// time a character on it is asked for.
pub struct GlyphTable {
    size: f32,
    /// Where each page starts in `entries`.  Offset zero is the shared page of
    /// blanks every untouched page points at, so a lookup is two loads with
    /// nothing to branch on.
    page_offset: Vec<u32>,
    entries: Vec<u16>,
    slots: Vec<GlyphSlot>,
    /// Bumped whenever `slots` grows, so a renderer holding an uploaded copy
    /// knows to send the new entries without diffing the table.
    generation: u32,
}

/// One page covers the 256 characters sharing a high byte, in all four faces.
const PAGE_ENTRIES: usize = 256 * 4;
const PAGES: usize = (char::MAX as usize + 1).div_ceil(256);
const EMPTY: u16 = u16::MAX;

/// Which page a character lives on, and where on that page it and `face` sit.
fn page_index(ch: char, face: Face) -> (usize, usize) {
    (ch as usize >> 8, (face as usize) << 8 | (ch as usize & 0xFF))
}

impl Default for GlyphTable {
    fn default() -> Self {
        Self {
            size: 0.0,
            page_offset: vec![0; PAGES],
            entries: vec![EMPTY; PAGE_ENTRIES],
            // Slot 0 is the blank cell; it is never looked up by character.
            slots: vec![GlyphSlot::default()],
            generation: 0,
        }
    }
}

impl GlyphTable {
    pub fn slots(&self) -> &[GlyphSlot] {
        &self.slots
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Drop every slot when the atlas or the font size moves under us.  A slot
    /// holds texel coordinates, so a repacked atlas leaves every one of them
    /// pointing at whatever landed in its place.
    pub fn clear(&mut self, size: f32) {
        self.size = size;
        self.page_offset.clear();
        self.page_offset.resize(PAGES, 0);
        self.entries.truncate(PAGE_ENTRIES);
        // A session that showed a wide spread of scripts leaves a page per
        // block behind, and nothing after this will ask for them again.
        self.entries.shrink_to_fit();
        self.slots.truncate(1);
        self.generation = self.generation.wrapping_add(1);
    }

    /// The slot for `ch`, laying it out through `galley` only on a miss.
    pub fn slot(
        &mut self,
        ch: char,
        face: Face,
        size: f32,
        galley: impl FnOnce() -> std::sync::Arc<Galley>,
    ) -> u16 {
        if self.size != size {
            self.clear(size);
        }
        let (page, entry) = page_index(ch, face);
        let at = self.page_offset[page] as usize + entry;
        if self.entries[at] != EMPTY {
            return self.entries[at];
        }

        let slot = match slot_from_galley(&galley()) {
            Some(s) if self.slots.len() < EMPTY as usize => {
                self.slots.push(s);
                self.generation = self.generation.wrapping_add(1);
                (self.slots.len() - 1) as u16
            },
            _ => BLANK_SLOT,
        };
        if self.page_offset[page] == 0 {
            self.page_offset[page] = self.entries.len() as u32;
            self.entries.resize(self.entries.len() + PAGE_ENTRIES, EMPTY);
        }
        self.entries[self.page_offset[page] as usize + entry] = slot;
        slot
    }
}

/// A frame's instance buffers, reused across frames.
///
/// `glyphs` is `cols * rows` long at all times so a row's records never move,
/// which is what makes a damage-driven partial upload possible.
#[derive(Default)]
pub struct GridInstances {
    pub glyphs: Vec<GlyphInstance>,
    cols: usize,
    rows: usize,
}

impl GridInstances {
    pub fn dimensions(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Byte range covering `rows`, for a partial buffer upload.
    pub fn row_bytes(&self, first: usize, count: usize) -> std::ops::Range<usize> {
        let stride = self.cols * size_of::<GlyphInstance>();
        first * stride..(first + count) * stride
    }

    pub fn resize(&mut self, cols: usize, rows: usize, default_bg: Color32) {
        if (self.cols, self.rows) == (cols, rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.glyphs.clear();
        self.glyphs.resize(cols * rows, GlyphInstance::default());
        for row in 0..rows {
            self.clear_row(row, default_bg.to_array());
        }
    }

    /// Clear `row` back to blank cells and the default background, ready for
    /// the runs that cover it to write over.
    fn clear_row(&mut self, row: usize, default_bg: [u8; 4]) {
        let blank = GlyphInstance { slot: BLANK_SLOT, flags: 0, fg: [0; 4], bg: default_bg };
        self.glyphs[row * self.cols..(row + 1) * self.cols].fill(blank);
    }

    /// Write every run in `runs` into the rows it covers, clearing those rows
    /// first.  `runs` must be confined to `rows_touched`.
    ///
    /// Runs arrive as an iterator rather than a slice because the caller's come
    /// out of a `flat_map`, whose `size_hint` floors at zero: collecting them
    /// grows a vector by doubling, which on a full-screen redraw of a colour
    /// per cell allocates megabytes per frame for a sequence read once.
    pub fn write_rows<'a>(
        &mut self,
        rows_touched: impl IntoIterator<Item = usize>,
        runs: impl IntoIterator<Item = RunView<'a>>,
        default_bg: Color32,
        mut slot_for: impl FnMut(char, Face) -> u16,
    ) {
        let blank = default_bg.to_array();
        for row in rows_touched {
            if row < self.rows {
                self.clear_row(row, blank);
            }
        }
        for run in runs {
            if run.row >= self.rows {
                continue;
            }
            let base = run.row * self.cols;
            let fg = run.fg.to_array();
            let bg = run.bg.to_array();
            // A blank on the default background is exactly what `clear_row`
            // already left behind, so its whole record can be skipped.
            let keeps_background = bg == blank;
            let mut col = run.start_col;
            for ch in run.text.chars() {
                if col >= self.cols {
                    break;
                }
                if ch == ' ' && keeps_background {
                    col += 1;
                    continue;
                }
                self.glyphs[base + col] = GlyphInstance {
                    slot: if ch == ' ' { BLANK_SLOT } else { slot_for(ch, run.face) },
                    flags: run.flags,
                    fg,
                    bg,
                };
                col += 1;
            }
        }
    }
}

/// What `write_rows` needs from a snapshot run, without borrowing the snapshot
/// itself — the caller resolves the text slice and the face once.
#[derive(Clone, Copy)]
pub struct RunView<'a> {
    pub text: &'a str,
    pub start_col: usize,
    pub row: usize,
    pub face: Face,
    pub flags: u16,
    pub fg: Color32,
    pub bg: Color32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn galley(ctx: &egui::Context, ch: char) -> std::sync::Arc<Galley> {
        let mut job = egui::text::LayoutJob::single_section(
            ch.to_string(),
            egui::TextFormat::simple(egui::FontId::monospace(14.0), Color32::PLACEHOLDER),
        );
        job.wrap.max_width = f32::INFINITY;
        ctx.fonts(|f| f.layout_job(job))
    }

    /// The whole point of the instance record: a cell costs twelve bytes
    /// where the mesh path spends four twenty-byte vertices on the same cell.
    #[test]
    fn a_cell_costs_twelve_bytes() {
        assert_eq!(size_of::<GlyphInstance>(), 12);
    }

    /// Not a gate — run it by hand:
    /// `cargo test -p alacritree --release -- --ignored --nocapture report_slot_lookup`
    ///
    /// What one screen's worth of slot lookups costs, split by how many
    /// distinct characters are on it.  ASCII resolves through a flat array;
    /// everything else takes the general path, which is what a CJK or
    /// Nerd-Font-heavy screen spends its whole frame in.
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_slot_lookup() {
        #[cfg(windows)]
        crate::harden_dll_search_path();

        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let cells = 318 * 83;

        for (name, base, distinct) in [
            ("ascii", ' ' as u32, 95u32),
            ("cjk, 256 distinct", 0x4E00, 256),
            ("cjk, 1024 distinct", 0x4E00, 1024),
            ("cjk, 4096 distinct", 0x4E00, 4096),
        ] {
            let asks: Vec<char> = (0..cells as u32)
                .map(|i| char::from_u32(base + i % distinct).expect("in range"))
                .collect();
            let mut table = GlyphTable::default();
            for &ch in &asks {
                table.slot(ch, Face::Normal, 14.0, || galley(&ctx, ch));
            }

            let mut body = || {
                for &ch in &asks {
                    std::hint::black_box(
                        table.slot(ch, Face::Normal, 14.0, || unreachable!("the table is warm")),
                    );
                }
            };
            for _ in 0..3 {
                body();
            }
            let start = std::time::Instant::now();
            for _ in 0..20 {
                body();
            }
            let each = start.elapsed() / 20;
            println!(
                "  {name:<19}: {each:?} for {cells} cells, {:.2} ns/cell",
                each.as_nanos() as f64 / cells as f64,
            );
        }
    }

    #[test]
    fn a_blank_cell_resolves_to_the_reserved_slot() {
        let table = GlyphTable::default();
        assert_eq!(table.slots()[BLANK_SLOT as usize].size, [0.0, 0.0]);
    }

    #[test]
    fn the_same_character_interns_once() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let mut table = GlyphTable::default();

        let first = table.slot('a', Face::Normal, 14.0, || galley(&ctx, 'a'));
        let before = table.generation();
        let again = table.slot('a', Face::Normal, 14.0, || galley(&ctx, 'a'));

        assert_eq!(first, again);
        assert_eq!(table.generation(), before, "a hit must not grow the table");
    }

    /// The face lives in the high half of a page entry, so the last character
    /// on a page in one face must not land on the first character in the next.
    #[test]
    fn a_page_entry_separates_every_face() {
        let faces = [Face::Normal, Face::Bold, Face::Italic, Face::BoldItalic];
        let edges = [0x4E00u32, 0x4EFF];

        let entries: Vec<usize> = faces
            .iter()
            .flat_map(|&f| edges.map(|c| page_index(char::from_u32(c).expect("valid"), f).1))
            .collect();

        let mut sorted = entries.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), entries.len(), "two faces share an entry: {entries:?}");
    }

    #[test]
    fn the_same_character_in_two_faces_takes_two_slots() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let mut table = GlyphTable::default();

        let normal = table.slot('a', Face::Normal, 14.0, || galley(&ctx, 'a'));
        let bold = table.slot('a', Face::Bold, 14.0, || galley(&ctx, 'a'));

        assert_ne!(normal, bold);
    }

    /// A slot holds texel coordinates into whatever atlas was live when it was
    /// read.  A font-size change relays out every glyph, so keeping the old
    /// slots would paint from the wrong part of the atlas.
    #[test]
    fn a_font_size_change_drops_every_slot() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let mut table = GlyphTable::default();
        table.slot('a', Face::Normal, 14.0, || galley(&ctx, 'a'));

        table.slot('a', Face::Normal, 20.0, || galley(&ctx, 'a'));

        assert_eq!(table.slots().len(), 2, "one blank slot plus the re-laid glyph");
    }

    /// Rows sit at a fixed stride so a damaged row can be rewritten and
    /// uploaded without touching its neighbours.
    #[test]
    fn a_row_keeps_its_place_when_a_neighbour_changes() {
        let mut grid = GridInstances::default();
        grid.resize(4, 3, Color32::BLACK);
        let runs = [RunView {
            text: "ab",
            start_col: 0,
            row: 2,
            face: Face::Normal,
            flags: 0,
            fg: Color32::WHITE,
            bg: Color32::BLACK,
        }];

        grid.write_rows([2], runs, Color32::BLACK, |_, _| 7);

        assert_eq!(grid.glyphs[8].slot, 7);
        assert_eq!(grid.glyphs[9].slot, 7);
        assert_eq!(grid.glyphs[10].slot, BLANK_SLOT);
        assert_eq!(grid.row_bytes(2, 1), 96..144);
    }

    #[test]
    fn rewriting_a_row_clears_what_the_last_frame_left() {
        let mut grid = GridInstances::default();
        grid.resize(4, 2, Color32::BLACK);
        let row0 = |text| {
            [RunView {
                text,
                start_col: 0,
                row: 0,
                face: Face::Normal,
                flags: 0,
                fg: Color32::WHITE,
                bg: Color32::BLACK,
            }]
        };
        grid.write_rows([0], row0("abcd"), Color32::BLACK, |_, _| 7);

        grid.write_rows([0], row0("ab"), Color32::BLACK, |_, _| 7);

        assert_eq!(grid.glyphs[2].slot, BLANK_SLOT, "the tail of the old run survived");
    }

    /// The background belongs to the cell's own record, so a coloured run
    /// paints its own cells and leaves its neighbours on the default.
    #[test]
    fn a_coloured_run_fills_only_its_own_cells() {
        let mut grid = GridInstances::default();
        grid.resize(4, 1, Color32::BLACK);
        let runs = [RunView {
            text: "ab",
            start_col: 1,
            row: 0,
            face: Face::Normal,
            flags: 0,
            fg: Color32::WHITE,
            bg: Color32::RED,
        }];

        grid.write_rows([0], runs, Color32::BLACK, |_, _| 1);

        assert_eq!(grid.glyphs[1].bg, Color32::RED.to_array());
        assert_eq!(grid.glyphs[2].bg, Color32::RED.to_array());
        assert_eq!(grid.glyphs[3].bg, Color32::BLACK.to_array());
    }
}
