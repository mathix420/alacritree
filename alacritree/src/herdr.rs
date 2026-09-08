//! Surface agents running under a herdr server in the sidebar.
//!
//! herdr owns its own PTYs and detects the agent in each pane; alacritree
//! only asks what it has and can hand one to a shell.  Everything here goes
//! through the `herdr` CLI rather than its socket, so a missing binary or an
//! absent server is a silent no-op and no wire protocol is pinned.  herdr
//! prints success on stdout and errors on stderr, which is why callers
//! capture both.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::config::AttachMode;
use crate::{command_ext, jobs, wsl};
use serde::Deserialize;

/// Which herdr server an agent belongs to.  Two servers on one machine
/// cannot see each other, so this is part of an agent's identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Side {
    Native,
    /// Named distro, as `wsl.exe -d` spells it.
    Wsl(String),
}

/// Which of herdr's two indicator sets its config selects.  Rows follow the
/// user's own choice, so a pane's mark in the sidebar is the mark it carries
/// in herdr itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Indicators {
    #[default]
    Dots,
    Symbols,
}

/// What alacritree reads out of herdr's config: how to leave a pane, and how
/// herdr draws the state it reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub detach: Option<String>,
    pub indicators: Indicators,
}

/// herdr's agent state.  An unrecognised string maps to `Unknown` so a value
/// herdr adds later renders as a plain row instead of dropping the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
}

impl Status {
    fn parse(raw: &str) -> Self {
        match raw {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }

    /// Word the sidebar paints for this status.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
}

/// One pane as herdr reports it.  `terminal_id` is the identity because
/// `pane_id` is positional: a pane moved between workspaces gets a new one,
/// and ids restart at `w1` after `session delete`.
#[derive(Debug, Clone)]
pub struct Agent {
    pub terminal_id: String,
    pub pane_id: String,
    /// The tab holding this pane.  `herdr agent focus` resolves its target
    /// through the agent registry, so a pane with no agent in it is reached
    /// through its tab instead.
    pub tab_id: Option<String>,
    pub kind: Option<String>,
    /// The pane's title, with the decorative agent prefix already removed by
    /// herdr.  Two agents of one kind in one checkout are told apart by this
    /// and nothing else.
    pub title: Option<String>,
    /// herdr's word on the agent in this pane, and `None` when herdr found no
    /// agent in it at all.  `Some(Unknown)` is the other half of that
    /// distinction: an agent is there and herdr cannot classify it.
    pub status: Option<Status>,
    /// The pane herdr's own window is showing.  A shared-view attach borrows
    /// that window rather than one pane, so this is what such a session has
    /// on screen.
    pub focused: bool,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
}

/// Which herdr listing a poll asks for.  `agent list` answers with the panes
/// herdr detected an agent in; `pane list` answers with every pane it owns,
/// a superset carrying the same fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    Agents,
    Panes,
}

impl Listing {
    /// The listing `[integrations.herdr] show_panes` asks for.
    pub fn wanted(show_panes: bool) -> Self {
        if show_panes { Self::Panes } else { Self::Agents }
    }

    /// The `herdr` subcommand that produces this listing.  A herdr too old
    /// for `pane list` answers with a usage error rather than an envelope,
    /// which `list_panes` cannot tell from no herdr on that side at all.
    pub fn args(self) -> [&'static str; 2] {
        match self {
            Self::Agents => ["agent", "list"],
            Self::Panes => ["pane", "list"],
        }
    }

    /// Panes from one reply.  An entry missing an identity — or, where the
    /// entry is an agent, a status — is dropped on its own; its siblings
    /// still parse.
    pub fn parse(self, stdout: &str) -> Vec<Agent> {
        let Ok(envelope) = serde_json::from_str::<Envelope>(stdout) else {
            return Vec::new();
        };
        let Some(listed) = envelope.result else {
            return Vec::new();
        };
        let raw = match self {
            Self::Agents => listed.agents,
            Self::Panes => listed.panes,
        };
        raw.into_iter().filter_map(|raw| raw.into_agent(self)).collect()
    }
}

#[derive(Deserialize)]
struct Envelope {
    result: Option<Listed>,
}

/// The one key the two listings differ in.  Both are absent-tolerant, so a
/// reply is read under the listing that was asked for rather than under
/// whichever key happens to be present.
#[derive(Deserialize)]
struct Listed {
    #[serde(default)]
    agents: Vec<RawPane>,
    #[serde(default)]
    panes: Vec<RawPane>,
}

/// Only the fields the sidebar renders.  Everything else herdr sends is
/// ignored, so an additive protocol change costs nothing.
#[derive(Deserialize)]
struct RawPane {
    terminal_id: Option<String>,
    pane_id: Option<String>,
    tab_id: Option<String>,
    agent_status: Option<String>,
    agent: Option<String>,
    display_agent: Option<String>,
    terminal_title_stripped: Option<String>,
    focused: Option<bool>,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
}

impl RawPane {
    fn into_agent(self, listing: Listing) -> Option<Agent> {
        // Everything `agent list` returns is an agent, whatever it says about
        // the agent's kind; in a pane listing the `agent` key is what says so.
        let has_agent = listing == Listing::Agents || self.agent.is_some();
        let status = if has_agent { Some(Status::parse(&self.agent_status?)) } else { None };
        Some(Agent {
            terminal_id: self.terminal_id?,
            pane_id: self.pane_id?,
            tab_id: self.tab_id,
            status,
            kind: self.display_agent.or(self.agent),
            title: self
                .terminal_title_stripped
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            focused: self.focused.unwrap_or(false),
            cwd: self.cwd,
            foreground_cwd: self.foreground_cwd,
        })
    }
}

