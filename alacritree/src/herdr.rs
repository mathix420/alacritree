//! Surface agents running under a herdr server in the sidebar.
//!
//! herdr owns its own PTYs and detects the agent in each pane; alacritree
//! only asks what it has and can hand one to a shell.  Everything here goes
//! through the `herdr` CLI rather than its socket, so a missing binary or an
//! absent server is a silent no-op and no wire protocol is pinned.  herdr
//! prints success on stdout and errors on stderr, which is why callers
//! capture both.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

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

/// One agent as herdr reports it.  `terminal_id` is the identity because
/// `pane_id` is positional: a pane moved between workspaces gets a new one,
/// and ids restart at `w1` after `session delete`.
#[derive(Debug, Clone)]
pub struct Agent {
    pub terminal_id: String,
    pub pane_id: String,
    pub kind: Option<String>,
    pub status: Status,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    result: Option<AgentList>,
}

#[derive(Deserialize)]
struct AgentList {
    #[serde(default)]
    agents: Vec<RawAgent>,
}

/// Only the fields the sidebar renders.  Everything else herdr sends is
/// ignored, so an additive protocol change costs nothing.
#[derive(Deserialize)]
struct RawAgent {
    terminal_id: Option<String>,
    pane_id: Option<String>,
    agent_status: Option<String>,
    agent: Option<String>,
    display_agent: Option<String>,
    cwd: Option<String>,
    foreground_cwd: Option<String>,
}

