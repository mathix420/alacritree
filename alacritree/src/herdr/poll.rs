//! Asking each reachable herdr server what it has, on a schedule that backs
//! off a side that never answers and recovers one that starts answering
//! again.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::{jobs, wsl};

use super::cli::list_panes;
use super::{Agent, Listing, PollError, Settings, Side, running_session_name, settings};

/// How long an endpoint known to have a herdr waits before being retried.
const RECOVERY_RETRY: Duration = Duration::from_secs(30);

/// How many polls in a row a side may fail before what it last reported is
/// dropped.  Counted in polls rather than in seconds so a slower cadence waits
/// proportionally longer, rather than giving its rows up on one missed turn.
const GRACE_POLLS: u32 = 3;

/// Whether an endpoint is worth talking to.  A side with no herdr on it is
/// abandoned, so a machine with none pays one failed spawn rather than one
/// per tick; a side that has a herdr is retried forever, because starting the
/// server is the ordinary thing to do after alacritree is already open.
#[derive(Debug, Default)]
pub struct Reach {
    ever_answered: bool,
    failing: bool,
    /// Whether the last failure was one that waiting cannot fix.
    absent: bool,
    last_error: Option<String>,
}

impl Reach {
    /// Whether to poll again, given how long it has been since the last try.
    pub fn should_retry(&self, since_last: Duration) -> bool {
        if !self.failing {
            return true;
        }
        !self.abandoned() && since_last >= RECOVERY_RETRY
    }

    /// Whether this endpoint has been given up on for the process lifetime:
    /// no herdr has ever spoken from it, and the last try found none there.
    pub fn abandoned(&self) -> bool {
        self.failing && self.absent && !self.ever_answered
    }

    pub fn record_success(&mut self) {
        self.ever_answered = true;
        self.failing = false;
        self.absent = false;
        self.last_error = None;
    }

    /// Records a failure, returning whether it is worth logging — a code
    /// repeating every tick is logged once, not once per poll.
    pub fn record_failure(&mut self, error: &PollError) -> bool {
        self.failing = true;
        self.absent = matches!(error, PollError::Absent(_));
        let novel = self.last_error.as_deref() != Some(error.code());
        self.last_error = Some(error.code().to_string());
        novel
    }
}

pub(super) struct ListingReply {
    sampled_at: Instant,
    listing: Listing,
    agents: Vec<Agent>,
    inventory: Option<Result<HashSet<String>, PollError>>,
}

impl ListingReply {
    pub(super) fn parse(
        stdout: &str,
        listing: Listing,
        sampled_at: Instant,
        attached: bool,
    ) -> Self {
        Self {
            sampled_at,
            listing,
            agents: listing.parse(stdout),
            inventory: (attached && listing == Listing::Panes)
                .then(|| parse_pane_inventory(stdout)),
        }
    }
}

/// Full terminal membership from one successful pane-list request.
pub struct PaneInventory {
    pub sampled_at: Instant,
    pub terminal_ids: HashSet<String>,
}

