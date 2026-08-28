//! The grid drawn by the GPU from one record per cell.
//!
//! epaint takes a mesh, but it takes it by copying every vertex into its own
//! buffer, and a full screen is a hundred thousand vertices the CPU wrote only
//! so the GPU could read them back.  A paint callback hands the frame straight
//! to OpenGL instead: epaint sees one shape with no geometry, and the vertex
//! shader derives each quad from a twelve-byte cell record.
//!
//! Nothing here owns a glyph atlas.  `egui_glow::Painter::texture` hands over
//! the raw texture epaint already packed its glyphs into, so the shader samples
//! the same artwork the mesh path would have.
//!
//! Three draws over one buffer, none of them carrying any geometry: a quad
//! instanced once per cell for the backgrounds, the same again for the glyphs,
//! then a band per cell for underlines and strikeouts.  Decorations go last
//! because alacritty draws its rects over the text (`display::draw`), so a
//! descender crossing an underline has to come out the same way here.
//! Emoji and box-drawing glyphs stay on egui's painter — they carry their own
//! textures or their own geometry, and they are rare enough to leave.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui_glow::ShaderVersion;
use eframe::glow::{self, HasContext};
use egui::Rect;

use crate::decoration_sprites::DecorationAtlas;
use crate::gpu_timing::{self, Ab, GpuTimers};
use crate::grid_instances::{GlyphInstance, GlyphSlot, GlyphTable, GridInstances};

/// Attribute locations, bound before linking so every program reads the same
/// record the same way.  `#version 140` has no `layout(location = ...)`, so
/// the binding has to come from this side.
const ATTRIBUTES: [(u32, &str); 4] = [(0, "a_slot"), (1, "a_fg"), (2, "a_bg"), (3, "a_deco")];

/// Slots packed across a texture row, two RGBA32F texels each.  A row per
/// slot would cap the table at the 2048 rows a driver is obliged to offer,
/// which a screen of CJK or Nerd Font icons reaches; packing sideways puts the
/// ceiling back on the u16 slot index, where the table already enforces it.
const SLOTS_PER_ROW: usize = 256;

/// What the shaders need that is not in a per-cell record.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Frame {
    /// Grid rect in points, relative to the callback viewport's top-left.
    pub origin: [f32; 2],
    pub cell: [f32; 2],
    pub grid: [u32; 2],
    /// The decoration strip, and how many tiles wide it is.  `None` leaves
    /// the decoration pass unrun, which is what a grid with no font yet has.
    pub decorations: Option<egui::TextureId>,
    pub decoration_tiles: u32,
    /// The terminal's own background, as the clear colour.  A grid rect is
    /// rarely an exact multiple of a cell, and the cell quads stop at the last
    /// whole one, so the strip past it is filled by clearing rather than by a
    /// shape epaint would have to tessellate every frame.
    pub default_bg: [f32; 4],
}

/// What the paint callback is asked to measure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    /// `[debug] gpu_timing`: time the upload and each draw on the GPU.
    pub enabled: bool,
    /// `[debug] gpu_ab`: alternate one of the callback's skips between report
    /// windows so both arms are timed against one driver and one grid.
    pub ab: Ab,
}

/// The CPU half, written by the UI thread and read by the paint callback.
///
/// Both run on the same thread under eframe, so the lock is never contended;
/// it exists because `egui_glow::CallbackFn` demands `Send + Sync`.
#[derive(Default)]
pub struct GridState {
    pub instances: GridInstances,
    pub table: GlyphTable,
    /// The rasterized decoration styles, rebuilt when the cell changes size.
    pub decorations: DecorationAtlas,
    pub frame: Frame,
    /// Rows rewritten since the last upload, as a half-open range.  Empty
    /// means the GPU copy is already current and only uniforms need sending.
    pub dirty_rows: std::ops::Range<usize>,
    uploaded_generation: u32,
    uploaded_dims: (usize, usize),
}

impl GridState {
    /// Every row is dirty after a resize: the records moved.
    pub fn mark_all_dirty(&mut self) {
        self.dirty_rows = 0..self.instances.dimensions().1;
    }

    /// The two buffers a frame writes, handed back separately so a run can be
    /// interned into the glyph table while it is being written into the
    /// instance buffer.
    pub fn buffers(&mut self) -> (&mut GridInstances, &mut GlyphTable) {
        (&mut self.instances, &mut self.table)
    }

    pub fn mark_rows_dirty(&mut self, rows: std::ops::Range<usize>) {
        if self.dirty_rows.is_empty() {
            self.dirty_rows = rows;
        } else {
            self.dirty_rows =
                self.dirty_rows.start.min(rows.start)..self.dirty_rows.end.max(rows.end);
        }
    }
}

