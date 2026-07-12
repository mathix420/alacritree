//! Mouse-report byte encodings, mirroring alacritty's `mouse_report` /
//! `normal_mouse_report` / `sgr_mouse_report`.  Apps that enable mouse
//! tracking (vim, htop, TUI agents) receive clicks, drags, and wheel ticks as
//! button reports instead of the terminal handling them locally.

use alacritty_terminal::index::Point;
use alacritty_terminal::term::TermMode;
use egui::Modifiers;

pub const BUTTON_LEFT: u8 = 0;
pub const BUTTON_MIDDLE: u8 = 1;
pub const BUTTON_RIGHT: u8 = 2;

/// Motion reports add 32 to the held button's code; `MOTION_NONE` is used for
/// pointer movement with no button down (any-motion tracking).
pub const MOTION_OFFSET: u8 = 32;
pub const MOTION_NONE: u8 = 35;

pub const WHEEL_UP: u8 = 64;
pub const WHEEL_DOWN: u8 = 65;
pub const WHEEL_LEFT: u8 = 66;
pub const WHEEL_RIGHT: u8 = 67;

/// xterm modifier bits for mouse reports: Shift +4, Alt +8, Ctrl +16.
fn modifier_offset(mods: Modifiers) -> u8 {
    (if mods.shift { 4 } else { 0 })
        + (if mods.alt { 8 } else { 0 })
        + (if mods.ctrl { 16 } else { 0 })
}

/// Encode a mouse button event at `point`.  `button` is the base code (0/1/2
/// for left/middle/right, `+32` for motion, 64-67 for wheel).  Returns `None`
/// when the pointer is over scrollback history or past the coordinate range the
/// active encoding can express.  Mirrors alacritty's `mouse_report`: SGR keeps
/// the button and marks release with a trailing `m`; the legacy encoding can't
/// name the button on release, so it reports button 3.
pub fn mouse_report(
    mode: TermMode,
    point: Point,
    button: u8,
    pressed: bool,
    mods: Modifiers,
) -> Option<Vec<u8>> {
    if point.line < 0 {
        return None;
    }
    let mods = modifier_offset(mods);

    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(
            format!(
                "\x1b[<{};{};{}{}",
                button + mods,
                point.column.0 + 1,
                point.line.0 + 1,
                suffix
            )
            .into_bytes(),
        );
    }

    let code = if pressed { button + mods } else { 3 + mods };
    normal_mouse_report(mode, point, code)
}

fn normal_mouse_report(mode: TermMode, point: Point, button: u8) -> Option<Vec<u8>> {
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let max_point = if utf8 { 2015 } else { 223 };
    if point.line.0 >= max_point || point.column.0 as i32 >= max_point {
        return None;
    }

    let mut msg = vec![0x1b, b'[', b'M', 32 + button];
    let mouse_pos_encode = |pos: usize| {
        let pos = 32 + 1 + pos;
        [(0xC0 + pos / 64) as u8, (0x80 + (pos & 63)) as u8]
    };

    if utf8 && point.column.0 >= 95 {
        msg.extend_from_slice(&mouse_pos_encode(point.column.0));
    } else {
        msg.push(32 + 1 + point.column.0 as u8);
    }
    if utf8 && point.line.0 >= 95 {
        msg.extend_from_slice(&mouse_pos_encode(point.line.0 as usize));
    } else {
        msg.push(32 + 1 + point.line.0 as u8);
    }
    Some(msg)
}

