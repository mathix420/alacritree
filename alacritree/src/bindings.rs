//! Parse `[[keyboard.bindings]]` from alacritty's config and match them
//! against key presses. Keys and modifiers are this module's own types so the
//! parser names no egui type. `shortcut` converts each binding's key to egui's
//! once, when the app is built, and converts only a press's modifiers. Running
//! a `NamedAction` goes through the app's `Action` trait, which does take egui.

pub(crate) mod action;

use enum_dispatch::enum_dispatch;
use schemars::JsonSchema;
use serde::Deserialize;
use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};

use crate::app::Action;

/// A key a binding can name. Variant names follow egui's, and `shortcut` maps
/// each one to the egui key of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, EnumIter)]
pub enum Key {
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    Escape,
    Tab,
    Backspace,
    Enter,
    Space,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Colon,
    Comma,
    Backslash,
    Slash,
    OpenBracket,
    CloseBracket,
    Backtick,
    Minus,
    Period,
    Plus,
    Equals,
    Semicolon,
    Quote,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
}

/// The modifiers a binding requires, or that a key press carried.  `command`
/// is Super on macOS; off macOS the input side raises it with every Ctrl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct Modifiers {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub command: bool,
}

impl Modifiers {
    pub const ALT: Self = Self { alt: true, ..Self::NONE };
    #[cfg(target_os = "macos")]
    pub const COMMAND: Self = Self { command: true, ..Self::NONE };
    pub const CTRL: Self = Self { ctrl: true, ..Self::NONE };
    pub const NONE: Self = Self { alt: false, ctrl: false, shift: false, command: false };
    pub const SHIFT: Self = Self { shift: true, ..Self::NONE };

    /// Alacritty semantics: `Control|Shift` does not fire on Ctrl alone, so alt
    /// and shift match exactly.  Ctrl and command are only required when the
    /// binding names them, because one Ctrl press can carry both; a binding
    /// naming neither refuses either.
    pub fn fires(self, required: Self) -> bool {
        if self.alt != required.alt || self.shift != required.shift {
            return false;
        }
        if !required.ctrl && !required.command {
            return !self.ctrl && !self.command;
        }
        (self.ctrl || !required.ctrl) && (self.command || !required.command)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self {
            alt: self.alt || rhs.alt,
            ctrl: self.ctrl || rhs.ctrl,
            shift: self.shift || rhs.shift,
            command: self.command || rhs.command,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyBinding {
    pub key: Key,
    pub mods: Modifiers,
    pub action: BindingAction,
}

#[derive(Debug, Clone, serde::Serialize)]
pub enum BindingAction {
    Chars(#[serde(serialize_with = "chars_as_string")] Vec<u8>),
    Named(NamedAction),
    Unsupported(String),
}

/// The bytes a binding writes are an escape sequence, which reads as an
/// escaped string rather than as the array of integers a byte vector
/// serialises to by default.
fn chars_as_string<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&String::from_utf8_lossy(bytes).escape_default().to_string())
}

/// A `Copy` stand-in for an action's identity, for logs that outlive the
/// action itself.  The payloads are dropped: they are the user's keystrokes
/// and config text, and naming the action is what a timing log needs.
#[derive(Clone, Copy)]
pub enum ActionLabel {
    Chars,
    Named(NamedAction),
    Unsupported,
}

impl std::fmt::Debug for ActionLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Chars => f.write_str("Chars"),
            Self::Named(action) => write!(f, "{action:?}"),
            Self::Unsupported => f.write_str("Unsupported"),
        }
    }
}

impl BindingAction {
    pub fn label(&self) -> ActionLabel {
        match self {
            Self::Chars(_) => ActionLabel::Chars,
            Self::Named(n) => ActionLabel::Named(*n),
            Self::Unsupported(_) => ActionLabel::Unsupported,
        }
    }
}

/// An app-level action a binding, the palette or IPC can run. Each variant
/// wraps its own type, so the compiler refuses a new action until it has an
/// `Action` impl saying what it does.
/// Variant order is the command palette's row order.
#[enum_dispatch(Action)]
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, EnumIter, EnumString, IntoStaticStr)]
#[serde(into = "String")]
pub enum NamedAction {
    Paste(action::Paste),
    PasteSelection(action::PasteSelection),
    Copy(action::Copy),
    CopySelection(action::CopySelection),
    ScrollPageUp(action::ScrollPageUp),
    ScrollPageDown(action::ScrollPageDown),
    ScrollHalfPageUp(action::ScrollHalfPageUp),
    ScrollHalfPageDown(action::ScrollHalfPageDown),
    ScrollLineUp(action::ScrollLineUp),
    ScrollLineDown(action::ScrollLineDown),
    ScrollToTop(action::ScrollToTop),
    ScrollToBottom(action::ScrollToBottom),
    ClearHistory(action::ClearHistory),
    #[strum(
        to_string = "SpawnNewInstance",
        serialize = "CreateNewWindow",
        serialize = "CreateNewTab"
    )]
    SpawnNewInstance(action::SpawnNewInstance),
    IncreaseFontSize(action::IncreaseFontSize),
    DecreaseFontSize(action::DecreaseFontSize),
    ResetFontSize(action::ResetFontSize),
    ToggleFullscreen(action::ToggleFullscreen),
    ToggleMaximized(action::ToggleMaximized),
    Minimize(action::Minimize),
    SelectNextTab(action::SelectNextTab),
    SelectPreviousTab(action::SelectPreviousTab),
    #[strum(disabled)]
    SelectTab(action::SelectTab),
    SelectLastTab(action::SelectLastTab),
    /// Like SelectNextTab/SelectPreviousTab, but one flat ring over every
    /// open session: crossing a workspace boundary switches workspaces.
    SelectNextSession(action::SelectNextSession),
    SelectPreviousSession(action::SelectPreviousSession),
    SelectNextWorkspace(action::SelectNextWorkspace),
    SelectPreviousWorkspace(action::SelectPreviousWorkspace),
    /// Open/select the current workspace's scratchpad, or close it when active.
    OpenScratchpad(action::OpenScratchpad),
    /// Open/select the current workspace's task list, or close it when active.
    OpenTasks(action::OpenTasks),
    ToggleLeftSidebar(action::ToggleLeftSidebar),
    ToggleRightSidebar(action::ToggleRightSidebar),
    AddProject(action::AddProject),
    ToggleSidebarFocus(action::ToggleSidebarFocus),
    CloseSession(action::CloseSession),
    /// Close the session on screen once its child has exited.  Its default
    /// trigger is a bare `Enter`, which only a scope this narrow can afford.
    CloseExitedSession(action::CloseExitedSession),
    /// Open a new pane in the multiplexer and a session on it, in the focused
    /// workspace and on the side the focused session's own pane belongs to.
    NewMultiplexerPane(action::NewMultiplexerPane),
    /// Open a session on every multiplexer pane the sidebar lists that none
    /// is attached to yet, leaving the panes that already hold one alone.
    AttachAllMultiplexerPanes(action::AttachAllMultiplexerPanes),
    /// End every session attached to a multiplexer pane. The panes keep
    /// running under the multiplexer and their rows come back unattached.
    DetachAllMultiplexerPanes(action::DetachAllMultiplexerPanes),
    SidebarTop(action::SidebarTop),
    SidebarBottom(action::SidebarBottom),
    SidebarNextProject(action::SidebarNextProject),
    SidebarPreviousProject(action::SidebarPreviousProject),
    FocusProjectsSidebar(action::FocusProjectsSidebar),
    FocusGitSidebar(action::FocusGitSidebar),
    FocusTerminal(action::FocusTerminal),
    /// Directional panel focus with TUI passthrough (see `focus_move` in
    /// `app.rs`).
    FocusLeft(action::FocusLeft),
    FocusRight(action::FocusRight),
    /// Flip the runtime `session_display.sidebar_always` value.
    ToggleSessionRows(action::ToggleSessionRows),
    /// Flip the runtime `session_display.tabs_always` value.
    ToggleSessionTabs(action::ToggleSessionTabs),
    /// Flip whether session rows can be dragged with the mouse.
    ToggleSessionDrag(action::ToggleSessionDrag),
    /// Move a session one position earlier in the sidebar and tab strip,
    /// continuing into the previous workspace when `[ui.session_reorder]
    /// scope` allows it.
    MoveSessionUp(action::MoveSessionUp),
    /// Move a session one position later, continuing into the next workspace
    /// when the scope allows it.
    MoveSessionDown(action::MoveSessionDown),
    /// Open the base-branch picker for the sidebar-cursored or current worktree.
    SetBaseBranch(action::SetBaseBranch),
    #[strum(disabled)]
    SpawnProfile(action::SpawnProfile),
    /// Confirm the focused sidebar's fuzzy search: activate the highlighted row
    /// and scroll it into view. Only fires while that panel is in search mode.
    SidebarSearchConfirm(action::SidebarSearchConfirm),
    /// Cancel the focused sidebar's fuzzy search, staying in the sidebar with the
    /// cursor on the previously active row.
    SidebarSearchCancel(action::SidebarSearchCancel),
    /// Cancel the focused sidebar's fuzzy search and return focus to the terminal.
    SidebarSearchCancelToTerminal(action::SidebarSearchCancelToTerminal),
    Quit(action::Quit),
    /// Open (or close) the Ctrl+K command palette.
    TogglePalette(action::TogglePalette),
    /// Move the palette cursor to the first row.
    PaletteTop(action::PaletteTop),
    /// Move the palette cursor to the last row.
    PaletteBottom(action::PaletteBottom),
    /// Move the palette cursor a screenful up.
    PalettePageUp(action::PalettePageUp),
    /// Move the palette cursor a screenful down.
    PalettePageDown(action::PalettePageDown),
    /// Used to unbind a key — consumes the press without acting on it.
    #[strum(serialize = "None")]
    NoOp(action::NoOp),
    /// Alacritty's pass-through marker: the matching binding runs (no-op for
    /// us) but suppress_chars stays off so the key still reaches the PTY.
    /// Mirrors `Action::ReceiveChar` in `alacritty/src/input/keyboard.rs`.
    ReceiveChar(action::ReceiveChar),
    /// Narrow the projects sidebar to workspaces with a live session.
    ToggleSessionsFilter(action::ToggleSessionsFilter),
    /// Flip whether `ToggleSessionsFilter` also counts a listed detached
    /// herdr agent as occupying a workspace.  Runtime-only, like
    /// `ToggleSessionRows`: it flips a preference rather than a panel toggle,
    /// so it is not scoped to the sidebar owning focus.
    ToggleDetachedSessionsFilter(action::ToggleDetachedSessionsFilter),
    /// Narrow the projects sidebar to workspaces whose session wants attention.
    ToggleAttentionFilter(action::ToggleAttentionFilter),
    /// PR-state filters.  One dimension: the active states union, and the
    /// result ANDs with the session and attention filters.
    TogglePrOpenFilter(action::TogglePrOpenFilter),
    TogglePrDraftFilter(action::TogglePrDraftFilter),
    TogglePrMergedFilter(action::TogglePrMergedFilter),
    TogglePrClosedFilter(action::TogglePrClosedFilter),
    /// Drop every projects-sidebar toggle.  Reachable without knowing which are
    /// set, which `Esc` cannot offer a caller that has no view of the state.
    ClearProjectFilters(action::ClearProjectFilters),
    /// Git sidebar change-kind filters.  The active kinds union.
    ToggleModifiedFilter(action::ToggleModifiedFilter),
    ToggleDeletedFilter(action::ToggleDeletedFilter),
    ToggleUntrackedFilter(action::ToggleUntrackedFilter),
    ClearGitFilters(action::ClearGitFilters),
    /// Switch between a query confined by the active toggles and one evaluated
    /// against every row.  Session-only; restarting returns to `[ui] search_scope`.
    ToggleSearchScope(action::ToggleSearchScope),
    /// Re-query `gh` for every cached worktree.
    RefreshPrStatus(action::RefreshPrStatus),
    /// Open the git panel's Staged section in the diff viewer, or close the
    /// pane when that section is already open.
    ReviewStaged(action::ReviewStaged),
    /// The same for the Unstaged section.
    ReviewUnstaged(action::ReviewUnstaged),
    /// The same for the `Changes vs` section.
    ReviewBranch(action::ReviewBranch),
    /// Re-run worktree discovery for every project in the sidebar.
    RefreshProjects(action::RefreshProjects),
    /// Act on the sidebar-cursored row: a session closes, a worktree gets
    /// the delete/prune dialog, a project gets the remove-from-sidebar
    /// prompt.
    DeleteSelected(action::DeleteSelected),
    /// Rename the sidebar-cursored row: a session gets a custom display
    /// name, a project gets the same rename dialog as its context menu.
    RenameSelected(action::RenameSelected),
    /// Expand or collapse the sidebar-cursored project, showing or hiding
    /// its worktrees and sessions.
    ToggleProjectExpanded(action::ToggleProjectExpanded),
}

