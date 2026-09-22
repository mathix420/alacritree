//! Running the `herdr` binary.
//!
//! A missing binary or an absent server is a silent no-op.  This is the only
//! file that builds a herdr command line, the event stream's bridge included.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::AttachMode;
use crate::multiplexer::{CreatedPane, Side};
use crate::tools::{self, Tool};
use crate::wsl_helper::{self, TransportError};
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

/// A call that takes longer than this is logged at info, so a stall shows in
/// a persistent log without every listing writing a line.
const SLOW_CALL: Duration = Duration::from_secs(1);

/// How long a failed call's stderr is waited for once the child is gone.
const STDERR_GRACE: Duration = Duration::from_millis(500);

/// Runs `f` on a worker thread and gives up on it after [`GESTURE_TIMEOUT`].
/// `Command::output` has no timeout of its own, so the bound comes from this
/// side, as the IPC client's does.  A call that times out leaves its thread
/// parked until the child exits, so it suits only a read whose late answer
/// changes nothing.
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

/// What one herdr call printed.  A request sent down the bridge has answered
/// once a line comes back, since the bridge is killed rather than left to
/// exit.
struct Reply {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

#[derive(Debug)]
enum CallError {
    /// Nothing came back inside the call's limit.
    NoAnswer,
    Failed(String),
}

/// How a call reached its side, named in the line that times it.
#[derive(Clone, Copy, Debug)]
enum Transport {
    Helper,
    Spawn,
}

/// Refuses a job the helper reached after its caller stopped waiting.  `$1`
/// is that moment in Unix seconds, or `-` for a caller that waits forever.
/// The helper cannot be told to drop a job it has queued, so this check is
/// what keeps a late focus move or a duplicate pane from landing.
const HELPER_DEADLINE: &str =
    r#"d=$1; shift; [ "$d" = - ] || [ "$(date +%s)" -le "$d" ] || exit 124; "#;

/// Prints what a command wrote to stdout, and on failure what it wrote to
/// stderr after it, with the command's own exit status.  The helper keeps
/// only a script's stdout.
const HELPER_COMMAND: &str =
    r#"{ e=$("$@" 2>&1 1>&3); r=$?; } 3>&1; [ "$r" -eq 0 ] || printf '%s' "$e"; exit "$r""#;

/// Writes the request in `$1` to the command after it and prints the first
/// line back.  herdr's Linux bridge answers after its stdin closes.  What it
/// printed on stderr, a connect failure for one, stands in only when no
/// answer came, so a warning cannot pass for the answer.
const HELPER_REQUEST: &str = r#"r=$1; shift; f=$(mktemp) || exit 1; o=$(printf '%s\n' "$r" | "$@" 2>"$f" | head -n 1); if [ -n "$o" ]; then printf '%s\n' "$o"; else head -c 1000 "$f"; fi; rm -f "$f""#;

/// Runs `herdr <args>` on `side`, writing `request` to its stdin as one line
/// when given.  A WSL side goes through the distro's resident helper while it
/// is up, which skips the `wsl.exe` launch and login shell a one-shot pays;
/// under load that launch has stalled for longer than any gesture waits.
/// `limit` bounds the wait.  A helper job reached after it is refused, and a
/// one-shot that runs out is killed.
fn call(
    side: &Side,
    args: &[&str],
    request: Option<&str>,
    limit: Option<Duration>,
    blocking: &jobs::Blocking,
) -> Result<Reply, CallError> {
    let started = Instant::now();
    let helped = match side {
        Side::Wsl(distro) => via_helper(distro, args, request, limit, blocking),
        Side::Native => None,
    };
    let (transport, result) = match helped {
        Some(result) => (Transport::Helper, result),
        None => (Transport::Spawn, spawned(side, args, request, limit)),
    };
    let elapsed = started.elapsed();
    let outcome = match &result {
        Ok(reply) if reply.ok => "answered",
        Ok(_) => "failed",
        Err(CallError::NoAnswer) => "got no answer",
        Err(CallError::Failed(_)) => "could not run",
    };
    let level =
        if elapsed >= SLOW_CALL || result.is_err() { log::Level::Info } else { log::Level::Debug };
    log::log!(
        level,
        "herdr ({side:?}): `{}` over {transport:?} {outcome} after {elapsed:.1?}",
        args.join(" ")
    );
    result
}

/// `None` when the helper is not up, or cannot say where herdr is without a
/// login shell, so the caller spawns one-shot instead.  A request the helper
/// may already have run is never retried.
fn via_helper(
    distro: &str,
    args: &[&str],
    request: Option<&str>,
    limit: Option<Duration>,
    blocking: &jobs::Blocking,
) -> Option<Result<Reply, CallError>> {
    let client = wsl_helper::client(distro)?;
    let program = tools::wsl_located(Tool::Herdr, distro, blocking)?;
    let body = if request.is_some() { HELPER_REQUEST } else { HELPER_COMMAND };
    let script = format!("{HELPER_DEADLINE}{body}");
    // Rounded up, so a job the helper reaches in time is never refused.
    let deadline = limit.map_or_else(
        || "-".to_string(),
        |limit| {
            let at = SystemTime::now() + limit;
            (at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() + 1).to_string()
        },
    );
    let argv: Vec<String> = std::iter::once(deadline)
        .chain(request.map(|request| request.trim_end().to_string()))
        .chain(std::iter::once(program))
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("alacritree-herdr-helper".into())
        .spawn(move || {
            let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
            let _ = tx.send(client.run(&script, &borrowed));
        })
        .ok()?;
    let answer = match limit {
        Some(limit) => rx.recv_timeout(limit).ok(),
        None => rx.recv().ok(),
    };
    Some(match answer {
        None => Err(CallError::NoAnswer),
        Some(Err(TransportError::NotWritten(_))) => return None,
        Some(Err(TransportError::NoReply(e))) => Err(CallError::Failed(e)),
        Some(Ok((exit, payload))) => Ok(match request {
            Some(_) => Reply { ok: !payload.is_empty(), stdout: payload, stderr: String::new() },
            None if exit == 0 => Reply { ok: true, stdout: payload, stderr: String::new() },
            None => Reply {
                ok: false,
                stderr: String::from_utf8_lossy(&payload).into_owned(),
                stdout: Vec::new(),
            },
        }),
    })
}

fn spawned(
    side: &Side,
    args: &[&str],
    request: Option<&str>,
    limit: Option<Duration>,
) -> Result<Reply, CallError> {
    let (program, argv) = side.command(&program(side), args);
    let mut command = command_ext::hidden(program);
    command.args(argv).env("WSL_UTF8", "1");
    run_child(command, request, limit)
}

/// Runs `command` to completion, or, given a `request`, until it answers one
/// line.  stdin stays open until then, since herdr's Windows bridge stops
/// relaying at EOF.  A child that runs out `limit` is killed.  On a WSL side
/// that is `wsl.exe`: a herdr already started inside the distro runs on.
#[allow(clippy::disallowed_methods)] // Every caller is a pool job.
fn run_child(
    mut command: Command,
    request: Option<&str>,
    limit: Option<Duration>,
) -> Result<Reply, CallError> {
    let failed = |e: io::Error| CallError::Failed(e.to_string());
    let stdin = if request.is_some() { Stdio::piped() } else { Stdio::null() };
    let mut child = command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(failed)?;
    let mut stdin = child.stdin.take();
    if let (Some(stdin), Some(request)) = (&mut stdin, request) {
        let _ = stdin.write_all(request.as_bytes()).and_then(|()| stdin.flush());
    }
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CallError::Failed("herdr started without its pipes".into()));
    };
    let one_line = request.is_some();
    let (tx, rx) = mpsc::channel();
    let reader =
        std::thread::Builder::new().name("alacritree-herdr-call".into()).spawn(move || {
            let mut out = Vec::new();
            let mut reader = BufReader::new(stdout);
            let _ = if one_line {
                reader.read_until(b'\n', &mut out).map(drop)
            } else {
                reader.read_to_end(&mut out).map(drop)
            };
            let _ = tx.send(out);
        });
    // Its own thread, so a child that fills stderr while stdout is drained
    // cannot wedge the read.
    let (errors_tx, errors) = mpsc::channel();
    let _ = std::thread::Builder::new().name("alacritree-herdr-stderr".into()).spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        let _ = errors_tx.send(text);
    });
    let answer = match (reader, limit) {
        (Err(e), _) => Err(failed(e)),
        (Ok(_), Some(limit)) => rx.recv_timeout(limit).map_err(|_| CallError::NoAnswer),
        (Ok(_), None) => rx.recv().map_err(|_| CallError::NoAnswer),
    };
    if one_line || answer.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    drop(stdin);
    let stdout = answer?;
    let ok = if one_line { !stdout.is_empty() } else { status.is_ok_and(|s| s.success()) };
    // A process the child started can hold stderr open past the kill, so
    // the reason a call failed is waited for only briefly.
    let stderr =
        if ok { String::new() } else { errors.recv_timeout(STDERR_GRACE).unwrap_or_default() };
    Ok(Reply { ok, stdout, stderr })
}

