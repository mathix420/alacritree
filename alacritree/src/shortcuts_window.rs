//! Data model for the searchable shortcuts window: the effective binding
//! rows, the static sidebar-navigation entries, the actions no binding
//! currently triggers, and the fuzzy matcher the search box filters them
//! with.  Pure — painting lives in `app.rs`.

use crate::bindings::{BindingAction, KeyBinding, NamedAction};

/// One line in the shortcuts window.
pub struct ShortcutRow {
    pub keys: String,
    pub name: String,
    pub description: String,
}

/// Case-insensitive subsequence match — `csw` finds `Ctrl+Shift+W`.  An
/// empty query matches everything, so the unfiltered window needs no
/// special case.
pub fn fuzzy_match(query: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    query.chars().flat_map(char::to_lowercase).all(|q| hay.any(|h| h == q))
}

// Matched per field rather than on a joined "keys name description" string:
// concatenating would let query letters bleed across field boundaries (e.g.
// "font" spuriously matching "f" in a Shift key plus "o"/"n"/"t" scattered
// across the name and description of an unrelated row).
pub fn row_matches(query: &str, row: &ShortcutRow) -> bool {
    fuzzy_match(query, &row.keys)
        || fuzzy_match(query, &row.name)
        || fuzzy_match(query, &row.description)
}

/// The effective app-shortcut rows: `parse_bindings` already replaced
/// shadowed defaults with the user's same-trigger bindings, so every Named
/// entry here genuinely fires.  `NoOp`/`ReceiveChar` unbind rather than
/// bind and `Chars`/`Unsupported` aren't app shortcuts — no rows for them.
pub fn named_rows(bindings: &[KeyBinding]) -> Vec<ShortcutRow> {
    bindings
        .iter()
        .filter_map(|b| {
            let BindingAction::Named(action) = &b.action else {
                return None;
            };
            if matches!(action, NamedAction::NoOp | NamedAction::ReceiveChar) {
                return None;
            }
            Some(ShortcutRow {
                keys: format_shortcut(b.key, b.mods),
                name: action.config_name(),
                description: action.description(),
            })
        })
        .collect()
}

/// Actions no current binding triggers — the discoverable remainder the
/// shortcuts window lists so users learn what `[[keyboard.bindings]]` can
/// name without reading the docs.
pub fn unbound_rows(bindings: &[KeyBinding]) -> Vec<ShortcutRow> {
    let bound: std::collections::HashSet<String> = bindings
        .iter()
        .filter_map(|b| match &b.action {
            BindingAction::Named(a) => Some(a.config_name()),
            _ => None,
        })
        .collect();
    bindable_rows()
        .into_iter()
        .filter(|(names, _)| names.iter().all(|n| !bound.contains(n)))
        .map(|(_, row)| row)
        .collect()
}

fn simple(a: NamedAction) -> (Vec<String>, ShortcutRow) {
    (
        vec![a.config_name()],
        ShortcutRow { keys: String::new(), name: a.config_name(), description: a.description() },
    )
}

fn family(prefix: &str, description: &str) -> (Vec<String>, ShortcutRow) {
    (
        (1..=9).map(|n| format!("{prefix}{n}")).collect(),
        ShortcutRow {
            keys: String::new(),
            name: format!("{prefix}1 … {prefix}9"),
            description: description.into(),
        },
    )
}

/// Every action a `[[keyboard.bindings]]` entry can name, paired with the
/// config names that hide its row once any of them is bound.  Parametrized
/// families collapse to one row each.  Kept in sync with `NamedAction` by
/// hand (`NoOp`/`ReceiveChar` unbind rather than bind — no rows).
fn bindable_rows() -> Vec<(Vec<String>, ShortcutRow)> {
    use NamedAction::*;
    let mut rows: Vec<_> = [
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
        ShowShortcuts,
        FocusProjectsSidebar,
        FocusGitSidebar,
        FocusTerminal,
        FocusLeft,
        FocusRight,
        ToggleSessionRows,
        ToggleSessionTabs,
        SetBaseBranch,
        Quit,
    ]
    .into_iter()
    .map(simple)
    .collect();
    rows.push(family("SelectTab", "Select the Nth session in the current workspace"));
    rows.push(family("SpawnProfile", "Open a session with the Nth [[ui.profiles]] entry"));
    rows
}

fn format_shortcut(key: egui::Key, mods: egui::Modifiers) -> String {
    egui::KeyboardShortcut::new(mods, key)
        .format(&egui::ModifierNames::NAMES, cfg!(target_os = "macos"))
}

fn nav_row(keys: &str, description: &str) -> ShortcutRow {
    ShortcutRow { keys: keys.into(), name: String::new(), description: description.into() }
}