impl NamedAction {
    /// Actions that only make sense while the projects sidebar owns focus.
    /// Their default keys (unmodified Home/End/PageUp/PageDown/R/O/Delete and
    /// Shift+R) are terminal input the rest of the time, so dispatch must
    /// not consume them unless the sidebar owns focus.
    pub fn is_sidebar_scoped(&self) -> bool {
        matches!(
            self,
            Self::SidebarTop(_)
                | Self::SidebarBottom(_)
                | Self::SidebarNextProject(_)
                | Self::SidebarPreviousProject(_)
                | Self::RefreshProjects(_)
                | Self::DeleteSelected(_)
                | Self::RenameSelected(_)
                | Self::ToggleProjectExpanded(_)
                | Self::RefreshPrStatus(_)
        )
    }

    /// Sidebar-cursor actions whose meaning depends on the project panel
    /// browsing rather than searching.  Narrower than `is_sidebar_scoped`: the
    /// four cursor *moves* stay valid mid-query, because navigating filtered
    /// results is the point of filtering.
    pub fn requires_project_browsing(&self) -> bool {
        matches!(
            self,
            Self::RefreshProjects(_)
                | Self::DeleteSelected(_)
                | Self::RenameSelected(_)
                | Self::ToggleProjectExpanded(_)
        )
    }

    /// Valid only while the projects sidebar owns focus: the default triggers
    /// are bare letters that belong to the PTY anywhere else.  No mode
    /// component is needed — a letter typed into the search box never reaches
    /// the binding table, because its text is swallowed first.
    pub fn is_projects_filter_scoped(&self) -> bool {
        matches!(
            self,
            Self::ToggleSessionsFilter(_)
                | Self::ToggleDetachedSessionsFilter(_)
                | Self::ToggleAttentionFilter(_)
                | Self::TogglePrOpenFilter(_)
                | Self::TogglePrDraftFilter(_)
                | Self::TogglePrMergedFilter(_)
                | Self::TogglePrClosedFilter(_)
                | Self::ClearProjectFilters(_)
        )
    }

    /// Valid only while the session on screen has exited.  The default trigger
    /// is a bare `Enter`, which belongs to the PTY in every live session, so
    /// dispatch must leave the press alone until the child that would have
    /// read it is gone.
    pub fn is_exited_session_scoped(&self) -> bool {
        matches!(self, Self::CloseExitedSession(_))
    }

    /// The git sidebar's equivalent.
    pub fn is_git_filter_scoped(&self) -> bool {
        matches!(
            self,
            Self::ToggleModifiedFilter(_)
                | Self::ToggleDeletedFilter(_)
                | Self::ToggleUntrackedFilter(_)
                | Self::ClearGitFilters(_)
        )
    }

    /// Actions whose keys should remain native text-editing input while the
    /// scratchpad owns focus. For example, Shift+Home selects to the start of
    /// a line in the editor instead of trying to scroll a terminal grid.
    pub fn is_terminal_only(&self) -> bool {
        matches!(
            self,
            Self::ScrollPageUp(_)
                | Self::ScrollPageDown(_)
                | Self::ScrollHalfPageUp(_)
                | Self::ScrollHalfPageDown(_)
                | Self::ScrollLineUp(_)
                | Self::ScrollLineDown(_)
                | Self::ScrollToTop(_)
                | Self::ScrollToBottom(_)
                | Self::ClearHistory(_)
        )
    }

    /// Actions that only act while the *focused* sidebar panel is in fuzzy-search
    /// mode. Their default keys (`Enter`, `Esc`, `Shift+Esc`) are terminal input
    /// otherwise, so dispatch is owned by the sidebar nav pass and suppressed in
    /// `handle_shortcuts`, keeping the terminal's own keys untouched.
    pub fn is_search_scoped(&self) -> bool {
        matches!(
            self,
            Self::SidebarSearchConfirm(_)
                | Self::SidebarSearchCancel(_)
                | Self::SidebarSearchCancelToTerminal(_)
        )
    }

    /// Actions that only act while the command palette is open. It is a modal
    /// that owns every key while it is up, so these are dispatched there and
    /// nowhere else — their default keys (unmodified Home/End/PageUp/PageDown)
    /// stay the sidebar's and the terminal's the rest of the time.
    pub fn is_palette_scoped(&self) -> bool {
        matches!(
            self,
            Self::PaletteTop(_)
                | Self::PaletteBottom(_)
                | Self::PalettePageUp(_)
                | Self::PalettePageDown(_)
        )
    }

    /// The name `parse_action` accepts for this action — what a user writes
    /// in `[[keyboard.bindings]]`, and the label the shortcuts window shows.
    pub fn config_name(&self) -> String {
        match self {
            Self::SelectTab(action::SelectTab(n)) => format!("SelectTab{n}"),
            Self::SpawnProfile(action::SpawnProfile(n)) => format!("SpawnProfile{n}"),
            other => <&'static str>::from(other).to_string(),
        }
    }