/// Agents from one `herdr agent list` reply.  An agent missing an identity
/// or a status is dropped on its own; its siblings still parse.
pub fn parse_agent_list(stdout: &str) -> Vec<Agent> {
    let Ok(envelope) = serde_json::from_str::<Envelope>(stdout) else {
        return Vec::new();
    };
    let Some(list) = envelope.result else {
        return Vec::new();
    };
    list.agents
        .into_iter()
        .filter_map(|raw| {
            Some(Agent {
                terminal_id: raw.terminal_id?,
                pane_id: raw.pane_id?,
                status: Status::parse(&raw.agent_status?),
                kind: raw.display_agent.or(raw.agent),
                cwd: raw.cwd,
                foreground_cwd: raw.foreground_cwd,
            })
        })
        .collect()
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

/// Focuses one pane in the user's own herdr window, the first half of the
/// native-Windows attach fallback.  A non-zero exit carries herdr's stderr
/// verbatim rather than `error_code`'s parsed code, since a user-facing
/// message wants herdr's human-readable text, not its machine code, and a
/// server that does not answer inside [`GESTURE_TIMEOUT`] refuses the same
/// way.
pub fn focus_agent(side: &Side, pane_id: &str) -> Result<(), String> {
    let (program, args) = side.command(&["agent", "focus", pane_id]);
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
/// refuse `c:\users\lev` against `C:\Users\Lev`.
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

/// How long a server that has answered before waits before being retried.
const RECOVERY_RETRY: Duration = Duration::from_secs(30);

/// Whether an endpoint is worth talking to.  An endpoint that has never
/// answered is abandoned, so a machine with no herdr pays one failed spawn
/// rather than one per tick; an endpoint that answered and then stopped is
/// retried forever, because `herdr update` restarts the server.
#[derive(Debug, Default)]
pub struct Reach {
    ever_answered: bool,
    failing: bool,
    last_error: Option<String>,
}

impl Reach {
    /// Whether to poll again, given how long it has been since the last try.
    pub fn should_retry(&self, since_last: Duration) -> bool {
        match (self.failing, self.ever_answered) {
            (false, _) => true,
            (true, true) => since_last >= RECOVERY_RETRY,
            (true, false) => false,
        }
    }

    /// Whether this endpoint has been given up on for the process lifetime:
    /// it failed without ever having answered, so nothing will poll it again.
    pub fn abandoned(&self) -> bool {
        self.failing && !self.ever_answered
    }

    pub fn record_success(&mut self) {
        self.ever_answered = true;
        self.failing = false;
        self.last_error = None;
    }

    /// Records a failure, returning whether it is worth logging — a code
    /// repeating every tick is logged once, not once per poll.
    pub fn record_failure(&mut self, code: &str) -> bool {
        self.failing = true;
        let novel = self.last_error.as_deref() != Some(code);
        self.last_error = Some(code.to_string());
        novel
    }
}

/// One herdr server's agents, refreshed off the UI thread.
pub struct EndpointCache {
    side: Side,
    agents: Vec<Agent>,
    generation: u64,
    reach: Reach,
    last_attempt: Option<Instant>,
    pending: Option<jobs::Job<Result<Vec<Agent>, String>>>,
    chord: Chord,
}

impl EndpointCache {
    pub fn new(side: Side) -> Self {
        Self {
            side,
            agents: Vec::new(),
            generation: 0,
            reach: Reach::default(),
            last_attempt: None,
            pending: None,
            chord: Chord::Unread,
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

    /// herdr's detach chord here, once the config read has landed.  `None`
    /// until then, and afterwards when herdr binds detach to nothing or the
    /// side could not be read — all three are reasons to say nothing rather
    /// than to name a chord the user may not have.
    pub fn detach(&self) -> Option<&str> {
        match &self.chord {
            Chord::Read(chord) => chord.as_deref(),
            Chord::Unread | Chord::Pending(_) => None,
        }
    }

    /// Adopt a landed config read.  The chord is part of what a row renders,
    /// so arriving late still has to invalidate the sidebar's comparison.
    fn advance_chord(&mut self) {
        let Chord::Pending(job) = &self.chord else { return };
        if let Some(chord) = job.poll() {
            self.chord = Chord::Read(chord);
            self.generation = self.generation.wrapping_add(1);
        } else if job.failed() {
            self.chord = Chord::Read(None);
        }
    }

    /// Read the config once this endpoint has proved a herdr lives here.
    /// Starting at construction would run a shell inside every installed
    /// distro to learn a chord no row will ever show.
    fn start_chord_read(&mut self) {
        if !matches!(self.chord, Chord::Unread) {
            return;
        }
        let side = self.side.clone();
        self.chord = Chord::Pending(
            jobs::pool()
                .spawn(jobs::Priority::Background, move |blocking| detach_chord(&side, blocking)),
        );
    }

    /// Adopts a landed result and starts a new poll when due.  Never blocks.
    pub fn poll(&mut self, interval: Duration) {
        self.advance_chord();
        if let Some(job) = &self.pending {
            match job.poll() {
                Some(Ok(agents)) => {
                    self.reach.record_success();
                    self.start_chord_read();
                    if rendered_differs(&self.agents, &agents) {
                        self.generation = self.generation.wrapping_add(1);
                    }
                    self.agents = agents;
                    self.pending = None;
                },
                Some(Err(code)) => {
                    self.note_failure(&code);
                    if !self.agents.is_empty() {
                        self.agents.clear();
                        self.generation = self.generation.wrapping_add(1);
                    }
                    self.pending = None;
                },
                // A closure that unwound answered nothing, which is what
                // `Reach` tracks, and recording it is what stops a panicking
                // poll from being respawned every tick for the process life.
                None if job.failed() => {
                    self.note_failure("poll_panicked");
                    self.pending = None;
                },
                None => return,
            }
        }

        let since = self.last_attempt.map_or(interval, |t| t.elapsed());
        if since < interval || !self.reach.should_retry(since) {
            return;
        }
        self.last_attempt = Some(Instant::now());
        let side = self.side.clone();
        self.pending = Some(
            jobs::pool()
                .spawn(jobs::Priority::Background, move |blocking| list_agents(&side, blocking)),
        );
    }

    /// Records one poll that produced no agents, and says so once.  A novel
    /// code that is not the ordinary "no server here" is a warning; giving up
    /// on an endpoint is a debug line, so a herdr that is installed but never
    /// answers can still be explained from a log rather than only by an empty
    /// sidebar.  Both fire at most once per endpoint, since an endpoint that
    /// never answered is never polled again.
    fn note_failure(&mut self, code: &str) {
        if self.reach.record_failure(code) && code != "server_not_running" {
            log::warn!("herdr ({:?}): {code}", self.side);
        }
        if self.reach.abandoned() {
            log::debug!("herdr ({:?}): {code}; not polling this endpoint again", self.side);
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
    pub fn poll(&mut self, interval: Duration) {
        self.refresh_running();
        for cache in &mut self.caches {
            cache.poll(interval);
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
                None if job.failed() => self.running = None,
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
                || a.cwd != b.cwd
                || a.foreground_cwd != b.foreground_cwd
                || a.pane_id != b.pane_id
        })
}

/// Runs `herdr agent list` on one side.  Success is on stdout, errors are on
/// stderr, so both are captured; the exit status decides which to read.
///
/// wsl.exe's own failure messages (a missing distro, for instance) come back
/// UTF-16LE unless WSL_UTF8 is set, and `from_utf8_lossy` mangles them without
/// it, so they never match an error code and degrade to `server_not_running`.
/// herdr's own output is a relayed Linux byte stream and is unaffected either
/// way.
#[allow(clippy::disallowed_methods)] // Running herdr is this function's job.
fn list_agents(side: &Side, _blocking: &jobs::Blocking) -> Result<Vec<Agent>, String> {
    let (program, args) = side.command(&["agent", "list"]);
    let output = command_ext::hidden(program)
        .args(args)
        .env("WSL_UTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|_| "spawn_failed".to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(error_code(&stderr).unwrap_or_else(|| "server_not_running".to_string()));
    }
    Ok(parse_agent_list(&String::from_utf8_lossy(&output.stdout)))
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

/// herdr's detach chord on this side, spelled as herdr documents it.  Both
/// halves are user-settable, so a hint naming a chord the user has rebound
/// would be worse than no hint at all.
pub fn detach_chord(side: &Side, blocking: &jobs::Blocking) -> Option<String> {
    detach_chord_from(&read_config(side, blocking)?)
}

/// A config read runs once per endpoint: herdr rereads its own config only on
/// request, and re-running a shell inside every distro on the listing cadence
/// would cost a process per distro per tick to learn a string that does not
/// move.
enum Chord {
    Unread,
    Pending(jobs::Job<Option<String>>),
    Read(Option<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Captured from a native Windows server.  `skip_serializing_if` drops
    /// `foreground_cwd`, `name`, `display_agent` and `agent_session` rather
    /// than emitting them as null.
    const WINDOWS: &str = r#"{"id":"cli:agent:list","result":{"agents":[
        {"agent":"claude","agent_status":"idle","pane_id":"w5:p1",
         "terminal_id":"term_65abfc8e300361","revision":7,"state_change_seq":3,
         "cwd":"C:\\Users\\Lev\\Git\\github\\alacritree","focused":true,
         "tab_id":"w5:t1","workspace_id":"w5"}],"type":"agent_list"}}"#;

    #[test]
    fn parses_a_windows_agent_with_absent_optional_fields() {
        let agents = parse_agent_list(WINDOWS);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].terminal_id, "term_65abfc8e300361");
        assert_eq!(agents[0].pane_id, "w5:p1");
        assert_eq!(agents[0].kind.as_deref(), Some("claude"));
        assert_eq!(agents[0].status, Status::Idle);
        assert_eq!(agents[0].foreground_cwd, None);
    }

    /// Captured from a WSL server, which does populate `foreground_cwd`.
    const WSL: &str = r#"{"id":"cli:agent:list","result":{"agents":[
        {"agent":"codex","agent_status":"idle","pane_id":"w4:p1",
         "terminal_id":"term_65ab9ae95a74d2","revision":9,"state_change_seq":9,
         "cwd":"/home/lev/Git/lev/devkit","foreground_cwd":"/home/lev/Git/lev/devkit",
         "focused":true,"tab_id":"w4:t1","workspace_id":"w4"}],"type":"agent_list"}}"#;

    #[test]
    fn parses_a_wsl_agent_with_foreground_cwd() {
        let agents = parse_agent_list(WSL);
        assert_eq!(agents[0].foreground_cwd.as_deref(), Some("/home/lev/Git/lev/devkit"));
        assert_eq!(agents[0].kind.as_deref(), Some("codex"));
    }

    #[test]
    fn empty_agent_list_is_not_an_error() {
        let reply = r#"{"id":"cli:agent:list","result":{"agents":[],"type":"agent_list"}}"#;
        assert!(parse_agent_list(reply).is_empty());
    }

    #[test]
    fn unknown_fields_and_unknown_status_survive() {
        let reply = r#"{"id":"x","surprise":1,"result":{"agents":[
            {"terminal_id":"t1","pane_id":"w1:p1","agent_status":"meditating",
             "future_field":true}],"type":"agent_list"}}"#;
        let agents = parse_agent_list(reply);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, Status::Unknown);
    }

    #[test]
    fn an_agent_without_an_identity_is_dropped_alone() {
        let reply = r#"{"id":"x","result":{"agents":[
            {"pane_id":"w1:p1","agent_status":"idle"},
            {"terminal_id":"t2","pane_id":"w1:p2","agent_status":"idle"}],"type":"agent_list"}}"#;
        let agents = parse_agent_list(reply);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].terminal_id, "t2");
    }

    #[test]
    fn display_agent_wins_over_agent() {
        let reply = r#"{"id":"x","result":{"agents":[
            {"terminal_id":"t1","pane_id":"w1:p1","agent_status":"idle",
             "agent":"claude","display_agent":"Claude Code"}],"type":"agent_list"}}"#;
        assert_eq!(parse_agent_list(reply)[0].kind.as_deref(), Some("Claude Code"));
    }

    /// The reply that arrives on stderr with stdout empty when no server is
    /// listening.  A parser reading only stdout never sees this.
    #[test]
    fn reads_the_error_code_off_stderr() {
        let stderr = r#"{"error":{"code":"server_not_running","message":"no herdr server"},"id":"cli:agent:list"}"#;
        assert_eq!(error_code(stderr).as_deref(), Some("server_not_running"));
        assert!(parse_agent_list("").is_empty());
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
    fn an_endpoint_that_never_answered_is_given_up_on() {
        let mut reach = Reach::default();
        reach.record_failure("server_not_running");
        assert!(!reach.should_retry(Duration::from_secs(3600)));
    }

    #[test]
    fn an_endpoint_that_answered_once_keeps_retrying() {
        let mut reach = Reach::default();
        reach.record_success();
        reach.record_failure("server_not_running");
        assert!(!reach.should_retry(Duration::from_secs(5)));
        assert!(reach.should_retry(Duration::from_secs(31)));
    }

    #[test]
    fn a_recovered_endpoint_polls_at_the_normal_interval_again() {
        let mut reach = Reach::default();
        reach.record_success();
        reach.record_failure("server_not_running");
        reach.record_success();
        assert!(reach.should_retry(Duration::from_secs(0)));
    }

    #[test]
    fn an_endpoint_is_abandoned_only_after_a_failure_it_never_answered() {
        let mut reach = Reach::default();
        assert!(!reach.abandoned());
        reach.record_success();
        reach.record_failure("server_not_running");
        assert!(!reach.abandoned(), "a server that answered once is retried, not given up on");

        let mut never = Reach::default();
        never.record_failure("server_not_running");
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
        assert!(reach.record_failure("protocol_mismatch"));
        assert!(!reach.record_failure("protocol_mismatch"));
        assert!(reach.record_failure("server_not_running"));
    }

    fn agent(id: &str, status: Status) -> Agent {
        Agent {
            terminal_id: id.into(),
            pane_id: "w1:p1".into(),
            kind: Some("claude".into()),
            status,
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
            kind: None,
            status: Status::Idle,
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
        let spaces = vec![PathBuf::from(r"C:\Users\Lev\repo")];
        let matched = match_workspace(&at(r"c:\users\lev\repo\src", None), &Side::Native, &spaces);
        assert_eq!(matched, Some(PathBuf::from(r"C:\Users\Lev\repo")));
    }

    #[test]
    fn wsl_agent_matches_by_the_translated_windows_path() {
        let distro = "kali-linux";
        let workspace = wsl::linux_to_windows("/mnt/c/Users/Lev/repo", distro);
        let spaces = vec![workspace.clone()];
        let matched = match_workspace(
            &at("/mnt/c/Users/Lev/repo/src", None),
            &Side::Wsl(distro.into()),
            &spaces,
        );
        assert_eq!(matched, Some(workspace));
    }

    #[test]
    fn a_wsl_agent_outside_every_workspace_still_has_none() {
        let distro = "kali-linux";
        let spaces = vec![wsl::linux_to_windows("/mnt/c/Users/Lev/repo", distro)];
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
