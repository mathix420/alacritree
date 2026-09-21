//! One session's cursor: whether it is drawn, in what shape, and where.
//!
//! The painter asks twice per frame.  Shape and visibility come first, because
//! they decide whether the grid capture records a cursor at all; the cell to
//! draw it in comes after, because the glide needs the cell that capture just
//! found.

pub(crate) mod anim;
pub(crate) mod blink;

use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::CursorShape;

use crate::config::CursorConfig;

/// Fractional column and row in the viewport.  Whole numbers are cell corners,
/// which is where the cursor sits whenever it is not moving.
pub(crate) type CellPos = (f32, f32);

/// What the terminal and the window say about the cursor, read off one frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Inputs {
    /// DECTCEM.  A full-screen app hides the cursor while it repaints and
    /// leaves it parked wherever its last write landed.
    pub shown: bool,
    /// The shape the running program asked for over DECSCUSR.
    pub shape: CursorShape,
    /// Whether that program also asked for a blinking cursor.
    pub blinking: bool,
    pub window_focused: bool,
    /// An IME preedit paints its own caret over the cell.
    pub composing: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Cursor {
    anim: anim::Animation,
    blink: blink::Blink,
}

impl Cursor {
    /// The shape to draw the cursor in, or `None` when it is not drawn at all.
    pub(crate) fn resolve(
        &mut self,
        config: &CursorConfig,
        inputs: Inputs,
        now: Instant,
    ) -> Option<CursorShape> {
        let drawn =
            inputs.shown && !inputs.composing && !matches!(inputs.shape, CursorShape::Hidden);
        // An unfocused window keeps a solid cursor: a blink in the corner of
        // the eye reads as the window still wanting something.
        let blinking = drawn
            && inputs.window_focused
            && config.blink.blinking.blinking_override().unwrap_or(inputs.blinking);
        // Unconditional, so the run ends the moment any of the above stops.
        let hidden = self.blink.resolve(&config.blink, blinking, now);

        if !drawn || hidden {
            return None;
        }
        Some(if !inputs.window_focused && config.unfocused_hollow {
            CursorShape::HollowBlock
        } else {
            inputs.shape
        })
    }

    /// The quad the cursor is smeared across on its way to the cell the
    /// capture found it in, or `None` once it has arrived and the cursor is
    /// just its own cell again.
    pub(crate) fn place(
        &mut self,
        config: &CursorConfig,
        target: Option<CellPos>,
        display_offset: i32,
        now: Instant,
    ) -> Option<anim::Corners> {
        let corners = self.anim.place(&config.motion, target, display_offset, now);
        corners.filter(|_| !self.anim.settled())
    }

    /// Hold the cursor solid and restart its blink, mirroring alacritty's
    /// `on_typing_start`.
    pub(crate) fn typed(&mut self, now: Instant) {
        self.blink.typed(now);
    }

    /// How long until this cursor needs another frame.  Nothing else wakes
    /// egui while it moves or blinks on its own, and `None` means it is at
    /// rest and nothing is owed.
    pub(crate) fn repaint_in(&self) -> Option<Duration> {
        if self.anim.settled() { self.blink.next_phase() } else { Some(Duration::ZERO) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CursorBlinking;

    /// A program showing a plain block cursor and asking for nothing else.
    fn inputs() -> Inputs {
        Inputs {
            shown: true,
            shape: CursorShape::Block,
            blinking: false,
            window_focused: true,
            composing: false,
        }
    }

    fn config(blinking: CursorBlinking) -> CursorConfig {
        let mut config = CursorConfig::default();
        config.blink.blinking = blinking;
        config
    }

    #[test]
    fn the_shape_the_program_asked_for_is_the_shape_drawn() {
        let mut cursor = Cursor::default();
        let inputs = Inputs { shape: CursorShape::Beam, ..inputs() };
        let drawn = cursor.resolve(&config(CursorBlinking::Off), inputs, Instant::now());
        assert_eq!(drawn, Some(CursorShape::Beam));
    }

    #[test]
    fn an_app_that_hides_the_cursor_gets_no_cursor() {
        let mut cursor = Cursor::default();
        let inputs = Inputs { shown: false, ..inputs() };
        assert_eq!(cursor.resolve(&config(CursorBlinking::Off), inputs, Instant::now()), None);
    }

    #[test]
    fn a_preedit_paints_its_own_caret_instead() {
        let mut cursor = Cursor::default();
        let inputs = Inputs { composing: true, ..inputs() };
        assert_eq!(cursor.resolve(&config(CursorBlinking::Off), inputs, Instant::now()), None);
    }

    #[test]
    fn an_unfocused_window_hollows_the_cursor() {
        let mut cursor = Cursor::default();
        let inputs = Inputs { window_focused: false, ..inputs() };
        let drawn = cursor.resolve(&config(CursorBlinking::Off), inputs, Instant::now());
        assert_eq!(drawn, Some(CursorShape::HollowBlock));
    }

    #[test]
    fn turning_unfocused_hollow_off_leaves_the_shape_alone() {
        let mut cursor = Cursor::default();
        let mut config = config(CursorBlinking::Off);
        config.unfocused_hollow = false;
        let inputs = Inputs { window_focused: false, ..inputs() };
        assert_eq!(cursor.resolve(&config, inputs, Instant::now()), Some(CursorShape::Block));
    }

    /// The four `blinking` values differ only in whether they overrule the
    /// program, so each pairs with what the program asked for.
    #[test]
    fn the_config_decides_who_wins_the_blink() {
        let cases = [
            (CursorBlinking::Off, false, false),
            (CursorBlinking::Off, true, true),
            (CursorBlinking::On, false, false),
            (CursorBlinking::Never, true, false),
            (CursorBlinking::Always, false, true),
        ];
        for (blinking, asked, blinks) in cases {
            let config = config(blinking);
            let mut cursor = Cursor::default();
            let inputs = Inputs { blinking: asked, ..inputs() };
            let now = Instant::now();
            cursor.resolve(&config, inputs, now);
            let hidden = cursor.resolve(&config, inputs, now + config.blink.interval).is_none();
            assert_eq!(hidden, blinks, "{blinking:?} against a program asking {asked}");
        }
    }

    #[test]
    fn an_unfocused_cursor_holds_still() {
        let config = config(CursorBlinking::Always);
        let mut cursor = Cursor::default();
        let inputs = Inputs { window_focused: false, ..inputs() };
        let now = Instant::now();
        cursor.resolve(&config, inputs, now);

        let drawn = cursor.resolve(&config, inputs, now + config.blink.interval);
        assert_eq!(drawn, Some(CursorShape::HollowBlock), "an unfocused cursor blinked");
        assert_eq!(cursor.repaint_in(), None, "an unfocused cursor asked for frames");
    }

    #[test]
    fn a_blinking_cursor_asks_for_the_frame_that_flips_it() {
        let config = config(CursorBlinking::Always);
        let mut cursor = Cursor::default();
        let now = Instant::now();
        cursor.resolve(&config, inputs(), now);
        assert_eq!(cursor.repaint_in(), Some(config.blink.interval));
    }

    #[test]
    fn a_still_cursor_asks_for_nothing() {
        let mut cursor = Cursor::default();
        cursor.resolve(&config(CursorBlinking::Off), inputs(), Instant::now());
        assert_eq!(cursor.repaint_in(), None);
    }
}
