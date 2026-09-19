//! Running the `zellij` binary.
//!
//! Everything goes through zellij's CLI, which reaches a session's server
//! over its socket, so a missing binary or no running session is a quiet
//! empty listing.  This is the only file that spawns zellij.

use std::process::{Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::listing;
use crate::command_ext;
use crate::multiplexer::{CreatedPane, Pane, Side};

/// How long one zellij call may take before it counts as no answer.  Each
/// call is a local socket round trip; this covers a cold `wsl.exe` start and
/// still keeps a wedged server from holding a pool worker.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// What one side's zellij servers answered in one poll.
#[derive(Debug, Clone)]
pub struct SideListing {
    pub side: Side,
    /// Every running session on this side, whether or not its panes could be
    /// read.
    pub sessions: Vec<String>,
    /// The sessions whose panes were read, so a pane missing from `panes` is
    /// evidence it is gone only when its session is here.
    pub read: Vec<String>,
    pub panes: Vec<Pane>,
    pub sampled_at: Instant,
}

impl SideListing {
    /// Whether this listing says a pane of `session` is gone: its session is
    /// no longer running, or its session was read and the pane was not in it.
    pub fn lost(&self, session: &str, terminal_id: &str) -> bool {
        let running = self.sessions.iter().any(|s| s == session);
        let read = self.read.iter().any(|s| s == session);
        !running || (read && !self.panes.iter().any(|pane| pane.terminal_id == terminal_id))
    }
}

/// Runs `program <args>` on `side` and hands back what it printed.  The
/// bound comes from this side because `Command::output` has none; a call
/// that times out leaves its thread parked until the child exits.
fn run(program: &str, side: &Side, args: &[&str]) -> Result<Output, String> {
    let (program, argv) = side.command(program, args);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("alacritree-zellij".into())
        .spawn(move || {
            #[allow(clippy::disallowed_methods)] // Running zellij is this function's job.
            let output = command_ext::hidden(program)
                .args(argv)
                .env("WSL_UTF8", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output();
            let _ = tx.send(output);
        })
        .map_err(|e| format!("failed to run zellij: {e}"))?;
    rx.recv_timeout(CALL_TIMEOUT)
        .map_err(|_| "zellij did not answer".to_string())?
        .map_err(|e| format!("failed to run zellij: {e}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// What zellij said when it refused, which it prints on either stream.
fn refusal(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = if stderr.trim().is_empty() { stdout(output) } else { stderr.into_owned() };
    said.trim().to_string()
}

/// Every running session on `side` and the terminal panes in each.  `Err`
/// means no zellij answered on this side at all.
pub fn list_side(program: &str, side: &Side) -> Result<SideListing, String> {
    let sampled_at = Instant::now();
    let output = run(program, side, &["list-sessions", "--no-formatting"])?;
    // zellij exits non-zero when it has no session to list, which is an
    // answer; any other failure is a shell that found no zellij.
    if !output.status.success() && !refusal(&output).contains("No active zellij sessions") {
        return Err(format!("no zellij on {}: {}", side.name(), refusal(&output)));
    }
    let sessions = listing::live_sessions(&stdout(&output));
    let mut read = Vec::new();
    let mut panes = Vec::new();
    for session in &sessions {
        let Ok(output) =
            run(program, side, &["--session", session, "action", "list-panes", "--json"])
        else {
            continue;
        };
        if let Some(listed) = listing::panes(session, &stdout(&output)) {
            read.push(session.clone());
            panes.extend(listed);
        }
    }
    Ok(SideListing { side: side.clone(), sessions, read, panes, sampled_at })
}

/// Brings `pane_id` to the front of `session`, switching to its tab.  zellij
/// applies a CLI focus to the client that last typed, or to the session's
/// own default view when no client is attached.
pub fn focus_pane(program: &str, side: &Side, session: &str, pane_id: &str) -> Result<(), String> {
    let output = run(program, side, &["--session", session, "action", "focus-pane-id", pane_id])?;
    let said = refusal(&output);
    // Focusing the pane that already has focus is refused with a non-zero
    // exit, and is exactly the state asked for.
    if output.status.success() || said.contains("already focused") {
        Ok(())
    } else {
        Err(format!("zellij refused to focus the pane: {said}"))
    }
}

/// The `zellij` arguments that open a pane in `session`.  zellij drops
/// `--cwd` from a pane given no command, so a directory comes with the
/// user's own shell named explicitly.
fn new_pane_args(session: &str, cwd: Option<&str>, focus: bool) -> Vec<String> {
    let mut args: Vec<String> =
        ["--session", session, "action", "new-pane"].map(str::to_string).into();
    if !focus {
        args.push("--no-focus".into());
    }
    if let Some(cwd) = cwd {
        args.extend(
            ["--close-on-exit", "--cwd", cwd, "--", "sh", "-c", r#"exec "${SHELL:-sh}""#]
                .map(str::to_string),
        );
    }
    args
}

/// Opens a pane in `session` on `side`, in `cwd` when one is given, spelled
/// where zellij resolves it.
pub fn create_pane(
    program: &str,
    side: &Side,
    session: &str,
    cwd: Option<&str>,
    focus: bool,
) -> Result<CreatedPane, String> {
    if cwd.is_some() && *side == Side::Native && cfg!(windows) {
        return Err("a native Windows zellij pane cannot open in a chosen directory: zellij \
                    takes a directory only alongside a command, and the shell is named through \
                    sh"
        .to_string());
    }
    let args = new_pane_args(session, cwd, focus);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run(program, side, &borrowed)?;
    let Some(id) = listing::created_pane_id(&stdout(&output)).filter(|_| output.status.success())
    else {
        return Err(format!("zellij refused to create the pane: {}", refusal(&output)));
    };
    let listed = run(program, side, &["--session", session, "action", "list-panes", "--json"])
        .ok()
        .and_then(|output| listing::panes(session, &stdout(&output)))
        .unwrap_or_default();
    let terminal_id = listing::terminal_id(session, id);
    let tab_id = listed
        .into_iter()
        .find(|pane| pane.terminal_id == terminal_id)
        .and_then(|pane| pane.tab_id)
        .unwrap_or_default();
    Ok(CreatedPane { terminal_id, pane_id: listing::pane_id(id), tab_id })
}

/// The program and argv of a client showing `session`.
pub fn attach(program: &str, side: &Side, session: &str) -> (String, Vec<String>) {
    side.command(program, &["attach", session])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// zellij applies `--cwd` only to a pane it runs a command in, so a
    /// directory without a shell named would open the pane somewhere else.
    #[test]
    fn a_pane_in_a_directory_names_the_shell() {
        let args = new_pane_args("s", Some("/repo"), true);
        let dash = args.iter().position(|a| a == "--").expect("a command follows `--`");
        assert!(args[..dash].windows(2).any(|w| w == ["--cwd", "/repo"]));
        assert_eq!(args[dash + 1..], ["sh", "-c", r#"exec "${SHELL:-sh}""#]);
    }

    #[test]
    fn a_pane_with_no_directory_is_zellijs_default() {
        assert_eq!(new_pane_args("s", None, true), ["--session", "s", "action", "new-pane"]);
    }

    #[test]
    fn a_pane_created_without_focus_says_so() {
        assert!(new_pane_args("s", None, false).contains(&"--no-focus".to_string()));
    }

    fn listing(sessions: &[&str], read: &[&str], panes: &[&str]) -> SideListing {
        let pane = |id: &&str| Pane {
            terminal_id: (*id).to_string(),
            pane_id: String::new(),
            tab_id: None,
            kind: None,
            title: None,
            status: None,
            focused: false,
            cwd: None,
            foreground_cwd: None,
        };
        SideListing {
            side: Side::Native,
            sessions: sessions.iter().map(|s| (*s).to_string()).collect(),
            read: read.iter().map(|s| (*s).to_string()).collect(),
            panes: panes.iter().map(pane).collect(),
            sampled_at: Instant::now(),
        }
    }

    #[test]
    fn a_pane_whose_session_ended_is_lost() {
        assert!(listing(&[], &[], &[]).lost("s", "s/terminal_1"));
    }

    #[test]
    fn a_pane_its_read_session_no_longer_lists_is_lost() {
        let listing = listing(&["s"], &["s"], &["s/terminal_2"]);
        assert!(listing.lost("s", "s/terminal_1"));
        assert!(!listing.lost("s", "s/terminal_2"));
    }

    /// A session whose pane listing failed this once says nothing about its
    /// panes, so none of them is closed on its account.
    #[test]
    fn a_session_that_could_not_be_read_loses_nothing() {
        assert!(!listing(&["s"], &[], &[]).lost("s", "s/terminal_1"));
    }
}