/// Focuses one pane in the user's own herdr window, the first half of the
/// native-Windows attach fallback.  A refusal carries herdr's message rather
/// than its code, since it is shown to the user, and a server that does not
/// answer inside [`GESTURE_TIMEOUT`] refuses the same way.
pub(super) fn focus_pane(
    side: &Side,
    pane_id: &str,
    blocking: &jobs::Blocking,
) -> Result<(), String> {
    let request = focus_request(pane_id);
    let reply = call(side, &["remote-api-bridge"], Some(&request), Some(GESTURE_TIMEOUT), blocking)
        .map_err(|e| match e {
            CallError::NoAnswer => format!("{NO_ANSWER} while focusing the pane"),
            CallError::Failed(e) => format!("failed to focus herdr pane: {e}"),
        })?;
    if !reply.ok {
        return Err(format!("failed to focus herdr pane: {}", reply.stderr.trim()));
    }
    decode_answer(&String::from_utf8_lossy(&reply.stdout))
        .map_err(|message| format!("herdr refused to focus the pane: {message}"))
}

/// The running session to attach to on this side.  `herdr session list
/// --json` is a flat object rather than the `result`-wrapped envelope
/// `agent list` uses.  An answer that names nothing falls back to `default`,
/// the name herdr gives an unnamed session; a server that does not answer
/// inside [`GESTURE_TIMEOUT`] is an `Err`, because attaching to a guessed
/// name would only park the wedged wait inside the new session.
pub(super) fn running_session_name(
    side: &Side,
    blocking: &jobs::Blocking,
) -> Result<String, String> {
    let fallback = || "default".to_string();
    let reply =
        match call(side, &["session", "list", "--json"], None, Some(GESTURE_TIMEOUT), blocking) {
            Err(CallError::NoAnswer) => {
                return Err(format!("{NO_ANSWER} while listing its sessions"));
            },
            Err(CallError::Failed(_)) => return Ok(fallback()),
            Ok(reply) if !reply.ok => return Ok(fallback()),
            Ok(reply) => reply,
        };
    Ok(serde_json::from_slice::<SessionList>(&reply.stdout)
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
    blocking: &jobs::Blocking,
) -> Result<CreatedPane, String> {
    let create = create_args(cwd.as_deref(), focus);
    let borrowed: Vec<&str> = create.iter().map(String::as_str).collect();
    let reply =
        call(side, &borrowed, None, Some(GESTURE_TIMEOUT), blocking).map_err(|e| match e {
            CallError::NoAnswer => format!("{NO_ANSWER} while creating the pane"),
            CallError::Failed(e) => format!("failed to create a herdr pane: {e}"),
        })?;
    if !reply.ok {
        return Err(format!("herdr refused to create the pane: {}", reply.stderr));
    }
    let created = serde_json::from_slice::<CreatedTab>(&reply.stdout)
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
pub(super) fn list_panes(
    side: &Side,
    listing: Listing,
    attached: bool,
    blocking: &jobs::Blocking,
) -> Result<ListingReply, PollError> {
    let sampled_at = Instant::now();
    let reply = call(side, &listing.args(), None, None, blocking)
        .map_err(|_| PollError::Absent("spawn_failed"))?;
    if !reply.ok {
        return Err(match error_code(&reply.stderr) {
            Some(code) => PollError::Server(code),
            None => PollError::Absent("herdr_unavailable"),
        });
    }
    Ok(ListingReply::parse(&String::from_utf8_lossy(&reply.stdout), listing, sampled_at, attached))
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
    blocking: &jobs::Blocking,
) -> HerdrAttachResult {
    // Two argv spawns, no shell: the only shell a `Native` command could
    // reach on this side is cmd.exe, which does not understand `sh_quote`'s
    // single-quoting.
    if let Some(pane_id) = focus {
        focus_pane(side, pane_id, blocking)?;
    }
    let session = match cached_name {
        Some(session) => session,
        None => running_session_name(side, blocking)?,
    };
    Ok(side.command(&program(side), &["session", "attach", &session]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell that writes `marker` once two seconds have passed.
    #[allow(clippy::disallowed_methods)] // A stand-in for a slow herdr.
    fn late_writer(marker: &std::path::Path) -> Command {
        let marker = marker.display();
        if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", &format!("ping -n 3 127.0.0.1 >nul & echo late> \"{marker}\"")]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("sleep 2; echo late > '{marker}'")]);
            command
        }
    }

    /// A gesture that ran out its time must not land afterwards: a focus
    /// move reaching herdr seconds late takes the user's window somewhere
    /// they already left.
    #[test]
    fn a_call_that_runs_out_its_time_never_lands() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late");

        let started = Instant::now();
        let result = run_child(late_writer(&marker), None, Some(Duration::from_millis(200)));

        assert!(matches!(result, Err(CallError::NoAnswer)), "{:?}", result.map(|r| r.ok));
        assert!(started.elapsed() < Duration::from_secs(1), "the caller waited for the child");
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "the timed-out child ran to completion");
    }

    /// The helper returns only a script's stdout and exit status, so a
    /// failing command has to carry its stderr there, and a request has to
    /// come back as the bridge's one line.
    #[cfg(unix)]
    #[test]
    #[allow(clippy::disallowed_methods)] // Runs the scripts the helper runs.
    fn helper_scripts_carry_what_the_caller_reads() {
        let run = |body: &str, args: &[&str]| {
            let script = format!("{HELPER_DEADLINE}{body}");
            let output = Command::new("sh").arg("-c").arg(script).arg("sh").args(args).output();
            let output = output.unwrap();
            (output.status.code(), String::from_utf8_lossy(&output.stdout).into_owned())
        };
        assert_eq!(run(HELPER_COMMAND, &["-", "sh", "-c", "echo out"]), (Some(0), "out\n".into()));
        assert_eq!(
            run(HELPER_COMMAND, &["-", "sh", "-c", "echo refused >&2; exit 3"]),
            (Some(3), "refused".into())
        );
        let warns_then_answers = ["-", "{\"id\":1}", "sh", "-c", "echo warning >&2; cat"];
        assert_eq!(run(HELPER_REQUEST, &warns_then_answers), (Some(0), "{\"id\":1}\n".into()));
        let unreachable = ["-", "{}", "sh", "-c", "echo failed to connect >&2"];
        assert_eq!(run(HELPER_REQUEST, &unreachable), (Some(0), "failed to connect\n".into()));
        assert_eq!(run(HELPER_COMMAND, &["1", "sh", "-c", "echo late"]), (Some(124), "".into()));
    }

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
