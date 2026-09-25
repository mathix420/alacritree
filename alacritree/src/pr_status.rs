//! Detect whether the current branch has a PR on the repository's forge, and
//! cache its base branch so the sidebar diff can target the PR's base instead
//! of the repo's default branch. The lookup is best-effort: a forge that
//! fails or finds nothing leaves the default branch in place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alacritree_forge::{Head, PrInfo, PrState, PullRequests, RemoteForge};

use crate::jobs;
use crate::projects::Worktree;
use crate::repaint::Repaint;

/// Re-query at most this often. PR base branches rarely change, and a stale
/// answer just falls back to the previous diff target, which is not worth
/// hammering the forge on every status refresh.
const TTL: Duration = Duration::from_secs(300);

/// How many bursts may run at once: what the config asks for, never above one
/// below the pool's background ceiling.
///
/// A ceiling rather than a limit that binds today. One frame's whole due list
/// becomes a single job, and every entry it covers stays `pending` until that
/// job settles, so nothing new falls due meanwhile: one request is in flight in
/// steady state and two across a handover, under any cap this returns. It
/// stays because the shape it guards against is cheap to reintroduce, since a
/// spawn per group would put a project's repositories on the pool at once, and
/// because reserving a slot below the ceiling is what leaves a worker for the
/// local work sharing the pool, at any pool size.
fn effective_cap(configured: Option<usize>, ceiling: usize) -> usize {
    configured.unwrap_or(usize::MAX).min(ceiling.saturating_sub(1)).max(1)
}

pub(crate) struct PrCache<F> {
    forge: F,
    entries: HashMap<PathBuf, Entry>,
    /// Requests in flight. `in_flight` counts these rather than branches:
    /// what a burst costs follows the repositories it spans, not the number
    /// of branches waiting on it.
    batches: Vec<Batch>,
    /// Entries that asked for a lookup this frame, handed to the forge by the
    /// next `drain_completed`. Batching needs a whole frame's worth of due
    /// entries before it can group them, which one `poll` call cannot see.
    due: Vec<Head>,
    in_flight: usize,
    concurrency: usize,
    generation: u64,
    /// Elapsed since this cache was built. A `Duration` rather than an
    /// `Instant` because an `Instant` cannot be constructed or advanced, so
    /// nothing could set one to test a boundary against.
    clock: Box<dyn Fn() -> Duration + Send>,
}

#[derive(Default)]
struct Entry {
    /// Branch the cached result was queried for. Switching branches in the
    /// same worktree invalidates the entry.
    branch: Option<String>,
    info: Option<PrInfo>,
    queried_at: Option<Duration>,
    /// Set from the moment this entry joins the due list until its answer is
    /// banked. `should_spawn` reads it to avoid asking twice for one badge,
    /// so it has to cover the queued frame as well as the running one.
    pending: bool,
    /// A refresh landed while `pending` was already occupied. The drain
    /// leaves `queried_at` cleared instead of stamping the fresh lookup's
    /// result as current, so the next poll re-queries.
    refresh_requested: bool,
}

/// One request in flight, and every entry waiting on it. A job that never
/// reports would otherwise hold its concurrency slot forever: a panicked one
/// reports through `Job::failed` immediately, a merely slow one is backed off
/// once it has been in flight past the TTL.
struct Batch {
    job: jobs::Job<PullRequests>,
    started: Duration,
    members: Vec<Head>,
}

