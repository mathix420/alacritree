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
    Activate,
    LeavePanel,
    Consumed,
}

/// Search/toggle state for one sidebar panel.
pub struct PanelFilter {
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

    /// Active toggles in `allowed_toggles` order (render order).
    pub fn active_toggles(&self) -> Vec<char> {
        self.allowed_toggles.iter().copied().filter(|k| self.toggles.contains(k)).collect()
    }

    /// Whether the panel currently narrows its rows: a non-empty query or
    /// any active toggle.
    pub fn is_filtering(&self) -> bool {
        !self.query.is_empty() || !self.toggles.is_empty()
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
                egui::Key::Enter => {
                    self.clear_query();
                    self.mode = Mode::Browsing;
                    Some(Outcome::Activate)
                },
                egui::Key::Escape => {
                    self.clear_query();
                    self.mode = Mode::Browsing;
                    Some(Outcome::FilterChanged)
                },
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
                let mut chars = text.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                if !self.allowed_toggles.contains(&c) {
                    return None;
                }
                if !self.toggles.remove(&c) {
                    self.toggles.insert(c);
                }
                Some(Outcome::FilterChanged)
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
    fn backspace_pops_and_esc_clears_back_to_browsing() {
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        f.on_text("foo");
        assert_eq!(f.on_key(egui::Key::Backspace), Some(Outcome::FilterChanged));
        assert_eq!(f.query(), "fo");

        assert_eq!(f.on_key(egui::Key::Escape), Some(Outcome::FilterChanged));
        assert_eq!(f.mode(), Mode::Browsing);
        assert_eq!(f.query(), "");
    }

    #[test]
    fn enter_in_search_activates_and_clears_the_query() {
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        f.on_text("foo");
        assert_eq!(f.on_key(egui::Key::Enter), Some(Outcome::Activate));
        assert_eq!(f.mode(), Mode::Browsing);
        assert_eq!(f.query(), "");
    }

    #[test]
    fn arrows_in_search_move_the_cursor() {
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("/");
        assert_eq!(f.on_key(egui::Key::ArrowUp), Some(Outcome::MoveCursor(-1)));
        assert_eq!(f.on_key(egui::Key::ArrowDown), Some(Outcome::MoveCursor(1)));
    }

    #[test]
    fn toggle_keys_flip_in_browsing_and_are_inert_in_search() {
        let mut f = PanelFilter::new(TOGGLES);
        assert_eq!(f.on_text("s"), Some(Outcome::FilterChanged));
        assert!(f.is_toggled('s'));
        assert_eq!(f.active_toggles(), vec!['s']);

        assert_eq!(f.on_text("s"), Some(Outcome::FilterChanged));
        assert!(!f.is_toggled('s'));

        f.on_text("/");
        assert_eq!(f.on_text("s"), Some(Outcome::FilterChanged));
        assert_eq!(f.query(), "s");
        assert!(!f.is_toggled('s'));
    }

    #[test]
    fn esc_in_browsing_clears_toggles_before_leaving_the_panel() {
        let mut f = PanelFilter::new(TOGGLES);
        f.on_text("s");
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
    fn is_filtering_tracks_query_and_toggles() {
        let mut f = PanelFilter::new(TOGGLES);
        assert!(!f.is_filtering());

        f.on_text("s");
        assert!(f.is_filtering());
        f.on_text("s");
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
}