/// Handle the app holds: the shared CPU state plus the GL objects, built on
/// the first paint because that is the first time a `glow::Context` exists.
pub struct GpuGrid {
    pub state: Arc<Mutex<GridState>>,
    gl: Arc<Mutex<GlSlot>>,
    /// Set by the paint callback when the GL side will not build.  Only the
    /// callback holds a `glow::Context`, so the caller cannot learn this
    /// before it has asked for one frame it will not get; from the next frame
    /// on it paints the mesh instead.
    failed: Arc<AtomicBool>,
}

/// Building the GL side is attempted exactly once.  A driver that rejects the
/// shaders rejects them every frame, and each attempt allocates before it
/// discovers that, so a retrying build is a leak with no visible cause.
enum GlSlot {
    Unbuilt,
    Ready(GlResources),
    Failed,
}

impl GpuGrid {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(GridState::default())),
            gl: Arc::new(Mutex::new(GlSlot::Unbuilt)),
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the GL side is known not to build, so the caller can paint the
    /// mesh instead of a shape that draws nothing.
    pub fn unavailable(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Stand in for a driver that rejects the shaders.  Nothing headless has a
    /// `glow::Context` for the callback to fail against, so the state it would
    /// have reached has to be set from outside.
    #[cfg(test)]
    pub fn mark_unavailable(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }

    /// The shape to hand egui.  Everything it draws comes from `state`, which
    /// the caller has already written this frame — except the atlas size,
    /// which only the atlas live at paint time can give.
    pub fn callback(&self, rect: Rect, ctx: &egui::Context, timing: Timing) -> egui::Shape {
        let (state, resources, ctx) = (self.state.clone(), self.gl.clone(), ctx.clone());
        let failed = self.failed.clone();
        egui::Shape::Callback(egui::epaint::PaintCallback {
            rect,
            callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |_info, painter| {
                let mut held = resources.lock().expect("gl resources");
                let gl = painter.gl().clone();
                if let GlSlot::Unbuilt = *held {
                    *held = match GlResources::new(&gl, timing) {
                        Ok(resources) => GlSlot::Ready(resources),
                        Err(err) => {
                            log::error!("gpu grid disabled: {err}");
                            failed.store(true, Ordering::Relaxed);
                            GlSlot::Failed
                        },
                    };
                }
                let GlSlot::Ready(resources) = &mut *held else {
                    return;
                };
                let mut state = state.lock().expect("grid state");
                let atlas = painter.texture(egui::TextureId::default());
                // A slot holds texels, and egui normalizes every uv against
                // the size the atlas ended the frame at.  Laying a glyph out
                // doubles that atlas when it runs out of room, so a size read
                // while the frame was still being built leaves the shader
                // dividing texels by half an atlas.
                let size = ctx.fonts(|f| f.font_image_size());
                let decorations = state.frame.decorations.and_then(|id| painter.texture(id));
                resources.draw(&gl, &mut state, atlas, size.map(|side| side as f32), decorations);
            })),
        })
    }
}

impl Default for GpuGrid {
    fn default() -> Self {
        Self::new()
    }
}

struct GlResources {
    glyph: Program,
    /// The pre-collapse glyph shader, built only when the A/B asks to price the
    /// colour-channel conversion against reading coverage from alpha.  It
    /// exists to be compared against, not to ship.
    glyph_gamma: Option<Program>,
    /// The glyph shader that discards fragments the atlas gives no coverage,
    /// built only while the A/B is pricing that against blending them.
    glyph_discard: Option<Program>,
    /// Paints the default background across the callback's rect with one draw,
    /// built only while the A/B is pricing that against `glClear`.
    clear_quad: Option<Program>,
    background: Program,
    decoration: Program,
    vao: glow::VertexArray,
    instances: glow::Buffer,
    instance_capacity: usize,
    slot_texture: glow::Texture,
    /// The slot table padded to whole texture rows, kept across uploads so a
    /// table that grew by one glyph does not allocate to send itself.
    slot_scratch: Vec<GlyphSlot>,
    /// `None` unless `[debug] gpu_timing` asked for it and the context can
    /// answer.
    timers: Option<GpuTimers>,
}

struct Program {
    program: glow::Program,
    uniforms: Vec<(String, glow::UniformLocation)>,
}

impl Program {
    fn location(&self, name: &str) -> Option<&glow::UniformLocation> {
        self.uniforms.iter().find(|(n, _)| n == name).map(|(_, l)| l)
    }
}

