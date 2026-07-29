//! Per-panel input mode, fuzzy-search query, and toggle-filter state for the
//! sidebars.
//!
//! The search prompt this drives is custom-drawn (`/query` + caret), not an
//! egui `TextEdit`: giving a widget native egui focus would fight the
//! terminal view, which egui fake-clicks on Space/Enter whenever it holds
//! that same native focus. Routing `Event::Text`/key presses through this
//! module instead lets the terminal view keep focus throughout.

use std::collections::BTreeSet;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Whether a panel is browsing its rows or typing a fuzzy-search query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browsing,
    Search,
}

/// What the caller should do in response to a key/text event the filter
/// consumed. `None` from `on_key`/`on_text` means the event fell through
/// unconsumed and the caller's existing handling should run instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    FilterChanged,
    MoveCursor(i32),
    LeavePanel,
    Consumed,
}

/// Search/toggle state for one sidebar panel.
pub struct PanelFilter {
    /// Render/bit order of this panel's toggle filters, not a set of keys:
    /// `active_toggles` renders in this order and `toggle_bits` indexes it.
    allowed_toggles: &'static [char],
    mode: Mode,
    query: String,
    toggles: BTreeSet<char>,
    pattern: Pattern,
    matcher: Matcher,
    buf: Vec<char>,
}

