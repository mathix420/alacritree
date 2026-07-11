use egui::{Event, Key, Modifiers};

pub fn event_to_bytes(event: &Event) -> Option<Vec<u8>> {
    match event {
        // OS-composed text — already accounts for Shift and dead-key composition.
        Event::Text(text) if !text.is_empty() => Some(text.as_bytes().to_vec()),
        Event::Key { key, pressed: true, modifiers, repeat: _, .. } => {
            key_to_bytes(*key, *modifiers)
        },
        // `Event::Paste` is handled by the caller via `paste::paste` so it
        // gets bracketed-paste wrapping and newline normalization.  `Copy` and
        // `Cut` carry no modifiers, so they can't tell Ctrl+C from Ctrl+Shift+C
        // and must not be encoded here; the Key event alongside them does it.
        _ => None,
    }
}

fn key_to_bytes(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    if mods.ctrl && !mods.alt {
        if let Some(b) = control_byte(key, mods) {
            return Some(vec![b]);
        }
    }

    let bytes: &[u8] = match key {
        Key::Enter => b"\r",
        Key::Tab => b"\t",
        Key::Backspace => b"\x7f",
        Key::Escape => b"\x1b",
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        Key::Insert => b"\x1b[2~",
        Key::Delete => b"\x1b[3~",
        Key::F1 => b"\x1bOP",
        Key::F2 => b"\x1bOQ",
        Key::F3 => b"\x1bOR",
        Key::F4 => b"\x1bOS",
        Key::F5 => b"\x1b[15~",
        Key::F6 => b"\x1b[17~",
        Key::F7 => b"\x1b[18~",
        Key::F8 => b"\x1b[19~",
        Key::F9 => b"\x1b[20~",
        Key::F10 => b"\x1b[21~",
        Key::F11 => b"\x1b[23~",
        Key::F12 => b"\x1b[24~",
        _ => return None,
    };

    if mods.alt {
        // Long-standing meta convention: Alt+key sends ESC + key.
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(bytes);
        Some(out)
    } else {
        Some(bytes.to_vec())
    }
}

/// Character a printable key produces, as far as byte encoding is concerned.
/// Letters honor `shift` for case; shifted punctuation already arrives as its
/// own logical key in egui (`?`, `{`, `|`, …), so those map one-to-one.
fn key_char(key: Key, shift: bool) -> Option<char> {
    let c = match key {
        Key::A => 'a',
        Key::B => 'b',
        Key::C => 'c',
        Key::D => 'd',
        Key::E => 'e',
        Key::F => 'f',
        Key::G => 'g',
        Key::H => 'h',
        Key::I => 'i',
        Key::J => 'j',
        Key::K => 'k',
        Key::L => 'l',
        Key::M => 'm',
        Key::N => 'n',
        Key::O => 'o',
        Key::P => 'p',
        Key::Q => 'q',
        Key::R => 'r',
        Key::S => 's',
        Key::T => 't',
        Key::U => 'u',
        Key::V => 'v',
        Key::W => 'w',
        Key::X => 'x',
        Key::Y => 'y',
        Key::Z => 'z',
        Key::Num0 => '0',
        Key::Num1 => '1',
        Key::Num2 => '2',
        Key::Num3 => '3',
        Key::Num4 => '4',
        Key::Num5 => '5',
        Key::Num6 => '6',
        Key::Num7 => '7',
        Key::Num8 => '8',
        Key::Num9 => '9',
        Key::Space => ' ',
        Key::Minus => '-',
        Key::Plus => '+',
        Key::Equals => '=',
        Key::Slash => '/',
        Key::Questionmark => '?',
        Key::Backslash => '\\',
        Key::Pipe => '|',
        Key::OpenBracket => '[',
        Key::CloseBracket => ']',
        Key::OpenCurlyBracket => '{',
        Key::CloseCurlyBracket => '}',
        Key::Semicolon => ';',
        Key::Colon => ':',
        Key::Comma => ',',
        Key::Period => '.',
        Key::Backtick => '`',
        Key::Quote => '\'',
        Key::Exclamationmark => '!',
        _ => return None,
    };
    Some(if shift && c.is_ascii_alphabetic() { c.to_ascii_uppercase() } else { c })
}