impl GlResources {
    fn new(gl: &glow::Context, timing: Timing) -> Result<Self, String> {
        let version = ShaderVersion::get(gl);
        // Instanced arrays, `texelFetch` and integer vertex attributes all
        // arrive together in GL 3 / GLES 3.  Older contexts keep the mesh path.
        let header = match version {
            ShaderVersion::Gl140 => "#version 140\n",
            ShaderVersion::Es300 => "#version 300 es\nprecision highp float;\n",
            ShaderVersion::Gl120 | ShaderVersion::Es100 => {
                return Err(format!("{version:?} has no instanced arrays"));
            },
        };
        unsafe {
            // A hybrid laptop can hand the window to either GPU, and a
            // microsecond figure from this callback is only plausible or
            // implausible once you know which one drew it.
            log::info!(
                "grid gl: {} | {} | {}",
                gl.get_parameter_string(glow::VENDOR),
                gl.get_parameter_string(glow::RENDERER),
                gl.get_parameter_string(glow::VERSION),
            );
            let mut programs = Vec::new();
            for (vertex, fragment) in [
                (GLYPH_VERT, GLYPH_FRAG),
                (BACKGROUND_VERT, BACKGROUND_FRAG),
                (DECORATION_VERT, DECORATION_FRAG),
            ] {
                match link(gl, header, vertex, fragment) {
                    Ok(program) => programs.push(program),
                    Err(err) => {
                        for spent in programs {
                            gl.delete_program(spent.program);
                        }
                        return Err(err);
                    },
                }
            }
            let [glyph, background, decoration]: [Program; 3] =
                programs.try_into().unwrap_or_else(|_| unreachable!("one program per pair"));
            // `glGen*` returns zero only on a context that is already dead, and
            // the caller latches the failure, so the objects a partial run
            // leaves behind are made at most once.
            let vao = gl.create_vertex_array()?;
            let instances = gl.create_buffer()?;
            let slot_texture = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(slot_texture));
            for (name, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, name, value as i32);
            }
            let timers = timing.enabled.then(|| GpuTimers::new(gl, timing.ab)).flatten();
            let glyph_gamma = match timers.as_ref().is_some_and(GpuTimers::wants_glyph_gamma) {
                true => Some(link(gl, header, GLYPH_VERT, GLYPH_FRAG_GAMMA)?),
                false => None,
            };
            let glyph_discard = match timers.as_ref().is_some_and(GpuTimers::wants_glyph_discard) {
                true => Some(link(gl, header, GLYPH_VERT, GLYPH_FRAG_DISCARD)?),
                false => None,
            };
            let clear_quad = match timers.as_ref().is_some_and(GpuTimers::wants_clear_quad) {
                true => Some(link(gl, header, CLEAR_VERT, CLEAR_FRAG)?),
                false => None,
            };
            Ok(Self {
                glyph,
                glyph_gamma,
                glyph_discard,
                clear_quad,
                background,
                decoration,
                vao,
                instances,
                instance_capacity: 0,
                slot_texture,
                slot_scratch: Vec::new(),
                timers,
            })
        }
    }

    fn draw(
        &mut self,
        gl: &glow::Context,
        state: &mut GridState,
        atlas: Option<glow::Texture>,
        atlas_size: [f32; 2],
        decorations: Option<glow::Texture>,
    ) {
        let (cols, rows) = state.instances.dimensions();
        if cols == 0 || rows == 0 {
            return;
        }
        let issued = std::time::Instant::now();
        // Held apart from `self` for the frame: the stages it wraps all take
        // `&mut self`, and a timer borrowed out of the same struct would keep
        // them from being called at all.
        let mut timers = self.timers.take();
        if let Some(timers) = &mut timers {
            timers.begin_frame(gl);
            timers.begin_whole(gl);
        }
        unsafe {
            if let Some(timers) = &mut timers {
                timers.begin(gl, gpu_timing::CLEAR);
            }
            // egui scissors the callback to its clip rect before handing over,
            // so this reaches the grid and nothing around it.
            let [r, g, b, a] = state.frame.default_bg;
            let unscissored = timers.as_ref().is_some_and(GpuTimers::lifts_clear_scissor);
            if unscissored {
                gl.disable(glow::SCISSOR_TEST);
            }
            match self.clear_quad.as_ref().filter(|_| {
                timers.as_ref().is_some_and(GpuTimers::draws_clear_quad)
            }) {
                // Writes the same pixels the clear would, through the ordinary
                // raster path rather than the driver's clear path.  The three
                // vertices come from `gl_VertexID`, so no buffer is bound and
                // whatever the VAO carries is never fetched.
                Some(program) => {
                    gl.use_program(Some(program.program));
                    if let Some(location) = program.location("u_color") {
                        gl.uniform_4_f32(Some(location), r, g, b, a);
                    }
                    gl.bind_vertex_array(Some(self.vao));
                    gl.draw_arrays(glow::TRIANGLES, 0, 3);
                },
                None if timers.as_ref().is_some_and(GpuTimers::skips_clear) => {},
                None => {
                    gl.clear_color(r, g, b, a);
                    gl.clear(glow::COLOR_BUFFER_BIT);
                },
            }
            if unscissored {
                gl.enable(glow::SCISSOR_TEST);
            }
            if let Some(timers) = &timers {
                timers.end(gl);
            }

            if let Some(timers) = &mut timers {
                timers.begin(gl, gpu_timing::UPLOAD);
            }
            self.upload(gl, state);
            if let Some(timers) = &timers {
                timers.end(gl);
            }
            gl.bind_vertex_array(Some(self.vao));
            self.bind_records(gl);

            if let Some(timers) = &mut timers {
                timers.begin(gl, gpu_timing::BACKGROUNDS);
            }
            // A colour no normalized byte can hold matches no cell, so the
            // A/B's other arm draws every quad the way it did before the
            // collapse without taking a different path to get there.
            let default_bg = match timers.as_ref().is_some_and(GpuTimers::forces_backgrounds) {
                true => [-1.0; 4],
                false => state.frame.default_bg,
            };
            // egui_glow leaves premultiplied blending on before handing over,
            // and every terminal colour is opaque, so blending this pass
            // computes the source it already had.  The A/B's other arm pays
            // for it the way the shipped path does.
            let unblended = timers.as_ref().is_some_and(GpuTimers::skips_background_blend);
            if unblended {
                gl.disable(glow::BLEND);
            }
            self.draw_backgrounds(gl, state, cols * rows, default_bg);
            if unblended {
                gl.enable(glow::BLEND);
            }
            if let Some(timers) = &timers {
                timers.end(gl);
            }
            if let Some(atlas) = atlas {
                if let Some(timers) = &mut timers {
                    timers.begin(gl, gpu_timing::GLYPHS);
                }
                // The A/B's other arm recovers coverage from the colour
                // channels, which needs its own program: the two shaders
                // differ at compile time, not in a uniform.
                let timing = timers.as_ref();
                let glyph = if timing.is_some_and(GpuTimers::forces_glyph_gamma) {
                    self.glyph_gamma.as_ref().unwrap_or(&self.glyph)
                } else if timing.is_some_and(GpuTimers::discards_blank_coverage) {
                    self.glyph_discard.as_ref().unwrap_or(&self.glyph)
                } else {
                    &self.glyph
                };
                self.draw_glyphs(gl, state, atlas, atlas_size, cols * rows, glyph);
                if let Some(timers) = &timers {
                    timers.end(gl);
                }
            }
            // Holding a strip only says the atlas exists.  Every cell still
            // gets an instance, so an undecorated screen was paying a
            // full-grid instanced draw to collapse every quad in the vertex
            // shader.  The A/B's other arm skips this test to price it.
            let decorated = state.instances.any_decorated()
                || timers.as_ref().is_some_and(GpuTimers::forces_decorations);
            match decorations.filter(|_| decorated) {
                Some(strip) => {
                    if let Some(timers) = &mut timers {
                        timers.begin(gl, gpu_timing::DECORATIONS);
                    }
                    self.draw_decorations(gl, state, strip, cols * rows);
                    if let Some(timers) = &timers {
                        timers.end(gl);
                    }
                },
                None => {
                    if let Some(timers) = &mut timers {
                        timers.skipped_decorations();
                    }
                },
            }

            for (index, _) in ATTRIBUTES {
                gl.disable_vertex_attrib_array(index);
            }
            gl.bind_vertex_array(None);
        }
        if let Some(timers) = &mut timers {
            timers.end_whole(gl);
            timers.end_frame(issued.elapsed(), (cols, rows));
        }
        self.timers = timers;
    }

    unsafe fn upload(&mut self, gl: &glow::Context, state: &mut GridState) {
        unsafe {
            let generation = state.table.generation();
            if generation != state.uploaded_generation {
                // Two RGBA32F texels per slot: the atlas rectangle, then where
                // it sits in the cell and how big it is drawn.  The tail of the
                // last row is padded out because a texture upload wants every
                // texel it declared; the shader never reads it.
                let rows = pad_slots(state.table.slots(), &mut self.slot_scratch);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.slot_texture));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA32F as i32,
                    (SLOTS_PER_ROW * 2) as i32,
                    rows as i32,
                    0,
                    glow::RGBA,
                    glow::FLOAT,
                    glow::PixelUnpackData::Slice(Some(bytemuck_cast(&self.slot_scratch))),
                );
                state.uploaded_generation = generation;
            }

            let dims = state.instances.dimensions();
            if state.uploaded_dims != dims {
                state.uploaded_dims = dims;
                state.mark_all_dirty();
            }

            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.instances));
            let bytes = bytemuck_cast(&state.instances.glyphs);
            if self.instance_capacity < bytes.len() {
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
                self.instance_capacity = bytes.len();
            } else if !state.dirty_rows.is_empty() {
                // The whole point of the fixed row stride: a frame that
                // rewrote three rows sends three rows.
                let span =
                    state.instances.row_bytes(state.dirty_rows.start, state.dirty_rows.len());
                gl.buffer_sub_data_u8_slice(
                    glow::ARRAY_BUFFER,
                    span.start as i32,
                    &bytes[span.clone()],
                );
            }

            state.dirty_rows = 0..0;
        }
    }

    /// Point every attribute at the one record buffer, advancing once per
    /// cell.  Both programs read from here; each ignores the fields it has no
    /// input for.
    unsafe fn bind_records(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.instances));
            let stride = size_of::<GlyphInstance>() as i32;
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_i32(0, 1, glow::UNSIGNED_SHORT, stride, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 4, glow::UNSIGNED_BYTE, true, stride, 4);
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 4, glow::UNSIGNED_BYTE, true, stride, 8);
            gl.enable_vertex_attrib_array(3);
            gl.vertex_attrib_pointer_i32(3, 1, glow::UNSIGNED_SHORT, stride, 2);
            for (index, _) in ATTRIBUTES {
                gl.vertex_attrib_divisor(index, 1);
            }
        }
    }

    /// The cell backgrounds, one instance per cell.  A cell still carrying
    /// `default_bg` collapses in the vertex shader, because the clear already
    /// painted the whole grid rect that colour; alacritty reaches the same end
    /// by giving such a cell zero alpha and discarding it in the fragment
    /// shader (`compute_bg_alpha`, `text.f.glsl`).
    unsafe fn draw_backgrounds(
        &self,
        gl: &glow::Context,
        state: &GridState,
        cells: usize,
        default_bg: [f32; 4],
    ) {
        unsafe {
            gl.use_program(Some(self.background.program));
            set_vec4(gl, &self.background, "u_default_bg", default_bg);
            set_i32(gl, &self.background, "u_cols", state.frame.grid[0] as i32);
            set_vec2(gl, &self.background, "u_origin", state.frame.origin);
            set_vec2(gl, &self.background, "u_cell", state.frame.cell);
            set_vec2(gl, &self.background, "u_viewport", viewport_points(state));
            gl.draw_arrays_instanced(glow::TRIANGLE_STRIP, 0, 4, cells as i32);
        }
    }

    /// Underlines and strikeouts, one instance per cell.  An undecorated cell
    /// collapses in the vertex shader, so the pass costs the instance count
    /// however few cells carry a line — which is why the caller skips it
    /// outright on a screen that carries none.
    unsafe fn draw_decorations(
        &self,
        gl: &glow::Context,
        state: &GridState,
        strip: glow::Texture,
        cells: usize,
    ) {
        unsafe {
            gl.use_program(Some(self.decoration.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(strip));
            set_i32(gl, &self.decoration, "u_decorations", 0);
            set_i32(gl, &self.decoration, "u_cols", state.frame.grid[0] as i32);
            set_i32(gl, &self.decoration, "u_tiles", state.frame.decoration_tiles as i32);
            set_vec2(gl, &self.decoration, "u_origin", state.frame.origin);
            set_vec2(gl, &self.decoration, "u_cell", state.frame.cell);
            set_vec2(gl, &self.decoration, "u_viewport", viewport_points(state));
            gl.draw_arrays_instanced(glow::TRIANGLE_STRIP, 0, 4, cells as i32);
        }
    }

    unsafe fn draw_glyphs(
        &self,
        gl: &glow::Context,
        state: &GridState,
        atlas: glow::Texture,
        atlas_size: [f32; 2],
        cells: usize,
        glyph: &Program,
    ) {
        unsafe {
            gl.use_program(Some(glyph.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.slot_texture));
            set_i32(gl, glyph, "u_atlas", 0);
            set_i32(gl, glyph, "u_slots", 1);
            set_i32(gl, glyph, "u_slots_per_row", SLOTS_PER_ROW as i32);
            set_i32(gl, glyph, "u_cols", state.frame.grid[0] as i32);
            set_vec2(gl, glyph, "u_origin", state.frame.origin);
            set_vec2(gl, glyph, "u_cell", state.frame.cell);
            set_vec2(gl, glyph, "u_atlas_size", atlas_size);
            set_vec2(gl, glyph, "u_viewport", viewport_points(state));

            gl.draw_arrays_instanced(glow::TRIANGLE_STRIP, 0, 4, cells as i32);
        }
    }
}

