//! What the grid's paint callback costs on the GPU.
//!
//! Every other timer in this crate measures the producer: capture, the record
//! write, the upload staged in memory.  None of them reach past
//! `GlResources::draw`, because nothing outside a live paint callback holds a
//! `glow::Context`, so the consumer half of a frame has never had a number
//! against it.  A ranking built only on producer microseconds is a ranking of
//! half the frame.
//!
//! `GL_TIME_ELAPSED` measures the GPU executing a range of commands, which is
//! not the wall time of submitting them: a driver that validates state on the
//! client spends its frame somewhere the query cannot see.  Both are reported
//! for that reason.
//!
//! Results come back a few frames late.  Asking for a query on the frame that
//! issued it blocks until the GPU catches up, which would make the instrument
//! the slowest thing in the frame it is measuring.

use std::time::Duration;

use eframe::glow::{self, HasContext};

/// The command ranges timed separately, in the order the callback issues them.
const STAGES: [&str; 4] = ["upload", "backgrounds", "glyphs", "decorations"];

/// Frames of queries in flight.  A slot is read on the frame that reuses it,
/// by which point its work is long retired.
const DEPTH: usize = 3;

/// Frames gathered before a line is logged and the buckets start over.
const REPORT_EVERY: usize = 240;

pub struct GpuTimers {
    queries: [[glow::Query; STAGES.len()]; DEPTH],
    /// Which queries a slot actually issued.  A frame that skipped the
    /// decoration pass runs three stages, not four, so this cannot be one flag
    /// per slot.
    issued: [[bool; STAGES.len()]; DEPTH],
    slot: usize,
    /// One bracket around everything the callback issues.  The stages still do
    /// not add up to it: the vertex-array binds between them belong to no
    /// stage, and a bracket that ends at bottom-of-pipe charges its stage for a
    /// drain the next stage would otherwise have overlapped, which pushes the
    /// sum the other way.  Only a span measured on its own says by how much.
    /// `GL_TIME_ELAPSED` cannot nest, so this alternates with the per-stage
    /// queries frame by frame rather than wrapping them, and one window reports
    /// both.
    frame_queries: [glow::Query; DEPTH],
    frame_issued: [bool; DEPTH],
    frame: Vec<f64>,
    /// Which set of queries this frame issues.
    whole_frame: bool,
    gpu: [Vec<f64>; STAGES.len()],
    /// Every stage of one frame added up, for the frames that got all their
    /// answers back.  A per-stage median cannot be summed into this: the
    /// medians come from different frames.
    total: Vec<f64>,
    /// Wall time inside the callback, which no query can see.
    submit: Vec<f64>,
    /// Frames this window that skipped the decoration pass.  Without it the
    /// report cannot tell a gate that fired on every frame from one that never
    /// fired: the stage median describes only the frames that drew.
    skipped: usize,
    /// Cells the last frame drew, so a microsecond figure on the report line
    /// converts to a rate.
    grid: (usize, usize),
}

impl GpuTimers {
    /// `None` on a context that cannot time itself.  The grid runs on anything
    /// from GL 3 up and timer queries arrive in 3.3, so this is a real case
    /// rather than a defensive one.
    pub fn new(gl: &glow::Context) -> Option<Self> {
        let version = gl.version();
        let core = !version.is_embedded && (version.major, version.minor) >= (3, 3);
        let extension = gl.supported_extensions().iter().any(|name| name.contains("timer_query"));
        if !core && !extension {
            log::warn!("gpu grid timing asked for, but this context has no timer queries");
            return None;
        }
        let mut made: Vec<[glow::Query; STAGES.len()]> = Vec::with_capacity(DEPTH);
        for _ in 0..DEPTH {
            let mut slot = Vec::with_capacity(STAGES.len());
            for _ in 0..STAGES.len() {
                slot.push(unsafe { gl.create_query() }.ok()?);
            }
            made.push(slot.try_into().ok()?);
        }
        let queries = made.try_into().ok()?;
        let mut whole = Vec::with_capacity(DEPTH);
        for _ in 0..DEPTH {
            whole.push(unsafe { gl.create_query() }.ok()?);
        }
        Some(Self {
            queries,
            issued: [[false; STAGES.len()]; DEPTH],
            slot: 0,
            frame_queries: whole.try_into().ok()?,
            frame_issued: [false; DEPTH],
            frame: Vec::new(),
            whole_frame: false,
            gpu: std::array::from_fn(|_| Vec::new()),
            total: Vec::new(),
            submit: Vec::new(),
            grid: (0, 0),
            skipped: 0,
        })
    }

    /// Record that this frame drew no decorations, so the report can say how
    /// often the gate fired rather than only what a drawn pass cost.
    pub fn skipped_decorations(&mut self) {
        self.skipped += 1;
    }

