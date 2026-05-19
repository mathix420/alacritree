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

    if bracketed && bracketed_active {
        // Strip ESC/ETX so pasted text can't forge the end marker or trip a
        // shell into terminating paste early.
        session.write(b"\x1b[200~".to_vec());
        let filtered = text.replace(['\x1b', '\x03'], "");
        session.write(filtered.into_bytes());
        session.write(b"\x1b[201~".to_vec());
    } else if bracketed {
        // Apps that didn't enable bracketed paste can't tell paste from
        // keystrokes — collapse newlines to `\r` (what Enter sends).
        let payload = text.replace("\r\n", "\r").replace('\n', "\r");
        session.write(payload.into_bytes());
    } else {
        session.write(text.as_bytes().to_vec());
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
