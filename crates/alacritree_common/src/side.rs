//! Where a program runs for a checkout, and running it there.
//!
//! A checkout under `\\wsl.localhost\<distro>\…` belongs to that distro, so
//! a tool acting on it is the distro's Linux build. The Windows one reads the
//! Windows side's config and knows none of the distro's paths. A program not
//! installed on the checkout's side is not an error, because someone with
//! projects on both sides rarely installs every tool on both.

use std::io;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use crate::jobs::Blocking;
use crate::{command_ext, wsl};

/// Which side of a Windows and WSL installation a checkout or a server lives
/// on. Two servers on one machine cannot see each other, so a multiplexer
/// pane's identity includes its side.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Side {
    Native,
    /// Named distro, as `wsl.exe -d` spells it.
    Wsl(String),
}

impl Side {
    pub fn of(path: &Path) -> Self {
        Self::from_location(&wsl::classify(path))
    }

    pub fn from_location(location: &wsl::Location) -> Self {
        match location {
            wsl::Location::Windows(_) => Side::Native,
            wsl::Location::Wsl { distro, .. } => Side::Wsl(distro.clone()),
        }
    }

    /// How a side is spelled outside the process: `native`, or `wsl:<distro>`
    /// as `wsl.exe -d` names it. A pane named to a client without its side
    /// is not named at all.
    pub fn name(&self) -> String {
        match self {
            Self::Native => "native".to_string(),
            Self::Wsl(distro) => format!("wsl:{distro}"),
        }
    }

    /// Read back what `name` wrote. A `wsl:` with nothing after it names no
    /// server, so it is refused rather than resolving to a distro called the
    /// empty string.
    pub fn parse(name: &str) -> Option<Self> {
        if name == "native" {
            return Some(Self::Native);
        }
        match name.strip_prefix("wsl:") {
            None | Some("") => None,
            Some(distro) => Some(Self::Wsl(distro.to_string())),
        }
    }

    /// How a row names this side. `None` on the native one, whose name would
    /// be the same word on every row of a machine that has only it.
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Native => None,
            Self::Wsl(distro) => Some(format!("wsl:{distro}")),
        }
    }

    /// Program and argv that run `program <args>` on this side. WSL goes
    /// through a login shell because these binaries install to
    /// `~/.local/bin`, which is not on the PATH `wsl.exe -e` inherits.
    pub fn command(&self, program: &str, args: &[&str]) -> (String, Vec<String>) {
        match self {
            Self::Native => (program.to_string(), args.iter().map(|a| (*a).to_string()).collect()),
            Self::Wsl(distro) => {
                let script = std::iter::once(sh_quote(program))
                    .chain(args.iter().map(|a| sh_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                // The login shell supplies PATH; exec preserves the PID
                // recorded by the foreground probe.
                wsl::exec_invocation(distro, &["sh", "-lc", &format!("exec {script}")])
            },
        }
    }
}

/// Single-quote a POSIX argument, since WSL invocations are one `sh -lc`
/// string rather than an argv.
fn sh_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=".contains(c)) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// A path as a program on its own side spells it: unchanged natively, the
/// distro's Linux path inside WSL.
pub fn spelling(location: &wsl::Location) -> String {
    match location {
        wsl::Location::Windows(path) => path.to_string_lossy().into_owned(),
        wsl::Location::Wsl { linux_path, .. } => linux_path.clone(),
    }
}

/// A program as configured for each side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Program {
    /// The name or path to run natively.
    pub native: String,
    /// The path to run inside a distro as written. `None` finds `name`
    /// through the distro user's login shell, which has their PATH.
    pub wsl: Option<String>,
    /// The bare name a distro looks up. Separate from `native`, which may be
    /// a Windows path that means nothing inside the distro.
    pub name: String,
}

/// A command line on the program's own side, and whether it passes through
/// a login shell, whose exit status 127 means the program was not found.
/// Inside a distro this is what follows `wsl.exe -d <distro> --exec`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    program: String,
    args: Vec<String>,
    via_login_shell: bool,
}

/// What running a program came to.
#[derive(Debug)]
pub enum Ran {
    /// The program is not installed on this side.
    Missing,
    Finished(Output),
    /// The program ran past its limit and was killed.
    TimedOut,
}

/// The status a POSIX shell exits with for a command it could not find.
const NOT_FOUND: i32 = 127;

/// Shell code that sets `$s` to the distro user's own login shell, since
/// `wsl.exe --exec` sees only the system PATH.
pub const LOGIN_SHELL: &str = r#"s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$s" ] || s=${SHELL:-/bin/sh}"#;

