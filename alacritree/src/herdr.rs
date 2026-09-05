//! Surface agents running under a herdr server in the sidebar.
//!
//! herdr owns its own PTYs and detects the agent in each pane; alacritree
//! only asks what it has and can hand one to a shell.  Everything here goes
//! through the `herdr` CLI rather than its socket, so a missing binary or an
//! absent server is a silent no-op and no wire protocol is pinned.  herdr
//! prints success on stdout and errors on stderr, which is why callers
//! capture both.

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
}
