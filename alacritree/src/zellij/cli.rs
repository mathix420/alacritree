//! Running the `zellij` binary.
//!
//! Everything goes through zellij's CLI, which reaches a session's server
//! over its socket, so a missing binary or no running session is a quiet
//! empty listing.  This is the only file that spawns zellij.

use std::fmt;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::listing;
use crate::multiplexer::{CreatedPane, Pane, Side};
use crate::{command_ext, wsl, wsl_helper};

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

/// Why a zellij call brought back nothing to read.
#[derive(Debug)]
pub enum CallError {
    /// No zellij ran on the side, or the one there would not list.
    Absent(String),
    /// zellij did not answer in time, which says nothing about whether it
    /// is there.
    NoAnswer,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent(why) => f.write_str(why),
            Self::NoAnswer => f.write_str("zellij did not answer"),
        }
    }
}

/// Runs `program <args>` on `side` and hands back what it printed.
fn run(program: &str, side: &Side, args: &[&str]) -> Result<Output, CallError> {
    let (program, argv) = side.command(program, args);
    let mut command = command_ext::hidden(program);
    command.args(argv).env("WSL_UTF8", "1");
    run_child(command, CALL_TIMEOUT)
}

/// Runs `command` to completion within `limit`, killing it past that.  The
/// bound comes from this side because `Command::output` has none.  On a WSL
/// side the child is `wsl.exe`, so a zellij already started inside the
/// distro runs on.
#[allow(clippy::disallowed_methods)] // Every caller is a pool job.
fn run_child(mut command: Command, limit: Duration) -> Result<Output, CallError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CallError::Absent(format!("failed to run zellij: {e}")))?;
    let deadline = Instant::now() + limit;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    // One thread per pipe, so a child filling the pipe read second cannot
    // wedge the read of the first.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let (Ok(stdout), Ok(stderr)) =
        (stdout.recv_timeout(remaining()), stderr.recv_timeout(remaining()))
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CallError::NoAnswer);
    };
    let status = child.wait().map_err(|e| CallError::Absent(format!("zellij vanished: {e}")))?;
    Ok(Output { status, stdout, stderr })
}

/// Reads `pipe` to its end on a thread of its own.
fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        let _ = std::thread::Builder::new().name("alacritree-zellij".into()).spawn(move || {
            let mut read = Vec::new();
            let _ = pipe.read_to_end(&mut read);
            let _ = tx.send(read);
        });
    }
    rx
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

/// The running sessions in what `zellij list-sessions` answered.  zellij
/// exits non-zero when it has no session to list, which is an answer; any
/// other failure is a shell that found no zellij.
fn sessions(side: &Side, succeeded: bool, said: &str) -> Result<Vec<String>, CallError> {
    if !succeeded && !said.contains("No active zellij sessions") {
        return Err(CallError::Absent(format!("no zellij on {}: {}", side.name(), said.trim())));
    }
    Ok(listing::live_sessions(said))
}

/// Every running session on `side` and the terminal panes in each.  `Err`
/// means no zellij answered on this side at all.
pub fn list_side(program: &str, side: &Side) -> Result<SideListing, CallError> {
    match side {
        Side::Native => list_natively(program, side),
        Side::Wsl(distro) => list_in_wsl(program, distro, side),
    }
}