fn viewport_points(state: &GridState) -> [f32; 2] {
    let (cols, rows) = state.instances.dimensions();
    [cols as f32 * state.frame.cell[0], rows as f32 * state.frame.cell[1]]
}

unsafe fn set_i32(gl: &glow::Context, program: &Program, name: &str, value: i32) {
    if let Some(location) = program.location(name) {
        unsafe { gl.uniform_1_i32(Some(location), value) };
    }
}

unsafe fn set_vec2(gl: &glow::Context, program: &Program, name: &str, value: [f32; 2]) {
    if let Some(location) = program.location(name) {
        unsafe { gl.uniform_2_f32(Some(location), value[0], value[1]) };
    }
}

unsafe fn set_vec4(gl: &glow::Context, program: &Program, name: &str, value: [f32; 4]) {
    if let Some(location) = program.location(name) {
        unsafe { gl.uniform_4_f32(Some(location), value[0], value[1], value[2], value[3]) };
    }
}

/// Copy the slot table into `into`, padded to whole texture rows, and say how
/// many rows that is.
///
/// Texels run row-major, so slot `i` lands at `(i % SLOTS_PER_ROW, i /
/// SLOTS_PER_ROW)` — the address the vertex shader computes for it.
fn pad_slots(slots: &[GlyphSlot], into: &mut Vec<GlyphSlot>) -> usize {
    let rows = slots.len().div_ceil(SLOTS_PER_ROW);
    into.clear();
    into.extend_from_slice(slots);
    into.resize(rows * SLOTS_PER_ROW, GlyphSlot::default());
    rows
}

