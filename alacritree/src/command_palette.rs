//! Data model and fuzzy ranking for the Ctrl+K command palette.
//!
//! The palette is one searchable list of everything the user can do from the
//! keyboard: run any bound-or-bindable action, jump to an open session, or
//! switch workspace. It is the successor to the old F1 shortcuts window, so it
//! keeps that window's "discover every action without the docs" property while
//! also *executing* the row you land on.
//!
//! Ranking uses `nucleo` — the same fzf-style matcher the sidebars filter with
//! (`panel_filter`) — so the whole app searches with one engine. This module
//! owns the item model, the ranking, and the palette's own query/selection
//! state; painting and dispatch live in `app.rs`.

use std::path::PathBuf;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::app::WorkspaceKey;
use crate::bindings::{BindingAction, KeyBinding, NamedAction};
use crate::session::SessionId;

/// What activating a palette row does. Each arm is resolved by
/// `run_palette_action` in `app.rs`, which already owns the machinery it needs.
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteAction {
    /// Dispatch a keyboard action, exactly as its binding would.
    Run(NamedAction),
    /// Focus an open session, switching workspace to reach it if needed.
    ActivateSession(SessionId),
    /// Switch to a workspace (`None` is the home tab).
    SwitchWorkspace(WorkspaceKey),
    /// Open the new-worktree prompt for a project, keyed by its root.
    CreateWorktree(PathBuf),
}

/// The heading a row files under. Grouping keeps the list readable now that a
/// row per action (rather than per binding) still runs to fifty-odd entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteSection {
    Clipboard,
    Scrollback,
    Sessions,
    Workspaces,
    Sidebar,
    Window,
    OpenSessions,
    SwitchWorkspace,
    NewWorktree,
}

impl PaletteSection {
    pub fn title(self) -> &'static str {
        match self {
            Self::Clipboard => "Clipboard",
            Self::Scrollback => "Scrollback",
            Self::Sessions => "Sessions",
            Self::Workspaces => "Workspaces & projects",
            Self::Sidebar => "Sidebar & focus",
            Self::Window => "Window & application",
            Self::OpenSessions => "Open sessions",
            Self::SwitchWorkspace => "Switch workspace",
            Self::NewWorktree => "New worktree",
        }
    }
}

/// Where an action row files. Hidden actions land in `Window` and never paint.
fn section_of(a: NamedAction) -> PaletteSection {
    use NamedAction::*;
    use PaletteSection::*;
    match a {
        Paste | PasteSelection | Copy | CopySelection => Clipboard,
        ScrollPageUp | ScrollPageDown | ScrollHalfPageUp | ScrollHalfPageDown => Scrollback,
        ScrollLineUp | ScrollLineDown | ScrollToTop | ScrollToBottom => Scrollback,
        ClearHistory => Scrollback,
        SpawnNewInstance | SpawnProfile(_) | CloseSession => Sessions,
        SelectNextTab | SelectPreviousTab | SelectTab(_) | SelectLastTab => Sessions,
        SelectNextSession | SelectPreviousSession => Sessions,
        ToggleSessionRows | ToggleSessionTabs => Sessions,
        SelectNextWorkspace | SelectPreviousWorkspace => Workspaces,
        AddProject | RefreshProjects | SetBaseBranch => Workspaces,
        ToggleLeftSidebar | ToggleRightSidebar | ToggleSidebarFocus => Sidebar,
        SidebarTop | SidebarBottom | SidebarNextProject | SidebarPreviousProject => Sidebar,
        DeleteSelected | RenameSelected | ToggleProjectExpanded => Sidebar,
        FocusProjectsSidebar | FocusGitSidebar | FocusTerminal => Sidebar,
        FocusLeft | FocusRight => Sidebar,
        SidebarSearchConfirm | SidebarSearchCancel | SidebarSearchCancelToTerminal => Sidebar,
        _ => Window,
    }
}

/// One selectable row. `keys`/`primary`/`secondary` are what the row paints;
/// `search` is the precomputed haystack the matcher scores, so ranking never
/// re-allocates a per-item string on every keystroke.
pub struct PaletteItem {
    pub action: PaletteAction,
    pub section: PaletteSection,
    pub keys: String,
    pub primary: String,
    pub secondary: String,
    search: String,
}

