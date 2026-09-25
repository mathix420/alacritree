//! Whether a worktree's checkout is still on disk, for the sidebar's benefit.
//!
//! `Project::discover` owns `Checkout::gone` and stays the only writer of
//! it: the delete flow reads that flag to choose between `git worktree remove`
//! and a prune, and `Project::apply` reads it to decide which rows survive a
//! refresh.  This cache never touches it.  It answers one question — "should
//! this row paint as gone?" — so a wrong answer costs a frame of styling and
//! can never pick a destructive branch.  Every action stats the path itself at
//! the moment it runs.
//!
//! Cost is the whole design.  Probing is a syscall per path, and on a
//! `\\wsl.localhost\` UNC path that is a 9P round trip, so probes run on a
//! worker, one batch at a time, only for rows the sidebar is drawing, and only
//! once per interval.  `wants_probe` is what the paint path asks first: on the
//! other ~89 frames of every 90 it is false and nothing here allocates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alacritree_vcs::Liveness;

/// How long a batch of results stands before the visible rows are checked
/// again.  Matches `status_cache::StatusCache`, which answers the same "did this
/// worktree change under us" question at the same human timescale.
const FRESH_FOR: Duration = Duration::from_millis(1500);

/// Probe results keyed by worktree path, plus when the next batch is due.
/// Entries live only as long as the sidebar keeps drawing their path, so a
/// project the user removes does not leave its worktrees behind.
#[derive(Default)]
pub(crate) struct LivenessCache {
    states: HashMap<PathBuf, Liveness>,
    /// The `HEAD` each path last asked for a refresh under, so one that
    /// discovery reads differently, an unborn branch say, asks only once.
    refreshed_heads: HashMap<PathBuf, String>,
    /// `None` until the first batch lands, which is what makes the first
    /// painted frame probe rather than wait out an interval.
    next_probe: Option<Instant>,
}

impl LivenessCache {
    /// Whether `path` is gone, or `None` when no definite answer exists and
    /// the row should keep discovery's word.  A definite answer overrides that
    /// word in *both* directions: a checkout restored under a path discovery
    /// last saw as pruned has to lose the grey, or this fixes one stale
    /// direction and leaves its mirror image behind.
    pub(crate) fn missing(&self, path: &Path) -> Option<bool> {
        match self.states.get(path)? {
            Liveness::Present => Some(false),
            Liveness::Missing => Some(true),
            Liveness::Unknown => None,
        }
    }

    /// Whether the interval has elapsed.  The sidebar asks this *before* it
    /// starts collecting the paths it draws, so a steady frame does no work
    /// and makes no allocation on this path at all.
    pub(crate) fn wants_probe(&self, now: Instant) -> bool {
        self.next_probe.is_none_or(|due| now >= due)
    }

    /// Take the batch to probe, forgetting every path the sidebar no longer
    /// draws — including all of them, when a filter or a collapsed project
    /// leaves nothing eligible.  All visible paths go in together: they are
    /// checked on one worker, so splitting them by individual freshness would
    /// buy nothing.
    pub(crate) fn batch(&mut self, visible: &[PathBuf]) -> Vec<PathBuf> {
        self.states.retain(|path, _| visible.contains(path));
        self.refreshed_heads.retain(|path, _| visible.contains(path));
        visible.to_vec()
    }

    /// Whether `head`, the label just read from `path`, differs from `known`,
    /// the one discovery recorded, under a head no refresh has been asked for
    /// yet.  Allocates only when it answers `true`.
    pub(crate) fn branch_moved(&mut self, path: &Path, head: &str, known: Option<&str>) -> bool {
        if Some(head) == known || self.refreshed_heads.get(path).is_some_and(|h| h == head) {
            return false;
        }
        self.refreshed_heads.insert(path.to_path_buf(), head.to_string());
        true
    }

    /// An `Unknown` result replaces the last answer rather than preserving
    /// it.  Keeping it would leave the row claiming a checkout is gone while
    /// the backend's probe, which has no memory and refuses to call an
    /// unreadable path missing, lets that same path spawn a shell.  Forgetting instead makes
    /// both say "cannot tell" and hands the row back to discovery's word.
    ///
    /// A round that probed nothing still restarts the interval, so a frame
    /// with no eligible rows cannot leave `wants_probe` true forever.
    pub(crate) fn adopt(
        &mut self,
        results: impl IntoIterator<Item = (PathBuf, Liveness)>,
        now: Instant,
    ) {
        for (path, state) in results {
            self.states.insert(path, state);
        }
        self.next_probe = Some(now + FRESH_FOR);
    }