fn parse_pane_inventory(stdout: &str) -> Result<HashSet<String>, PollError> {
    let malformed = || PollError::Absent("invalid_pane_inventory");
    let envelope: serde_json::Value = serde_json::from_str(stdout).map_err(|_| malformed())?;
    let envelope = envelope.as_object().ok_or_else(malformed)?;
    if envelope.contains_key("error") {
        return Err(malformed());
    }
    let panes = envelope
        .get("result")
        .and_then(|result| result.get("panes"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(malformed)?;
    panes
        .iter()
        .map(|pane| {
            pane.get("terminal_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(malformed)
        })
        .collect()
}

/// Last known pane details, with live fields valid only in a current reply.
pub struct PaneMetadata {
    pub agent: Agent,
    pub current: bool,
}

/// One herdr server's agents, refreshed off the UI thread.
pub struct EndpointCache {
    side: Side,
    agents: Vec<Agent>,
    attachment_panes: Vec<PaneMetadata>,
    generation: u64,
    reach: Reach,
    last_attempt: Option<Instant>,
    /// When a run of failed listings stops being worth waiting out.  Set on
    /// the first failure of the run and cleared by the next answer.
    blank_at: Option<Instant>,
    sampled_at: Option<Instant>,
    inventory: Option<PaneInventory>,
    pending: Option<jobs::Job<Result<ListingReply, PollError>>>,
    settings: Read<Settings>,
    session_name: Read<String>,
}

impl EndpointCache {
    pub fn new(side: Side) -> Self {
        Self {
            side,
            agents: Vec::new(),
            attachment_panes: Vec::new(),
            generation: 0,
            reach: Reach::default(),
            last_attempt: None,
            blank_at: None,
            sampled_at: None,
            inventory: None,
            pending: None,
            settings: Read::Unread,
            session_name: Read::Unread,
        }
    }

    /// Bumped only when a rendered field changes, so the sidebar's per-frame
    /// comparison does not rebuild for agent churn nobody can see.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn side(&self) -> &Side {
        &self.side
    }

    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// Unfiltered attachment details survive an unavailable display poll.
    pub fn attachment_pane(&self, terminal_id: &str) -> Option<&PaneMetadata> {
        self.attachment_panes.iter().find(|pane| pane.agent.terminal_id == terminal_id)
    }

    /// When the successful listing began, so a focus change can reject a
    /// reply that was already in flight before it landed.
    pub fn sampled_at(&self) -> Option<Instant> {
        self.sampled_at
    }

    pub fn inventory(&self) -> Option<&PaneInventory> {
        self.inventory.as_ref()
    }

    #[cfg(test)]
    pub fn set_agents_for_test(&mut self, agents: Vec<Agent>) {
        self.agents = agents;
    }

    /// A cache holding one listing at a chosen sample time, for tests that
    /// drive `HerdrViewSync` without a poll behind them.
    #[cfg(test)]
    pub fn for_test(side: Side, agents: Vec<Agent>, sampled_at: Instant) -> Self {
        let mut cache = Self::new(side);
        cache.agents = agents;
        cache.sampled_at = Some(sampled_at);
        cache
    }

    /// One listing driven to completion, for tests that need a cache in the
    /// state a given reply leaves it in.
    ///
    /// A failure handed straight in is one on a side that has been failing
    /// long enough to have given its rows up; the grace period holding the
    /// first of a run is its own concern, and `fail_listing_for_test` drives
    /// that.
    #[cfg(test)]
    pub fn complete_listing_for_test(
        &mut self,
        result: Result<&str, PollError>,
        listing: Listing,
        display: Listing,
        sampled_at: Instant,
    ) {
        if result.is_err() {
            self.blank_at = Some(Instant::now());
        }
        self.settings = Read::Done(Settings::default());
        self.session_name = Read::Done("fixture".into());
        self.last_attempt = Some(Instant::now());
        self.pending = Some(jobs::Job::ready(
            result.map(|stdout| ListingReply::parse(stdout, listing, sampled_at, true)),
        ));
        self.poll(Duration::from_secs(60), display, true);
    }

    /// One listing that did not answer, with the grace period holding what
    /// herdr last reported still running.
    #[cfg(test)]
    pub fn fail_listing_for_test(&mut self, error: PollError, interval: Duration) {
        self.settings = Read::Done(Settings::default());
        self.session_name = Read::Done("fixture".into());
        self.last_attempt = Some(Instant::now());
        self.pending = Some(jobs::Job::ready(Err(error)));
        self.poll(interval, Listing::Agents, true);
    }

    /// Ages the current run of failures past its grace period, so a test
    /// reaches the state a side that stopped answering ends up in without
    /// waiting the polls out.
    #[cfg(test)]
    pub fn expire_grace_for_test(&mut self) {
        self.blank_at = Some(Instant::now());
    }

    fn adopt_reply(&mut self, reply: ListingReply, display: Listing, attached: bool) {
        let mut agents = reply.agents;
        self.sampled_at = Some(reply.sampled_at);
        if let Some(inventory) = reply.inventory.filter(|_| attached) {
            match inventory {
                Ok(terminal_ids) => {
                    self.attachment_panes
                        .retain(|pane| terminal_ids.contains(&pane.agent.terminal_id));
                    for pane in &mut self.attachment_panes {
                        pane.current = false;
                        pane.agent.status = None;
                        pane.agent.focused = false;
                    }
                    for agent in &agents {
                        if let Some(pane) = self
                            .attachment_panes
                            .iter_mut()
                            .find(|pane| pane.agent.terminal_id == agent.terminal_id)
                        {
                            *pane = PaneMetadata { agent: agent.clone(), current: true };
                        } else {
                            self.attachment_panes
                                .push(PaneMetadata { agent: agent.clone(), current: true });
                        }
                    }
                    self.note_success();
                    if self.inventory.as_ref().is_none_or(|old| old.terminal_ids != terminal_ids) {
                        log::debug!(
                            "herdr inventory side={:?} sampled_at={:?} terminal_ids={:?}",
                            self.side,
                            reply.sampled_at,
                            terminal_ids
                        );
                    }
                    self.inventory =
                        Some(PaneInventory { sampled_at: reply.sampled_at, terminal_ids });
                },
                Err(error) => {
                    self.sampled_at = None;
                    self.note_failure(&error);
                },
            }
        } else {
            self.note_success();
            self.inventory = None;
            if attached {
                for pane in &mut self.attachment_panes {
                    pane.current = false;
                    pane.agent.status = None;
                    pane.agent.focused = false;
                }
            } else {
                self.attachment_panes.clear();
            }
        }
        if display == Listing::Agents && reply.listing == Listing::Panes {
            agents.retain(|agent| agent.status.is_some());
        }
        if rendered_differs(&self.agents, &agents) {
            self.generation = self.generation.wrapping_add(1);
        }
        self.agents = agents;
    }

    /// What herdr's config here says, once the read has landed.  Before that
    /// it is herdr's own defaults, which is what herdr would be running on if
    /// its config said nothing — except for the chord, which stays `None`
    /// until it is known, since naming one the user has rebound would be
    /// worse than naming none.
    pub fn settings(&self) -> Settings {
        match &self.settings {
            Read::Done(settings) => settings.clone(),
            Read::Unread | Read::Pending(_) => Settings::default(),
        }
    }

    /// The running session's name here, once the read has landed.  `None`
    /// means the attach has to ask herdr itself.
    pub fn session_name(&self) -> Option<String> {
        match &self.session_name {
            Read::Done(name) => Some(name.clone()),
            Read::Unread | Read::Pending(_) => None,
        }
    }

    /// Adopt a landed session-name read.  A read that answered nothing goes
    /// back to unread rather than to a guess: the name is what an attach
    /// targets, so asking again next tick beats attaching to a name herdr
    /// never gave.
    fn advance_session_name(&mut self) {
        let Read::Pending(job) = &self.session_name else { return };
        match job.poll() {
            Some(Some(name)) => self.session_name = Read::Done(name),
            Some(None) => self.session_name = Read::Unread,
            None if job.failed() => self.session_name = Read::Unread,
            None => {},
        }
    }

    /// Learn the session name in the background.  The attach gesture runs on
    /// the UI thread, and on a side where herdr cannot attach one agent it
    /// already spends a spawn focusing the pane; asking for a name that is
    /// the same on every click would double that wait.
    fn start_session_name_read(&mut self) {
        if !matches!(self.session_name, Read::Unread) {
            return;
        }
        let side = self.side.clone();
        self.session_name = Read::Pending(
            jobs::pool()
                .spawn(jobs::Priority::Background, move |_| running_session_name(&side).ok()),
        );
    }

    /// Adopt a landed config read.  Both halves are part of what a row
    /// renders, so arriving late still has to invalidate the sidebar's
    /// comparison.  A side that could not be read falls back to herdr's own
    /// defaults rather than to nothing.
    fn advance_settings(&mut self) {
        let Read::Pending(job) = &self.settings else { return };
        if let Some(settings) = job.poll() {
            self.settings = Read::Done(settings.unwrap_or_default());
            self.generation = self.generation.wrapping_add(1);
        } else if job.failed() {
            self.settings = Read::Done(Settings::default());
        }
    }

    /// Read the config once this endpoint has proved a herdr lives here.
    /// Starting at construction would run a shell inside every installed
    /// distro to learn values no row will ever show.
    fn start_settings_read(&mut self) {
        if !matches!(self.settings, Read::Unread) {
            return;
        }
        let side = self.side.clone();
        self.settings = Read::Pending(
            jobs::pool()
                .spawn(jobs::Priority::Background, move |blocking| settings(&side, blocking)),
        );
    }

    /// Adopts a landed result and starts a new poll when due.  Never blocks.
    pub fn poll(&mut self, interval: Duration, listing: Listing, attached: bool) {
        if attached && self.attachment_panes.is_empty() {
            self.attachment_panes.extend(
                self.agents
                    .iter()
                    .cloned()
                    .map(|agent| PaneMetadata { agent, current: self.sampled_at.is_some() }),
            );
        }
        self.advance_settings();
        self.advance_session_name();
        if let Some(job) = &self.pending {
            match job.poll() {
                Some(Ok(reply)) => {
                    self.adopt_reply(reply, listing, attached);
                    self.start_settings_read();
                    self.start_session_name_read();
                    self.pending = None;
                },
                Some(Err(error)) => {
                    self.sampled_at = None;
                    self.note_missing_listing(&error, interval);
                    // herdr restarting may name its session differently, and
                    // attaching to the old name reaches nothing.
                    self.session_name = Read::Unread;
                    self.pending = None;
                },
                // A worker panic supplies no membership evidence. Attached
                // sessions still need retries on the configured cadence.
                None if job.failed() => {
                    self.sampled_at = None;
                    self.note_missing_listing(&PollError::Absent("poll_panicked"), interval);
                    self.pending = None;
                },
                None => return,
            }
        }

        if !attached {
            self.inventory = None;
            self.attachment_panes.clear();
        }
        if !self.poll_due(interval, attached) {
            return;
        }
        self.last_attempt = Some(Instant::now());
        let side = self.side.clone();
        let listing = if attached { Listing::Panes } else { listing };
        self.pending = Some(jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            list_panes(&side, listing, attached, blocking)
        }));
    }

    fn poll_due(&self, interval: Duration, attached: bool) -> bool {
        let since = self.last_attempt.map_or(interval, |t| t.elapsed());
        since >= interval && (attached || self.reach.should_retry(since))
    }

    /// Records a reply that landed but could not be read.  Something did
    /// answer here, so it is evidence about this side, and what it displaces
    /// goes at once.
    fn note_failure(&mut self, error: &PollError) {
        self.inventory = None;
        self.forget_listing();
        self.log_failure(error);
    }

    /// Records a listing that never answered.  A poll that could not run is no
    /// evidence about the agents, since herdr's own state is untouched by a
    /// process that failed to spawn, so what it last said stands until the
    /// failures outlast [`GRACE_POLLS`] of them.  Giving the rows up on the
    /// first trades a rare stale status for a certain blank whenever a spawn
    /// hiccups, which on a loaded machine is the common case.
    fn note_missing_listing(&mut self, error: &PollError, interval: Duration) {
        // Membership is the exception: a pane is removed on a listing that
        // carries every pane but that one, which a failure is not.
        self.inventory = None;
        let blank_at =
            *self.blank_at.get_or_insert_with(|| Instant::now() + interval * GRACE_POLLS);
        if Instant::now() >= blank_at {
            self.forget_listing();
            if !self.agents.is_empty() {
                self.agents.clear();
                self.generation = self.generation.wrapping_add(1);
            }
        }
        self.log_failure(error);
    }

    /// Drops the live half of what herdr last said about this side's panes.
    /// A status nothing is refreshing still reads as current, which is worse
    /// than showing none at all.
    fn forget_listing(&mut self) {
        for pane in &mut self.attachment_panes {
            pane.current = false;
            pane.agent.status = None;
            pane.agent.focused = false;
        }
    }

    fn note_success(&mut self) {
        self.reach.record_success();
        self.blank_at = None;
    }

    /// Says a poll produced no agents, once.  A novel code that is not the
    /// ordinary "no server here" is a warning; giving up on an endpoint is a
    /// debug line, so a herdr that is installed but never answers can still be
    /// explained from a log rather than only by an empty sidebar.  A code that
    /// repeats is logged the first time only, so an endpoint retried for the
    /// whole session still costs one line.
    fn log_failure(&mut self, error: &PollError) {
        let code = error.code();
        let novel = self.reach.record_failure(error);
        if novel && code != "server_not_running" {
            log::warn!("herdr ({:?}): {code}", self.side);
        }
        if novel && self.reach.abandoned() {
            log::debug!(
                "herdr ({:?}): {code}; only attached sessions will retry this endpoint",
                self.side
            );
        }
    }
}