/// Single-quote a POSIX argument, since WSL invocations are one `sh -lc`
/// string rather than an argv.
fn sh_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=".contains(c)) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

impl Side {
    /// Program and argv that run `herdr <args>` on this side.  WSL goes
    /// through a login shell because herdr lives in `~/.local/bin`, which is
    /// not on the PATH `wsl.exe -e` inherits.
    pub fn command(&self, args: &[&str]) -> (String, Vec<String>) {
        match self {
            Self::Native => ("herdr".to_string(), args.iter().map(|a| (*a).to_string()).collect()),
            Self::Wsl(distro) => {
                let script = std::iter::once("herdr".to_string())
                    .chain(args.iter().map(|a| sh_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                // `--exec` hands wsl.exe a bare program lookup, and herdr
                // installs to ~/.local/bin, which is off that PATH; routing
                // through `sh -lc` sources the login shell that puts it back.
                wsl::exec_invocation(distro, &["sh", "-lc", &script])
            },
        }
    }

    /// How a row names this side.  `None` on the native one, whose name would
    /// be the same word on every row of a machine that has only it.
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Native => None,
            Self::Wsl(distro) => Some(format!("wsl:{distro}")),
        }
    }
}

/// Direct attach to one agent.  Unsupported on native Windows, where
/// `run_terminal_attach` is a `#[cfg(windows)]` refusal.
pub fn attach_args(pane_id: &str) -> Vec<String> {
    vec!["agent".into(), "attach".into(), pane_id.into()]
}

/// Whether direct per-agent attach works on this side.  herdr's
/// `run_terminal_attach` is a `#[cfg(windows)]` refusal, so a native Windows
/// server falls back to focusing the pane and attaching the whole session.
pub fn can_attach(side: &Side) -> bool {
    match side {
        Side::Native => !cfg!(windows),
        Side::Wsl(_) => true,
    }
}

/// Whether a row on `side` opens the agent's own pane.  Capability and
/// preference are separate questions and only disagree in one direction: a
/// user who asks for a direct attach on a side that has none gets the
/// session, and no user can be given a direct attach they did not ask for.
///
/// `has_agent` is false for a pane herdr found no agent in.  Every `herdr
/// agent` subcommand resolves its target through the agent registry, which
/// holds nothing for such a pane, so it is only ever reachable through the
/// session.
pub fn attaches_directly(side: &Side, mode: AttachMode, has_agent: bool) -> bool {
    has_agent && mode == AttachMode::Agent && can_attach(side)
}

/// How long the attach gesture waits for herdr before calling it a refusal.
/// These two calls run on the UI thread, and herdr answers a socket on the
/// same machine in milliseconds; three seconds covers a cold process start
/// behind an on-access scanner and still keeps a wedged server from taking
/// the window with it.
const GESTURE_TIMEOUT: Duration = Duration::from_secs(3);

/// Runs `f` on a worker thread and gives up on it after [`GESTURE_TIMEOUT`].
/// `Command::output` has no timeout of its own, so the bound comes from this
/// side, as the IPC client's does.  A call that times out leaves its thread
/// parked until the child exits, which is only reachable when herdr is
/// already wedged.
fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("alacritree-herdr-gesture".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(GESTURE_TIMEOUT).ok()
}

/// The `herdr` subcommand that brings `agent`'s pane to the front of the
/// user's own herdr window.  `agent focus` resolves its target through the
/// agent registry and answers `agent_not_found` for a pane with no agent in
/// it, so such a pane is reached by focusing the tab that holds it.
pub fn focus_args(agent: &Agent) -> Vec<String> {
    match (agent.status, &agent.tab_id) {
        (None, Some(tab_id)) => vec!["tab".into(), "focus".into(), tab_id.clone()],
        _ => focus_pane_args(&agent.pane_id),
    }
}

/// The `herdr` subcommand that focuses one pane by id.
pub fn focus_pane_args(pane_id: &str) -> Vec<String> {
    vec!["agent".into(), "focus".into(), pane_id.into()]
}

