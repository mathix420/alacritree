//! Detect whether the current branch has an open PR on GitHub, and cache
//! its base branch so the sidebar diff can target the PR's base instead of
//! the repo's default branch.
//!
//! Why shell out to `gh` rather than hit the API directly: it inherits the
//! user's existing auth and host config (enterprise, multiple accounts), and
//! we already require `git` on PATH — adding `gh` is a familiar dependency
//! for anyone who lives in this workflow.  The lookup is best-effort: if
//! `gh` is missing, unauthenticated, or no PR exists, we silently fall back
//! to the repo's default branch.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::projects::Worktree;
use crate::{command_ext, jobs, pr_query, wsl};

/// Re-query at most this often.  PR base branches rarely change, and a stale
/// answer just falls back to the previous diff target — not worth hammering
/// `gh` on every status refresh.
const TTL: Duration = Duration::from_secs(300);

/// GitHub's PR lifecycle, folded to what the sidebar paints.  `gh` reports
/// draftness as a separate boolean, so OPEN splits into Open/Draft here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrInfo {
    pub number: u64,
    pub base_branch: String,
    pub url: String,
    pub state: PrState,
}

/// How many bursts may run at once: what the config asks for, never above one
/// below the pool's background ceiling.
///
/// A ceiling rather than a limit that binds today.  One frame's whole due list
/// becomes a single job, and every entry it covers stays `pending` until that
/// job settles, so nothing new falls due meanwhile: one request is in flight in
/// steady state and two across a handover, under any cap this returns.  It
/// stays because the shape it guards against is cheap to reintroduce — a spawn
/// per group would put a project's repositories on the pool at once — and
/// because reserving a slot below the ceiling is what leaves a worker for the
/// local work sharing the pool, at any pool size.
fn effective_cap(configured: Option<usize>, ceiling: usize) -> usize {
    configured.unwrap_or(usize::MAX).min(ceiling.saturating_sub(1)).max(1)
}

pub struct PrCache {
    entries: HashMap<PathBuf, Entry>,
    /// Requests in flight.  `in_flight` counts these rather than branches:
    /// what a burst costs follows the repositories it spans — a resolve and a
    /// query each, or one `gh pr list` per member of a group that could not be
    /// batched — not the number of branches waiting on it.
    batches: Vec<Batch>,
    /// Entries that asked for a lookup this frame, grouped and spawned by the
    /// next `drain_completed`.  Batching needs a whole frame's worth of due
    /// entries before it can group them, which one `poll` call cannot see.
    due: Vec<Member>,
    in_flight: usize,
    concurrency: usize,
    generation: u64,
    /// Elapsed since this cache was built.  A `Duration` rather than an
    /// `Instant` because an `Instant` cannot be constructed or advanced, so
    /// nothing could set one to test a boundary against.
    clock: Box<dyn Fn() -> Duration + Send>,
}

impl Default for PrCache {
    fn default() -> Self {
        let origin = Instant::now();
        Self::with_clock(move || origin.elapsed())
    }
}

#[derive(Default)]
struct Entry {
    /// Branch the cached result was queried for.  Switching branches in the
    /// same worktree invalidates the entry.
    branch: Option<String>,
    info: Option<PrInfo>,
    queried_at: Option<Duration>,
    /// Set from the moment this entry joins the due list until its answer is
    /// banked.  `should_spawn` reads it to avoid asking twice for one badge,
    /// so it has to cover the queued frame as well as the running one.
    pending: bool,
    /// A refresh landed while `pending` was already occupied.  The drain
    /// leaves `queried_at` cleared instead of stamping the fresh lookup's
    /// result as current, so the next poll re-queries.
    refresh_requested: bool,
}

/// A worktree and the branch its badge is keyed to.  Carried through
/// grouping and back out through the drain, so a batched answer can find
/// every entry that asked for it.
#[derive(Debug, Clone, PartialEq)]
struct Member {
    path: PathBuf,
    branch: String,
}

/// One request in flight, and every entry waiting on it.  A job that never
/// reports would otherwise hold its concurrency slot forever: a panicked one
/// reports through `Job::failed` immediately, a merely slow one is backed off
/// once it has been in flight past the TTL.
struct Batch {
    job: jobs::Job<BatchResult>,
    started: Duration,
    members: Vec<Member>,
}

/// What one request reports back: an answer per worktree it covered.  Keyed by
/// path rather than by branch, because two repositories in one burst can hold
/// the same branch name.  `None` means the request covered that path and found
/// no PR, which is a real answer.
type BatchResult = HashMap<PathBuf, Option<PrInfo>>;