/// xterm's legacy Ctrl encoding. Upstream alacritty receives these bytes
/// pre-composed by winit as key text; egui reports only the logical key, so
/// the byte is derived from the key's character here instead.
fn control_byte(key: Key, mods: Modifiers) -> Option<u8> {
    let c = key_char(key, false)?;
    let byte = match c {
        ' ' | '2' => 0x00,
        'a'..='z' => c as u8 & 0x1f,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '6' => 0x1e,
        '-' | '/' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    let _ = mods;
    Some(byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    use Modifiers as M;

    // egui ships `Modifiers::NONE/CTRL/SHIFT/ALT` constants; combos are
    // built with struct-update syntax inside test bodies.
    fn ctrl_shift() -> Modifiers {
        Modifiers { ctrl: true, shift: true, ..M::NONE }
    }

    #[test]
    fn ctrl_slash_sends_unit_separator() {
        assert_eq!(key_to_bytes(Key::Slash, M::CTRL), Some(vec![0x1f]));
    }

    #[test]
    fn ctrl_letters_send_c0_bytes() {
        assert_eq!(key_to_bytes(Key::A, M::CTRL), Some(vec![0x01]));
        assert_eq!(key_to_bytes(Key::C, M::CTRL), Some(vec![0x03]));
        assert_eq!(key_to_bytes(Key::Z, M::CTRL), Some(vec![0x1a]));
        // Shift+Ctrl+letter sends the same byte as Ctrl+letter.
        assert_eq!(key_to_bytes(Key::C, ctrl_shift()), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_punctuation_matches_xterm() {
        assert_eq!(key_to_bytes(Key::Space, M::CTRL), Some(vec![0x00]));
        assert_eq!(key_to_bytes(Key::Num2, M::CTRL), Some(vec![0x00]));
        assert_eq!(key_to_bytes(Key::OpenBracket, M::CTRL), Some(vec![0x1b]));
        assert_eq!(key_to_bytes(Key::Backslash, M::CTRL), Some(vec![0x1c]));
        assert_eq!(key_to_bytes(Key::CloseBracket, M::CTRL), Some(vec![0x1d]));
        assert_eq!(key_to_bytes(Key::Num6, M::CTRL), Some(vec![0x1e]));
        assert_eq!(key_to_bytes(Key::Minus, M::CTRL), Some(vec![0x1f]));
        assert_eq!(key_to_bytes(Key::Questionmark, M::CTRL), Some(vec![0x7f]));
    }

    #[test]
    fn ctrl_unmapped_key_sends_nothing() {
        assert_eq!(key_to_bytes(Key::Quote, M::CTRL), None);
    }

    #[test]
    fn plain_named_keys_unchanged() {
        assert_eq!(key_to_bytes(Key::ArrowUp, M::NONE), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_bytes(Key::Enter, M::NONE), Some(b"\r".to_vec()));
        assert_eq!(key_to_bytes(Key::Tab, M::NONE), Some(b"\t".to_vec()));
        assert_eq!(key_to_bytes(Key::Backspace, M::NONE), Some(b"\x7f".to_vec()));
        assert_eq!(key_to_bytes(Key::F1, M::NONE), Some(b"\x1bOP".to_vec()));
        assert_eq!(key_to_bytes(Key::F5, M::NONE), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn text_event_passes_through() {
        let ev = Event::Text("é".to_string());
        assert_eq!(event_to_bytes(&ev), Some("é".as_bytes().to_vec()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// egui's clipboard commands ignore Shift, so Ctrl+Shift+C raises the same
    /// synthetic `Copy` as Ctrl+C.  Encoding it would send the interrupt on the
    /// copy shortcut; the Key event emitted alongside it carries the modifiers
    /// and is what the binding table and the encoder act on.
    #[test]
    fn synthetic_clipboard_events_send_nothing() {
        assert_eq!(event_to_bytes(&Event::Copy), None);
        assert_eq!(event_to_bytes(&Event::Cut), None);
    }

    /// The interrupt still has to reach the PTY off the Key event.
    #[test]
    fn ctrl_c_still_sends_sigint() {
        let ctrl = Modifiers { ctrl: true, ..Modifiers::NONE };
        let event = Event::Key {
            key: Key::C,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: ctrl,
        };
        assert_eq!(event_to_bytes(&event), Some(vec![0x03]));
    }
}
