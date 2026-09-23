//! Where a program runs for a checkout, and running it there.
//!
//! A checkout under `\\wsl.localhost\<distro>\…` belongs to that distro, so
//! a tool acting on it is the distro's Linux build: the Windows one reads the
//! Windows side's config and knows none of the distro's paths.  A program not
//! installed on the checkout's side is not an error, because someone with
//! projects on both sides rarely installs every tool on both.

use std::io;
use std::path::Path;
use std::process::{Output, Stdio};

use crate::jobs::Blocking;
use crate::{command_ext, wsl};

/// Which side of a Windows and WSL installation a checkout lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    Native,
    Wsl { distro: String },
}

impl Side {
    pub fn of(path: &Path) -> Self {
        Self::from_location(&wsl::classify(path))
    }

    pub fn from_location(location: &wsl::Location) -> Self {
        match location {
            wsl::Location::Windows(_) => Side::Native,
            wsl::Location::Wsl { distro, .. } => Side::Wsl { distro: distro.clone() },
        }
    }
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
    /// The path to run inside a distro as written.  `None` finds `name`
    /// through the distro user's login shell, which has their PATH.
    pub wsl: Option<String>,
    /// The bare name a distro looks up.  Separate from `native`, which may be
    /// a Windows path that means nothing inside the distro.
    pub name: String,
}

/// A command line on the program's own side, and whether it passes through
/// a login shell, whose exit status 127 means the program was not found.
/// Inside a distro this is what follows `wsl.exe -d <distro> --exec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub via_login_shell: bool,
}

/// What running a program came to.
#[derive(Debug)]
pub enum Ran {
    /// The program is not installed on this side.
    Missing,
    Finished(Output),
}

/// The status a POSIX shell exits with for a command it could not find.
const NOT_FOUND: i32 = 127;

/// The distro user's own login shell, resolved the way custom diff viewers
/// resolve it, since `wsl.exe --exec` sees only the system PATH.
const LOGIN_SHELL: &str = r#"s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$s" ] || s=${SHELL:-/bin/sh}"#;

pub fn invocation(side: &Side, program: &Program, args: &[String]) -> Invocation {
    let (program, mut argv, via_login_shell) = match (side, &program.wsl) {
        (Side::Native, _) => (program.native.clone(), Vec::new(), false),
        (Side::Wsl { .. }, Some(path)) => (path.clone(), Vec::new(), false),
        (Side::Wsl { .. }, None) => {
            let script = format!(r#"{LOGIN_SHELL}; exec "$s" -lc 'exec "$@"' "$s" "$@""#);
            ("sh".to_string(), vec!["-c".into(), script, "sh".into(), program.name.clone()], true)
        },
    };
    argv.extend(args.iter().cloned());
    Invocation { program, args: argv, via_login_shell }
}

/// Run `program` on `side`, killing it if the job is cancelled.  Blocks, so
/// it takes the pool's token: call it from a job, never the UI thread.
pub fn run(
    side: &Side,
    program: &Program,
    cwd: Option<&Path>,
    args: &[String],
    blocking: &Blocking,
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
        Side::Wsl { distro } => {
            let mut cmd = wsl::command(distro, cwd);
            cmd.arg(&inv.program);
            cmd
        },
    };
    cmd.args(&inv.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    match blocking.run_cancellable(&mut cmd) {
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
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn wsl_location(distro: &str, linux: &str) -> wsl::Location {
        wsl::Location::Wsl { distro: distro.into(), linux_path: linux.into() }
    }

    #[test]
    fn a_wsl_location_runs_in_its_distro() {
        assert_eq!(Side::from_location(&wsl_location("Ubuntu", "/home/u/wt")), Side::Wsl {
            distro: "Ubuntu".into()
        });
        assert_eq!(
            Side::from_location(&wsl::Location::Windows(PathBuf::from("C:/wt"))),
            Side::Native
        );
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
        let side = Side::Wsl { distro: "Ubuntu".into() };
        let inv =
            invocation(&side, &program("mise", Some("/usr/bin/mise"), "mise"), &["trust".into()]);
        assert_eq!(inv.program, "/usr/bin/mise");
        assert_eq!(inv.args, ["trust"]);
        assert!(!inv.via_login_shell);
    }

    #[test]
    fn an_unconfigured_wsl_program_goes_through_the_login_shell() {
        let side = Side::Wsl { distro: "Ubuntu".into() };
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
        let side = Side::Wsl { distro: "Ubuntu".into() };
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
    /// is gone: the worktree dialog closing cancels the job.
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
}
