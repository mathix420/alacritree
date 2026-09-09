//! What alacritree means by a herdr agent, and the pure questions it can ask
//! about one.

use std::path::{Path, PathBuf};

use crate::wsl;

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
    pub(super) fn parse(raw: &str) -> Self {
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

/// Identifies one herdr agent across polls.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HerdrKey {
    pub side: Side,
    pub terminal_id: String,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_names_each_variant() {
        assert_eq!(Status::Idle.label(), "idle");
        assert_eq!(Status::Working.label(), "working");
        assert_eq!(Status::Blocked.label(), "blocked");
        assert_eq!(Status::Done.label(), "done");
        assert_eq!(Status::Unknown.label(), "unknown");
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