impl<F: RemoteForge + Clone + Send + 'static> PrCache<F> {
    pub(crate) fn new(forge: F) -> Self {
        let origin = Instant::now();
        Self::with_clock(forge, move || origin.elapsed())
    }

    pub(crate) fn with_clock(forge: F, clock: impl Fn() -> Duration + Send + 'static) -> Self {
        Self {
            forge,
            entries: HashMap::new(),
            batches: Vec::new(),
            due: Vec::new(),
            in_flight: 0,
            concurrency: effective_cap(None, jobs::pool().background_ceiling()),
            generation: 0,
            clock: Box::new(clock),
        }
    }

    fn now(&self) -> Duration {
        (self.clock)()
    }

    /// The state of a cached lookup, without starting or refreshing one.
    /// `None` unless the entry was queried for `branch`: an entry is keyed by
    /// path but only ever valid for one branch, so a caller reading it under a
    /// different branch would be reading the previous branch's PR.
    pub(crate) fn state(&self, path: &Path, branch: Option<&str>) -> Option<PrState> {
        let entry = self.entries.get(path)?;
        if entry.branch.as_deref() != branch {
            return None;
        }
        entry.info.as_ref().map(|i| i.state)
    }

    /// Returns the PR info known for `(path, branch)` right now, kicking off
    /// a background refresh if the cache is stale or branch-mismatched.
    /// Never blocks. The caller sees the previous value (or `None`) until the
    /// worker finishes and the next frame picks up the result.
    pub(crate) fn poll(
        &mut self,
        path: &Path,
        branch: Option<&str>,
        repaint: &impl Repaint,
    ) -> Option<PrInfo> {
        let now = self.now();
        let entry = self.entries.entry(path.to_path_buf()).or_default();

        // A `None` poll (the git-status compute hasn't produced a branch
        // yet, or never will) carries no information about the current
        // branch, so it must not evict or refresh a lookup keyed to a real
        // one from another caller. It just reads whatever is cached.
        let Some(branch) = branch else {
            return entry.info.clone();
        };

        let spawn = should_spawn(
            entry.branch.as_deref(),
            Some(branch),
            entry.queried_at,
            entry.pending,
            now,
        );

        if spawn {
            // Clear stale data immediately on branch switch so we don't show
            // a PR base that belongs to a different branch.
            if should_invalidate(entry.branch.as_deref(), Some(branch)) {
                entry.info = None;
            }
            entry.branch = Some(branch.to_string());
            entry.pending = true;
            self.due.push(Head { path: path.to_path_buf(), branch: branch.to_string() });
            // The frame that queues a lookup is not the frame that starts one,
            // the next drain is, and egui paints on demand. Without asking
            // for that frame the request waits on the user's next input
            // instead of on the TTL.
            //
            // Only while a slot is free, though: over the cap the drain
            // refuses the member and leaves it due, so an unconditional ask
            // would repaint at frame rate for as long as the batch runs. The
            // guard inside the spawn closure delivers that wake when a slot
            // frees, on the panicking path too.
            if may_spawn(self.concurrency, self.in_flight) {
                repaint.wake();
            }
        }

        self.entries.get(path).and_then(|entry| entry.info.clone())
    }

    /// Advances whenever what `state` would answer may have moved. The sidebar
    /// reconciler compares it to know a filtered row set needs rebuilding; a
    /// banked result that happens to match the previous one costs one extra
    /// rebuild, which is cheaper than diffing states to avoid it.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The cap on lookups in flight at once: `configured` if given, else the
    /// pool decides. Either way it never exceeds the pool's own background
    /// ceiling, so a cold cache can't fork one forge process per eligible
    /// worktree and starve the local work sharing the pool.
    pub(crate) fn set_concurrency(&mut self, configured: Option<usize>) {
        self.concurrency = effective_cap(configured, jobs::pool().background_ceiling());
    }

    /// Bank every finished request and free its slot, then turn the frame's
    /// due list into new requests. Runs once a frame ahead of every poll
    /// site rather than inside `poll`: an entry whose project collapsed
    /// mid-lookup is never polled again, and a slot it still held would never
    /// come back.
    pub(crate) fn drain_completed(&mut self, repaint: &impl Repaint) {
        let now = self.now();
        let mut banked = false;
        let mut still_running = Vec::new();
        for batch in std::mem::take(&mut self.batches) {
            if let Some(found) = batch.job.poll() {
                for m in &batch.members {
                    self.settle(m, answer(&found, m), now);
                }
                banked = true;
            } else if batch.job.failed() || now.saturating_sub(batch.started) > TTL {
                // A request that never reports has no answer to bank, but its
                // members must still be stamped: leaving them due re-spawns a
                // lookup every frame for as long as the failure lasts.
                for m in &batch.members {
                    self.back_off(m, now);
                }
            } else {
                still_running.push(batch);
                continue;
            }
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.batches = still_running;
        if banked {
            self.generation = self.generation.wrapping_add(1);
        }
        self.spawn_due(repaint);
    }

    /// Record one member's answer. `None` means the request covered this
    /// branch and found no PR, which is a real answer and gets stamped.
    fn settle(&mut self, m: &Head, info: Option<PrInfo>, now: Duration) {
        let entry = self.entries.entry(m.path.clone()).or_default();
        entry.branch = Some(m.branch.clone());
        entry.info = info;
        // A refresh that arrived mid-request wants the *next* answer, so
        // leave the entry stale and let the next poll re-query.
        entry.queried_at = if entry.refresh_requested { None } else { Some(now) };
        entry.refresh_requested = false;
        entry.pending = false;
    }

    /// Stamp a member whose request produced nothing, keeping its previous
    /// answer on screen and holding it off for a TTL.
    fn back_off(&mut self, m: &Head, now: Duration) {
        let entry = self.entries.entry(m.path.clone()).or_default();
        entry.queried_at = Some(now);
        entry.refresh_requested = false;
        entry.pending = false;
    }

    /// Hand the frame's due list to one worker. The frame only decides whether
    /// there is room to ask; how the list is grouped into requests is the
    /// forge's business, and may need blocking reads to decide.
    ///
    /// Over the cap the list is dropped, and every member is returned to the
    /// state it was polled in, not just `pending` cleared. `poll` has already
    /// written the new branch, so on a branch switch the stamp is the only
    /// thing left saying the entry is stale; keeping it would read as a fresh
    /// answer for a branch nothing ever looked up.
    fn spawn_due(&mut self, repaint: &impl Repaint) {
        let due = std::mem::take(&mut self.due);
        if due.is_empty() {
            return;
        }
        if !may_spawn(self.concurrency, self.in_flight) {
            for m in &due {
                if let Some(entry) = self.entries.get_mut(&m.path) {
                    entry.pending = false;
                    entry.queried_at = None;
                }
            }
            return;
        }
        let members = due.clone();
        let repaint = repaint.clone();
        let forge = self.forge.clone();
        let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            // Fires on a panicking unwind too, since it's a local: the drain
            // that frees this slot only runs on a frame, so an exit without a
            // repaint can stall polling for good.
            let _wake = WakeOnDrop(repaint);
            forge.pull_requests(due, blocking)
        });
        self.bank_batch(members, job);
    }

    /// Mark every entry stale. Entries with a lookup already running also get
    /// `refresh_requested`, because clearing `queried_at` alone cannot reach
    /// them: `poll` will not spawn while `pending` is occupied, and the drain
    /// would stamp a fresh timestamp over the request.
    pub(crate) fn invalidate_all(&mut self) {
        for entry in self.entries.values_mut() {
            entry.queried_at = None;
            if entry.pending {
                entry.refresh_requested = true;
            }
        }
        self.generation = self.generation.wrapping_add(1);
    }

    /// Record a started request against every entry it covers. Each entry is
    /// keyed to the branch being asked about rather than to the last banked
    /// answer: a worker that dies without sending leaves nothing for the drain
    /// to key it with, and a mismatched branch makes the entry due again on the
    /// next frame however recently it was queried.
    fn bank_batch(&mut self, members: Vec<Head>, job: jobs::Job<PullRequests>) {
        let started = self.now();
        for m in &members {
            let entry = self.entries.entry(m.path.clone()).or_default();
            entry.branch = Some(m.branch.clone());
            entry.pending = true;
        }
        self.batches.push(Batch { job, started, members });
        self.in_flight += 1;
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.in_flight
    }

    #[cfg(test)]
    fn is_due(&self, path: &Path, branch: &str) -> bool {
        let now = self.now();
        self.entries.get(path).is_none_or(|e| {
            should_spawn(e.branch.as_deref(), Some(branch), e.queried_at, e.pending, now)
        })
    }
}