impl PaletteItem {
    fn new(
        action: PaletteAction,
        section: PaletteSection,
        keys: String,
        primary: String,
        secondary: String,
    ) -> Self {
        let search = format!("{primary} {secondary} {keys}");
        Self { action, section, keys, primary, secondary, search }
    }

    fn action(a: NamedAction, keys: String) -> Self {
        Self::new(PaletteAction::Run(a), section_of(a), keys, a.description(), a.config_name())
    }

    pub fn session(id: SessionId, primary: String, secondary: String) -> Self {
        Self::new(
            PaletteAction::ActivateSession(id),
            PaletteSection::OpenSessions,
            String::new(),
            primary,
            secondary,
        )
    }

    pub fn workspace(ws: WorkspaceKey, primary: String, secondary: String) -> Self {
        Self::new(
            PaletteAction::SwitchWorkspace(ws),
            PaletteSection::SwitchWorkspace,
            String::new(),
            primary,
            secondary,
        )
    }

    pub fn create_worktree(root: PathBuf, primary: String, secondary: String) -> Self {
        Self::new(
            PaletteAction::CreateWorktree(root),
            PaletteSection::NewWorktree,
            String::new(),
            primary,
            secondary,
        )
    }
}

/// Rows the palette must not offer to run: unbinds, the pass-through marker,
/// its own toggle (running which from inside would just reopen it), and the
/// palette's own cursor moves — activating a row closes the palette, so a
/// "scroll the palette" row could never do anything. Their keys are advertised
/// in the palette's footer hint instead.
fn is_hidden(a: NamedAction) -> bool {
    matches!(a, NamedAction::NoOp | NamedAction::ReceiveChar | NamedAction::TogglePalette)
        || a.is_palette_scoped()
}

/// Every runnable keyboard action as one row, listing every key bound to it.
/// The fixed vocabulary comes first, then whatever else the config binds — the
/// parametrized `SelectTab`/`SpawnProfile` families, which need an index to
/// run and so only exist as concrete bindings. Actions no binding names are
/// listed too, keyless: runnable from here all the same, and that is how the
/// full vocabulary stays discoverable without the docs.
pub fn action_items(bindings: &[KeyBinding]) -> Vec<PaletteItem> {
    let mut order: Vec<NamedAction> =
        bindable_actions().into_iter().filter(|a| !is_hidden(*a)).collect();
    for b in bindings {
        if let BindingAction::Named(a) = &b.action {
            if !is_hidden(*a) && !order.contains(a) {
                order.push(*a);
            }
        }
    }
    order.into_iter().map(|a| PaletteItem::action(a, keys_for(bindings, a))).collect()
}

