//! What alacritree reads from herdr, and the pure questions it asks about a
//! pane herdr lists.

use crate::multiplexer::{MultiplexerKind, Pane, PaneKey, Side};

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