    /// One-line human description for the shortcuts window.
    pub fn description(&self) -> String {
        match self {
            Self::Paste(_) => "Paste from the clipboard".into(),
            Self::PasteSelection(_) => "Paste from the primary (X11) selection".into(),
            Self::Copy(_) => "Copy the selection to the clipboard".into(),
            Self::CopySelection(_) => "Copy the selection to the primary selection".into(),
            Self::ScrollPageUp(_) => "Scroll the scrollback one page up".into(),
            Self::ScrollPageDown(_) => "Scroll the scrollback one page down".into(),
            Self::ScrollHalfPageUp(_) => "Scroll the scrollback half a page up".into(),
            Self::ScrollHalfPageDown(_) => "Scroll the scrollback half a page down".into(),
            Self::ScrollLineUp(_) => "Scroll the scrollback one line up".into(),
            Self::ScrollLineDown(_) => "Scroll the scrollback one line down".into(),
            Self::ScrollToTop(_) => "Scroll to the top of the scrollback".into(),
            Self::ScrollToBottom(_) => "Scroll to the bottom of the scrollback".into(),
            Self::ClearHistory(_) => "Clear the scrollback buffer".into(),
            Self::SpawnNewInstance(_) => "Open a new shell session in the current workspace".into(),
            Self::IncreaseFontSize(_) => "Increase the font size".into(),
            Self::DecreaseFontSize(_) => "Decrease the font size".into(),
            Self::ResetFontSize(_) => "Reset the font size".into(),
            Self::ToggleFullscreen(_) => "Toggle fullscreen".into(),
            Self::ToggleMaximized(_) => "Toggle the maximized window state".into(),
            Self::Minimize(_) => "Minimize the window".into(),
            Self::SelectNextTab(_) => "Cycle to the next session in the workspace".into(),
            Self::SelectPreviousTab(_) => "Cycle to the previous session in the workspace".into(),
            Self::SelectTab(action::SelectTab(n)) => {
                format!("Select session {n} in the current workspace")
            },
            Self::SelectLastTab(_) => "Select the last session in the current workspace".into(),
            Self::SelectNextSession(_) => {
                "Cycle to the next session, continuing across workspaces".into()
            },
            Self::SelectPreviousSession(_) => {
                "Cycle to the previous session, continuing across workspaces".into()
            },
            Self::ToggleLeftSidebar(_) => "Toggle the projects sidebar".into(),
            Self::ToggleRightSidebar(_) => "Toggle the git sidebar".into(),
            Self::SelectNextWorkspace(_) => "Switch to the next workspace".into(),
            Self::SelectPreviousWorkspace(_) => "Switch to the previous workspace".into(),
            Self::OpenScratchpad(_) => "Toggle the workspace scratchpad tab".into(),
            Self::OpenTasks(_) => "Toggle the workspace tasks tab".into(),
            Self::AddProject(_) => "Add a project to the sidebar".into(),
            Self::ToggleSidebarFocus(_) => {
                "Toggle keyboard focus between terminal and sidebar".into()
            },
            Self::CloseSession(_) => "Close the cursored or active session".into(),
            Self::CloseExitedSession(_) => {
                "Close the session on screen once its child exited".into()
            },
            Self::NewMultiplexerPane(_) => "Open a new multiplexer pane and a session on it".into(),
            Self::AttachAllMultiplexerPanes(_) => {
                "Attach every unattached multiplexer pane the sidebar lists".into()
            },
            Self::DetachAllMultiplexerPanes(_) => "Detach every attached multiplexer pane".into(),
            Self::SidebarTop(_) => "Move the sidebar cursor to the first row".into(),
            Self::SidebarBottom(_) => "Move the sidebar cursor to the last row".into(),
            Self::SidebarNextProject(_) => "Jump the sidebar cursor to the next project".into(),
            Self::SidebarPreviousProject(_) => {
                "Jump the sidebar cursor to the previous project".into()
            },
            Self::RefreshProjects(_) => "Rescan every project's worktrees".into(),
            Self::DeleteSelected(_) => "Close the selected session, delete the selected worktree, \
                                        or remove the selected project"
                .into(),
            Self::RenameSelected(_) => "Rename the selected project".into(),
            Self::ToggleProjectExpanded(_) => "Expand or collapse the selected project".into(),
            Self::FocusProjectsSidebar(_) => "Focus the projects sidebar".into(),
            Self::FocusGitSidebar(_) => "Focus the git sidebar".into(),
            Self::FocusTerminal(_) => "Focus the terminal".into(),
            Self::SpawnProfile(action::SpawnProfile(n)) => {
                format!("Open a session with shell profile {n}")
            },
            Self::Quit(_) => "Open the quit confirmation dialog".into(),
            Self::TogglePalette(_) => "Open the command palette".into(),
            Self::PaletteTop(_) => "Move the palette cursor to the first row".into(),
            Self::PaletteBottom(_) => "Move the palette cursor to the last row".into(),
            Self::PalettePageUp(_) => "Move the palette cursor a screenful up".into(),
            Self::PalettePageDown(_) => "Move the palette cursor a screenful down".into(),
            Self::ToggleSessionRows(_) => "Toggle single-session sidebar rows".into(),
            Self::ToggleSessionTabs(_) => "Toggle single-session tab segments".into(),
            Self::ToggleSessionDrag(_) => "Toggle dragging session rows to reorder".into(),
            Self::MoveSessionUp(_) => "Move the session one position up".into(),
            Self::MoveSessionDown(_) => "Move the session one position down".into(),
            Self::SetBaseBranch(_) => "Choose the branch the git panel diffs against".into(),
            Self::FocusLeft(_) => "Move panel focus left (TUIs get the key first)".into(),
            Self::FocusRight(_) => "Move panel focus right (TUIs get the key first)".into(),
            Self::SidebarSearchConfirm(_) => {
                "Leave the sidebar search with the highlighted row selected".into()
            },
            Self::SidebarSearchCancel(_) => {
                "Cancel the sidebar search, staying in the sidebar".into()
            },
            Self::SidebarSearchCancelToTerminal(_) => {
                "Cancel the sidebar search and focus the terminal".into()
            },
            Self::ToggleSessionsFilter(_) => {
                "Filter the sidebar to workspaces with a session".into()
            },
            Self::ToggleDetachedSessionsFilter(_) => {
                "Toggle whether the sessions filter counts detached herdr agents".into()
            },
            Self::ToggleAttentionFilter(_) => {
                "Filter the sidebar to workspaces wanting attention".into()
            },
            Self::TogglePrOpenFilter(_) => {
                "Filter to worktrees with an open PR (requires [integrations.gh] pr_status)".into()
            },
            Self::TogglePrDraftFilter(_) => {
                "Filter to worktrees with a draft PR (requires [integrations.gh] pr_status)".into()
            },
            Self::TogglePrMergedFilter(_) => {
                "Filter to worktrees with a merged PR (requires [integrations.gh] pr_status)".into()
            },
            Self::TogglePrClosedFilter(_) => {
                "Filter to worktrees with a closed PR (requires [integrations.gh] pr_status)".into()
            },
            Self::ClearProjectFilters(_) => "Clear every projects-sidebar toggle".into(),
            Self::ToggleModifiedFilter(_) => {
                "Filter git changes to modified and renamed files".into()
            },
            Self::ToggleDeletedFilter(_) => "Filter git changes to deleted files".into(),
            Self::ToggleUntrackedFilter(_) => {
                "Filter git changes to untracked and added files".into()
            },
            Self::ClearGitFilters(_) => "Clear every git-sidebar toggle".into(),
            Self::ToggleSearchScope(_) => {
                "Search inside the active filters or across every row".into()
            },
            Self::RefreshPrStatus(_) => "Re-query GitHub for every worktree's PR".into(),
            Self::ReviewStaged(_) => "Review every staged change in the diff viewer".into(),
            Self::ReviewUnstaged(_) => "Review every unstaged change in the diff viewer".into(),
            Self::ReviewBranch(_) => "Review the branch against its base in the diff viewer".into(),
            Self::NoOp(_) | Self::ReceiveChar(_) => String::new(),
        }
    }
}

impl std::fmt::Debug for NamedAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.config_name())
    }
}

impl From<NamedAction> for String {
    fn from(action: NamedAction) -> Self {
        action.config_name()
    }
}

