//! Running the `herdr` binary.
//!
//! A missing binary or an absent server is a silent no-op.  This is the only
//! file that builds a herdr command line, the event stream's bridge included.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::AttachMode;
use crate::multiplexer::{CreatedPane, Side};
use crate::tools::{self, Tool};
use crate::{command_ext, jobs};

use super::wire::{CreatedTab, SessionList};
use super::{Listing, ListingReply, PollError, error_code};

/// The herdr binary every call on `side` runs. Each side resolves its own
/// configured path and WSL uses a login shell for bare names.
pub(super) fn program(side: &Side) -> String {
    match side {
        Side::Native => tools::program(Tool::Herdr),
        Side::Wsl(_) => tools::wsl_program(Tool::Herdr),
    }
}

/// The long-lived relay an event stream reads through.  herdr resolves the
/// socket itself, so the stream reaches the same server every other call
/// here does.
pub(super) fn bridge_command(side: &Side) -> (String, Vec<String>) {
    side.command(&program(side), &["remote-api-bridge"])
}

/// Direct attach to one agent.  Unsupported on native Windows, where
/// `run_terminal_attach` is a `#[cfg(windows)]` refusal.
pub(super) fn attach_args(pane_id: &str) -> Vec<String> {
    vec!["agent".into(), "attach".into(), pane_id.into()]
}

