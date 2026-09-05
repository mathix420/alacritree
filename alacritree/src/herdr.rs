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
    pub state_change_seq: u64,
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
    #[serde(default)]
    state_change_seq: u64,
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
                state_change_seq: raw.state_change_seq,
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

/// The native-Windows fallback: focus the pane, then attach to the whole
/// herdr session.  Two commands, so this returns a shell line rather than an
/// argv.  herdr's focus is server-global, so this moves the pane the user's
/// own herdr window is showing.
pub fn session_attach_script(pane_id: &str, session: &str) -> String {
    format!("herdr agent focus {} && herdr session attach {}", sh_quote(pane_id), sh_quote(session))
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
        }
    }

    /// Bumped only when a rendered field changes, so the sidebar's per-frame
    /// comparison does not rebuild for `state_change_seq` churn nobody can see.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn side(&self) -> &Side {
        &self.side
    }

    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// Adopts a landed result and starts a new poll when due.  Never blocks.
    pub fn poll(&mut self, interval: Duration) {
        if let Some(job) = &self.pending {
            match job.poll() {
                Some(Ok(agents)) => {
                    self.reach.record_success();
                    if rendered_differs(&self.agents, &agents) {
                        self.generation = self.generation.wrapping_add(1);
                    }
                    self.agents = agents;
                    self.pending = None;
                },
                Some(Err(code)) => {
                    if self.reach.record_failure(&code) && code != "server_not_running" {
                        log::warn!("herdr ({:?}): {code}", self.side);
                    }
                    if !self.agents.is_empty() {
                        self.agents.clear();
                        self.generation = self.generation.wrapping_add(1);
                    }
                    self.pending = None;
                },
                None if job.failed() => self.pending = None,
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
}

/// Whether anything the sidebar draws changed.  `state_change_seq`
/// deliberately does not count: it moves on output the row does not show.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_windows_fallback_focuses_then_attaches_the_session() {
        assert_eq!(
            session_attach_script("w5:p1", "default"),
            "herdr agent focus 'w5:p1' && herdr session attach default"
        );
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
            state_change_seq: 0,
        }
    }

    #[test]
    fn churn_the_sidebar_cannot_see_does_not_count_as_a_change() {
        let was = vec![agent("t1", Status::Idle)];
        let mut now = was.clone();
        now[0].state_change_seq = 99;
        assert!(!rendered_differs(&was, &now));
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
            state_change_seq: 0,
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
        let matched =
            match_workspace(&at("/mnt/c/Users/Lev/repo/src", None), &Side::Wsl(distro.into()), &spaces);
        assert_eq!(matched, Some(workspace));
    }

    #[test]
    fn a_wsl_agent_outside_every_workspace_still_has_none() {
        let distro = "kali-linux";
        let spaces = vec![wsl::linux_to_windows("/mnt/c/Users/Lev/repo", distro)];
        let matched = match_workspace(&at("/mnt/d/elsewhere", None), &Side::Wsl(distro.into()), &spaces);
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
