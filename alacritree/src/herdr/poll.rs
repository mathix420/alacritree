//! What each reachable herdr server has.  An event stream says when a side
//! changed, a listing says what it now holds, and a side whose stream drops
//! is reconnected on a backoff until its herdr comes back.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::{jobs, wsl};

use super::events::{self, Event, Message, Stream};
use super::{Listing, PollError, Settings, running_session_name, settings};
use crate::multiplexer::{Pane, Side};

/// How long a side with no stream waits before each reconnect in a run of
/// failures.  The last step repeats, so a herdr started after alacritree is
/// found within that long.
const RECONNECT_BACKOFF: [Duration; 4] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];

/// How long rows outlive the side that reported them.  Long enough to ride
/// out a herdr restart without the sidebar blanking, short enough that a
/// herdr that stayed down stops showing a status nobody refreshes.
const LISTING_GRACE: Duration = Duration::from_secs(6);

/// How long a listing that failed while the stream stayed up waits before
/// running again.
const LISTING_RETRY: Duration = Duration::from_secs(2);

/// Whether an endpoint is worth talking to.  A side with no herdr on it is
/// abandoned, so a machine with none pays one failed spawn; a side that has a
/// herdr is retried forever, because starting the server is the ordinary
/// thing to do after alacritree is already open.
#[derive(Debug, Default)]
pub(super) struct Reach {
    ever_answered: bool,
    failing: bool,
    /// Whether the last failure was one that waiting cannot fix.
    absent: bool,
    last_error: Option<String>,
}

impl Reach {
    /// Whether this endpoint has been given up on for the process lifetime:
    /// no herdr has ever spoken from it, and the last try found none there.
    pub(super) fn abandoned(&self) -> bool {
        self.failing && self.absent && !self.ever_answered
    }

    pub(super) fn record_success(&mut self) {
        self.ever_answered = true;
        self.failing = false;
        self.absent = false;
        self.last_error = None;
    }

    /// Records a failure, returning whether it is worth logging — a code
    /// repeating every tick is logged once, not once per poll.
    pub(super) fn record_failure(&mut self, error: &PollError) -> bool {
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
    agents: Vec<Pane>,
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
    pub agent: Pane,
    pub current: bool,
}

/// The subscription that says when a side changed.
enum Link {
    /// No stream.  A reconnect is due at `retry_at`, the `failures`th in a
    /// row.
    Down {
        failures: usize,
        retry_at: Instant,
    },
    /// Spawned, and waiting for herdr to accept the subscription.  Carries
    /// the run of failures this attempt would extend.
    Connecting {
        stream: Stream,
        failures: usize,
    },
    Up(Stream),
    /// No herdr has ever answered here and the last try found none.
    Abandoned,
}

/// The per-pane status subscription and the pane ids it names.
struct StatusLink {
    pane_ids: Vec<String>,
    stream: Stream,
}

/// Starts a subscription on a side.  A test build never starts a real bridge.
fn open_stream(side: &Side, request: String) -> Stream {
    if cfg!(test) { Stream::unreachable() } else { Stream::open(side, request) }
}

/// Starts a listing on a side.  In a test build it never lands, and the test
/// replaces it with the reply it wants.
fn start_listing(
    side: &Side,
    listing: Listing,
    attached: bool,
) -> jobs::Job<Result<ListingReply, PollError>> {
    if cfg!(test) {
        return jobs::Job::never();
    }
    let side = side.clone();
    jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
        super::cli::list_panes(&side, listing, attached, blocking)
    })
}

