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
    /// Like SelectNextTab/SelectPreviousTab, but one flat ring over every
    /// open session: crossing a workspace boundary switches workspaces.
    SelectNextSession,
    SelectPreviousSession,
    ToggleLeftSidebar,
    ToggleRightSidebar,
    SelectNextWorkspace,
    SelectPreviousWorkspace,
    AddProject,
    ToggleSidebarFocus,
    CloseSession,
    SidebarTop,
    SidebarBottom,
    SidebarNextProject,
    SidebarPreviousProject,
    /// Re-run worktree discovery for every project in the sidebar.
    RefreshProjects,
    /// Act on the sidebar-cursored row: a session closes, a worktree gets
    /// the delete/prune dialog, a project gets the remove-from-sidebar
    /// prompt.
    DeleteSelected,
    /// Rename the sidebar-cursored row: a session gets a custom display
    /// name, a project gets the same rename dialog as its context menu.
    RenameSelected,
    /// Expand or collapse the sidebar-cursored project, showing or hiding
    /// its worktrees and sessions.
    ToggleProjectExpanded,
    /// Open (or close) the Ctrl+K command palette.
    TogglePalette,
    FocusProjectsSidebar,
    FocusGitSidebar,
    FocusTerminal,
    /// Flip the runtime `session_display.sidebar_always` value.
    ToggleSessionRows,
    /// Flip the runtime `session_display.tabs_always` value.
    ToggleSessionTabs,
    /// Open the base-branch picker for the sidebar-cursored or current worktree.
    SetBaseBranch,
    /// 1-indexed into the `[[ui.profiles]]` order.
    SpawnProfile(u8),
    Quit,
    /// Used to unbind a key — consumes the press without acting on it.
    NoOp,
    /// Alacritty's pass-through marker: the matching binding runs (no-op for
    /// us) but suppress_chars stays off so the key still reaches the PTY.
    /// Mirrors `Action::ReceiveChar` in `alacritty/src/input/keyboard.rs`.
    ReceiveChar,
    /// Directional panel focus with TUI passthrough (see `focus_move` in
    /// `app.rs`).
    FocusLeft,
    FocusRight,
}

impl NamedAction {
    /// Actions that only make sense while the projects sidebar owns focus.
    /// Their default keys (unmodified Home/End/PageUp/PageDown/R/O/Delete and
    /// Shift+R) are terminal input the rest of the time, so dispatch must
    /// not consume them unless the sidebar owns focus.
    pub fn is_sidebar_scoped(&self) -> bool {
        matches!(
            self,
            Self::SidebarTop
                | Self::SidebarBottom
                | Self::SidebarNextProject
                | Self::SidebarPreviousProject
                | Self::RefreshProjects
                | Self::DeleteSelected
                | Self::RenameSelected
                | Self::ToggleProjectExpanded
        )
    }

    /// The name `parse_action` accepts for this action — what a user writes
    /// in `[[keyboard.bindings]]`, and the label the shortcuts window shows.
    pub fn config_name(&self) -> String {
        match self {
            Self::SelectTab(n) => format!("SelectTab{n}"),
            Self::SpawnProfile(n) => format!("SpawnProfile{n}"),
            other => format!("{other:?}"),
        }
    }