/// Focuses one pane in the user's own herdr window, the first half of the
/// native-Windows attach fallback.  A non-zero exit carries herdr's stderr
/// verbatim rather than `error_code`'s parsed code, since a user-facing
/// message wants herdr's human-readable text, not its machine code, and a
/// server that does not answer inside [`GESTURE_TIMEOUT`] refuses the same
/// way.
pub fn focus_pane(side: &Side, focus: &[String]) -> Result<(), String> {
    let borrowed: Vec<&str> = focus.iter().map(String::as_str).collect();
    let (program, args) = side.command(&borrowed);
    #[allow(clippy::disallowed_methods)] // Running herdr is this function's job.
    let run = move || {
        command_ext::hidden(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
    };
    let Some(output) = bounded(run) else {
        return Err("herdr did not answer while focusing the pane".to_string());
    };
    let output = output.map_err(|e| format!("failed to focus herdr pane: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("herdr refused to focus the pane: {stderr}"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct SessionList {
    #[serde(default)]
    sessions: Vec<RawSession>,
}

#[derive(Deserialize)]
struct RawSession {
    name: String,
    #[serde(default)]
    running: bool,
}

/// The running session to attach to on this side.  `herdr session list
/// --json` is a flat object rather than the `result`-wrapped envelope
/// `agent list` uses.  An answer that names nothing falls back to `default`,
/// the name herdr gives an unnamed session; a server that does not answer
/// inside [`GESTURE_TIMEOUT`] is an `Err`, because attaching to a guessed
/// name would only park the wedged wait inside the new session.
pub fn running_session_name(side: &Side) -> Result<String, String> {
    let (program, args) = side.command(&["session", "list", "--json"]);
    #[allow(clippy::disallowed_methods)] // Running herdr is this function's job.
    let run = move || {
        command_ext::hidden(program)
            .args(args)
            .env("WSL_UTF8", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
    };
    let fallback = || "default".to_string();
    let Some(output) = bounded(run) else {
        return Err("herdr did not answer while listing its sessions".to_string());
    };
    let Ok(output) = output else {
        return Ok(fallback());
    };
    if !output.status.success() {
        return Ok(fallback());
    }
    Ok(serde_json::from_slice::<SessionList>(&output.stdout)
        .ok()
        .and_then(|list| list.sessions.into_iter().find(|s| s.running).map(|s| s.name))
        .unwrap_or_else(fallback))
}

/// Identifies one herdr agent across polls.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HerdrKey {
    pub side: Side,
    pub terminal_id: String,
}

/// The agents on `side` that no live session is attached to.  These are the
/// ones that get a sidebar row; an attached agent is drawn by its session
/// row instead, so each agent appears exactly once.
pub fn unattached<'a>(agents: &'a [Agent], side: &Side, claimed: &[HerdrKey]) -> Vec<&'a Agent> {
    agents
        .iter()
        .filter(|a| !claimed.iter().any(|k| k.side == *side && k.terminal_id == a.terminal_id))
        .collect()
}

/// The `code` from an error envelope on stderr, for deciding whether a
/// failure is the ordinary "no server" case or worth a log line.
pub fn error_code(stderr: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct ErrEnvelope {
        error: ErrBody,
    }
    #[derive(Deserialize)]
    struct ErrBody {
        code: String,
    }
    serde_json::from_str::<ErrEnvelope>(stderr).ok().map(|e| e.error.code)
}

/// The sidebar workspace an agent is working in, by longest path prefix.
/// `None` means it belongs under Home.
pub fn match_workspace(agent: &Agent, side: &Side, workspaces: &[PathBuf]) -> Option<PathBuf> {
    let reported = agent.foreground_cwd.as_deref().or(agent.cwd.as_deref())?;
    let cwd = match side {
        Side::Native => PathBuf::from(reported),
        Side::Wsl(distro) => wsl::linux_to_windows(reported, distro),
    };
    workspaces
        .iter()
        .filter(|ws| starts_with(&cwd, ws))
        .max_by_key(|ws| ws.components().count())
        .cloned()
}

/// Component-wise prefix test.  Case-insensitive on Windows, where herdr
/// reports the cwd as the shell spelled it and `Path::starts_with` would
/// refuse `c:\users\dev` against `C:\Users\Dev`.
fn starts_with(cwd: &Path, workspace: &Path) -> bool {
    if cfg!(windows) {
        let mut want = workspace.components();
        let mut have = cwd.components();
        loop {
            match (want.next(), have.next()) {
                (None, _) => return true,
                (Some(_), None) => return false,
                (Some(w), Some(h)) => {
                    let (w, h) = (w.as_os_str(), h.as_os_str());
                    if !w.eq_ignore_ascii_case(h) {
                        return false;
                    }
                },
            }
        }
    } else {
        cwd.starts_with(workspace)
    }
}

/// How long an endpoint known to have a herdr waits before being retried.
const RECOVERY_RETRY: Duration = Duration::from_secs(30);

/// Why a poll produced no agents.  What separates the two is whether a herdr
/// ran at all, because that is what says whether waiting can change the
/// answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollError {
    /// herdr answered in its own voice, carrying the `code` from its error
    /// envelope.  A herdr is installed here and something about this moment
    /// stopped it — most often that its server is not up yet.
    Server(String),
    /// Nothing herdr-shaped answered: no binary, no distro, or output that
    /// was not an envelope.  A property of the machine rather than the moment.
    Absent(&'static str),
}

impl PollError {
    pub fn code(&self) -> &str {
        match self {
            Self::Server(code) => code,
            Self::Absent(code) => code,
        }
    }
}

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

struct ListingReply {
    sampled_at: Instant,
    listing: Listing,
    agents: Vec<Agent>,
    inventory: Option<Result<HashSet<String>, PollError>>,
}

impl ListingReply {
    fn parse(stdout: &str, listing: Listing, sampled_at: Instant, attached: bool) -> Self {
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

    #[cfg(test)]
    pub fn complete_listing_for_test(
        &mut self,
        result: Result<&str, PollError>,
        listing: Listing,
        display: Listing,
        sampled_at: Instant,
    ) {
        self.settings = Read::Done(Settings::default());
        self.session_name = Read::Done("fixture".into());
        self.last_attempt = Some(Instant::now());
        self.pending = Some(jobs::Job::ready(
            result.map(|stdout| ListingReply::parse(stdout, listing, sampled_at, true)),
        ));
        self.poll(Duration::from_secs(60), display, true);
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
                    self.reach.record_success();
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
            self.reach.record_success();
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
                    self.note_failure(&error);
                    // herdr restarting may name its session differently, and
                    // attaching to the old name reaches nothing.
                    self.session_name = Read::Unread;
                    if !self.agents.is_empty() {
                        self.agents.clear();
                        self.generation = self.generation.wrapping_add(1);
                    }
                    self.pending = None;
                },
                // A worker panic supplies no membership evidence. Attached
                // sessions still need retries on the configured cadence.
                None if job.failed() => {
                    self.sampled_at = None;
                    self.note_failure(&PollError::Absent("poll_panicked"));
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

    /// Records one poll that produced no agents, and says so once.  A novel
    /// code that is not the ordinary "no server here" is a warning; giving up
    /// on an endpoint is a debug line, so a herdr that is installed but never
    /// answers can still be explained from a log rather than only by an empty
    /// sidebar.  A code that repeats is logged the first time only, so an
    /// endpoint retried for the whole session still costs one line.
    fn note_failure(&mut self, error: &PollError) {
        self.inventory = None;
        for pane in &mut self.attachment_panes {
            pane.current = false;
            pane.agent.status = None;
            pane.agent.focused = false;
        }
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

/// Runs one of herdr's listings on one side.  Success is on stdout, errors
/// are on stderr, so both are captured; the exit status decides which to read.
///
/// wsl.exe's own failure messages (a missing distro, for instance) come back
/// UTF-16LE unless WSL_UTF8 is set, and `from_utf8_lossy` mangles them without
/// it, so they never parse as an envelope and read as no herdr on that side.
/// herdr's own output is a relayed Linux byte stream and is unaffected either
/// way.
#[allow(clippy::disallowed_methods)] // Running herdr is this function's job.
fn list_panes(
    side: &Side,
    listing: Listing,
    attached: bool,
    _blocking: &jobs::Blocking,
) -> Result<ListingReply, PollError> {
    let (program, args) = side.command(&listing.args());
    let sampled_at = Instant::now();
    let output = command_ext::hidden(program)
        .args(args)
        .env("WSL_UTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|_| PollError::Absent("spawn_failed"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(match error_code(&stderr) {
            Some(code) => PollError::Server(code),
            None => PollError::Absent("herdr_unavailable"),
        });
    }
    Ok(ListingReply::parse(&String::from_utf8_lossy(&output.stdout), listing, sampled_at, attached))
}

/// herdr's own defaults.  A config that binds neither still detaches on
/// these, so an untouched installation has a chord to name rather than an
/// unknown one.
const DEFAULT_PREFIX: &str = "ctrl+b";
const DEFAULT_DETACH: &str = "prefix+q";

#[derive(Deserialize, Default)]
struct RawHerdrConfig {
    #[serde(default)]
    keys: RawKeys,
    #[serde(default)]
    ui: RawUi,
}

#[derive(Deserialize, Default)]
struct RawUi {
    status_indicators: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawKeys {
    prefix: Option<String>,
    detach: Option<RawBinding>,
}

/// herdr binds an action to one key or to several.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawBinding {
    One(String),
    Many(Vec<String>),
}

impl RawBinding {
    /// The binding a hint names.  herdr's own help surface leads with the
    /// first, and a row has space for one.  An empty string is herdr's
    /// spelling for unbound.
    fn first(&self) -> Option<&str> {
        match self {
            Self::One(value) => Some(value.as_str()),
            Self::Many(values) => values.first().map(String::as_str),
        }
        .map(str::trim)
        .filter(|value| !value.is_empty())
    }
}

/// Render one `+`-joined combo the way alacritree spells chords elsewhere:
/// `ctrl+b` reads as `Ctrl+B`.
fn render_combo(raw: &str) -> String {
    raw.split('+').map(render_key).collect::<Vec<_>>().join("+")
}

fn render_key(key: &str) -> String {
    let mut chars = key.chars();
    let Some(first) = chars.next() else { return String::new() };
    if key.len() == 1 {
        return first.to_ascii_uppercase().to_string();
    }
    first.to_uppercase().chain(chars).collect()
}

/// The half of a prefix binding that follows the prefix.  A bare key keeps
/// herdr's own lowercase spelling, so the hint matches herdr's documentation
/// (`ctrl+b q`); a modified one is a combo and reads like one.
fn render_prefixed(rest: &str) -> String {
    if rest.contains('+') { render_combo(rest) } else { rest.to_string() }
}

/// The detach chord `config` binds, spelled as herdr documents it.
///
/// A config herdr itself would reject falls back to the defaults herdr would
/// then run with, so a typo anywhere in the file does not silence the hint.
/// `None` means detach is bound to nothing — the one case where there is no
/// chord to advertise.
fn detach_chord_from(config: &str) -> Option<String> {
    let parsed: RawHerdrConfig = toml::from_str(config).unwrap_or_default();
    let prefix = parsed
        .keys
        .prefix
        .as_deref()
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or(DEFAULT_PREFIX);
    let detach = match &parsed.keys.detach {
        Some(binding) => binding.first()?,
        None => DEFAULT_DETACH,
    };
    Some(match detach.strip_prefix("prefix+") {
        Some(rest) => format!("{} {}", render_combo(prefix), render_prefixed(rest)),
        None => render_combo(detach),
    })
}

fn indicators_from(config: &str) -> Indicators {
    let parsed: RawHerdrConfig = toml::from_str(config).unwrap_or_default();
    match parsed.ui.status_indicators.as_deref() {
        Some("symbols") => Indicators::Symbols,
        _ => Indicators::Dots,
    }
}

fn settings_from(config: &str) -> Settings {
    Settings { detach: detach_chord_from(config), indicators: indicators_from(config) }
}

/// Where herdr looks for its config, mirroring its own resolution so both
/// programs read the same file.  `HERDR_CONFIG_PATH` wins everywhere, then
/// `XDG_CONFIG_HOME` — herdr consults it on Windows too, before the platform
/// directory, so a Windows user with it set keeps one config under `~`.
fn native_config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("HERDR_CONFIG_PATH") {
        return Some(PathBuf::from(path));
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("herdr").join("config.toml"));
    }
    #[cfg(windows)]
    if let Ok(dir) = std::env::var("APPDATA") {
        return Some(PathBuf::from(dir).join("herdr").join("config.toml"));
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
    Some(PathBuf::from(home).join(".config").join("herdr").join("config.toml"))
}

/// The shell that prints a distro's herdr config.  Resolution happens inside
/// the distro because that is where the environment it depends on lives; a
/// missing file prints nothing and still exits zero, which reads as herdr's
/// defaults rather than as a distro we could not reach.
const CONFIG_SCRIPT: &str = concat!(
    r#"p=${HERDR_CONFIG_PATH:-${XDG_CONFIG_HOME:-$HOME/.config}/herdr/config.toml}; "#,
    r#"[ -f "$p" ] && cat "$p" || true"#,
);

/// herdr's config text for this side, or `None` when the side could not be
/// read at all.  An absent file is `Some("")`: herdr runs on its defaults
/// there, and so should the hint.
fn read_config(side: &Side, _blocking: &jobs::Blocking) -> Option<String> {
    match side {
        Side::Native => match std::fs::read_to_string(native_config_path()?) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
            Err(_) => None,
        },
        Side::Wsl(distro) => {
            let (program, args) = wsl::exec_invocation(distro, &["sh", "-lc", CONFIG_SCRIPT]);
            #[allow(clippy::disallowed_methods)] // Reading the distro's config is this arm's job.
            let output = command_ext::hidden(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        },
    }
}

/// What herdr's own config on this side says about leaving a pane and drawing
/// its state.  Every part is user-settable, so a row spelling out a chord the
/// user has rebound would be worse than a row that stays quiet.
pub fn settings(side: &Side, blocking: &jobs::Blocking) -> Option<Settings> {
    Some(settings_from(&read_config(side, blocking)?))
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

    #[test]
    fn attachment_metadata_survives_a_first_poll_failure_after_binding() {
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

    /// herdr strips its own decorative title prefix already, and that stripped
    /// form is what distinguishes two agents of the same kind in one checkout.
    #[test]
    fn an_agents_pane_title_is_parsed() {
        let stdout = r#"{"result":{"agents":[
            {"terminal_id":"t1","pane_id":"w5:p1","agent_status":"idle","agent":"claude",
             "terminal_title":"✫ primary","terminal_title_stripped":"primary"}]}}"#;
        let agents = Listing::Agents.parse(stdout);
        assert_eq!(agents[0].title.as_deref(), Some("primary"));
    }

    /// An agent herdr reports no title for is the common case, not an error.
    #[test]
    fn a_titleless_agent_parses_with_no_title() {
        let stdout = r#"{"result":{"agents":[
            {"terminal_id":"t1","pane_id":"w5:p1","agent_status":"idle","agent":"claude"}]}}"#;
        assert_eq!(Listing::Agents.parse(stdout)[0].title, None);
    }

    /// A title of nothing but spaces says as little as an absent one, and must
    /// not claim the row's identifying slot.
    #[test]
    fn a_blank_title_is_no_title() {
        let stdout = r#"{"result":{"agents":[
            {"terminal_id":"t1","pane_id":"w5:p1","agent_status":"idle","agent":"claude",
             "terminal_title_stripped":"   "}]}}"#;
        assert_eq!(Listing::Agents.parse(stdout)[0].title, None);
    }

    /// herdr ships `ctrl+b` / `prefix+q`, so an untouched config is not an
    /// unknown chord — it is the documented one.
    #[test]
    fn an_untouched_config_yields_herdrs_documented_chord() {
        assert_eq!(detach_chord_from(""), Some("Ctrl+B q".to_string()));
        assert_eq!(
            detach_chord_from(
                "onboarding = false
"
            ),
            Some("Ctrl+B q".to_string())
        );
    }

    #[test]
    fn a_rebound_prefix_moves_the_first_half() {
        let cfg = "[keys]
prefix = \"f12\"
";
        assert_eq!(detach_chord_from(cfg), Some("F12 q".to_string()));
    }

    #[test]
    fn a_rebound_detach_moves_the_second_half() {
        let cfg = "[keys]
detach = \"prefix+shift+d\"
";
        assert_eq!(detach_chord_from(cfg), Some("Ctrl+B Shift+D".to_string()));
    }

    /// A detach bound off the prefix is one chord, not two, so it renders
    /// without the leading prefix it never goes through.
    #[test]
    fn a_direct_detach_binding_drops_the_prefix() {
        let cfg = "[keys]
detach = \"ctrl+alt+q\"
";
        assert_eq!(detach_chord_from(cfg), Some("Ctrl+Alt+Q".to_string()));
    }

    /// herdr accepts a list of bindings for one action.  The row has space
    /// for one, and the first is the one its own help surface leads with.
    #[test]
    fn a_list_of_bindings_renders_the_first() {
        let cfg = "[keys]
detach = [\"prefix+q\", \"prefix+d\"]
";
        assert_eq!(detach_chord_from(cfg), Some("Ctrl+B q".to_string()));
    }

    /// An empty binding is herdr's spelling for "unbound", and a chord that
    /// does not exist must not be advertised.
    #[test]
    fn an_unbound_detach_has_no_chord() {
        assert_eq!(
            detach_chord_from(
                "[keys]
detach = \"\"
"
            ),
            None
        );
        assert_eq!(
            detach_chord_from(
                "[keys]
detach = []
"
            ),
            None
        );
    }

    /// A config herdr itself would reject falls back to the defaults it
    /// would then run with, rather than leaving the row silent.
    #[test]
    fn an_unparseable_config_falls_back_to_the_defaults() {
        assert_eq!(detach_chord_from("[keys"), Some("Ctrl+B q".to_string()));
    }

    /// herdr offers two indicator sets and ships the dotted one, so a config
    /// that says nothing has still chosen it.
    #[test]
    fn the_indicator_set_follows_herdrs_own_choice() {
        assert_eq!(indicators_from(""), Indicators::Dots);
        assert_eq!(
            indicators_from(
                "[ui]
status_indicators = \"symbols\"
"
            ),
            Indicators::Symbols
        );
        assert_eq!(
            indicators_from(
                "[ui]
status_indicators = \"dots\"
"
            ),
            Indicators::Dots
        );
    }

    /// A set herdr adds later is not a reason to paint nothing: the rows keep
    /// the shipped vocabulary until alacritree learns the new one.
    #[test]
    fn an_unknown_indicator_set_keeps_the_shipped_one() {
        let cfg = "[ui]
status_indicators = \"runes\"
";
        assert_eq!(indicators_from(cfg), Indicators::Dots);
    }

    #[test]
    fn settings_carry_both_halves_of_the_config() {
        let cfg = "[keys]
prefix = \"f12\"
[ui]
status_indicators = \"symbols\"
";
        assert_eq!(settings_from(cfg), Settings {
            detach: Some("F12 q".into()),
            indicators: Indicators::Symbols
        });
    }

    /// Captured from a native Windows server.  `skip_serializing_if` drops
    /// `foreground_cwd`, `name`, `display_agent` and `agent_session` rather
    /// than emitting them as null.
    const WINDOWS: &str = r#"{"id":"cli:agent:list","result":{"agents":[
        {"agent":"claude","agent_status":"idle","pane_id":"w5:p1",
         "terminal_id":"term_65abfc8e300361","revision":7,"state_change_seq":3,
         "cwd":"C:\\projects\\alacritree","focused":true,
         "tab_id":"w5:t1","workspace_id":"w5"}],"type":"agent_list"}}"#;

    #[test]
    fn parses_a_windows_agent_with_absent_optional_fields() {
        let agents = Listing::Agents.parse(WINDOWS);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].terminal_id, "term_65abfc8e300361");
        assert_eq!(agents[0].pane_id, "w5:p1");
        assert_eq!(agents[0].kind.as_deref(), Some("claude"));
        assert_eq!(agents[0].status, Some(Status::Idle));
        assert_eq!(agents[0].foreground_cwd, None);
    }

    /// Captured from a WSL server, which does populate `foreground_cwd`.
    const WSL: &str = r#"{"id":"cli:agent:list","result":{"agents":[
        {"agent":"codex","agent_status":"idle","pane_id":"w4:p1",
         "terminal_id":"term_65ab9ae95a74d2","revision":9,"state_change_seq":9,
         "cwd":"/home/dev/Git/devkit","foreground_cwd":"/home/dev/Git/devkit",
         "focused":true,"tab_id":"w4:t1","workspace_id":"w4"}],"type":"agent_list"}}"#;

    #[test]
    fn parses_a_wsl_agent_with_foreground_cwd() {
        let agents = Listing::Agents.parse(WSL);
        assert_eq!(agents[0].foreground_cwd.as_deref(), Some("/home/dev/Git/devkit"));
        assert_eq!(agents[0].kind.as_deref(), Some("codex"));
    }

    #[test]
    fn empty_agent_list_is_not_an_error() {
        let reply = r#"{"id":"cli:agent:list","result":{"agents":[],"type":"agent_list"}}"#;
        assert!(Listing::Agents.parse(reply).is_empty());
    }

    #[test]
    fn unknown_fields_and_unknown_status_survive() {
        let reply = r#"{"id":"x","surprise":1,"result":{"agents":[
            {"terminal_id":"t1","pane_id":"w1:p1","agent_status":"meditating",
             "future_field":true}],"type":"agent_list"}}"#;
        let agents = Listing::Agents.parse(reply);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, Some(Status::Unknown));
    }

    #[test]
    fn an_agent_without_an_identity_is_dropped_alone() {
        let reply = r#"{"id":"x","result":{"agents":[
            {"pane_id":"w1:p1","agent_status":"idle"},
            {"terminal_id":"t2","pane_id":"w1:p2","agent_status":"idle"}],"type":"agent_list"}}"#;
        let agents = Listing::Agents.parse(reply);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].terminal_id, "t2");
    }

    #[test]
    fn display_agent_wins_over_agent() {
        let reply = r#"{"id":"x","result":{"agents":[
            {"terminal_id":"t1","pane_id":"w1:p1","agent_status":"idle",
             "agent":"claude","display_agent":"Claude Code"}],"type":"agent_list"}}"#;
        assert_eq!(Listing::Agents.parse(reply)[0].kind.as_deref(), Some("Claude Code"));
    }

    /// Captured from a native Windows server.  The second pane runs a plain
    /// shell: herdr carries no `agent` key for it and calls its status
    /// `unknown`, which is the state word of an agent it cannot classify and
    /// not a claim that one is there.
    const PANES: &str = r#"{"id":"cli:pane:list","result":{"panes":[
        {"agent":"claude","agent_status":"idle","pane_id":"w1:p1","tab_id":"w1:t1",
         "terminal_id":"term_a","cwd":"C:\\projects\\alacritree","focused":true,
         "terminal_title":"✫ Claude Code","terminal_title_stripped":"Claude Code",
         "scroll":{"offset_from_bottom":0},"workspace_id":"w1"},
        {"agent_status":"unknown","pane_id":"w1:p4","tab_id":"w1:t4",
         "terminal_id":"term_b","cwd":"C:\\projects\\alacritree","focused":false,
         "terminal_title":"~/p/alacritree","terminal_title_stripped":"~/p/alacritree",
         "scroll":{"offset_from_bottom":0},"workspace_id":"w1"}],"type":"pane_list"}}"#;

    #[test]
    fn a_pane_listing_keeps_the_shell_beside_the_agent() {
        let panes = Listing::Panes.parse(PANES);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].status, Some(Status::Idle));
        assert_eq!(panes[0].kind.as_deref(), Some("claude"));
        assert_eq!(panes[1].status, None);
        assert_eq!(panes[1].kind, None);
        assert_eq!(panes[1].title.as_deref(), Some("~/p/alacritree"));
        assert_eq!(panes[1].tab_id.as_deref(), Some("w1:t4"));
    }

    /// One call either way: each poll is a process spawn per side, and the
    /// pane listing already carries everything the agent listing does.
    #[test]
    fn showing_panes_swaps_the_listing_rather_than_adding_one() {
        assert_eq!(Listing::wanted(false).args(), ["agent", "list"]);
        assert_eq!(Listing::wanted(true).args(), ["pane", "list"]);
    }

    /// `herdr agent focus` answers `agent_not_found` for a pane with no agent
    /// in it, so the tab is the only handle such a pane has.
    #[test]
    fn a_pane_with_no_agent_is_focused_through_its_tab() {
        let panes = Listing::Panes.parse(PANES);
        assert_eq!(focus_args(&panes[0]), vec!["agent", "focus", "w1:p1"]);
        assert_eq!(focus_args(&panes[1]), vec!["tab", "focus", "w1:t4"]);
    }

    /// A pane herdr detected no agent in has nothing `herdr agent attach`
    /// could resolve, whatever the side and the configured mode allow.
    #[test]
    fn a_pane_with_no_agent_never_attaches_directly() {
        assert!(!attaches_directly(&Side::Wsl("d".into()), AttachMode::Agent, false));
    }

    /// The reply that arrives on stderr with stdout empty when no server is
    /// listening.  A parser reading only stdout never sees this.
    #[test]
    fn reads_the_error_code_off_stderr() {
        let stderr = r#"{"error":{"code":"server_not_running","message":"no herdr server"},"id":"cli:agent:list"}"#;
        assert_eq!(error_code(stderr).as_deref(), Some("server_not_running"));
        assert!(Listing::Agents.parse("").is_empty());
    }

    #[test]
    fn status_label_names_each_variant() {
        assert_eq!(Status::Idle.label(), "idle");
        assert_eq!(Status::Working.label(), "working");
        assert_eq!(Status::Blocked.label(), "blocked");
        assert_eq!(Status::Done.label(), "done");
        assert_eq!(Status::Unknown.label(), "unknown");
    }

    #[test]
    fn native_windows_cannot_attach_directly() {
        assert_eq!(can_attach(&Side::Native), !cfg!(windows));
    }

    /// A WSL server runs herdr's unix build whatever the host is.
    #[test]
    fn wsl_can_always_attach() {
        assert!(can_attach(&Side::Wsl("d".into())));
    }

    /// The preference can only ever give up a direct attach, never conjure
    /// one on a side that has none.
    #[test]
    fn asking_for_the_session_gives_up_a_direct_attach() {
        let wsl = Side::Wsl("d".into());
        assert!(attaches_directly(&wsl, AttachMode::Agent, true));
        assert!(!attaches_directly(&wsl, AttachMode::Session, true));
        assert!(!attaches_directly(&Side::Native, AttachMode::Session, true));
        assert_eq!(attaches_directly(&Side::Native, AttachMode::Agent, true), !cfg!(windows));
    }

    #[test]
    fn native_runs_herdr_directly() {
        let (program, args) = Side::Native.command(&["agent", "list"]);
        assert_eq!(program, "herdr");
        assert_eq!(args, vec!["agent", "list"]);
    }

    /// herdr installs to ~/.local/bin, which reaches PATH only under a login
    /// shell.  `wsl.exe -e herdr` fails with execvpe ENOENT.
    #[test]
    fn wsl_wraps_in_a_login_shell() {
        let (program, args) = Side::Wsl("kali-linux".into()).command(&["agent", "list"]);
        assert_eq!(program, "wsl.exe");
        assert_eq!(args, vec!["-d", "kali-linux", "--exec", "sh", "-lc", "herdr agent list"]);
    }

    #[test]
    fn wsl_quotes_arguments_that_need_it() {
        let (_, args) = Side::Wsl("d".into()).command(&["agent", "attach", "w1:p1"]);
        assert_eq!(args.last().unwrap(), "herdr agent attach 'w1:p1'");
    }

    #[test]
    fn direct_attach_targets_the_pane_id() {
        assert_eq!(attach_args("w5:p1"), vec!["agent", "attach", "w5:p1"]);
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

    fn at(cwd: &str, foreground: Option<&str>) -> Agent {
        Agent {
            terminal_id: "t1".into(),
            pane_id: "w1:p1".into(),
            tab_id: Some("w1:t1".into()),
            kind: None,
            title: None,
            status: Some(Status::Idle),
            focused: false,
            cwd: Some(cwd.into()),
            foreground_cwd: foreground.map(str::to_string),
        }
    }

    #[test]
    fn prefers_foreground_cwd_when_present() {
        let spaces = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let matched = match_workspace(&at("/a", Some("/b")), &Side::Native, &spaces);
        assert_eq!(matched, Some(PathBuf::from("/b")));
    }

    #[test]
    fn falls_back_to_cwd_when_foreground_is_absent() {
        let spaces = vec![PathBuf::from("/a")];
        assert_eq!(match_workspace(&at("/a/src", None), &Side::Native, &spaces), Some("/a".into()));
    }

    #[test]
    fn takes_the_longest_matching_prefix() {
        let spaces = vec![PathBuf::from("/a"), PathBuf::from("/a/nested")];
        let matched = match_workspace(&at("/a/nested/src", None), &Side::Native, &spaces);
        assert_eq!(matched, Some(PathBuf::from("/a/nested")));
    }

    /// Component-wise, so a sibling sharing a string prefix never matches.
    #[test]
    fn a_sibling_with_a_shared_prefix_does_not_match() {
        let spaces = vec![PathBuf::from("/repo")];
        assert_eq!(match_workspace(&at("/repo-other", None), &Side::Native, &spaces), None);
    }

    #[test]
    fn an_unmatched_agent_has_no_workspace() {
        let spaces = vec![PathBuf::from("/a")];
        assert_eq!(match_workspace(&at("/elsewhere", None), &Side::Native, &spaces), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefixes_compare_case_insensitively() {
        let spaces = vec![PathBuf::from(r"C:\Users\Dev\repo")];
        let matched = match_workspace(&at(r"c:\users\dev\repo\src", None), &Side::Native, &spaces);
        assert_eq!(matched, Some(PathBuf::from(r"C:\Users\Dev\repo")));
    }

    /// A translated WSL path is a Windows path, and off Windows that is one
    /// opaque component which never prefixes another.
    #[cfg(windows)]
    #[test]
    fn wsl_agent_matches_by_the_translated_windows_path() {
        let distro = "kali-linux";
        let workspace = wsl::linux_to_windows("/mnt/c/Users/dev/repo", distro);
        let spaces = vec![workspace.clone()];
        let matched = match_workspace(
            &at("/mnt/c/Users/dev/repo/src", None),
            &Side::Wsl(distro.into()),
            &spaces,
        );
        assert_eq!(matched, Some(workspace));
    }

    #[cfg(windows)]
    #[test]
    fn a_wsl_agent_outside_every_workspace_still_has_none() {
        let distro = "kali-linux";
        let spaces = vec![wsl::linux_to_windows("/mnt/c/Users/dev/repo", distro)];
        let matched =
            match_workspace(&at("/mnt/d/elsewhere", None), &Side::Wsl(distro.into()), &spaces);
        assert_eq!(matched, None);
    }

    #[test]
    fn an_attached_agent_yields_no_row() {
        let agents = vec![agent("t1", Status::Idle), agent("t2", Status::Working)];
        let claimed = [HerdrKey { side: Side::Native, terminal_id: "t1".into() }];
        let rows = unattached(&agents, &Side::Native, &claimed);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].terminal_id, "t2");
    }

    #[test]
    fn detaching_brings_the_row_back() {
        let agents = vec![agent("t1", Status::Idle)];
        assert_eq!(unattached(&agents, &Side::Native, &[]).len(), 1);
    }

    /// Terminal ids are unique only within one server, so a claim on one side
    /// must not hide the same id on another.
    #[test]
    fn a_claim_on_one_side_does_not_hide_the_other_side() {
        let agents = vec![agent("t1", Status::Idle)];
        let claimed = [HerdrKey { side: Side::Wsl("d".into()), terminal_id: "t1".into() }];
        assert_eq!(unattached(&agents, &Side::Native, &claimed).len(), 1);
    }
}
