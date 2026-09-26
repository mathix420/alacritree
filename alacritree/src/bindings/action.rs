//! One type per keyboard action, for `enum_dispatch` to route a `NamedAction`
//! variant to. What running one does lives with the app, beside the state it
//! touches, so this file links no GUI framework.

macro_rules! unit_actions {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
            pub struct $name;
        )*
    };
}

unit_actions!(
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
    SelectLastTab,
    SelectNextSession,
    SelectPreviousSession,
    ToggleLeftSidebar,
    ToggleRightSidebar,
    SelectNextWorkspace,
    SelectPreviousWorkspace,
    OpenScratchpad,
    OpenTasks,
    IndentTask,
    DedentTask,
    MoveTaskUp,
    MoveTaskDown,
    DeleteTask,
    ToggleCompletedTasks,
    AddProject,
    ToggleSidebarFocus,
    CloseSession,
    CloseExitedSession,
    SidebarTop,
    SidebarBottom,
    SidebarNextProject,
    SidebarPreviousProject,
    RefreshProjects,
    DeleteSelected,
    RenameSelected,
    ToggleProjectExpanded,
    TogglePalette,
    PaletteTop,
    PaletteBottom,
    PalettePageUp,
    PalettePageDown,
    FocusProjectsSidebar,
    FocusGitSidebar,
    FocusTerminal,
    ToggleSessionRows,
    ToggleSessionTabs,
    ToggleSessionDrag,
    MoveSessionUp,
    MoveSessionDown,
    SetBaseBranch,
    Quit,
    NoOp,
    ReceiveChar,
    FocusLeft,
    FocusRight,
    SidebarSearchConfirm,
    SidebarSearchCancel,
    SidebarSearchCancelToTerminal,
    ToggleSessionsFilter,
    ToggleDetachedSessionsFilter,
    NewMultiplexerPane,
    AttachAllMultiplexerPanes,
    DetachAllMultiplexerPanes,
    ToggleAttentionFilter,
    TogglePrOpenFilter,
    TogglePrDraftFilter,
    TogglePrMergedFilter,
    TogglePrClosedFilter,
    ClearProjectFilters,
    ToggleModifiedFilter,
    ToggleDeletedFilter,
    ToggleUntrackedFilter,
    ClearGitFilters,
    ToggleSearchScope,
    RefreshPrStatus,
    ReviewStaged,
    ReviewUnstaged,
    ReviewBranch,
);

/// Select the workspace's nth session, 1-indexed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SelectTab(pub u8);

/// Spawn the nth `[[ui.profiles]]` entry, 1-indexed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpawnProfile(pub u8);
