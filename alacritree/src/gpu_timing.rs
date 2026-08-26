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
    /// Which queries a slot actually issued.  A frame with no decoration
    /// strip runs three stages, not four, so this cannot be one flag per slot.
    issued: [[bool; STAGES.len()]; DEPTH],
    slot: usize,
    gpu: [Vec<f64>; STAGES.len()],
    /// Wall time inside the callback, which no query can see.
    submit: Vec<f64>,
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
        Some(Self {
            queries,
            issued: [[false; STAGES.len()]; DEPTH],
            slot: 0,
            gpu: std::array::from_fn(|_| Vec::new()),
            submit: Vec::new(),
        })
    }

    /// Collect whatever the slot about to be reused finished, so the frame
    /// that issued those queries is the one that paid for them.
    pub fn begin_frame(&mut self, gl: &glow::Context) {
        for stage in 0..STAGES.len() {
            if !std::mem::take(&mut self.issued[self.slot][stage]) {
                continue;
            }
            let query = self.queries[self.slot][stage];
            unsafe {
                // A driver still behind after `DEPTH` frames is better skipped
                // than waited on: the wait would be charged to the frame doing
                // the asking, not the frame that earned it.
                if gl.get_query_parameter_u32(query, glow::QUERY_RESULT_AVAILABLE) == 0 {
                    continue;
                }
                let ns = gl.get_query_parameter_u32(query, glow::QUERY_RESULT);
                self.gpu[stage].push(f64::from(ns) / 1000.0);
            }
        }
    }

    /// `GL_TIME_ELAPSED` queries cannot nest, so a stage has to end before the
    /// next one starts.
    pub fn begin(&mut self, gl: &glow::Context, stage: usize) {
        self.issued[self.slot][stage] = true;
        unsafe { gl.begin_query(glow::TIME_ELAPSED, self.queries[self.slot][stage]) };
    }

    pub fn end(&self, gl: &glow::Context) {
        unsafe { gl.end_query(glow::TIME_ELAPSED) };
    }

    pub fn end_frame(&mut self, submit: Duration) {
        self.submit.push(submit.as_secs_f64() * 1e6);
        self.slot = (self.slot + 1) % DEPTH;
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
        for (stage, name) in STAGES.iter().enumerate() {
            // A stage whose samples all came back unavailable has nothing to
            // say, and printing 0 would read as "free" rather than "unknown".
            match self.gpu[stage].len() {
                0 => line.push_str(&format!("  {name} -")),
                _ => line.push_str(&format!("  {name} {:.0}us", median(&mut self.gpu[stage]))),
            }
            self.gpu[stage].clear();
        }
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
