use alacritty_terminal::term::TermMode;
use egui::{Event, Key, Modifiers};

pub fn event_to_bytes(event: &Event, mode: TermMode) -> Option<Vec<u8>> {
    match event {
        // OS-composed text — already accounts for Shift and dead-key composition.
        // When the app asked for every key as an escape sequence, the Key event
        // already encodes the press; passing the text too would double the input.
        Event::Text(text) if !text.is_empty() => {
            if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
                None
            } else {
                Some(text.as_bytes().to_vec())
            }
        },
        Event::Key { key, pressed: true, modifiers, repeat: _, .. } => {
            key_to_bytes(*key, *modifiers, mode)
        },
        // `Event::Paste` is handled by the caller via `paste::paste` so it
        // gets bracketed-paste wrapping and newline normalization.  `Copy` and
        // `Cut` carry no modifiers, so they can't tell Ctrl+C from Ctrl+Shift+C
        // and must not be encoded here; the Key event alongside them does it.
        _ => None,
    }
}

pub fn key_to_bytes(key: Key, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if should_build_kitty(key, mods, mode)
        && let Some(bytes) = kitty_sequence(key, mods)
    {
        return Some(bytes);
    }

    // Named keys never produce composed text, so every modifier combination
    // is safe to encode.
    if let Some(bytes) = named_key_bytes(key, mods) {
        return Some(bytes);
    }

    // winit reports AltGr as Ctrl+Alt.  A printable key carrying both must
    // stay silent: the composed character arrives via `Event::Text`, and
    // emitting bytes here too would double the input.
    if mods.ctrl && mods.alt {
        return None;
    }

    if mods.ctrl {
        return control_byte(key, mods).map(|b| vec![b]);
    }

    // Plain Alt is meta only where no composed text follows the key event.
    // Windows delivers no `Event::Text` alongside Alt+<printable>, so the
    // ESC prefix is safe there. On macOS, Option composes characters
    // (Option+B is "∫"), and on Linux xkb composes the plain character
    // regardless of Alt — both arrive via `Event::Text`, so emitting bytes
    // here as well would double the input. Matches upstream alacritty's
    // macOS default (`option_as_alt = "None"`: Option composes, not meta).
    #[cfg(windows)]
    if mods.alt {
        // Long-standing meta convention: Alt+char sends ESC + char.
        let c = key_char(key, mods.shift)?;
        let mut out = vec![0x1b];
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return Some(out);
    }

    None
}

/// xterm modifier parameter: `1 + (Shift=1 | Alt=2 | Ctrl=4)`, as an ASCII
/// digit.  `None` when no encodable modifier is held, so callers can emit
/// the shorter unmodified sequence.
fn csi_modifier(mods: Modifiers) -> Option<u8> {
    let m = (mods.shift as u8) | ((mods.alt as u8) << 1) | ((mods.ctrl as u8) << 2);
    (m != 0).then_some(b'1' + m)
}

/// Kitty modifier bits (Shift=1, Alt=2, Ctrl=4, Super=8), before the
/// protocol's +1 offset.
fn kitty_mods(mods: Modifiers) -> u8 {
    (mods.shift as u8)
        | ((mods.alt as u8) << 1)
        | ((mods.ctrl as u8) << 2)
        | ((mods.mac_cmd as u8) << 3)
}

/// Mirror of alacritty's `should_build_sequence`, reduced to what egui can
/// observe (no key location, so numpad disambiguation is unavailable).
fn should_build_kitty(key: Key, mods: Modifiers, mode: TermMode) -> bool {
    if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        return true;
    }
    if !mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        return false;
    }
    let m = kitty_mods(mods);
    key == Key::Escape
        || (m != 0 && (m != 1 || matches!(key, Key::Tab | Key::Enter | Key::Backspace)))
}

/// CSI-u encoding for the keys whose legacy bytes are ambiguous: the C0
/// control keys plus modified printables.  Arrows, F-keys and the editing
/// block keep their legacy CSI encodings even under the kitty protocol, so
/// they fall through to `named_key_bytes`.
fn kitty_sequence(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    let code: u32 = match key {
        Key::Tab => 9,
        Key::Enter => 13,
        Key::Escape => 27,
        Key::Backspace => 127,
        _ => {
            // winit reports AltGr as Ctrl+Alt; the composed character arrives
            // via Event::Text, same as on the legacy path.
            if mods.ctrl && mods.alt {
                return None;
            }
            // Kitty wants the unshifted key code, with Shift reported in the
            // modifier field.  Shifted punctuation arrives as its own logical
            // key in egui and carries no layout info, so it is used as-is —
            // upstream resolves it via winit's key_without_modifiers.
            u32::from(key_char(key, false)?)
        },
    };
    let m = kitty_mods(mods);
    let seq = if m == 0 { format!("\x1b[{code}u") } else { format!("\x1b[{code};{}u", m + 1) };
    Some(seq.into_bytes())
}