impl PrCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_clock(clock: impl Fn() -> Duration + Send + 'static) -> Self {
        Self {
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
    pub fn state(&self, path: &Path, branch: Option<&str>) -> Option<PrState> {
        let entry = self.entries.get(path)?;
        if entry.branch.as_deref() != branch {
            return None;
        }
        entry.info.as_ref().map(|i| i.state)
    }

    /// Returns the PR info known for `(path, branch)` right now, kicking off
    /// a background refresh if the cache is stale or branch-mismatched.
    /// Never blocks — the caller will see the previous value (or `None`)
    /// until the worker finishes and the next frame picks up the result.
    pub fn poll(
        &mut self,
        path: &Path,
        branch: Option<&str>,
        ctx: &egui::Context,
    ) -> Option<PrInfo> {
        let now = self.now();
        let entry = self.entries.entry(path.to_path_buf()).or_default();

        // A `None` poll (the git-status compute hasn't produced a branch
        // yet, or never will) carries no information about the current
        // branch, so it must not evict or refresh a lookup keyed to a real
        // one from another caller — just read whatever is cached.
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
            self.due.push(Member { path: path.to_path_buf(), branch: branch.to_string() });
            // The frame that queues a lookup is not the frame that starts one
            // — the next drain is — and egui paints on demand.  Without asking
            // for that frame the request waits on the user's next input
            // instead of on the TTL.
            //
            // Only while a slot is free, though: over the cap the drain
            // refuses the member and leaves it due, so an unconditional ask
            // would repaint at frame rate for as long as the batch runs.  The
            // guard inside the spawn closure delivers that wake when a slot
            // frees, on the panicking path too.
            if may_spawn(self.concurrency, self.in_flight) {
                ctx.request_repaint();
            }
        }

        self.entries.get(path).and_then(|entry| entry.info.clone())
    }

    /// Advances whenever what `state` would answer may have moved.  The sidebar
    /// reconciler compares it to know a filtered row set needs rebuilding; a
    /// banked result that happens to match the previous one costs one extra
    /// rebuild, which is cheaper than diffing states to avoid it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The cap on lookups in flight at once: `configured` if given, else the
    /// pool decides.  Either way it never exceeds the pool's own background
    /// ceiling, so a cold cache can't fork one `gh` process per eligible
    /// worktree and starve the local work sharing the pool.
    pub fn set_concurrency(&mut self, configured: Option<usize>) {
        self.concurrency = effective_cap(configured, jobs::pool().background_ceiling());
    }

    /// Bank every finished request and free its slot, then turn the frame's
    /// due list into new requests.  Runs once a frame ahead of every poll
    /// site rather than inside `poll`: an entry whose project collapsed
    /// mid-lookup is never polled again, and a slot it still held would never
    /// come back.
    pub fn drain_completed(&mut self, ctx: &egui::Context) {
        let now = self.now();
        let mut banked = false;
        let mut still_running = Vec::new();
        for batch in std::mem::take(&mut self.batches) {
            if let Some(found) = batch.job.poll() {
                for m in &batch.members {
                    self.settle(m, found.get(&m.path).cloned().flatten(), now);
                }
                banked = true;
            } else if batch.job.failed() || now.saturating_sub(batch.started) > TTL {
                // A request that never reports has no answer to bank, but its
                // members must still be stamped: leaving them due re-spawns a
                // `gh` process every frame for as long as the failure lasts.
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
        self.spawn_due(ctx);
    }

    /// Record one member's answer.  `None` means the request covered this
    /// branch and found no PR, which is a real answer and gets stamped.
    fn settle(&mut self, m: &Member, info: Option<PrInfo>, now: Duration) {
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
    fn back_off(&mut self, m: &Member, now: Duration) {
        let entry = self.entries.entry(m.path.clone()).or_default();
        entry.queried_at = Some(now);
        entry.refresh_requested = false;
        entry.pending = false;
    }

    /// Hand the frame's due list to one worker.  Grouping needs `git2` to read
    /// each path's `origin`, which is why nothing here inspects the list: the
    /// frame only decides whether there is room to ask.
    ///
    /// Over the cap the list is dropped, and every member is returned to the
    /// state it was polled in — not just `pending` cleared.  `poll` has
    /// already written the new branch, so on a branch switch the stamp is the
    /// only thing left saying the entry is stale; keeping it would read as a
    /// fresh answer for a branch nothing ever looked up.
    fn spawn_due(&mut self, ctx: &egui::Context) {
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
        let ctx = ctx.clone();
        let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            // Fires on a panicking unwind too, since it's a local: the drain
            // that frees this slot only runs on a frame, so an exit without a
            // repaint can stall polling for good.
            let _wake = RepaintOnDrop(ctx);
            run_due(due, blocking)
        });
        self.bank_batch(members, job);
    }

    /// Mark every entry stale.  Entries with a lookup already running also get
    /// `refresh_requested`, because clearing `queried_at` alone cannot reach
    /// them: `poll` will not spawn while `pending` is occupied, and the drain
    /// would stamp a fresh timestamp over the request.
    pub fn invalidate_all(&mut self) {
        for entry in self.entries.values_mut() {
            entry.queried_at = None;
            if entry.pending {
                entry.refresh_requested = true;
            }
        }
        self.generation = self.generation.wrapping_add(1);
    }

    /// Record a started request against every entry it covers.  Each entry is
    /// keyed to the branch being asked about rather than to the last banked
    /// answer: a worker that dies without sending leaves nothing for the drain
    /// to key it with, and a mismatched branch makes the entry due again on the
    /// next frame however recently it was queried.
    fn bank_batch(&mut self, members: Vec<Member>, job: jobs::Job<BatchResult>) {
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

/// A `None` incoming branch never invalidates — the caller has nothing to
/// compare against. A `Some` branch that disagrees with the cached one means
/// a real branch switch and must invalidate.
fn should_invalidate(cached_branch: Option<&str>, incoming_branch: Option<&str>) -> bool {
    match incoming_branch {
        None => false,
        Some(_) => cached_branch != incoming_branch,
    }
}

/// Whether a worktree in `state` survives the projects panel's PR dimension.
/// The active states union; with none active every worktree passes.  An unknown
/// state — no lookup yet, no PR, or no `gh` — satisfies no active toggle.
pub fn pr_pass(
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

/// The branch a worktree's PR lookup is keyed to.  The active worktree prefers
/// its live status branch; every other worktree, and an active one whose
/// `StatusCache` has not produced a branch yet, uses the stored snapshot.
///
/// The split is what keeps two pollers of one path from fighting.  [`PrCache`]
/// is keyed by path alone, so the right sidebar — which polls the active
/// workspace with its live `StatusCache` branch, recomputed every ~1.5 s — and
/// the projects sidebar must agree on a branch, or each drain flips
/// `entry.branch` and they invalidate each other's lookups forever after an
/// in-terminal checkout.  Every other worktree has a single poller, and an
/// inactive workspace's `StatusCache` is created once and then never re-polled
/// or pruned: reading it would freeze the branch at whatever it was on the last
/// visit and shadow later `refresh_project` updates to `wt.branch`.
pub fn effective_branch<'a>(
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

/// Group a whole burst and ask for each group in turn, reporting one answer
/// per worktree.  Runs on a worker: both the `git2` reads that grouping needs
/// and the requests themselves block.
fn run_due(due: Vec<Member>, blocking: &jobs::Blocking) -> BatchResult {
    let mut out = HashMap::new();
    for group in groups(due, blocking) {
        // A cancel landing between groups has no child to kill — neither the
        // request nor the sweep registers one — so each group asks before
        // starting rather than forking `gh` for a caller that is gone.
        if blocking.cancelled() {
            break;
        }
        let found = query_group(
            &group,
            |cwd, query| run_graphql(cwd, query, blocking),
            |m| query_gh(&m.path, &m.branch, blocking),
        );
        for m in &group.members {
            out.insert(m.path.clone(), found.get(&m.branch).cloned());
        }
    }
    out
}

/// What one request covers: the branches asked about, and one worktree inside
/// the repository to run `gh` from.  An absent `slug` means this group has no
/// batched form and runs the per-branch path instead.
struct Group {
    /// Any worktree of this repository; `gh` resolves the repo from its cwd.
    cwd: PathBuf,
    slug: Option<(String, String)>,
    members: Vec<Member>,
}

/// One request per repository, chunked, plus one per path that cannot be
/// grouped.  Reading `origin` costs a git2 open per due path, and resolving
/// costs a `gh` process per repository, which is why this runs on a worker
/// rather than on the frame.
fn groups(due: Vec<Member>, blocking: &jobs::Blocking) -> Vec<Group> {
    groups_with(due, |cwd| resolve_repo(cwd, blocking))
}

/// `resolve` names the repository a group asks about, given any worktree of
/// it.  Separate from [`groups`] so a test can pin which repository a group
/// ends up asking without a `gh` process deciding it.
fn groups_with(
    due: Vec<Member>,
    resolve: impl Fn(&Path) -> Option<(String, String)>,
) -> Vec<Group> {
    let mut by_repo: HashMap<(String, String), Group> = HashMap::new();
    let mut ungrouped = Vec::new();
    for m in due {
        let slug = match wsl::classify(&m.path) {
            wsl::Location::Windows(p) => origin_slug(&p),
            // Nothing here can read a repository inside a distro, and its
            // `gh` runs as a script rather than a `Command`.
            wsl::Location::Wsl { .. } => None,
        };
        match slug {
            Some((owner, name)) => by_repo
                .entry((owner.clone(), name.clone()))
                .or_insert_with(|| Group {
                    cwd: m.path.clone(),
                    slug: Some((owner, name)),
                    members: Vec::new(),
                })
                .members
                .push(m),
            None => ungrouped.push(Group { cwd: m.path.clone(), slug: None, members: vec![m] }),
        }
    }
    by_repo
        .into_values()
        .flat_map(|mut g| {
            // `origin` says only which worktrees share a repository.  Which
            // repository to ask is `gh`'s answer, and the two differ on a fork
            // checkout: `origin` names the fork, while a pull request is listed
            // under the repository it targets.  One resolve per repository, so
            // a project's worktrees still cost one process between them.
            g.slug = resolve(&g.cwd);
            g.members
                .chunks(pr_query::CHUNK)
                .map(|c| Group { cwd: g.cwd.clone(), slug: g.slug.clone(), members: c.to_vec() })
                .collect::<Vec<_>>()
        })
        .chain(ungrouped)
        .collect()
}

/// Ask GitHub about a whole group in one request, falling back to the
/// per-branch path when there is no batched form or the request produced no
/// usable answer.  GraphQL can need scopes `gh pr list` does not, so an
/// install that works today can fail here, and a project's badges must not
/// vanish when it does.
///
/// An answer naming no PR at all is still an answer and returns as one: a
/// repository whose branches have no open PRs is the common case, and
/// sweeping it per branch would find the same nothing at one process each.
///
/// `request` and `per_branch` are injected so a test can pin which of the two
/// paths a given response takes without spawning `gh`.
fn query_group(
    group: &Group,
    request: impl Fn(&Path, &str) -> Option<Vec<u8>>,
    per_branch: impl Fn(&Member) -> Option<PrInfo>,
) -> HashMap<String, PrInfo> {
    let branches: Vec<String> = group.members.iter().map(|m| m.branch.clone()).collect();
    if let Some((owner, name)) = &group.slug {
        let query = pr_query::build(owner, name, &branches);
        if let Some(stdout) = request(&group.cwd, &query) {
            if let Some(parsed) = pr_query::parse(&stdout, &branches, Some(owner)) {
                return parsed;
            }
        }
    }
    group.members.iter().filter_map(|m| per_branch(m).map(|i| (m.branch.clone(), i))).collect()
}

/// Run one GraphQL document through `gh`, returning its stdout.
///
/// The query goes in on stdin because `-f query=` puts it in argv, and a
/// Windows command line caps at 32,767 characters, which a full chunk of
/// aliases can exceed.  `--input -` reads a JSON body, so a bare query piped
/// in comes back as HTTP 502 rather than as an argument error.
///
/// Only ever called for a group with a slug, which means a native path: a WSL
/// group has no slug and never reaches here.
#[allow(clippy::disallowed_methods)] // Running `gh` is this function's job.
fn run_graphql(cwd: &Path, query: &str, _blocking: &jobs::Blocking) -> Option<Vec<u8>> {
    let mut child = command_ext::hidden("gh")
        .current_dir(cwd)
        .args(["api", "graphql", "--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        use std::io::Write;
        // The scope ends before the wait below, which closes the pipe and lets
        // `gh` see EOF; held open, the child waits for input that never ends.
        let mut stdin = child.stdin.take()?;
        stdin.write_all(pr_query::body(query).as_bytes()).ok()?;
    }
    let output = child.wait_with_output().ok()?;
    output.status.success().then_some(output.stdout)
}

struct RepaintOnDrop(egui::Context);

impl Drop for RepaintOnDrop {
    fn drop(&mut self) {
        self.0.request_repaint();
    }
}

fn pr_state(state: &str, is_draft: bool) -> PrState {
    match state {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        "OPEN" if is_draft => PrState::Draft,
        // Unknown states paint as open rather than vanishing; gh's enum is
        // stable, so this is a forward-compatibility hedge, not a real case.
        _ => PrState::Open,
    }
}

/// Ask `gh` for the PR associated with `branch` in `path`.  Returns `None`
/// on any failure mode (no `gh`, not authenticated, no PR, non-GitHub
/// remote, ...).  The branch is named explicitly so the answer is tied to
/// that specific branch rather than whatever ref happens to be checked out
/// in the worktree.
///
/// `--head` rather than `gh pr view <branch>`: `pr view` matches a PR's head
/// *label*, which is the bare branch only while the head lives in the base
/// repo and becomes `owner:branch` once it lives on a fork.  A checkout whose
/// `origin` is a personal fork therefore finds nothing.  `--head` filters on
/// the head ref name alone, which both layouts share, and `--state all` keeps
/// the merged and closed badges that `pr list` would otherwise drop.
#[allow(clippy::disallowed_methods)] // Running `gh` is this function's job.
fn query_gh(path: &Path, branch: &str, blocking: &jobs::Blocking) -> Option<PrInfo> {
    const PR_JSON_FIELDS: &str = "number,baseRefName,url,state,isDraft,headRepositoryOwner";
    // `--head` matches the ref name in every head repository and `--state all`
    // keeps the closed and merged ones, so a generic branch name in a busy base
    // repo overflows `gh`'s default page of 30 and the owner preference below
    // never sees this checkout's own PR.
    const PR_LIMIT: &str = "100";
    match wsl::classify(path) {
        wsl::Location::Windows(p) => {
            let owner = origin_slug(&p).map(|(owner, _)| owner);
            let output = command_ext::hidden("gh")
                .current_dir(p)
                .args([
                    "pr",
                    "list",
                    "--head",
                    branch,
                    "--state",
                    "all",
                    "--limit",
                    PR_LIMIT,
                    "--json",
                    PR_JSON_FIELDS,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            parse_gh_output(&output.stdout, owner.as_deref())
        },
        // `gh` must be installed and authenticated *inside* the distro; any
        // failure falls back to the default branch, same as a missing
        // Windows gh.  The batch script rides the resident helper when it
        // is up (a one-shot spawn otherwise); the capability path from the
        // helper's hello honors per-user install dirs that the default
        // `--exec` PATH lacks.
        wsl::Location::Wsl { distro, linux_path } => {
            let gh = crate::wsl_helper::capability_gh(&distro).unwrap_or_else(|| "gh".to_string());
            // The `origin` URL rides along on the first line: git2 cannot read
            // a repository that lives inside the distro, and a second round
            // trip would double the cost of a badge that already forks `gh`.
            // The substitution collapses a missing remote to a blank line, so
            // the JSON always starts after exactly one newline.
            let script = r#"cd "$1" || exit 1
printf '%s\n' "$(git config --get remote.origin.url 2>/dev/null)"
exec "$2" pr list --head "$3" --state all --limit "$4" --json "$5""#;
            let stdout = wsl::run_batch(
                &distro,
                script,
                &[&linux_path, &gh, branch, PR_LIMIT, PR_JSON_FIELDS],
                blocking,
            )
            .ok()?;
            let (origin_url, json) = split_origin_url_line(&stdout);
            let owner = origin_url.and_then(github_slug_from_url).map(|(owner, _)| owner);
            parse_gh_output(json, owner.as_deref())
        },
    }
}

/// Split the WSL batch's leading `origin` URL off the JSON that follows it.
/// An empty first line means the worktree has no readable `origin`.
fn split_origin_url_line(stdout: &[u8]) -> (Option<&str>, &[u8]) {
    let Some(end) = stdout.iter().position(|b| *b == b'\n') else {
        // Nothing ran far enough to emit the line; hand the payload to the
        // JSON parser, which rejects it the way it rejects any non-JSON.
        return (None, stdout);
    };
    let url = std::str::from_utf8(&stdout[..end]).ok().map(str::trim).filter(|u| !u.is_empty());
    (url, &stdout[end + 1..])
}

/// The repository `gh` itself would act on from this worktree, which is the
/// one holding the pull requests: `origin` on a fork checkout names the fork,
/// while a pull request opened from it is listed under the repository it
/// targets.  Asking `gh` rather than reimplementing its resolution also
/// honours `gh repo set-default` and the `upstream` remote convention.
///
/// `None` for anything that does not answer with a GitHub `owner/name`, which
/// leaves the group on the per-branch path.
#[allow(clippy::disallowed_methods)] // Running `gh` is this function's job.
fn resolve_repo(cwd: &Path, _blocking: &jobs::Blocking) -> Option<(String, String)> {
    let output = command_ext::hidden("gh")
        .current_dir(cwd)
        .args(["repo", "view", "--json", "nameWithOwner"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_name_with_owner(&output.stdout)
}

/// Split `gh repo view --json nameWithOwner` into its two halves.
fn parse_name_with_owner(stdout: &[u8]) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let (owner, name) = value.get("nameWithOwner")?.as_str()?.split_once('/')?;
    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_string(), name.to_string()))
}

/// The GitHub `(owner, repository)` of this worktree's `origin`, read straight
/// from the repository config.  This is the grouping key — which worktrees
/// share a repository — not what the request asks about; `resolve_repo`
/// decides that.  `None` for a missing, unreadable or non-GitHub remote, which
/// leaves the path on the per-branch path.
fn origin_slug(path: &Path) -> Option<(String, String)> {
    let repo = git2::Repository::open(path).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    github_slug_from_url(remote.url()?)
}

/// Owner and repository of a GitHub remote URL, for the shapes git accepts:
/// `https://github.com/owner/repo.git`, `git@github.com:owner/repo.git`, and
/// the scp-style host alias `gh:owner/repo.git`.  The `.git` suffix comes off
/// so that two spellings of one remote group together.  `None` for anything
/// else — the owner only breaks ties, and an ungroupable worktree just takes
/// the per-branch path.
fn github_slug_from_url(url: &str) -> Option<(String, String)> {
    let (host, path) = split_remote_url(url.trim())?;
    if !is_github_host(host) {
        return None;
    }
    let (owner, repo) = path.trim_start_matches('/').split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!owner.is_empty() && !repo.is_empty()).then(|| (owner.to_string(), repo.to_string()))
}

/// Host and path of a remote URL, covering both the scheme form and the
/// scp-style `[user@]host:path` one that git reads whenever the colon comes
/// before any slash.
fn split_remote_url(url: &str) -> Option<(&str, &str)> {
    if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        return Some((remote_host(authority), path));
    }
    let (authority, path) = url.split_once(':')?;
    // A leading slash means an absolute local path (`C:/repos/x`), which git
    // does not read as scp-style however much it looks like one.
    if authority.contains('/') || path.starts_with('/') {
        return None;
    }
    Some((remote_host(authority), path))
}

fn remote_host(authority: &str) -> &str {
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    host.split(':').next().unwrap_or(host)
}

/// `github.com`, or any host with no dot in it.  A dotless host is an
/// `~/.ssh/config` alias whose real target we cannot see, and aliases are how
/// fork checkouts pick an SSH identity — refusing them would blind the owner
/// preference to the layout it exists for.  Guessing wrong on one just yields
/// an owner no PR matches.
fn is_github_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com") || !host.contains('.')
}

/// Pick the PR a head branch's badge should show.  Two rules, in order:
///
/// `--head` matches the ref name across *every* head repository, so a generic
/// branch name ("dev", "patch-1") also collects PRs strangers opened from their
/// own forks.  A head repo owned by the same account as this worktree's
/// `origin` is the one this checkout actually pushed, so it outranks a
/// stranger's however live that one is.  Without a readable owner, or with no
/// candidate matching it, every PR stays in the running.
///
/// Among what survives, `gh pr list` answers newest first and a branch
/// accumulates PRs over its life; an open one is the live PR, so it outranks a
/// newer abandoned attempt.  Drafts report `OPEN` too, so this covers them.
/// Mirrors how `gh pr view` orders its own candidates.
fn select_pr<'a>(
    prs: &'a [serde_json::Value],
    origin_owner: Option<&str>,
) -> Option<&'a serde_json::Value> {
    origin_owner
        .and_then(|owner| open_or_newest(prs.iter().filter(|pr| head_owner_is(pr, owner))))
        .or_else(|| open_or_newest(prs.iter()))
}

fn open_or_newest<'a>(
    prs: impl Iterator<Item = &'a serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    let mut newest = None;
    for pr in prs {
        if pr.get("state").and_then(|s| s.as_str()) == Some("OPEN") {
            return Some(pr);
        }
        newest = newest.or(Some(pr));
    }
    newest
}

