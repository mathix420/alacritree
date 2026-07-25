//! Opt-in frame timing, enabled with `ALACRITREE_FRAME_LOG=1`.
//!
//! The paint harnesses time the grid alone in a headless context, which says
//! nothing about the sidebars, event draining, or git status that share the
//! same frame.  This measures whole frames in the app that is actually
//! lagging, and splits off the grid so the two can be compared.
//!
//! Disabled it costs one `Option` check per frame and never allocates.

use std::time::{Duration, Instant};

/// How often the accumulated frames are summarized.
const REPORT_EVERY: Duration = Duration::from_secs(5);

pub struct FrameLog {
    frames: Vec<(Duration, Duration)>,
    reported_at: Instant,
}

impl FrameLog {
    /// A log if `ALACRITREE_FRAME_LOG` is set to anything but `0`, otherwise
    /// nothing — the caller keeps an `Option` so a normal run pays nothing.
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var_os("ALACRITREE_FRAME_LOG")
            .is_some_and(|v| !matches!(v.to_str(), Some("0") | Some("")));
        enabled.then(|| Self { frames: Vec::new(), reported_at: Instant::now() })
    }

    pub fn record(&mut self, total: Duration, grid: Duration) {
        self.frames.push((total, grid));
        if self.reported_at.elapsed() >= REPORT_EVERY {
            self.report();
        }
    }

    fn report(&mut self) {
        let elapsed = self.reported_at.elapsed();
        self.reported_at = Instant::now();
        let frames = std::mem::take(&mut self.frames);
        if frames.is_empty() {
            return;
        }

        let mut totals: Vec<Duration> = frames.iter().map(|(t, _)| *t).collect();
        let mut grids: Vec<Duration> = frames.iter().map(|(_, g)| *g).collect();
        totals.sort_unstable();
        grids.sort_unstable();
        let spent: Duration = totals.iter().sum();

        log::info!(
            "frames: {} in {:.1}s ({:.0}/s, {:.0}% of the thread) | total p50 {:?} p95 {:?} p99 \
             {:?} max {:?} | grid p50 {:?} p95 {:?}",
            frames.len(),
            elapsed.as_secs_f64(),
            frames.len() as f64 / elapsed.as_secs_f64(),
            100.0 * spent.as_secs_f64() / elapsed.as_secs_f64(),
            quantile(&totals, 0.50),
            quantile(&totals, 0.95),
            quantile(&totals, 0.99),
            totals[totals.len() - 1],
            quantile(&grids, 0.50),
            quantile(&grids, 0.95),
        );
    }
}

fn quantile(sorted: &[Duration], q: f64) -> Duration {
    sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_index_within_the_sample() {
        let sorted: Vec<Duration> = (0..100).map(Duration::from_millis).collect();

        assert_eq!(quantile(&sorted, 0.0), Duration::from_millis(0));
        assert_eq!(quantile(&sorted, 0.5), Duration::from_millis(50));
        assert_eq!(quantile(&sorted, 1.0), Duration::from_millis(99));
    }

    /// A single frame is the whole distribution, and the percentile maths must
    /// not index past it.
    #[test]
    fn a_single_frame_reports_without_panicking() {
        let sorted = [Duration::from_millis(7)];

        assert_eq!(quantile(&sorted, 0.99), Duration::from_millis(7));
    }
}