/// `&[T]` as bytes, for types that are plain data by construction.
fn bytemuck_cast<T>(slice: &[T]) -> &[u8] {
    // SAFETY: every caller passes a `#[repr(C)]` aggregate of integers and
    // floats, which has no padding to expose and no invalid bit patterns.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), std::mem::size_of_val(slice)) }
}

/// Give back everything a failed `link` created.  Deleting the program
/// detaches whatever is still attached to it, so the shaders flagged here are
/// released either way.
unsafe fn discard(gl: &glow::Context, program: glow::Program, shaders: &[glow::Shader]) {
    unsafe {
        for &shader in shaders {
            gl.delete_shader(shader);
        }
        gl.delete_program(program);
    }
}

unsafe fn link(
    gl: &glow::Context,
    header: &str,
    vertex: &str,
    fragment: &str,
) -> Result<Program, String> {
    unsafe {
        let program = gl.create_program()?;
        for (index, name) in ATTRIBUTES {
            gl.bind_attrib_location(program, index, name);
        }
        let mut shaders = Vec::new();
        for (kind, body) in [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)] {
            let shader = match gl.create_shader(kind) {
                Ok(shader) => shader,
                Err(err) => {
                    discard(gl, program, &shaders);
                    return Err(err);
                },
            };
            shaders.push(shader);
            gl.shader_source(shader, &format!("{header}{body}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                discard(gl, program, &shaders);
                return Err(log);
            }
            gl.attach_shader(program, shader);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            discard(gl, program, &shaders);
            return Err(log);
        }
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        let count = gl.get_active_uniforms(program);
        let mut uniforms = Vec::new();
        for index in 0..count {
            if let Some(uniform) = gl.get_active_uniform(program, index)
                && let Some(location) = gl.get_uniform_location(program, &uniform.name)
            {
                uniforms.push((uniform.name, location));
            }
        }
        Ok(Program { program, uniforms })
    }
}

