//! What alacritree reads from herdr, and the pure questions it asks about a
//! pane herdr lists.

use std::path::{Path, PathBuf};

use crate::multiplexer::{MultiplexerKind, Pane, PaneKey, Side};
use crate::wsl;

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
pub(super) fn unattached<'a>(
    agents: &'a [Pane],
    side: &Side,
    claimed: &[PaneKey],
) -> Vec<&'a Pane> {
    agents
        .iter()
        .filter(|a| {
            !claimed.iter().any(|k| {
                k.multiplexer == MultiplexerKind::Herdr
                    && k.side == *side
                    && k.terminal_id == a.terminal_id
            })
        })
        .collect()
}

/// The sidebar workspace an agent is working in, by longest path prefix.
/// `None` means it belongs under Home.
pub(super) fn match_workspace(
    agent: &Pane,
    side: &Side,
    workspaces: &[PathBuf],
) -> Option<PathBuf> {
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

/// The key a herdr pane on `side` is known by.
pub(crate) fn pane_key(side: Side, terminal_id: String) -> PaneKey {
    PaneKey { multiplexer: MultiplexerKind::Herdr, side, terminal_id }
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
    use crate::multiplexer::PaneStatus;

    #[test]
    fn status_label_names_each_variant() {
        assert_eq!(PaneStatus::Idle.label(), "idle");
        assert_eq!(PaneStatus::Working.label(), "working");
        assert_eq!(PaneStatus::Blocked.label(), "blocked");
        assert_eq!(PaneStatus::Done.label(), "done");
        assert_eq!(PaneStatus::Unknown.label(), "unknown");
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

    fn at(cwd: &str, foreground: Option<&str>) -> Pane {
        Pane {
            terminal_id: "t1".into(),
            pane_id: "w1:p1".into(),
            tab_id: Some("w1:t1".into()),
            kind: None,
            title: None,
            status: Some(PaneStatus::Idle),
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
        let agents = vec![agent("t1", PaneStatus::Idle), agent("t2", PaneStatus::Working)];
        let claimed = [pane_key(Side::Native, "t1".into())];
        let rows = unattached(&agents, &Side::Native, &claimed);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].terminal_id, "t2");
    }

    #[test]
    fn detaching_brings_the_row_back() {
        let agents = vec![agent("t1", PaneStatus::Idle)];
        assert_eq!(unattached(&agents, &Side::Native, &[]).len(), 1);
    }

    /// Terminal ids are unique only within one server, so a claim on one side
    /// must not hide the same id on another.
    #[test]
    fn a_claim_on_one_side_does_not_hide_the_other_side() {
        let agents = vec![agent("t1", PaneStatus::Idle)];
        let claimed = [pane_key(Side::Wsl("d".into()), "t1".into())];
        assert_eq!(unattached(&agents, &Side::Native, &claimed).len(), 1);
    }
}
