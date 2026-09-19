//! Reading what the zellij CLI prints about its sessions and panes.

use serde::Deserialize;

use crate::multiplexer::Pane;

/// The live sessions in `zellij list-sessions --no-formatting` output.  A
/// session zellij keeps only so it can be resurrected is listed too, marked
/// `EXITED`, and has no server to ask about its panes.
pub(super) fn live_sessions(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| !line.contains("(EXITED"))
        .filter_map(|line| line.split_once(" [Created ").map(|(name, _)| name))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// One entry of `zellij action list-panes --json`.  Only the fields a row
/// needs are read, so a field zellij adds later changes nothing here.
#[derive(Deserialize)]
struct ListedPane {
    id: u32,
    is_plugin: bool,
    is_focused: bool,
    title: String,
    tab_id: usize,
    #[serde(default)]
    pane_cwd: Option<String>,
}

/// The terminal panes in `zellij action list-panes --json` output for
/// `session`.  Plugin panes (the tab bar, the status bar) are zellij's own
/// chrome and never a row.  `None` when the output is not a listing: zellij
/// asked about a session that is gone prints the session list instead, and
/// exits zero.
pub(super) fn panes(session: &str, stdout: &str) -> Option<Vec<Pane>> {
    let listed: Vec<ListedPane> = serde_json::from_str(stdout).ok()?;
    Some(
        listed
            .into_iter()
            .filter(|pane| !pane.is_plugin)
            .map(|pane| Pane {
                terminal_id: terminal_id(session, pane.id),
                pane_id: pane_id(pane.id),
                tab_id: Some(pane.tab_id.to_string()),
                kind: None,
                title: Some(pane.title).filter(|title| !title.trim().is_empty()),
                status: None,
                focused: pane.is_focused,
                cwd: pane.pane_cwd,
                foreground_cwd: None,
            })
            .collect(),
    )
}

/// How zellij's CLI names a terminal pane.
pub(super) fn pane_id(id: u32) -> String {
    format!("terminal_{id}")
}

/// A pane's identity across polls.  zellij numbers terminal panes from one
/// counter per server that never hands a number out twice, and a pane keeps
/// its number when it moves between tabs, so the number is stable within its
/// session.  zellij refuses a session name containing `/`, which is what
/// makes the split in [`split_terminal_id`] unambiguous.
pub fn terminal_id(session: &str, id: u32) -> String {
    format!("{session}/{}", pane_id(id))
}

/// The session and the CLI's pane id a [`terminal_id`] was built from.
pub fn split_terminal_id(terminal_id: &str) -> Option<(&str, &str)> {
    terminal_id.split_once('/').filter(|(session, pane)| {
        !session.is_empty()
            && pane.strip_prefix("terminal_").is_some_and(|n| n.parse::<u32>().is_ok())
    })
}

/// The pane `zellij action new-pane` just made, from the id it prints.
pub(super) fn created_pane_id(stdout: &str) -> Option<u32> {
    stdout.trim().strip_prefix("terminal_")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSIONS: &str = include_str!("fixtures/list-sessions.txt");
    const PANES: &str = include_str!("fixtures/list-panes.json");

    /// Captured from zellij 0.45.1, which lists a resurrectable session
    /// alongside the running ones.
    #[test]
    fn a_session_kept_for_resurrection_is_not_live() {
        assert_eq!(live_sessions(SESSIONS), vec!["alacritree-probe", "alacritree-probe2"]);
    }

    #[test]
    fn no_sessions_list_nothing() {
        assert!(live_sessions("").is_empty());
    }

    /// Captured from zellij 0.45.1 with two tabs, a floating pane whose
    /// command exited, and the tab and status bar plugins.
    #[test]
    fn plugin_panes_are_not_rows() {
        let panes = panes("probe", PANES).expect("the fixture is a listing");
        let ids: Vec<&str> = panes.iter().map(|pane| pane.pane_id.as_str()).collect();
        assert_eq!(ids, [
            "terminal_0",
            "terminal_1",
            "terminal_3",
            "terminal_4",
            "terminal_5",
            "terminal_8",
            "terminal_7"
        ]);
    }

    #[test]
    fn a_pane_carries_its_tab_cwd_and_session() {
        let panes = panes("probe", PANES).expect("the fixture is a listing");
        let pane = panes.iter().find(|pane| pane.pane_id == "terminal_7").expect("listed");
        assert_eq!(pane.terminal_id, "probe/terminal_7");
        assert_eq!(pane.tab_id.as_deref(), Some("1"));
        assert_eq!(pane.cwd.as_deref(), Some("/tmp"));
        assert_eq!(pane.status, None, "zellij detects no agents");
    }

    /// The fixture's session had `terminal_1` focused through the CLI and no
    /// client ever showing the second tab.
    #[test]
    fn the_focused_terminal_pane_is_marked() {
        let panes = panes("probe", PANES).expect("the fixture is a listing");
        let focused: Vec<&str> =
            panes.iter().filter(|pane| pane.focused).map(|pane| pane.pane_id.as_str()).collect();
        assert_eq!(focused, ["terminal_1"]);
    }

    /// Asked about a session that is gone, zellij prints its session list and
    /// exits zero, which must not read as a session with no panes.
    #[test]
    fn a_session_list_is_not_a_pane_listing() {
        assert!(panes("gone", SESSIONS).is_none());
    }

    #[test]
    fn a_terminal_id_splits_back_into_session_and_pane() {
        let id = terminal_id("my session", 12);
        assert_eq!(split_terminal_id(&id), Some(("my session", "terminal_12")));
    }

    /// herdr's ids have no `/`, so a zellij lookup never claims one.
    #[test]
    fn a_foreign_terminal_id_does_not_split() {
        for id in ["t1", "w1:p1", "/terminal_1", "s/plugin_2", "s/terminal_x"] {
            assert_eq!(split_terminal_id(id), None, "{id} split");
        }
    }

    #[test]
    fn a_created_pane_id_is_read_from_new_pane_output() {
        assert_eq!(created_pane_id("terminal_5\n"), Some(5));
        assert_eq!(created_pane_id("plugin_2\n"), None);
    }
}