/// How long a listing of running distros stands before it is taken again.
/// Starting a distro is a human action, so noticing one on a slower clock
/// than the agent poll's is enough, and each listing is its own `wsl.exe`
/// spawn.
const DISTRO_REFRESH: Duration = Duration::from_secs(10);

/// Odd multiplier (the 64-bit golden ratio) that lifts a membership change
/// clear of the per-endpoint generation steps it is summed with.
const MEMBERSHIP_SCALE: u64 = 0x9E37_79B9_7F4A_7C15;

/// Every herdr server alacritree talks to.  The native side is permanent; a
/// WSL side exists only while its distro's VM is up, because reaching into a
/// stopped distro boots it — seconds of disk I/O and a VM's worth of memory —
/// only to find that nothing is listening there.
pub struct Endpoints {
    caches: Vec<EndpointCache>,
    /// Moves whenever an endpoint joins or leaves, so a set change reaches the
    /// sidebar even when the generations it replaces happen to sum the same.
    membership: u64,
    running: Option<jobs::Job<Option<Vec<String>>>>,
    /// Latches while listings keep failing, so the reason is logged once
    /// rather than every refresh.
    listing_failed: bool,
    last_refresh: Option<Instant>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            caches: vec![EndpointCache::new(Side::Native)],
            membership: 0,
            running: None,
            listing_failed: false,
            last_refresh: None,
        }
    }
}