/// A lookup that failed reads as no PR, the way it always has when `gh` is
/// missing or unauthenticated, so the diff falls back to the default branch.
fn answer(found: &PullRequests, m: &Head) -> Option<PrInfo> {
    match found.get(&m.path)? {
        Ok(info) => info.clone(),
        Err(e) => {
            log::debug!("PR lookup for {} in {} failed: {e}", m.branch, m.path.display());
            None
        },
    }
}

/// Whether another lookup may start.
fn may_spawn(concurrency: usize, in_flight: usize) -> bool {
    in_flight < concurrency
}

/// Whether this entry is due for a lookup, ignoring the concurrency cap.
fn should_spawn(
    cached_branch: Option<&str>,
    branch: Option<&str>,
    queried_at: Option<Duration>,
    pending: bool,
    now: Duration,
) -> bool {
    if pending {
        return false;
    }
    let invalidate = should_invalidate(cached_branch, branch);
    let fresh = queried_at.is_some_and(|when| now.saturating_sub(when) < TTL);
    invalidate || !fresh
}

/// A `None` incoming branch never invalidates, since the caller has nothing
/// to compare against. A `Some` branch that disagrees with the cached one
/// means a real branch switch and must invalidate.
fn should_invalidate(cached_branch: Option<&str>, incoming_branch: Option<&str>) -> bool {
    match incoming_branch {
        None => false,
        Some(_) => cached_branch != incoming_branch,
    }
}

/// Whether a worktree in `state` survives the projects panel's PR dimension.
/// The active states union; with none active every worktree passes. An unknown
/// state, whether no lookup yet, no PR, or no `gh`, satisfies no active toggle.
pub(crate) fn pr_pass(
    state: Option<PrState>,
    open: bool,
    draft: bool,
    merged: bool,
    closed: bool,
) -> bool {
    if !(open || draft || merged || closed) {
        return true;
    }
    match state {
        None => false,
        Some(PrState::Open) => open,
        Some(PrState::Draft) => draft,
        Some(PrState::Merged) => merged,
        Some(PrState::Closed) => closed,
    }
}

/// The branch a worktree's PR lookup is keyed to. The active worktree prefers
/// its live status branch; every other worktree, and an active one whose
/// `StatusCache` has not produced a branch yet, uses the stored snapshot.
///
/// The split is what keeps two pollers of one path from fighting. [`PrCache`]
/// is keyed by path alone, so the right sidebar, which polls the active
/// workspace with its live `StatusCache` branch recomputed every ~1.5 s, and
/// the projects sidebar must agree on a branch, or each drain flips
/// `entry.branch` and they invalidate each other's lookups forever after an
/// in-terminal checkout. Every other worktree has a single poller, and an
/// inactive workspace's `StatusCache` is created once and then never re-polled
/// or pruned: reading it would freeze the branch at whatever it was on the last
/// visit and shadow later `refresh_project` updates to `wt.branch`.
pub(crate) fn effective_branch<'a>(
    wt: &'a Worktree,
    current_workspace: Option<&Path>,
    live_branch: Option<&'a str>,
) -> Option<&'a str> {
    if current_workspace == Some(wt.path.as_path()) {
        live_branch.or(wt.branch.as_deref())
    } else {
        wt.branch.as_deref()
    }
}

