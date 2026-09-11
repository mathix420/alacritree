//! Running the `herdr` binary.
//!
//! Everything goes through the CLI rather than herdr's socket, so a missing
//! binary or an absent server is a silent no-op and no wire protocol is
//! pinned.  This is the only file that knows how to reach a herdr server; a
//! second multiplexer would need its own equivalent and nothing else.

use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::config::AttachMode;
use crate::multiplexer::{CreatedPane, PaneTarget};
use crate::{command_ext, jobs};

use super::wire::{CreatedTab, SessionList};
use super::{Listing, ListingReply, PollError, Side, error_code};

/// The binary every call here runs.  `Side::command` takes it as an argument
/// so a second multiplexer reaches its own through the same plumbing.
pub const PROGRAM: &str = "herdr";

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

/// Whether a row on `side` opens the agent's own pane.  Capability and
/// preference are separate questions and only disagree in one direction: a
/// user who asks for a direct attach on a side that has none gets the
/// session, and no user can be given a direct attach they did not ask for.
///
/// `has_agent` is false for a pane herdr found no agent in.  Every `herdr
/// agent` subcommand resolves its target through the agent registry, which
/// holds nothing for such a pane, so it is only ever reachable through the
/// session.
pub fn attaches_directly(side: &Side, mode: AttachMode, has_agent: bool) -> bool {
    has_agent && mode == AttachMode::Agent && can_attach(side)
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
pub(super) fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("alacritree-herdr-gesture".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(GESTURE_TIMEOUT).ok()
}

/// The `herdr` subcommand that brings `target`'s pane to the front of the
/// user's own herdr window.  `agent focus` resolves its target through the
/// agent registry and answers `agent_not_found` for a pane with no agent in
/// it, so such a pane is reached by focusing the tab that holds it.
pub fn focus_args(target: &PaneTarget) -> Vec<String> {
    match (target.has_agent, &target.tab_id) {
        (false, Some(tab_id)) => vec!["tab".into(), "focus".into(), tab_id.clone()],
        _ => focus_pane_args(&target.pane_id),
    }
}

/// The `herdr` subcommand that focuses one pane by id.
pub fn focus_pane_args(pane_id: &str) -> Vec<String> {
    vec!["agent".into(), "focus".into(), pane_id.into()]
}

/// Focuses one pane in the user's own herdr window, the first half of the
/// native-Windows attach fallback.  A non-zero exit carries herdr's stderr
/// verbatim rather than `error_code`'s parsed code, since a user-facing
/// message wants herdr's human-readable text, not its machine code, and a
/// server that does not answer inside [`GESTURE_TIMEOUT`] refuses the same
/// way.
pub fn focus_pane(side: &Side, focus: &[String]) -> Result<(), String> {
    let borrowed: Vec<&str> = focus.iter().map(String::as_str).collect();
    let (program, args) = side.command(PROGRAM, &borrowed);
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

/// The running session to attach to on this side.  `herdr session list
/// --json` is a flat object rather than the `result`-wrapped envelope
/// `agent list` uses.  An answer that names nothing falls back to `default`,
/// the name herdr gives an unnamed session; a server that does not answer
/// inside [`GESTURE_TIMEOUT`] is an `Err`, because attaching to a guessed
/// name would only park the wedged wait inside the new session.
pub fn running_session_name(side: &Side) -> Result<String, String> {
    let (program, args) = side.command(PROGRAM, &["session", "list", "--json"]);
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

/// The `herdr` subcommand that opens a tab and brings it to the front, so a
/// shared view attaching afterwards is already showing the pane it made.
/// `--cwd` is left off entirely when no directory is chosen, since herdr's
/// own default is a better answer than an empty path.
fn create_args(cwd: Option<&str>) -> Vec<String> {
    let mut args = vec!["tab".into(), "create".into(), "--focus".into()];
    if let Some(cwd) = cwd {
        args.push("--cwd".into());
        args.push(cwd.into());
    }
    args
}

/// Opens a tab in the user's own herdr window and focuses it.  A process
/// spawn that waits on herdr starting up, so it only ever runs on the pool.
///
/// `cwd` is spelled in the side's own terms: a Windows path on the native
/// side, and a path inside the distro on a WSL one, since herdr resolves it
/// where it runs.
pub fn create_pane(side: &Side, cwd: Option<String>) -> Result<CreatedPane, String> {
    let create = create_args(cwd.as_deref());
    let borrowed: Vec<&str> = create.iter().map(String::as_str).collect();
    let (program, args) = side.command(PROGRAM, &borrowed);
    #[allow(clippy::disallowed_methods)] // Running herdr is this function's job.
    let run = move || {
        command_ext::hidden(program)
            .args(args)
            .env("WSL_UTF8", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    };
    let Some(output) = bounded(run) else {
        return Err("herdr did not answer while creating the pane".to_string());
    };
    let output = output.map_err(|e| format!("failed to create a herdr pane: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("herdr refused to create the pane: {stderr}"));
    }
    let created = serde_json::from_slice::<CreatedTab>(&output.stdout)
        .map_err(|_| "herdr answered with no pane".to_string())?;
    let root = created.result.root_pane;
    Ok(CreatedPane { terminal_id: root.terminal_id, pane_id: root.pane_id, tab_id: root.tab_id })
}

/// Runs one of herdr's listings on one side.  Success is on stdout, errors
/// are on stderr, so both are captured; the exit status decides which to read.
///
/// wsl.exe's own failure messages (a missing distro, for instance) come back
/// UTF-16LE unless WSL_UTF8 is set, and `from_utf8_lossy` mangles them without
/// it, so they never parse as an envelope and read as no herdr on that side.
/// herdr's own output is a relayed Linux byte stream and is unaffected either
/// way.
#[allow(clippy::disallowed_methods)] // Running herdr is this function's job.
pub(super) fn list_panes(
    side: &Side,
    listing: Listing,
    attached: bool,
    _blocking: &jobs::Blocking,
) -> Result<ListingReply, PollError> {
    let (program, args) = side.command(PROGRAM, &listing.args());
    let sampled_at = Instant::now();
    let output = command_ext::hidden(program)
        .args(args)
        .env("WSL_UTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|_| PollError::Absent("spawn_failed"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(match error_code(&stderr) {
            Some(code) => PollError::Server(code),
            None => PollError::Absent("herdr_unavailable"),
        });
    }
    Ok(ListingReply::parse(&String::from_utf8_lossy(&output.stdout), listing, sampled_at, attached))
}

pub type HerdrAttachResult = Result<(String, Vec<String>), String>;

/// What a shared-view attach asks herdr before its client can start: focus
/// the pane, since every app client draws whatever herdr has focused, then
/// name the session, since that is what the client attaches to.  Both are
/// process spawns, and on native Windows both wait on herdr starting up,
/// which is why this only ever runs on the pool.
///
/// `cached_name` is what the endpoint learned in the background.  A gesture
/// that beats the first read asks herdr itself: a wait is better than a
/// refusal.
pub fn herdr_attach_gesture(
    side: &Side,
    focus: &[String],
    cached_name: Option<String>,
) -> HerdrAttachResult {
    // Two argv spawns, no shell: the only shell a `Native` command could
    // reach on this side is cmd.exe, which does not understand `sh_quote`'s
    // single-quoting.
    focus_pane(side, focus)?;
    let session = match cached_name {
        Some(session) => session,
        None => running_session_name(side)?,
    };
    Ok(side.command(PROGRAM, &["session", "attach", &session]))
}

#[cfg(test)]
mod tests {
    use super::super::wire::PANES;
    use super::*;

    /// `herdr agent focus` answers `agent_not_found` for a pane with no agent
    /// in it, so the tab is the only handle such a pane has.
    #[test]
    fn a_pane_with_no_agent_is_focused_through_its_tab() {
        let panes = Listing::Panes.parse(PANES);
        assert_eq!(focus_args(&panes[0].target(&Side::Native)), vec!["agent", "focus", "w1:p1"]);
        assert_eq!(focus_args(&panes[1].target(&Side::Native)), vec!["tab", "focus", "w1:t4"]);
    }

    /// A pane herdr detected no agent in has nothing `herdr agent attach`
    /// could resolve, whatever the side and the configured mode allow.
    #[test]
    fn a_pane_with_no_agent_never_attaches_directly() {
        assert!(!attaches_directly(&Side::Wsl("d".into()), AttachMode::Agent, false));
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

    /// The preference can only ever give up a direct attach, never conjure
    /// one on a side that has none.
    #[test]
    fn asking_for_the_session_gives_up_a_direct_attach() {
        let wsl = Side::Wsl("d".into());
        assert!(attaches_directly(&wsl, AttachMode::Agent, true));
        assert!(!attaches_directly(&wsl, AttachMode::Session, true));
        assert!(!attaches_directly(&Side::Native, AttachMode::Session, true));
        assert_eq!(attaches_directly(&Side::Native, AttachMode::Agent, true), !cfg!(windows));
    }

    /// Every herdr app client draws whatever herdr has focused, so a pane
    /// created without `--focus` would be attached to while the window still
    /// showed the pane before it.
    #[test]
    fn a_created_pane_is_focused_and_takes_a_cwd_only_when_one_is_chosen() {
        assert_eq!(create_args(None), vec!["tab", "create", "--focus"]);
        assert_eq!(create_args(Some("/tmp/review")), vec![
            "tab",
            "create",
            "--focus",
            "--cwd",
            "/tmp/review"
        ]);
    }

    #[test]
    fn direct_attach_targets_the_pane_id() {
        assert_eq!(attach_args("w5:p1"), vec!["agent", "attach", "w5:p1"]);
    }
}
