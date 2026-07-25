//! Opt-in frame timing, enabled with `ALACRITREE_FRAME_LOG=1`.
//!
//! The paint harnesses time the grid alone in a headless context, which says
//! nothing about the sidebars, event draining, or git status that share the
//! same frame.  This measures whole frames in the app that is actually
//! lagging, and splits off the grid so the two can be compared.
//!
//! `update` is only part of a frame: eframe tessellates, uploads and presents
//! after it returns.  So the period between two frame starts is recorded
//! alongside eframe's own `cpu_usage`, which covers rendering but stops short
//! of the vsync wait.  A period far above the CPU time, while frames are being
//! produced at the display's cap, is time blocked on present rather than time
//! spent working.
//!
//! Disabled it costs one `Option` check per frame, one flag read per PTY
//! wakeup, and never allocates.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How often the accumulated frames are summarized.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// Whether `ALACRITREE_FRAME_LOG` asked for measurements.  Read by the PTY
/// threads, which have no handle on the `FrameLog` the UI thread owns.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("ALACRITREE_FRAME_LOG")
            .is_some_and(|v| !matches!(v.to_str(), Some("0") | Some("")))
    })
}

/// Terminal output waiting to reach the screen, as nanoseconds since
/// `epoch()`; zero when the last frame already painted everything.
static OUTPUT_PENDING: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Note that a session produced output that no frame has shown yet.
///
/// Called from the PTY threads.
pub fn output_arrived() {
    if enabled() {
        mark_pending(&OUTPUT_PENDING, now());
    }
}

/// How long the output this frame is about to paint has been waiting.
///
/// Read before the frame builds anything, so output that arrives mid-frame
/// stays pending rather than being credited to a paint that may have missed
/// it.
pub fn output_wait() -> Option<Duration> {
    take_pending(&OUTPUT_PENDING, now())
}

fn now() -> u64 {
    epoch().elapsed().as_nanos() as u64
}

/// Only the oldest pending output is kept, so what a frame reports is the
/// longest any of it waited, not the shortest.  Zero means nothing is pending,
/// so an arrival at that exact nanosecond is nudged forward by one.
fn mark_pending(slot: &AtomicU64, arrived: u64) {
    let _ = slot.compare_exchange(0, arrived.max(1), Ordering::Relaxed, Ordering::Relaxed);
}

fn take_pending(slot: &AtomicU64, painted: u64) -> Option<Duration> {
    match slot.swap(0, Ordering::Relaxed) {
        0 => None,
        arrived => Some(Duration::from_nanos(painted.saturating_sub(arrived))),
    }
}

#[derive(Default)]
struct Samples {
    totals: Vec<Duration>,
    grids: Vec<Duration>,
    periods: Vec<Duration>,
    cpus: Vec<Duration>,
    waits: Vec<Duration>,
}

/// One frame's readings, gathered across `update` and handed over at the end.
pub struct Timings {
    pub started: Instant,
    pub grid: Duration,
    /// eframe's reading for the frame *before* this one; the off-by-one does
    /// not survive into a percentile over thousands of frames.
    pub cpu: Option<Duration>,
    pub waited: Option<Duration>,
}

pub struct FrameLog {
    samples: Samples,
    started_previous: Option<Instant>,
    reported_at: Instant,
}

impl FrameLog {
    /// A log if `ALACRITREE_FRAME_LOG` is set to anything but `0`, otherwise
    /// nothing — the caller keeps an `Option` so a normal run pays nothing.
    pub fn from_env() -> Option<Self> {
        enabled().then(|| Self {
            samples: Samples::default(),
            started_previous: None,
            reported_at: Instant::now(),
        })
    }

    pub fn record(&mut self, frame: Timings) {
        let Timings { started, grid, cpu, waited } = frame;
        self.samples.totals.push(started.elapsed());
        self.samples.grids.push(grid);
        self.samples.cpus.extend(cpu);
        self.samples.waits.extend(waited);
        if let Some(previous) = self.started_previous.replace(started) {
            self.samples.periods.push(started.saturating_duration_since(previous));
        }

        if self.reported_at.elapsed() >= REPORT_EVERY {
            self.report();
        }
    }