    /// One-line human description for the shortcuts window.
    pub fn description(&self) -> String {
        match self {
            Self::Paste => "Paste from the clipboard".into(),
            Self::PasteSelection => "Paste from the primary (X11) selection".into(),
            Self::Copy => "Copy the selection to the clipboard".into(),
            Self::CopySelection => "Copy the selection to the primary selection".into(),
            Self::ScrollPageUp => "Scroll the scrollback one page up".into(),
            Self::ScrollPageDown => "Scroll the scrollback one page down".into(),
            Self::ScrollHalfPageUp => "Scroll the scrollback half a page up".into(),
            Self::ScrollHalfPageDown => "Scroll the scrollback half a page down".into(),
            Self::ScrollLineUp => "Scroll the scrollback one line up".into(),
            Self::ScrollLineDown => "Scroll the scrollback one line down".into(),
            Self::ScrollToTop => "Scroll to the top of the scrollback".into(),
            Self::ScrollToBottom => "Scroll to the bottom of the scrollback".into(),
            Self::ClearHistory => "Clear the scrollback buffer".into(),
            Self::SpawnNewInstance => "Open a new shell session in the current workspace".into(),
            Self::IncreaseFontSize => "Increase the font size".into(),
            Self::DecreaseFontSize => "Decrease the font size".into(),
            Self::ResetFontSize => "Reset the font size".into(),
            Self::ToggleFullscreen => "Toggle fullscreen".into(),
            Self::ToggleMaximized => "Toggle the maximized window state".into(),
            Self::Minimize => "Minimize the window".into(),
            Self::SelectNextTab => "Cycle to the next session in the workspace".into(),
            Self::SelectPreviousTab => "Cycle to the previous session in the workspace".into(),
            Self::SelectTab(n) => format!("Select session {n} in the current workspace"),
            Self::SelectLastTab => "Select the last session in the current workspace".into(),
            Self::SelectNextSession => {
                "Cycle to the next session, continuing across workspaces".into()
            },
            Self::SelectPreviousSession => {
                "Cycle to the previous session, continuing across workspaces".into()
            },
            Self::ToggleLeftSidebar => "Toggle the projects sidebar".into(),
            Self::ToggleRightSidebar => "Toggle the git sidebar".into(),
            Self::SelectNextWorkspace => "Switch to the next workspace".into(),
            Self::SelectPreviousWorkspace => "Switch to the previous workspace".into(),
            Self::AddProject => "Add a project to the sidebar".into(),
            Self::ToggleSidebarFocus => "Toggle keyboard focus between terminal and sidebar".into(),
            Self::CloseSession => "Close the cursored or active session".into(),
            Self::SidebarTop => "Move the sidebar cursor to the first row".into(),
            Self::SidebarBottom => "Move the sidebar cursor to the last row".into(),
            Self::SidebarNextProject => "Jump the sidebar cursor to the next project".into(),
            Self::SidebarPreviousProject => {
                "Jump the sidebar cursor to the previous project".into()
            },
            Self::RefreshProjects => "Rescan every project's worktrees".into(),
            Self::DeleteSelected => {
                "Close the selected session, delete the selected worktree, or remove the selected \
                 project"
                    .into()
            },
            Self::RenameSelected => "Rename the selected project".into(),
            Self::ToggleProjectExpanded => "Expand or collapse the selected project".into(),
            Self::FocusProjectsSidebar => "Focus the projects sidebar".into(),
            Self::FocusGitSidebar => "Focus the git sidebar".into(),
            Self::FocusTerminal => "Focus the terminal".into(),
            Self::SpawnProfile(n) => format!("Open a session with shell profile {n}"),
            Self::Quit => "Open the quit confirmation dialog".into(),
            Self::TogglePalette => "Open the command palette".into(),
            Self::ToggleSessionRows => "Toggle single-session sidebar rows".into(),
            Self::ToggleSessionTabs => "Toggle single-session tab segments".into(),
            Self::SetBaseBranch => "Choose the branch the git panel diffs against".into(),
            Self::FocusLeft => "Move panel focus left (TUIs get the key first)".into(),
            Self::FocusRight => "Move panel focus right (TUIs get the key first)".into(),
            Self::NoOp | Self::ReceiveChar => String::new(),
        }
    }
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
        KeyBinding {
            key: Key::Home,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarTop),
        },
        KeyBinding {
            key: Key::End,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarBottom),
        },
        KeyBinding {
            key: Key::PageDown,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarNextProject),
        },
        KeyBinding {
            key: Key::PageUp,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarPreviousProject),
        },
        KeyBinding {
            key: Key::R,
            mods: Modifiers::NONE,
            action: BindingAction::Named(RefreshProjects),
        },
        KeyBinding {
            key: Key::Delete,
            mods: Modifiers::NONE,
            action: BindingAction::Named(DeleteSelected),
        },
        KeyBinding {
            key: Key::R,
            mods: Modifiers::SHIFT,
            action: BindingAction::Named(RenameSelected),
        },
        KeyBinding {
            key: Key::O,
            mods: Modifiers::NONE,
            action: BindingAction::Named(ToggleProjectExpanded),
        },
        KeyBinding { key: Key::K, mods: ctrl, action: BindingAction::Named(TogglePalette) },
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
    if n.starts_with("Numpad") {
        // egui-winit collapses numpad keys into their standard counterparts
        // (`KeyCode::NumpadEnter` → `egui::Key::Enter`, NumpadAdd → the plus
        // key, ...), so a numpad binding can't be told apart from the main
        // key.  Aliasing would silently fire it on the standard key — drop
        // the binding instead.
        log::warn!("ignoring {n} binding: egui cannot distinguish numpad keys");
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
        // Alacritty names keys after winit's `NamedKey` ("ArrowUp") and keeps
        // the pre-0.13 names ("Up") as legacy aliases — accept both.
        "ArrowUp" | "Up" => Key::ArrowUp,
        "ArrowDown" | "Down" => Key::ArrowDown,
        "ArrowLeft" | "Left" => Key::ArrowLeft,
        "ArrowRight" | "Right" => Key::ArrowRight,
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
        "Colon" => Key::Colon,
        // Legacy alacritty digit names: "Key1" is the top-row 1.
        n if n.len() == 4 && n.starts_with("Key") => {
            return char_to_key(n.chars().nth(3).unwrap());
        },
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
        '-' => Key::Minus,
        '=' => Key::Equals,
        '+' => Key::Plus,
        ',' => Key::Comma,
        '.' => Key::Period,
        '/' => Key::Slash,
        '\\' => Key::Backslash,
        ';' => Key::Semicolon,
        ':' => Key::Colon,
        '\'' => Key::Quote,
        '`' => Key::Backtick,
        '[' => Key::OpenBracket,
        ']' => Key::CloseBracket,
        _ => return None,
    })
}

