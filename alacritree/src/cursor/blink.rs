//! Whether the blink hides the cursor this frame, behind alacritty's
//! `[cursor] style.blinking`.
//!
//! alacritty runs a repeating timer that flips a flag and a one-shot that
//! stops it.  egui has no scheduler and wants an answer per frame, so the
//! phase is derived from how long the current run of blinking has lasted: an
//! even half-cycle shows the cursor, an odd one hides it.  Typing restarts
//! that clock, which is what holds the cursor solid while you type.

use std::time::{Duration, Instant};

use crate::config::CursorBlink;

#[derive(Debug, Default)]
pub(crate) struct Blink {
    /// When the current run of blinking began, or `None` while the cursor is
    /// not blinking at all.
    started: Option<Instant>,
    /// How long until the phase changes, recorded by `resolve` so the caller
    /// can schedule a frame without deriving it again.
    next: Option<Duration>,
}

impl Blink {
    /// Whether the blink hides the cursor this frame.  `blinking` is the
    /// resolved on/off, and passing `false` ends the run, so the cursor turns
    /// solid the moment focus or the program's request goes away.
    pub(crate) fn resolve(&mut self, config: &CursorBlink, blinking: bool, now: Instant) -> bool {
        self.next = None;
        if !blinking {
            self.started = None;
            return false;
        }

        let started = *self.started.get_or_insert(now);
        let elapsed = now.saturating_duration_since(started);
        if !config.timeout.is_zero() && elapsed >= config.timeout {
            return false;
        }

        // Whole phases are flips, so the fraction is how far into the current
        // one we are.
        let phase = elapsed.div_duration_f64(config.interval);
        let mut next = config.interval.mul_f64(phase.floor() + 1.0 - phase);
        if !config.timeout.is_zero() {
            // The frame that ends the run has to arrive on time too.
            next = next.min(config.timeout - elapsed);
        }
        self.next = Some(next);
        phase as u64 % 2 == 1
    }

    /// Hold the cursor solid and restart the run, mirroring alacritty's
    /// `on_typing_start`.
    pub(crate) fn typed(&mut self, now: Instant) {
        self.started = Some(now);
    }

    /// How long until the cursor needs another frame, or `None` when it has
    /// stopped blinking and nothing is coming.
    pub(crate) fn next_phase(&self) -> Option<Duration> {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CursorBlinking;

    const INTERVAL: Duration = Duration::from_millis(500);

    fn config(timeout: Duration) -> CursorBlink {
        CursorBlink { blinking: CursorBlinking::On, interval: INTERVAL, timeout }
    }

    fn no_timeout() -> CursorBlink {
        config(Duration::ZERO)
    }

    #[test]
    fn a_run_starts_with_the_cursor_showing() {
        let mut blink = Blink::default();
        let now = Instant::now();
        assert!(!blink.resolve(&no_timeout(), true, now));
        assert_eq!(blink.next_phase(), Some(INTERVAL));
    }

    #[test]
    fn the_cursor_hides_for_the_second_half_cycle() {
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&no_timeout(), true, now);
        assert!(blink.resolve(&no_timeout(), true, now + INTERVAL));
        assert!(!blink.resolve(&no_timeout(), true, now + INTERVAL * 2));
        assert!(blink.resolve(&no_timeout(), true, now + INTERVAL * 3));
    }

    #[test]
    fn the_next_frame_lands_on_the_flip() {
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&no_timeout(), true, now);
        blink.resolve(&no_timeout(), true, now + INTERVAL / 4);
        assert_eq!(blink.next_phase(), Some(INTERVAL / 4 * 3));
    }

    #[test]
    fn typing_holds_the_cursor_solid() {
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&no_timeout(), true, now);
        assert!(blink.resolve(&no_timeout(), true, now + INTERVAL));

        blink.typed(now + INTERVAL);
        assert!(!blink.resolve(&no_timeout(), true, now + INTERVAL));
    }

    #[test]
    fn the_run_ends_at_the_timeout_with_the_cursor_showing() {
        let cfg = config(INTERVAL * 3);
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&cfg, true, now);
        assert!(blink.resolve(&cfg, true, now + INTERVAL));

        assert!(!blink.resolve(&cfg, true, now + INTERVAL * 3));
        assert_eq!(blink.next_phase(), None, "nothing more to wake for");
    }

    #[test]
    fn the_last_frame_of_a_run_is_scheduled_for_the_timeout() {
        let cfg = config(INTERVAL * 3);
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&cfg, true, now);
        blink.resolve(&cfg, true, now + INTERVAL * 2 + INTERVAL / 2);
        assert_eq!(blink.next_phase(), Some(INTERVAL / 2));
    }

    #[test]
    fn typing_after_the_timeout_starts_a_fresh_run() {
        let cfg = config(INTERVAL * 3);
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&cfg, true, now);
        assert!(!blink.resolve(&cfg, true, now + INTERVAL * 3));

        blink.typed(now + INTERVAL * 3);
        assert!(blink.resolve(&cfg, true, now + INTERVAL * 4));
    }

    #[test]
    fn a_zero_timeout_never_stops() {
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&no_timeout(), true, now);
        assert!(blink.resolve(&no_timeout(), true, now + INTERVAL * 1001));
        assert!(blink.next_phase().is_some());
    }

    #[test]
    fn not_blinking_shows_the_cursor_and_asks_for_no_frames() {
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&no_timeout(), true, now);
        assert!(blink.resolve(&no_timeout(), true, now + INTERVAL));

        assert!(!blink.resolve(&no_timeout(), false, now + INTERVAL));
        assert_eq!(blink.next_phase(), None);
    }

    #[test]
    fn blinking_again_restarts_from_a_shown_cursor() {
        let mut blink = Blink::default();
        let now = Instant::now();
        blink.resolve(&no_timeout(), true, now);
        blink.resolve(&no_timeout(), false, now + INTERVAL);

        assert!(!blink.resolve(&no_timeout(), true, now + INTERVAL * 5));
    }
}
