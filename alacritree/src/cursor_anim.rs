//! The quad the cursor smears across while it catches up with the cell it is
//! really in, behind `[ui.cursor] animate`.
//!
//! Each of neovide's four corners eases to its own destination, so the edge
//! leading a move arrives before the edge trailing it. Sliding a whole box
//! instead draws the cursor in a handful of places along the way, which reads
//! as several cursors rather than one that moved.

use std::time::{Duration, Instant};

use crate::config::CursorMotion;

/// Fractional column and row in the viewport.  Whole numbers are cell corners,
/// which is where the cursor sits whenever it is not moving.
pub(crate) type CellPos = (f32, f32);

/// The cursor's four corners, clockwise from the top-left of its cell.
pub(crate) type Corners = [CellPos; 4];

/// Where each corner sits inside its own cell, in the order `Corners` holds
/// them.
const OFFSETS: Corners = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];

#[derive(Debug, Default)]
pub(crate) struct CursorAnimation {
    glide: Option<Glide>,
}

#[derive(Debug)]
struct Glide {
    corners: [Corner; 4],
    /// The cell every corner is heading for.
    to: CellPos,
    /// Scrollback position the stored cells were recorded against.  Scrolling
    /// renumbers every row at once, and sliding the cursor along with a jump
    /// the whole screen made is not an animation of anything.
    display_offset: i32,
}

#[derive(Debug, Clone, Copy)]
struct Corner {
    from: CellPos,
    to: CellPos,
    at: CellPos,
    started: Instant,
    /// This corner's share of the configured glide.  Leading corners get less
    /// than the whole and trailing ones more, which is the entire stretch.
    duration: Duration,
}

impl CursorAnimation {
    /// The cursor's corners for this frame.  `target` is the cell the terminal
    /// has it in, or `None` while it is hidden or scrolled out of view, which
    /// also clears the glide so its next appearance starts where it appears.
    pub(crate) fn place(
        &mut self,
        motion: &CursorMotion,
        target: Option<CellPos>,
        display_offset: i32,
        now: Instant,
    ) -> Option<Corners> {
        let Some(target) = target else {
            self.glide = None;
            return None;
        };
        if !motion.animate || motion.duration.is_zero() {
            self.glide = None;
            return Some(corners_of(target));
        }

        let glide = self.glide.get_or_insert_with(|| Glide::parked(target, now, display_offset));
        glide.follow_scroll(display_offset);
        if glide.to != target {
            if jump_cells(glide.to, target) < motion.min_cells {
                *glide = Glide::parked(target, now, display_offset);
            } else {
                glide.aim(target, motion.duration, now);
            }
        }
        Some(glide.advance(now))
    }

    /// Whether every corner has reached its cell.  A frame that says no has to
    /// ask for another one: nothing else wakes egui while the cursor moves on
    /// its own.
    pub(crate) fn settled(&self) -> bool {
        self.glide.as_ref().is_none_or(Glide::settled)
    }
}

/// A point inside the cursor's quad, by its fractions across and down.  The
/// quad skews while the cursor moves, so a beam or an underline is cut out of
/// it rather than measured off the cell.
pub(crate) fn within(corners: &Corners, u: f32, v: f32) -> CellPos {
    lerp(lerp(corners[0], corners[1], u), lerp(corners[3], corners[2], u), v)
}

impl Glide {
    fn parked(cell: CellPos, started: Instant, display_offset: i32) -> Self {
        let corners = corners_of(cell).map(|at| Corner {
            from: at,
            to: at,
            at,
            started,
            duration: Duration::ZERO,
        });
        Self { corners, to: cell, display_offset }
    }

    /// Send every corner to its share of `target`.  How long each takes falls
    /// out of how far it points along the move: a corner on the leading edge
    /// is most of the way there already and arrives almost at once, one on the
    /// trailing edge takes nearly twice the configured glide.
    fn aim(&mut self, target: CellPos, duration: Duration, now: Instant) {
        let travel = unit((target.0 - self.to.0, target.1 - self.to.1));
        for (corner, offset) in self.corners.iter_mut().zip(OFFSETS) {
            let out = unit((offset.0 - 0.5, offset.1 - 0.5));
            let alignment = travel.0 * out.0 + travel.1 * out.1;
            *corner = Corner {
                from: corner.at,
                to: (target.0 + offset.0, target.1 + offset.1),
                at: corner.at,
                started: now,
                duration: duration.mul_f32(1.0 - alignment),
            };
        }
        self.to = target;
    }