    fn report(&mut self) {
        let elapsed = self.reported_at.elapsed();
        self.reported_at = Instant::now();
        let Samples { mut totals, mut grids, mut periods, mut cpus, mut waits } =
            std::mem::take(&mut self.samples);
        if totals.is_empty() {
            return;
        }

        totals.sort_unstable();
        grids.sort_unstable();
        periods.sort_unstable();
        cpus.sort_unstable();
        waits.sort_unstable();
        let spent: Duration = totals.iter().sum();

        log::info!(
            "frames: {} in {:.1}s ({:.0}/s, {:.0}% of the thread) | total p50 {:?} p95 {:?} p99 \
             {:?} max {:?} | grid p50 {:?} p95 {:?} | period p50 {:?} p95 {:?} | render+update \
             p50 {:?} p95 {:?} | output waited p50 {:?} p95 {:?} max {:?}",
            totals.len(),
            elapsed.as_secs_f64(),
            totals.len() as f64 / elapsed.as_secs_f64(),
            100.0 * spent.as_secs_f64() / elapsed.as_secs_f64(),
            Reading(quantile(&totals, 0.50)),
            Reading(quantile(&totals, 0.95)),
            Reading(quantile(&totals, 0.99)),
            totals[totals.len() - 1],
            Reading(quantile(&grids, 0.50)),
            Reading(quantile(&grids, 0.95)),
            Reading(quantile(&periods, 0.50)),
            Reading(quantile(&periods, 0.95)),
            Reading(quantile(&cpus, 0.50)),
            Reading(quantile(&cpus, 0.95)),
            Reading(quantile(&waits, 0.50)),
            Reading(quantile(&waits, 0.95)),
            Reading(waits.last().copied()),
        );
    }
}

/// A percentile of an empty sample has no value; `-` says so without the
/// `Some(..)` wrapper `Option`'s own `Debug` would print.
struct Reading(Option<Duration>);

impl std::fmt::Debug for Reading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(value) => write!(f, "{value:?}"),
            None => f.write_str("-"),
        }
    }
}

fn quantile(sorted: &[Duration], q: f64) -> Option<Duration> {
    let last = sorted.len().checked_sub(1)?;
    Some(sorted[((sorted.len() as f64 * q) as usize).min(last)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> FrameLog {
        FrameLog {
            samples: Samples::default(),
            started_previous: None,
            reported_at: Instant::now(),
        }
    }

    fn frame(started: Instant) -> Timings {
        Timings { started, grid: Duration::ZERO, cpu: None, waited: None }
    }

    #[test]
    fn quantiles_index_within_the_sample() {
        let sorted: Vec<Duration> = (0..100).map(Duration::from_millis).collect();

        assert_eq!(quantile(&sorted, 0.0), Some(Duration::from_millis(0)));
        assert_eq!(quantile(&sorted, 0.5), Some(Duration::from_millis(50)));
        assert_eq!(quantile(&sorted, 1.0), Some(Duration::from_millis(99)));
    }

    /// A single frame is the whole distribution, and the percentile maths must
    /// not index past it.
    #[test]
    fn a_single_frame_reports_without_panicking() {
        let sorted = [Duration::from_millis(7)];

        assert_eq!(quantile(&sorted, 0.99), Some(Duration::from_millis(7)));
    }

    #[test]
    fn a_percentile_of_nothing_has_no_value() {
        assert_eq!(quantile(&[], 0.5), None);
    }

    /// The point of the period is the gap the frame did *not* spend in
    /// `update`, so it runs start-to-start, not end-to-start.
    #[test]
    fn a_frames_period_spans_from_the_previous_frames_start() {
        let mut log = log();
        let first = Instant::now();

        log.record(frame(first));
        log.record(frame(first + Duration::from_millis(9)));

        assert_eq!(log.samples.periods, [Duration::from_millis(9)]);
    }

    /// Nothing preceded the first frame, so it contributes a total but no gap.
    #[test]
    fn the_first_frame_contributes_no_period() {
        let mut log = log();

        log.record(frame(Instant::now()));

        assert_eq!(log.samples.totals.len(), 1);
        assert!(log.samples.periods.is_empty());
    }

    /// eframe has no reading for its first frame, and a missing one must not
    /// shift the others by taking a slot in the sample.
    #[test]
    fn a_missing_cpu_reading_is_left_out_of_the_sample() {
        let mut log = log();
        let started = Instant::now();

        log.record(frame(started));
        log.record(Timings { cpu: Some(Duration::from_millis(3)), ..frame(started) });

        assert_eq!(log.samples.cpus, [Duration::from_millis(3)]);
    }

    /// Output that piles up between frames is reported at the age of the
    /// oldest piece: that is how long the screen was actually behind.
    #[test]
    fn the_oldest_pending_output_sets_the_wait() {
        let slot = AtomicU64::new(0);

        mark_pending(&slot, 1_000);
        mark_pending(&slot, 4_000);

        assert_eq!(take_pending(&slot, 9_000), Some(Duration::from_nanos(8_000)));
    }

    /// The wait belongs to output that was already pending when the frame
    /// began, so a second frame with nothing new to show reports no wait.
    #[test]
    fn output_that_reaches_a_frame_stops_being_pending() {
        let slot = AtomicU64::new(0);
        mark_pending(&slot, 1_000);

        assert!(take_pending(&slot, 2_000).is_some());
        assert_eq!(take_pending(&slot, 3_000), None);
    }

    /// Zero is the "nothing pending" marker, so output arriving on that exact
    /// nanosecond must still register.
    #[test]
    fn output_arriving_at_the_epoch_still_counts_as_pending() {
        let slot = AtomicU64::new(0);

        mark_pending(&slot, 0);

        assert!(take_pending(&slot, 5_000).is_some());
    }
}