impl Endpoints {
    #[cfg(test)]
    pub fn caches_mut_for_test(&mut self) -> &mut Vec<EndpointCache> {
        &mut self.caches
    }

    pub fn caches(&self) -> &[EndpointCache] {
        &self.caches
    }

    /// One number standing for every endpoint's rendered state, so the
    /// sidebar's per-frame comparison stays a `u64` compare.
    ///
    /// Membership enters scaled rather than added: an endpoint leaving takes
    /// its own generation out of the sum, and a membership step of one would
    /// cancel exactly against the endpoint whose generation is one — which is
    /// what a cache holds the moment its first agent lands.
    pub fn generation(&self) -> u64 {
        let membership = self.membership.wrapping_mul(MEMBERSHIP_SCALE);
        self.caches.iter().map(EndpointCache::generation).fold(membership, u64::wrapping_add)
    }

    /// Refreshes the endpoint set and each endpoint's agents.  Never blocks.
    pub fn poll(&mut self, interval: Duration, listing: Listing, attached: impl Fn(&Side) -> bool) {
        self.refresh_running();
        for cache in &mut self.caches {
            let has_attachments = attached(cache.side());
            cache.poll(interval, listing, has_attachments);
        }
    }

    /// Keeps the endpoint set in step with which distros are running.  The
    /// listing itself spawns `wsl.exe`, so it goes through the pool.
    fn refresh_running(&mut self) {
        if let Some(job) = &self.running {
            match job.poll() {
                Some(listing) => {
                    self.adopt_listing(listing);
                    self.running = None;
                },
                None if job.failed() => {
                    self.adopt_listing(None);
                    self.running = None;
                },
                None => return,
            }
        }
        if self.last_refresh.is_some_and(|t| t.elapsed() < DISTRO_REFRESH) {
            return;
        }
        self.last_refresh = Some(Instant::now());
        // A machine with nothing registered has nothing to start, so it never
        // pays for the listing at all.
        if wsl::distros().is_empty() {
            return;
        }
        self.running = Some(jobs::pool().spawn(jobs::Priority::Background, wsl::running_distros));
    }