const GLYPH_VERT: &str = r#"
uniform vec2 u_origin;
uniform vec2 u_cell;
uniform vec2 u_viewport;
uniform vec2 u_atlas_size;
uniform int u_cols;
uniform int u_slots_per_row;
uniform sampler2D u_slots;

in uint a_slot;
in vec4 a_fg;

out vec2 v_uv;
// Per-instance, so every vertex of the quad carries the same colour and
// interpolating it would compute a constant.  Alacritty qualifies the same
// values the same way (`res/glsl3/text.v.glsl`).
flat out vec4 v_fg;

void main() {
    int slot = int(a_slot);
    ivec2 at = ivec2((slot % u_slots_per_row) * 2, slot / u_slots_per_row);
    vec4 rect = texelFetch(u_slots, at, 0);
    vec4 geom = texelFetch(u_slots, at + ivec2(1, 0), 0);

    // Records sit at a fixed row stride, so the instance index is the cell.
    vec2 grid_cell = vec2(gl_InstanceID % u_cols, gl_InstanceID / u_cols);
    // 0 = top-left, 1 = top-right, 2 = bottom-left, 3 = bottom-right.
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    vec2 pos = u_origin + grid_cell * u_cell + geom.xy + corner * geom.zw;

    gl_Position = vec4(
        2.0 * pos.x / u_viewport.x - 1.0,
        1.0 - 2.0 * pos.y / u_viewport.y,
        0.0,
        1.0);
    v_uv = mix(rect.xy, rect.zw, corner) / u_atlas_size;
    v_fg = a_fg;
}
"#;

