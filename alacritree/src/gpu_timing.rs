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
//!
//! The timers also drive the A/B for the callback's skips.  Two builds
//! launched in turn cannot resolve a few microseconds on a loaded machine,
//! because the round-to-round spread swamps it; alternating the arms between
//! report windows of one process puts them in front of the same driver, the
//! same grid and the same minute, and leaves the reader a pair of adjacent
//! lines.

use std::time::Duration;

use eframe::glow::{self, HasContext};
use schemars::JsonSchema;
use serde::Deserialize;

/// Which of the paint callback's skips the A/B alternates between report
/// windows.  One at a time: two flipping together would leave a pair differing
/// in two things, and neither difference attributable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Ab {
    /// Every window runs the shipped path.
    #[default]
    Off,
    /// Drawing the decoration pass over a grid that carries no decoration.
    Decorations,
    /// Drawing a background quad over a cell already cleared to its colour.
    Backgrounds,
    /// Recovering glyph coverage from the colour channels rather than alpha.
    Glyphs,
}

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
    gpu: [Vec<f64>; STAGES.len()],
    /// Every stage of one frame added up, for the frames that got all their
    /// answers back.  A per-stage median cannot be summed into this: the
    /// medians come from different frames.
    total: Vec<f64>,
    /// Wall time inside the callback, which no query can see.
    submit: Vec<f64>,
    /// Which skip is being A/B'd, if any.  Alternating the two arms between
    /// report windows of one process is what makes them comparable: they meet
    /// the same driver, the same grid and the same minute, where two binaries
    /// launched in turn meet neither.
    ab: Ab,
    /// The arm this window is running.  `true` is the baseline, drawing the
    /// pass under test the way the code did before its skip.
    arm: bool,
    /// Frames this window that skipped the decoration pass.  Without it the
    /// report cannot tell a gate that fired on every frame from one that never
    /// fired: the stage median describes only the frames that drew.
    skipped: usize,
    /// Frames left before this window's samples are its own.  A result arrives
    /// `DEPTH` frames after the draw that earned it, so the reads just after an
    /// arm flip describe the arm that ended, and counting them would credit one
    /// arm with the other's work.
    settling: usize,
}

impl GpuTimers {
    /// `None` on a context that cannot time itself.  The grid runs on anything
    /// from GL 3 up and timer queries arrive in 3.3, so this is a real case
    /// rather than a defensive one.
    pub fn new(gl: &glow::Context, ab: Ab) -> Option<Self> {
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
            total: Vec::new(),
            submit: Vec::new(),
            ab,
            arm: false,
            skipped: 0,
            settling: 0,
        })
    }

    /// True while the A/B is running its baseline arm, which draws the
    /// decoration pass whether or not any cell carries one.
    pub fn forces_decorations(&self) -> bool {
        self.ab == Ab::Decorations && self.arm
    }

    /// True while the A/B is running its baseline arm, which draws a quad for
    /// every cell whether or not its background differs from the clear.
    pub fn forces_backgrounds(&self) -> bool {
        self.ab == Ab::Backgrounds && self.arm
    }

    /// True while the A/B is running its baseline arm, which reads glyph
    /// coverage back out of the colour channels the way egui's shader does.
    pub fn forces_glyph_gamma(&self) -> bool {
        self.ab == Ab::Glyphs && self.arm
    }

    /// Whether a second glyph program has to be built for the baseline arm.
    /// The two shaders differ at compile time, so one program cannot serve
    /// both the way a uniform serves the background arms.
    pub fn wants_glyph_gamma(&self) -> bool {
        self.ab == Ab::Glyphs
    }

    /// Record that this frame drew no decorations, so the report can say how
    /// often the gate fired rather than only what a drawn pass cost.
    pub fn skipped_decorations(&mut self) {
        self.skipped += 1;
    }

    /// Collect whatever the slot about to be reused finished, so the frame
    /// that issued those queries is the one that paid for them.
    pub fn begin_frame(&mut self, gl: &glow::Context) {
        // The queries still have to be read even while settling, or the slot
        // is reused with a result outstanding.
        let stale = self.settling > 0;
        self.settling = self.settling.saturating_sub(1);
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
                if !stale {
                    self.gpu[stage].push(us);
                }
                total += us;
            }
        }
        // A frame one stage short has no total worth keeping: the sum of the
        // rest would read as a cheaper frame rather than an unfinished one.
        if ran && complete && !stale {
            self.total.push(total);
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
        // Windows are the unit of comparison when the A/B runs, so the arm has
        // to be on the line: two adjacent windows are the pair.
        let arm = match (self.ab, self.arm) {
            (Ab::Off, _) => "",
            (Ab::Decorations, true) => " [deco always]",
            (Ab::Decorations, false) => " [deco gated]",
            (Ab::Backgrounds, true) => " [bg always]",
            (Ab::Backgrounds, false) => " [bg gated]",
            (Ab::Glyphs, true) => " [glyph always]",
            (Ab::Glyphs, false) => " [glyph gated]",
        };
        let mut line = format!(
            "gpu grid{arm}, {} frames: submit {:.0}us",
            self.submit.len(),
            median(&mut self.submit)
        );
        line.push_str(&format!("  skipped {}/{}", self.skipped, self.submit.len()));
        match self.total.len() {
            0 => line.push_str("  total -"),
            _ => line.push_str(&format!("  total {:.0}us", median(&mut self.total))),
        }
        for (stage, name) in STAGES.iter().enumerate() {
            // A stage whose samples all came back unavailable has nothing to
            // say, and printing 0 would read as "free" rather than "unknown".
            match self.gpu[stage].len() {
                0 => line.push_str(&format!("  {name} -")),
                _ => line.push_str(&format!("  {name} {:.0}us", median(&mut self.gpu[stage]))),
            }
            self.gpu[stage].clear();
        }
        self.total.clear();
        self.skipped = 0;
        // Only the A/B moves the arm, so only the A/B has a boundary to settle
        // across; a steady run would throw away good samples every window.
        if self.ab != Ab::Off {
            self.settling = DEPTH;
        }
        self.submit.clear();
        self.arm = !self.arm;
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