/// The hardcoded sidebar keys (`handle_sidebar_nav` / `PanelFilter`), which
/// the binding table never sees.  Kept in sync by hand; they change rarely.
pub fn sidebar_nav_rows() -> Vec<ShortcutRow> {
    vec![
        nav_row("Up / Down", "Move the cursor"),
        nav_row("Right", "Expand a project, or open the cursored session"),
        nav_row("Left", "Collapse a project, or jump to the parent row"),
        nav_row("Enter", "Activate the cursored row"),
        nav_row("Escape", "Clear the filter, or return focus to the terminal"),
        nav_row("/", "Start fuzzy-filtering the panel's rows"),
        nav_row("s", "Projects panel: show only workspaces with open sessions"),
        nav_row("a", "Projects panel: show only workspaces needing attention"),
        nav_row("m / d / u", "Git panel: show only modified / deleted / untracked files"),
        nav_row("Backspace", "Delete the last filter character (while filtering)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::{NamedAction, RawBinding, parse_bindings};

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

    #[test]
    fn fuzzy_match_is_a_case_insensitive_subsequence() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("csw", "Ctrl+Shift+W CloseSession"));
        assert!(fuzzy_match("CLOSE", "close the cursored session"));
        // Subsequence, not substring: letters may be spread out…
        assert!(fuzzy_match("cse", "CloseSession"));
        // …but order matters and letters aren't reused.
        assert!(!fuzzy_match("wsc", "Ctrl+Shift+W"));
        assert!(!fuzzy_match("zz", "\u{2318}z"));
    }

    #[test]
    fn named_rows_lists_defaults_with_descriptions() {
        let rows = named_rows(&parse_bindings(vec![]));
        let close =
            rows.iter().find(|r| r.name == "CloseSession").expect("CloseSession missing from rows");
        assert_eq!(close.keys, "Ctrl+Shift+W");
        assert!(!close.description.is_empty());
        // Chars defaults (Shift+Tab -> CSI Z) are terminal plumbing, not
        // app shortcuts: no row.
        assert!(!rows.iter().any(|r| r.keys == "Shift+Tab"));
    }

    #[test]
    fn named_rows_honors_user_overrides_and_unbinds() {
        let rows = named_rows(&parse_bindings(vec![
            raw_action("W", Some("Control|Shift"), "Quit"),
            raw_action("B", Some("Control"), "ReceiveChar"),
        ]));
        // The rebound trigger shows the user's action only.
        let w: Vec<_> = rows.iter().filter(|r| r.keys == "Ctrl+Shift+W").collect();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].name, "Quit");
        // A key freed with ReceiveChar disappears entirely.
        assert!(!rows.iter().any(|r| r.keys == "Ctrl+B"));
    }

    #[test]
    fn every_named_action_row_has_a_nonempty_description() {
        for row in named_rows(&parse_bindings(vec![])) {
            assert!(!row.description.is_empty(), "{} has no description", row.name);
        }
        // Parametrized actions too, which no default binding covers.
        assert!(!NamedAction::SelectTab(3).description().is_empty());
        assert!(!NamedAction::SpawnProfile(2).description().is_empty());
        assert_eq!(NamedAction::SelectTab(3).config_name(), "SelectTab3");
    }

    #[test]
    fn unbound_rows_lists_only_actions_without_bindings() {
        let rows = unbound_rows(&parse_bindings(vec![]));
        // Actions no default binds show up…
        assert!(rows.iter().any(|r| r.name == "SelectNextSession"));
        assert!(rows.iter().any(|r| r.name == "FocusTerminal"));
        // …while bound defaults stay out.
        assert!(!rows.iter().any(|r| r.name == "CloseSession"));
        // Every row is discoverable by name and description; keys stay empty.
        for r in &rows {
            assert!(r.keys.is_empty(), "{} has keys", r.name);
            assert!(!r.name.is_empty() && !r.description.is_empty());
        }
    }

    #[test]
    fn binding_an_action_removes_its_unbound_row() {
        let rows = unbound_rows(&parse_bindings(vec![raw_action(
            "N",
            Some("Control|Shift"),
            "SelectNextSession",
        )]));
        assert!(!rows.iter().any(|r| r.name == "SelectNextSession"));
    }

    #[test]
    fn a_family_row_covers_all_members_and_hides_once_one_is_bound() {
        let rows = unbound_rows(&parse_bindings(vec![]));
        // SpawnProfile has no default binding on any platform: one collapsed
        // row for the whole family, not nine.
        assert_eq!(rows.iter().filter(|r| r.name.contains("SpawnProfile")).count(), 1);
        let rows = unbound_rows(&parse_bindings(vec![raw_action(
            "2",
            Some("Control|Shift"),
            "SpawnProfile2",
        )]));
        assert!(!rows.iter().any(|r| r.name.contains("SpawnProfile")));
    }

    #[test]
    fn sidebar_nav_rows_cover_the_hardcoded_keys() {
        let rows = sidebar_nav_rows();
        for key in ["Up / Down", "Enter", "Escape", "/"] {
            assert!(rows.iter().any(|r| r.keys == key), "{key} missing");
        }
        assert!(rows.iter().all(|r| !r.description.is_empty()));
    }

    #[test]
    fn row_matches_searches_keys_name_and_description() {
        let row = ShortcutRow {
            keys: "Ctrl+Shift+W".into(),
            name: "CloseSession".into(),
            description: "Close the cursored or active session".into(),
        };
        assert!(row_matches("ctrl+shift", &row));
        assert!(row_matches("closesess", &row));
        assert!(row_matches("cursored", &row));
        assert!(!row_matches("font", &row));
    }
}