const GLYPH_FRAG: &str = r#"
uniform sampler2D u_atlas;
in vec2 v_uv;
flat in vec4 v_fg;
out vec4 f_color;

void main() {
    // Only alpha carries coverage, the same as the decoration strip.  epaint
    // writes every font texel as `from_rgba_premultiplied(a, a, a, a)` and
    // colour glyphs never reach this pass, so the colour channels hold nothing
    // alpha does not, and alpha is stored linearly whatever the driver did to
    // them.  egui converts all three back to gamma because its shader also
    // draws images, where they differ.
    //
    // Multiplied in gamma space, the same as egui's own text shader: it is the
    // only way glyph edges come out the weight the atlas was rasterized for.
    f_color = v_fg * texture(u_atlas, v_uv).a;
}
"#;

const BACKGROUND_VERT: &str = r#"
uniform vec2 u_origin;
uniform vec2 u_cell;
uniform vec2 u_viewport;
uniform int u_cols;
uniform vec4 u_default_bg;

in vec4 a_bg;

flat out vec4 v_bg;

void main() {
    // The whole grid rect was cleared to the default background, so a cell
    // still carrying it would repaint what is already there.  Collapsing to a
    // point costs one compare instead of a cell of fragments.
    vec2 size = a_bg == u_default_bg ? vec2(0.0) : u_cell;

    vec2 grid_cell = vec2(gl_InstanceID % u_cols, gl_InstanceID / u_cols);
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    vec2 pos = u_origin + grid_cell * u_cell + corner * size;

    gl_Position = vec4(
        2.0 * pos.x / u_viewport.x - 1.0,
        1.0 - 2.0 * pos.y / u_viewport.y,
        0.0,
        1.0);
    v_bg = a_bg;
}
"#;

const BACKGROUND_FRAG: &str = r#"
flat in vec4 v_bg;
out vec4 f_color;

void main() {
    f_color = v_bg;
}
"#;

const DECORATION_VERT: &str = r#"
uniform vec2 u_origin;
uniform vec2 u_cell;
uniform vec2 u_viewport;
uniform int u_cols;
uniform int u_tiles;

in vec4 a_fg;
in uint a_deco;

out vec2 v_uv;
flat out vec4 v_fg;

void main() {
    // An undecorated cell collapses to a point rather than reaching the
    // rasterizer with a cell of fragments that all sample an empty tile.
    vec2 size = a_deco == 0u ? vec2(0.0) : u_cell;

    vec2 grid_cell = vec2(gl_InstanceID % u_cols, gl_InstanceID / u_cols);
    // 0 = top-left, 1 = top-right, 2 = bottom-left, 3 = bottom-right.
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    vec2 pos = u_origin + grid_cell * u_cell + corner * size;

    gl_Position = vec4(
        2.0 * pos.x / u_viewport.x - 1.0,
        1.0 - 2.0 * pos.y / u_viewport.y,
        0.0,
        1.0);
    // Tiles sit side by side in one row, each a cell wide.
    v_uv = vec2((float(a_deco) + corner.x) / float(u_tiles), corner.y);
    v_fg = a_fg;
}
"#;

const DECORATION_FRAG: &str = r#"
uniform sampler2D u_decorations;
in vec2 v_uv;
flat in vec4 v_fg;
out vec4 f_color;

void main() {
    // The strip is a coverage mask, so only alpha carries shape: it is stored
    // linearly whatever the driver did to the colour channels.
    f_color = v_fg * texture(u_decorations, v_uv).a;
}
"#;

#[cfg(test)]
mod tests {
    use egui::Color32;

    use super::*;

    /// A resize moves every record, so nothing the GPU holds is still valid.
    #[test]
    fn a_resize_dirties_every_row() {
        let mut state = GridState::default();
        state.instances.resize(4, 3, Color32::BLACK);

        state.mark_all_dirty();

        assert_eq!(state.dirty_rows, 0..3);
    }

    /// Two damaged spans in one frame upload as one range: a second
    /// `glBufferSubData` costs more than the rows between them.
    #[test]
    fn two_damaged_spans_merge_into_one_upload() {
        let mut state = GridState::default();
        state.instances.resize(4, 10, Color32::BLACK);

        state.mark_rows_dirty(2..3);
        state.mark_rows_dirty(7..9);

        assert_eq!(state.dirty_rows, 2..9);
    }