    /// Takes what the listing job answered.  A listing that failed is not
    /// evidence that nothing is running, so it leaves the endpoint set — and
    /// every endpoint's cached agents and backoff state — exactly as it is;
    /// only an answer may remove an endpoint.  The failure is logged once per
    /// run of failures, so a `wsl.exe` that cannot list at all says so without
    /// writing a line every refresh.
    fn adopt_listing(&mut self, listing: Option<Vec<String>>) {
        match listing {
            Some(running) => {
                self.listing_failed = false;
                self.adopt_running(&running);
            },
            None => {
                for cache in &mut self.caches {
                    if matches!(cache.side, Side::Wsl(_)) {
                        cache.inventory = None;
                    }
                }
                if !self.listing_failed {
                    log::debug!("herdr: listing running distros failed; keeping the endpoints");
                    self.listing_failed = true;
                }
            },
        }
    }

    /// Adopts a listing of running distros: an endpoint appears when its
    /// distro starts and goes when it stops, since a stopped distro's agents
    /// went down with its VM.  The native endpoint is never one of them.
    fn adopt_running(&mut self, running: &[String]) {
        let before = self.caches.len();
        self.caches.retain(|cache| match cache.side() {
            Side::Native => true,
            Side::Wsl(distro) => running.iter().any(|name| name == distro),
        });
        let mut changed = self.caches.len() != before;
        for distro in running {
            let side = Side::Wsl(distro.clone());
            if !self.caches.iter().any(|cache| *cache.side() == side) {
                self.caches.push(EndpointCache::new(side));
                changed = true;
            }
        }
        if changed {
            self.membership = self.membership.wrapping_add(1);
        }
    }
}

/// Whether anything the sidebar draws changed.  Named field by field rather
/// than a whole-struct compare, so a field herdr reports that no row shows
/// cannot force the tree to rebuild.
fn rendered_differs(was: &[Agent], now: &[Agent]) -> bool {
    was.len() != now.len()
        || was.iter().zip(now).any(|(a, b)| {
            a.terminal_id != b.terminal_id
                || a.status != b.status
                || a.kind != b.kind
                || a.title != b.title
                || a.focused != b.focused
                || a.cwd != b.cwd
                || a.foreground_cwd != b.foreground_cwd
                || a.pane_id != b.pane_id
        })
}

/// A config read runs once per endpoint: herdr rereads its own config only on
/// request, and re-running a shell inside every distro on the listing cadence
/// would cost a process per distro per tick to learn values that do not move.
enum Read<T> {
    Unread,
    Pending(jobs::Job<Option<T>>),
    Done(T),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Status;

