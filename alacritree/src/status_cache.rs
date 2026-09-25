//! The git panel's status for one checkout, refreshed off the UI thread.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alacritree_vcs::{ChangeKind, Dirty, Head, Status, VersionControl};

use crate::jobs;
use crate::repaint::Repaint;
use crate::vcs::Vcs;

const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

/// Long enough that no healthy compute reaches it, short enough that a
/// frozen panel is recorded while the process that froze it is still alive.
const STALL_WARNING: Duration = Duration::from_secs(120);

/// What the panel shows for a compute whose worker unwound. The panic itself
/// is logged from the pool; the row only needs to stop claiming knowledge it
/// does not have.
const WORKER_DIED: &str = "the background worker did not finish";

/// Background-refreshed cache. A status walks the working tree and runs a
/// tree-to-tree diff against the default branch. On a large repo that can
/// take long enough to be felt as a stutter when done on the UI thread, so we
/// spawn the work on a helper thread and let `poll` adopt the result on a
/// later frame. Callers always see the last known status immediately.
pub(crate) struct StatusCache {
    path: PathBuf,
    vcs: Vcs,
    last: Status,
    /// Why the last compute has no status. `last` is then empty.
    last_error: Option<String>,
    last_refreshed: Option<Instant>,
    last_hint: Option<String>,
    pending: Option<Pending>,
}

struct Pending {
    /// Hint the in-flight compute was started with, so we can tell whether
    /// the result that lands matches what the UI is currently asking for.
    hint: Option<String>,
    job: jobs::Job<Result<Status, String>>,
    /// When the compute was spawned, so a caller can tell a slow one from
    /// one that will never answer.
    started: Instant,
    /// Set once the stall warning has been logged, so a frozen panel
    /// repainting at monitor rate records the freeze once rather than on
    /// every frame.
    warned: bool,
}

impl StatusCache {
    pub(crate) fn new(path: PathBuf, vcs: Vcs) -> Self {
        Self {
            path,
            vcs,
            last: Status::default(),
            last_error: None,
            last_refreshed: None,
            last_hint: None,
            pending: None,
        }
    }

    /// Last head we read, for callers that need it before triggering a new
    /// poll (e.g. the PR cache wants the branch name to query `gh`). `None`
    /// until a compute has read one.
    pub(crate) fn live_head(&self) -> Option<&Head> {
        self.last.head.label().is_some().then_some(&self.last.head)
    }

    /// The most recent known status without triggering a refresh, for callers
    /// that need to re-derive rows between polls (e.g. re-filtering on a
    /// keystroke).
    pub(crate) fn last(&self) -> &Status {
        &self.last
    }

    /// Why the last compute produced no status, if it failed.
    pub(crate) fn error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// How long the in-flight compute has been running, or `None` when
    /// nothing is in flight. A compute that never returns pins `pending`,
    /// and `poll` will not spawn another while it does, so the panel keeps
    /// rendering whatever it last held.
    pub(crate) fn stalled_for(&self) -> Option<Duration> {
        self.pending.as_ref().map(|pending| pending.started.elapsed())
    }

    /// Whether a compute has landed and actually knows the tree. A cache
    /// entry exists the moment the git panel first renders a workspace,
    /// before its first background compute finishes, and `last()` answers
    /// `Status::default()` (all-zero counts) until then, which callers
    /// must not read as "known clean". A compute that landed but failed
    /// (`error()` is `Some`, e.g. the repository could not be opened) is the
    /// same "don't know" case, as is a compute whose worker unwound, which
    /// is banked the same way. Both still set `last_refreshed` so `poll`
    /// doesn't retry every frame, but both answer `false` here too.
    pub(crate) fn has_status(&self) -> bool {
        self.last_refreshed.is_some() && self.last_error.is_none()
    }

