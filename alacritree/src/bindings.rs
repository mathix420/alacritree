//! Parse `[[keyboard.bindings]]` from alacritty's config and match them
//! against egui input events.

use egui::{Key, Modifiers};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct KeyBinding {
    pub key: Key,
    pub mods: Modifiers,
    pub action: BindingAction,
}

#[derive(Debug, Clone)]
pub enum BindingAction {
    Chars(Vec<u8>),
    Named(NamedAction),
    Unsupported(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedAction {
    Paste,
    PasteSelection,
    Copy,
    CopySelection,
    ScrollPageUp,
    ScrollPageDown,
    ScrollHalfPageUp,
    ScrollHalfPageDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollToTop,
    ScrollToBottom,
    ClearHistory,
    SpawnNewInstance,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    ToggleFullscreen,
    ToggleMaximized,
    Minimize,
    SelectNextTab,
    SelectPreviousTab,
    /// 1-indexed.
    SelectTab(u8),
    SelectLastTab,
    ToggleLeftSidebar,
    ToggleRightSidebar,
    SelectNextWorkspace,
    SelectPreviousWorkspace,
    AddProject,
    ToggleSidebarFocus,
    CloseSession,
    FocusProjectsSidebar,
    FocusGitSidebar,
    FocusTerminal,
    /// 1-indexed into the `[[ui.profiles]]` order.
    SpawnProfile(u8),
    Quit,
    /// Used to unbind a key — consumes the press without acting on it.
    NoOp,
    /// Alacritty's pass-through marker: the matching binding runs (no-op for
    /// us) but suppress_chars stays off so the key still reaches the PTY.
    /// Mirrors `Action::ReceiveChar` in `alacritty/src/input/keyboard.rs`.
    ReceiveChar,
}

#[derive(Debug, Deserialize)]
pub struct RawBinding {
    pub key: String,
    #[serde(default)]
    pub mods: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub chars: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub command: Option<toml::Value>,
}

pub fn parse_bindings(raw: Vec<RawBinding>) -> Vec<KeyBinding> {
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        if r.mode.is_some() {
            // vi/search-mode bindings need terminal-mode tracking we don't have.
            continue;
        }
        let Some(key) = parse_key(&r.key) else {
            if !is_silent_unsupported_key(&r.key) {
                log::warn!("ignoring binding for unknown key: {}", r.key);
            }
            continue;
        };
        let mods = match r.mods.as_deref() {
            None => Modifiers::NONE,
            Some(s) => match parse_mods(s) {
                Some(m) => m,
                None => {
                    log::warn!("ignoring binding for '{}': mods '{s}' unavailable here", r.key);
                    continue;
                },
            },
        };
        let action = if let Some(chars) = r.chars {
            BindingAction::Chars(unescape(&chars).into_bytes())
        } else if let Some(action) = r.action {
            parse_action(&action)
        } else if r.command.is_some() {
            BindingAction::Unsupported("command".into())
        } else {
            continue;
        };
        out.push(KeyBinding { key, mods, action });
    }
    // Alacritty replaces a default binding when a user binding has the same
    // trigger — key + mods (`Binding::triggers_match` in
    // `alacritty/src/config/bindings.rs`; modes don't apply here because
    // mode-bindings are dropped above).  Without the filter, a rebound key
    // would run both the user action and the default one, and a key freed
    // via `ReceiveChar` would still trigger the default.
    let user_triggers: Vec<_> = out.iter().map(|b| (b.key, b.mods)).collect();
    let defaults =
        default_bindings().into_iter().filter(|d| !user_triggers.contains(&(d.key, d.mods)));
    out.extend(defaults);
    out
}

/// Alacritty's hardcoded default key bindings.  Alacritty merges these with
/// the user's TOML at runtime; without them, configs that rely on bindings
/// like `Ctrl+Shift+V → Paste` (never written explicitly because they're
/// "always there" in alacritty) silently do nothing.
fn default_bindings() -> Vec<KeyBinding> {
    use NamedAction::*;
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
    let ctrl = Modifiers::CTRL;
    let shift = Modifiers::SHIFT;
    let alt = Modifiers::ALT;
    let alt_shift = Modifiers::ALT | Modifiers::SHIFT;

    let mut b = vec![
        KeyBinding { key: Key::V, mods: ctrl_shift, action: BindingAction::Named(Paste) },
        KeyBinding { key: Key::C, mods: ctrl_shift, action: BindingAction::Named(Copy) },
        KeyBinding { key: Key::Insert, mods: shift, action: BindingAction::Named(PasteSelection) },
        KeyBinding { key: Key::Num0, mods: ctrl, action: BindingAction::Named(ResetFontSize) },
        KeyBinding { key: Key::Equals, mods: ctrl, action: BindingAction::Named(IncreaseFontSize) },
        KeyBinding { key: Key::Plus, mods: ctrl, action: BindingAction::Named(IncreaseFontSize) },
        KeyBinding { key: Key::Minus, mods: ctrl, action: BindingAction::Named(DecreaseFontSize) },
        KeyBinding { key: Key::Home, mods: shift, action: BindingAction::Named(ScrollToTop) },
        KeyBinding { key: Key::End, mods: shift, action: BindingAction::Named(ScrollToBottom) },
        KeyBinding { key: Key::PageUp, mods: shift, action: BindingAction::Named(ScrollPageUp) },
        KeyBinding {
            key: Key::PageDown,
            mods: shift,
            action: BindingAction::Named(ScrollPageDown),
        },
        // Alacritty emits CSI Z for Shift+Tab and ESC + CSI Z for Alt+Shift+Tab
        // so apps that handle reverse-tab (readline, vim, etc.) keep working.
        KeyBinding { key: Key::Tab, mods: shift, action: BindingAction::Chars(b"\x1b[Z".to_vec()) },
        KeyBinding {
            key: Key::Tab,
            mods: alt_shift,
            action: BindingAction::Chars(b"\x1b\x1b[Z".to_vec()),
        },
    ];

    // App-level (alacritree) shortcuts: sidebars, session/workspace cycling,
    // project management.  Each can be rebound, or freed for the PTY with a
    // user binding on the same key+mods (`ReceiveChar` forwards the key,
    // `None` swallows it).
    b.extend([
        KeyBinding { key: Key::B, mods: ctrl, action: BindingAction::Named(ToggleLeftSidebar) },
        KeyBinding { key: Key::G, mods: ctrl, action: BindingAction::Named(ToggleRightSidebar) },
        KeyBinding { key: Key::Tab, mods: ctrl, action: BindingAction::Named(SelectNextTab) },
        KeyBinding {
            key: Key::Tab,
            mods: ctrl_shift,
            action: BindingAction::Named(SelectPreviousTab),
        },
        KeyBinding {
            key: Key::ArrowRight,
            mods: alt,
            action: BindingAction::Named(SelectNextWorkspace),
        },
        KeyBinding {
            key: Key::ArrowLeft,
            mods: alt,
            action: BindingAction::Named(SelectPreviousWorkspace),
        },
        KeyBinding { key: Key::O, mods: ctrl_shift, action: BindingAction::Named(AddProject) },
        KeyBinding {
            key: Key::B,
            mods: ctrl_shift,
            action: BindingAction::Named(ToggleSidebarFocus),
        },
        KeyBinding { key: Key::G, mods: ctrl_shift, action: BindingAction::Named(FocusGitSidebar) },
        KeyBinding { key: Key::W, mods: ctrl_shift, action: BindingAction::Named(CloseSession) },
        KeyBinding { key: Key::T, mods: ctrl, action: BindingAction::Named(SpawnNewInstance) },
        KeyBinding { key: Key::Q, mods: ctrl, action: BindingAction::Named(Quit) },
    ]);

    // macOS uses Cmd instead of Ctrl+Shift for clipboard / window actions.
    #[cfg(target_os = "macos")]
    {
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let cmd_ctrl = Modifiers::COMMAND | Modifiers::CTRL;
        b.extend([
            KeyBinding { key: Key::V, mods: cmd, action: BindingAction::Named(Paste) },
            KeyBinding { key: Key::C, mods: cmd, action: BindingAction::Named(Copy) },
            KeyBinding { key: Key::N, mods: cmd, action: BindingAction::Named(SpawnNewInstance) },
            KeyBinding { key: Key::T, mods: cmd, action: BindingAction::Named(SpawnNewInstance) },
            KeyBinding { key: Key::Num0, mods: cmd, action: BindingAction::Named(ResetFontSize) },
            KeyBinding {
                key: Key::Equals,
                mods: cmd,
                action: BindingAction::Named(IncreaseFontSize),
            },
            KeyBinding {
                key: Key::Plus,
                mods: cmd,
                action: BindingAction::Named(IncreaseFontSize),
            },
            KeyBinding {
                key: Key::Minus,
                mods: cmd,
                action: BindingAction::Named(DecreaseFontSize),
            },
            KeyBinding {
                key: Key::CloseBracket,
                mods: cmd_shift,
                action: BindingAction::Named(SelectNextTab),
            },
            KeyBinding {
                key: Key::OpenBracket,
                mods: cmd_shift,
                action: BindingAction::Named(SelectPreviousTab),
            },
            KeyBinding { key: Key::Num1, mods: cmd, action: BindingAction::Named(SelectTab(1)) },
            KeyBinding { key: Key::Num2, mods: cmd, action: BindingAction::Named(SelectTab(2)) },
            KeyBinding { key: Key::Num3, mods: cmd, action: BindingAction::Named(SelectTab(3)) },
            KeyBinding { key: Key::Num4, mods: cmd, action: BindingAction::Named(SelectTab(4)) },
            KeyBinding { key: Key::Num5, mods: cmd, action: BindingAction::Named(SelectTab(5)) },
            KeyBinding { key: Key::Num6, mods: cmd, action: BindingAction::Named(SelectTab(6)) },
            KeyBinding { key: Key::Num7, mods: cmd, action: BindingAction::Named(SelectTab(7)) },
            KeyBinding { key: Key::Num8, mods: cmd, action: BindingAction::Named(SelectTab(8)) },
            KeyBinding { key: Key::Num9, mods: cmd, action: BindingAction::Named(SelectLastTab) },
            KeyBinding {
                key: Key::F,
                mods: cmd_ctrl,
                action: BindingAction::Named(ToggleFullscreen),
            },
            KeyBinding { key: Key::M, mods: cmd, action: BindingAction::Named(Minimize) },
            KeyBinding { key: Key::K, mods: cmd, action: BindingAction::Named(ClearHistory) },
            KeyBinding { key: Key::Q, mods: cmd, action: BindingAction::Named(Quit) },
        ]);
    }

    b
}

/// Every binding that fires for `(key, mods)`.  Alacritty runs *all* matching
/// bindings (see `Processor::process_key_bindings`), so the user's typical
/// pattern of stacking `ClearLogNotice` + `chars = "\f"` on Ctrl+L works:
/// the first action is our `Unsupported` no-op, the second writes 0x0c.
pub fn all_matches(bindings: &[KeyBinding], key: Key, mods: Modifiers) -> Vec<&BindingAction> {
    bindings
        .iter()
        .filter(|b| b.key == key && mods_match(b.mods, mods))
        .map(|b| &b.action)
        .collect()
}

/// Alacritty semantics: `Control|Shift` does not fire on Ctrl alone even though
/// the modifier sets overlap.  Use egui's `matches_exact`, which requires
/// alt/shift to match the pattern exactly while doing the platform-aware
/// ctrl/cmd dance — egui-winit on Linux populates both `ctrl` and `command` on
/// every Ctrl press, so a naive field-by-field eq would never match.
fn mods_match(required: Modifiers, pressed: Modifiers) -> bool {
    pressed.matches_exact(required)
}

fn parse_key(name: &str) -> Option<Key> {
    let n = name.trim();
    if n.len() == 1 {
        let c = n.chars().next().unwrap().to_ascii_uppercase();
        return char_to_key(c);
    }
    if n == "NumpadEnter" {
        // egui-winit maps both `KeyCode::Enter` and `KeyCode::NumpadEnter` to
        // `egui::Key::Enter`, so we can't tell them apart.  Aliasing NumpadEnter
        // to Enter would silently fire NumpadEnter bindings on the regular
        // Return key — drop the binding instead.
        log::warn!("ignoring NumpadEnter binding: egui cannot distinguish it from Return");
        return None;
    }
    Some(match n {
        "Return" | "Enter" => Key::Enter,
        "Space" => Key::Space,
        "Tab" => Key::Tab,
        "Backspace" | "Back" => Key::Backspace,
        "Escape" | "Esc" => Key::Escape,
        "Insert" => Key::Insert,
        "Delete" => Key::Delete,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Up" => Key::ArrowUp,
        "Down" => Key::ArrowDown,
        "Left" => Key::ArrowLeft,
        "Right" => Key::ArrowRight,
        "Minus" => Key::Minus,
        "Equals" | "Equal" => Key::Equals,
        "Plus" => Key::Plus,
        "Comma" => Key::Comma,
        "Period" => Key::Period,
        "Slash" => Key::Slash,
        "Backslash" => Key::Backslash,
        "Semicolon" => Key::Semicolon,
        "Apostrophe" | "Quote" => Key::Quote,
        "LBracket" | "LeftBracket" => Key::OpenBracket,
        "RBracket" | "RightBracket" => Key::CloseBracket,
        "Grave" | "Backtick" => Key::Backtick,
        // F1..F35.
        n if n.starts_with('F') => {
            let num: u8 = n[1..].parse().ok()?;
            return f_key(num);
        },
        _ => return None,
    })
}

fn char_to_key(c: char) -> Option<Key> {
    Some(match c {
        'A' => Key::A,
        'B' => Key::B,
        'C' => Key::C,
        'D' => Key::D,
        'E' => Key::E,
        'F' => Key::F,
        'G' => Key::G,
        'H' => Key::H,
        'I' => Key::I,
        'J' => Key::J,
        'K' => Key::K,
        'L' => Key::L,
        'M' => Key::M,
        'N' => Key::N,
        'O' => Key::O,
        'P' => Key::P,
        'Q' => Key::Q,
        'R' => Key::R,
        'S' => Key::S,
        'T' => Key::T,
        'U' => Key::U,
        'V' => Key::V,
        'W' => Key::W,
        'X' => Key::X,
        'Y' => Key::Y,
        'Z' => Key::Z,
        '0' => Key::Num0,
        '1' => Key::Num1,
        '2' => Key::Num2,
        '3' => Key::Num3,
        '4' => Key::Num4,
        '5' => Key::Num5,
        '6' => Key::Num6,
        '7' => Key::Num7,
        '8' => Key::Num8,
        '9' => Key::Num9,
        _ => return None,
    })
}

/// Winit key names that egui doesn't model.  Default alacritty configs include
/// a handful of these, so swallow them silently rather than logging noise.
fn is_silent_unsupported_key(name: &str) -> bool {
    matches!(
        name.trim(),
        "Paste"
            | "Copy"
            | "Cut"
            | "Find"
            | "Help"
            | "Undo"
            | "BrowserBack"
            | "BrowserForward"
            | "BrowserRefresh"
            | "BrowserStop"
            | "BrowserHome"
            | "BrowserSearch"
            | "BrowserFavorites"
            | "MediaPlayPause"
            | "MediaStop"
            | "MediaTrackNext"
            | "MediaTrackPrevious"
            | "VolumeUp"
            | "VolumeDown"
            | "VolumeMute"
            // `parse_key` already logs a dedicated message explaining why
            // NumpadEnter is dropped; suppress the generic "unknown key" follow-up.
            | "NumpadEnter"
    )
}

fn f_key(n: u8) -> Option<Key> {
    Some(match n {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        13 => Key::F13,
        14 => Key::F14,
        15 => Key::F15,
        16 => Key::F16,
        17 => Key::F17,
        18 => Key::F18,
        19 => Key::F19,
        20 => Key::F20,
        _ => return None,
    })
}

/// `None` when the chord can't be represented on this platform, so the caller
/// drops the binding rather than letting it fire on the wrong keys.
fn parse_mods(s: &str) -> Option<Modifiers> {
    let mut m = Modifiers::NONE;
    for token in s.split('|') {
        match token.trim() {
            "Control" | "Ctrl" => m.ctrl = true,
            "Shift" => m.shift = true,
            "Alt" | "Option" => m.alt = true,
            "Super" | "Command" | "Meta" => m.command = true,
            other => log::warn!("unknown modifier '{other}'"),
        }
    }
    // Off macOS there is no Super modifier to match on: egui carries no such
    // field, and egui-winit raises `command` on every Ctrl press.  A Super
    // chord could therefore only ever fire on the Ctrl chord instead — and for
    // the clipboard bindings a shared alacritty.toml carries (`Super+C ->
    // Copy`), that means eating Ctrl+C.  Drop it rather than steal the
    // interrupt.
    #[cfg(not(target_os = "macos"))]
    if m.command {
        return None;
    }
    Some(m)
}

fn parse_action(name: &str) -> BindingAction {
    use NamedAction::*;
    match name {
        "Paste" => BindingAction::Named(Paste),
        "PasteSelection" => BindingAction::Named(PasteSelection),
        "Copy" => BindingAction::Named(Copy),
        "CopySelection" => BindingAction::Named(CopySelection),
        "ScrollPageUp" => BindingAction::Named(ScrollPageUp),
        "ScrollPageDown" => BindingAction::Named(ScrollPageDown),
        "ScrollHalfPageUp" => BindingAction::Named(ScrollHalfPageUp),
        "ScrollHalfPageDown" => BindingAction::Named(ScrollHalfPageDown),
        "ScrollLineUp" => BindingAction::Named(ScrollLineUp),
        "ScrollLineDown" => BindingAction::Named(ScrollLineDown),
        "ScrollToTop" => BindingAction::Named(ScrollToTop),
        "ScrollToBottom" => BindingAction::Named(ScrollToBottom),
        "ClearHistory" => BindingAction::Named(ClearHistory),
        "SpawnNewInstance" | "CreateNewWindow" | "CreateNewTab" => {
            BindingAction::Named(SpawnNewInstance)
        },
        "IncreaseFontSize" => BindingAction::Named(IncreaseFontSize),
        "DecreaseFontSize" => BindingAction::Named(DecreaseFontSize),
        "ResetFontSize" => BindingAction::Named(ResetFontSize),
        "ToggleFullscreen" => BindingAction::Named(ToggleFullscreen),
        "ToggleMaximized" => BindingAction::Named(ToggleMaximized),
        "Minimize" => BindingAction::Named(Minimize),
        "SelectNextTab" => BindingAction::Named(SelectNextTab),
        "SelectPreviousTab" => BindingAction::Named(SelectPreviousTab),
        "SelectTab1" => BindingAction::Named(SelectTab(1)),
        "SelectTab2" => BindingAction::Named(SelectTab(2)),
        "SelectTab3" => BindingAction::Named(SelectTab(3)),
        "SelectTab4" => BindingAction::Named(SelectTab(4)),
        "SelectTab5" => BindingAction::Named(SelectTab(5)),
        "SelectTab6" => BindingAction::Named(SelectTab(6)),
        "SelectTab7" => BindingAction::Named(SelectTab(7)),
        "SelectTab8" => BindingAction::Named(SelectTab(8)),
        "SelectTab9" => BindingAction::Named(SelectTab(9)),
        "SelectLastTab" => BindingAction::Named(SelectLastTab),
        "ToggleLeftSidebar" => BindingAction::Named(ToggleLeftSidebar),
        "ToggleRightSidebar" => BindingAction::Named(ToggleRightSidebar),
        "SelectNextWorkspace" => BindingAction::Named(SelectNextWorkspace),
        "SelectPreviousWorkspace" => BindingAction::Named(SelectPreviousWorkspace),
        "AddProject" => BindingAction::Named(AddProject),
        "ToggleSidebarFocus" => BindingAction::Named(ToggleSidebarFocus),
        "CloseSession" => BindingAction::Named(CloseSession),
        "FocusProjectsSidebar" => BindingAction::Named(FocusProjectsSidebar),
        "FocusGitSidebar" => BindingAction::Named(FocusGitSidebar),
        "FocusTerminal" => BindingAction::Named(FocusTerminal),
        "SpawnProfile1" => BindingAction::Named(SpawnProfile(1)),
        "SpawnProfile2" => BindingAction::Named(SpawnProfile(2)),
        "SpawnProfile3" => BindingAction::Named(SpawnProfile(3)),
        "SpawnProfile4" => BindingAction::Named(SpawnProfile(4)),
        "SpawnProfile5" => BindingAction::Named(SpawnProfile(5)),
        "SpawnProfile6" => BindingAction::Named(SpawnProfile(6)),
        "SpawnProfile7" => BindingAction::Named(SpawnProfile(7)),
        "SpawnProfile8" => BindingAction::Named(SpawnProfile(8)),
        "SpawnProfile9" => BindingAction::Named(SpawnProfile(9)),

        "Quit" => BindingAction::Named(Quit),
        "None" => BindingAction::Named(NoOp),
        "ReceiveChar" => BindingAction::Named(ReceiveChar),
        other => BindingAction::Unsupported(other.to_string()),
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('e') => out.push('\u{1b}'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(b) = u8::from_str_radix(&hex, 16) {
                    out.push(b as char);
                }
            },
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Ok(b) = u32::from_str_radix(&hex, 16) {
                    if let Some(c) = char::from_u32(b) {
                        out.push(c);
                    }
                }
            },
            Some(other) => {
                out.push('\\');
                out.push(other);
            },
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_action(key: &str, mods: Option<&str>, action: &str) -> RawBinding {
        RawBinding {
            key: key.into(),
            mods: mods.map(Into::into),
            mode: None,
            chars: None,
            action: Some(action.into()),
            command: None,
        }
    }

    fn parse_one(action: &str) -> BindingAction {
        let raw = RawBinding {
            key: "F1".into(),
            mods: None,
            mode: None,
            chars: None,
            action: Some(action.into()),
            command: None,
        };
        // User bindings are parsed before the appended defaults, so the
        // first entry is ours.
        parse_bindings(vec![raw]).remove(0).action
    }

    fn raw_chars(key: &str, mods: Option<&str>, chars: &str) -> RawBinding {
        RawBinding {
            key: key.into(),
            mods: mods.map(Into::into),
            mode: None,
            chars: Some(chars.into()),
            action: None,
            command: None,
        }
    }

    /// The `NamedAction`s that fire for a key press, ignoring other kinds.
    fn named_matches(bindings: &[KeyBinding], key: Key, mods: Modifiers) -> Vec<NamedAction> {
        all_matches(bindings, key, mods)
            .into_iter()
            .filter_map(|a| match a {
                BindingAction::Named(n) => Some(*n),
                _ => None,
            })
            .collect()
    }

    /// A shared alacritty.toml commonly carries macOS clipboard bindings like
    /// `Super+C -> Copy`.  egui has no Super modifier and egui-winit raises
    /// `command` on every Ctrl press, so honoring that binding here would let
    /// it fire on Ctrl+C and swallow the interrupt every terminal app needs.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn super_binding_does_not_swallow_the_interrupt() {
        let bindings = parse_bindings(vec![raw_action("c", Some("Super"), "Copy")]);
        let ctrl = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
        let matched = all_matches(&bindings, Key::C, ctrl);
        assert!(matched.is_empty(), "Super+C hijacked Ctrl+C: {matched:?}");
    }

