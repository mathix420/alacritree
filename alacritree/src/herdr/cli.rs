//! Running the `herdr` binary.
//!
//! Everything goes through the CLI rather than herdr's socket, so a missing
//! binary or an absent server is a silent no-op and no wire protocol is
//! pinned.  This is the only file that knows how to reach a herdr server; a
//! second multiplexer would need its own equivalent and nothing else.

use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::config::AttachMode;
use crate::{command_ext, jobs, wsl};

use super::wire::SessionList;
use super::{error_code, Agent, Listing, ListingReply, PollError, Side};

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

    /// How a row names this side.  `None` on the native one, whose name would
    /// be the same word on every row of a machine that has only it.
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Native => None,
            Self::Wsl(distro) => Some(format!("wsl:{distro}")),
        }
    }
}

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

/// The `herdr` subcommand that brings `agent`'s pane to the front of the
/// user's own herdr window.  `agent focus` resolves its target through the
/// agent registry and answers `agent_not_found` for a pane with no agent in
/// it, so such a pane is reached by focusing the tab that holds it.
pub fn focus_args(agent: &Agent) -> Vec<String> {
    match (agent.status, &agent.tab_id) {
        (None, Some(tab_id)) => vec!["tab".into(), "focus".into(), tab_id.clone()],
        _ => focus_pane_args(&agent.pane_id),
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
    let (program, args) = side.command(&borrowed);
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
    let (program, args) = side.command(&["session", "list", "--json"]);
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
    let (program, args) = side.command(&listing.args());
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `herdr agent focus` answers `agent_not_found` for a pane with no agent
    /// in it, so the tab is the only handle such a pane has.
    #[test]
    fn a_pane_with_no_agent_is_focused_through_its_tab() {
        let panes = Listing::Panes.parse(PANES);
        assert_eq!(focus_args(&panes[0]), vec!["agent", "focus", "w1:p1"]);
        assert_eq!(focus_args(&panes[1]), vec!["tab", "focus", "w1:t4"]);
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
}