    #[test]
    fn an_untouched_frame_uploads_nothing() {
        let state = GridState::default();

        assert!(state.dirty_rows.is_empty());
    }

    /// The shader reads slot `i` at `(i % SLOTS_PER_ROW, i / SLOTS_PER_ROW)`,
    /// which only finds it if the padding leaves every slot at its own index.
    #[test]
    fn padding_leaves_each_slot_at_its_own_index() {
        let marked = |n: usize| GlyphSlot { size: [n as f32, 0.0], ..GlyphSlot::default() };
        let slots: Vec<GlyphSlot> = (0..SLOTS_PER_ROW + 44).map(marked).collect();
        let mut padded = Vec::new();

        let rows = pad_slots(&slots, &mut padded);

        assert_eq!(rows, 2);
        assert_eq!(padded.len(), 2 * SLOTS_PER_ROW, "a row was left short of texels");
        assert_eq!(padded[SLOTS_PER_ROW + 43], marked(SLOTS_PER_ROW + 43));
        assert_eq!(padded[SLOTS_PER_ROW + 44], GlyphSlot::default(), "the tail carries data");
    }

    /// Why the atlas size is read in the paint callback and not while the
    /// frame is built: laying glyphs out doubles the atlas when it runs out of
    /// room, and egui normalizes every uv against the size the atlas ended the
    /// frame at.  A size read before the grid's own layout would be half the
    /// truth, and every glyph would sample from the wrong place.
    #[test]
    fn laying_glyphs_out_can_grow_the_atlas_mid_frame() {
        #[cfg(windows)]
        crate::harden_dll_search_path();

        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let before = ctx.fonts(|f| f.font_image_size());

        for ch in ('\u{20}'..='\u{4ff}').filter(|c| !c.is_control()) {
            let mut job = egui::text::LayoutJob::single_section(
                ch.to_string(),
                egui::TextFormat::simple(egui::FontId::monospace(72.0), Color32::WHITE),
            );
            job.wrap.max_width = f32::INFINITY;
            let _ = ctx.fonts(|f| f.layout_job(job));
        }

        assert_ne!(before, ctx.fonts(|f| f.font_image_size()));
    }
}

/// Covers the callback's rect with one triangle, so the default background can
/// be priced against `glClear` writing the same pixels.  Built only when
/// `[debug] gpu_ab = "quad"` asks for it, and never on the path a release
/// paints with.
///
/// The vertices are derived rather than fetched: a triangle twice the size of
/// the viewport covers it, and the scissor egui leaves enabled trims the
/// overhang the same way it trims the clear.
const CLEAR_VERT: &str = r#"
void main() {
    vec2 corner = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const CLEAR_FRAG: &str = r#"
uniform vec4 u_color;
out vec4 f_color;

void main() {
    f_color = u_color;
}
"#;

/// The glyph shader with the blank half of every atlas rectangle thrown away
/// rather than blended.  Built only when `[debug] gpu_ab = "fill"` asks to
/// price it, and never on the path a release paints with.
const GLYPH_FRAG_DISCARD: &str = r#"
uniform sampler2D u_atlas;
in vec2 v_uv;
flat in vec4 v_fg;
out vec4 f_color;

void main() {
    float coverage = texture(u_atlas, v_uv).a;
    // A glyph's rectangle is a box around its ink, and most of that box is
    // empty.  Blending a fragment at zero coverage produces the destination it
    // is about to overwrite, so the write can be dropped instead of made.
    if (coverage == 0.0) {
        discard;
    }
    f_color = v_fg * coverage;
}
"#;

/// The glyph shader as it stood before coverage was read from alpha: egui's
/// general conversion, which recovers it from the colour channels instead.
/// Built only when `[debug] gpu_ab = "glyphs"` asks to price the difference,
/// and never on the path a release paints with.
const GLYPH_FRAG_GAMMA: &str = r#"
uniform sampler2D u_atlas;
in vec2 v_uv;
flat in vec4 v_fg;
out vec4 f_color;

vec3 srgb_gamma_from_linear(vec3 rgb) {
    bvec3 cutoff = lessThan(rgb, vec3(0.0031308));
    vec3 lower = rgb * vec3(12.92);
    vec3 higher = vec3(1.055) * pow(rgb, vec3(1.0 / 2.4)) - vec3(0.055);
    return mix(higher, lower, vec3(cutoff));
}

void main() {
    vec4 tex = texture(u_atlas, v_uv);
    tex = vec4(srgb_gamma_from_linear(tex.rgb), tex.a);
    f_color = v_fg * tex;
}
"#;
