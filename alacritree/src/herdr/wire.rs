//! What herdr's CLI prints, and how it becomes an [`Agent`].
//!
//! herdr prints success on stdout and errors on stderr, and its field set
//! grows between releases, so every type here tolerates unknown fields and a
//! reply that carries none of what was asked for.

use serde::Deserialize;

use super::{Agent, Listing, Status};

impl Listing {
    /// Panes from one reply.  An entry missing an identity — or, where the
    /// entry is an agent, a status — is dropped on its own; its siblings
    /// still parse.
    pub fn parse(&self, stdout: &str) -> Vec<Agent> {
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
        raw.into_iter().filter_map(|raw| raw.into_agent(*self)).collect()
    }
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
            kind: self
                .display_agent
                .or(self.agent)
                .map(|kind| kind.trim().to_string())
                .filter(|kind| !kind.is_empty()),
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

#[derive(Deserialize)]
pub(super) struct SessionList {
    #[serde(default)]
    pub(super) sessions: Vec<RawSession>,
}

#[derive(Deserialize)]
pub(super) struct RawSession {
    pub(super) name: String,
    #[serde(default)]
    pub(super) running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::error_code;

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

    /// A kind of nothing but spaces names no agent, and a row that carried it
    /// would render an empty word between its separators.
    #[test]
    fn a_blank_kind_is_no_kind() {
        let stdout = r#"{"result":{"agents":[
            {"terminal_id":"t1","pane_id":"w5:p1","agent_status":"idle","agent":"  "}]}}"#;
        assert_eq!(Listing::Agents.parse(stdout)[0].kind, None);
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
    fn reads_the_error_code_off_stderr() {
        let stderr = r#"{"error":{"code":"server_not_running","message":"no herdr server"},"id":"cli:agent:list"}"#;
        assert_eq!(error_code(stderr).as_deref(), Some("server_not_running"));
        assert!(Listing::Agents.parse("").is_empty());
    }
}
