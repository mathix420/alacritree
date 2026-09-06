//! Opt-in frame timing, enabled with `[debug] frame_log` or
//! `ALACRITREE_FRAME_LOG=1`.
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

use std::ffi::OsStr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How often the accumulated frames are summarized.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// Whether measurements were asked for.  Process-wide because the PTY threads
/// read it and have no handle on the `FrameLog` the UI thread owns.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// What `ALACRITREE_FRAME_LOG` says, or `None` when it is unset.  It wins over
/// `[debug] frame_log`: the variable is the only switch available before the
/// config parses.
fn env_override(raw: Option<&OsStr>) -> Option<bool> {
    raw.map(|v| !matches!(v.to_str(), Some("0") | Some("")))
}

/// Publish the config's answer.  Run this before the first session spawns: the
/// PTY threads read the flag without synchronizing against startup, so a value
/// stored after they start is ignored rather than refused.
pub fn set_enabled(from_config: bool) {
    let asked = env_override(std::env::var_os("ALACRITREE_FRAME_LOG").as_deref());
    ENABLED.store(asked.unwrap_or(from_config), Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Terminal output waiting to reach the screen, as nanoseconds since
/// `epoch()`; zero when the last frame already painted everything.
static OUTPUT_PENDING: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// When the visible session last had a keystroke written to its PTY, as
/// nanoseconds since `epoch()`; zero when nothing is awaiting an echo.
static KEYSTROKE_AT: AtomicU64 = AtomicU64::new(0);

/// The round trip of the most recent keystroke, waiting to be sampled.
static ECHO: AtomicU64 = AtomicU64::new(0);

/// Note that a keystroke went to the visible session's PTY.
///
/// The newest keystroke replaces the last, so a burst measures the round trip
/// of the character still outstanding rather than of the one that started it.
pub fn keystroke_sent() {
    if enabled() {
        KEYSTROKE_AT.store(now().max(1), Ordering::Relaxed);
    }
}

/// Note that a session produced output that no frame has shown yet.
///
/// Called from the PTY threads.
pub fn output_arrived() {
    if !enabled() {
        return;
    }
    let now = now();
    mark_pending(&OUTPUT_PENDING, now);

    close_echo(&KEYSTROKE_AT, &ECHO, now);
}

/// The round trip of the last keystroke to be echoed, once.
pub fn echo() -> Option<Duration> {
    take_echo(&ECHO)
}

/// Close the round trip of the keystroke still outstanding, if any.
///
/// Everything between the write and here is outside this process: the ConPTY
/// round trip, the child's own redraw, and alacritty's parse.  It is the one
/// segment of a keystroke's journey the frame timings cannot see.  Output the
/// keystroke did not cause closes the trip early, so this bounds the segment
/// from below.
fn close_echo(sent_at: &AtomicU64, echo: &AtomicU64, arrived: u64) {
    let sent = sent_at.swap(0, Ordering::Relaxed);
    if sent != 0 {
        echo.store(arrived.saturating_sub(sent).max(1), Ordering::Relaxed);
    }
}

fn take_echo(echo: &AtomicU64) -> Option<Duration> {
    match echo.swap(0, Ordering::Relaxed) {
        0 => None,
        nanos => Some(Duration::from_nanos(nanos)),
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

/// A frame this slow is felt as a hitch rather than seen as a frame rate, so
/// it is worth a line of its own naming what consumed it.
const SLOW_FRAME: Duration = Duration::from_millis(15);

/// One piece of work worth naming inside a slow frame.  Below the frame
/// threshold, so the thing that caused a hitch is named even when it shared
/// the frame with something bigger.
const SLOW_PHASE: Duration = Duration::from_millis(10);

/// Phases that fit in one frame's breakdown.  Marks past this are dropped:
/// the breakdown is a debugging aid, not a reason to allocate mid-frame.
const MAX_PHASES: usize = 16;

/// Where one frame's time went.
///
/// A stall that strikes once every few seconds does not move any percentile,
/// so the summary cannot find it.  This names the phase it happened in.
pub struct Phases {
    marks: [(&'static str, Duration); MAX_PHASES],
    len: usize,
    since: Instant,
    on: bool,
}

impl Phases {
    pub fn new() -> Self {
        Self {
            marks: [("", Duration::ZERO); MAX_PHASES],
            len: 0,
            since: Instant::now(),
            on: enabled(),
        }
    }

    pub fn restart(&mut self) {
        self.len = 0;
        self.since = Instant::now();
    }

    /// Close the phase that ended here, naming it.
    pub fn mark(&mut self, name: &'static str) {
        if !self.on || self.len == MAX_PHASES {
            return;
        }
        let now = Instant::now();
        self.marks[self.len] = (name, now.saturating_duration_since(self.since));
        self.since = now;
        self.len += 1;
    }

    pub fn report_if_slow(&self) {
        let marks = &self.marks[..self.len];
        let total: Duration = marks.iter().map(|(_, d)| *d).sum();
        if total < SLOW_FRAME {
            return;
        }

        let mut ranked: Vec<_> =
            marks.iter().filter(|(_, d)| *d >= Duration::from_millis(1)).collect();
        ranked.sort_unstable_by_key(|(_, d)| std::cmp::Reverse(*d));
        let breakdown =
            ranked.iter().map(|(name, d)| format!("{name} {d:?}")).collect::<Vec<_>>().join(", ");
        log::info!("slow frame: {total:?} | {breakdown}");
    }
}

/// Name the individual piece of work that made a phase slow.
///
/// A phase is only as useful as its narrowest name: "ipc 101 ms" says the
/// stall is on the socket path, not which request sat on the UI thread.
pub fn note_if_slow(kind: &str, what: impl std::fmt::Debug, took: Duration) {
    if enabled() && took >= SLOW_PHASE {
        log::info!("slow {kind}: {what:?} {took:?}");
    }
}

/// One phase of opening a session, logged only under `ALACRITREE_FRAME_LOG`.
///
/// Spawn cost is charged to whichever frame phase happened to be running when
/// the click arrived, so without a marker of its own it reads as a sidebar or
/// shortcut problem.  The session id is what pairs a phase with the tab it
/// belongs to when several are opening at once.
pub fn spawn_phase(session: Option<u64>, phase: &str, elapsed: Duration) {
    if !enabled() {
        return;
    }
    let millis = elapsed.as_secs_f64() * 1000.0;
    match session {
        Some(id) => log::info!("spawn {phase} [{id}]: {millis:.1}ms"),
        None => log::info!("spawn {phase}: {millis:.1}ms"),
    }
}

#[derive(Default)]
struct Samples {
    totals: Vec<Duration>,
    grids: Vec<Duration>,
    periods: Vec<Duration>,
    cpus: Vec<Duration>,
    waits: Vec<Duration>,
    echoes: Vec<Duration>,
}

/// One frame's readings, gathered across `update` and handed over at the end.
pub struct Timings {
    pub started: Instant,
    pub grid: Duration,
    /// eframe's reading for the frame *before* this one; the off-by-one does
    /// not survive into a percentile over thousands of frames.
    pub cpu: Option<Duration>,
    pub waited: Option<Duration>,
    pub echo: Option<Duration>,
}

pub struct FrameLog {
    samples: Samples,
    started_previous: Option<Instant>,
    reported_at: Instant,
}

impl FrameLog {
    /// A log if measurements were asked for, otherwise nothing, so a normal run
    /// pays one `Option` check per frame.  Reads the flag `set_enabled` stores,
    /// so a `FrameLog` built before that call measures nothing.
    pub fn start() -> Option<Self> {
        enabled().then(|| Self {
            samples: Samples::default(),
            started_previous: None,
            reported_at: Instant::now(),
        })
    }

    pub fn record(&mut self, frame: Timings) {
        let Timings { started, grid, cpu, waited, echo } = frame;
        self.samples.totals.push(started.elapsed());
        self.samples.grids.push(grid);
        self.samples.cpus.extend(cpu);
        self.samples.waits.extend(waited);
        self.samples.echoes.extend(echo);
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
        let Samples { mut totals, mut grids, mut periods, mut cpus, mut waits, mut echoes } =
            std::mem::take(&mut self.samples);
        if totals.is_empty() {
            return;
        }

        totals.sort_unstable();
        grids.sort_unstable();
        periods.sort_unstable();
        cpus.sort_unstable();
        waits.sort_unstable();
        echoes.sort_unstable();
        let spent: Duration = totals.iter().sum();

        log::info!(
            "frames: {} in {:.1}s ({:.0}/s, {:.0}% of the thread) | total p50 {:?} p95 {:?} p99 \
             {:?} max {:?} | grid p50 {:?} p95 {:?} | period p50 {:?} p95 {:?} | render+update \
             p50 {:?} p95 {:?} | output waited p50 {:?} p95 {:?} max {:?} | echo n={} p50 {:?} \
             p95 {:?} p99 {:?}",
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
            echoes.len(),
            Reading(quantile(&echoes, 0.50)),
            Reading(quantile(&echoes, 0.95)),
            Reading(quantile(&echoes, 0.99)),
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
        Timings { started, grid: Duration::ZERO, cpu: None, waited: None, echo: None }
    }

    #[test]
    fn an_unset_variable_leaves_the_decision_to_the_config() {
        assert_eq!(env_override(None), None);
    }

    #[test]
    fn the_variable_turns_measurements_off_as_well_as_on() {
        assert_eq!(env_override(Some(OsStr::new("0"))), Some(false));
        assert_eq!(env_override(Some(OsStr::new(""))), Some(false));
        assert_eq!(env_override(Some(OsStr::new("1"))), Some(true));
        assert_eq!(env_override(Some(OsStr::new("yes"))), Some(true));
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

    /// The round trip runs from the keystroke's write to the output that
    /// answered it.
    #[test]
    fn output_after_a_keystroke_closes_its_round_trip() {
        let (sent_at, echo) = (AtomicU64::new(2_000), AtomicU64::new(0));

        close_echo(&sent_at, &echo, 9_000);

        assert_eq!(take_echo(&echo), Some(Duration::from_nanos(7_000)));
    }

    /// Output nobody typed for is most of what a terminal produces, and it
    /// must not enter the sample as a round trip of its own.
    #[test]
    fn output_with_no_keystroke_outstanding_reports_nothing() {
        let (sent_at, echo) = (AtomicU64::new(0), AtomicU64::new(0));

        close_echo(&sent_at, &echo, 9_000);

        assert_eq!(take_echo(&echo), None);
    }

    /// One keystroke is one round trip: the output that keeps coming after it
    /// is the child still redrawing, not further keystrokes being answered.
    #[test]
    fn only_the_first_output_after_a_keystroke_counts() {
        let (sent_at, echo) = (AtomicU64::new(2_000), AtomicU64::new(0));

        close_echo(&sent_at, &echo, 9_000);
        close_echo(&sent_at, &echo, 40_000);

        assert_eq!(take_echo(&echo), Some(Duration::from_nanos(7_000)));
    }

    /// Each mark closes the phase that ran since the previous one, so the
    /// phases partition the frame instead of all dating from its start.
    ///
    /// The first phase is back-dated rather than slept through, so the second
    /// one has seconds of room before it could be mistaken for a phase still
    /// dating from the frame's start.
    #[test]
    fn each_phase_measures_only_its_own_span() {
        const AGED: Duration = Duration::from_secs(5);

        let mut phases = Phases { on: true, ..Phases::new() };

        phases.restart();
        phases.since = phases.since.checked_sub(AGED).expect("a clock older than the back-date");
        phases.mark("first");
        phases.mark("second");

        let [(_, first), (_, second)] = phases.marks[..2] else { unreachable!() };
        assert!(first >= AGED, "the first phase lost the span it was given: {first:?}");
        assert!(second < first, "the second phase still dated from the frame's start: {second:?}");
    }

    /// A frame with more phases than the breakdown holds must drop the extras
    /// rather than run off the end of the array.
    #[test]
    fn marks_past_the_last_slot_are_dropped() {
        let mut phases = Phases { on: true, ..Phases::new() };

        for _ in 0..MAX_PHASES + 5 {
            phases.mark("phase");
        }

        assert_eq!(phases.len, MAX_PHASES);
    }

    /// Disabled, a mark must not even read the clock.
    #[test]
    fn a_disabled_breakdown_records_nothing() {
        let mut phases = Phases::new();
        phases.on = false;

        phases.mark("phase");

        assert_eq!(phases.len, 0);
    }
}