    fn advance(&mut self, now: Instant) -> Corners {
        for corner in &mut self.corners {
            corner.advance(now);
        }
        self.corners.map(|corner| corner.at)
    }

    fn settled(&self) -> bool {
        self.corners.iter().all(|corner| corner.at == corner.to)
    }

    fn follow_scroll(&mut self, display_offset: i32) {
        let rows = (display_offset - self.display_offset) as f32;
        if rows == 0.0 {
            return;
        }
        for corner in &mut self.corners {
            corner.from.1 += rows;
            corner.to.1 += rows;
            corner.at.1 += rows;
        }
        self.to.1 += rows;
        self.display_offset = display_offset;
    }
}

impl Corner {
    fn advance(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.started);
        self.at = if self.duration.is_zero() || elapsed >= self.duration {
            // Landed exactly, so `settled` can compare rather than measure.
            self.to
        } else {
            let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
            lerp(self.from, self.to, ease_out_cubic(t))
        };
    }
}

fn corners_of(cell: CellPos) -> Corners {
    OFFSETS.map(|offset| (cell.0 + offset.0, cell.1 + offset.1))
}

/// The same direction at length one, or nothing when there is no direction to
/// take.
fn unit(v: CellPos) -> CellPos {
    let len = v.0.hypot(v.1);
    if len == 0.0 { (0.0, 0.0) } else { (v.0 / len, v.1 / len) }
}

/// How far the cursor jumped, in cells.  The larger of the two axes rather
/// than the diagonal, so one threshold reads the same whichever way it moved.
fn jump_cells(from: CellPos, to: CellPos) -> f32 {
    (to.0 - from.0).abs().max((to.1 - from.1).abs())
}