/// One `[[keyboard.bindings]]` entry.  A binding needs `key`, plus exactly one
/// of `chars`, `action` or `command` to say what pressing it does.
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct RawBinding {
    /// The key, as alacritty spells it: a character (`"A"`), a named key
    /// (`"F5"`, `"PageUp"`), or a scancode.  A key alacritree cannot map is
    /// dropped with a warning.
    pub key: String,
    /// Modifiers held with the key, joined by `|`: `"Control"`, `"Shift"`,
    /// `"Alt"`, `"Super"`.  Unset means no modifiers.
    #[serde(default)]
    pub mods: Option<String>,
    /// Terminal mode the binding applies in, e.g. `"Vi"` or `"~Search"`.
    /// alacritree tracks neither mode, so a binding with a `mode` is read and
    /// ignored.
    #[serde(default)]
    pub mode: Option<String>,
    /// Bytes to write to the PTY, with the usual escapes (`\x1b`, `\u001b`).
    #[serde(default)]
    pub chars: Option<String>,
    /// Named action to run, e.g. `"Paste"`, `"ToggleLeftSidebar"`.  The schema
    /// suggests every name alacritree implements without rejecting the rest:
    /// the shared `alacritty.toml` legitimately carries actions only the real
    /// alacritty implements, and alacritree ignores those rather than
    /// rejecting them.  `docs/keyboard-shortcuts.md` says what each one does.
    #[serde(default)]
    #[schemars(schema_with = "action_schema")]
    pub action: Option<String>,
    /// External program to run.  alacritree parses it so the binding still
    /// displaces alacritty's default for that key, but never runs it.
    #[serde(default)]
    #[schemars(with = "Option<BindingCommand>")]
    pub command: Option<toml::Value>,
}

// Exists only to describe `RawBinding::command`, which is read as an opaque
// `toml::Value` because alacritree needs no more than the fact that the key
// was claimed.  Never constructed.
/// An external program to run, as either a bare path or a table with
/// arguments.
#[derive(JsonSchema)]
#[schemars(untagged, rename = "BindingCommand")]
#[allow(dead_code)]
enum BindingCommand {
    /// Just the program.
    Program(String),
    /// Program and its arguments.
    Detailed {
        /// Path to the program.
        program: String,
        /// Arguments passed to the program.  Optional.
        #[schemars(default)]
        args: Vec<String>,
    },
}

pub(crate) fn parse_bindings(raw: Vec<RawBinding>) -> Vec<KeyBinding> {
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
        KeyBinding {
            key: Key::V,
            mods: ctrl_shift,
            action: BindingAction::Named(Paste(action::Paste)),
        },
        KeyBinding {
            key: Key::C,
            mods: ctrl_shift,
            action: BindingAction::Named(Copy(action::Copy)),
        },
        KeyBinding {
            key: Key::Insert,
            mods: shift,
            action: BindingAction::Named(PasteSelection(action::PasteSelection)),
        },
        KeyBinding {
            key: Key::Num0,
            mods: ctrl,
            action: BindingAction::Named(ResetFontSize(action::ResetFontSize)),
        },
        KeyBinding {
            key: Key::Equals,
            mods: ctrl,
            action: BindingAction::Named(IncreaseFontSize(action::IncreaseFontSize)),
        },
        KeyBinding {
            key: Key::Plus,
            mods: ctrl,
            action: BindingAction::Named(IncreaseFontSize(action::IncreaseFontSize)),
        },
        KeyBinding {
            key: Key::Minus,
            mods: ctrl,
            action: BindingAction::Named(DecreaseFontSize(action::DecreaseFontSize)),
        },
        KeyBinding {
            key: Key::Home,
            mods: shift,
            action: BindingAction::Named(ScrollToTop(action::ScrollToTop)),
        },
        KeyBinding {
            key: Key::End,
            mods: shift,
            action: BindingAction::Named(ScrollToBottom(action::ScrollToBottom)),
        },
        KeyBinding {
            key: Key::PageUp,
            mods: shift,
            action: BindingAction::Named(ScrollPageUp(action::ScrollPageUp)),
        },
        KeyBinding {
            key: Key::PageDown,
            mods: shift,
            action: BindingAction::Named(ScrollPageDown(action::ScrollPageDown)),
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
        KeyBinding {
            key: Key::B,
            mods: ctrl,
            action: BindingAction::Named(ToggleLeftSidebar(action::ToggleLeftSidebar)),
        },
        KeyBinding {
            key: Key::G,
            mods: ctrl,
            action: BindingAction::Named(ToggleRightSidebar(action::ToggleRightSidebar)),
        },
        KeyBinding {
            key: Key::Backtick,
            mods: ctrl,
            action: BindingAction::Named(OpenScratchpad(action::OpenScratchpad)),
        },
        KeyBinding {
            key: Key::Backtick,
            mods: ctrl_shift,
            action: BindingAction::Named(OpenTasks(action::OpenTasks)),
        },
        KeyBinding {
            key: Key::Tab,
            mods: ctrl,
            action: BindingAction::Named(SelectNextTab(action::SelectNextTab)),
        },
        KeyBinding {
            key: Key::Tab,
            mods: ctrl_shift,
            action: BindingAction::Named(SelectPreviousTab(action::SelectPreviousTab)),
        },
        KeyBinding {
            key: Key::ArrowRight,
            mods: alt,
            action: BindingAction::Named(SelectNextWorkspace(action::SelectNextWorkspace)),
        },
        KeyBinding {
            key: Key::ArrowLeft,
            mods: alt,
            action: BindingAction::Named(SelectPreviousWorkspace(action::SelectPreviousWorkspace)),
        },
        KeyBinding {
            key: Key::O,
            mods: ctrl_shift,
            action: BindingAction::Named(AddProject(action::AddProject)),
        },
        KeyBinding {
            key: Key::B,
            mods: ctrl_shift,
            action: BindingAction::Named(ToggleSidebarFocus(action::ToggleSidebarFocus)),
        },
        KeyBinding {
            key: Key::Home,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarTop(action::SidebarTop)),
        },
        KeyBinding {
            key: Key::End,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarBottom(action::SidebarBottom)),
        },
        KeyBinding {
            key: Key::PageDown,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarNextProject(action::SidebarNextProject)),
        },
        KeyBinding {
            key: Key::PageUp,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarPreviousProject(action::SidebarPreviousProject)),
        },
        KeyBinding {
            key: Key::R,
            mods: Modifiers::NONE,
            action: BindingAction::Named(RefreshProjects(action::RefreshProjects)),
        },
        KeyBinding {
            key: Key::Delete,
            mods: Modifiers::NONE,
            action: BindingAction::Named(DeleteSelected(action::DeleteSelected)),
        },
        KeyBinding {
            key: Key::R,
            mods: Modifiers::SHIFT,
            action: BindingAction::Named(RenameSelected(action::RenameSelected)),
        },
        KeyBinding {
            key: Key::O,
            mods: Modifiers::NONE,
            action: BindingAction::Named(ToggleProjectExpanded(action::ToggleProjectExpanded)),
        },
        KeyBinding {
            key: Key::S,
            mods: Modifiers::NONE,
            action: BindingAction::Named(ToggleSessionsFilter(action::ToggleSessionsFilter)),
        },
        KeyBinding {
            key: Key::A,
            mods: Modifiers::NONE,
            action: BindingAction::Named(ToggleAttentionFilter(action::ToggleAttentionFilter)),
        },
        KeyBinding {
            key: Key::M,
            mods: Modifiers::NONE,
            action: BindingAction::Named(ToggleModifiedFilter(action::ToggleModifiedFilter)),
        },
        KeyBinding {
            key: Key::D,
            mods: Modifiers::NONE,
            action: BindingAction::Named(ToggleDeletedFilter(action::ToggleDeletedFilter)),
        },
        KeyBinding {
            key: Key::U,
            mods: Modifiers::NONE,
            action: BindingAction::Named(ToggleUntrackedFilter(action::ToggleUntrackedFilter)),
        },
        KeyBinding {
            key: Key::K,
            mods: ctrl,
            action: BindingAction::Named(TogglePalette(action::TogglePalette)),
        },
        KeyBinding {
            key: Key::G,
            mods: ctrl_shift,
            action: BindingAction::Named(FocusGitSidebar(action::FocusGitSidebar)),
        },
        KeyBinding {
            key: Key::W,
            mods: ctrl_shift,
            action: BindingAction::Named(CloseSession(action::CloseSession)),
        },
        KeyBinding {
            key: Key::T,
            mods: ctrl,
            action: BindingAction::Named(SpawnNewInstance(action::SpawnNewInstance)),
        },
        KeyBinding { key: Key::Q, mods: ctrl, action: BindingAction::Named(Quit(action::Quit)) },
        // Sidebar fuzzy-search: gated to the focused searching panel, so these
        // pass straight through to the PTY whenever the terminal owns focus.
        KeyBinding {
            key: Key::Enter,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarSearchConfirm(action::SidebarSearchConfirm)),
        },
        // Shares that Enter, and the same reason it is safe: a held session has
        // no child left to read the key.
        KeyBinding {
            key: Key::Enter,
            mods: Modifiers::NONE,
            action: BindingAction::Named(CloseExitedSession(action::CloseExitedSession)),
        },
        KeyBinding {
            key: Key::Escape,
            mods: Modifiers::NONE,
            action: BindingAction::Named(SidebarSearchCancel(action::SidebarSearchCancel)),
        },
        KeyBinding {
            key: Key::Escape,
            mods: shift,
            action: BindingAction::Named(SidebarSearchCancelToTerminal(
                action::SidebarSearchCancelToTerminal,
            )),
        },
        // Palette list navigation: the same unmodified keys the sidebar uses,
        // claimed only while the palette modal is up.
        KeyBinding {
            key: Key::Home,
            mods: Modifiers::NONE,
            action: BindingAction::Named(PaletteTop(action::PaletteTop)),
        },
        KeyBinding {
            key: Key::End,
            mods: Modifiers::NONE,
            action: BindingAction::Named(PaletteBottom(action::PaletteBottom)),
        },
        KeyBinding {
            key: Key::PageUp,
            mods: Modifiers::NONE,
            action: BindingAction::Named(PalettePageUp(action::PalettePageUp)),
        },
        KeyBinding {
            key: Key::PageDown,
            mods: Modifiers::NONE,
            action: BindingAction::Named(PalettePageDown(action::PalettePageDown)),
        },
    ]);

    // macOS uses Cmd instead of Ctrl+Shift for clipboard / window actions.
    #[cfg(target_os = "macos")]
    {
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let cmd_ctrl = Modifiers::COMMAND | Modifiers::CTRL;
        b.extend([
            KeyBinding {
                key: Key::V,
                mods: cmd,
                action: BindingAction::Named(Paste(action::Paste)),
            },
            KeyBinding { key: Key::C, mods: cmd, action: BindingAction::Named(Copy(action::Copy)) },
            KeyBinding {
                key: Key::N,
                mods: cmd,
                action: BindingAction::Named(SpawnNewInstance(action::SpawnNewInstance)),
            },
            KeyBinding {
                key: Key::T,
                mods: cmd,
                action: BindingAction::Named(SpawnNewInstance(action::SpawnNewInstance)),
            },
            KeyBinding {
                key: Key::Num0,
                mods: cmd,
                action: BindingAction::Named(ResetFontSize(action::ResetFontSize)),
            },
            KeyBinding {
                key: Key::Equals,
                mods: cmd,
                action: BindingAction::Named(IncreaseFontSize(action::IncreaseFontSize)),
            },
            KeyBinding {
                key: Key::Plus,
                mods: cmd,
                action: BindingAction::Named(IncreaseFontSize(action::IncreaseFontSize)),
            },
            KeyBinding {
                key: Key::Minus,
                mods: cmd,
                action: BindingAction::Named(DecreaseFontSize(action::DecreaseFontSize)),
            },
            KeyBinding {
                key: Key::CloseBracket,
                mods: cmd_shift,
                action: BindingAction::Named(SelectNextTab(action::SelectNextTab)),
            },
            KeyBinding {
                key: Key::OpenBracket,
                mods: cmd_shift,
                action: BindingAction::Named(SelectPreviousTab(action::SelectPreviousTab)),
            },
            KeyBinding {
                key: Key::Num1,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(1))),
            },
            KeyBinding {
                key: Key::Num2,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(2))),
            },
            KeyBinding {
                key: Key::Num3,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(3))),
            },
            KeyBinding {
                key: Key::Num4,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(4))),
            },
            KeyBinding {
                key: Key::Num5,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(5))),
            },
            KeyBinding {
                key: Key::Num6,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(6))),
            },
            KeyBinding {
                key: Key::Num7,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(7))),
            },
            KeyBinding {
                key: Key::Num8,
                mods: cmd,
                action: BindingAction::Named(SelectTab(action::SelectTab(8))),
            },
            KeyBinding {
                key: Key::Num9,
                mods: cmd,
                action: BindingAction::Named(SelectLastTab(action::SelectLastTab)),
            },
            KeyBinding {
                key: Key::F,
                mods: cmd_ctrl,
                action: BindingAction::Named(ToggleFullscreen(action::ToggleFullscreen)),
            },
            KeyBinding {
                key: Key::M,
                mods: cmd,
                action: BindingAction::Named(Minimize(action::Minimize)),
            },
            KeyBinding {
                key: Key::K,
                mods: cmd,
                action: BindingAction::Named(ClearHistory(action::ClearHistory)),
            },
            KeyBinding { key: Key::Q, mods: cmd, action: BindingAction::Named(Quit(action::Quit)) },
        ]);
    }

    b
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
        log::warn!("ignoring {n} binding: numpad keys cannot be told apart from the main keys");
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

