//! Following herdr's focus, driven through the real binary.
//!
//! Every check here is about what the running window does when herdr's focus
//! moves, which no in-crate test can observe: following spawns attach
//! clients, switches workspace and takes terminal focus. So these pilot the
//! executable against a herdr server of their own.
//!
//! Opt-in. They need a herdr binary, they hold the foreground for their whole
//! run, and a window appears while they do.

#![cfg(windows)]
// This crate has no lib target, reaching neither `command_ext::hidden` nor
// `alacritree::jobs`, and `clippy.toml` disallows only `Command` methods
// here — there is no UI thread for a blocking wait to stall.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a piloted assertion waits for the window to catch up. The poll
/// interval puts up to two seconds of age on a focus change before alacritree
/// can see it at all, and an attach on Windows costs more.
const SETTLE: Duration = Duration::from_secs(15);

/// How long `drop` waits for `herdr server stop` before killing it outright,
/// so a hung stop cannot hold the window open on the developer's desktop.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_alacritree")
}

/// The session name is the whole isolation story: it names the herdr session,
/// the config directory under it, and what the seatbelt asserts it sees.
const SESSION: &str = "alacritree-e2e";

struct Harness {
    home: tempfile::TempDir,
    server: Option<Child>,
    child: Option<Child>,
}

impl Harness {
    /// One redirected environment for all three parties. Redirecting it for
    /// only the alacritree child breaks in both directions: its herdr
    /// children would look for the socket under the temp directory where no
    /// server is listening, and a shared-view attach resolves its session
    /// name from whatever herdr lists as running, which would be the
    /// developer's own.
    fn env(command: &mut Command, home: &Path) {
        command
            .env("APPDATA", home)
            .env("LOCALAPPDATA", home)
            .env("XDG_CONFIG_HOME", home)
            .env("XDG_STATE_HOME", home)
            .env("HOME", home)
            .env("HERDR_SESSION", SESSION)
            // wsl.exe forwards nothing by default, so a herdr inside a distro
            // would answer for the user's real session. Forwarding the name
            // points it at one that does not exist there, which reads as a
            // server error and leaves the side retrying rather than
            // answering.
            .env("WSLENV", "HERDR_SESSION")
            // herdr sets these inside every managed pane, and the socket path
            // outranks the session name, so a run started from inside a pane
            // would otherwise drive the pane it is running in.
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH");
    }

