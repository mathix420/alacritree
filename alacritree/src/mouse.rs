//! Mouse-report byte encodings, mirroring alacritty's `mouse_report` /
//! `normal_mouse_report` / `sgr_mouse_report`.  Apps that enable mouse
//! tracking (vim, htop, TUI agents) receive wheel events as button reports
//! instead of scrollback movement.

use alacritty_terminal::index::Point;
use alacritty_terminal::term::TermMode;
use egui::Modifiers;

pub const WHEEL_UP: u8 = 64;
pub const WHEEL_DOWN: u8 = 65;
pub const WHEEL_LEFT: u8 = 66;
pub const WHEEL_RIGHT: u8 = 67;

/// Encode one wheel tick at `point` as a mouse report.  Returns `None` when
/// the pointer is over scrollback history or past the coordinate range the
/// active encoding can express.
pub fn wheel_report(mode: TermMode, point: Point, button: u8, mods: Modifiers) -> Option<Vec<u8>> {
    if point.line < 0 {
        return None;
    }

    // xterm modifier bits for mouse reports: Shift +4, Alt +8, Ctrl +16.
    let button = button
        + if mods.shift { 4 } else { 0 }
        + if mods.alt { 8 } else { 0 }
        + if mods.ctrl { 16 } else { 0 };

    if mode.contains(TermMode::SGR_MOUSE) {
        // Wheel ticks are momentary, so only the press ('M') form is sent.
        return Some(
            format!("\x1b[<{};{};{}M", button, point.column.0 + 1, point.line.0 + 1).into_bytes(),
        );
    }

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
}