    /// Ctrl+Shift+C stays the copy shortcut.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn ctrl_shift_c_still_copies() {
        let bindings = parse_bindings(vec![]);
        let ctrl_shift = Modifiers { ctrl: true, shift: true, command: true, ..Modifiers::NONE };
        let matched = all_matches(&bindings, Key::C, ctrl_shift);
        assert!(
            matched.iter().any(|a| matches!(a, BindingAction::Named(NamedAction::Copy))),
            "Ctrl+Shift+C no longer copies: {matched:?}"
        );
    }

    /// Paste is a Ctrl+Shift+V binding; Ctrl+V belongs to the PTY (SYN).
    #[test]
    fn ctrl_v_is_not_bound_to_paste() {
        let bindings = parse_bindings(vec![]);
        let matched = all_matches(&bindings, Key::V, Modifiers::CTRL);
        assert!(matched.is_empty(), "Ctrl+V is bound: {matched:?}");

        let matched = all_matches(&bindings, Key::V, Modifiers::CTRL | Modifiers::SHIFT);
        assert!(
            matched.iter().any(|a| matches!(a, BindingAction::Named(NamedAction::Paste))),
            "Ctrl+Shift+V no longer pastes: {matched:?}"
        );
    }

    #[test]
    fn spawn_profile_actions_parse() {
        for n in 1..=9u8 {
            let action = parse_one(&format!("SpawnProfile{n}"));
            assert!(
                matches!(action, BindingAction::Named(NamedAction::SpawnProfile(m)) if m == n),
                "SpawnProfile{n} parsed to {action:?}"
            );
        }
    }

    #[test]
    fn user_binding_replaces_same_trigger_default() {
        // `ReceiveChar` on Ctrl+B frees the tmux prefix: the default
        // ToggleLeftSidebar must be gone, not merely outvoted.
        let b = parse_bindings(vec![raw_action("B", Some("Control"), "ReceiveChar")]);
        assert_eq!(named_matches(&b, Key::B, Modifiers::CTRL), vec![NamedAction::ReceiveChar]);
    }

    #[test]
    fn replacement_requires_exact_mods() {
        let b = parse_bindings(vec![raw_action("Tab", Some("Control|Shift"), "SelectLastTab")]);
        assert_eq!(
            named_matches(&b, Key::Tab, Modifiers::CTRL),
            vec![NamedAction::SelectNextTab],
            "Ctrl+Tab default must survive a Ctrl+Shift+Tab user binding"
        );
        assert_eq!(
            named_matches(&b, Key::Tab, Modifiers::CTRL | Modifiers::SHIFT),
            vec![NamedAction::SelectLastTab]
        );
    }

    #[test]
    fn user_rebind_suppresses_default_action() {
        // Regression guard: a rebound Ctrl+Shift+V must not also run the
        // default Paste.
        let b = parse_bindings(vec![raw_chars("V", Some("Control|Shift"), "x")]);
        let m = all_matches(&b, Key::V, Modifiers::CTRL | Modifiers::SHIFT);
        assert!(
            matches!(m.as_slice(), [BindingAction::Chars(c)] if c == b"x"),
            "expected only the user Chars binding, got {m:?}"
        );
    }

    #[test]
    fn new_action_names_parse() {
        for (name, expected) in [
            ("ToggleLeftSidebar", NamedAction::ToggleLeftSidebar),
            ("ToggleRightSidebar", NamedAction::ToggleRightSidebar),
            ("SelectNextWorkspace", NamedAction::SelectNextWorkspace),
            ("SelectPreviousWorkspace", NamedAction::SelectPreviousWorkspace),
            ("AddProject", NamedAction::AddProject),
            ("ToggleSidebarFocus", NamedAction::ToggleSidebarFocus),
            ("FocusProjectsSidebar", NamedAction::FocusProjectsSidebar),
            ("FocusTerminal", NamedAction::FocusTerminal),
            ("FocusGitSidebar", NamedAction::FocusGitSidebar),
        ] {
            let b = parse_bindings(vec![raw_action("F1", None, name)]);
            assert_eq!(named_matches(&b, Key::F1, Modifiers::NONE), vec![expected], "{name}");
        }
    }

    #[test]
    fn user_binding_replaces_sidebar_focus_default() {
        let b = parse_bindings(vec![raw_action("B", Some("Control|Shift"), "ReceiveChar")]);
        assert_eq!(
            named_matches(&b, Key::B, Modifiers::CTRL | Modifiers::SHIFT),
            vec![NamedAction::ReceiveChar]
        );
    }

    #[test]
    fn unknown_action_is_unsupported() {
        let b = parse_bindings(vec![raw_action("F1", None, "FlyToTheMoon")]);
        let m = all_matches(&b, Key::F1, Modifiers::NONE);
        assert!(matches!(m.as_slice(), [BindingAction::Unsupported(n)] if n == "FlyToTheMoon"));
    }

    #[test]
    fn stacked_user_bindings_all_run() {
        let b = parse_bindings(vec![
            raw_action("L", Some("Control"), "ClearHistory"),
            raw_chars("L", Some("Control"), "\\x0c"),
        ]);
        let m = all_matches(&b, Key::L, Modifiers::CTRL);
        assert_eq!(m.len(), 2);
        assert!(matches!(m[0], BindingAction::Named(NamedAction::ClearHistory)));
        assert!(matches!(m[1], BindingAction::Chars(c) if c == b"\x0c"));
    }

    #[test]
    fn mode_binding_does_not_replace_default() {
        let mut r = raw_action("B", Some("Control"), "ToggleViMode");
        r.mode = Some("Vi".into());
        let b = parse_bindings(vec![r]);
        assert_eq!(
            named_matches(&b, Key::B, Modifiers::CTRL),
            vec![NamedAction::ToggleLeftSidebar]
        );
    }

    #[test]
    fn default_app_shortcuts_present_without_user_config() {
        use NamedAction::*;
        let ctrl = Modifiers::CTRL;
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let alt = Modifiers::ALT;
        let b = parse_bindings(Vec::new());
        for (key, mods, expected) in [
            (Key::B, ctrl, ToggleLeftSidebar),
            (Key::G, ctrl, ToggleRightSidebar),
            (Key::Tab, ctrl, SelectNextTab),
            (Key::Tab, ctrl_shift, SelectPreviousTab),
            (Key::ArrowRight, alt, SelectNextWorkspace),
            (Key::ArrowLeft, alt, SelectPreviousWorkspace),
            (Key::O, ctrl_shift, AddProject),
            (Key::T, ctrl, SpawnNewInstance),
            (Key::Q, ctrl, Quit),
            (Key::B, ctrl_shift, ToggleSidebarFocus),
            (Key::G, ctrl_shift, FocusGitSidebar),
        ] {
            assert_eq!(named_matches(&b, key, mods), vec![expected], "{key:?}+{mods:?}");
        }
    }

    #[test]
    fn out_of_range_spawn_profile_is_unsupported() {
        for name in ["SpawnProfile0", "SpawnProfile10", "SpawnProfile"] {
            let action = parse_one(name);
            assert!(
                matches!(&action, BindingAction::Unsupported(s) if s == name),
                "{name} parsed to {action:?}"
            );
        }
    }

    #[test]
    fn close_session_is_a_default_ctrl_shift_w_binding() {
        let b = parse_bindings(vec![]);
        assert_eq!(
            named_matches(&b, Key::W, Modifiers::CTRL | Modifiers::SHIFT),
            vec![NamedAction::CloseSession]
        );
    }

    #[test]
    fn close_session_parses_from_config_name() {
        assert!(matches!(
            parse_action("CloseSession"),
            BindingAction::Named(NamedAction::CloseSession)
        ));
    }
}