    fn start(follow_focus: &str) -> Self {
        let home = tempfile::tempdir().expect("a temp dir");
        write_config(home.path(), follow_focus);

        let mut server = Command::new("herdr");
        Self::env(&mut server, home.path());
        let server = server
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("herdr is on PATH; these tests are opt-in and a missing binary is a failure");

        // `Child` has no killing `Drop`, so the server must live inside a
        // `Harness` from the moment it exists — everything below can panic
        // without orphaning it, since `Drop` now runs on the unwind.
        let mut harness = Self { home, server: Some(server), child: None };
        let harness_home = harness.home().to_path_buf();
        wait_for(|| running_session(&harness_home).is_some())
            .expect("the throwaway herdr server starts");

        let mut child = Command::new(binary());
        Self::env(&mut child, harness.home());
        harness.child = Some(child.spawn().expect("the alacritree binary runs"));

        harness.assert_isolated();
        wait_for(|| harness.foreground_is_child()).expect("the window takes foreground");
        harness
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// `start` is the only place this is briefly `None`; every other method
    /// runs after it returns `Self`, so the pid is always present by then.
    fn child_pid(&self) -> u32 {
        self.child.as_ref().expect("the harness has finished starting").id()
    }

    /// Before any mutating herdr command: exactly one running session, named
    /// after this test, with its directory under the temp directory. The
    /// isolation failed twice by accident while this recipe was established.
    fn assert_isolated(&self) {
        let session = running_session(self.home()).expect("the throwaway session is running");
        assert_eq!(session["name"], SESSION, "a foreign herdr session is running");
        let dir = PathBuf::from(session["session_dir"].as_str().expect("a session directory"));
        // Windows spells this with a backslash, so comparing a
        // `sessions/<name>` substring would silently never match.
        assert!(
            dir.starts_with(self.home()),
            "the herdr session directory {dir:?} is outside {:?}",
            self.home()
        );
    }

    fn herdr(&self, args: &[&str]) -> Output {
        self.assert_isolated();
        let mut command = Command::new("herdr");
        Self::env(&mut command, self.home());
        command.args(args).output().expect("herdr runs")
    }

    /// Every alacritree command names the child's own socket. Without it a
    /// client that inherited no socket variable finds an instance by listing
    /// the pipe directory, and the developer's live window is in there.
    fn alacritree(&self, args: &[&str]) -> Output {
        let mut command = Command::new(binary());
        Self::env(&mut command, self.home());
        command
            .env("ALACRITREE_SOCKET", format!(r"\\.\pipe\alacritree-{}.sock", self.child_pid()))
            .args(args)
            .output()
            .expect("the alacritree binary runs")
    }

    fn sessions(&self) -> Value {
        let out = self.alacritree(&["session", "list", "--json"]);
        serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
            panic!(
                "session list --json is not JSON: {err}: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// Following only fires while the window is the OS-focused one, and
    /// nothing in the reply carries window focus, so the test asks Win32.
    fn foreground_is_child(&self) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        let window = unsafe { GetForegroundWindow() };
        if window.is_null() {
            return false;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        pid == self.child_pid()
    }
}

impl Drop for Harness {
    /// Order matters. `server stop` runs first, bounded so a hang cannot
    /// hold the rest of teardown open, giving every attach client a chance
    /// to exit on its own and herdr its own shutdown; `Child::kill` is
    /// `TerminateProcess` here and skips every `Drop`. Then the window,
    /// whose conpty children die with the pseudoconsole handle, since `Quit`
    /// opens a dialog rather than exiting. Then the server, killed directly
    /// whether or not the graceful stop reached it first, since terminating
    /// an already-exited process is a no-op. Then the session, which refuses
    /// to be deleted while it runs.
    fn drop(&mut self) {
        let mut stop = Command::new("herdr");
        Self::env(&mut stop, self.home.path());
        if let Ok(mut stop) = stop
            .args(["server", "stop"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if !wait_bounded(&mut stop, STOP_TIMEOUT) {
                let _ = stop.kill();
            }
            let _ = stop.wait();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        let mut delete = Command::new("herdr");
        Self::env(&mut delete, self.home.path());
        let _ = delete.args(["session", "delete", SESSION]).output();
    }
}

/// `show_panes` is on because a tab created on the test server runs a plain
/// shell, and an unattached side asks `agent list`, which does not list one.
fn write_config(home: &Path, follow_focus: &str) {
    let dir = home.join("alacritty");
    std::fs::create_dir_all(&dir).expect("the config directory");
    let mut file = std::fs::File::create(dir.join("alacritree.toml")).expect("the config file");
    write!(
        file,
        "[integrations.herdr]\nshow_panes = true\nshow_unmatched = true\nattach = \
         \"session\"\nfollow_focus = \"{follow_focus}\"\n"
    )
    .expect("the config is written");
}

fn running_session(home: &Path) -> Option<Value> {
    let mut command = Command::new("herdr");
    Harness::env(&mut command, home);
    let out = command.args(["session", "list", "--json"]).output().ok()?;
    let listing: Value = serde_json::from_slice(&out.stdout).ok()?;
    listing["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["running"].as_bool().unwrap_or(false))
        .cloned()
}

fn wait_for(mut ready: impl FnMut() -> bool) -> Result<(), ()> {
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(())
}

/// A non-blocking poll for teardown, so a wedged child cannot hold `drop`
/// open forever; the caller kills it outright once this gives up.
fn wait_bounded(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return false,
        }
    }
    false
}

/// The active session for a workspace, as the reply names it. Finds the
/// active tab first and reads its terminal id second, so a session missing
/// one yields `None` instead of the scan quietly continuing to a neighbour.
fn active_terminal(sessions: &Value) -> Option<String> {
    let workspace = sessions["current_workspace"].clone();
    let active = sessions["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["workspace"] == workspace && s["is_active_tab"].as_bool().unwrap_or(false))?;
    active["multiplexer"]["terminal_id"].as_str().map(str::to_string)
}

/// The default mode follows herdr only from a session already showing its
/// view, so a new pane created while a native session is active must not move
/// the window. This is the guard on Arnaud's unmodified config.
#[test]
#[ignore = "spawns a herdr server and a window; run with the e2e task"]
fn the_default_mode_does_not_follow_from_a_native_session() {
    let harness = Harness::start("herdr");
    let before = active_terminal(&harness.sessions());
    let created = harness.herdr(&["tab", "create", "--focus"]);
    assert!(created.status.success(), "tab create failed: {created:?}");
    // Long enough for two poll intervals plus the quiet gap, so a follow that
    // was going to happen has happened.
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(active_terminal(&harness.sessions()), before, "the window followed herdr");
}