/// Winit key names that egui doesn't model.  Default alacritty configs include
/// a handful of these, so swallow them silently rather than logging noise.
fn is_silent_unsupported_key(name: &str) -> bool {
    let n = name.trim();
    // `parse_key` already logs a dedicated message explaining why numpad
    // bindings are dropped; suppress the generic "unknown key" follow-up.
    n.starts_with("Numpad")
        || matches!(
            n,
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

pub fn parse_action(name: &str) -> BindingAction {
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
        "SelectNextSession" => BindingAction::Named(SelectNextSession),
        "SelectPreviousSession" => BindingAction::Named(SelectPreviousSession),
        "ToggleLeftSidebar" => BindingAction::Named(ToggleLeftSidebar),
        "ToggleRightSidebar" => BindingAction::Named(ToggleRightSidebar),
        "SelectNextWorkspace" => BindingAction::Named(SelectNextWorkspace),
        "SelectPreviousWorkspace" => BindingAction::Named(SelectPreviousWorkspace),
        "AddProject" => BindingAction::Named(AddProject),
        "ToggleSidebarFocus" => BindingAction::Named(ToggleSidebarFocus),
        "CloseSession" => BindingAction::Named(CloseSession),
        "SidebarTop" => BindingAction::Named(SidebarTop),
        "SidebarBottom" => BindingAction::Named(SidebarBottom),
        "SidebarNextProject" => BindingAction::Named(SidebarNextProject),
        "SidebarPreviousProject" => BindingAction::Named(SidebarPreviousProject),
        "RefreshProjects" => BindingAction::Named(RefreshProjects),
        "DeleteSelected" => BindingAction::Named(DeleteSelected),
        "RenameSelected" => BindingAction::Named(RenameSelected),
        "ToggleProjectExpanded" => BindingAction::Named(ToggleProjectExpanded),
        "TogglePalette" => BindingAction::Named(TogglePalette),
        "FocusProjectsSidebar" => BindingAction::Named(FocusProjectsSidebar),
        "FocusGitSidebar" => BindingAction::Named(FocusGitSidebar),
        "FocusTerminal" => BindingAction::Named(FocusTerminal),
        "ToggleSessionRows" => BindingAction::Named(ToggleSessionRows),
        "ToggleSessionTabs" => BindingAction::Named(ToggleSessionTabs),
        "SetBaseBranch" => BindingAction::Named(SetBaseBranch),
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
        "FocusLeft" => BindingAction::Named(FocusLeft),
        "FocusRight" => BindingAction::Named(FocusRight),
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

    /// Modern alacritty configs name keys after winit's `NamedKey`
    /// ("ArrowUp"); the legacy alacritty names ("Up") are aliases.  Both
    /// spellings must keep their bindings.
    #[test]
    fn winit_named_arrow_keys_parse() {
        for (name, key) in [
            ("ArrowUp", Key::ArrowUp),
            ("ArrowDown", Key::ArrowDown),
            ("ArrowLeft", Key::ArrowLeft),
            ("ArrowRight", Key::ArrowRight),
            ("Up", Key::ArrowUp),
        ] {
            let bindings = parse_bindings(vec![raw_chars(name, None, "x")]);
            let matched = all_matches(&bindings, key, Modifiers::NONE);
            assert!(
                matched.iter().any(|a| matches!(a, BindingAction::Chars(c) if c == b"x")),
                "{name} binding was dropped: {matched:?}"
            );
        }
    }

    /// Alacritty accepts any single character as a key name; the punctuation
    /// egui models must round-trip instead of being dropped as unknown.
    #[test]
    fn single_char_punctuation_parses() {
        for (name, key) in [
            ("-", Key::Minus),
            ("=", Key::Equals),
            ("+", Key::Plus),
            ("[", Key::OpenBracket),
            ("]", Key::CloseBracket),
            (";", Key::Semicolon),
            (":", Key::Colon),
            ("'", Key::Quote),
            ("`", Key::Backtick),
            (",", Key::Comma),
            (".", Key::Period),
            ("/", Key::Slash),
            ("\\", Key::Backslash),
        ] {
            let bindings = parse_bindings(vec![raw_chars(name, None, "x")]);
            let matched = all_matches(&bindings, key, Modifiers::NONE);
            assert!(
                matched.iter().any(|a| matches!(a, BindingAction::Chars(c) if c == b"x")),
                "{name:?} binding was dropped: {matched:?}"
            );
        }
    }

    /// Alacritty's legacy digit names ("Key1") and the "Colon" alias must keep
    /// their bindings like they do upstream.
    #[test]
    fn legacy_key_names_parse() {
        for (name, key) in
            [("Key0", Key::Num0), ("Key1", Key::Num1), ("Key9", Key::Num9), ("Colon", Key::Colon)]
        {
            let bindings = parse_bindings(vec![raw_chars(name, None, "x")]);
            let matched = all_matches(&bindings, key, Modifiers::NONE);
            assert!(
                matched.iter().any(|a| matches!(a, BindingAction::Chars(c) if c == b"x")),
                "{name} binding was dropped: {matched:?}"
            );
        }
    }

    /// egui collapses numpad keys into their standard counterparts, so numpad
    /// bindings are dropped — with the dedicated log message, not the generic
    /// unknown-key warning.
    #[test]
    fn numpad_bindings_drop_without_unknown_key_noise() {
        for name in ["NumpadEnter", "NumpadAdd", "Numpad1"] {
            let bindings = parse_bindings(vec![raw_chars(name, None, "x")]);
            assert!(
                !bindings.iter().any(|b| matches!(&b.action, BindingAction::Chars(c) if c == b"x")),
                "{name} must not produce a binding"
            );
            assert!(
                is_silent_unsupported_key(name),
                "{name} must be exempt from the generic unknown-key warning"
            );
        }
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
            ("ToggleSessionRows", NamedAction::ToggleSessionRows),
            ("ToggleSessionTabs", NamedAction::ToggleSessionTabs),
            ("FocusLeft", NamedAction::FocusLeft),
            ("FocusRight", NamedAction::FocusRight),
            ("SetBaseBranch", NamedAction::SetBaseBranch),
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

    #[test]
    fn select_session_actions_parse_from_config_names() {
        for (name, expected) in [
            ("SelectNextSession", NamedAction::SelectNextSession),
            ("SelectPreviousSession", NamedAction::SelectPreviousSession),
        ] {
            assert!(
                matches!(parse_action(name), BindingAction::Named(a) if a == expected),
                "{name} does not parse"
            );
        }
    }

    #[test]
    fn sidebar_nav_actions_have_unmodified_defaults_and_parse() {
        let b = parse_bindings(vec![]);
        for (key, expected, name) in [
            (Key::Home, NamedAction::SidebarTop, "SidebarTop"),
            (Key::End, NamedAction::SidebarBottom, "SidebarBottom"),
            (Key::PageDown, NamedAction::SidebarNextProject, "SidebarNextProject"),
            (Key::PageUp, NamedAction::SidebarPreviousProject, "SidebarPreviousProject"),
        ] {
            assert_eq!(named_matches(&b, key, Modifiers::NONE), vec![expected], "{name}");
            assert!(
                matches!(parse_action(name), BindingAction::Named(a) if a == expected),
                "{name} does not parse"
            );
        }
    }

    /// Only the sidebar cursor actions and RefreshProjects are focus-scoped:
    /// everything else (CloseSession included) must keep firing from the
    /// terminal.
    #[test]
    fn only_sidebar_actions_are_sidebar_scoped() {
        use NamedAction::*;
        for a in [
            SidebarTop,
            SidebarBottom,
            SidebarNextProject,
            SidebarPreviousProject,
            RefreshProjects,
            DeleteSelected,
            RenameSelected,
            ToggleProjectExpanded,
        ] {
            assert!(a.is_sidebar_scoped(), "{a:?}");
        }
        for a in [CloseSession, ScrollToTop, ScrollPageUp, ToggleSidebarFocus, Quit] {
            assert!(!a.is_sidebar_scoped(), "{a:?}");
        }
    }

    /// Delete is forward-delete inside a shell, so the default binding only
    /// works because the action is sidebar-scoped.
    #[test]
    fn delete_selected_has_an_unmodified_delete_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(
            named_matches(&b, Key::Delete, Modifiers::NONE),
            vec![NamedAction::DeleteSelected]
        );
        assert!(matches!(
            parse_action("DeleteSelected"),
            BindingAction::Named(NamedAction::DeleteSelected)
        ));
    }

    #[test]
    fn refresh_projects_has_an_unmodified_r_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::R, Modifiers::NONE), vec![NamedAction::RefreshProjects]);
        assert!(matches!(
            parse_action("RefreshProjects"),
            BindingAction::Named(NamedAction::RefreshProjects)
        ));
    }

    /// Shift+R types `R` inside a shell, so like Delete the default only
    /// works because the action is sidebar-scoped.
    #[test]
    fn rename_selected_has_a_shift_r_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::R, Modifiers::SHIFT), vec![NamedAction::RenameSelected]);
        assert_eq!(named_matches(&b, Key::R, Modifiers::NONE), vec![NamedAction::RefreshProjects]);
        assert!(matches!(
            parse_action("RenameSelected"),
            BindingAction::Named(NamedAction::RenameSelected)
        ));
    }

    /// `o` is terminal input, so the unmodified default only works because
    /// the action is sidebar-scoped.
    #[test]
    fn toggle_project_expanded_has_an_unmodified_o_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(
            named_matches(&b, Key::O, Modifiers::NONE),
            vec![NamedAction::ToggleProjectExpanded]
        );
        assert!(matches!(
            parse_action("ToggleProjectExpanded"),
            BindingAction::Named(NamedAction::ToggleProjectExpanded)
        ));
    }

    #[test]
    fn toggle_palette_is_a_default_ctrl_k_binding_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::K, Modifiers::CTRL), vec![NamedAction::TogglePalette]);
        assert!(matches!(
            parse_action("TogglePalette"),
            BindingAction::Named(NamedAction::TogglePalette)
        ));
        // The F1 shortcuts window is gone and the palette lists every action,
        // so the old `ShowShortcuts` name is no longer recognized.
        assert!(matches!(parse_action("ShowShortcuts"), BindingAction::Unsupported(_)));
    }
}