fn lerp(from: CellPos, to: CellPos, t: f32) -> CellPos {
    (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
}

/// Fast off the mark and easing into the destination, which is what makes a
/// short glide read as the cursor arriving rather than as lag.
fn ease_out_cubic(t: f32) -> f32 {
    let n = t - 1.0;
    n * n * n + 1.0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const DURATION: Duration = Duration::from_millis(100);
    const TOP_LEFT: usize = 0;
    const TOP_RIGHT: usize = 1;
    const BOTTOM_RIGHT: usize = 2;
    const BOTTOM_LEFT: usize = 3;

    fn motion(animate: bool) -> CursorMotion {
        CursorMotion { animate, duration: DURATION, min_cells: 2.0 }
    }

    #[test]
    fn the_first_frame_covers_the_cell_the_cursor_is_in() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let drawn = anim.place(&motion(true), Some((4.0, 2.0)), 0, now);
        assert_eq!(drawn, Some([(4.0, 2.0), (5.0, 2.0), (5.0, 3.0), (4.0, 3.0)]));
        assert!(anim.settled());
    }

    /// The whole point of the four corners: the edge in front of the move is
    /// already at the new cell while the edge behind it is still on its way,
    /// so what gets drawn spans the gap rather than sitting somewhere in it.
    #[test]
    fn the_leading_edge_arrives_while_the_trailing_edge_is_still_coming() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let cfg = motion(true);
        anim.place(&cfg, Some((0.0, 0.0)), 0, now);
        // The frame that notices the move starts the clock; the one after it
        // is the first with anywhere to have moved to.
        anim.place(&cfg, Some((10.0, 0.0)), 0, now);

        let drawn = anim.place(&cfg, Some((10.0, 0.0)), 0, now + DURATION / 2).unwrap();
        assert_eq!(drawn[TOP_RIGHT].0, 11.0, "the leading edge had not arrived");
        assert_eq!(drawn[BOTTOM_RIGHT].0, 11.0, "the leading edge had not arrived");
        assert!(drawn[TOP_LEFT].0 < 10.0, "the trailing edge was not left behind");
        assert!(drawn[BOTTOM_LEFT].0 > 0.0, "the trailing edge had not set off");
        assert!(!anim.settled());
    }

    #[test]
    fn the_glide_ends_on_the_cell_exactly() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let cfg = motion(true);
        anim.place(&cfg, Some((0.0, 0.0)), 0, now);
        anim.place(&cfg, Some((20.0, 0.0)), 0, now);

        anim.place(&cfg, Some((20.0, 0.0)), 0, now + DURATION);
        // The trailing corners take nearly twice the configured glide, so the
        // run is over only once they have landed too.
        let drawn = anim.place(&cfg, Some((20.0, 0.0)), 0, now + DURATION * 2).unwrap();
        assert_eq!(drawn, corners_of((20.0, 0.0)));
        assert!(anim.settled());
    }

    #[test]
    fn a_short_hop_snaps() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let cfg = motion(true);
        anim.place(&cfg, Some((0.0, 0.0)), 0, now);

        let drawn = anim.place(&cfg, Some((1.0, 0.0)), 0, now).unwrap();
        assert_eq!(drawn, corners_of((1.0, 0.0)));
        assert!(anim.settled());
    }

    #[test]
    fn a_destination_that_moves_mid_glide_replays_from_the_drawn_quad() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let cfg = motion(true);
        anim.place(&cfg, Some((0.0, 0.0)), 0, now);
        anim.place(&cfg, Some((40.0, 0.0)), 0, now);

        let midway = anim.place(&cfg, Some((40.0, 0.0)), 0, now + DURATION / 2).unwrap();
        let redirected = anim.place(&cfg, Some((0.0, 10.0)), 0, now + DURATION / 2).unwrap();
        assert_eq!(redirected, midway, "the new glide starts where the last frame drew it");
    }

    #[test]
    fn scrolling_moves_the_cursor_without_animating_it() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let cfg = motion(true);
        anim.place(&cfg, Some((3.0, 20.0)), 0, now);

        // Ten lines of scrollback push every row down by ten.
        let drawn = anim.place(&cfg, Some((3.0, 30.0)), 10, now).unwrap();
        assert_eq!(drawn, corners_of((3.0, 30.0)));
        assert!(anim.settled());
    }

    #[test]
    fn the_option_being_off_draws_every_cell_directly() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let cfg = motion(false);
        anim.place(&cfg, Some((0.0, 0.0)), 0, now);
        let drawn = anim.place(&cfg, Some((40.0, 0.0)), 0, now).unwrap();
        assert_eq!(drawn, corners_of((40.0, 0.0)));
        assert!(anim.settled());
    }

    #[test]
    fn a_zero_duration_draws_every_cell_directly() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let mut cfg = motion(true);
        cfg.duration = Duration::ZERO;
        anim.place(&cfg, Some((0.0, 0.0)), 0, now);
        let drawn = anim.place(&cfg, Some((40.0, 0.0)), 0, now).unwrap();
        assert_eq!(drawn, corners_of((40.0, 0.0)));
        assert!(anim.settled());
    }

    #[test]
    fn a_hidden_cursor_reappears_where_it_reappears() {
        let mut anim = CursorAnimation::default();
        let now = Instant::now();
        let cfg = motion(true);
        anim.place(&cfg, Some((0.0, 0.0)), 0, now);
        assert_eq!(anim.place(&cfg, None, 0, now), None);
        let drawn = anim.place(&cfg, Some((70.0, 20.0)), 0, now).unwrap();
        assert_eq!(drawn, corners_of((70.0, 20.0)));
        assert!(anim.settled());
    }

    /// A beam is cut out of the quad, so while the quad is skewed the beam
    /// leans with it instead of standing upright in the middle of a smear.
    #[test]
    fn a_shape_narrower_than_the_cell_is_cut_out_of_the_quad() {
        let quad = [(0.0, 0.0), (10.0, 0.0), (10.0, 1.0), (0.0, 1.0)];
        assert_eq!(within(&quad, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(within(&quad, 1.0, 1.0), (10.0, 1.0));
        assert_eq!(within(&quad, 0.5, 0.5), (5.0, 0.5));
    }
}