/// Every trigger bound to `action`, in binding order — user bindings before the
/// defaults they did not replace.
fn keys_for(bindings: &[KeyBinding], action: NamedAction) -> String {
    bindings
        .iter()
        .filter(|b| matches!(&b.action, BindingAction::Named(a) if *a == action))
        .map(|b| format_shortcut(b.key, b.mods))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first key bound to `action`, for the footer hint.
pub fn first_key(bindings: &[KeyBinding], action: NamedAction) -> Option<String> {
    bindings
        .iter()
        .find(|b| matches!(&b.action, BindingAction::Named(a) if *a == action))
        .map(|b| format_shortcut(b.key, b.mods))
}

/// Ranked rows grouped under their headings. A section appears where its
/// best-ranked row does, so the top match always leads the list and an
/// unfiltered palette keeps the natural order.
pub fn group(items: &[PaletteItem], ranked: &[usize]) -> Vec<(PaletteSection, Vec<usize>)> {
    let mut out: Vec<(PaletteSection, Vec<usize>)> = Vec::new();
    for &i in ranked {
        let section = items[i].section;
        match out.iter_mut().find(|(s, _)| *s == section) {
            Some((_, rows)) => rows.push(i),
            None => out.push((section, vec![i])),
        }
    }
    out
}

/// Every simple (non-parametrized) `NamedAction`, kept in sync with the enum by
/// hand. Mirrors the old shortcuts window's bindable list; `SelectTab`/
/// `SpawnProfile` are excluded here because they carry an index.
fn bindable_actions() -> [NamedAction; 50] {
    use NamedAction::*;
    [
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
        SelectNextWorkspace,
        SelectPreviousWorkspace,
        OpenScratchpad,
        ToggleLeftSidebar,
        ToggleRightSidebar,
        AddProject,
        ToggleSidebarFocus,
        CloseSession,
        SidebarTop,
        SidebarBottom,
        SidebarNextProject,
        SidebarPreviousProject,
        FocusProjectsSidebar,
        FocusGitSidebar,
        FocusTerminal,
        FocusLeft,
        FocusRight,
        ToggleSessionRows,
        ToggleSessionTabs,
        SetBaseBranch,
        SidebarSearchConfirm,
        SidebarSearchCancel,
        SidebarSearchCancelToTerminal,
        Quit,
        TogglePalette,
    ]
}

fn format_shortcut(key: egui::Key, mods: egui::Modifiers) -> String {
    egui::KeyboardShortcut::new(mods, key)
        .format(&egui::ModifierNames::NAMES, cfg!(target_os = "macos"))
}

/// The palette's query, selection cursor, and reusable `nucleo` matcher.
/// Owning the matcher here keeps its scratch allocations alive across
/// keystrokes instead of rebuilding one per frame.
pub struct CommandPalette {
    open: bool,
    query: String,
    selected: usize,
    matcher: Matcher,
    buf: Vec<char>,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self {
            open: false,
            query: String::new(),
            selected: 0,
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open fresh: a stale query or cursor from last time would be confusing.
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open();
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn query_mut(&mut self) -> &mut String {
        &mut self.query
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.selected = 0;
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Indices into `items`, best match first. An empty query keeps the natural
    /// order (actions, then sessions, then workspaces); ties hold their input
    /// order so the list stays stable as the user types.
    pub fn rank(&mut self, items: &[PaletteItem]) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..items.len()).collect();
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut scored: Vec<(u32, usize)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let haystack = Utf32Str::new(&item.search, &mut self.buf);
                pattern.score(haystack, &mut self.matcher).map(|score| (score, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    /// After a rank, settle the cursor: a query edit jumps to the top match,
    /// otherwise the cursor holds its place, clamped to the new result count.
    pub fn reseed(&mut self, query_changed: bool, len: usize) {
        self.selected = if query_changed { 0 } else { self.selected.min(len.saturating_sub(1)) };
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_next(&mut self, len: usize) {
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
        }
    }

    pub fn select_top(&mut self) {
        self.selected = 0;
    }

    pub fn select_bottom(&mut self, len: usize) {
        self.selected = len.saturating_sub(1);
    }

    pub fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(PAGE);
    }

    pub fn page_down(&mut self, len: usize) {
        self.selected = (self.selected + PAGE).min(len.saturating_sub(1));
    }
}

/// Rows a page jump covers — a screenful of the palette's list at its default
/// height, so PgUp/PgDn moves about as far as the eye already sees.
const PAGE: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::parse_bindings;

    fn find<'a>(items: &'a [PaletteItem], config_name: &str) -> Option<&'a PaletteItem> {
        items.iter().find(|i| i.secondary == config_name)
    }

    #[test]
    fn action_items_carry_keys_for_bound_actions() {
        let items = action_items(&parse_bindings(vec![]));
        let close = find(&items, "CloseSession").expect("CloseSession missing");
        assert_eq!(close.keys, "Ctrl+Shift+W");
        assert_eq!(close.action, PaletteAction::Run(NamedAction::CloseSession));
        assert!(!close.primary.is_empty());
    }

    #[test]
    fn action_items_list_unbound_actions_without_keys() {
        let items = action_items(&parse_bindings(vec![]));
        // FocusTerminal has no default binding: present, discoverable, keyless.
        let focus = find(&items, "FocusTerminal").expect("FocusTerminal missing");
        assert!(focus.keys.is_empty());
    }

    #[test]
    fn palette_lists_the_sidebar_search_actions() {
        let items = action_items(&parse_bindings(vec![]));
        for name in ["SidebarSearchConfirm", "SidebarSearchCancel", "SidebarSearchCancelToTerminal"]
        {
            assert!(items.iter().any(|i| i.secondary == name), "{name} should be a palette row");
        }
    }

    /// A multi-bound action stays one row that lists both keys, rather than one
    /// row per key.
    #[test]
    fn a_multi_bound_action_is_one_row_listing_every_key() {
        let bindings = parse_bindings(vec![]);
        let items = action_items(&bindings);
        let rows: Vec<_> = items.iter().filter(|i| i.secondary == "IncreaseFontSize").collect();
        assert_eq!(rows.len(), 1, "IncreaseFontSize should be a single row");
        let expected = keys_for(&bindings, NamedAction::IncreaseFontSize);
        assert!(expected.contains(", "), "IncreaseFontSize has two default keys: {expected}");
        assert_eq!(rows[0].keys, expected);
    }

    /// The palette's own cursor moves are keyboard-only: a row for them would
    /// close the palette it is meant to scroll.
    #[test]
    fn palette_nav_actions_are_not_rows() {
        let items = action_items(&parse_bindings(vec![]));
        for name in ["PaletteTop", "PaletteBottom", "PalettePageUp", "PalettePageDown"] {
            assert!(!items.iter().any(|i| i.secondary == name), "{name} must not be a row");
        }
    }

    #[test]
    fn sections_follow_the_best_match_and_hold_natural_order_unfiltered() {
        let items = action_items(&parse_bindings(vec![]));
        let mut palette = CommandPalette::new();

        let sections: Vec<_> =
            group(&items, &palette.rank(&items)).into_iter().map(|(s, _)| s).collect();
        assert_eq!(
            sections.first().copied(),
            Some(PaletteSection::Clipboard),
            "an unfiltered palette keeps the declared order"
        );

        palette.query_mut().push_str("scrollback");
        let ranked = palette.rank(&items);
        let grouped = group(&items, &ranked);
        assert_eq!(
            grouped.first().map(|(s, _)| *s),
            Some(PaletteSection::Scrollback),
            "the section holding the best match leads"
        );
        // Grouping is a regrouping of the ranked rows, never a filter.
        let total: usize = grouped.iter().map(|(_, rows)| rows.len()).sum();
        assert_eq!(total, ranked.len());
    }

    #[test]
    fn page_and_edge_moves_clamp_to_the_result_count() {
        let mut palette = CommandPalette::new();
        palette.page_down(100);
        assert_eq!(palette.selected(), PAGE);
        palette.page_up();
        assert_eq!(palette.selected(), 0);
        // Already at the top, a page up stays there rather than wrapping.
        palette.page_up();
        assert_eq!(palette.selected(), 0);

        palette.select_bottom(4);
        assert_eq!(palette.selected(), 3);
        palette.page_down(4);
        assert_eq!(palette.selected(), 3);
        palette.select_top();
        assert_eq!(palette.selected(), 0);
        // An empty result set has no row to land on.
        palette.select_bottom(0);
        assert_eq!(palette.selected(), 0);
    }

    #[test]
    fn palette_never_offers_to_toggle_itself() {
        // TogglePalette is bound to Ctrl+K by default, yet must not appear as a
        // row — running it from inside the palette would only reopen it.
        let items = action_items(&parse_bindings(vec![]));
        assert!(!items.iter().any(|i| i.action == PaletteAction::Run(NamedAction::TogglePalette)));
    }

    #[test]
    fn rank_orders_matches_ahead_of_non_matches_and_filters_the_rest() {
        let items = vec![
            PaletteItem::action(NamedAction::Paste, String::new()),
            PaletteItem::action(NamedAction::CloseSession, String::new()),
        ];
        let mut palette = CommandPalette::new();
        palette.query_mut().push_str("paste");
        let ranked = palette.rank(&items);
        assert_eq!(ranked.first().copied(), Some(0), "Paste should rank first");

        palette.clear_query();
        palette.query_mut().push_str("zzxxqq");
        assert!(palette.rank(&items).is_empty(), "a no-match query filters everything");
    }

    #[test]
    fn empty_query_keeps_every_item_in_order() {
        let items = action_items(&parse_bindings(vec![]));
        let mut palette = CommandPalette::new();
        assert_eq!(palette.rank(&items), (0..items.len()).collect::<Vec<_>>());
    }

    #[test]
    fn selection_reseeds_on_edit_and_clamps_otherwise() {
        let mut palette = CommandPalette::new();
        palette.select_next(5);
        palette.select_next(5);
        assert_eq!(palette.selected(), 2);
        // A held cursor clamps to a shrunken list.
        palette.reseed(false, 2);
        assert_eq!(palette.selected(), 1);
        // Editing the query jumps back to the top match.
        palette.reseed(true, 2);
        assert_eq!(palette.selected(), 0);
    }
}