/// Encode one wheel tick.  Wheel ticks are momentary, so they always report as
/// a press.
pub fn wheel_report(mode: TermMode, point: Point, button: u8, mods: Modifiers) -> Option<Vec<u8>> {
    mouse_report(mode, point, button, true, mods)
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::{Column, Line, Point};

    use super::*;

    fn point(line: i32, col: usize) -> Point {
        Point::new(Line(line), Column(col))
    }

    #[test]
    fn sgr_wheel_report_uses_one_based_coordinates() {
        assert_eq!(
            wheel_report(TermMode::SGR_MOUSE, point(4, 2), WHEEL_UP, Modifiers::NONE),
            Some(b"\x1b[<64;3;5M".to_vec())
        );
    }

    #[test]
    fn normal_wheel_report_offsets_by_32() {
        assert_eq!(
            wheel_report(TermMode::empty(), point(4, 2), WHEEL_DOWN, Modifiers::NONE),
            Some(vec![0x1b, b'[', b'M', 32 + 65, 32 + 1 + 2, 32 + 1 + 4])
        );
    }

    #[test]
    fn modifiers_offset_the_button_code() {
        // xterm modifier bits for mouse reports: Shift +4, Alt +8, Ctrl +16.
        let mods = Modifiers { shift: true, ctrl: true, ..Modifiers::NONE };
        assert_eq!(
            wheel_report(TermMode::SGR_MOUSE, point(0, 0), WHEEL_UP, mods),
            Some(b"\x1b[<84;1;1M".to_vec())
        );
    }

    #[test]
    fn scrollback_position_is_not_reported() {
        assert_eq!(
            wheel_report(TermMode::SGR_MOUSE, point(-1, 0), WHEEL_UP, Modifiers::NONE),
            None
        );
    }

    #[test]
    fn normal_report_drops_out_of_range_coordinates() {
        assert_eq!(wheel_report(TermMode::empty(), point(0, 230), WHEEL_UP, Modifiers::NONE), None);
    }

    #[test]
    fn utf8_mouse_extends_the_coordinate_range() {
        // Column 200 encodes as 32+1+200 = 233 → 0xC3 0xA9 (UTF-8-style pair).
        assert_eq!(
            wheel_report(TermMode::UTF8_MOUSE, point(0, 200), WHEEL_UP, Modifiers::NONE),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 0xC3, 0xA9, 32 + 1])
        );
    }

    #[test]
    fn wheel_left_right_codes() {
        assert_eq!(
            wheel_report(TermMode::SGR_MOUSE, point(0, 0), WHEEL_LEFT, Modifiers::NONE),
            Some(b"\x1b[<66;1;1M".to_vec())
        );
        assert_eq!(
            wheel_report(TermMode::SGR_MOUSE, point(0, 0), WHEEL_RIGHT, Modifiers::NONE),
            Some(b"\x1b[<67;1;1M".to_vec())
        );
    }

    #[test]
    fn sgr_press_and_release_differ_only_by_suffix() {
        assert_eq!(
            mouse_report(TermMode::SGR_MOUSE, point(4, 2), BUTTON_LEFT, true, Modifiers::NONE),
            Some(b"\x1b[<0;3;5M".to_vec())
        );
        assert_eq!(
            mouse_report(TermMode::SGR_MOUSE, point(4, 2), BUTTON_LEFT, false, Modifiers::NONE),
            Some(b"\x1b[<0;3;5m".to_vec())
        );
    }

    #[test]
    fn sgr_report_keeps_the_button_on_release() {
        assert_eq!(
            mouse_report(TermMode::SGR_MOUSE, point(0, 0), BUTTON_RIGHT, false, Modifiers::NONE),
            Some(b"\x1b[<2;1;1m".to_vec())
        );
    }

    #[test]
    fn legacy_release_reports_button_three() {
        assert_eq!(
            mouse_report(TermMode::empty(), point(0, 0), BUTTON_RIGHT, false, Modifiers::NONE),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33, 33])
        );
    }

    #[test]
    fn legacy_release_folds_modifiers_into_button_three() {
        let mods = Modifiers { ctrl: true, ..Modifiers::NONE };
        assert_eq!(
            mouse_report(TermMode::empty(), point(0, 0), BUTTON_LEFT, false, mods),
            Some(vec![0x1b, b'[', b'M', 32 + 3 + 16, 33, 33])
        );
    }

    #[test]
    fn motion_report_uses_the_dragged_button_code() {
        // Left-drag = button 0 + 32.
        assert_eq!(
            mouse_report(
                TermMode::SGR_MOUSE,
                point(1, 1),
                BUTTON_LEFT + MOTION_OFFSET,
                true,
                Modifiers::NONE
            ),
            Some(b"\x1b[<32;2;2M".to_vec())
        );
    }

    #[test]
    fn scrollback_position_is_not_reported_for_clicks() {
        assert_eq!(
            mouse_report(TermMode::SGR_MOUSE, point(-1, 0), BUTTON_LEFT, true, Modifiers::NONE),
            None
        );
    }
}
