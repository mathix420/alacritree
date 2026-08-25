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
//! Two draws over one buffer, neither carrying any geometry: a quad instanced
//! once per cell for the backgrounds, then the same again for the glyphs.
//! Underlines, emoji and box-drawing glyphs stay on egui's painter — they
//! carry their own textures or their own geometry, and they are rare enough
//! to leave.

use std::sync::{Arc, Mutex};

use eframe::egui_glow::ShaderVersion;
use eframe::glow::{self, HasContext};
use egui::Rect;

use crate::grid_instances::{GlyphInstance, GlyphSlot, GlyphTable, GridInstances};

/// Attribute locations, bound before linking so both programs read the same
/// record the same way.  `#version 140` has no `layout(location = ...)`, so
/// the binding has to come from this side.
const ATTRIBUTES: [(u32, &str); 3] = [(0, "a_slot"), (1, "a_fg"), (2, "a_bg")];

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
    /// Height of an underline and a strikeout, in points.
    pub line_thickness: f32,
    /// The terminal's own background, as the clear colour.  A grid rect is
    /// rarely an exact multiple of a cell, and the cell quads stop at the last
    /// whole one, so the strip past it is filled by clearing rather than by a
    /// shape epaint would have to tessellate every frame.
    pub default_bg: [f32; 4],
}