struct WakeOnDrop<R: Repaint>(R);

impl<R: Repaint> Drop for WakeOnDrop<R> {
    fn drop(&mut self) {
        self.0.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    use alacritree_forge::fake::FakeForge;

    use crate::repaint::Recorder;

    fn cache() -> PrCache<FakeForge> {
        PrCache::new(FakeForge::default())
    }

    fn cache_with_clock(now: &Arc<Mutex<Duration>>) -> PrCache<FakeForge> {
        let reader = Arc::clone(now);
        PrCache::with_clock(FakeForge::default(), move || *reader.lock().expect("clock poisoned"))
    }

    /// Spawn a job that blocks until `release` fires, so a test can hold a
    /// lookup pending for as long as it needs. Dropping `release` unblocks
    /// it too, which is what reclaims the slot once a test is done with it.
    ///
    /// Runs on a throwaway pool rather than the process-wide `jobs::pool()`:
    /// this deliberately wedges a background slot, and the shared pool is a
    /// handful of workers other test binaries in this crate poll against
    /// with their own deadlines. A pool built just for this call can never
    /// starve them, or be starved by them.
    fn spawn_stuck_job() -> (mpsc::Sender<()>, jobs::Job<PullRequests>) {
        let (release, gate) = mpsc::channel::<()>();
        let job = jobs::Pool::new(2).spawn(jobs::Priority::Background, move |_| {
            let _ = gate.recv();
            PullRequests::new()
        });
        (release, job)
    }

    /// Bank `job` as the request covering `(path, branch)`, the shape `poll`
    /// and the drain produce for a single due worktree.
    fn bank_one<F: RemoteForge + Clone + Send + 'static>(
        cache: &mut PrCache<F>,
        path: &str,
        branch: &str,
        job: jobs::Job<PullRequests>,
    ) {
        cache.bank_batch(vec![Head { path: PathBuf::from(path), branch: branch.to_string() }], job);
    }

    /// Wire a stuck request into `cache` as if it had been in flight since
    /// `started`, for tests that need to force `drain_completed`'s TTL branch
    /// without waiting out the real TTL. `bank_batch` stamps `started` from
    /// the cache's own clock, so the batch is assembled by hand instead.
    fn insert_stuck_entry(
        cache: &mut PrCache<FakeForge>,
        path: &Path,
        branch: &str,
        started: Duration,
    ) -> mpsc::Sender<()> {
        let (release, job) = spawn_stuck_job();
        let member = Head { path: path.to_path_buf(), branch: branch.to_string() };
        cache.entries.insert(path.to_path_buf(), Entry {
            branch: Some(branch.to_string()),
            pending: true,
            ..Default::default()
        });
        cache.batches.push(Batch { job, started, members: vec![member] });
        cache.in_flight += 1;
        release
    }

    /// Drive `drain_completed` until the entry at `path` has no request
    /// outstanding, mirroring how the UI's frame loop drives it.
    fn drain_until(cache: &mut PrCache<FakeForge>, path: &Path, timeout: Duration) {
        let repaint = Recorder::default();
        let deadline = Instant::now() + timeout;
        loop {
            cache.drain_completed(&repaint);
            if cache.entries.get(path).is_none_or(|e| !e.pending) {
                return;
            }
            assert!(Instant::now() < deadline, "the lookup never landed");
            thread::yield_now();
        }
    }

    fn sample_info() -> PrInfo {
        PrInfo {
            number: 7,
            base_branch: "main".to_string(),
            url: "https://github.com/o/r/pull/7".to_string(),
            state: PrState::Open,
        }
    }

    /// A poll that falls due reaches the forge on the next drain, and the
    /// forge's answer is what the following poll reads.
    #[test]
    fn a_due_poll_asks_the_forge_and_banks_its_answer() {
        let forge = FakeForge::default().with_pr("topic", sample_info());
        let mut cache = PrCache::new(forge.clone());
        let path = Path::new("/repo/wt");
        let repaint = Recorder::default();

        assert_eq!(cache.poll(path, Some("topic"), &repaint), None);
        drain_until(&mut cache, path, Duration::from_secs(5));

        assert_eq!(forge.calls(), [vec![Head { path: path.into(), branch: "topic".into() }]]);
        assert_eq!(cache.poll(path, Some("topic"), &repaint), Some(sample_info()));
        assert_eq!(forge.calls().len(), 1, "a banked answer is fresh for a TTL");
    }