    /// Returns the most recent known status, kicking off a background refresh
    /// when stale or when the default-branch hint changed since the last
    /// completed compute. Never blocks the caller.
    pub(crate) fn poll(
        &mut self,
        default_branch_hint: Option<&str>,
        repaint: &impl Repaint,
    ) -> &Status {
        // Drain any completed background result before deciding whether to
        // spawn another. A fresh answer shouldn't be ignored just because
        // the staleness timer also tripped.
        if let Some(pending) = &self.pending {
            if let Some(result) = pending.job.poll() {
                (self.last, self.last_error) = match result {
                    Ok(status) => (status, None),
                    Err(e) => (Status::default(), Some(e)),
                };
                self.last_refreshed = Some(Instant::now());
                self.last_hint = pending.hint.clone();
                self.pending = None;
            } else if pending.job.failed() {
                // A panicked compute reports no status, and merely forgetting
                // it leaves the cache looking never-refreshed: the next poll
                // starts another, and the pool wakes a frame at every job end,
                // so a compute that fails every time would respawn at frame
                // rate. Bank it as the failure it is, on the clock a landed
                // error already uses, and the retry lands one interval later
                // like any other.
                self.last = Status::default();
                self.last_error = Some(WORKER_DIED.to_string());
                self.last_refreshed = Some(Instant::now());
                self.last_hint = pending.hint.clone();
                self.pending = None;
            }
        }

        // Nothing healthy takes this long: the resident transport caps a
        // request and the fallback is a single wsl.exe round trip. Past it
        // the panel is frozen on a stale answer rather than waiting on a
        // slow one, and that difference is invisible from outside.
        if let Some(stalled) = self.stalled_for() {
            if stalled > STALL_WARNING {
                if let Some(pending) = self.pending.as_mut() {
                    if !pending.warned {
                        pending.warned = true;
                        log::warn!(
                            "git status for {} has been computing for {:.0}s; the panel is \
                             showing a stale result",
                            self.path.display(),
                            stalled.as_secs_f64()
                        );
                    }
                }
            }
        }

        let hint_changed = self.last_hint.as_deref() != default_branch_hint;
        let stale = self.last_refreshed.map_or(true, |when| when.elapsed() > REFRESH_INTERVAL);
        let needs_refresh = self.last_refreshed.is_none() || hint_changed || stale;

        if needs_refresh && self.pending.is_none() {
            self.pending = Some(spawn_compute(
                self.path.clone(),
                self.vcs.clone(),
                default_branch_hint.map(str::to_string),
                repaint.clone(),
            ));
        }

        &self.last
    }
}

/// The delete dialog's counts from a status the git panel already polled, so
/// opening the dialog costs no repository walk.
pub(crate) fn dirty_of(status: &Status) -> Dirty {
    let untracked = status.working.iter().filter(|c| c.kind == ChangeKind::Untracked).count();
    let staged = status.staged.as_ref().map_or(0, Vec::len);
    Dirty { staged, modified: status.working.len() - untracked, untracked }
}

fn spawn_compute(path: PathBuf, vcs: Vcs, hint: Option<String>, repaint: impl Repaint) -> Pending {
    let worker_hint = hint.clone();
    let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
        let status = vcs.status(&path, worker_hint.as_deref(), blocking).map_err(|e| e.to_string());
        repaint.wake();
        status
    });
    Pending { hint, job, started: Instant::now(), warned: false }
}

#[cfg(test)]
mod tests {
    use alacritree_vcs::FileChange;

    use super::*;

    #[test]
    fn dirty_counts_come_from_a_status_the_panel_already_has() {
        let status = Status {
            head: Head { name: Some("main".into()), ..Head::default() },
            staged: Some(vec![FileChange { path: "a".into(), kind: ChangeKind::Added }]),
            working: vec![
                FileChange { path: "b".into(), kind: ChangeKind::Modified },
                FileChange { path: "c".into(), kind: ChangeKind::Untracked },
                FileChange { path: "d".into(), kind: ChangeKind::Untracked },
            ],
            ..Status::default()
        };
        let dirty = dirty_of(&status);
        assert_eq!(dirty, Dirty { staged: 1, modified: 1, untracked: 2 });
        assert!(dirty.is_dirty());
    }
    use crate::repaint::Recorder;

    fn git() -> Vcs {
        Vcs::Git(alacritree_git::GitBackend)
    }

    /// A compute that panics. Interactive, since a background job runs below
    /// normal priority and a saturated machine can leave it unscheduled for
    /// seconds, while what these tests check is only how the cache takes a
    /// failure.
    fn panicking_compute() -> jobs::Job<Result<Status, String>> {
        jobs::pool()
            .spawn(jobs::Priority::Interactive, |_: &jobs::Blocking| -> Result<Status, String> {
                panic!("boom")
            })
    }

    #[test]
    fn a_status_poll_reports_without_blocking_its_caller() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A bare `Repository::init` leaves HEAD unborn (no commit for it to
        // point at), and `compute` never reports a branch for that; give it
        // one so the background result has a branch to land.
        alacritree_git::test_support::init_repo(dir.path());

        let repaint = Recorder::default();
        let mut cache = StatusCache::new(dir.path().to_path_buf(), git());
        // The first poll has nothing banked and must return anyway.
        let started = Instant::now();
        let _ = cache.poll(None, &repaint);
        assert!(started.elapsed() < Duration::from_millis(50), "poll blocked its caller");

        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.live_head().is_none() && Instant::now() < deadline {
            let _ = cache.poll(None, &repaint);
            std::thread::yield_now();
        }
        assert!(cache.live_head().is_some(), "the background compute never landed");
        assert_eq!(repaint.wakes(), 1, "the landed compute should wake the UI");
    }