/// Encoding for keys that never produce composed text.  Because no
/// `Event::Text` follows these, every modifier combination is safe to encode
/// here — including Ctrl+Alt, which on printables must stay silent (AltGr).
fn named_key_bytes(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
    // Arrows/Home/End: `ESC [ <final>`, or `ESC [ 1 ; <m> <final>` modified.
    let csi = |final_byte: u8| match csi_modifier(mods) {
        Some(m) => vec![0x1b, b'[', b'1', b';', m, final_byte],
        None => vec![0x1b, b'[', final_byte],
    };
    // F1-F4 are SS3 (`ESC O <f>`) unmodified but switch to CSI when modified.
    let ss3 = |final_byte: u8| match csi_modifier(mods) {
        Some(m) => vec![0x1b, b'[', b'1', b';', m, final_byte],
        None => vec![0x1b, b'O', final_byte],
    };
    // Editing/function keys: `ESC [ <n> ~`, or `ESC [ <n> ; <m> ~` modified.
    let tilde = |num: &[u8]| {
        let mut v = vec![0x1b, b'['];
        v.extend_from_slice(num);
        if let Some(m) = csi_modifier(mods) {
            v.push(b';');
            v.push(m);
        }
        v.push(b'~');
        v
    };

    let bytes = match key {
        Key::ArrowUp => csi(b'A'),
        Key::ArrowDown => csi(b'B'),
        Key::ArrowRight => csi(b'C'),
        Key::ArrowLeft => csi(b'D'),
        Key::Home => csi(b'H'),
        Key::End => csi(b'F'),
        Key::Insert => tilde(b"2"),
        Key::Delete => tilde(b"3"),
        Key::PageUp => tilde(b"5"),
        Key::PageDown => tilde(b"6"),
        Key::F1 => ss3(b'P'),
        Key::F2 => ss3(b'Q'),
        Key::F3 => ss3(b'R'),
        Key::F4 => ss3(b'S'),
        Key::F5 => tilde(b"15"),
        Key::F6 => tilde(b"17"),
        Key::F7 => tilde(b"18"),
        Key::F8 => tilde(b"19"),
        Key::F9 => tilde(b"20"),
        Key::F10 => tilde(b"21"),
        Key::F11 => tilde(b"23"),
        Key::F12 => tilde(b"24"),
        Key::Tab if mods.shift => vec![0x1b, b'[', b'Z'],
        Key::Enter | Key::Tab | Key::Backspace | Key::Escape => {
            let base: &[u8] = match key {
                Key::Enter => b"\r",
                Key::Tab => b"\t",
                Key::Backspace => b"\x7f",
                Key::Escape => b"\x1b",
                _ => unreachable!(),
            };
            if mods.alt {
                // Long-standing meta convention: Alt+key sends ESC + key.
                let mut out = Vec::with_capacity(base.len() + 1);
                out.push(0x1b);
                out.extend_from_slice(base);
                out
            } else {
                base.to_vec()
            }
        },
        _ => return None,
    };
    Some(bytes)
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
    let c = key_char(key, mods.shift)?;
    let byte = match c {
        ' ' | '2' => 0x00,
        'a'..='z' => c as u8 & 0x1f,
        'A'..='Z' => c.to_ascii_lowercase() as u8 & 0x1f,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '6' => 0x1e,
        '-' | '/' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
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

    /// Legacy-mode shorthand; kitty-protocol tests call `super::key_to_bytes`
    /// with an explicit mode instead.
    fn key_to_bytes(key: Key, mods: Modifiers) -> Option<Vec<u8>> {
        super::key_to_bytes(key, mods, TermMode::empty())
    }

    const DISAMBIGUATE: TermMode = TermMode::DISAMBIGUATE_ESC_CODES;
    const REPORT_ALL: TermMode = TermMode::REPORT_ALL_KEYS_AS_ESC;

    #[test]
    fn kitty_disambiguate_encodes_modified_enter() {
        assert_eq!(
            super::key_to_bytes(Key::Enter, M::SHIFT, DISAMBIGUATE),
            Some(b"\x1b[13;2u".to_vec())
        );
        assert_eq!(
            super::key_to_bytes(Key::Enter, M::ALT, DISAMBIGUATE),
            Some(b"\x1b[13;3u".to_vec())
        );
        assert_eq!(
            super::key_to_bytes(Key::Enter, M::CTRL, DISAMBIGUATE),
            Some(b"\x1b[13;5u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_encodes_shifted_tab_and_backspace() {
        assert_eq!(
            super::key_to_bytes(Key::Tab, M::SHIFT, DISAMBIGUATE),
            Some(b"\x1b[9;2u".to_vec())
        );
        assert_eq!(
            super::key_to_bytes(Key::Backspace, M::SHIFT, DISAMBIGUATE),
            Some(b"\x1b[127;2u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_keeps_plain_keys_legacy() {
        assert_eq!(super::key_to_bytes(Key::Enter, M::NONE, DISAMBIGUATE), Some(b"\r".to_vec()));
        assert_eq!(super::key_to_bytes(Key::Tab, M::NONE, DISAMBIGUATE), Some(b"\t".to_vec()));
        assert_eq!(
            super::key_to_bytes(Key::Backspace, M::NONE, DISAMBIGUATE),
            Some(b"\x7f".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_always_escapes_escape() {
        assert_eq!(
            super::key_to_bytes(Key::Escape, M::NONE, DISAMBIGUATE),
            Some(b"\x1b[27u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_encodes_ctrl_printables_unshifted() {
        assert_eq!(
            super::key_to_bytes(Key::A, M::CTRL, DISAMBIGUATE),
            Some(b"\x1b[97;5u".to_vec())
        );
        assert_eq!(
            super::key_to_bytes(Key::Space, M::CTRL, DISAMBIGUATE),
            Some(b"\x1b[32;5u".to_vec())
        );
        // Shift is reported in the modifier field, not in the key code.
        assert_eq!(
            super::key_to_bytes(Key::C, ctrl_shift(), DISAMBIGUATE),
            Some(b"\x1b[99;6u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_leaves_shift_only_printables_to_text() {
        assert_eq!(super::key_to_bytes(Key::A, M::SHIFT, DISAMBIGUATE), None);
    }

    #[test]
    fn kitty_disambiguate_keeps_legacy_csi_for_modified_arrows() {
        assert_eq!(
            super::key_to_bytes(Key::ArrowUp, M::CTRL, DISAMBIGUATE),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_altgr_printables_stay_silent() {
        let ctrl_alt = Modifiers { ctrl: true, alt: true, ..Modifiers::NONE };
        assert_eq!(super::key_to_bytes(Key::Q, ctrl_alt, DISAMBIGUATE), None);
    }

    #[test]
    fn kitty_report_all_encodes_plain_keys_and_mutes_text() {
        assert_eq!(
            super::key_to_bytes(Key::Enter, M::NONE, REPORT_ALL),
            Some(b"\x1b[13u".to_vec())
        );
        assert_eq!(super::key_to_bytes(Key::A, M::NONE, REPORT_ALL), Some(b"\x1b[97u".to_vec()));
        assert_eq!(event_to_bytes(&Event::Text("a".to_string()), REPORT_ALL), None);
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
        assert_eq!(event_to_bytes(&ev, TermMode::empty()), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn modified_arrows_and_nav_keys_use_csi_modifiers() {
        assert_eq!(key_to_bytes(Key::ArrowRight, M::CTRL), Some(b"\x1b[1;5C".to_vec()));
        assert_eq!(key_to_bytes(Key::ArrowLeft, M::ALT), Some(b"\x1b[1;3D".to_vec()));
        assert_eq!(key_to_bytes(Key::ArrowUp, M::SHIFT), Some(b"\x1b[1;2A".to_vec()));
        assert_eq!(
            key_to_bytes(Key::Home, Modifiers { ctrl: true, shift: true, ..Modifiers::NONE }),
            Some(b"\x1b[1;6H".to_vec())
        );
        assert_eq!(key_to_bytes(Key::Delete, M::SHIFT), Some(b"\x1b[3;2~".to_vec()));
        assert_eq!(key_to_bytes(Key::PageUp, M::CTRL), Some(b"\x1b[5;5~".to_vec()));
    }

    #[test]
    fn modified_function_keys() {
        // Modified F1-F4 switch from SS3 to CSI form.
        assert_eq!(key_to_bytes(Key::F1, M::SHIFT), Some(b"\x1b[1;2P".to_vec()));
        assert_eq!(key_to_bytes(Key::F5, M::CTRL), Some(b"\x1b[15;5~".to_vec()));
    }

    #[test]
    fn shift_tab_sends_backtab() {
        assert_eq!(key_to_bytes(Key::Tab, M::SHIFT), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn alt_on_simple_named_keys_prefixes_esc() {
        assert_eq!(key_to_bytes(Key::Enter, M::ALT), Some(b"\x1b\r".to_vec()));
        assert_eq!(key_to_bytes(Key::Backspace, M::ALT), Some(b"\x1b\x7f".to_vec()));
    }

    #[test]
    fn ctrl_alt_on_named_keys_is_encoded_not_suppressed() {
        // AltGr suppression applies to printables only; arrows never compose
        // text, so Ctrl+Alt encodes as modifier 7.
        let ctrl_alt = Modifiers { ctrl: true, alt: true, ..Modifiers::NONE };
        assert_eq!(key_to_bytes(Key::ArrowRight, ctrl_alt), Some(b"\x1b[1;7C".to_vec()));
    }

    #[cfg(windows)]
    #[test]
    fn alt_printables_send_esc_prefixed_char() {
        assert_eq!(key_to_bytes(Key::B, M::ALT), Some(b"\x1bb".to_vec()));
        assert_eq!(
            key_to_bytes(Key::B, Modifiers { alt: true, shift: true, ..Modifiers::NONE }),
            Some(b"\x1bB".to_vec())
        );
        assert_eq!(key_to_bytes(Key::Period, M::ALT), Some(b"\x1b.".to_vec()));
        assert_eq!(key_to_bytes(Key::Num1, M::ALT), Some(b"\x1b1".to_vec()));
    }

    /// Off Windows the composed character arrives via `Event::Text`, so the
    /// key event itself must stay silent (see the cfg in `key_to_bytes`).
    #[cfg(not(windows))]
    #[test]
    fn alt_printables_stay_silent_where_text_composes() {
        assert_eq!(key_to_bytes(Key::B, M::ALT), None);
        assert_eq!(key_to_bytes(Key::Period, M::ALT), None);
    }

    #[test]
    fn ctrl_alt_printables_stay_silent_for_altgr() {
        // winit reports AltGr as Ctrl+Alt; the composed character arrives via
        // Event::Text, so emitting bytes here would double the input.
        let ctrl_alt = Modifiers { ctrl: true, alt: true, ..Modifiers::NONE };
        assert_eq!(key_to_bytes(Key::Q, ctrl_alt), None);
        assert_eq!(key_to_bytes(Key::Num2, ctrl_alt), None);
        assert_eq!(key_to_bytes(Key::OpenBracket, ctrl_alt), None);
    }

    /// egui's clipboard commands ignore Shift, so Ctrl+Shift+C raises the same
    /// synthetic `Copy` as Ctrl+C.  Encoding it would send the interrupt on the
    /// copy shortcut; the Key event emitted alongside it carries the modifiers
    /// and is what the binding table and the encoder act on.
    #[test]
    fn synthetic_clipboard_events_send_nothing() {
        assert_eq!(event_to_bytes(&Event::Copy, TermMode::empty()), None);
        assert_eq!(event_to_bytes(&Event::Cut, TermMode::empty()), None);
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
        assert_eq!(event_to_bytes(&event, TermMode::empty()), Some(vec![0x03]));
    }

    /// The kitty encodings above only ever run if the terminal negotiates the
    /// protocol, and `Term` ignores an app's request for it unless
    /// `TermConfig::kitty_keyboard` is set.  Drives the enable sequence a real
    /// app sends through a terminal built from alacritree's own config, so the
    /// negotiation and the encoding are covered as one path rather than the
    /// mode being assumed.
    #[test]
    fn shift_enter_is_kitty_encoded_once_an_app_enables_the_protocol() {
        use alacritty_terminal::Term;
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::config::Config;
        use crate::session::{TermSize, term_config};

        let size = TermSize::new(80, 24);
        let mut term = Term::new(term_config(&Config::default()), &size, VoidListener);

        // `CSI > 1 u`: push a keyboard mode with the disambiguate flag, which
        // is what Claude Code and neovim send on startup.
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[>1u");

        let mode = *term.mode();
        assert!(
            mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            "terminal ignored the kitty enable sequence; mode is {mode:?}"
        );
        assert_eq!(super::key_to_bytes(Key::Enter, M::SHIFT, mode), Some(b"\x1b[13;2u".to_vec()));
    }
}
