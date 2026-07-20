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

use std::collections::HashSet;
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

/// One selectable row. `keys`/`primary`/`secondary` are what the row paints;
/// `search` is the precomputed haystack the matcher scores, so ranking never
/// re-allocates a per-item string on every keystroke.
pub struct PaletteItem {
    pub action: PaletteAction,
    pub keys: String,
    pub primary: String,
    pub secondary: String,
    search: String,
}

impl PaletteItem {
    fn new(action: PaletteAction, keys: String, primary: String, secondary: String) -> Self {
        let search = format!("{primary} {secondary} {keys}");
        Self { action, keys, primary, secondary, search }
    }

    fn action(a: NamedAction, keys: String) -> Self {
        Self::new(PaletteAction::Run(a), keys, a.description(), a.config_name())
    }

    pub fn session(id: SessionId, primary: String, secondary: String) -> Self {
        Self::new(PaletteAction::ActivateSession(id), String::new(), primary, secondary)
    }

    pub fn workspace(ws: WorkspaceKey, primary: String, secondary: String) -> Self {
        Self::new(PaletteAction::SwitchWorkspace(ws), String::new(), primary, secondary)
    }

    pub fn create_worktree(root: PathBuf, primary: String, secondary: String) -> Self {
        Self::new(PaletteAction::CreateWorktree(root), String::new(), primary, secondary)
    }
}

/// Rows the palette must not offer to run: unbinds, the pass-through marker,
/// and its own toggle (running which from inside would just reopen it).
fn is_hidden(a: NamedAction) -> bool {
    matches!(a, NamedAction::NoOp | NamedAction::ReceiveChar | NamedAction::TogglePalette)
}

/// Every runnable keyboard action as a palette row: one per binding first (so a
/// multi-bound action shows once per key, and a concrete `SelectTab`/
/// `SpawnProfile` binding carries its index), then the actions no binding names
/// yet — runnable all the same, shown without keys so the full vocabulary stays
/// discoverable. The parametrized families themselves are left out: they need
/// an index to run, and the palette's session rows already cover "jump to the
/// Nth session" more directly.
pub fn action_items(bindings: &[KeyBinding]) -> Vec<PaletteItem> {
    let mut items = Vec::new();
    for b in bindings {
        if let BindingAction::Named(a) = &b.action {
            if !is_hidden(*a) {
                items.push(PaletteItem::action(*a, format_shortcut(b.key, b.mods)));
            }
        }
    }
    let bound: HashSet<String> = bindings
        .iter()
        .filter_map(|b| match &b.action {
            BindingAction::Named(a) => Some(a.config_name()),
            _ => None,
        })
        .collect();
    for a in bindable_actions() {
        if !is_hidden(a) && !bound.contains(&a.config_name()) {
            items.push(PaletteItem::action(a, String::new()));
        }
    }
    items
}

/// Every simple (non-parametrized) `NamedAction`, kept in sync with the enum by
/// hand. Mirrors the old shortcuts window's bindable list; `SelectTab`/
/// `SpawnProfile` are excluded here because they carry an index.
fn bindable_actions() -> [NamedAction; 46] {
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
}

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
