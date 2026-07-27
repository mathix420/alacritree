//! Port of `alacritty/src/event.rs::paste` and `copy_selection`.  Alacritty
//! is a binary crate so we can't link to it directly; the logic itself is
//! pure terminal protocol and trivial to mirror.

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::{Term, TermMode};

use crate::clipboard::{self, Target};
use crate::config::Config;
use crate::session::{EventProxy, Session};

/// Pass `bracketed = true` for user-driven pastes; `false` is reserved for
/// `Action::Esc` style writes that must reach the PTY verbatim.
pub fn paste(session: &Session, text: &str, bracketed: bool) {
    let bracketed_active = session.term.lock().mode().contains(TermMode::BRACKETED_PASTE);

    on_terminal_input_start(session);

    session.write(paste_bytes(text, bracketed, bracketed_active));
}

/// The bytes a paste puts on the PTY.
///
/// ESC and ETX are stripped inside a bracketed paste so the text cannot forge
/// the end marker.  Without bracketed paste the receiving application cannot
/// tell a paste from typing, so a newline has to become `\r` — which is also
/// what Enter sends, meaning any newline reaching here submits a line.  That is
/// why text built from filenames is filtered first
/// (`file_drop::is_terminal_safe`).
pub(crate) fn paste_bytes(text: &str, bracketed: bool, bracketed_active: bool) -> Vec<u8> {
    if bracketed && bracketed_active {
        let filtered = text.replace(['\x1b', '\x03'], "");
        format!("\x1b[200~{filtered}\x1b[201~").into_bytes()
    } else if bracketed {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Acquires the term lock; mouse handlers that already hold it should call
/// `write_selection` instead.
pub fn copy_selection(session: &Session, config: &Config, target: Target) {
    let term = session.term.lock();
    write_selection(&term, config, target);
}

pub fn write_selection(term: &Term<EventProxy>, config: &Config, target: Target) {
    let Some(text) = term.selection_to_string().filter(|s| !s.is_empty()) else {
        return;
    };
    // Selection→Clipboard mirror is opt-in and one-way — Clipboard writes
    // never reach Primary.
    if matches!(target, Target::Primary) && config.selection.save_to_clipboard {
        clipboard::write(Target::Clipboard, &text);
    }
    clipboard::write(target, &text);
}

/// Mirrors alacritty's `on_terminal_input_start`: any keypress or paste that
/// reaches the PTY clears the active selection and snaps the view back to the
/// active line so the user sees what they just typed.
pub fn on_terminal_input_start(session: &Session) {
    let mut term = session.term.lock();
    let _ = term.selection.take();
    if term.grid().display_offset() != 0 {
        term.scroll_display(Scroll::Bottom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bracketed_paste_wraps_the_text_and_strips_the_end_marker_bytes() {
        let bytes = paste_bytes("a\x1bb\x03c", true, true);
        assert_eq!(bytes, b"\x1b[200~abc\x1b[201~".to_vec());
    }

    /// An application that never enabled bracketed paste sees typing, and `\r`
    /// is Enter — so every newline here submits a line.
    #[test]
    fn a_paste_into_an_app_without_bracketed_paste_turns_newlines_into_enter() {
        assert_eq!(paste_bytes("a\r\nb\nc", true, false), b"a\rb\rc".to_vec());
    }

    /// `bracketed = false` is for writes that must reach the PTY verbatim.
    #[test]
    fn an_unbracketed_write_is_passed_through_untouched() {
        assert_eq!(paste_bytes("a\nb\x1b", false, true), "a\nb\x1b".as_bytes().to_vec());
    }
}