    /// Collect whatever the slot about to be reused finished, so the frame
    /// that issued those queries is the one that paid for them.
    pub fn begin_frame(&mut self, gl: &glow::Context) {
        let (mut total, mut ran, mut complete) = (0.0, false, true);
        for stage in 0..STAGES.len() {
            if !std::mem::take(&mut self.issued[self.slot][stage]) {
                continue;
            }
            ran = true;
            let query = self.queries[self.slot][stage];
            unsafe {
                // A driver still behind after `DEPTH` frames is better skipped
                // than waited on: the wait would be charged to the frame doing
                // the asking, not the frame that earned it.
                if gl.get_query_parameter_u32(query, glow::QUERY_RESULT_AVAILABLE) == 0 {
                    complete = false;
                    continue;
                }
                let ns = gl.get_query_parameter_u32(query, glow::QUERY_RESULT);
                let us = f64::from(ns) / 1000.0;
                self.gpu[stage].push(us);
                total += us;
            }
        }
        // A frame still waiting on a query it issued has no total worth
        // keeping: the sum of the rest would read as a cheaper frame rather
        // than an unfinished one.  A stage the callback never issued is a
        // different case -- a gated decoration pass leaves a three-stage frame
        // that is complete as drawn, and its total counts.
        if ran && complete {
            self.total.push(total);
        }
        if std::mem::take(&mut self.frame_issued[self.slot]) {
            let query = self.frame_queries[self.slot];
            unsafe {
                if gl.get_query_parameter_u32(query, glow::QUERY_RESULT_AVAILABLE) != 0 {
                    let ns = gl.get_query_parameter_u32(query, glow::QUERY_RESULT);
                    self.frame.push(f64::from(ns) / 1000.0);
                }
            }
        }
    }

    /// Bracket everything the callback issues, the clear included, so the
    /// report can be read against the stages that are supposed to add up to it.
    /// Silent on the frames measuring stages, which cannot nest inside this.
    pub fn begin_whole(&mut self, gl: &glow::Context) {
        if !self.whole_frame {
            return;
        }
        self.frame_issued[self.slot] = true;
        unsafe { gl.begin_query(glow::TIME_ELAPSED, self.frame_queries[self.slot]) };
    }

    pub fn end_whole(&self, gl: &glow::Context) {
        if self.whole_frame {
            unsafe { gl.end_query(glow::TIME_ELAPSED) };
        }
    }

    /// `GL_TIME_ELAPSED` queries cannot nest, so a stage has to end before the
    /// next one starts, and a frame measuring the whole callback measures no
    /// stage at all.
    pub fn begin(&mut self, gl: &glow::Context, stage: usize) {
        if self.whole_frame {
            return;
        }
        self.issued[self.slot][stage] = true;
        unsafe { gl.begin_query(glow::TIME_ELAPSED, self.queries[self.slot][stage]) };
    }

    pub fn end(&self, gl: &glow::Context) {
        if !self.whole_frame {
            unsafe { gl.end_query(glow::TIME_ELAPSED) };
        }
    }

    pub fn end_frame(&mut self, submit: Duration, grid: (usize, usize)) {
        self.submit.push(submit.as_secs_f64() * 1e6);
        self.grid = grid;
        self.slot = (self.slot + 1) % DEPTH;
        self.whole_frame = !self.whole_frame;
        if self.submit.len() >= REPORT_EVERY {
            self.report();
        }
    }

    fn report(&mut self) {
        let mut line = format!(
            "gpu grid, {} frames: submit {:.0}us",
            self.submit.len(),
            median(&mut self.submit)
        );
        line.push_str(&format!("  skipped {}/{}", self.skipped, self.submit.len()));
        match self.total.len() {
            0 => line.push_str("  total -"),
            n => line.push_str(&format!("  total {:.0}us/{n}", median(&mut self.total))),
        }
        // Read against `total`, which is the stages added up.  What is left is
        // the binds between stages plus whatever a per-stage bracket charges
        // its stage for beyond the work inside it.
        match self.frame.len() {
            0 => line.push_str("  frame -"),
            n => line.push_str(&format!("  frame {:.0}us/{n}", median(&mut self.frame))),
        }
        for (stage, name) in STAGES.iter().enumerate() {
            // A stage whose samples all came back unavailable has nothing to
            // say, and printing 0 would read as "free" rather than "unknown".
            match self.gpu[stage].len() {
                0 => line.push_str(&format!("  {name} -")),
                n => line.push_str(&format!("  {name} {:.0}us/{n}", median(&mut self.gpu[stage]))),
            }
            self.gpu[stage].clear();
        }
        line.push_str(&format!("  grid {}x{}", self.grid.0, self.grid.1));
        self.total.clear();
        self.frame.clear();
        self.skipped = 0;
        self.submit.clear();
        log::info!("{line}");
    }
}

/// The stage index the callback passes to `begin`.
pub const UPLOAD: usize = 0;
pub const BACKGROUNDS: usize = 1;
pub const GLYPHS: usize = 2;
pub const DECORATIONS: usize = 3;

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN from a duration"));
    samples[samples.len() / 2]
}
