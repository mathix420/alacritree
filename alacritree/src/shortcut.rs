//! Where egui key presses meet the bindings table.  `bindings` names keys and
//! modifiers in its own types so the config parser links no GUI framework, and
//! this module translates them once, when the app is built.

use crate::bindings::{BindingAction, Key, KeyBinding, Modifiers, NamedAction};

/// The configured bindings with each key already in egui's terms and each
/// trigger already spelled, so a key press or a painted palette row converts
/// nothing.
pub struct Shortcuts {
    entries: Vec<Shortcut>,
}

struct Shortcut {
    key: egui::Key,
    mods: Modifiers,
    action: BindingAction,
    label: String,
}

impl Shortcuts {
    pub fn new(bindings: &[KeyBinding]) -> Self {
        let entries = bindings
            .iter()
            .map(|b| Shortcut {
                key: egui_key(b.key),
                mods: b.mods,
                action: b.action.clone(),
                label: label(b.key, b.mods),
            })
            .collect();
        Self { entries }
    }

    /// Every binding that fires for a key press.  Alacritty runs *all*
    /// matching bindings (see `Processor::process_key_bindings`), so the
    /// user's typical pattern of stacking `ClearLogNotice` + `chars = "\f"` on
    /// Ctrl+L works: the first action is our `Unsupported` no-op, the second
    /// writes 0x0c.
    pub fn matches(&self, key: egui::Key, mods: egui::Modifiers) -> Vec<&BindingAction> {
        let mods = binding_mods(mods);
        self.entries
            .iter()
            .filter(|s| s.key == key && mods.fires(s.mods))
            .map(|s| &s.action)
            .collect()
    }

    /// Every bound action, in binding order.
    pub fn actions(&self) -> impl Iterator<Item = &BindingAction> {
        self.entries.iter().map(|s| &s.action)
    }

    /// The spelled triggers bound to `action`, in binding order: user bindings
    /// before the defaults they did not replace.
    pub fn labels(&self, action: NamedAction) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(move |s| matches!(s.action, BindingAction::Named(a) if a == action))
            .map(|s| s.label.as_str())
    }
}

/// egui-winit raises `command` alongside `ctrl` on every Ctrl press off macOS,
/// and sets `mac_cmd` only where `command` already says the same, so dropping
/// `mac_cmd` loses nothing `Modifiers::fires` needs.
fn binding_mods(mods: egui::Modifiers) -> Modifiers {
    Modifiers { alt: mods.alt, ctrl: mods.ctrl, shift: mods.shift, command: mods.command }
}

fn label(key: Key, mods: Modifiers) -> String {
    let mods = egui::Modifiers {
        alt: mods.alt,
        ctrl: mods.ctrl,
        shift: mods.shift,
        mac_cmd: false,
        command: mods.command,
    };
    egui::KeyboardShortcut::new(mods, egui_key(key))
        .format(&egui::ModifierNames::NAMES, cfg!(target_os = "macos"))
}