/// The CPU half, written by the UI thread and read by the paint callback.
///
/// Both run on the same thread under eframe, so the lock is never contended;
/// it exists because `egui_glow::CallbackFn` demands `Send + Sync`.
#[derive(Default)]
pub struct GridState {
    pub instances: GridInstances,
    pub table: GlyphTable,
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
        }
    }

    /// The shape to hand egui.  Everything it draws comes from `state`, which
    /// the caller has already written this frame — except the atlas size,
    /// which only the atlas live at paint time can give.
    pub fn callback(&self, rect: Rect, ctx: &egui::Context) -> egui::Shape {
        let (state, resources, ctx) = (self.state.clone(), self.gl.clone(), ctx.clone());
        egui::Shape::Callback(egui::epaint::PaintCallback {
            rect,
            callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |_info, painter| {
                let mut held = resources.lock().expect("gl resources");
                let gl = painter.gl().clone();
                if let GlSlot::Unbuilt = *held {
                    *held = match GlResources::new(&gl) {
                        Ok(resources) => GlSlot::Ready(resources),
                        Err(err) => {
                            log::error!("gpu grid disabled: {err}");
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
                resources.draw(&gl, &mut state, atlas, size.map(|side| side as f32));
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
    background: Program,
    vao: glow::VertexArray,
    instances: glow::Buffer,
    instance_capacity: usize,
    slot_texture: glow::Texture,
    /// The slot table padded to whole texture rows, kept across uploads so a
    /// table that grew by one glyph does not allocate to send itself.
    slot_scratch: Vec<GlyphSlot>,
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
    fn new(gl: &glow::Context) -> Result<Self, String> {
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
        // egui uploads its atlas as `SRGB8_ALPHA8` wherever the driver admits
        // to an sRGB extension, and its own fragment shader converts back to
        // gamma before multiplying.  Making the same choice here is what keeps
        // glyph weight identical to the rest of the UI, so the flag is compiled
        // into the shader rather than kept around.
        let srgb_atlas =
            gl.supported_extensions().iter().any(|extension| extension.contains("sRGB"))
                || version == ShaderVersion::Es300;

        unsafe {
            let glyph = link(gl, header, GLYPH_VERT, GLYPH_FRAG, srgb_atlas)?;
            let background = match link(gl, header, BACKGROUND_VERT, BACKGROUND_FRAG, srgb_atlas) {
                Ok(program) => program,
                Err(err) => {
                    gl.delete_program(glyph.program);
                    return Err(err);
                },
            };
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
            Ok(Self {
                glyph,
                background,
                vao,
                instances,
                instance_capacity: 0,
                slot_texture,
                slot_scratch: Vec::new(),
            })
        }
    }

    fn draw(
        &mut self,
        gl: &glow::Context,
        state: &mut GridState,
        atlas: Option<glow::Texture>,
        atlas_size: [f32; 2],
    ) {
        let (cols, rows) = state.instances.dimensions();
        if cols == 0 || rows == 0 {
            return;
        }
        unsafe {
            // egui scissors the callback to its clip rect before handing over,
            // so this reaches the grid and nothing around it.
            let [r, g, b, a] = state.frame.default_bg;
            gl.clear_color(r, g, b, a);
            gl.clear(glow::COLOR_BUFFER_BIT);

            self.upload(gl, state);
            gl.bind_vertex_array(Some(self.vao));
            self.bind_records(gl);

            self.draw_backgrounds(gl, state, cols * rows);
            if let Some(atlas) = atlas {
                self.draw_glyphs(gl, state, atlas, atlas_size, cols * rows);
            }

            for (index, _) in ATTRIBUTES {
                gl.disable_vertex_attrib_array(index);
            }
            gl.bind_vertex_array(None);
        }
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
            for (index, _) in ATTRIBUTES {
                gl.vertex_attrib_divisor(index, 1);
            }
        }
    }

    unsafe fn draw_backgrounds(&self, gl: &glow::Context, state: &GridState, cells: usize) {
        unsafe {
            gl.use_program(Some(self.background.program));
            set_i32(gl, &self.background, "u_cols", state.frame.grid[0] as i32);
            set_vec2(gl, &self.background, "u_origin", state.frame.origin);
            set_vec2(gl, &self.background, "u_cell", state.frame.cell);
            set_vec2(gl, &self.background, "u_viewport", viewport_points(state));
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
    ) {
        unsafe {
            gl.use_program(Some(self.glyph.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.slot_texture));
            set_i32(gl, &self.glyph, "u_atlas", 0);
            set_i32(gl, &self.glyph, "u_slots", 1);
            set_i32(gl, &self.glyph, "u_slots_per_row", SLOTS_PER_ROW as i32);
            set_i32(gl, &self.glyph, "u_cols", state.frame.grid[0] as i32);
            set_vec2(gl, &self.glyph, "u_origin", state.frame.origin);
            set_vec2(gl, &self.glyph, "u_cell", state.frame.cell);
            set_vec2(gl, &self.glyph, "u_atlas_size", atlas_size);
            set_vec2(gl, &self.glyph, "u_viewport", viewport_points(state));

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
    srgb_atlas: bool,
) -> Result<Program, String> {
    unsafe {
        let defines = format!("#define SRGB_TEXTURES {}\n", srgb_atlas as i32);
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
            gl.shader_source(shader, &format!("{header}{defines}{body}"));
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
out vec4 v_fg;

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
in vec4 v_fg;
out vec4 f_color;

vec3 srgb_gamma_from_linear(vec3 rgb) {
    bvec3 cutoff = lessThan(rgb, vec3(0.0031308));
    vec3 lower = rgb * vec3(12.92);
    vec3 higher = vec3(1.055) * pow(rgb, vec3(1.0 / 2.4)) - vec3(0.055);
    return mix(higher, lower, vec3(cutoff));
}

void main() {
    vec4 tex = texture(u_atlas, v_uv);
#if SRGB_TEXTURES
    tex = vec4(srgb_gamma_from_linear(tex.rgb), tex.a);
#endif
    // Multiplied in gamma space, the same as egui's own text shader: it is the
    // only way glyph edges come out the weight the atlas was rasterized for.
    f_color = v_fg * tex;
}
"#;

const BACKGROUND_VERT: &str = r#"
uniform vec2 u_origin;
uniform vec2 u_cell;
uniform vec2 u_viewport;
uniform int u_cols;

in vec4 a_bg;

out vec4 v_bg;

void main() {
    vec2 grid_cell = vec2(gl_InstanceID % u_cols, gl_InstanceID / u_cols);
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    vec2 pos = u_origin + (grid_cell + corner) * u_cell;

    gl_Position = vec4(
        2.0 * pos.x / u_viewport.x - 1.0,
        1.0 - 2.0 * pos.y / u_viewport.y,
        0.0,
        1.0);
    v_bg = a_bg;
}
"#;

const BACKGROUND_FRAG: &str = r#"
in vec4 v_bg;
out vec4 f_color;

void main() {
    f_color = v_bg;
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