/// Whether direct per-agent attach works on this side.  herdr's
/// `run_terminal_attach` is a `#[cfg(windows)]` refusal, so a native Windows
/// server falls back to focusing the pane and attaching the whole session.
pub(super) fn can_attach(side: &Side) -> bool {
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
pub(super) fn attaches_directly(side: &Side, mode: AttachMode, has_agent: bool) -> bool {
    has_agent && mode == AttachMode::Agent && can_attach(side)
}

/// How long the attach gesture waits for herdr before calling it a refusal.
/// These two calls run on the UI thread, and herdr answers a socket on the
/// same machine in milliseconds; three seconds covers a cold process start
/// behind an on-access scanner and still keeps a wedged server from taking
/// the window with it.
const GESTURE_TIMEOUT: Duration = Duration::from_secs(3);

/// How every gesture that ran out [`GESTURE_TIMEOUT`] begins its refusal, so
/// a herdr that went silent can be told from one that said no.
pub(super) const NO_ANSWER: &str = "herdr did not answer";

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

/// The socket request that brings one pane to the front of the user's own
/// herdr window.  The CLI has no way to name a pane with no agent in it:
/// `agent focus` refuses one, and `tab focus` lands on whichever pane of the
/// tab herdr last focused.
fn focus_request(pane_id: &str) -> String {
    let request = json!({
        "id": "alacritree:focus",
        "method": "pane.focus",
        "params": { "pane_id": pane_id },
    });
    format!("{request}\n")
}

/// herdr's answer to a one-shot request, or the message it refused with.
fn decode_answer(line: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Answer {
        result: Option<Value>,
        error: Option<Refusal>,
    }
    #[derive(Deserialize)]
    struct Refusal {
        message: String,
    }
    match serde_json::from_str::<Answer>(line) {
        Ok(Answer { error: Some(refusal), .. }) => Err(refusal.message),
        Ok(Answer { result: Some(_), .. }) => Ok(()),
        _ => Err(line.trim().to_string()),
    }
}

/// Focuses one pane in the user's own herdr window, the first half of the
/// native-Windows attach fallback.  A refusal carries herdr's message rather
/// than its code, since it is shown to the user, and a server that does not
/// answer inside [`GESTURE_TIMEOUT`] refuses the same way.
pub(super) fn focus_pane(side: &Side, pane_id: &str) -> Result<(), String> {
    let request = focus_request(pane_id);
    let (program, args) = bridge_command(side);
    let Some(answer) = bounded(move || ask(program, args, &request)) else {
        return Err(format!("{NO_ANSWER} while focusing the pane"));
    };
    let answer = answer.map_err(|e| format!("failed to focus herdr pane: {e}"))?;
    decode_answer(&answer).map_err(|message| format!("herdr refused to focus the pane: {message}"))
}

/// Sends one request down a fresh bridge and reads herdr's one-line answer.
/// stdin stays open until the answer is in, since herdr's Windows bridge
/// stops relaying at EOF.  A bridge that answers nothing, because no server
/// is running, leaves its reason on stderr.
fn ask(program: String, args: Vec<String>, request: &str) -> io::Result<String> {
    #[allow(clippy::disallowed_methods)] // Running herdr is this function's job.
    let mut child = command_ext::hidden(program)
        .args(args)
        .env("WSL_UTF8", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(io::Error::other("herdr's bridge started without its pipes"));
    };
    let mut answer = String::new();
    let _ = stdin.write_all(request.as_bytes()).and_then(|()| stdin.flush());
    let read = BufReader::new(stdout).read_line(&mut answer);
    let _ = child.kill();
    let _ = child.wait();
    read?;
    if answer.is_empty() {
        stderr.read_to_string(&mut answer)?;
        return Err(io::Error::other(answer.trim().to_string()));
    }
    Ok(answer)
}

/// The running session to attach to on this side.  `herdr session list
/// --json` is a flat object rather than the `result`-wrapped envelope
/// `agent list` uses.  An answer that names nothing falls back to `default`,
/// the name herdr gives an unnamed session; a server that does not answer
/// inside [`GESTURE_TIMEOUT`] is an `Err`, because attaching to a guessed
/// name would only park the wedged wait inside the new session.
pub(super) fn running_session_name(side: &Side) -> Result<String, String> {
    let (program, args) = side.command(&program(side), &["session", "list", "--json"]);
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
        return Err(format!("{NO_ANSWER} while listing its sessions"));
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

/// The `herdr` subcommand that opens a tab, bringing it to the front when
/// `focus` is set so a shared view attaching afterwards is already showing
/// the pane it made. `--cwd` is left off entirely when no directory is
/// chosen, since herdr's own default is a better answer than an empty path.
fn create_args(cwd: Option<&str>, focus: bool) -> Vec<String> {
    let focus = if focus { "--focus" } else { "--no-focus" };
    let mut args = vec!["tab".into(), "create".into(), focus.into()];
    if let Some(cwd) = cwd {
        args.push("--cwd".into());
        args.push(cwd.into());
    }
    args
}

/// Opens a tab in the user's own herdr window, focusing it when `focus` is
/// set. A process spawn that waits on herdr starting up, so it only ever
/// runs on the pool.
///
/// `cwd` is spelled in the side's own terms: a Windows path on the native
/// side, and a path inside the distro on a WSL one, since herdr resolves it
/// where it runs.
pub(super) fn create_pane(
    side: &Side,
    cwd: Option<String>,
    focus: bool,
) -> Result<CreatedPane, String> {
    let create = create_args(cwd.as_deref(), focus);
    let borrowed: Vec<&str> = create.iter().map(String::as_str).collect();
    let (program, args) = side.command(&program(side), &borrowed);
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
        return Err(format!("{NO_ANSWER} while creating the pane"));
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
    let (program, args) = side.command(&program(side), &listing.args());
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

pub(super) type HerdrAttachResult = Result<(String, Vec<String>), String>;

/// What a shared-view attach asks herdr before its client can start: focus
/// the pane, since every app client draws whatever herdr has focused, then
/// name the session, since that is what the client attaches to. A `None` focus
/// leaves herdr where it is. Both are process spawns, and on native Windows
/// both wait on herdr starting up, which is why this only runs on the pool.
///
/// `cached_name` is what the endpoint learned in the background.  A gesture
/// that beats the first read asks herdr itself: a wait is better than a
/// refusal.
pub(super) fn herdr_attach_gesture(
    side: &Side,
    focus: Option<&str>,
    cached_name: Option<String>,
) -> HerdrAttachResult {
    // Two argv spawns, no shell: the only shell a `Native` command could
    // reach on this side is cmd.exe, which does not understand `sh_quote`'s
    // single-quoting.
    if let Some(pane_id) = focus {
        focus_pane(side, pane_id)?;
    }
    let session = match cached_name {
        Some(session) => session,
        None => running_session_name(side)?,
    };
    Ok(side.command(&program(side), &["session", "attach", &session]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_focus_names_the_pane_to_the_socket() {
        let request: Value = serde_json::from_str(&focus_request("w1:p4")).unwrap();
        assert_eq!(request["method"], "pane.focus");
        assert_eq!(request["params"]["pane_id"], "w1:p4");
    }

    /// Answers captured from herdr 0.9.1's socket.
    #[test]
    fn a_focus_answer_is_a_pane_or_herdr_s_refusal() {
        let focused = r#"{"id":"alacritree:focus","result":{"type":"pane_info","pane":{"pane_id":"w11:p7","focused":true}}}"#;
        assert_eq!(decode_answer(focused), Ok(()));
        let refused = r#"{"id":"alacritree:focus","error":{"code":"pane_not_found","message":"pane w99:p9 not found"}}"#;
        assert_eq!(decode_answer(refused), Err("pane w99:p9 not found".to_string()));
        assert_eq!(decode_answer("garbage\n"), Err("garbage".to_string()));
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
        assert_eq!(create_args(None, true), vec!["tab", "create", "--focus"]);
        assert_eq!(create_args(Some("/tmp/review"), true), vec![
            "tab",
            "create",
            "--focus",
            "--cwd",
            "/tmp/review"
        ]);
    }

    /// herdr focuses a new tab unless told otherwise, so leaving the user's
    /// window alone has to be said out loud.
    #[test]
    fn a_pane_created_without_focus_says_so() {
        assert_eq!(create_args(None, false), vec!["tab", "create", "--no-focus"]);
    }

    #[test]
    fn direct_attach_targets_the_pane_id() {
        assert_eq!(attach_args("w5:p1"), vec!["agent", "attach", "w5:p1"]);
    }
}
