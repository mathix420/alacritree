//! Where a task lives, as a taskwarrior project node. The CLI, the hook and
//! the tab each gather facts their own way and meet here, so all three name
//! a place the same way.

pub(crate) const GLOBAL: &str = "global";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// An agent conversation, keyed by the harness's own id so a resumed
/// conversation finds its tasks again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRef {
    pub harness: Harness,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    Global,
    Project { repo: String },
    Workspace { repo: String, branch: String },
}

/// Taskwarrior splits project names on `.`, so a dot inside one segment would
/// invent a level.
pub(crate) fn sanitize(segment: &str) -> String {
    segment.chars().map(|c| if matches!(c, '.' | '/' | '\\') { '-' } else { c }).collect()
}

/// A session only means something inside a workspace; above one there is no
/// conversation to key.
pub(crate) fn node(place: &Place, session: Option<&SessionRef>) -> String {
    match place {
        Place::Global => GLOBAL.to_string(),
        Place::Project { repo } => sanitize(repo),
        Place::Workspace { repo, branch } => {
            let workspace = format!("{}.{}", sanitize(repo), sanitize(branch));
            match session {
                Some(s) => format!("{workspace}.{}-{}", s.harness.prefix(), sanitize(&s.id)),
                None => workspace,
            }
        },
    }
}

/// Codex's hook payload carries the root session id, which is also what it
/// exports as `CODEX_SESSION_ID`, so keying on it keeps a subagent's shell
/// and the hook on one node.
pub(crate) fn session_from_env(get: impl Fn(&str) -> Option<String>) -> Option<SessionRef> {
    [("CODEX_SESSION_ID", Harness::Codex), ("CLAUDE_CODE_SESSION_ID", Harness::Claude)]
        .into_iter()
        .find_map(|(key, harness)| {
            let id = get(key)?.trim().to_string();
            (!id.is_empty()).then_some(SessionRef { harness, id })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(repo: &str, branch: &str) -> Place {
        Place::Workspace { repo: repo.into(), branch: branch.into() }
    }

    fn codex(id: &str) -> SessionRef {
        SessionRef { harness: Harness::Codex, id: id.into() }
    }

    #[test]
    fn each_place_names_its_node() {
        assert_eq!(node(&Place::Global, None), "global");
        assert_eq!(node(&Place::Project { repo: "alacritree".into() }, None), "alacritree");
        assert_eq!(node(&ws("alacritree", "master"), None), "alacritree.master");
        assert_eq!(
            node(&ws("alacritree", "master"), Some(&codex("0199a"))),
            "alacritree.master.codex-0199a"
        );
    }

    #[test]
    fn a_session_below_anything_but_a_workspace_is_dropped() {
        assert_eq!(node(&Place::Global, Some(&codex("x"))), "global");
        assert_eq!(node(&Place::Project { repo: "r".into() }, Some(&codex("x"))), "r");
    }

    #[test]
    fn dots_and_slashes_never_add_levels() {
        let session = SessionRef { harness: Harness::Claude, id: "a.b/c".into() };
        let name = node(&ws("my.repo", "feat/v1.2"), Some(&session));
        assert_eq!(name, "my-repo.feat-v1-2.claude-a-b-c");
        assert_eq!(name.matches('.').count(), 2);
    }

    #[test]
    fn codex_session_id_wins_over_claude() {
        let env = |key: &str| match key {
            "CODEX_SESSION_ID" => Some("c1".to_string()),
            "CLAUDE_CODE_SESSION_ID" => Some("k1".to_string()),
            _ => None,
        };
        assert_eq!(session_from_env(env), Some(codex("c1")));
    }

    #[test]
    fn claude_session_id_is_read_when_codex_is_absent() {
        let env = |key: &str| (key == "CLAUDE_CODE_SESSION_ID").then(|| "k1".to_string());
        assert_eq!(
            session_from_env(env),
            Some(SessionRef { harness: Harness::Claude, id: "k1".into() })
        );
    }

    #[test]
    fn blank_ids_are_no_session() {
        let env = |key: &str| (key == "CODEX_SESSION_ID").then(|| "  ".to_string());
        assert_eq!(session_from_env(env), None);
    }

    #[test]
    fn harness_names_round_trip() {
        assert_eq!(Harness::parse("claude"), Some(Harness::Claude));
        assert_eq!(Harness::parse("codex"), Some(Harness::Codex));
        assert_eq!(Harness::parse("gemini"), None);
    }
}