/// Every name `parse_action` accepts for an action alacritree implements.
fn action_names() -> Vec<String> {
    let select_tabs = (1u8..=9).map(|n| NamedAction::SelectTab(action::SelectTab(n)));
    let spawn_profiles = (1u8..=9).map(|n| NamedAction::SpawnProfile(action::SpawnProfile(n)));
    NamedAction::iter().chain(select_tabs).chain(spawn_profiles).map(|a| a.config_name()).collect()
}

/// The schema for a binding's `action`: every name alacritree implements as an
/// `enum`, beside an open string branch.  The `enum` alone would be wrong —
/// the shared `alacritty.toml` legitimately carries actions only the real
/// alacritty implements, which alacritree ignores rather than rejects — so an
/// editor completes from the names while every other value still validates.
fn action_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [{ "enum": action_names() }, { "type": "string" }],
    })
}

/// The action a binding's `action` names. A name alacritree does not
/// implement comes back `Unsupported` rather than failing the config, since
/// a shared `alacritty.toml` carries actions only alacritty knows.
pub(crate) fn parse_action(name: &str) -> BindingAction {
    let index = |prefix: &str| match name.strip_prefix(prefix)?.as_bytes() {
        &[digit @ b'1'..=b'9'] => Some(digit - b'0'),
        _ => None,
    };
    let action = if let Some(n) = index("SelectTab") {
        NamedAction::SelectTab(action::SelectTab(n))
    } else if let Some(n) = index("SpawnProfile") {
        NamedAction::SpawnProfile(action::SpawnProfile(n))
    } else if let Ok(action) = name.parse() {
        action
    } else {
        return BindingAction::Unsupported(name.to_string());
    };
    BindingAction::Named(action)
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

    fn all_matches(bindings: &[KeyBinding], key: Key, mods: Modifiers) -> Vec<&BindingAction> {
        bindings.iter().filter(|b| b.key == key && mods.fires(b.mods)).map(|b| &b.action).collect()
    }

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
            matched
                .iter()
                .any(|a| matches!(a, BindingAction::Named(NamedAction::Copy(action::Copy)))),
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
            matched
                .iter()
                .any(|a| matches!(a, BindingAction::Named(NamedAction::Paste(action::Paste)))),
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
                matches!(action, BindingAction::Named(NamedAction::SpawnProfile(action::SpawnProfile(m))) if m == n),
                "SpawnProfile{n} parsed to {action:?}"
            );
        }
    }

    #[test]
    fn user_binding_replaces_same_trigger_default() {
        // `ReceiveChar` on Ctrl+B frees the tmux prefix: the default
        // ToggleLeftSidebar must be gone, not merely outvoted.
        let b = parse_bindings(vec![raw_action("B", Some("Control"), "ReceiveChar")]);
        assert_eq!(named_matches(&b, Key::B, Modifiers::CTRL), vec![NamedAction::ReceiveChar(
            action::ReceiveChar
        )]);
    }

    #[test]
    fn replacement_requires_exact_mods() {
        let b = parse_bindings(vec![raw_action("Tab", Some("Control|Shift"), "SelectLastTab")]);
        assert_eq!(
            named_matches(&b, Key::Tab, Modifiers::CTRL),
            vec![NamedAction::SelectNextTab(action::SelectNextTab)],
            "Ctrl+Tab default must survive a Ctrl+Shift+Tab user binding"
        );
        assert_eq!(named_matches(&b, Key::Tab, Modifiers::CTRL | Modifiers::SHIFT), vec![
            NamedAction::SelectLastTab(action::SelectLastTab)
        ]);
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
            ("ToggleLeftSidebar", NamedAction::ToggleLeftSidebar(action::ToggleLeftSidebar)),
            ("ToggleRightSidebar", NamedAction::ToggleRightSidebar(action::ToggleRightSidebar)),
            ("SelectNextWorkspace", NamedAction::SelectNextWorkspace(action::SelectNextWorkspace)),
            (
                "SelectPreviousWorkspace",
                NamedAction::SelectPreviousWorkspace(action::SelectPreviousWorkspace),
            ),
            ("AddProject", NamedAction::AddProject(action::AddProject)),
            ("ToggleSidebarFocus", NamedAction::ToggleSidebarFocus(action::ToggleSidebarFocus)),
            (
                "FocusProjectsSidebar",
                NamedAction::FocusProjectsSidebar(action::FocusProjectsSidebar),
            ),
            ("FocusTerminal", NamedAction::FocusTerminal(action::FocusTerminal)),
            ("FocusGitSidebar", NamedAction::FocusGitSidebar(action::FocusGitSidebar)),
            ("ToggleSessionRows", NamedAction::ToggleSessionRows(action::ToggleSessionRows)),
            (
                "ToggleDetachedSessionsFilter",
                NamedAction::ToggleDetachedSessionsFilter(action::ToggleDetachedSessionsFilter),
            ),
            ("ToggleSessionTabs", NamedAction::ToggleSessionTabs(action::ToggleSessionTabs)),
            ("ToggleSessionDrag", NamedAction::ToggleSessionDrag(action::ToggleSessionDrag)),
            ("MoveSessionUp", NamedAction::MoveSessionUp(action::MoveSessionUp)),
            ("MoveSessionDown", NamedAction::MoveSessionDown(action::MoveSessionDown)),
            ("FocusLeft", NamedAction::FocusLeft(action::FocusLeft)),
            ("FocusRight", NamedAction::FocusRight(action::FocusRight)),
            ("SetBaseBranch", NamedAction::SetBaseBranch(action::SetBaseBranch)),
        ] {
            let b = parse_bindings(vec![raw_action("F1", None, name)]);
            assert_eq!(named_matches(&b, Key::F1, Modifiers::NONE), vec![expected], "{name}");
        }
    }

    #[test]
    fn search_action_names_parse() {
        for (name, expected) in [
            (
                "SidebarSearchConfirm",
                NamedAction::SidebarSearchConfirm(action::SidebarSearchConfirm),
            ),
            ("SidebarSearchCancel", NamedAction::SidebarSearchCancel(action::SidebarSearchCancel)),
            (
                "SidebarSearchCancelToTerminal",
                NamedAction::SidebarSearchCancelToTerminal(action::SidebarSearchCancelToTerminal),
            ),
        ] {
            let b = parse_bindings(vec![raw_action("F1", None, name)]);
            assert_eq!(named_matches(&b, Key::F1, Modifiers::NONE), vec![expected], "{name}");
        }
    }

    #[test]
    fn session_reorder_actions_parse_from_config_names() {
        for (name, expected) in [
            ("ToggleSessionDrag", NamedAction::ToggleSessionDrag(action::ToggleSessionDrag)),
            ("MoveSessionUp", NamedAction::MoveSessionUp(action::MoveSessionUp)),
            ("MoveSessionDown", NamedAction::MoveSessionDown(action::MoveSessionDown)),
        ] {
            assert!(
                matches!(parse_action(name), BindingAction::Named(a) if a == expected),
                "{name} does not parse"
            );
        }
    }

    #[test]
    fn is_search_scoped_is_exactly_the_three_search_actions() {
        assert!(NamedAction::SidebarSearchConfirm(action::SidebarSearchConfirm).is_search_scoped());
        assert!(NamedAction::SidebarSearchCancel(action::SidebarSearchCancel).is_search_scoped());
        assert!(
            NamedAction::SidebarSearchCancelToTerminal(action::SidebarSearchCancelToTerminal)
                .is_search_scoped()
        );
        for a in [
            NamedAction::Paste(action::Paste),
            NamedAction::FocusTerminal(action::FocusTerminal),
            NamedAction::SidebarTop(action::SidebarTop),
            NamedAction::ToggleProjectExpanded(action::ToggleProjectExpanded),
            NamedAction::Quit(action::Quit),
        ] {
            assert!(!a.is_search_scoped(), "{a:?} must not be search-scoped");
        }
    }

    #[test]
    fn search_actions_have_default_bindings() {
        let b = parse_bindings(vec![]);
        // Enter is shared with the exited-session close: two disjoint scopes on
        // one trigger, neither of which is live while the terminal is.
        assert!(
            named_matches(&b, Key::Enter, Modifiers::NONE)
                .contains(&NamedAction::SidebarSearchConfirm(action::SidebarSearchConfirm))
        );
        assert_eq!(named_matches(&b, Key::Escape, Modifiers::NONE), vec![
            NamedAction::SidebarSearchCancel(action::SidebarSearchCancel)
        ]);
        assert_eq!(named_matches(&b, Key::Escape, Modifiers::SHIFT), vec![
            NamedAction::SidebarSearchCancelToTerminal(action::SidebarSearchCancelToTerminal)
        ]);
        // Plain Esc and Shift+Esc are distinct triggers, not aliases.
        assert!(named_matches(&b, Key::Enter, Modifiers::SHIFT).is_empty());
    }

    #[test]
    fn user_binding_replaces_search_confirm_default() {
        let b = parse_bindings(vec![raw_action("Enter", None, "ReceiveChar")]);
        assert_eq!(named_matches(&b, Key::Enter, Modifiers::NONE), vec![NamedAction::ReceiveChar(
            action::ReceiveChar
        )]);
    }

    #[test]
    fn user_binding_replaces_sidebar_focus_default() {
        let b = parse_bindings(vec![raw_action("B", Some("Control|Shift"), "ReceiveChar")]);
        assert_eq!(named_matches(&b, Key::B, Modifiers::CTRL | Modifiers::SHIFT), vec![
            NamedAction::ReceiveChar(action::ReceiveChar)
        ]);
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
        assert!(matches!(
            m[0],
            BindingAction::Named(NamedAction::ClearHistory(action::ClearHistory))
        ));
        assert!(matches!(m[1], BindingAction::Chars(c) if c == b"\x0c"));
    }

    #[test]
    fn mode_binding_does_not_replace_default() {
        let mut r = raw_action("B", Some("Control"), "ToggleViMode");
        r.mode = Some("Vi".into());
        let b = parse_bindings(vec![r]);
        assert_eq!(named_matches(&b, Key::B, Modifiers::CTRL), vec![
            NamedAction::ToggleLeftSidebar(action::ToggleLeftSidebar)
        ]);
    }

    #[test]
    fn default_app_shortcuts_present_without_user_config() {
        use NamedAction::*;
        let ctrl = Modifiers::CTRL;
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let alt = Modifiers::ALT;
        let b = parse_bindings(Vec::new());
        for (key, mods, expected) in [
            (Key::B, ctrl, ToggleLeftSidebar(action::ToggleLeftSidebar)),
            (Key::G, ctrl, ToggleRightSidebar(action::ToggleRightSidebar)),
            (Key::Backtick, ctrl, OpenScratchpad(action::OpenScratchpad)),
            (Key::Backtick, ctrl_shift, OpenTasks(action::OpenTasks)),
            (Key::Tab, ctrl, SelectNextTab(action::SelectNextTab)),
            (Key::Tab, ctrl_shift, SelectPreviousTab(action::SelectPreviousTab)),
            (Key::ArrowRight, alt, SelectNextWorkspace(action::SelectNextWorkspace)),
            (Key::ArrowLeft, alt, SelectPreviousWorkspace(action::SelectPreviousWorkspace)),
            (Key::O, ctrl_shift, AddProject(action::AddProject)),
            (Key::T, ctrl, SpawnNewInstance(action::SpawnNewInstance)),
            (Key::Q, ctrl, Quit(action::Quit)),
            (Key::B, ctrl_shift, ToggleSidebarFocus(action::ToggleSidebarFocus)),
            (Key::G, ctrl_shift, FocusGitSidebar(action::FocusGitSidebar)),
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
        assert_eq!(named_matches(&b, Key::W, Modifiers::CTRL | Modifiers::SHIFT), vec![
            NamedAction::CloseSession(action::CloseSession)
        ]);
    }

    /// The bare `Enter` default is only survivable because the action is
    /// exited-scoped, and it shares the trigger with the sidebar's own
    /// search-scoped `Enter`: both match, and scope decides which one acts.
    #[test]
    fn close_exited_session_has_a_bare_enter_default_and_parses() {
        let b = parse_bindings(vec![]);
        let matched = named_matches(&b, Key::Enter, Modifiers::NONE);
        assert!(
            matched.contains(&NamedAction::CloseExitedSession(action::CloseExitedSession)),
            "{matched:?}"
        );
        assert!(
            matched.contains(&NamedAction::SidebarSearchConfirm(action::SidebarSearchConfirm)),
            "{matched:?}"
        );
        assert!(matches!(
            parse_action("CloseExitedSession"),
            BindingAction::Named(NamedAction::CloseExitedSession(action::CloseExitedSession))
        ));
    }

    /// Exited scope is its own axis: sharing another would hand the bare
    /// `Enter` a second dispatch path that a live session's PTY never sees.
    #[test]
    fn only_close_exited_session_is_exited_scoped() {
        use NamedAction::*;
        assert!(CloseExitedSession(action::CloseExitedSession).is_exited_session_scoped());
        assert!(!CloseExitedSession(action::CloseExitedSession).is_search_scoped());
        assert!(!CloseExitedSession(action::CloseExitedSession).is_palette_scoped());
        assert!(!CloseExitedSession(action::CloseExitedSession).is_sidebar_scoped());
        assert!(!CloseExitedSession(action::CloseExitedSession).is_terminal_only());
        for a in [
            CloseSession(action::CloseSession),
            SidebarSearchConfirm(action::SidebarSearchConfirm),
            PaletteTop(action::PaletteTop),
            Quit(action::Quit),
            Paste(action::Paste),
        ] {
            assert!(!a.is_exited_session_scoped(), "{a:?}");
        }
    }

    #[test]
    fn close_session_parses_from_config_name() {
        assert!(matches!(
            parse_action("CloseSession"),
            BindingAction::Named(NamedAction::CloseSession(action::CloseSession))
        ));
    }

    #[test]
    fn select_session_actions_parse_from_config_names() {
        for (name, expected) in [
            ("SelectNextSession", NamedAction::SelectNextSession(action::SelectNextSession)),
            (
                "SelectPreviousSession",
                NamedAction::SelectPreviousSession(action::SelectPreviousSession),
            ),
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
            (Key::Home, NamedAction::SidebarTop(action::SidebarTop), "SidebarTop"),
            (Key::End, NamedAction::SidebarBottom(action::SidebarBottom), "SidebarBottom"),
            (
                Key::PageDown,
                NamedAction::SidebarNextProject(action::SidebarNextProject),
                "SidebarNextProject",
            ),
            (
                Key::PageUp,
                NamedAction::SidebarPreviousProject(action::SidebarPreviousProject),
                "SidebarPreviousProject",
            ),
        ] {
            assert!(named_matches(&b, key, Modifiers::NONE).contains(&expected), "{name}");
            assert!(
                matches!(parse_action(name), BindingAction::Named(a) if a == expected),
                "{name} does not parse"
            );
        }
    }

    /// The palette shares the sidebar's unmodified nav keys: both actions match
    /// the press, and scope decides which one acts.
    #[test]
    fn palette_nav_actions_have_unmodified_defaults_and_parse() {
        let b = parse_bindings(vec![]);
        for (key, expected, name) in [
            (Key::Home, NamedAction::PaletteTop(action::PaletteTop), "PaletteTop"),
            (Key::End, NamedAction::PaletteBottom(action::PaletteBottom), "PaletteBottom"),
            (Key::PageUp, NamedAction::PalettePageUp(action::PalettePageUp), "PalettePageUp"),
            (
                Key::PageDown,
                NamedAction::PalettePageDown(action::PalettePageDown),
                "PalettePageDown",
            ),
        ] {
            assert!(named_matches(&b, key, Modifiers::NONE).contains(&expected), "{name}");
            assert!(
                matches!(parse_action(name), BindingAction::Named(a) if a == expected),
                "{name} does not parse"
            );
            assert!(expected.is_palette_scoped(), "{name} must be palette-scoped");
            assert!(!expected.is_sidebar_scoped(), "{name} must not be sidebar-scoped");
        }
    }

    #[test]
    fn only_palette_nav_actions_are_palette_scoped() {
        for a in [
            NamedAction::Paste(action::Paste),
            NamedAction::TogglePalette(action::TogglePalette),
            NamedAction::SidebarTop(action::SidebarTop),
            NamedAction::SidebarSearchConfirm(action::SidebarSearchConfirm),
            NamedAction::Quit(action::Quit),
        ] {
            assert!(!a.is_palette_scoped(), "{a:?} must not be palette-scoped");
        }
    }

    /// Rebinding a palette key replaces the default on that trigger, sidebar
    /// action included — the shared key stays one trigger, not two.
    #[test]
    fn user_binding_replaces_palette_nav_default() {
        let b = parse_bindings(vec![raw_action("End", None, "PaletteTop")]);
        assert_eq!(named_matches(&b, Key::End, Modifiers::NONE), vec![NamedAction::PaletteTop(
            action::PaletteTop
        )]);
    }

    /// Only the sidebar cursor actions and RefreshProjects are focus-scoped:
    /// everything else (CloseSession included) must keep firing from the
    /// terminal.
    #[test]
    fn only_sidebar_actions_are_sidebar_scoped() {
        use NamedAction::*;
        for a in [
            SidebarTop(action::SidebarTop),
            SidebarBottom(action::SidebarBottom),
            SidebarNextProject(action::SidebarNextProject),
            SidebarPreviousProject(action::SidebarPreviousProject),
            RefreshProjects(action::RefreshProjects),
            DeleteSelected(action::DeleteSelected),
            RenameSelected(action::RenameSelected),
            ToggleProjectExpanded(action::ToggleProjectExpanded),
        ] {
            assert!(a.is_sidebar_scoped(), "{a:?}");
        }
        for a in [
            CloseSession(action::CloseSession),
            ScrollToTop(action::ScrollToTop),
            ScrollPageUp(action::ScrollPageUp),
            ToggleSidebarFocus(action::ToggleSidebarFocus),
            Quit(action::Quit),
        ] {
            assert!(!a.is_sidebar_scoped(), "{a:?}");
        }
    }

    /// Exactly the four actions that carry a browsing-mode guard at dispatch. It
    /// must not be widened to `is_sidebar_scoped`, whose four extra actions run
    /// from the palette during search today.
    #[test]
    fn requires_project_browsing_is_exactly_the_four_guarded_actions() {
        use NamedAction::*;
        for a in [
            RefreshProjects(action::RefreshProjects),
            DeleteSelected(action::DeleteSelected),
            RenameSelected(action::RenameSelected),
            ToggleProjectExpanded(action::ToggleProjectExpanded),
        ] {
            assert!(a.requires_project_browsing(), "{a:?}");
        }
        for a in [
            SidebarTop(action::SidebarTop),
            SidebarBottom(action::SidebarBottom),
            SidebarNextProject(action::SidebarNextProject),
            SidebarPreviousProject(action::SidebarPreviousProject),
            CloseSession(action::CloseSession),
            Quit(action::Quit),
        ] {
            assert!(!a.requires_project_browsing(), "{a:?}");
        }
    }

    #[test]
    fn only_terminal_grid_actions_are_terminal_only() {
        use NamedAction::*;
        for action in [
            ScrollPageUp(action::ScrollPageUp),
            ScrollPageDown(action::ScrollPageDown),
            ScrollHalfPageUp(action::ScrollHalfPageUp),
            ScrollHalfPageDown(action::ScrollHalfPageDown),
            ScrollLineUp(action::ScrollLineUp),
            ScrollLineDown(action::ScrollLineDown),
            ScrollToTop(action::ScrollToTop),
            ScrollToBottom(action::ScrollToBottom),
            ClearHistory(action::ClearHistory),
        ] {
            assert!(action.is_terminal_only(), "{action:?}");
        }
        for action in [
            Paste(action::Paste),
            Copy(action::Copy),
            OpenScratchpad(action::OpenScratchpad),
            OpenTasks(action::OpenTasks),
            CloseSession(action::CloseSession),
            Quit(action::Quit),
        ] {
            assert!(!action.is_terminal_only(), "{action:?}");
        }
    }

    /// Delete is forward-delete inside a shell, so the default binding only
    /// works because the action is sidebar-scoped.
    #[test]
    fn delete_selected_has_an_unmodified_delete_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::Delete, Modifiers::NONE), vec![
            NamedAction::DeleteSelected(action::DeleteSelected)
        ]);
        assert!(matches!(
            parse_action("DeleteSelected"),
            BindingAction::Named(NamedAction::DeleteSelected(action::DeleteSelected))
        ));
    }

    #[test]
    fn refresh_projects_has_an_unmodified_r_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::R, Modifiers::NONE), vec![NamedAction::RefreshProjects(
            action::RefreshProjects
        )]);
        assert!(matches!(
            parse_action("RefreshProjects"),
            BindingAction::Named(NamedAction::RefreshProjects(action::RefreshProjects))
        ));
    }

    /// Shift+R types `R` inside a shell, so like Delete the default only
    /// works because the action is sidebar-scoped.
    #[test]
    fn rename_selected_has_a_shift_r_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::R, Modifiers::SHIFT), vec![NamedAction::RenameSelected(
            action::RenameSelected
        )]);
        assert_eq!(named_matches(&b, Key::R, Modifiers::NONE), vec![NamedAction::RefreshProjects(
            action::RefreshProjects
        )]);
        assert!(matches!(
            parse_action("RenameSelected"),
            BindingAction::Named(NamedAction::RenameSelected(action::RenameSelected))
        ));
    }

    /// `o` is terminal input, so the unmodified default only works because
    /// the action is sidebar-scoped.
    #[test]
    fn toggle_project_expanded_has_an_unmodified_o_default_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::O, Modifiers::NONE), vec![
            NamedAction::ToggleProjectExpanded(action::ToggleProjectExpanded)
        ]);
        assert!(matches!(
            parse_action("ToggleProjectExpanded"),
            BindingAction::Named(NamedAction::ToggleProjectExpanded(action::ToggleProjectExpanded))
        ));
    }

    #[test]
    fn toggle_palette_is_a_default_ctrl_k_binding_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::K, Modifiers::CTRL), vec![NamedAction::TogglePalette(
            action::TogglePalette
        )]);
        assert!(matches!(
            parse_action("TogglePalette"),
            BindingAction::Named(NamedAction::TogglePalette(action::TogglePalette))
        ));
        // The F1 shortcuts window is gone and the palette lists every action,
        // so the old `ShowShortcuts` name is no longer recognized.
        assert!(matches!(parse_action("ShowShortcuts"), BindingAction::Unsupported(_)));
    }

    #[test]
    fn tasks_tab_is_a_default_ctrl_shift_backtick_binding_and_parses() {
        let b = parse_bindings(Vec::new());
        assert_eq!(named_matches(&b, Key::Backtick, Modifiers::CTRL | Modifiers::SHIFT), vec![
            NamedAction::OpenTasks(action::OpenTasks)
        ]);
        assert!(matches!(
            parse_action("OpenTasks"),
            BindingAction::Named(NamedAction::OpenTasks(action::OpenTasks))
        ));
    }

    #[test]
    fn scratchpad_tab_is_a_default_ctrl_backtick_binding_and_parses() {
        let b = parse_bindings(vec![]);
        assert_eq!(named_matches(&b, Key::Backtick, Modifiers::CTRL), vec![
            NamedAction::OpenScratchpad(action::OpenScratchpad)
        ]);
        assert!(matches!(
            parse_action("OpenScratchpad"),
            BindingAction::Named(NamedAction::OpenScratchpad(action::OpenScratchpad))
        ));
    }

    const PROJECT_FILTER_ACTIONS: [NamedAction; 8] = [
        NamedAction::ToggleSessionsFilter(action::ToggleSessionsFilter),
        NamedAction::ToggleDetachedSessionsFilter(action::ToggleDetachedSessionsFilter),
        NamedAction::ToggleAttentionFilter(action::ToggleAttentionFilter),
        NamedAction::TogglePrOpenFilter(action::TogglePrOpenFilter),
        NamedAction::TogglePrDraftFilter(action::TogglePrDraftFilter),
        NamedAction::TogglePrMergedFilter(action::TogglePrMergedFilter),
        NamedAction::TogglePrClosedFilter(action::TogglePrClosedFilter),
        NamedAction::ClearProjectFilters(action::ClearProjectFilters),
    ];

    const GIT_FILTER_ACTIONS: [NamedAction; 4] = [
        NamedAction::ToggleModifiedFilter(action::ToggleModifiedFilter),
        NamedAction::ToggleDeletedFilter(action::ToggleDeletedFilter),
        NamedAction::ToggleUntrackedFilter(action::ToggleUntrackedFilter),
        NamedAction::ClearGitFilters(action::ClearGitFilters),
    ];

    #[test]
    fn every_new_action_round_trips_and_is_described() {
        let mut all = PROJECT_FILTER_ACTIONS.to_vec();
        all.extend(GIT_FILTER_ACTIONS);
        all.push(NamedAction::ToggleSearchScope(action::ToggleSearchScope));
        all.push(NamedAction::RefreshPrStatus(action::RefreshPrStatus));
        all.push(NamedAction::ReviewStaged(action::ReviewStaged));
        all.push(NamedAction::ReviewUnstaged(action::ReviewUnstaged));
        all.push(NamedAction::ReviewBranch(action::ReviewBranch));
        all.push(NamedAction::CloseExitedSession(action::CloseExitedSession));
        all.push(NamedAction::NewMultiplexerPane(action::NewMultiplexerPane));
        all.push(NamedAction::AttachAllMultiplexerPanes(action::AttachAllMultiplexerPanes));
        all.push(NamedAction::DetachAllMultiplexerPanes(action::DetachAllMultiplexerPanes));
        for a in all {
            let name = a.config_name();
            assert!(
                matches!(parse_action(&name), BindingAction::Named(p) if p == a),
                "{name} does not parse back"
            );
            assert!(!a.description().is_empty(), "{name} has no description");
        }
    }

    #[test]
    fn filter_actions_are_scoped_to_their_own_panel() {
        for a in PROJECT_FILTER_ACTIONS {
            assert!(a.is_projects_filter_scoped(), "{a:?}");
            assert!(!a.is_git_filter_scoped(), "{a:?}");
        }
        for a in GIT_FILTER_ACTIONS {
            assert!(a.is_git_filter_scoped(), "{a:?}");
            assert!(!a.is_projects_filter_scoped(), "{a:?}");
        }
    }

    /// The filter actions own their own focus predicates and must not leak into
    /// the ones that already gate other dispatch paths.
    #[test]
    fn filter_actions_carry_no_other_scope() {
        let mut all = PROJECT_FILTER_ACTIONS.to_vec();
        all.extend(GIT_FILTER_ACTIONS);
        for a in all {
            assert!(!a.is_sidebar_scoped(), "{a:?}");
            assert!(!a.is_search_scoped(), "{a:?}");
            assert!(!a.is_palette_scoped(), "{a:?}");
            assert!(!a.requires_project_browsing(), "{a:?}");
        }
    }

    #[test]
    fn refresh_pr_status_is_sidebar_scoped_and_toggle_search_scope_is_unscoped() {
        assert!(NamedAction::RefreshPrStatus(action::RefreshPrStatus).is_sidebar_scoped());
        assert!(!NamedAction::RefreshPrStatus(action::RefreshPrStatus).requires_project_browsing());
        assert!(!NamedAction::ToggleSearchScope(action::ToggleSearchScope).is_sidebar_scoped());
        assert!(
            !NamedAction::ToggleSearchScope(action::ToggleSearchScope).is_projects_filter_scoped()
        );
        assert!(!NamedAction::ToggleSearchScope(action::ToggleSearchScope).is_git_filter_scoped());
    }

    /// The five filters that exist today keep their keys; the four PR filters
    /// introduce no new bare-letter default for anyone.
    #[test]
    fn default_bindings_cover_the_existing_filters_and_no_pr_filter() {
        let binds = parse_bindings(vec![]);
        let bound = |a: NamedAction| {
            binds.iter().find(|b| matches!(&b.action, BindingAction::Named(n) if *n == a))
        };
        for (key, action) in [
            (Key::S, NamedAction::ToggleSessionsFilter(action::ToggleSessionsFilter)),
            (Key::A, NamedAction::ToggleAttentionFilter(action::ToggleAttentionFilter)),
            (Key::M, NamedAction::ToggleModifiedFilter(action::ToggleModifiedFilter)),
            (Key::D, NamedAction::ToggleDeletedFilter(action::ToggleDeletedFilter)),
            (Key::U, NamedAction::ToggleUntrackedFilter(action::ToggleUntrackedFilter)),
        ] {
            let b = bound(action).unwrap_or_else(|| panic!("{action:?} has no default"));
            assert_eq!(b.key, key);
            assert_eq!(b.mods, Modifiers::NONE);
        }
        for a in [
            NamedAction::TogglePrOpenFilter(action::TogglePrOpenFilter),
            NamedAction::TogglePrDraftFilter(action::TogglePrDraftFilter),
            NamedAction::TogglePrMergedFilter(action::TogglePrMergedFilter),
            NamedAction::TogglePrClosedFilter(action::TogglePrClosedFilter),
            NamedAction::RefreshPrStatus(action::RefreshPrStatus),
            NamedAction::ReviewStaged(action::ReviewStaged),
            NamedAction::ReviewUnstaged(action::ReviewUnstaged),
            NamedAction::ReviewBranch(action::ReviewBranch),
            NamedAction::ToggleSearchScope(action::ToggleSearchScope),
            NamedAction::ToggleDetachedSessionsFilter(action::ToggleDetachedSessionsFilter),
        ] {
            assert!(bound(a).is_none(), "{a:?} must ship without a default key");
        }
    }

    /// Two user bindings on one trigger both survive and both come back from
    /// `all_matches`, which is how a user who claimed a letter recovers the
    /// default it displaced. Only the *default* is dropped.
    #[test]
    fn two_user_bindings_on_one_trigger_both_survive() {
        let raw = |action: &str| RawBinding {
            key: "D".into(),
            mods: None,
            mode: None,
            chars: None,
            action: Some(action.into()),
            command: None,
        };
        let binds = parse_bindings(vec![raw("DeleteSelected"), raw("ToggleDeletedFilter")]);

        let matched: Vec<_> = all_matches(&binds, Key::D, Modifiers::NONE);
        assert!(matched.iter().any(|a| matches!(
            a,
            BindingAction::Named(NamedAction::DeleteSelected(action::DeleteSelected))
        )));
        assert!(matched.iter().any(|a| matches!(
            a,
            BindingAction::Named(NamedAction::ToggleDeletedFilter(action::ToggleDeletedFilter))
        )));
    }
}
