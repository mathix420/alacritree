//! The zellij CLI calls alacritree makes, against a throwaway session.
//!
//! Opt-in, since they need a zellij binary: natively, or inside a running
//! WSL distro.  A machine with neither passes without checking anything.
//! Run them with `cargo nextest run -p alacritree --test zellij_e2e
//! --run-ignored ignored-only`.

// A test has no UI thread for a blocking wait to stall.
#![allow(clippy::disallowed_methods)]

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use alacritree::multiplexer::Side;
use alacritree_zellij::{self as zellij, SideListing};

/// How long a check waits for zellij to report a change it just made.
const SETTLE: Duration = Duration::from_secs(10);

fn run(side: &Side, program: &str, args: &[&str]) -> Output {
    let (program, argv) = side.command(program, args);
    Command::new(program)
        .args(argv)
        .env("WSL_UTF8", "1")
        .stdin(Stdio::null())
        .output()
        .expect("the zellij side runs")
}

/// A side with zellij on it, and the program that reaches it there.  A
/// distro's own login shell finds a zellij the `sh` that alacritree's WSL
/// calls use may not, so the path it finds is used as written.
fn zellij_side() -> Option<(Side, String)> {
    let native = Command::new("zellij").arg("--version").output();
    if native.is_ok_and(|output| output.status.success()) {
        return Some((Side::Native, "zellij".to_string()));
    }
    if !cfg!(windows) {
        return None;
    }
    let running = Command::new("wsl.exe").args(["--list", "--running", "--quiet"]).output().ok()?;
    // wsl.exe prints UTF-16LE here whatever `WSL_UTF8` says.
    let units: Vec<u16> =
        running.stdout.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    String::from_utf16_lossy(&units).lines().map(str::trim).filter(|d| !d.is_empty()).find_map(
        |distro| {
            let found = Command::new("wsl.exe")
                .args(["-d", distro, "--exec", "bash", "-lc", "command -v zellij"])
                .output()
                .ok()?;
            let path = String::from_utf8_lossy(&found.stdout).trim().to_string();
            (found.status.success() && !path.is_empty())
                .then(|| (Side::Wsl(distro.to_string()), path))
        },
    )
}

/// A background session that is killed and forgotten however the test ends.
struct Session {
    side: Side,
    program: String,
    name: String,
}

impl Session {
    fn start(side: Side, program: String) -> Self {
        let name = format!("alacritree-e2e-{}", std::process::id());
        let session = Self { side, program, name };
        let started =
            run(&session.side, &session.program, &["attach", "--create-background", &session.name]);
        assert!(started.status.success(), "zellij started no session: {started:?}");
        session
    }

    fn list(&self) -> SideListing {
        zellij::list_side(&self.program, &self.side).expect("zellij answers on its side")
    }

    /// The listing once `done` holds for it, or the last one when it never
    /// does within [`SETTLE`].
    fn list_until(&self, done: impl Fn(&SideListing) -> bool) -> SideListing {
        let deadline = Instant::now() + SETTLE;
        loop {
            let listing = self.list();
            if done(&listing) || Instant::now() > deadline {
                return listing;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        run(&self.side, &self.program, &["kill-session", &self.name]);
        run(&self.side, &self.program, &["delete-session", "--force", &self.name]);
    }
}

#[test]
#[ignore = "needs a zellij binary; run with --run-ignored ignored-only"]
fn zellij_lists_creates_focuses_and_loses_a_pane() {
    let Some((side, program)) = zellij_side() else {
        eprintln!("no zellij natively or in a running WSL distro; nothing to check");
        return;
    };
    let session = Session::start(side.clone(), program.clone());

    let listing = session.list_until(|listing| {
        listing.read.contains(&session.name)
            && listing.panes.iter().any(|pane| pane.terminal_id.starts_with(&session.name))
    });
    let first = listing
        .panes
        .iter()
        .find(|pane| pane.terminal_id.starts_with(&format!("{}/", session.name)))
        .expect("a new session lists its first pane");
    let (named, _) = zellij::split_terminal_id(&first.terminal_id).expect("an id alacritree made");
    assert_eq!(named, session.name);

    let cwd = (side != Side::Native || !cfg!(windows)).then_some("/tmp");
    let created = zellij::create_pane(&program, &side, &session.name, cwd, false)
        .expect("zellij opens a pane in the session");
    let listing = session.list_until(|listing| {
        listing.panes.iter().any(|pane| {
            pane.terminal_id == created.terminal_id
                && cwd.is_none_or(|cwd| pane.cwd.as_deref() == Some(cwd))
        })
    });
    let pane = listing
        .panes
        .iter()
        .find(|pane| pane.terminal_id == created.terminal_id)
        .expect("the created pane is listed");
    assert_eq!(pane.tab_id.as_deref(), Some(created.tab_id.as_str()));
    if let Some(cwd) = cwd {
        assert_eq!(pane.cwd.as_deref(), Some(cwd), "the pane opened in the directory asked for");
    }

    zellij::focus_pane(&program, &side, &session.name, &created.pane_id)
        .expect("zellij focuses the pane");
    zellij::focus_pane(&program, &side, &session.name, &created.pane_id)
        .expect("focusing the focused pane is no refusal");
    let listing = session.list_until(|listing| {
        listing.panes.iter().any(|pane| pane.terminal_id == created.terminal_id && pane.focused)
    });
    assert!(
        listing.panes.iter().any(|pane| pane.terminal_id == created.terminal_id && pane.focused),
        "the focused pane is the one listed as focused"
    );

    let (_, argv) = zellij::attach(&program, &side, &session.name);
    assert!(argv.iter().any(|arg| arg.contains(&session.name)), "{argv:?} attaches elsewhere");

    run(&side, &program, &["kill-session", &session.name]);
    let listing = session.list_until(|listing| !listing.sessions.contains(&session.name));
    assert!(listing.lost(&session.name, &created.terminal_id), "a killed session's pane is gone");
}