    /// How long until the next batch is due, for `request_repaint_after`, or
    /// `None` when no batch has ever run and there is nothing to wake up for.
    /// Without that wake-up the sidebar would only re-probe when something
    /// else happened to draw a frame, and a worktree deleted from an otherwise
    /// idle terminal would stay marked live indefinitely.  The `Option` is
    /// what keeps "no deadline" from collapsing into a zero wait, which
    /// `request_repaint_after` reads as "repaint now" — every frame, forever.
    pub(crate) fn wait(&self, now: Instant) -> Option<Duration> {
        self.next_probe.map(|due| due.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// An unborn branch has a `HEAD` discovery reads as no branch at all, so
    /// the two never agree.  Asking once is the most that can help.
    #[test]
    fn a_moved_head_asks_for_one_refresh() {
        let mut cache = LivenessCache::default();
        let head = "other";

        assert!(!cache.branch_moved(&p("/a"), head, Some("other")), "discovery agrees");
        assert!(cache.branch_moved(&p("/a"), head, Some("main")));
        assert!(!cache.branch_moved(&p("/a"), head, Some("main")), "already asked");
        assert!(cache.branch_moved(&p("/a"), "third", Some("main")));
    }

    #[test]
    fn a_path_the_sidebar_stopped_drawing_can_ask_again() {
        let mut cache = LivenessCache::default();
        let head = "other";
        assert!(cache.branch_moved(&p("/a"), head, None));

        cache.batch(&[]);

        assert!(cache.branch_moved(&p("/a"), head, None));
    }

    #[test]
    fn the_first_frame_probes_rather_than_waiting_out_an_interval() {
        assert!(LivenessCache::default().wants_probe(Instant::now()));
    }

    #[test]
    fn a_batch_holds_the_interval_shut_until_it_expires() {
        let now = Instant::now();
        let mut cache = LivenessCache::default();
        cache.adopt(vec![(p("/a"), Liveness::Present)], now);

        assert!(!cache.wants_probe(now + FRESH_FOR / 2), "the steady frame does nothing");
        assert!(cache.wants_probe(now + FRESH_FOR));
    }

    #[test]
    fn only_a_definite_answer_greys_the_row() {
        let now = Instant::now();
        let mut cache = LivenessCache::default();

        cache.adopt(vec![(p("/a"), Liveness::Present)], now);
        assert_eq!(cache.missing(&p("/a")), Some(false));

        cache.adopt(vec![(p("/a"), Liveness::Missing)], now);
        assert_eq!(cache.missing(&p("/a")), Some(true));
    }

    /// A distro that stops answering turns every path it owns `Unknown`.  The
    /// row has to stop claiming those checkouts are gone, because `is_gone`
    /// has already stopped refusing to spawn shells in them.
    #[test]
    fn an_unknown_result_forgets_the_last_answer() {
        let now = Instant::now();
        let mut cache = LivenessCache::default();
        cache.adopt(vec![(p("/a"), Liveness::Missing)], now);

        cache.adopt(vec![(p("/a"), Liveness::Unknown)], now + FRESH_FOR);

        assert_eq!(cache.missing(&p("/a")), None, "neither greyed nor vouched for");
        assert!(
            !cache.wants_probe(now + FRESH_FOR),
            "but the failed probe still restarts the tick"
        );
    }

    /// A frame whose rows are all collapsed, filtered away or on WSL probes
    /// nothing.  Leaving the interval open would keep `wait` at zero, and the
    /// caller's `request_repaint_after` would then ask for the next frame on
    /// every frame.
    #[test]
    fn a_round_that_probed_nothing_still_restarts_the_interval() {
        let now = Instant::now();
        let mut cache = LivenessCache::default();

        cache.adopt(Vec::new(), now);

        assert!(!cache.wants_probe(now + FRESH_FOR / 2));
        assert_eq!(cache.wait(now), Some(FRESH_FOR));
    }

    /// `git worktree add` on the same path brings the checkout back; the row
    /// has to lose the grey again rather than stay marked gone.
    #[test]
    fn a_path_that_reappears_goes_back_to_present() {
        let now = Instant::now();
        let mut cache = LivenessCache::default();
        cache.adopt(vec![(p("/a"), Liveness::Missing)], now);

        cache.adopt(vec![(p("/a"), Liveness::Present)], now + FRESH_FOR);

        assert_eq!(cache.missing(&p("/a")), Some(false));
    }

    #[test]
    fn a_path_the_sidebar_stopped_drawing_is_forgotten() {
        let now = Instant::now();
        let mut cache = LivenessCache::default();
        cache.adopt(vec![(p("/a"), Liveness::Missing)], now);

        assert_eq!(cache.batch(&[p("/b")]), vec![p("/b")]);

        assert_eq!(cache.missing(&p("/a")), None, "the entry went with the row");
    }

    #[test]
    fn the_wait_counts_down_to_the_next_batch() {
        let now = Instant::now();
        let mut cache = LivenessCache::default();
        assert_eq!(cache.wait(now), None, "nothing to wake up for before the first batch");

        cache.adopt(vec![(p("/a"), Liveness::Present)], now);

        assert_eq!(cache.wait(now + FRESH_FOR / 2), Some(FRESH_FOR / 2));
        // An overdue tick asks for the next frame, not a negative span.
        assert_eq!(cache.wait(now + FRESH_FOR * 2), Some(Duration::ZERO));
    }
}