    /// A lookup the forge reports as failed clears the badge and holds off a
    /// TTL, as a missing or unauthenticated `gh` always has.
    #[test]
    fn a_failed_lookup_reads_as_no_pr_until_the_ttl() {
        let forge = FakeForge::default().failing_on("topic");
        let mut cache = PrCache::new(forge);
        let path = Path::new("/repo/wt");
        let repaint = Recorder::default();
        cache.entries.insert(path.to_path_buf(), Entry {
            branch: Some("topic".into()),
            info: Some(sample_info()),
            ..Entry::default()
        });

        cache.poll(path, Some("topic"), &repaint);
        drain_until(&mut cache, path, Duration::from_secs(5));

        assert_eq!(cache.state(path, Some("topic")), None);
        assert!(!cache.is_due(path, "topic"), "a failure is stamped, not retried every frame");
    }

    #[test]
    fn none_branch_does_not_invalidate_a_cached_branch() {
        assert!(!should_invalidate(Some("b"), None));
    }

    #[test]
    fn mismatched_branch_invalidates() {
        assert!(should_invalidate(Some("b"), Some("a")));
    }

    #[test]
    fn matching_branch_does_not_invalidate() {
        assert!(!should_invalidate(Some("b"), Some("b")));
    }

    #[test]
    fn polling_with_none_retains_info_from_a_completed_some_branch_lookup() {
        let mut cache = cache();
        let path = PathBuf::from("/repo");
        cache.entries.insert(path.clone(), Entry {
            branch: Some("b".to_string()),
            info: Some(sample_info()),
            queried_at: Some(Duration::ZERO),
            pending: false,
            refresh_requested: false,
        });

        let repaint = Recorder::default();
        let result = cache.poll(&path, None, &repaint);

        assert_eq!(result.map(|info| info.number), Some(7));
        let entry = cache.entries.get(&path).unwrap();
        assert_eq!(entry.branch.as_deref(), Some("b"));
        assert!(entry.info.is_some(), "None poll must not clear the cached info");
        assert!(!entry.pending, "None poll must not queue a competing lookup");
    }