fn list_natively(program: &str, side: &Side) -> Result<SideListing, CallError> {
    let sampled_at = Instant::now();
    let output = run(program, side, &["list-sessions", "--no-formatting"])?;
    let said = if output.status.success() { stdout(&output) } else { refusal(&output) };
    let sessions = sessions(side, output.status.success(), &said)?;
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

/// Lists a distro's sessions and the panes of each in one run, so a poll
/// costs one round trip.  `$1` is zellij, looked up through the user's login
/// shell when it is a bare name.  Prints the listing's exit status and what
/// it said, then a section per live session: its name on the first line and
/// its panes after.  Each zellij call is bounded, since the resident helper
/// cannot stop a job it started, and a wedged server would otherwise keep
/// one alive per poll.
const LIST_SCRIPT: &str = r#"z=$1
case $z in */*) ;; *)
  sh=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$sh" ] || sh=${SHELL:-/bin/sh}
  z=$("$sh" -lc "command -v $1 || echo" 2>/dev/null); z=${z:-$1} ;;
esac
zj() { if command -v timeout >/dev/null; then timeout 5 "$z" "$@"; else "$z" "$@"; fi; }
o=$(zj list-sessions --no-formatting 2>&1 </dev/null); printf '%s\n%s' "$?" "$o"
printf '%s\n' "$o" | while IFS= read -r l; do
  case $l in *'(EXITED'*) continue ;; *' [Created '*) s=${l%%' [Created '*} ;; *) continue ;; esac
  [ -n "$s" ] || continue
  printf '\n@@ALACRITREE@@\n%s\n' "$s"
  zj --session "$s" action list-panes --json 2>/dev/null </dev/null
done
"#;

/// The exit status `timeout` gives a command it stopped.
const TIMED_OUT: &str = "124";

/// Runs [`LIST_SCRIPT`] on the distro's resident helper, or as a one-shot
/// `wsl.exe` when the helper is not up.  A bare name the helper's hello
/// found is passed as its path, sparing the script a login shell per poll.
fn list_in_wsl(program: &str, distro: &str, side: &Side) -> Result<SideListing, CallError> {
    let sampled_at = Instant::now();
    let located =
        if program.contains('/') { None } else { wsl_helper::capability(distro, program) };
    let program = located.as_deref().unwrap_or(program);
    let printed = match wsl_helper::try_run(distro, LIST_SCRIPT, &[program]) {
        Some(result) => result.map_err(|_| CallError::NoAnswer)?,
        None => {
            let (wsl, argv) =
                wsl::exec_invocation(distro, &["sh", "-c", LIST_SCRIPT, "sh", program]);
            let mut command = command_ext::hidden(wsl);
            command.args(argv).env("WSL_UTF8", "1");
            run_child(command, CALL_TIMEOUT)?.stdout
        },
    };
    read_listing(side, &printed, sampled_at)
}

/// What [`LIST_SCRIPT`] printed, as a listing.
fn read_listing(
    side: &Side,
    printed: &[u8],
    sampled_at: Instant,
) -> Result<SideListing, CallError> {
    let mut sections = wsl::split_sections(printed).into_iter().map(String::from_utf8_lossy);
    let head = sections.next().unwrap_or_default();
    let (status, said) = head.split_once('\n').unwrap_or((&head, ""));
    let status = status.trim();
    if status.is_empty() || status == TIMED_OUT {
        return Err(CallError::NoAnswer);
    }
    let sessions = sessions(side, status == "0", said)?;
    let mut read = Vec::new();
    let mut panes = Vec::new();
    for section in sections {
        let (session, json) = section.split_once('\n').unwrap_or((&section, ""));
        if let Some(listed) = listing::panes(session, json) {
            read.push(session.to_string());
            panes.extend(listed);
        }
    }
    Ok(SideListing { side: side.clone(), sessions, read, panes, sampled_at })
}

/// Brings `pane_id` to the front of `session`, switching to its tab.  zellij
/// applies a CLI focus to the client that last typed, or to the session's
/// own default view when no client is attached.
pub fn focus_pane(program: &str, side: &Side, session: &str, pane_id: &str) -> Result<(), String> {
    let output = run(program, side, &["--session", session, "action", "focus-pane-id", pane_id])
        .map_err(|e| e.to_string())?;
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
    let output = run(program, side, &borrowed).map_err(|e| e.to_string())?;
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

    /// A shell that writes `marker` once two seconds have passed.
    #[allow(clippy::disallowed_methods)] // A stand-in for a wedged zellij.
    fn late_writer(marker: &std::path::Path) -> Command {
        let marker = marker.display();
        #[cfg(windows)]
        let command = {
            use std::os::windows::process::CommandExt;
            // Raw, since cmd reads the `\"` Rust would escape a quote to as
            // part of the path.
            let mut command = Command::new("cmd");
            command.raw_arg(format!("/C \"ping -n 3 127.0.0.1 >nul & echo late> \"{marker}\"\""));
            command
        };
        #[cfg(not(windows))]
        let command = {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("sleep 2; echo late > '{marker}'")]);
            command
        };
        command
    }

    /// A call given up on is killed, so calls to a stalled side cannot pile
    /// up one per poll.
    #[test]
    fn a_call_that_runs_out_its_time_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late");

        let started = Instant::now();
        let result = run_child(late_writer(&marker), Duration::from_millis(200));

        assert!(result.is_err(), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(1), "the caller waited for the child");
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "the timed-out child ran to completion");
    }

    const SESSIONS: &str = include_str!("fixtures/list-sessions.txt");
    const PANES: &str = include_str!("fixtures/list-panes.json");

    fn wsl() -> Side {
        Side::Wsl("d".into())
    }

    /// What [`LIST_SCRIPT`] prints for the recorded sessions, with the one
    /// live session whose panes were recorded.
    fn printed() -> String {
        format!("0\n{SESSIONS}\n@@ALACRITREE@@\nalacritree-probe\n{PANES}")
    }

    #[test]
    fn a_wsl_listing_reads_every_live_session_and_its_panes() {
        let listing = read_listing(&wsl(), printed().as_bytes(), Instant::now()).unwrap();
        assert_eq!(listing.sessions, ["alacritree-probe", "alacritree-probe2"]);
        assert_eq!(listing.read, ["alacritree-probe"]);
        assert!(!listing.panes.is_empty());
        assert!(listing.panes.iter().all(|pane| pane.terminal_id.starts_with("alacritree-probe/")));
    }

    #[test]
    fn a_distro_with_no_session_running_answers_empty() {
        let printed = "1\nNo active zellij sessions found.";
        let listing = read_listing(&wsl(), printed.as_bytes(), Instant::now()).unwrap();
        assert!(listing.sessions.is_empty());
    }

    #[test]
    fn a_distro_with_no_zellij_says_it_is_absent() {
        let printed = "127\nsh: 1: zellij: not found";
        let listing = read_listing(&wsl(), printed.as_bytes(), Instant::now());
        assert!(matches!(listing, Err(CallError::Absent(_))), "{listing:?}");
    }

    /// A listing zellij never finished, or one that printed nothing, says
    /// nothing about whether zellij is there.
    #[test]
    fn a_listing_cut_short_is_no_answer() {
        for printed in ["124\n", ""] {
            let listing = read_listing(&wsl(), printed.as_bytes(), Instant::now());
            assert!(matches!(listing, Err(CallError::NoAnswer)), "{printed:?}: {listing:?}");
        }
    }

    /// The script and its reader together, against a zellij that answers
    /// with the recorded output.
    #[cfg(unix)]
    #[test]
    #[allow(clippy::disallowed_methods)] // Runs the script the helper runs.
    fn the_list_script_prints_what_the_reader_reads() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sessions"), SESSIONS).unwrap();
        std::fs::write(dir.path().join("panes"), PANES).unwrap();
        let fake = dir.path().join("zellij");
        let d = dir.path().display();
        let body = format!(
            "#!/bin/sh\ncase $1 in list-sessions) cat '{d}/sessions' ;; --session) [ \"$2\" = \
             alacritree-probe ] && cat '{d}/panes' ;; esac\n"
        );
        std::fs::write(&fake, body).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output =
            Command::new("sh").args(["-c", LIST_SCRIPT, "sh"]).arg(&fake).output().unwrap();
        let listing = read_listing(&wsl(), &output.stdout, Instant::now()).unwrap();
        let expected = read_listing(&wsl(), printed().as_bytes(), Instant::now()).unwrap();
        assert_eq!(listing.sessions, expected.sessions);
        assert_eq!(listing.read, expected.read);
        assert_eq!(listing.panes.len(), expected.panes.len());
    }

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