fn egui_key(key: Key) -> egui::Key {
    match key {
        Key::ArrowDown => egui::Key::ArrowDown,
        Key::ArrowLeft => egui::Key::ArrowLeft,
        Key::ArrowRight => egui::Key::ArrowRight,
        Key::ArrowUp => egui::Key::ArrowUp,
        Key::Escape => egui::Key::Escape,
        Key::Tab => egui::Key::Tab,
        Key::Backspace => egui::Key::Backspace,
        Key::Enter => egui::Key::Enter,
        Key::Space => egui::Key::Space,
        Key::Insert => egui::Key::Insert,
        Key::Delete => egui::Key::Delete,
        Key::Home => egui::Key::Home,
        Key::End => egui::Key::End,
        Key::PageUp => egui::Key::PageUp,
        Key::PageDown => egui::Key::PageDown,
        Key::Colon => egui::Key::Colon,
        Key::Comma => egui::Key::Comma,
        Key::Backslash => egui::Key::Backslash,
        Key::Slash => egui::Key::Slash,
        Key::OpenBracket => egui::Key::OpenBracket,
        Key::CloseBracket => egui::Key::CloseBracket,
        Key::Backtick => egui::Key::Backtick,
        Key::Minus => egui::Key::Minus,
        Key::Period => egui::Key::Period,
        Key::Plus => egui::Key::Plus,
        Key::Equals => egui::Key::Equals,
        Key::Semicolon => egui::Key::Semicolon,
        Key::Quote => egui::Key::Quote,
        Key::Num0 => egui::Key::Num0,
        Key::Num1 => egui::Key::Num1,
        Key::Num2 => egui::Key::Num2,
        Key::Num3 => egui::Key::Num3,
        Key::Num4 => egui::Key::Num4,
        Key::Num5 => egui::Key::Num5,
        Key::Num6 => egui::Key::Num6,
        Key::Num7 => egui::Key::Num7,
        Key::Num8 => egui::Key::Num8,
        Key::Num9 => egui::Key::Num9,
        Key::A => egui::Key::A,
        Key::B => egui::Key::B,
        Key::C => egui::Key::C,
        Key::D => egui::Key::D,
        Key::E => egui::Key::E,
        Key::F => egui::Key::F,
        Key::G => egui::Key::G,
        Key::H => egui::Key::H,
        Key::I => egui::Key::I,
        Key::J => egui::Key::J,
        Key::K => egui::Key::K,
        Key::L => egui::Key::L,
        Key::M => egui::Key::M,
        Key::N => egui::Key::N,
        Key::O => egui::Key::O,
        Key::P => egui::Key::P,
        Key::Q => egui::Key::Q,
        Key::R => egui::Key::R,
        Key::S => egui::Key::S,
        Key::T => egui::Key::T,
        Key::U => egui::Key::U,
        Key::V => egui::Key::V,
        Key::W => egui::Key::W,
        Key::X => egui::Key::X,
        Key::Y => egui::Key::Y,
        Key::Z => egui::Key::Z,
        Key::F1 => egui::Key::F1,
        Key::F2 => egui::Key::F2,
        Key::F3 => egui::Key::F3,
        Key::F4 => egui::Key::F4,
        Key::F5 => egui::Key::F5,
        Key::F6 => egui::Key::F6,
        Key::F7 => egui::Key::F7,
        Key::F8 => egui::Key::F8,
        Key::F9 => egui::Key::F9,
        Key::F10 => egui::Key::F10,
        Key::F11 => egui::Key::F11,
        Key::F12 => egui::Key::F12,
        Key::F13 => egui::Key::F13,
        Key::F14 => egui::Key::F14,
        Key::F15 => egui::Key::F15,
        Key::F16 => egui::Key::F16,
        Key::F17 => egui::Key::F17,
        Key::F18 => egui::Key::F18,
        Key::F19 => egui::Key::F19,
        Key::F20 => egui::Key::F20,
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator;

    use super::*;
    use crate::bindings::{RawBinding, parse_bindings};

    fn named(shortcuts: &Shortcuts, key: egui::Key, mods: egui::Modifiers) -> Vec<NamedAction> {
        shortcuts
            .matches(key, mods)
            .into_iter()
            .filter_map(|a| match a {
                BindingAction::Named(n) => Some(*n),
                _ => None,
            })
            .collect()
    }

    /// Both key types spell each key the same way, so a key whose arm points
    /// at any other egui key fails here by name.
    #[test]
    fn every_binding_key_converts_to_the_egui_key_of_the_same_name() {
        for key in Key::iter() {
            assert_eq!(format!("{:?}", egui_key(key)), format!("{key:?}"));
        }
    }

    /// egui-winit's Ctrl press carries `command` too off macOS; a Ctrl binding
    /// still fires on it, and an unmodified binding on the same key does not.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_ctrl_press_carrying_command_fires_ctrl_bindings_only() {
        let shortcuts = Shortcuts::new(&parse_bindings(vec![RawBinding {
            key: "L".into(),
            mods: None,
            mode: None,
            chars: None,
            action: Some("ToggleSessionRows".into()),
            command: None,
        }]));
        let ctrl = egui::Modifiers { ctrl: true, command: true, ..egui::Modifiers::NONE };
        assert_eq!(named(&shortcuts, egui::Key::K, ctrl), vec![NamedAction::TogglePalette]);
        assert!(named(&shortcuts, egui::Key::L, ctrl).is_empty());
        assert_eq!(named(&shortcuts, egui::Key::L, egui::Modifiers::NONE), vec![
            NamedAction::ToggleSessionRows
        ]);
    }

    #[test]
    fn a_key_no_binding_names_fires_nothing() {
        let shortcuts = Shortcuts::new(&parse_bindings(Vec::new()));
        assert!(shortcuts.matches(egui::Key::F35, egui::Modifiers::NONE).is_empty());
    }
}