    /// One working agent, for the tests about what survives a poll that did
    /// not answer.
    const LISTING: &str = r#"{"result":{"panes":[
        {"terminal_id":"agent","pane_id":"w1:p1","agent":"claude","agent_status":"working"}
    ]}}"#;

    #[test]
    fn attachment_metadata_survives_a_side_that_stopped_answering() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.settings = Read::Done(Settings::default());
        cache.session_name = Read::Done("fixture".into());
        cache.last_attempt = Some(Instant::now());
        cache.pending = Some(jobs::Job::ready(Ok(ListingReply::parse(
            r#"{"result":{"agents":[{"terminal_id":"agent","pane_id":"w1:p1","agent":"claude","agent_status":"working","terminal_title_stripped":"review work"}]}}"#,
            Listing::Agents,
            Instant::now(),
            false,
        ))));
        cache.poll(Duration::from_secs(60), Listing::Agents, false);
        assert!(cache.attachment_panes.is_empty());
        assert_eq!(cache.agents()[0].title.as_deref(), Some("review work"));

        cache.complete_listing_for_test(
            Err(PollError::Absent("spawn_failed")),
            Listing::Panes,
            Listing::Agents,
            Instant::now(),
        );

        let pane =
            cache.attachment_pane("agent").expect("the bound pane retains its known details");
        assert_eq!(pane.agent.title.as_deref(), Some("review work"));
        assert!(!pane.current);
        assert!(pane.agent.status.is_none());
        assert!(!pane.agent.focused);
    }

    /// A spawn that could not run is not evidence that the agents went away,
    /// and the poll two seconds behind it usually answers.  Blanking on the
    /// first failure is what a loaded machine shows: every status on the side
    /// goes at once, and comes back a poll or two later.
    #[test]
    fn one_failed_listing_keeps_what_herdr_last_reported() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.complete_listing_for_test(
            Ok(LISTING),
            Listing::Panes,
            Listing::Panes,
            Instant::now(),
        );

        cache.fail_listing_for_test(PollError::Absent("spawn_failed"), Duration::from_secs(2));

        assert_eq!(cache.agents().len(), 1);
        assert_eq!(cache.agents()[0].status, Some(Status::Working));
        let pane = cache.attachment_pane("agent").expect("the pane keeps its row");
        assert_eq!(pane.agent.status, Some(Status::Working));
        assert!(pane.current);
        // Membership is not held back: a failure may never remove a pane, and
        // holding the last listing as evidence would let it.
        assert!(cache.inventory().is_none());
    }

    /// A side that has stopped answering has to give its rows up in the end,
    /// or a herdr that went down leaves a status nobody is refreshing on
    /// screen for the rest of the session.
    #[test]
    fn listings_that_keep_failing_drop_what_they_knew() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.complete_listing_for_test(
            Ok(LISTING),
            Listing::Panes,
            Listing::Panes,
            Instant::now(),
        );
        let interval = Duration::from_secs(2);
        cache.fail_listing_for_test(PollError::Absent("spawn_failed"), interval);
        cache.expire_grace_for_test();

        cache.fail_listing_for_test(PollError::Absent("spawn_failed"), interval);

        assert!(cache.agents().is_empty());
        let pane = cache.attachment_pane("agent").expect("the bound pane keeps its row");
        assert!(pane.agent.status.is_none());
        assert!(!pane.current);
    }

    /// The run is measured from its first failure, so an answer in between
    /// has to start the next run over rather than leaving the side one miss
    /// away from blanking for the rest of the session.
    #[test]
    fn an_answer_between_failures_starts_the_grace_over() {
        let mut cache = EndpointCache::new(Side::Native);
        let interval = Duration::from_secs(2);
        cache.fail_listing_for_test(PollError::Absent("spawn_failed"), interval);
        cache.expire_grace_for_test();

        cache.complete_listing_for_test(
            Ok(LISTING),
            Listing::Panes,
            Listing::Panes,
            Instant::now(),
        );
        cache.fail_listing_for_test(PollError::Absent("spawn_failed"), interval);

        assert_eq!(cache.agents()[0].status, Some(Status::Working));
    }

    #[test]
    fn attachment_metadata_is_released_without_bindings() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.complete_listing_for_test(
            Ok(r#"{"result":{"panes":[{"terminal_id":"shell","pane_id":"w1:p1"}]}}"#),
            Listing::Panes,
            Listing::Agents,
            Instant::now(),
        );
        assert!(cache.attachment_pane("shell").is_some());
        cache.poll(Duration::from_secs(60), Listing::Agents, false);
        assert!(cache.attachment_pane("shell").is_none());
        assert!(cache.pending.is_none());
    }

    #[test]
    fn inventory_adoption_keeps_the_request_timestamp_when_display_changes_in_flight() {
        let mut cache = EndpointCache::new(Side::Native);
        let started = Instant::now() - Duration::from_secs(1);
        cache.complete_listing_for_test(
            Ok(r#"{"result":{"panes":[
            {"terminal_id":"shell","pane_id":"w1:p1"},
            {"terminal_id":"agent","pane_id":"w1:p2","agent":"codex","agent_status":"idle"}
        ]}}"#),
            Listing::Panes,
            Listing::Agents,
            started,
        );

        assert_eq!(cache.inventory().unwrap().sampled_at, started);
        assert_eq!(cache.inventory().unwrap().terminal_ids.len(), 2);
        assert_eq!(cache.agents()[0].terminal_id, "agent");
        assert_eq!(cache.agents().len(), 1);

        cache.complete_listing_for_test(
            Ok(r#"{"result":{"agents":[]}}"#),
            Listing::Agents,
            Listing::Panes,
            started,
        );
        assert!(cache.inventory().is_none());
    }

    #[test]
    fn attached_inventory_retries_failed_and_malformed_polls_at_the_configured_interval() {
        let interval = Duration::from_secs(2);
        for result in [
            Err(PollError::Absent("spawn_failed")),
            Err(PollError::Server("server_not_running".into())),
            Ok("invalid json"),
        ] {
            let mut cache = EndpointCache::new(Side::Native);
            cache.complete_listing_for_test(
                result,
                Listing::Panes,
                Listing::Agents,
                Instant::now(),
            );
            assert!(cache.inventory().is_none());
            assert!(!cache.poll_due(interval, true));
            cache.last_attempt = Some(Instant::now() - interval);
            assert!(cache.poll_due(interval, true));
            assert!(!cache.poll_due(interval, false));
        }
    }

    #[test]
    fn failed_inventory_jobs_invalidate_success_and_keep_attached_retries_alive() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.complete_listing_for_test(
            Ok(r#"{"result":{"panes":[]}}"#),
            Listing::Panes,
            Listing::Panes,
            Instant::now(),
        );
        assert!(cache.inventory().is_some());
        cache.pending = Some(jobs::Job::panicked());

        cache.poll(Duration::from_secs(60), Listing::Panes, true);

        assert!(cache.inventory().is_none());
        assert!(cache.sampled_at().is_none());
        assert!(cache.pending.is_none());
        cache.last_attempt = Some(Instant::now() - Duration::from_secs(2));
        assert!(cache.poll_due(Duration::from_secs(2), true));
    }

    #[test]
    fn inventory_unchanged_frames_do_not_request_immediate_polls() {
        let mut cache = EndpointCache::new(Side::Native);
        let started = Instant::now();
        cache.complete_listing_for_test(
            Ok(r#"{"result":{"panes":[]}}"#),
            Listing::Panes,
            Listing::Panes,
            started,
        );
        for _ in 0..20 {
            cache.poll(Duration::from_secs(60), Listing::Panes, true);
            assert!(cache.pending.is_none());
            assert_eq!(cache.inventory().unwrap().sampled_at, started);
        }
        cache.poll(Duration::from_secs(60), Listing::Panes, false);
        assert!(cache.inventory().is_none());
        assert!(cache.pending.is_none());
    }

    #[test]
    fn pane_display_without_attachments_does_not_build_an_inventory() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.settings = Read::Done(Settings::default());
        cache.session_name = Read::Done("fixture".into());
        cache.last_attempt = Some(Instant::now());
        cache.pending = Some(jobs::Job::ready(Ok(ListingReply::parse(
            r#"{"result":{"panes":[{"terminal_id":"shell","pane_id":"w1:p1"}]}}"#,
            Listing::Panes,
            Instant::now(),
            false,
        ))));

        cache.poll(Duration::from_secs(60), Listing::Panes, false);

        assert!(cache.inventory().is_none());
        assert_eq!(cache.agents()[0].terminal_id, "shell");
    }

    #[test]
    fn failed_distro_listing_invalidates_only_wsl_inventory_evidence() {
        let mut endpoints = Endpoints::default();
        endpoints.adopt_running(&["ubuntu".into()]);
        for cache in &mut endpoints.caches {
            cache.complete_listing_for_test(
                Ok(r#"{"result":{"panes":[]}}"#),
                Listing::Panes,
                Listing::Panes,
                Instant::now(),
            );
        }

        endpoints.adopt_listing(None);

        assert!(endpoints.caches[0].inventory().is_some());
        assert!(endpoints.caches[1].inventory().is_none());
        endpoints.adopt_running(&[]);
        assert_eq!(endpoints.caches.len(), 1);
    }

    #[test]
    fn unchanged_listings_advance_freshness_without_rebuilding_rows() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.settings = Read::Done(Settings::default());
        cache.session_name = Read::Done("fixture".into());
        let first = Instant::now() - Duration::from_secs(2);
        let second = first + Duration::from_secs(1);
        let mut first_generation = None;
        for started in [first, second] {
            cache.last_attempt = Some(started);
            cache.pending = Some(jobs::Job::ready(Ok(ListingReply::parse(
                r#"{"result":{"panes":[
                        {"terminal_id":"t2","pane_id":"w2:p1","tab_id":"w2:t1","focused":true}
                    ]}}"#,
                Listing::Panes,
                started,
                true,
            ))));
            let deadline = Instant::now() + Duration::from_secs(2);
            while cache.pending.is_some() {
                cache.poll(Duration::from_secs(60), Listing::Panes, true);
                assert!(Instant::now() < deadline, "listing job did not settle");
                std::thread::yield_now();
            }
            assert_eq!(cache.sampled_at(), Some(started));
            assert_eq!(cache.agents()[0].terminal_id, "t2");
            assert!(cache.agents()[0].focused);
            if let Some(generation) = first_generation {
                assert_eq!(cache.generation(), generation);
            } else {
                first_generation = Some(cache.generation());
            }
        }
    }

    use std::time::Duration;

    #[test]
    fn an_endpoint_with_no_herdr_is_given_up_on() {
        let mut reach = Reach::default();
        reach.record_failure(&PollError::Absent("spawn_failed"));
        assert!(!reach.should_retry(Duration::from_secs(3600)));
    }

    /// The common way to meet herdr is to start it after alacritree, and an
    /// endpoint that has only ever answered "no server" has still proved a
    /// herdr lives there.
    #[test]
    fn a_server_that_starts_later_is_still_found() {
        let mut reach = Reach::default();
        reach.record_failure(&PollError::Server("server_not_running".into()));
        assert!(!reach.abandoned());
        assert!(!reach.should_retry(Duration::from_secs(5)));
        assert!(reach.should_retry(Duration::from_secs(31)));
    }

    /// A herdr that stops answering in its own voice, then stops answering at
    /// all, is a herdr that went away — the endpoint follows the newer
    /// evidence rather than the first thing it saw.
    #[test]
    fn a_side_that_loses_its_herdr_stops_being_polled() {
        let mut reach = Reach::default();
        reach.record_failure(&PollError::Server("server_not_running".into()));
        reach.record_failure(&PollError::Absent("herdr_unavailable"));
        assert!(reach.abandoned());
    }

    #[test]
    fn an_endpoint_that_answered_once_keeps_retrying() {
        let mut reach = Reach::default();
        reach.record_success();
        reach.record_failure(&PollError::Absent("spawn_failed"));
        assert!(!reach.should_retry(Duration::from_secs(5)));
        assert!(reach.should_retry(Duration::from_secs(31)));
    }

    #[test]
    fn a_recovered_endpoint_polls_at_the_normal_interval_again() {
        let mut reach = Reach::default();
        reach.record_success();
        reach.record_failure(&PollError::Server("server_not_running".into()));
        reach.record_success();
        assert!(reach.should_retry(Duration::from_secs(0)));
    }

    #[test]
    fn an_endpoint_is_abandoned_only_after_a_failure_it_never_answered() {
        let mut reach = Reach::default();
        assert!(!reach.abandoned());
        reach.record_success();
        reach.record_failure(&PollError::Absent("spawn_failed"));
        assert!(!reach.abandoned(), "a server that answered once is retried, not given up on");

        let mut never = Reach::default();
        never.record_failure(&PollError::Absent("spawn_failed"));
        assert!(never.abandoned());
    }

    /// A stopped distro cannot be running a server, and polling one boots its
    /// VM, so the endpoint set follows the running distros rather than the
    /// registered ones.
    #[test]
    fn an_endpoint_follows_its_distro_starting_and_stopping() {
        let mut endpoints = Endpoints::default();
        assert_eq!(endpoints.caches().len(), 1);

        endpoints.adopt_running(&["kali-linux".to_string()]);
        let started = endpoints.generation();
        assert!(endpoints.caches().iter().any(|c| *c.side() == Side::Wsl("kali-linux".into())));

        endpoints.adopt_running(&["kali-linux".to_string()]);
        assert_eq!(endpoints.generation(), started, "an unchanged set is not a change");

        endpoints.adopt_running(&[]);
        assert_eq!(endpoints.caches().len(), 1);
        assert_eq!(*endpoints.caches()[0].side(), Side::Native, "the native side is permanent");
        assert_ne!(endpoints.generation(), started, "the rows the endpoint carried are gone");
    }

    /// The endpoint an adoption removes carries its own generation away with
    /// it, and a set change has to outweigh that however far the endpoint had
    /// counted — `1`, what a cache holds after its first agent lands, most of
    /// all.
    #[test]
    fn removing_an_endpoint_that_landed_a_poll_is_still_observable() {
        let mut endpoints = Endpoints::default();
        endpoints.adopt_running(&["kali-linux".to_string()]);
        endpoints.caches[1].generation = 1;
        let with_agent = endpoints.generation();

        endpoints.adopt_running(&[]);
        assert_ne!(endpoints.generation(), with_agent);
    }

    /// A listing that failed says nothing about what is running, so it must
    /// not read as "nothing is": one `wsl.exe` hiccup would otherwise drop
    /// every WSL endpoint along with its agents and its backoff state.
    #[test]
    fn a_failed_listing_leaves_the_endpoint_set_alone() {
        let mut endpoints = Endpoints::default();
        endpoints.adopt_listing(Some(vec!["kali-linux".to_string()]));
        let listed = endpoints.generation();

        endpoints.adopt_listing(None);
        assert_eq!(endpoints.caches().len(), 2);
        assert_eq!(endpoints.generation(), listed);

        endpoints.adopt_listing(Some(Vec::new()));
        assert_eq!(endpoints.caches().len(), 1, "an answered empty listing does remove it");
    }

    #[test]
    fn a_repeated_error_is_logged_once() {
        let mut reach = Reach::default();
        assert!(reach.record_failure(&PollError::Server("protocol_mismatch".into())));
        assert!(!reach.record_failure(&PollError::Server("protocol_mismatch".into())));
        assert!(reach.record_failure(&PollError::Server("server_not_running".into())));
    }

    fn agent(id: &str, status: Status) -> Agent {
        Agent {
            terminal_id: id.into(),
            pane_id: "w1:p1".into(),
            tab_id: Some("w1:t1".into()),
            kind: Some("claude".into()),
            title: None,
            status: Some(status),
            focused: false,
            cwd: Some("/repo".into()),
            foreground_cwd: None,
        }
    }

    #[test]
    fn an_unchanged_agent_list_is_not_a_change() {
        let was = vec![agent("t1", Status::Idle)];
        assert!(!rendered_differs(&was, &was));
    }

    #[test]
    fn a_status_change_counts() {
        let was = vec![agent("t1", Status::Idle)];
        let now = vec![agent("t1", Status::Working)];
        assert!(rendered_differs(&was, &now));
    }
}