fn invocation(side: &Side, program: &Program, args: &[String]) -> Invocation {
    let (program, mut argv, via_login_shell) = match (side, &program.wsl) {
        (Side::Native, _) => (program.native.clone(), Vec::new(), false),
        (Side::Wsl(_), Some(path)) => (path.clone(), Vec::new(), false),
        (Side::Wsl(_), None) => {
            let script = format!(r#"{LOGIN_SHELL}; exec "$s" -lc 'exec "$@"' "$s" "$@""#);
            ("sh".to_string(), vec!["-c".into(), script, "sh".into(), program.name.clone()], true)
        },
    };
    argv.extend(args.iter().cloned());
    Invocation { program, args: argv, via_login_shell }
}

/// Run `program` on `side`, killing it if the job is cancelled or it runs
/// past [`LIMIT`]. Blocks, so it takes the pool's token. Call it from a job,
/// never the UI thread.
pub fn run(
    side: &Side,
    program: &Program,
    cwd: Option<&Path>,
    args: &[String],
    blocking: &Blocking,
) -> io::Result<Ran> {
    run_within(side, program, cwd, args, blocking, LIMIT)
}

/// How long a program may run before it is killed. Generous, because a
/// user's hook may install dependencies; bounded, because a worktree removal
/// or first open has no cancel, and a hook that never exits would otherwise
/// hold a pool worker, and a sidebar spinner, until the app quits.
pub const LIMIT: Duration = Duration::from_secs(300);

fn run_within(
    side: &Side,
    program: &Program,
    cwd: Option<&Path>,
    args: &[String],
    blocking: &Blocking,
    limit: Duration,
) -> io::Result<Ran> {
    let inv = invocation(side, program, args);
    // `wsl::command` sets WSL_UTF8, without which wsl.exe's own errors (a
    // stopped or missing distro) arrive as UTF-16LE and read as garbage.
    let mut cmd = match side {
        Side::Native => {
            let mut cmd = command_ext::hidden(&inv.program);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            cmd
        },
        Side::Wsl(distro) => {
            let mut cmd = wsl::command(distro, cwd);
            cmd.arg(&inv.program);
            cmd
        },
    };
    cmd.args(&inv.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    match blocking.run_drained(&mut cmd, limit) {
        Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(Ran::TimedOut),
        Err(e) if e.kind() == io::ErrorKind::NotFound && *side == Side::Native => Ok(Ran::Missing),
        Err(e) => Err(e),
        Ok(output) if inv.via_login_shell && output.status.code() == Some(NOT_FOUND) => {
            Ok(Ran::Missing)
        },
        Ok(output) => Ok(Ran::Finished(output)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn wsl_location(distro: &str, linux: &str) -> wsl::Location {
        wsl::Location::Wsl { distro: distro.into(), linux_path: linux.into() }
    }

    #[test]
    fn a_wsl_location_runs_in_its_distro() {
        assert_eq!(
            Side::from_location(&wsl_location("Ubuntu", "/home/u/wt")),
            Side::Wsl("Ubuntu".into())
        );
        assert_eq!(
            Side::from_location(&wsl::Location::Windows(PathBuf::from("C:/wt"))),
            Side::Native
        );
    }

    /// A side has two spellings, and they are not interchangeable. `label` is
    /// a row's word for it and stays silent on the native side, while a
    /// client that cannot see the row needs the side named every time.
    #[test]
    fn a_side_names_itself_on_both_sides_of_the_wire() {
        assert_eq!(Side::Native.name(), "native");
        assert_eq!(Side::Wsl("Ubuntu-24.04".into()).name(), "wsl:Ubuntu-24.04");
    }

    /// A client names a pane by the side it read out of a listing, so a side
    /// that does not survive the round trip points an attach at the wrong
    /// server or at none.
    #[test]
    fn a_side_reads_back_as_the_side_it_spelled() {
        for side in [Side::Native, Side::Wsl("Ubuntu-24.04".into())] {
            assert_eq!(Side::parse(&side.name()), Some(side));
        }
    }

    #[test]
    fn a_side_that_names_no_server_is_refused() {
        for name in ["", "wsl", "wsl:", "Native", "tmux:0"] {
            assert_eq!(Side::parse(name), None, "{name} named a server");
        }
    }

    #[test]
    fn native_runs_the_program_directly() {
        let (program, args) = Side::Native.command("herdr", &["agent", "list"]);
        assert_eq!(program, "herdr");
        assert_eq!(args, vec!["agent", "list"]);
    }

    /// These binaries install to ~/.local/bin, which reaches PATH only under
    /// a login shell. `wsl.exe -e herdr` fails with execvpe ENOENT.
    #[test]
    fn wsl_wraps_in_a_login_shell() {
        let side = Side::Wsl("kali-linux".into());
        let (program, args) = side.command("herdr", &["agent", "list"]);
        assert_eq!(program, "wsl.exe");
        assert_eq!(args, vec!["-d", "kali-linux", "--exec", "sh", "-lc", "exec herdr agent list"]);
    }

    #[test]
    fn wsl_quotes_arguments_that_need_it() {
        let side = Side::Wsl("d".into());
        let (_, args) = side.command("herdr", &["agent", "attach", "w1:p1"]);
        assert_eq!(args.last().unwrap(), "exec herdr agent attach 'w1:p1'");
    }

    #[test]
    fn a_wsl_location_is_spelled_as_its_linux_path() {
        assert_eq!(spelling(&wsl_location("Ubuntu", "/home/u/my wt")), "/home/u/my wt");
        assert_eq!(spelling(&wsl::Location::Windows(PathBuf::from("/srv/wt"))), "/srv/wt");
    }

    fn program(native: &str, wsl: Option<&str>, name: &str) -> Program {
        Program { native: native.into(), wsl: wsl.map(Into::into), name: name.into() }
    }

    #[test]
    fn a_native_program_runs_as_configured() {
        let inv = invocation(&Side::Native, &program("mise", None, "mise"), &["trust".into()]);
        assert_eq!(inv.program, "mise");
        assert_eq!(inv.args, ["trust"]);
        assert!(!inv.via_login_shell);
    }

    #[test]
    fn a_configured_wsl_path_is_executed_directly() {
        let side = Side::Wsl("Ubuntu".into());
        let inv =
            invocation(&side, &program("mise", Some("/usr/bin/mise"), "mise"), &["trust".into()]);
        assert_eq!(inv.program, "/usr/bin/mise");
        assert_eq!(inv.args, ["trust"]);
        assert!(!inv.via_login_shell);
    }

    #[test]
    fn an_unconfigured_wsl_program_goes_through_the_login_shell() {
        let side = Side::Wsl("Ubuntu".into());
        let inv = invocation(&side, &program("mise", None, "mise"), &[
            "trust".into(),
            "/home/u/wt".into(),
        ]);
        assert_eq!(inv.program, "sh");
        assert_eq!(inv.args[0], "-c");
        assert!(inv.args[1].contains(r#"-lc 'exec "$@"'"#), "{}", inv.args[1]);
        assert_eq!(&inv.args[2..], ["sh", "mise", "trust", "/home/u/wt"]);
        assert!(inv.via_login_shell);
    }

    /// A configured Windows path means nothing inside a distro; handing it to
    /// the login shell would exit 127 and silently skip the hook.
    #[test]
    fn a_native_path_is_not_handed_to_the_distro() {
        let side = Side::Wsl("Ubuntu".into());
        let inv = invocation(&side, &program(r"C:\Tools\doppler.exe", None, "doppler"), &[]);
        assert_eq!(&inv.args[2..], ["sh", "doppler"]);
        assert!(!inv.args.iter().any(|a| a.contains(r"C:\Tools")), "{:?}", inv.args);
    }

    #[test]
    fn a_missing_native_program_is_missing_not_an_error() {
        let program = program("alacritree-no-such-program", None, "alacritree-no-such-program");
        let ran = jobs::on_this_thread(|b| run(&Side::Native, &program, None, &[], b)).unwrap();
        assert!(matches!(ran, Ran::Missing));
    }

    #[cfg(unix)]
    #[test]
    fn a_native_program_finishes_with_its_output() {
        let program = program("sh", None, "sh");
        let args = ["-c".into(), "echo out; echo err >&2; exit 3".into()];
        let ran = jobs::on_this_thread(|b| run(&Side::Native, &program, None, &args, b)).unwrap();
        let Ran::Finished(output) = ran else { panic!("sh is installed") };
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    /// A hook that never exits must not keep a pool worker once the caller
    /// is gone. Closing the worktree dialog cancels the job.
    #[cfg(unix)]
    #[test]
    fn cancelling_the_job_kills_a_hanging_program() {
        let (tx, rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |b| {
            let _ = started_tx.send(());
            let program = program("sleep", None, "sleep");
            let _ = tx.send(run(&Side::Native, &program, None, &["30".into()], b).is_ok());
        });
        started_rx.recv_timeout(Duration::from_secs(5)).expect("the job never started");
        let begun = Instant::now();
        drop(job);
        rx.recv_timeout(Duration::from_secs(10)).expect("run never returned after cancel");
        assert!(begun.elapsed() < Duration::from_secs(10));
    }

    /// A hook as chatty as a dependency install must not stall on a full pipe
    /// and leave the create waiting on it forever.
    #[cfg(unix)]
    #[test]
    fn a_program_that_fills_a_pipe_still_finishes() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let program = program("sh", None, "sh");
            let args =
                ["-c".into(), "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2".into()];
            let ran = jobs::on_this_thread(|b| run(&Side::Native, &program, None, &args, b));
            let _ = tx.send(matches!(ran, Ok(Ran::Finished(ref out)) if out.status.success()));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true), "the program stalled");
    }

    /// A hook that never exits is stopped, not waited on until the app quits.
    #[test]
    fn a_program_past_its_limit_is_timed_out() {
        let program = if cfg!(windows) {
            program("ping", None, "ping")
        } else {
            program("sleep", None, "sleep")
        };
        let args: Vec<String> = if cfg!(windows) {
            vec!["-n".into(), "31".into(), "127.0.0.1".into()]
        } else {
            vec!["30".into()]
        };
        let begun = Instant::now();
        let ran = jobs::on_this_thread(|b| {
            run_within(&Side::Native, &program, None, &args, b, Duration::from_millis(200))
        });
        assert!(matches!(ran, Ok(Ran::TimedOut)), "{ran:?}");
        assert!(begun.elapsed() < Duration::from_secs(10));
    }
}