    /// A panicked compute must not wedge the cache: without clearing
    /// `pending` on `Job::failed`, `needs_refresh && self.pending.is_none()`
    /// would refuse every future refresh for this worktree.
    #[test]
    fn a_failed_compute_clears_pending_so_a_future_poll_is_not_blocked() {
        let mut cache = StatusCache::new(PathBuf::from("/doesnt/matter"), git());
        let job = panicking_compute();
        cache.pending = Some(Pending { hint: None, job, started: Instant::now(), warned: false });

        let repaint = Recorder::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.pending.is_some() {
            let _ = cache.poll(None, &repaint);
            assert!(Instant::now() < deadline, "pending was never cleared after the job failed");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_compute_that_never_answers_is_reported_as_stalled() {
        let mut cache = StatusCache::new(PathBuf::from("/nonexistent"), git());
        assert_eq!(cache.stalled_for(), None, "nothing in flight yet");

        // A gated worker rather than one that parks forever: the test drops
        // the sender before returning, so the worker exits instead of
        // costing one of the pool's fixed slots for the rest of the process.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let job = jobs::pool().spawn(
            jobs::Priority::Background,
            move |_: &jobs::Blocking| -> Result<Status, String> {
                let _ = release_rx.recv();
                Ok(Status::default())
            },
        );

        // Backdated past STALL_WARNING rather than slept past it, so the
        // warn-once assertions below need no sleep of their own.
        let started = Instant::now()
            .checked_sub(STALL_WARNING + Duration::from_secs(1))
            .expect("the process has not been up for STALL_WARNING yet");
        cache.pending = Some(Pending { hint: None, job, started, warned: false });

        let stalled = cache.stalled_for().expect("a held compute is in flight");
        assert!(stalled > STALL_WARNING);

        let repaint = Recorder::default();
        let pending_warned =
            |cache: &StatusCache| cache.pending.as_ref().expect("still in flight").warned;
        assert!(!pending_warned(&cache), "not warned before the first poll");
        let _ = cache.poll(None, &repaint);
        assert!(pending_warned(&cache), "a stall past STALL_WARNING must be logged");
        let _ = cache.poll(None, &repaint);
        assert!(pending_warned(&cache), "the warning must not repeat on every frame");

        let _ = release_tx.send(());
    }

    /// The regression this guards: a failure that leaves the cache looking
    /// never-refreshed is spawned again by the very next poll, and the pool
    /// wakes a frame at every job end, so a compute that panics every time
    /// would respawn at frame rate, burning a worker for as long as the panel
    /// is open. A compute that fails must not be retried more often than one
    /// that succeeds.
    #[test]
    fn a_failed_compute_backs_off_as_far_as_a_successful_one() {
        let job = panicking_compute();
        // Latch the failure before the cache sees it, so the poll below reads
        // a settled job rather than racing the worker.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.failed() {
            assert!(job.poll().is_none(), "a panicking job never reports a value");
            assert!(Instant::now() < deadline, "the failure was never observed");
            std::thread::yield_now();
        }

        let mut cache = StatusCache::new(PathBuf::from("/doesnt/matter"), git());
        cache.pending = Some(Pending { hint: None, job, started: Instant::now(), warned: false });
        let repaint = Recorder::default();

        let _ = cache.poll(None, &repaint);
        assert!(cache.pending.is_none(), "the poll that banks a failure must not start another");
        assert!(!cache.has_status(), "a compute that failed knows nothing about the tree");
        let _ = cache.poll(None, &repaint);
        assert!(cache.pending.is_none(), "nor may the frames that follow it inside the interval");
    }

    /// The regression this guards: a cache entry exists from the moment the
    /// git panel first renders a workspace, before its first compute lands
    /// -- `has_status` must read `false` for that entry so a caller deciding
    /// whether to trust `last()` doesn't mistake "never checked" for "known
    /// clean" (an all-zero `Status::default()`).
    #[test]
    fn has_status_is_false_until_a_compute_lands() {
        let dir = tempfile::tempdir().expect("a temp dir");
        alacritree_git::test_support::init_repo(dir.path());

        let repaint = Recorder::default();
        let mut cache = StatusCache::new(dir.path().to_path_buf(), git());
        assert!(!cache.has_status(), "a fresh cache has never completed a compute");

        let deadline = Instant::now() + Duration::from_secs(10);
        while !cache.has_status() && Instant::now() < deadline {
            let _ = cache.poll(None, &repaint);
            std::thread::yield_now();
        }
        assert!(cache.has_status(), "the background compute never landed");
    }

    /// The regression this guards: a compute that lands but fails to open
    /// the repository still sets `last_refreshed` (so `poll` doesn't retry
    /// every frame), which must not let `has_status` read it as a known,
    /// clean tree -- a caller deciding whether to force a destructive action
    /// needs "don't know" to stay "don't know" through this path too.
    #[test]
    fn has_status_is_false_for_an_errored_compute() {
        // Not a git repository, so `compute` lands an error rather than a
        // status.
        let dir = tempfile::tempdir().expect("a temp dir");
        let repaint = Recorder::default();
        let mut cache = StatusCache::new(dir.path().to_path_buf(), git());

        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.error().is_none() && Instant::now() < deadline {
            let _ = cache.poll(None, &repaint);
            std::thread::yield_now();
        }
        assert!(cache.error().is_some(), "the background compute never landed an error");
        assert!(!cache.has_status(), "an errored compute must not read as a known status");
    }
}