fn head_owner_is(pr: &serde_json::Value, owner: &str) -> bool {
    pr.get("headRepositoryOwner")
        .and_then(|o| o.get("login"))
        .and_then(|l| l.as_str())
        .is_some_and(|login| login.eq_ignore_ascii_case(owner))
}

fn parse_gh_output(stdout: &[u8], origin_owner: Option<&str>) -> Option<PrInfo> {
    let list: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    select_and_build(list.as_array()?, origin_owner)
}

/// Select the winning PR from a candidate list and build the `PrInfo` for it.
/// Shared by the single-branch `gh pr list` path and the batched GraphQL one,
/// so a change to selection or field reads applies to both by construction.
pub(crate) fn select_and_build(
    prs: &[serde_json::Value],
    origin_owner: Option<&str>,
) -> Option<PrInfo> {
    let value = select_pr(prs, origin_owner)?;
    let number = value.get("number")?.as_u64()?;
    let base = value.get("baseRefName")?.as_str()?.to_string();
    let url = value.get("url")?.as_str()?.to_string();
    let state = value.get("state").and_then(|v| v.as_str()).unwrap_or("OPEN");
    let is_draft = value.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(PrInfo { number, base_branch: base, url, state: pr_state(state, is_draft) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    use crate::test_util::{add_worktree, init_repo};

    /// Spawn a job that blocks until `release` fires, so a test can hold a
    /// lookup pending for as long as it needs.  Dropping `release` unblocks
    /// it too, which is what reclaims the slot once a test is done with it.
    ///
    /// Runs on a throwaway pool rather than the process-wide `jobs::pool()`:
    /// this deliberately wedges a background slot, and the shared pool is a
    /// handful of workers other test binaries in this crate poll against
    /// with their own deadlines — a pool built just for this call can never
    /// starve them, or be starved by them.
    fn spawn_stuck_job() -> (mpsc::Sender<()>, jobs::Job<BatchResult>) {
        let (release, gate) = mpsc::channel::<()>();
        let job = jobs::Pool::new(2).spawn(jobs::Priority::Background, move |_| {
            let _ = gate.recv();
            BatchResult::new()
        });
        (release, job)
    }

    /// Bank `job` as the request covering `(path, branch)`, the shape `poll`
    /// and the drain produce for a single due worktree.
    fn bank_one(cache: &mut PrCache, path: &str, branch: &str, job: jobs::Job<BatchResult>) {
        cache.bank_batch(
            vec![Member { path: PathBuf::from(path), branch: branch.to_string() }],
            job,
        );
    }

    /// Wire a stuck request into `cache` as if it had been in flight since
    /// `started`, for tests that need to force `drain_completed`'s TTL branch
    /// without waiting out the real TTL.  `bank_batch` stamps `started` from
    /// the cache's own clock, so the batch is assembled by hand instead.
    fn insert_stuck_entry(
        cache: &mut PrCache,
        path: &Path,
        branch: &str,
        started: Duration,
    ) -> mpsc::Sender<()> {
        let (release, job) = spawn_stuck_job();
        let member = Member { path: path.to_path_buf(), branch: branch.to_string() };
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
    fn drain_until(cache: &mut PrCache, path: &Path, timeout: Duration) {
        let ctx = egui::Context::default();
        let deadline = Instant::now() + timeout;
        loop {
            cache.drain_completed(&ctx);
            if cache.entries.get(path).is_none_or(|e| !e.pending) {
                return;
            }
            assert!(Instant::now() < deadline, "the lookup never landed");
            thread::yield_now();
        }
    }

    #[test]
    fn parses_gh_json() {
        let stdout =
            br#"[{"baseRefName":"main","number":42,"url":"https://github.com/o/r/pull/42"}]"#;
        let info = parse_gh_output(stdout, None).unwrap();
        assert_eq!(info.number, 42);
        assert_eq!(info.base_branch, "main");
        assert_eq!(info.url, "https://github.com/o/r/pull/42");
    }

    #[test]
    fn rejects_empty_output() {
        assert!(parse_gh_output(b"", None).is_none());
    }

    #[test]
    fn rejects_an_empty_pr_list() {
        assert!(parse_gh_output(b"[]", None).is_none());
    }

    /// `gh` answers errors as a bare object, so valid JSON that is not a list
    /// must degrade the same way malformed output does.
    #[test]
    fn rejects_json_that_is_not_a_list() {
        assert!(parse_gh_output(b"{}", None).is_none());
    }

    /// A head branch accumulates PRs over its life. `gh pr list` answers newest
    /// first, but the open one is the live PR — a newer abandoned attempt must
    /// not shadow it.
    #[test]
    fn an_open_pr_wins_over_a_newer_closed_one() {
        let stdout = br#"[
            {"baseRefName":"main","number":9,"url":"u9","state":"CLOSED","isDraft":false},
            {"baseRefName":"main","number":4,"url":"u4","state":"OPEN","isDraft":false}
        ]"#;
        let info = parse_gh_output(stdout, None).unwrap();
        assert_eq!(info.number, 4);
        assert_eq!(info.state, PrState::Open);
    }

    #[test]
    fn a_draft_counts_as_open_when_selecting() {
        let stdout = br#"[
            {"baseRefName":"main","number":9,"url":"u9","state":"MERGED","isDraft":false},
            {"baseRefName":"main","number":4,"url":"u4","state":"OPEN","isDraft":true}
        ]"#;
        let info = parse_gh_output(stdout, None).unwrap();
        assert_eq!(info.number, 4);
        assert_eq!(info.state, PrState::Draft);
    }

    /// With nothing open, the newest attempt is the one worth painting.
    #[test]
    fn the_newest_pr_wins_when_none_are_open() {
        let stdout = br#"[
            {"baseRefName":"main","number":9,"url":"u9","state":"MERGED","isDraft":false},
            {"baseRefName":"main","number":4,"url":"u4","state":"CLOSED","isDraft":false}
        ]"#;
        let info = parse_gh_output(stdout, None).unwrap();
        assert_eq!(info.number, 9);
        assert_eq!(info.state, PrState::Merged);
    }

    #[test]
    fn parses_pr_states() {
        for (json_state, is_draft, expected) in [
            ("OPEN", false, PrState::Open),
            ("OPEN", true, PrState::Draft),
            ("MERGED", false, PrState::Merged),
            ("CLOSED", false, PrState::Closed),
            ("SOMETHING_NEW", false, PrState::Open),
        ] {
            let stdout = format!(
                r#"[{{"baseRefName":"main","number":1,"url":"https://github.com/o/r/pull/1","state":"{json_state}","isDraft":{is_draft}}}]"#
            );
            let info = parse_gh_output(stdout.as_bytes(), None).unwrap();
            assert_eq!(info.state, expected, "state={json_state} draft={is_draft}");
        }
    }

    #[test]
    fn missing_state_fields_default_to_open() {
        // Old gh versions may omit fields we didn't ask for; degrade, don't drop.
        let stdout =
            br#"[{"baseRefName":"main","number":42,"url":"https://github.com/o/r/pull/42"}]"#;
        assert_eq!(parse_gh_output(stdout, None).unwrap().state, PrState::Open);
    }

    fn pr(number: u64, state: &str, head_owner: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "baseRefName": "main",
            "url": format!("u{number}"),
            "state": state,
            "isDraft": false,
            "headRepositoryOwner": { "login": head_owner },
        })
    }

    fn number_of(pr: Option<&serde_json::Value>) -> Option<u64> {
        pr?.get("number")?.as_u64()
    }

    #[test]
    fn select_pr_prefers_an_open_pr_over_a_newer_non_open_one() {
        let prs = [pr(9, "MERGED", "someone"), pr(4, "OPEN", "someone")];
        assert_eq!(number_of(select_pr(&prs, None)), Some(4));
    }

    #[test]
    fn select_pr_takes_the_newest_when_none_are_open() {
        let prs = [pr(9, "MERGED", "someone"), pr(4, "CLOSED", "someone")];
        assert_eq!(number_of(select_pr(&prs, None)), Some(9));
    }

    /// `--head` matches the ref name in *every* head repository, so a generic
    /// branch name ("dev", "patch-1") collects strangers' PRs.  Theirs must not
    /// decide this worktree's badge or diff base, however live they are.
    #[test]
    fn select_pr_prefers_the_origin_owners_pr_over_a_strangers_open_one() {
        let prs = [pr(9, "OPEN", "stranger"), pr(4, "MERGED", "me")];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    #[test]
    fn select_pr_prefers_an_open_pr_among_the_origin_owners_own() {
        let prs = [pr(9, "MERGED", "me"), pr(7, "OPEN", "stranger"), pr(4, "OPEN", "me")];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    /// GitHub logins are case-insensitive, so a remote URL that disagrees with
    /// the API's casing still names the same account.
    #[test]
    fn select_pr_matches_the_owner_case_insensitively() {
        let prs = [pr(9, "OPEN", "stranger"), pr(4, "MERGED", "Me")];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    #[test]
    fn select_pr_falls_back_to_the_plain_policy_when_no_owner_matches() {
        let prs = [pr(9, "MERGED", "stranger"), pr(4, "OPEN", "other")];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    /// A `gh` too old to report the head owner must not filter every candidate
    /// away — an unknown owner is no evidence the PR belongs to someone else.
    #[test]
    fn select_pr_tolerates_a_missing_head_owner() {
        let prs = [serde_json::json!({"number": 4, "state": "OPEN"})];
        assert_eq!(number_of(select_pr(&prs, Some("me"))), Some(4));
    }

    #[test]
    fn select_pr_reports_nothing_for_an_empty_list() {
        assert!(select_pr(&[], Some("me")).is_none());
    }

    /// The regression this preference exists for: a stale PR opened by a
    /// stranger on the same branch name, still carrying the base branch the
    /// repository has since renamed away from.
    #[test]
    fn the_origin_owners_pr_decides_the_diff_base() {
        let stdout = br#"[
            {"baseRefName":"master","number":9,"url":"u9","state":"OPEN","isDraft":false,"headRepositoryOwner":{"login":"stranger"}},
            {"baseRefName":"main","number":4,"url":"u4","state":"MERGED","isDraft":false,"headRepositoryOwner":{"login":"me"}}
        ]"#;
        let info = parse_gh_output(stdout, Some("me")).unwrap();
        assert_eq!(info.number, 4);
        assert_eq!(info.base_branch, "main");
    }

    #[test]
    fn derives_the_slug_from_an_https_remote() {
        let slug = github_slug_from_url("https://github.com/owner/repo.git");
        assert_eq!(slug, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn derives_the_slug_from_an_scp_style_ssh_remote() {
        let slug = github_slug_from_url("git@github.com:owner/repo.git");
        assert_eq!(slug, Some(("owner".to_string(), "repo".to_string())));
    }

    /// The slug is the grouping key, so two spellings of one remote have to
    /// produce the same one or a repository splits into two requests.
    #[test]
    fn two_spellings_of_one_remote_share_a_grouping_key() {
        let with_suffix = github_slug_from_url("https://github.com/owner/repo.git");
        let without = github_slug_from_url("git@github.com:owner/repo");
        assert_eq!(with_suffix, Some(("owner".to_string(), "repo".to_string())));
        assert_eq!(with_suffix, without);
    }

    /// An `~/.ssh/config` alias is how a fork checkout picks an identity, and it
    /// hides the host it resolves to — reading it as foreign would blind the
    /// preference to exactly the layout it exists for.
    #[test]
    fn derives_the_owner_from_an_ssh_host_alias() {
        let slug = github_slug_from_url("gh:owner/repo.git");
        assert_eq!(slug.map(|(owner, _)| owner).as_deref(), Some("owner"));
    }

    #[test]
    fn rejects_a_non_github_remote() {
        assert!(github_slug_from_url("https://gitlab.com/owner/repo.git").is_none());
        assert!(github_slug_from_url("git@gitlab.com:owner/repo.git").is_none());
    }

    #[test]
    fn rejects_a_malformed_remote() {
        assert!(github_slug_from_url("").is_none());
        assert!(github_slug_from_url("not a url").is_none());
        assert!(github_slug_from_url("https://github.com/owner").is_none());
        assert!(github_slug_from_url("C:/repos/checkout").is_none());
    }

    #[test]
    fn the_wsl_batch_line_carries_the_origin_url() {
        let (url, json) = split_origin_url_line(b"gh:me/repo.git\n[]");
        assert_eq!(url, Some("gh:me/repo.git"));
        assert_eq!(json, b"[]".as_slice());
    }

    #[test]
    fn a_worktree_without_a_remote_leaves_the_wsl_line_blank() {
        let (url, json) = split_origin_url_line(b"\n[]");
        assert_eq!(url, None);
        assert_eq!(json, b"[]".as_slice());
    }

    fn sample_info() -> PrInfo {
        PrInfo {
            number: 7,
            base_branch: "main".to_string(),
            url: "https://github.com/o/r/pull/7".to_string(),
            state: PrState::Open,
        }
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
        let mut cache = PrCache::new();
        let path = PathBuf::from("/repo");
        cache.entries.insert(path.clone(), Entry {
            branch: Some("b".to_string()),
            info: Some(sample_info()),
            queried_at: Some(Duration::ZERO),
            pending: false,
            refresh_requested: false,
        });

        let ctx = egui::Context::default();
        let result = cache.poll(&path, None, &ctx);

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
        let mut cache = PrCache::new();
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
        let mut cache = PrCache::new();
        cache.set_concurrency(Some(1));
        let job = jobs::pool().spawn(jobs::Priority::Background, |_| BatchResult::new());
        bank_one(&mut cache, "/repo/wt", "main", job);
        assert_eq!(cache.in_flight(), 1);

        drain_until(&mut cache, Path::new("/repo/wt"), Duration::from_secs(5));

        assert_eq!(cache.in_flight(), 0);
    }

    /// A panicking job must free its slot the moment `drain_completed`
    /// observes `Job::failed`, not after waiting out the TTL — distinct from
    /// the TTL tests below, which backdate `started` instead of panicking.
    #[test]
    fn drain_completed_frees_a_slot_immediately_when_the_job_panics() {
        let mut cache = PrCache::new();
        cache.set_concurrency(Some(1));
        let job = jobs::Pool::new(2)
            .spawn(jobs::Priority::Background, |_| -> BatchResult { panic!("boom") });
        bank_one(&mut cache, "/repo/wt", "main", job);
        assert_eq!(cache.in_flight(), 1);

        drain_until(&mut cache, Path::new("/repo/wt"), Duration::from_secs(5));

        assert_eq!(cache.in_flight(), 0);
    }

    /// A job that never reports — a panic, or a `gh` call that hangs — must
    /// not hold its slot forever. Without the TTL backoff a capped cache
    /// would stop polling permanently.
    #[test]
    fn drain_completed_frees_a_slot_for_a_job_stuck_past_the_ttl() {
        let now = Arc::new(Mutex::new(Duration::ZERO));
        let reader = Arc::clone(&now);
        let mut cache = PrCache::with_clock(move || *reader.lock().expect("clock poisoned"));
        cache.set_concurrency(Some(1));
        let _release =
            insert_stuck_entry(&mut cache, Path::new("/repo/wt"), "main", Duration::ZERO);
        assert_eq!(cache.in_flight(), 1);

        *now.lock().expect("clock poisoned") = TTL + Duration::from_nanos(1);
        cache.drain_completed(&egui::Context::default());

        assert_eq!(cache.in_flight(), 0);
    }

    /// A job just backed off by the TTL banks no answer, so nothing but a
    /// fresh `queried_at` can hold the entry back — and the guard's repaint
    /// delivers the frame that would re-spawn it.
    #[test]
    fn a_job_stuck_past_the_ttl_leaves_the_entry_ineligible_to_respawn() {
        let now = Arc::new(Mutex::new(Duration::ZERO));
        let reader = Arc::clone(&now);
        let mut cache = PrCache::with_clock(move || *reader.lock().expect("clock poisoned"));
        let _release =
            insert_stuck_entry(&mut cache, Path::new("/repo/wt"), "main", Duration::ZERO);

        *now.lock().expect("clock poisoned") = TTL + Duration::from_nanos(1);
        cache.drain_completed(&egui::Context::default());

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
        let reader = Arc::clone(&now);
        let mut cache = PrCache::with_clock(move || *reader.lock().expect("clock poisoned"));

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
        let mut cache = PrCache::new();
        let _release =
            insert_stuck_entry(&mut cache, Path::new("/repo/pending"), "main", Duration::ZERO);

        let before = cache.generation();
        cache.drain_completed(&egui::Context::default());
        assert_eq!(cache.generation(), before, "a frame that banks nothing must not invalidate");

        let job = jobs::pool().spawn(jobs::Priority::Background, |_| BatchResult::new());
        bank_one(&mut cache, "/repo/banked", "main", job);
        drain_until(&mut cache, Path::new("/repo/banked"), Duration::from_secs(5));
        assert!(cache.generation() > before);
    }

    /// A refresh that lands while a lookup is in flight must survive it: `poll`
    /// only spawns when `pending` is empty, and the drain would otherwise stamp
    /// a fresh `queried_at` and swallow the request.
    #[test]
    fn a_refresh_during_a_lookup_survives_the_drain() {
        let mut cache = PrCache::new();
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
        let mut cache = PrCache::new();
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
        let cache = PrCache::new();
        assert!(may_spawn(cache.concurrency, cache.in_flight));
    }

    #[test]
    fn set_concurrency_clamps_zero_to_one() {
        let mut cache = PrCache::new();
        cache.set_concurrency(Some(0));
        assert!(may_spawn(cache.concurrency, 0));
        assert!(!may_spawn(cache.concurrency, 1));
    }

    /// `gh` is the slowest thing the pool runs and the least urgent.  Letting it
    /// take the last background slot puts the git status panel, which is what a
    /// user reads to decide what to do next, behind a network call.
    #[test]
    fn gh_never_takes_the_last_background_slot() {
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
    /// helper: a cold cache polls every eligible worktree in one frame.  A
    /// refused member falls due again rather than being lost.
    #[test]
    fn the_drain_respects_the_concurrency_cap() {
        let mut cache = PrCache::new();
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
        let ctx = egui::Context::default();
        cache.poll(capped, Some("feature"), &ctx);
        cache.drain_completed(&ctx);

        assert_eq!(cache.in_flight(), 1, "the cap must refuse the second request");
        assert!(cache.is_due(capped, "feature"), "a refused member must fall due again");
    }

    /// Drive a fresh context to the point where it wants no repaint of its
    /// own: it always asks for an initial one, and `run` only clears that
    /// once the request has been consumed — hence two passes.
    fn quiesce(ctx: &egui::Context) {
        let _ = ctx.run(Default::default(), |_| {});
        let _ = ctx.run(Default::default(), |_| {});
        assert!(!ctx.has_requested_repaint(), "precondition: no repaint pending");
    }

    /// The frame that queues a lookup is not the frame that spawns it — the
    /// next drain is — and egui paints on demand, so without this the request
    /// waits on the user's next input instead of on the TTL.
    #[test]
    fn a_queued_poll_asks_for_the_frame_that_spawns_it() {
        let ctx = egui::Context::default();
        quiesce(&ctx);
        let mut cache = PrCache::new();

        cache.poll(Path::new("/repo/wt"), Some("main"), &ctx);

        assert!(ctx.has_requested_repaint(), "a queued lookup must ask for its spawning frame");
    }

    /// A member the cap refuses has its `pending` cleared, so it falls due
    /// again on the very next frame.  Asking for that frame while nothing can
    /// spawn spins the UI at frame rate for as long as the batch runs, and a
    /// batch runs several serial `gh` processes.  Nothing is lost by staying
    /// quiet: the guard inside the spawn closure delivers the wake the moment
    /// a slot frees, on the panicking path too.
    #[test]
    fn a_poll_the_cap_will_refuse_does_not_ask_for_another_frame() {
        let ctx = egui::Context::default();
        let mut cache = PrCache::new();
        cache.set_concurrency(Some(1));
        let (_release, job) = spawn_stuck_job();
        bank_one(&mut cache, "/repo/busy", "main", job);
        quiesce(&ctx);

        cache.poll(Path::new("/repo/capped"), Some("feature"), &ctx);

        assert!(!ctx.has_requested_repaint(), "a saturated cap must not spin the frame loop");
    }

    /// The drain that frees a concurrency slot only runs on a frame, so a
    /// worker that exits without waking the app can stall polling for good.
    #[test]
    fn dropping_the_guard_wakes_the_app() {
        let ctx = egui::Context::default();
        // A fresh context always wants an initial repaint, and `run` only
        // clears it once that first request has been consumed — hence two
        // passes, so the assertion below can only be satisfied by the guard.
        let _ = ctx.run(Default::default(), |_| {});
        let _ = ctx.run(Default::default(), |_| {});
        assert!(!ctx.has_requested_repaint(), "precondition: no repaint pending");

        drop(RepaintOnDrop(ctx.clone()));

        assert!(ctx.has_requested_repaint());
    }

    /// The spawn has no sender of its own — the pool's channel is internal —
    /// so this drives a real job through the pool instead of hand-rolling a
    /// thread, and checks the failure the same way production code does:
    /// `poll` until `failed` latches.
    #[test]
    fn a_panicking_worker_still_wakes_the_app_and_reports_failed() {
        let ctx = egui::Context::default();
        // See `dropping_the_guard_wakes_the_app`: a fresh context needs two
        // passes before its initial repaint request is fully consumed.
        let _ = ctx.run(Default::default(), |_| {});
        let _ = ctx.run(Default::default(), |_| {});
        assert!(!ctx.has_requested_repaint(), "precondition: no repaint pending");

        let job = {
            let ctx = ctx.clone();
            jobs::Pool::new(2).spawn(jobs::Priority::Background, move |_| -> BatchResult {
                let _wake = RepaintOnDrop(ctx);
                panic!("worker died");
            })
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.failed() {
            assert!(job.poll().is_none(), "a panicking job never reports a value");
            assert!(Instant::now() < deadline, "the failure was never observed");
            thread::yield_now();
        }

        assert!(ctx.has_requested_repaint(), "a panicking unwind still wakes the app");
    }

    fn group_of(branches: &[&str]) -> Group {
        Group {
            cwd: PathBuf::from("/repo"),
            slug: Some(("owner".to_string(), "repo".to_string())),
            members: branches
                .iter()
                .map(|b| Member { path: PathBuf::from("/repo"), branch: (*b).to_string() })
                .collect(),
        }
    }

    /// A repository where nothing has a PR is the common case.  Reading its
    /// answer as a failure would spend one `gh pr list` per branch finding the
    /// same nothing, every TTL, which is the cost this batching exists to
    /// remove.
    #[test]
    fn a_good_response_with_no_prs_does_not_fall_back() {
        let group = group_of(&["topic-a", "topic-b"]);
        let sweeps = AtomicUsize::new(0);

        let found = query_group(
            &group,
            |_, _| {
                Some(br#"{"data":{"repository":{"b0":{"nodes":[]},"b1":{"nodes":[]}}}}"#.to_vec())
            },
            |_| {
                sweeps.fetch_add(1, Ordering::Relaxed);
                Some(sample_info())
            },
        );

        assert!(found.is_empty());
        assert_eq!(sweeps.load(Ordering::Relaxed), 0, "an answer of `none` is still an answer");
    }

    /// GraphQL can need scopes `gh pr list` does not, and GitHub reports a
    /// query it could not run as an HTTP 200 with a null `repository`.  Every
    /// badge in the project depends on that reading as a failure.
    #[test]
    fn a_failed_request_sweeps_the_group_per_branch() {
        let group = group_of(&["topic-a", "topic-b"]);
        let sweeps = AtomicUsize::new(0);

        let found = query_group(
            &group,
            |_, _| Some(br#"{"data":{"repository":null},"errors":[{"message":"nope"}]}"#.to_vec()),
            |_| {
                sweeps.fetch_add(1, Ordering::Relaxed);
                Some(sample_info())
            },
        );

        assert_eq!(sweeps.load(Ordering::Relaxed), 2, "one lookup per branch");
        assert_eq!(found.len(), 2);
    }

    /// A group with no repository to name — a WSL worktree, or one whose
    /// remote nothing could read — never reaches the batched form at all.
    #[test]
    fn a_group_without_a_repository_never_asks_for_a_batch() {
        let mut group = group_of(&["topic"]);
        group.slug = None;

        let found = query_group(
            &group,
            |_, _| panic!("a group with no repository has nothing to ask about"),
            |_| Some(sample_info()),
        );

        assert_eq!(found.len(), 1);
    }

    /// A cancel landing between groups has no child to kill — neither the
    /// batched request nor the per-branch sweep registers one — so the loop
    /// has to ask.  Otherwise a burst keeps forking `gh` to build an answer
    /// the drain has already backed off and nobody will read.
    #[test]
    fn run_due_stops_between_groups_once_cancelled() {
        let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().expect("temp dir")).collect();
        let due: Vec<Member> = dirs
            .iter()
            .map(|d| Member { path: d.path().to_path_buf(), branch: "topic".into() })
            .collect();
        let (tx, rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();

        let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            // Both halves of this handshake are load-bearing, for the reasons
            // spelled out in `worktree::tests::create_stops_between_steps_
            // once_cancelled`: the started signal keeps the task off the
            // pre-start skip, and the gate keeps it from racing past the first
            // check before the flag lands.
            let _ = started_tx.send(());
            let _ = gate_rx.recv();
            let _ = tx.send(run_due(due, blocking));
        });
        started_rx.recv_timeout(Duration::from_secs(5)).expect("the job never started");
        drop(job);
        let _ = gate_tx.send(());

        let out = rx.recv_timeout(Duration::from_secs(30)).expect("run_due never returned");
        assert!(out.is_empty(), "a cancelled burst kept asking: {out:?}");
    }

    /// One result covers many entries, so the drain has to fan a single map out
    /// across every path that contributed to it.
    #[test]
    fn one_banked_result_reaches_every_member() {
        let ctx = egui::Context::default();
        let mut cache = PrCache::new();
        let members =
            vec![Member { path: PathBuf::from("/repo/a"), branch: "topic-a".into() }, Member {
                path: PathBuf::from("/repo/b"),
                branch: "topic-b".into(),
            }];
        let job = jobs::Pool::new(2).spawn(jobs::Priority::Background, |_| {
            HashMap::from([
                (
                    PathBuf::from("/repo/a"),
                    Some(PrInfo {
                        number: 7,
                        base_branch: "master".into(),
                        url: "u".into(),
                        state: PrState::Open,
                    }),
                ),
                (PathBuf::from("/repo/b"), None),
            ])
        });
        cache.bank_batch(members, job);
        assert_eq!(cache.in_flight(), 1, "one request, not one per branch");

        for _ in 0..200 {
            cache.drain_completed(&ctx);
            if cache.in_flight() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(cache.in_flight(), 0, "the request never reported");

        assert_eq!(cache.state(Path::new("/repo/a"), Some("topic-a")), Some(PrState::Open));
        // Asked about and absent from the answer means no PR, not "never asked":
        // the entry must be stamped, or it re-queries on the very next frame.
        assert_eq!(cache.state(Path::new("/repo/b"), Some("topic-b")), None);
        assert!(!cache.is_due(Path::new("/repo/b"), "topic-b"), "banked as no-PR, not left due");
    }

    /// A WSL worktree has no `origin` git2 can read and no `Command` to pipe a
    /// query into.  Grouping must leave it on the per-branch path rather than
    /// dropping it, or its badge disappears.
    #[test]
    fn an_ungroupable_path_still_gets_its_own_group() {
        let dir = tempfile::tempdir().expect("temp dir");
        let due = vec![Member { path: dir.path().to_path_buf(), branch: "topic".into() }];
        let resolves = AtomicUsize::new(0);

        let out = groups_with(due, |_| {
            resolves.fetch_add(1, Ordering::Relaxed);
            Some(("resolved".to_string(), "repo".to_string()))
        });

        assert_eq!(out.len(), 1);
        assert!(out[0].slug.is_none(), "a repo with no readable origin cannot be grouped");
        assert_eq!(out[0].members.len(), 1);
        assert_eq!(resolves.load(Ordering::Relaxed), 0, "and costs no `gh` process to find out");
    }

    /// `origin` on a fork checkout names the fork, and a pull request opened
    /// from it belongs to the repository it targets.  Asking the fork finds
    /// nothing, so the group must ask whatever `gh` resolves instead — once
    /// for the repository, not once per worktree.
    #[test]
    fn a_group_asks_the_resolved_repository_not_its_origin() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = init_repo(&dir.path().join("main"));
        repo.remote("origin", "https://github.com/me/fork.git").expect("remote");
        let linked = add_worktree(&repo, "topic-b");
        let due =
            vec![Member { path: dir.path().join("main"), branch: "topic-a".into() }, Member {
                path: linked,
                branch: "topic-b".into(),
            }];
        let resolves = AtomicUsize::new(0);

        let out = groups_with(due, |_| {
            resolves.fetch_add(1, Ordering::Relaxed);
            Some(("upstream".to_string(), "repo".to_string()))
        });

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].slug, Some(("upstream".to_string(), "repo".to_string())));
        assert_eq!(resolves.load(Ordering::Relaxed), 1, "one resolve for the whole repository");
    }

    /// Nothing groups a worktree whose repository cannot be resolved, so it
    /// keeps the per-branch path rather than losing its badge.
    #[test]
    fn a_repository_that_does_not_resolve_falls_back_to_per_branch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = init_repo(dir.path());
        repo.remote("origin", "https://github.com/owner/repo.git").expect("remote");
        let due = vec![Member { path: dir.path().to_path_buf(), branch: "topic".into() }];

        let out = groups_with(due, |_| None);

        assert_eq!(out.len(), 1);
        assert!(out[0].slug.is_none());
        assert_eq!(out[0].members.len(), 1);
    }

    #[test]
    fn reads_the_repository_gh_resolved() {
        let slug = parse_name_with_owner(br#"{"nameWithOwner":"mathix420/alacritree"}"#);
        assert_eq!(slug, Some(("mathix420".to_string(), "alacritree".to_string())));
    }

    #[test]
    fn rejects_output_that_names_no_repository() {
        assert!(parse_name_with_owner(b"").is_none());
        assert!(parse_name_with_owner(b"{}").is_none());
        assert!(parse_name_with_owner(br#"{"nameWithOwner":"alacritree"}"#).is_none());
        assert!(parse_name_with_owner(br#"{"nameWithOwner":"/alacritree"}"#).is_none());
    }

    /// Branches of one repository share a request; a chunk boundary splits them
    /// into two rather than growing one request without limit.
    #[test]
    fn one_repository_chunks_at_the_limit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = init_repo(dir.path());
        repo.remote("origin", "https://github.com/owner/repo.git").expect("remote");
        let due: Vec<Member> = (0..pr_query::CHUNK + 1)
            .map(|i| Member { path: dir.path().to_path_buf(), branch: format!("b{i}") })
            .collect();

        let out = groups_with(due, |_| Some(("owner".to_string(), "repo".to_string())));

        assert_eq!(out.len(), 2, "one chunk over the limit is two requests");
        assert!(out.iter().all(|g| g.slug == Some(("owner".into(), "repo".into()))));
        assert_eq!(out.iter().map(|g| g.members.len()).sum::<usize>(), pr_query::CHUNK + 1);
    }
}