    fn worktree(path: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            name: String::new(),
            path: PathBuf::from(path),
            branch: branch.map(String::from),
            is_main: false,
            prunable: false,
            upstream: None,
        }
    }

    #[test]
    fn no_active_pr_toggle_passes_every_state() {
        for state in [
            None,
            Some(PrState::Open),
            Some(PrState::Draft),
            Some(PrState::Merged),
            Some(PrState::Closed),
        ] {
            assert!(pr_pass(state, false, false, false, false), "{state:?}");
        }
    }

    #[test]
    fn an_active_pr_toggle_admits_only_its_own_state() {
        assert!(pr_pass(Some(PrState::Open), true, false, false, false));
        assert!(!pr_pass(Some(PrState::Draft), true, false, false, false));
        assert!(!pr_pass(Some(PrState::Merged), true, false, false, false));
    }

    #[test]
    fn pr_toggles_union_within_the_dimension() {
        for state in [PrState::Open, PrState::Draft] {
            assert!(pr_pass(Some(state), true, true, false, false), "{state:?}");
        }
        assert!(!pr_pass(Some(PrState::Closed), true, true, false, false));
    }

    /// No lookup yet, no PR, or no `gh` are indistinguishable here, and none of
    /// them is evidence a worktree belongs in a PR-filtered list.
    #[test]
    fn an_unknown_state_never_satisfies_an_active_toggle() {
        assert!(!pr_pass(None, true, false, false, false));
        assert!(!pr_pass(None, true, true, true, true));
    }

    #[test]
    fn effective_branch_prefers_the_live_branch_for_the_active_worktree() {
        let wt = worktree("/repo/wt", Some("stored"));
        let active = Some(Path::new("/repo/wt"));
        assert_eq!(effective_branch(&wt, active, Some("live")), Some("live"));
    }

    /// A workspace that just became active has a fresh `StatusCache` with no
    /// branch yet; falling back to the stored one is what stops a valid cached
    /// lookup from reading as unknown for a frame.
    #[test]
    fn effective_branch_falls_back_to_the_stored_branch() {
        let wt = worktree("/repo/wt", Some("stored"));
        let active = Some(Path::new("/repo/wt"));
        assert_eq!(effective_branch(&wt, active, None), Some("stored"));
    }

    #[test]
    fn effective_branch_ignores_a_live_branch_from_another_workspace() {
        let wt = worktree("/repo/wt", Some("stored"));
        let active = Some(Path::new("/repo/other"));
        assert_eq!(effective_branch(&wt, active, Some("live")), Some("stored"));
    }

    #[test]
    fn state_is_none_for_a_branch_the_entry_was_not_queried_for() {
        let mut cache = cache();
        cache.entries.insert(PathBuf::from("/repo/wt"), Entry {
            branch: Some("main".into()),
            info: Some(PrInfo {
                number: 1,
                base_branch: "master".into(),
                url: String::new(),
                state: PrState::Open,
            }),
            queried_at: None,
            pending: false,
            refresh_requested: false,
        });

        let p = Path::new("/repo/wt");
        assert_eq!(cache.state(p, Some("main")), Some(PrState::Open));
        assert_eq!(cache.state(p, Some("feature")), None);
        assert_eq!(cache.state(p, None), None);
    }

    /// A collapsed project stops polling its entry, so a decrement that lived
    /// in `poll` would strand the slot forever.
    #[test]
    fn drain_completed_frees_a_slot_for_an_entry_nobody_polls() {
        let mut cache = cache();
        cache.set_concurrency(Some(1));
        let job = jobs::pool().spawn(jobs::Priority::Background, |_| PullRequests::new());
        bank_one(&mut cache, "/repo/wt", "main", job);
        assert_eq!(cache.in_flight(), 1);

        drain_until(&mut cache, Path::new("/repo/wt"), Duration::from_secs(5));

        assert_eq!(cache.in_flight(), 0);
    }

    /// A panicking job must free its slot the moment `drain_completed`
    /// observes `Job::failed`, not after waiting out the TTL. This differs
    /// from the TTL tests below, which backdate `started` instead of
    /// panicking.
    #[test]
    fn drain_completed_frees_a_slot_immediately_when_the_job_panics() {
        let mut cache = cache();
        cache.set_concurrency(Some(1));
        let job = jobs::Pool::new(2)
            .spawn(jobs::Priority::Background, |_| -> PullRequests { panic!("boom") });
        bank_one(&mut cache, "/repo/wt", "main", job);
        assert_eq!(cache.in_flight(), 1);

        drain_until(&mut cache, Path::new("/repo/wt"), Duration::from_secs(5));

        assert_eq!(cache.in_flight(), 0);
    }

    /// A job that never reports, whether it panicked or its forge call hangs,
    /// must not hold its slot forever. Without the TTL backoff a capped cache
    /// would stop polling permanently.
    #[test]
    fn drain_completed_frees_a_slot_for_a_job_stuck_past_the_ttl() {
        let now = Arc::new(Mutex::new(Duration::ZERO));
        let mut cache = cache_with_clock(&now);
        cache.set_concurrency(Some(1));
        let _release =
            insert_stuck_entry(&mut cache, Path::new("/repo/wt"), "main", Duration::ZERO);
        assert_eq!(cache.in_flight(), 1);

        *now.lock().expect("clock poisoned") = TTL + Duration::from_nanos(1);
        cache.drain_completed(&Recorder::default());

        assert_eq!(cache.in_flight(), 0);
    }

    /// A job just backed off by the TTL banks no answer, so nothing but a
    /// fresh `queried_at` can hold the entry back, and the guard's repaint
    /// delivers the frame that would re-spawn it.
    #[test]
    fn a_job_stuck_past_the_ttl_leaves_the_entry_ineligible_to_respawn() {
        let now = Arc::new(Mutex::new(Duration::ZERO));
        let mut cache = cache_with_clock(&now);
        let _release =
            insert_stuck_entry(&mut cache, Path::new("/repo/wt"), "main", Duration::ZERO);

        *now.lock().expect("clock poisoned") = TTL + Duration::from_nanos(1);
        cache.drain_completed(&Recorder::default());

        let entry = cache.entries.get(Path::new("/repo/wt")).unwrap();
        assert!(
            !should_spawn(
                entry.branch.as_deref(),
                Some("main"),
                entry.queried_at,
                entry.pending,
                cache.now()
            ),
            "a job just backed off by the TTL must not leave the entry due on the very next frame"
        );
    }

    /// The TTL boundary itself, which `Instant` arithmetic could not reach: an
    /// `Instant` cannot be constructed or advanced, so a test could only subtract
    /// from now and hope the machine had been up long enough.
    #[test]
    fn the_ttl_boundary_is_exact() {
        let now = Arc::new(Mutex::new(Duration::ZERO));
        let mut cache = cache_with_clock(&now);

        cache.entries.insert(PathBuf::from("/repo"), Entry {
            branch: Some("main".into()),
            queried_at: Some(Duration::ZERO),
            ..Entry::default()
        });

        *now.lock().expect("clock poisoned") = TTL - Duration::from_nanos(1);
        assert!(!should_spawn(
            Some("main"),
            Some("main"),
            Some(Duration::ZERO),
            false,
            cache.now()
        ));

        *now.lock().expect("clock poisoned") = TTL;
        assert!(should_spawn(Some("main"), Some("main"), Some(Duration::ZERO), false, cache.now()));
    }

    #[test]
    fn generation_advances_on_a_banked_result_and_holds_still_otherwise() {
        let mut cache = cache();
        let _release =
            insert_stuck_entry(&mut cache, Path::new("/repo/pending"), "main", Duration::ZERO);

        let before = cache.generation();
        cache.drain_completed(&Recorder::default());
        assert_eq!(cache.generation(), before, "a frame that banks nothing must not invalidate");

        let job = jobs::pool().spawn(jobs::Priority::Background, |_| PullRequests::new());
        bank_one(&mut cache, "/repo/banked", "main", job);
        drain_until(&mut cache, Path::new("/repo/banked"), Duration::from_secs(5));
        assert!(cache.generation() > before);
    }

    /// A refresh that lands while a lookup is in flight must survive it: `poll`
    /// only spawns when `pending` is empty, and the drain would otherwise stamp
    /// a fresh `queried_at` and swallow the request.
    #[test]
    fn a_refresh_during_a_lookup_survives_the_drain() {
        let mut cache = cache();
        let (release, job) = spawn_stuck_job();
        bank_one(&mut cache, "/repo/wt", "main", job);

        cache.invalidate_all();

        let _ = release.send(());
        drain_until(&mut cache, Path::new("/repo/wt"), Duration::from_secs(5));

        let entry = cache.entries.get(Path::new("/repo/wt")).unwrap();
        assert!(entry.queried_at.is_none(), "the next poll must re-query");
        assert!(!entry.refresh_requested, "and the request is spent, not sticky");
        // `queried_at: None` is only the precondition; assert the decision that
        // actually re-queries, or this passes with a `poll` that never spawns.
        assert!(
            should_spawn(
                entry.branch.as_deref(),
                Some("main"),
                entry.queried_at,
                entry.pending,
                cache.now()
            ),
            "a spent refresh must leave the entry eligible to spawn"
        );
    }

    /// Setting the flag on idle entries too would double-poll every one of
    /// them: the drain banks nothing, `poll` starts the lookup, and the still-set
    /// flag then refuses to stamp `queried_at`, so a second lookup starts.
    #[test]
    fn a_refresh_on_an_idle_entry_does_not_set_the_flag() {
        let mut cache = cache();
        cache.entries.insert(PathBuf::from("/repo/wt"), Entry {
            branch: Some("main".into()),
            info: None,
            queried_at: Some(Duration::ZERO),
            pending: false,
            refresh_requested: false,
        });

        cache.invalidate_all();

        let entry = cache.entries.get(Path::new("/repo/wt")).unwrap();
        assert!(entry.queried_at.is_none());
        assert!(!entry.refresh_requested);
    }

    #[test]
    fn the_cap_admits_until_it_is_reached() {
        assert!(!may_spawn(0, 0), "a zero cap never admits a lookup");
        assert!(may_spawn(2, 0));
        assert!(may_spawn(2, 1));
        assert!(!may_spawn(2, 2));
        assert!(!may_spawn(2, 3), "an over-count must not reopen the gate");
    }

    /// A zero cap admits nothing, so the cache cannot start life holding one:
    /// a caller that never reaches `set_concurrency` would poll every frame
    /// and spawn nothing.
    #[test]
    fn a_cache_that_was_never_configured_still_admits_a_lookup() {
        let cache = cache();
        assert!(may_spawn(cache.concurrency, cache.in_flight));
    }

    #[test]
    fn set_concurrency_clamps_zero_to_one() {
        let mut cache = cache();
        cache.set_concurrency(Some(0));
        assert!(may_spawn(cache.concurrency, 0));
        assert!(!may_spawn(cache.concurrency, 1));
    }

    /// A forge lookup is the slowest thing the pool runs and the least urgent.
    /// Letting it take the last background slot puts the git status panel,
    /// which is what a user reads to decide what to do next, behind a network
    /// call.
    #[test]
    fn a_lookup_never_takes_the_last_background_slot() {
        // A four-worker pool admits three background tasks; an eight-worker one,
        // seven.
        assert_eq!(effective_cap(None, 3), 2);
        assert_eq!(effective_cap(None, 7), 6);
    }

    /// The setting lowers the cap and never raises it, which is what its doc
    /// comment already claims.
    #[test]
    fn the_configured_cap_can_only_lower() {
        assert_eq!(effective_cap(Some(1), 7), 1);
        assert_eq!(effective_cap(Some(99), 7), 6);
    }

    /// A two-worker pool has a background ceiling of one, and one minus the
    /// reservation is zero, which would admit no lookup at all.
    #[test]
    fn the_cap_never_reaches_zero() {
        assert_eq!(effective_cap(None, 1), 1);
        assert_eq!(effective_cap(Some(0), 7), 1);
    }

    /// The cap has to hold where a due list becomes requests, not just in the
    /// helper: a cold cache polls every eligible worktree in one frame. A
    /// refused member falls due again rather than being lost.
    #[test]
    fn the_drain_respects_the_concurrency_cap() {
        let mut cache = cache();
        cache.set_concurrency(Some(1));
        let (_release, job) = spawn_stuck_job();
        bank_one(&mut cache, "/repo/busy", "main", job);

        let capped = Path::new("/repo/capped");
        // A worktree that has just switched branch is the case a cleared
        // `pending` alone cannot rescue: `poll` writes the new branch before
        // the cap has had its say, so the mismatch that would make the entry
        // due is gone and the old branch's stamp is still inside the TTL.
        cache.entries.insert(capped.to_path_buf(), Entry {
            branch: Some("old".into()),
            info: Some(sample_info()),
            queried_at: Some(Duration::ZERO),
            pending: false,
            refresh_requested: false,
        });
        let repaint = Recorder::default();
        cache.poll(capped, Some("feature"), &repaint);
        cache.drain_completed(&repaint);

        assert_eq!(cache.in_flight(), 1, "the cap must refuse the second request");
        assert!(cache.is_due(capped, "feature"), "a refused member must fall due again");
    }

    /// The next drain spawns a queued lookup, not the frame that queued it,
    /// and egui paints on demand. Without a wake the request waits on the
    /// user's next input instead of on the TTL.
    #[test]
    fn a_queued_poll_asks_for_the_frame_that_spawns_it() {
        let repaint = Recorder::default();
        let mut cache = cache();

        cache.poll(Path::new("/repo/wt"), Some("main"), &repaint);

        assert_eq!(repaint.wakes(), 1, "a queued lookup must ask for its spawning frame");
    }

    /// A member the cap refuses has its `pending` cleared, so it falls due
    /// again on the very next frame. Asking for that frame while nothing can
    /// spawn spins the UI at frame rate for as long as the batch runs, and a
    /// batch can run several serial forge processes. Nothing is lost by staying
    /// quiet: the guard inside the spawn closure delivers the wake the moment
    /// a slot frees, on the panicking path too.
    #[test]
    fn a_poll_the_cap_will_refuse_does_not_ask_for_another_frame() {
        let repaint = Recorder::default();
        let mut cache = cache();
        cache.set_concurrency(Some(1));
        let (_release, job) = spawn_stuck_job();
        bank_one(&mut cache, "/repo/busy", "main", job);

        cache.poll(Path::new("/repo/capped"), Some("feature"), &repaint);

        assert_eq!(repaint.wakes(), 0, "a saturated cap must not spin the frame loop");
    }

    /// The drain that frees a concurrency slot only runs on a frame, so a
    /// worker that exits without waking the app can stall polling for good.
    #[test]
    fn dropping_the_guard_wakes_the_app() {
        let repaint = Recorder::default();

        drop(WakeOnDrop(repaint.clone()));

        assert_eq!(repaint.wakes(), 1);
    }

    /// The spawn has no sender of its own, since the pool's channel is
    /// internal, so this drives a real job through the pool instead of
    /// hand-rolling a thread, and checks the failure the same way production
    /// code does: `poll` until `failed` latches.
    #[test]
    fn a_panicking_worker_still_wakes_the_app_and_reports_failed() {
        let repaint = Recorder::default();

        let job = {
            let repaint = repaint.clone();
            // Interactive, since a background job runs below normal priority and
            // a saturated machine can leave it unscheduled for seconds.
            jobs::Pool::new(2).spawn(jobs::Priority::Interactive, move |_| -> PullRequests {
                let _wake = WakeOnDrop(repaint);
                panic!("worker died");
            })
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.failed() {
            assert!(job.poll().is_none(), "a panicking job never reports a value");
            assert!(Instant::now() < deadline, "the failure was never observed");
            thread::yield_now();
        }

        assert_eq!(repaint.wakes(), 1, "a panicking unwind still wakes the app");
    }

    /// One result covers many entries, so the drain has to fan a single map out
    /// across every path that contributed to it.
    #[test]
    fn one_banked_result_reaches_every_member() {
        let repaint = Recorder::default();
        let mut cache = cache();
        let members =
            vec![Head { path: PathBuf::from("/repo/a"), branch: "topic-a".into() }, Head {
                path: PathBuf::from("/repo/b"),
                branch: "topic-b".into(),
            }];
        let job = jobs::Pool::new(2).spawn(jobs::Priority::Background, |_| {
            PullRequests::from([
                (
                    PathBuf::from("/repo/a"),
                    Ok(Some(PrInfo {
                        number: 7,
                        base_branch: "master".into(),
                        url: "u".into(),
                        state: PrState::Open,
                    })),
                ),
                (PathBuf::from("/repo/b"), Ok(None)),
            ])
        });
        cache.bank_batch(members, job);
        assert_eq!(cache.in_flight(), 1, "one request, not one per branch");

        for _ in 0..200 {
            cache.drain_completed(&repaint);
            if cache.in_flight() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(cache.in_flight(), 0, "the request never reported");

        assert_eq!(cache.state(Path::new("/repo/a"), Some("topic-a")), Some(PrState::Open));
        // Asked about and answered with no PR means no PR, not "never asked":
        // the entry must be stamped, or it re-queries on the very next frame.
        assert_eq!(cache.state(Path::new("/repo/b"), Some("topic-b")), None);
        assert!(!cache.is_due(Path::new("/repo/b"), "topic-b"), "banked as no-PR, not left due");
    }
}