/// One herdr server's agents, refreshed off the UI thread.
pub struct EndpointCache {
    side: Side,
    agents: Vec<Pane>,
    attachment_panes: Vec<PaneMetadata>,
    generation: u64,
    reach: Reach,
    link: Link,
    status: Option<StatusLink>,
    /// The pane ids herdr last refused a status subscription for, so the same
    /// refusal is not asked for again until a listing names other panes.
    status_refused: Option<Vec<String>>,
    /// When the next listing has to start.  `None` while nothing has changed
    /// since the last one.
    listing_due: Option<Instant>,
    /// What the last listing was asked for, since a change in what the sidebar
    /// wants is itself a reason to list again.
    listed_for: Option<(Listing, bool)>,
    /// Patches that landed while a listing was in flight.  They are newer than
    /// that listing, so they go on top of it once it lands.
    held: Vec<Event>,
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
            link: Link::Down { failures: 0, retry_at: Instant::now() },
            status: None,
            status_refused: None,
            listing_due: None,
            listed_for: None,
            held: Vec::new(),
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

    pub fn agents(&self) -> &[Pane] {
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
    pub fn set_agents_for_test(&mut self, agents: Vec<Pane>) {
        self.agents = agents;
    }

    #[cfg(test)]
    pub fn set_settings_for_test(&mut self, settings: Settings) {
        self.settings = Read::Done(settings);
    }

    /// A cache holding one listing at a chosen sample time, for tests that
    /// drive `HerdrViewSync` without a poll behind them.
    #[doc(hidden)]
    pub fn for_test(side: Side, agents: Vec<Pane>, sampled_at: Instant) -> Self {
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
        self.pending = Some(jobs::Job::ready(
            result.map(|stdout| ListingReply::parse(stdout, listing, sampled_at, true)),
        ));
        self.listed_for = Some((display, true));
        self.poll(display, true);
    }

    /// One listing that did not answer, with the grace period holding what
    /// herdr last reported still running.
    #[cfg(test)]
    pub fn fail_listing_for_test(&mut self, error: PollError) {
        self.settings = Read::Done(Settings::default());
        self.session_name = Read::Done("fixture".into());
        self.pending = Some(jobs::Job::ready(Err(error)));
        self.poll(Listing::Agents, true);
    }

    /// A side whose stream herdr has accepted, fed by the returned sender.
    #[cfg(test)]
    pub(super) fn connect_for_test(&mut self) -> std::sync::mpsc::Sender<Message> {
        let (tx, stream) = Stream::fake();
        self.link = Link::Connecting { stream, failures: 0 };
        tx
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
                Err(error) => self.note_failure(&error),
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

    /// Reads what the streams said, adopts a landed listing, and starts
    /// whatever is due.  Never blocks.
    pub fn poll(&mut self, listing: Listing, attached: bool) {
        let now = Instant::now();
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
        self.advance_link(now);
        self.advance_status(now);
        if let Some(job) = &self.pending {
            match job.poll() {
                Some(Ok(reply)) => {
                    self.adopt_reply(reply, listing, attached);
                    for event in std::mem::take(&mut self.held) {
                        self.apply(event, now);
                    }
                    self.start_settings_read();
                    self.start_session_name_read();
                    self.pending = None;
                },
                Some(Err(error)) => {
                    self.listing_failed(&error, now);
                    // herdr restarting may name its session differently, and
                    // attaching to the old name reaches nothing.
                    self.session_name = Read::Unread;
                    self.pending = None;
                },
                // A worker panic supplies no membership evidence.
                None if job.failed() => {
                    self.listing_failed(&PollError::Absent("poll_panicked"), now);
                    self.pending = None;
                },
                None => {},
            }
        }

        if !attached {
            self.inventory = None;
            self.attachment_panes.clear();
        }
        if !matches!(self.link, Link::Up(_)) {
            self.expire_grace(now);
            return;
        }
        if self.pending.is_some() {
            return;
        }
        if self.listed_for != Some((listing, attached)) {
            self.listing_due = Some(now);
        }
        if self.listing_due.is_some_and(|due| due <= now) {
            self.listing_due = None;
            self.listed_for = Some((listing, attached));
            let listing = if attached { Listing::Panes } else { listing };
            self.pending = Some(start_listing(&self.side, listing, attached));
            return;
        }
        self.sync_status();
    }

    /// Reconnects a side that has no stream, now rather than on the backoff.
    /// For a user action aimed at the side, which is the moment herdr being
    /// up matters most.
    pub fn reconnect_now(&mut self) {
        if let Link::Down { retry_at, .. } = &mut self.link {
            *retry_at = Instant::now();
        }
    }

    /// Drops this side's streams and reconnects at once.  For a herdr that
    /// stopped answering commands without closing its streams, which is the
    /// one way a hung server can be noticed.
    pub fn restart(&mut self) {
        if matches!(self.link, Link::Abandoned) {
            return;
        }
        self.status = None;
        self.link = Link::Down { failures: 0, retry_at: Instant::now() };
    }

    /// Reads the lifecycle stream, and starts one when a reconnect is due.
    fn advance_link(&mut self, now: Instant) {
        let messages = match &mut self.link {
            Link::Down { failures, retry_at } => {
                if *retry_at <= now {
                    let failures = *failures;
                    let stream = open_stream(&self.side, events::lifecycle_request());
                    self.link = Link::Connecting { stream, failures };
                }
                return;
            },
            Link::Abandoned => return,
            Link::Connecting { stream, .. } | Link::Up(stream) => stream.messages(),
        };
        for message in messages {
            match message {
                Message::Started => {
                    let link = std::mem::replace(&mut self.link, Link::Abandoned);
                    self.link = match link {
                        Link::Connecting { stream, .. } => Link::Up(stream),
                        other => other,
                    };
                    self.note_success();
                    self.listing_due = Some(now);
                },
                Message::Event(event) => self.receive(event, now),
                Message::Ended(reason) => {
                    self.link_ended(reason, now);
                    return;
                },
            }
        }
    }

    /// A lifecycle stream ending.  A stream herdr had accepted ended because
    /// herdr went away, so the reconnect starts at once; one that never
    /// started backs off, or gives the side up when nothing herdr-shaped
    /// answered there.
    fn link_ended(&mut self, reason: Option<PollError>, now: Instant) {
        let failures = match &self.link {
            Link::Connecting { failures, .. } => failures + 1,
            Link::Down { .. } | Link::Up(_) | Link::Abandoned => 0,
        };
        // Only a stream that was up has a listing behind it to retire; a
        // reconnect that failed found everything already retired.
        if matches!(self.link, Link::Up(_)) {
            self.status = None;
            self.inventory = None;
            self.session_name = Read::Unread;
            self.blank_at.get_or_insert(now + LISTING_GRACE);
            events::wake_after(LISTING_GRACE);
        }
        if let Some(error) = &reason {
            self.log_failure(error);
            if self.reach.abandoned() {
                self.link = Link::Abandoned;
                return;
            }
        }
        let delay = match failures {
            0 => Duration::ZERO,
            n => RECONNECT_BACKOFF[(n - 1).min(RECONNECT_BACKOFF.len() - 1)],
        };
        self.link = Link::Down { failures, retry_at: now + delay };
        events::wake_after(delay);
    }

    /// Reads the status stream.  A refusal is remembered against the panes it
    /// named, and a stream that ends is dropped; the next listing decides
    /// whether another is worth opening.
    fn advance_status(&mut self, now: Instant) {
        let Some(link) = &mut self.status else { return };
        for message in link.stream.messages() {
            match message {
                // herdr streams no status a pane already had, so a change
                // between the listing and this ack is only learned by listing
                // again.
                Message::Started => self.listing_due = Some(now),
                Message::Event(event) => self.receive(event, now),
                Message::Ended(reason) => {
                    if let Some(link) = self.status.take()
                        && let Some(error) = reason
                    {
                        log::debug!(
                            "herdr ({:?}): status stream refused: {}",
                            self.side,
                            error.code()
                        );
                        self.status_refused = Some(link.pane_ids);
                    }
                    return;
                },
            }
        }
    }

    /// Keeps the status subscription naming exactly the agents last listed.
    /// Each entry costs herdr a pane lookup every delivery tick, so a pane
    /// with no agent in it is left out; one that gains an agent is announced
    /// by `pane.agent_detected`, which relists.
    fn sync_status(&mut self) {
        let mut pane_ids: Vec<String> = self
            .agents
            .iter()
            .chain(self.attachment_panes.iter().filter(|pane| pane.current).map(|pane| &pane.agent))
            .filter(|pane| pane.status.is_some())
            .map(|pane| pane.pane_id.clone())
            .collect();
        pane_ids.sort_unstable();
        pane_ids.dedup();
        if self.status.as_ref().is_some_and(|link| link.pane_ids == pane_ids)
            || self.status_refused.as_ref() == Some(&pane_ids)
        {
            return;
        }
        self.status = (!pane_ids.is_empty()).then(|| StatusLink {
            stream: open_stream(&self.side, events::status_request(&pane_ids)),
            pane_ids,
        });
    }

    /// An event from either stream.  A patch lands at once, and is held as
    /// well when a listing is in flight, since that listing may have sampled
    /// herdr before the change and would otherwise undo it.
    fn receive(&mut self, event: Event, now: Instant) {
        if self.pending.is_some() && !matches!(event, Event::Changed) {
            self.held.push(event.clone());
        }
        self.apply(event, now);
    }

    fn apply(&mut self, event: Event, now: Instant) {
        let changed = match event {
            Event::Changed => {
                self.listing_due = Some(now);
                false
            },
            Event::Status { pane_id, status } => {
                let mut changed = false;
                for pane in self.panes_mut().filter(|pane| pane.pane_id == pane_id) {
                    if pane.status.is_some() && pane.status != Some(status) {
                        pane.status = Some(status);
                        changed = true;
                    }
                }
                changed
            },
            Event::Focused { pane_id } => {
                let mut changed = false;
                for pane in self.panes_mut() {
                    let focused = pane.pane_id == pane_id;
                    changed |= pane.focused != focused;
                    pane.focused = focused;
                }
                changed
            },
            Event::Updated(updated) => self.apply_update(updated, now),
        };
        if changed {
            self.generation = self.generation.wrapping_add(1);
            // A patch is herdr's state as of now, and `HerdrViewSync` follows
            // only a sample newer than its own last focus move.
            if self.sampled_at.is_some() {
                self.sampled_at = Some(now);
            }
        }
    }

    /// A pane herdr re-described.  A pane this side does not show is left
    /// alone, since a shell's title churns and a new row arrives on its own
    /// event; an agent that left its pane changes which rows exist, which
    /// only a listing can say.
    fn apply_update(&mut self, updated: Pane, now: Instant) -> bool {
        let hides_shells = matches!(self.listed_for, Some((Listing::Agents, false)));
        let mut changed = false;
        if let Some(pane) = self.agents.iter_mut().find(|pane| pane.pane_id == updated.pane_id) {
            if hides_shells && updated.status.is_none() {
                self.listing_due = Some(now);
            } else {
                changed =
                    rendered_differs(std::slice::from_ref(pane), std::slice::from_ref(&updated));
                *pane = updated.clone();
            }
        }
        if let Some(pane) = self
            .attachment_panes
            .iter_mut()
            .find(|pane| pane.current && pane.agent.pane_id == updated.pane_id)
        {
            pane.agent = updated;
        }
        changed
    }

    /// Every pane this side holds a live description of.
    fn panes_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.agents.iter_mut().chain(
            self.attachment_panes
                .iter_mut()
                .filter(|pane| pane.current)
                .map(|pane| &mut pane.agent),
        )
    }

    /// A listing that failed while the stream is up is run again shortly;
    /// the stream says nothing about what the failed one would have shown.
    fn listing_failed(&mut self, error: &PollError, now: Instant) {
        // Held patches belong on top of the listing that failed; the next one
        // samples herdr after them.
        self.held.clear();
        self.note_missing_listing(error, now);
        if matches!(self.link, Link::Up(_)) {
            self.listing_due = Some(now + LISTING_RETRY);
            events::wake_after(LISTING_RETRY);
        }
    }

    /// Records a reply that landed but could not be read.  Something did
    /// answer here, so it is evidence about this side, and what it displaces
    /// goes at once.
    fn note_failure(&mut self, error: &PollError) {
        self.inventory = None;
        self.forget_listing();
        self.log_failure(error);
    }

    /// Records a listing that never answered.  A listing that could not run is
    /// no evidence about the agents, since herdr's own state is untouched by a
    /// process that failed to spawn, so what it last said stands until the
    /// failures outlast [`LISTING_GRACE`].  Giving the rows up on the first
    /// trades a rare stale status for a certain blank whenever a spawn
    /// hiccups, which on a loaded machine is the common case.
    fn note_missing_listing(&mut self, error: &PollError, now: Instant) {
        // Membership is the exception: a pane is removed on a listing that
        // carries every pane but that one, which a failure is not.
        self.inventory = None;
        self.blank_at.get_or_insert(now + LISTING_GRACE);
        self.expire_grace(now);
        self.log_failure(error);
    }

    /// Gives up the rows once a run of failures has outlasted its grace.
    fn expire_grace(&mut self, now: Instant) {
        if self.blank_at.is_none_or(|blank_at| now < blank_at) {
            return;
        }
        self.forget_listing();
        if !self.agents.is_empty() {
            self.agents.clear();
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Drops the live half of what herdr last said about this side's panes.
    /// A status nothing is refreshing still reads as current, which is worse
    /// than showing none at all.  The sample time goes with it, since a
    /// reader takes one as proof the side is answering.
    fn forget_listing(&mut self) {
        self.sampled_at = None;
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
                "herdr ({:?}): {code}; no herdr here, so this endpoint is not retried",
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
    pub fn poll(&mut self, listing: Listing, attached: impl Fn(&Side) -> bool) {
        self.refresh_running();
        for cache in &mut self.caches {
            let has_attachments = attached(cache.side());
            cache.poll(listing, has_attachments);
        }
    }

    pub fn cache_mut(&mut self, side: &Side) -> Option<&mut EndpointCache> {
        self.caches.iter_mut().find(|cache| cache.side() == side)
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
fn rendered_differs(was: &[Pane], now: &[Pane]) -> bool {
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
    use std::sync::mpsc;

    use super::*;
    use crate::multiplexer::PaneStatus;

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
        cache.pending = Some(jobs::Job::ready(Ok(ListingReply::parse(
            r#"{"result":{"agents":[{"terminal_id":"agent","pane_id":"w1:p1","agent":"claude","agent_status":"working","terminal_title_stripped":"review work"}]}}"#,
            Listing::Agents,
            Instant::now(),
            false,
        ))));
        cache.poll(Listing::Agents, false);
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

        cache.fail_listing_for_test(PollError::Absent("spawn_failed"));

        assert_eq!(cache.agents().len(), 1);
        assert_eq!(cache.agents()[0].status, Some(PaneStatus::Working));
        let pane = cache.attachment_pane("agent").expect("the pane keeps its row");
        assert_eq!(pane.agent.status, Some(PaneStatus::Working));
        assert!(pane.current);
        // The listing and when it was taken are one fact, so a reader asking
        // whether the side is answering agrees with the rows still drawn.
        assert!(cache.sampled_at().is_some());
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
        cache.fail_listing_for_test(PollError::Absent("spawn_failed"));
        cache.expire_grace_for_test();

        cache.fail_listing_for_test(PollError::Absent("spawn_failed"));

        assert!(cache.agents().is_empty());
        assert!(cache.sampled_at().is_none());
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
        cache.fail_listing_for_test(PollError::Absent("spawn_failed"));
        cache.expire_grace_for_test();

        cache.complete_listing_for_test(
            Ok(LISTING),
            Listing::Panes,
            Listing::Panes,
            Instant::now(),
        );
        cache.fail_listing_for_test(PollError::Absent("spawn_failed"));

        assert_eq!(cache.agents()[0].status, Some(PaneStatus::Working));
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
        cache.poll(Listing::Agents, false);
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

    /// A side whose stream herdr accepted, holding the listing `json` taken
    /// with an attached session, which is the listing that carries every pane.
    fn live(json: &str) -> (mpsc::Sender<Message>, EndpointCache) {
        let mut cache = EndpointCache::new(Side::Native);
        let tx = cache.connect_for_test();
        tx.send(Message::Started).unwrap();
        cache.poll(Listing::Panes, true);
        assert!(cache.pending.is_some(), "an accepted stream lists the side");
        land(&mut cache, json);
        (tx, cache)
    }

    /// Lands `json` as the reply to the listing in flight.
    fn land(cache: &mut EndpointCache, json: &str) {
        cache.settings = Read::Done(Settings::default());
        cache.session_name = Read::Done("fixture".into());
        cache.pending = Some(jobs::Job::ready(Ok(ListingReply::parse(
            json,
            Listing::Panes,
            Instant::now(),
            true,
        ))));
        cache.poll(Listing::Panes, true);
    }

    fn send(tx: &mpsc::Sender<Message>, cache: &mut EndpointCache, event: Event) {
        tx.send(Message::Event(event)).unwrap();
        cache.poll(Listing::Panes, true);
    }

    const TWO_AGENTS: &str = r#"{"result":{"panes":[
        {"terminal_id":"a","pane_id":"w1:p1","agent":"claude","agent_status":"idle","focused":true},
        {"terminal_id":"b","pane_id":"w1:p2","agent":"codex","agent_status":"idle"}
    ]}}"#;

    #[test]
    fn a_landed_listing_is_not_followed_by_another() {
        let (_tx, mut cache) = live(TWO_AGENTS);
        assert_eq!(cache.agents().len(), 2);
        cache.poll(Listing::Panes, true);
        assert!(cache.pending.is_none(), "nothing changed, so nothing is listed");
    }

    /// The point of the stream: a status change reaches the row without
    /// waiting on a listing.
    #[test]
    fn a_status_event_patches_the_row_without_a_listing() {
        let (tx, mut cache) = live(TWO_AGENTS);
        let before = cache.generation();

        send(&tx, &mut cache, Event::Status {
            pane_id: "w1:p2".into(),
            status: PaneStatus::Working,
        });

        assert_eq!(cache.agents()[1].status, Some(PaneStatus::Working));
        assert_ne!(cache.generation(), before);
        assert!(cache.pending.is_none());
    }

    #[test]
    fn a_focus_event_moves_focus_within_the_side() {
        let (tx, mut cache) = live(TWO_AGENTS);

        send(&tx, &mut cache, Event::Focused { pane_id: "w1:p2".into() });

        assert!(!cache.agents()[0].focused);
        assert!(cache.agents()[1].focused);
        assert!(!cache.attachment_pane("a").unwrap().agent.focused);
        assert!(cache.attachment_pane("b").unwrap().agent.focused);
    }

    /// Following herdr's focus only credits a sample newer than alacritree's
    /// own last focus move, so a focus patch has to count as one or the
    /// sidebar would wait for an unrelated listing to follow.
    #[test]
    fn a_focus_event_is_a_fresh_sample() {
        let (tx, mut cache) = live(TWO_AGENTS);
        let listed = cache.sampled_at().unwrap();
        std::thread::sleep(Duration::from_millis(2));

        send(&tx, &mut cache, Event::Focused { pane_id: "w1:p2".into() });

        assert!(cache.sampled_at().unwrap() > listed);
    }

    /// herdr streams no status a pane already had, so the gap between a
    /// listing and a new status subscription's ack is closed by listing again.
    #[test]
    fn a_status_subscription_starting_lists_the_side_again() {
        let (_tx, mut cache) = live(TWO_AGENTS);
        let (status_tx, stream) = Stream::fake();
        cache.status.as_mut().unwrap().stream = stream;

        status_tx.send(Message::Started).unwrap();
        cache.poll(Listing::Panes, true);

        assert!(cache.pending.is_some());
    }

    #[test]
    fn a_pane_appearing_lists_the_side_again() {
        let (tx, mut cache) = live(TWO_AGENTS);
        send(&tx, &mut cache, Event::Changed);
        assert!(cache.pending.is_some());
    }

    /// A listing in flight may have sampled herdr before the change it is
    /// racing, so landing it must not undo a patch that arrived meanwhile.
    #[test]
    fn a_patch_during_a_listing_survives_it() {
        let (tx, mut cache) = live(TWO_AGENTS);
        send(&tx, &mut cache, Event::Changed);
        send(&tx, &mut cache, Event::Status {
            pane_id: "w1:p1".into(),
            status: PaneStatus::Blocked,
        });

        land(&mut cache, TWO_AGENTS);

        assert_eq!(cache.agents()[0].status, Some(PaneStatus::Blocked));
    }

    /// A shell's title churns with every prompt, so an update for a pane the
    /// side does not show must not cost a listing.
    #[test]
    fn an_update_for_a_hidden_pane_is_ignored() {
        let (tx, mut cache) = live(TWO_AGENTS);
        let shell = Listing::Panes
            .parse(r#"{"result":{"panes":[{"terminal_id":"s","pane_id":"w1:p9","title":"~"}]}}"#);
        send(&tx, &mut cache, Event::Updated(shell[0].clone()));
        assert!(cache.pending.is_none());
        assert_eq!(cache.agents().len(), 2);
    }

    #[test]
    fn the_status_subscription_names_the_listed_agents() {
        let (_tx, cache) = live(TWO_AGENTS);
        let pane_ids = &cache.status.as_ref().expect("agents are listed").pane_ids;
        assert_eq!(pane_ids, &["w1:p1".to_string(), "w1:p2".to_string()]);
    }

    /// herdr refusing a status subscription is remembered against the panes
    /// it named, so the same refusal is not asked for every frame.
    #[test]
    fn a_refused_status_subscription_is_not_asked_for_again() {
        let (_tx, mut cache) = live(TWO_AGENTS);
        cache.poll(Listing::Panes, true);
        assert!(cache.status.is_none(), "this test build's bridge always ends");
        assert!(cache.status_refused.is_some());
        cache.poll(Listing::Panes, true);
        assert!(cache.status.is_none());
    }

    /// herdr closing a stream it had accepted means herdr went away, and the
    /// usual reason is a restart, so the side reconnects at once and keeps
    /// its rows through the grace.
    #[test]
    fn a_closed_stream_reconnects_at_once_and_keeps_the_rows() {
        let (tx, mut cache) = live(TWO_AGENTS);

        tx.send(Message::Ended(None)).unwrap();
        cache.poll(Listing::Panes, true);

        assert!(
            matches!(cache.link, Link::Down { failures: 0, retry_at } if retry_at <= Instant::now())
        );
        assert_eq!(cache.agents().len(), 2);
        assert!(cache.inventory().is_none(), "membership is only ever read from a listing");
    }

    /// Starting herdr after alacritree is the ordinary order, so a side whose
    /// herdr is not up yet is retried, on a backoff.
    #[test]
    fn a_server_not_yet_running_is_retried_on_a_backoff() {
        let mut cache = EndpointCache::new(Side::Native);
        let tx = cache.connect_for_test();

        tx.send(Message::Ended(Some(PollError::Server("server_not_running".into())))).unwrap();
        cache.poll(Listing::Panes, true);

        let Link::Down { failures, retry_at } = cache.link else { panic!("not down") };
        assert_eq!(failures, 1);
        assert!(retry_at > Instant::now());
    }

    #[test]
    fn a_side_with_no_herdr_is_given_up_on() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.poll(Listing::Agents, false);
        cache.poll(Listing::Agents, false);
        assert!(matches!(cache.link, Link::Abandoned));
    }

    /// A user reaching for a side that is waiting out its backoff is told
    /// the truth now rather than five seconds from now.
    #[test]
    fn reaching_for_a_side_reconnects_it_now() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.link = Link::Down { failures: 3, retry_at: Instant::now() + Duration::from_secs(5) };
        cache.reconnect_now();
        assert!(matches!(cache.link, Link::Down { retry_at, .. } if retry_at <= Instant::now()));
    }

    #[test]
    fn a_failed_listing_on_a_live_side_is_retried() {
        let (tx, mut cache) = live(TWO_AGENTS);
        send(&tx, &mut cache, Event::Changed);
        cache.pending = Some(jobs::Job::ready(Err(PollError::Absent("spawn_failed"))));

        cache.poll(Listing::Panes, true);

        assert!(cache.pending.is_none());
        assert!(cache.listing_due.is_some_and(|due| due > Instant::now()));
        assert_eq!(cache.agents().len(), 2, "one failure sits inside the grace");
    }

    #[test]
    fn failed_inventory_jobs_invalidate_success() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.complete_listing_for_test(
            Ok(r#"{"result":{"panes":[]}}"#),
            Listing::Panes,
            Listing::Panes,
            Instant::now(),
        );
        assert!(cache.inventory().is_some());
        cache.pending = Some(jobs::Job::panicked());

        cache.poll(Listing::Panes, true);

        assert!(cache.inventory().is_none());
        // One panicked listing sits inside the grace, so the listing it did
        // not replace still stands.
        assert!(cache.sampled_at().is_some());
        assert!(cache.pending.is_none());
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
            cache.poll(Listing::Panes, true);
            assert!(cache.pending.is_none());
            assert_eq!(cache.inventory().unwrap().sampled_at, started);
        }
        cache.poll(Listing::Panes, false);
        assert!(cache.inventory().is_none());
        assert!(cache.pending.is_none());
    }

    #[test]
    fn pane_display_without_attachments_does_not_build_an_inventory() {
        let mut cache = EndpointCache::new(Side::Native);
        cache.settings = Read::Done(Settings::default());
        cache.session_name = Read::Done("fixture".into());
        cache.pending = Some(jobs::Job::ready(Ok(ListingReply::parse(
            r#"{"result":{"panes":[{"terminal_id":"shell","pane_id":"w1:p1"}]}}"#,
            Listing::Panes,
            Instant::now(),
            false,
        ))));

        cache.poll(Listing::Panes, false);

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
                cache.poll(Listing::Panes, true);
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

    #[test]
    fn an_endpoint_with_no_herdr_is_given_up_on() {
        let mut reach = Reach::default();
        reach.record_failure(&PollError::Absent("spawn_failed"));
        assert!(reach.abandoned());
    }

    /// The common way to meet herdr is to start it after alacritree, and an
    /// endpoint that has only ever answered "no server" has still proved a
    /// herdr lives there.
    #[test]
    fn a_server_that_starts_later_is_still_found() {
        let mut reach = Reach::default();
        reach.record_failure(&PollError::Server("server_not_running".into()));
        assert!(!reach.abandoned());
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

    fn agent(id: &str, status: PaneStatus) -> Pane {
        Pane {
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
        let was = vec![agent("t1", PaneStatus::Idle)];
        assert!(!rendered_differs(&was, &was));
    }

    #[test]
    fn a_status_change_counts() {
        let was = vec![agent("t1", PaneStatus::Idle)];
        let now = vec![agent("t1", PaneStatus::Working)];
        assert!(rendered_differs(&was, &now));
    }
}