impl PanelFilter {
    pub fn new(allowed_toggles: &'static [char]) -> Self {
        Self {
            allowed_toggles,
            mode: Mode::Browsing,
            pattern: parse_pattern(""),
            query: String::new(),
            toggles: BTreeSet::new(),
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn is_toggled(&self, key: char) -> bool {
        self.toggles.contains(&key)
    }

    /// Flip one toggle by its identity char.  A char outside `allowed_toggles`
    /// names no filter on this panel and is ignored.
    pub fn toggle(&mut self, key: char) {
        if !self.allowed_toggles.contains(&key) {
            return;
        }
        if !self.toggles.remove(&key) {
            self.toggles.insert(key);
        }
    }

    pub fn clear_toggles(&mut self) {
        self.toggles.clear();
    }

    /// Active toggles in `allowed_toggles` order (render order).
    pub fn active_toggles(&self) -> Vec<char> {
        self.allowed_toggles.iter().copied().filter(|k| self.toggles.contains(k)).collect()
    }

    /// The active toggles as a bitmask over `allowed_toggles` order.  The
    /// focus reconciler compares this on every frame, where `active_toggles`'s
    /// `Vec` would put an allocation in the steady-state path.
    pub fn toggle_bits(&self) -> u32 {
        self.allowed_toggles
            .iter()
            .enumerate()
            .filter(|(_, key)| self.toggles.contains(key))
            .fold(0, |bits, (i, _)| bits | (1 << i))
    }

    /// Whether the panel currently narrows its rows: a non-empty query or
    /// any active toggle.
    pub fn is_filtering(&self) -> bool {
        !self.query.is_empty() || !self.toggles.is_empty()
    }

    /// Whether the toggle filters apply this frame.  Under `All` a live query
    /// stands them down, so a search reaches rows the toggles hide.
    pub fn toggles_apply(&self, scope: crate::config::SearchScope) -> bool {
        scope == crate::config::SearchScope::Filtered || self.query.is_empty()
    }

    pub fn on_key(&mut self, key: egui::Key) -> Option<Outcome> {
        match self.mode {
            Mode::Browsing => match key {
                egui::Key::Escape if !self.toggles.is_empty() => {
                    self.toggles.clear();
                    Some(Outcome::FilterChanged)
                },
                egui::Key::Escape => Some(Outcome::LeavePanel),
                _ => None,
            },
            Mode::Search => match key {
                egui::Key::Backspace => {
                    self.query.pop();
                    self.rebuild_pattern();
                    Some(Outcome::FilterChanged)
                },
                egui::Key::ArrowUp => Some(Outcome::MoveCursor(-1)),
                egui::Key::ArrowDown => Some(Outcome::MoveCursor(1)),
                // Enter/Escape are search-scoped keyboard actions dispatched
                // through the binding table by the sidebar nav handler, not
                // consumed here, so they stay rebindable.
                _ => None,
            },
        }
    }

    pub fn on_text(&mut self, text: &str) -> Option<Outcome> {
        match self.mode {
            Mode::Browsing => {
                if text == "/" {
                    self.mode = Mode::Search;
                    return Some(Outcome::Consumed);
                }
                None
            },
            Mode::Search => {
                self.query.push_str(text);
                self.rebuild_pattern();
                Some(Outcome::FilterChanged)
            },
        }
    }

    /// Whether `haystack` matches the current query. An empty query matches
    /// everything.
    pub fn matches(&mut self, haystack: &str) -> bool {
        if self.query.is_empty() {
            return true;
        }
        let haystack = Utf32Str::new(haystack, &mut self.buf);
        self.pattern.score(haystack, &mut self.matcher).is_some()
    }

    /// Leave search mode: clear the query (rebuilding the empty, match-all
    /// pattern) and return to browsing. Toggle filters are a separate dimension
    /// and are left intact.
    pub fn exit_search(&mut self) {
        self.clear_query();
        self.mode = Mode::Browsing;
    }

    fn clear_query(&mut self) {
        self.query.clear();
        self.rebuild_pattern();
    }

    fn rebuild_pattern(&mut self) {
        self.pattern = parse_pattern(&self.query);
    }
}

fn parse_pattern(query: &str) -> Pattern {
    Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOGGLES: &[char] = &['s', 'a'];

    #[test]
    fn slash_enters_search_mode_and_is_consumed() {
        let mut f = PanelFilter::new(TOGGLES);
        assert_eq!(f.on_text("/"), Some(Outcome::Consumed));
        assert_eq!(f.mode(), Mode::Search);
    }

    #[test]
    fn typing_in_search_builds_the_query_and_reports_filter_change() {
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        assert_eq!(f.on_text("f"), Some(Outcome::FilterChanged));
        assert_eq!(f.on_text("oo"), Some(Outcome::FilterChanged));
        assert_eq!(f.query(), "foo");
    }

    #[test]
    fn backspace_pops_the_query_in_search() {
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        f.on_text("foo");
        assert_eq!(f.on_key(egui::Key::Backspace), Some(Outcome::FilterChanged));
        assert_eq!(f.query(), "fo");
        assert_eq!(f.mode(), Mode::Search);
    }

    #[test]
    fn enter_and_escape_in_search_fall_through_unconsumed() {
        // Both are rebindable search actions dispatched via the binding table,
        // so the filter must not consume them or mutate its own state.
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        f.on_text("foo");
        assert_eq!(f.on_key(egui::Key::Enter), None);
        assert_eq!(f.on_key(egui::Key::Escape), None);
        assert_eq!(f.mode(), Mode::Search);
        assert_eq!(f.query(), "foo");
    }

    #[test]
    fn exit_search_clears_query_and_returns_to_browsing_keeping_toggles() {
        let mut f = PanelFilter::new(TOGGLES);
        f.toggle('s');
        f.on_text("/");
        f.on_text("foo");
        assert!(f.is_toggled('s'));

        f.exit_search();
        assert_eq!(f.mode(), Mode::Browsing);
        assert_eq!(f.query(), "");
        assert!(f.is_toggled('s'), "exit_search must leave toggle filters intact");
    }

    #[test]
    fn arrows_in_search_move_the_cursor() {
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        assert_eq!(f.on_key(egui::Key::ArrowUp), Some(Outcome::MoveCursor(-1)));
        assert_eq!(f.on_key(egui::Key::ArrowDown), Some(Outcome::MoveCursor(1)));
    }

    #[test]
    fn toggle_flips_an_allowed_identity_and_ignores_an_unknown_one() {
        let mut f = PanelFilter::new(TOGGLES);
        f.toggle('s');
        assert!(f.is_toggled('s'));
        assert_eq!(f.active_toggles(), vec!['s']);

        f.toggle('s');
        assert!(!f.is_toggled('s'));

        f.toggle('z');
        assert!(!f.is_toggled('z'), "a char outside allowed_toggles is not a filter");
        assert_eq!(f.toggle_bits(), 0);
    }

    #[test]
    fn clear_toggles_empties_the_set_and_leaves_the_query() {
        let mut f = PanelFilter::new(TOGGLES);
        f.toggle('s');
        f.toggle('a');
        f.on_text("/");
        f.on_text("foo");

        f.clear_toggles();
        assert_eq!(f.toggle_bits(), 0);
        assert_eq!(f.query(), "foo", "the query is a separate dimension");
    }

    /// Browsing recognizes only `/`; every other char falls through so the
    /// binding table can act on the paired key event.
    #[test]
    fn browsing_text_other_than_slash_is_not_consumed() {
        let mut f = PanelFilter::new(TOGGLES);
        assert_eq!(f.on_text("s"), None);
        assert_eq!(f.on_text("x"), None);
        assert_eq!(f.on_text("/"), Some(Outcome::Consumed));
        assert_eq!(f.on_text("s"), Some(Outcome::FilterChanged), "in search it is query input");
        assert_eq!(f.query(), "s");
    }

    #[test]
    fn esc_in_browsing_clears_toggles_before_leaving_the_panel() {
        let mut f = PanelFilter::new(TOGGLES);
        f.toggle('s');
        assert!(f.is_toggled('s'));

        assert_eq!(f.on_key(egui::Key::Escape), Some(Outcome::FilterChanged));
        assert!(!f.is_toggled('s'));

        assert_eq!(f.on_key(egui::Key::Escape), Some(Outcome::LeavePanel));
    }

    #[test]
    fn unknown_keys_and_text_are_not_consumed_in_browsing() {
        let mut f = PanelFilter::new(TOGGLES);
        assert_eq!(f.on_text("x"), None);
        assert_eq!(f.on_key(egui::Key::ArrowDown), None);
        assert_eq!(f.on_key(egui::Key::Enter), None);
    }

    #[test]
    fn toggle_bits_report_the_set_without_allocating() {
        let mut f = PanelFilter::new(TOGGLES);
        assert_eq!(f.toggle_bits(), 0);

        f.toggle('s');
        assert_eq!(f.toggle_bits(), 0b01, "'s' is index 0 in TOGGLES");

        f.toggle('a');
        assert_eq!(f.toggle_bits(), 0b11);

        f.toggle('s');
        assert_eq!(f.toggle_bits(), 0b10, "'a' alone is index 1");
    }

    #[test]
    fn is_filtering_tracks_query_and_toggles() {
        let mut f = PanelFilter::new(TOGGLES);
        assert!(!f.is_filtering());

        f.toggle('s');
        assert!(f.is_filtering());
        f.toggle('s');
        assert!(!f.is_filtering());

        f.on_text("/");
        f.on_text("x");
        assert!(f.is_filtering());
    }

    #[test]
    fn fuzzy_match_is_subsequence_and_smart_case() {
        let mut f = PanelFilter::new(TOGGLES);
        assert!(f.matches("anything"));

        f.on_text("/");
        f.on_text("fdps");
        assert!(f.matches("fix/diff-pane-scroll"));

        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        f.on_text("readme");
        assert!(f.matches("README.md"));

        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        f.on_text("Read");
        assert!(!f.matches("readme"));
    }

    #[test]
    fn toggles_apply_only_stands_down_for_a_live_query_under_all() {
        use crate::config::SearchScope;
        let mut f = PanelFilter::new(TOGGLES);

        assert!(f.toggles_apply(SearchScope::Filtered));
        assert!(f.toggles_apply(SearchScope::All), "an empty query narrows nothing");

        f.on_text("/");
        f.on_text("foo");
        assert!(f.toggles_apply(SearchScope::Filtered));
        assert!(!f.toggles_apply(SearchScope::All));
    }
}
