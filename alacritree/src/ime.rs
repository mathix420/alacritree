//! IME composition state.  Mirrors alacritty's `display::Ime`
//! (alacritty/src/display/mod.rs) minus what egui makes unreachable:
//! enablement is output-driven (`PlatformOutput::ime`), and egui-winit
//! drops winit's preedit cursor offset, so the caret is always at the
//! end of the composition.

use egui::ImeEvent;
use unicode_width::UnicodeWidthChar;

use crate::session::SessionId;

#[derive(Default)]
pub struct Ime {
    /// In-progress composition; `Some` suppresses key input to the PTY.
    preedit: Option<String>,
    /// Session the composition targets.  Composition belongs to the
    /// window's focused terminal, so a session switch orphans it.
    owner: Option<SessionId>,
}

impl Ime {
    pub fn preedit(&self) -> Option<&str> {
        self.preedit.as_deref()
    }

    /// Drop any active composition (focus loss; the IME's `Disabled`
    /// event arrives only while we are still draining input).
    pub fn clear(&mut self) {
        self.preedit = None;
    }

    /// Point the composition at the currently shown session, dropping it
    /// if the session changed mid-composition.
    pub fn retarget(&mut self, session: SessionId) {
        if self.owner != Some(session) {
            self.preedit = None;
            self.owner = Some(session);
        }
    }

    /// Apply an IME event; returns text to write to the PTY on commit.
    pub fn process(&mut self, event: &ImeEvent) -> Option<String> {
        match event {
            ImeEvent::Enabled | ImeEvent::Disabled => {
                self.preedit = None;
                None
            },
            ImeEvent::Preedit(text) => {
                self.preedit = (!text.is_empty()).then(|| text.clone());
                None
            },
            ImeEvent::Commit(text) => {
                self.preedit = None;
                // Confirming a composition can emit a bare newline commit
                // on some platforms; egui's TextEdit ignores those, and a
                // terminal must too or Enter doubles.
                (!text.is_empty() && text != "\n" && text != "\r").then(|| text.clone())
            },
        }
    }
}

/// Terminal cell width of one char.  Control/zero-width chars count 1 so a
/// malformed preedit still advances and stays visible.
pub fn char_cells(c: char) -> usize {
    c.width().unwrap_or(1).max(1)
}

pub struct PreeditLayout<'a> {
    pub start_col: usize,
    pub visible: &'a str,
    pub width: usize,
}

/// Where the preedit overlay sits on the grid.  Mirrors alacritty's
/// `draw_ime_preview` placement rule: the *end* of the composition (where
/// the caret is) stays visible — right-aligned against the grid edge when
/// the cursor is too far right, truncated from the left (whole chars) when
/// wider than the grid.
pub fn preedit_layout(text: &str, cursor_col: usize, cols: usize) -> PreeditLayout<'_> {
    let mut width: usize = text.chars().map(char_cells).sum();
    let mut start_byte = 0;
    let mut chars = text.char_indices();
    while width > cols {
        let Some((idx, c)) = chars.next() else { break };
        width -= char_cells(c);
        start_byte = idx + c.len_utf8();
    }
    let visible = &text[start_byte..];
    let end = (cursor_col + width).min(cols);
    PreeditLayout { start_col: end.saturating_sub(width), visible, width }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::ImeEvent;

    #[test]
    fn preedit_tracks_composition() {
        let mut ime = Ime::default();
        assert_eq!(ime.process(&ImeEvent::Enabled), None);
        assert_eq!(ime.process(&ImeEvent::Preedit("あ".into())), None);
        assert_eq!(ime.preedit(), Some("あ"));
        assert_eq!(ime.process(&ImeEvent::Preedit("あい".into())), None);
        assert_eq!(ime.preedit(), Some("あい"));
    }

    #[test]
    fn empty_preedit_clears() {
        let mut ime = Ime::default();
        ime.process(&ImeEvent::Preedit("あ".into()));
        ime.process(&ImeEvent::Preedit(String::new()));
        assert_eq!(ime.preedit(), None);
    }

    #[test]
    fn commit_returns_text_and_clears_preedit() {
        let mut ime = Ime::default();
        ime.process(&ImeEvent::Preedit("あ".into()));
        assert_eq!(ime.process(&ImeEvent::Commit("愛".into())), Some("愛".into()));
        assert_eq!(ime.preedit(), None);
    }

    #[test]
    fn bare_newline_and_empty_commits_are_dropped() {
        let mut ime = Ime::default();
        assert_eq!(ime.process(&ImeEvent::Commit("\n".into())), None);
        assert_eq!(ime.process(&ImeEvent::Commit("\r".into())), None);
        assert_eq!(ime.process(&ImeEvent::Commit(String::new())), None);
    }

    #[test]
    fn enabled_and_disabled_clear_preedit() {
        let mut ime = Ime::default();
        ime.process(&ImeEvent::Preedit("あ".into()));
        ime.process(&ImeEvent::Disabled);
        assert_eq!(ime.preedit(), None);
        ime.process(&ImeEvent::Preedit("い".into()));
        ime.process(&ImeEvent::Enabled);
        assert_eq!(ime.preedit(), None);
    }

    #[test]
    fn retarget_clears_only_on_session_change() {
        let mut ime = Ime::default();
        ime.retarget(1);
        ime.process(&ImeEvent::Preedit("あ".into()));
        ime.retarget(1);
        assert_eq!(ime.preedit(), Some("あ"));
        ime.retarget(2);
        assert_eq!(ime.preedit(), None);
    }

    #[test]
    fn layout_ascii_at_cursor() {
        let l = preedit_layout("abc", 5, 80);
        assert_eq!((l.start_col, l.visible, l.width), (5, "abc", 3));
    }

    #[test]
    fn layout_wide_chars_take_two_cells() {
        let l = preedit_layout("あい", 5, 80);
        assert_eq!((l.start_col, l.visible, l.width), (5, "あい", 4));
    }

    #[test]
    fn layout_right_aligns_at_grid_edge() {
        // Cursor at col 78 of 80: a 4-cell preedit must end at the last
        // column, so it starts at 76.
        let l = preedit_layout("あい", 78, 80);
        assert_eq!((l.start_col, l.width), (76, 4));
    }

    #[test]
    fn layout_truncates_from_the_left_keeping_the_end() {
        // 6 cells of text in a 4-column grid: the first wide char drops.
        let l = preedit_layout("あいう", 0, 4);
        assert_eq!((l.start_col, l.visible, l.width), (0, "いう", 4));
    }

    #[test]
    fn layout_empty_and_degenerate_grid() {
        let l = preedit_layout("", 3, 80);
        assert_eq!((l.start_col, l.visible, l.width), (3, "", 0));
        let l = preedit_layout("あ", 0, 0);
        assert_eq!((l.visible, l.width), ("", 0));
    }
}
