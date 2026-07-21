//! IPC socket for driving alacritree from outside the process (`alacritree
//! mcp` and anything else that speaks the protocol).
//!
//! Follows alacritty's `polling/ipc.rs`: a local socket advertised through the
//! `ALACRITREE_SOCKET` environment variable (so processes running *inside* an
//! alacritree session find their own instance), one newline-delimited JSON
//! request per connection, one JSON reply line back.  Unlike alacritty we need
//! replies with data, so every request gets a `{"ok": …}` / `{"error": …}`
//! response instead of fire-and-forget.
//!
//! The transport is a unix domain socket under `$XDG_RUNTIME_DIR/alacritree`
//! on unix and a named pipe under `\\.\pipe\` on Windows — `interprocess`
//! addresses both as a path, so the two differ only in where the path points.
//! Alacritty's IPC is unix-only, but nothing above the transport is, and the
//! MCP bridge is worth as much on Windows.
//!
//! Threading: the listener accepts on its own thread and spawns one thread
//! per connection.  Requests that touch app state are forwarded to the UI
//! thread as [`AppCall`]s (drained once per frame — the accompanying
//! `request_repaint` is what wakes an idle egui loop, same contract as
//! `EventProxy`).  Requests that would stall a frame (git status walks,
//! worktree creation with its `git fetch`) run directly on the connection
//! thread instead.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, ToFsName};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::git_status::{self, ChangeKind, GitStatus};
use crate::worktree::{self as wt, CreateRequest, Progress};

pub const SOCKET_ENV: &str = "ALACRITREE_SOCKET";

/// How long a connection waits for the UI thread before giving up — long
/// enough for a busy frame, short enough that a wedged app doesn't hang
/// clients forever.
const APP_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything a client can ask of a running alacritree.  Tagged so the wire
/// format is `{"type": "list_sessions", …fields}` — the MCP bridge builds
/// these directly from tool names + arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    ListProjects,
    ListSessions,
    SelectWorkspace {
        #[serde(default)]
        path: Option<PathBuf>,
    },
    CreateSession {
        #[serde(default)]
        workspace: Option<PathBuf>,
    },
    CloseSession {
        session_id: u64,
    },
    SendText {
        session_id: u64,
        text: String,
    },
    MoveSession {
        session_id: u64,
        path: PathBuf,
    },
    ReadScreen {
        session_id: u64,
        #[serde(default)]
        scrollback_lines: usize,
    },
    RefreshProject {
        root: PathBuf,
    },
    AddProject {
        path: PathBuf,
    },
    RemoveProject {
        root: PathBuf,
    },
    RenameProject {
        root: PathBuf,
        #[serde(default)]
        label: Option<String>,
    },
    GitStatus {
        path: PathBuf,
    },
    CreateWorktree {
        project_root: PathBuf,
        branch: String,
    },
    /// Run a named key-binding action (`FocusLeft`, `ToggleLeftSidebar`, …)
    /// as if its key had been pressed.  `bindings::parse_action` defines the
    /// accepted names, so every action a key can be bound to is reachable
    /// over the socket without a dedicated request.
    RunAction {
        action: String,
    },
}

pub type IpcResult = Result<Value, String>;

/// One request en route to the UI thread, with the channel the connection
/// thread is blocking on for the reply.
pub struct AppCall {
    pub request: IpcRequest,
    pub reply_tx: Sender<IpcResult>,
}

/// Owns the socket; dropping it (app shutdown) unlinks the path so clients
/// don't find a dead socket.
pub struct SocketHandle {
    path: PathBuf,
}

impl SocketHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        unlink_socket(&self.path);
    }
}

pub fn spawn_listener(ctx: egui::Context) -> std::io::Result<(SocketHandle, Receiver<AppCall>)> {
    let listener = listen_at(socket_path(), ctx)?;

    // Advertise the socket to child PTYs, like alacritty does with
    // ALACRITTY_SOCKET.  Startup runs before the first session spawns, so
    // no other thread is reading the environment concurrently.
    unsafe { std::env::set_var(SOCKET_ENV, listener.0.path()) };

    // Only WSLENV-listed variables cross the wsl.exe boundary — in either
    // direction.  Listing the socket lets programs in a distro find this
    // instance, whether they read the variable themselves or exec the
    // Windows CLI through interop (which inherits the distro's view); the
    // session id lets them name their own session in requests.  No
    // conversion flags: neither a pipe name nor an id is a path WSL should
    // translate.
    #[cfg(windows)]
    unsafe {
        std::env::set_var(
            "WSLENV",
            wslenv_with_alacritree_vars(std::env::var("WSLENV").ok().as_deref()),
        )
    };

    Ok(listener)
}

/// `WSLENV` extended with the variables alacritree exports — [`SOCKET_ENV`]
/// and [`crate::session::SESSION_ID_ENV`] — preserving whatever the user
/// already shares across the boundary.
#[cfg(any(test, windows))]
fn wslenv_with_alacritree_vars(current: Option<&str>) -> String {
    let mut wslenv = current.unwrap_or("").to_string();
    for name in [SOCKET_ENV, crate::session::SESSION_ID_ENV] {
        let listed = wslenv.split(':').any(|entry| entry.split('/').next() == Some(name));
        if !listed {
            if !wslenv.is_empty() {
                wslenv.push(':');
            }
            wslenv.push_str(name);
        }
    }
    wslenv
}

/// Listen on a caller-chosen path, without advertising it.
///
/// The real socket is named after the process id, which is unique per alacritree
/// but not per *listener*: two tests in one test binary would otherwise bind the
/// same name and answer each other's requests.
fn listen_at(
    path: PathBuf,
    ctx: egui::Context,
) -> std::io::Result<(SocketHandle, Receiver<AppCall>)> {
    // A leftover socket file at our pid (crashed predecessor) blocks bind; only
    // remove it once we've confirmed nothing is listening.
    #[cfg(unix)]
    if path.exists() && connect(&path).is_err() {
        unlink_socket(&path);
    }
    let name = path.clone().to_fs_name::<GenericFilePath>()?;
    let listener = ListenerOptions::new().name(name).create_sync()?;

    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("alacritree-ipc".into()).spawn(move || {
        // A Windows pipe accepts new connections only while the listener is
        // between accepts, so this loop must never stop calling `accept`; the
        // work happens on the per-connection thread.
        loop {
            let Ok(stream) = listener.accept() else { continue };
            let tx = tx.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("alacritree-ipc-conn".into())
                .spawn(move || handle_connection(stream, tx, ctx))
                .ok();
        }
    })?;

    Ok((SocketHandle { path }, rx))
}

fn handle_connection(stream: Stream, app_tx: Sender<AppCall>, ctx: egui::Context) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {},
    }
    let result = match serde_json::from_str::<IpcRequest>(&line) {
        Ok(request) => dispatch(request, &app_tx, &ctx),
        Err(e) => Err(format!("invalid IPC request: {e}")),
    };
    let reply = match &result {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "error": e }),
    };
    let mut writer = &stream;
    let _ = writer.write_all(reply.to_string().as_bytes());
    let _ = writer.write_all(b"\n");
    let _ = writer.flush();
}

fn dispatch(request: IpcRequest, app_tx: &Sender<AppCall>, ctx: &egui::Context) -> IpcResult {
    match request {
        // `compute` walks the working tree — the same work StatusCache
        // pushes to a background thread — so keep it off the UI thread.
        IpcRequest::GitStatus { path } => Ok(git_status_json(&git_status::compute(&path, None))),
        IpcRequest::CreateWorktree { project_root, branch } => {
            create_worktree(project_root, branch, app_tx, ctx)
        },
        other => call_app(other, app_tx, ctx),
    }
}

fn call_app(request: IpcRequest, app_tx: &Sender<AppCall>, ctx: &egui::Context) -> IpcResult {
    let (reply_tx, reply_rx) = mpsc::channel();
    app_tx
        .send(AppCall { request, reply_tx })
        .map_err(|_| "alacritree is shutting down".to_string())?;
    ctx.request_repaint();
    reply_rx
        .recv_timeout(APP_REPLY_TIMEOUT)
        .map_err(|_| "alacritree did not respond (app busy or closed)".to_string())?
}

/// Runs the same background flow as the sidebar's "+" button, blocking
/// this connection until git finishes.  `default_branch: None` makes the
/// worker resolve the base from `origin/HEAD` itself.  `base_dir: None` uses
/// the built-in default location: the connection thread has no `Config`, so it
/// can't honor the `[workspace]` override the UI path applies.
fn create_worktree(
    project_root: PathBuf,
    branch: String,
    app_tx: &Sender<AppCall>,
    ctx: &egui::Context,
) -> IpcResult {
    wt::validate_branch_name(&branch)?;
    let req = CreateRequest {
        project_root: project_root.clone(),
        default_branch: None,
        branch,
        base_dir: None,
    };
    let rx = wt::spawn_create(req, ctx.clone());
    let mut steps = Vec::new();
    loop {
        match rx.recv() {
            Ok(Progress::Step(s)) => steps.push(s),
            Ok(Progress::Done(Ok(path))) => {
                // Best-effort: if the project is in the sidebar, show the
                // new worktree without waiting for a manual refresh.
                let _ = call_app(IpcRequest::RefreshProject { root: project_root }, app_tx, ctx);
                return Ok(json!({ "path": path, "steps": steps }));
            },
            Ok(Progress::Done(Err(e))) => return Err(e),
            Err(_) => return Err("worktree creation worker died".to_string()),
        }
    }
}

pub fn git_status_json(status: &GitStatus) -> Value {
    if let Some(err) = &status.error {
        return json!({ "error": err });
    }
    let changes = |list: &[git_status::FileChange]| -> Vec<Value> {
        list.iter().map(|f| json!({ "path": f.path, "kind": kind_name(f.kind) })).collect()
    };
    json!({
        "branch": status.branch,
        "default_branch": status.default_branch,
        "staged": changes(&status.staged),
        "unstaged": changes(&status.unstaged),
        "diff_vs_default_branch": status
            .branch_diff
            .iter()
            .map(|d| json!({ "path": d.path, "additions": d.additions, "deletions": d.deletions }))
            .collect::<Vec<_>>(),
    })
}

fn kind_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
        ChangeKind::Untracked => "untracked",
        ChangeKind::Conflicted => "conflicted",
    }
}

// --- Client side (used by `alacritree mcp` and the CLI) ----------------------

/// Why a request did not produce a reply.
///
/// [`NoInstance`](SendError::NoInstance) is kept apart from every other failure
/// because it is not really an error: it is how the CLI learns there is no app
/// to talk to, and falls back to serving the request itself.  Distinguishing it
/// by matching on an error message would break the day someone rewords one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    NoInstance,
    Failed(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NoInstance => f.write_str("no running alacritree instance found"),
            SendError::Failed(e) => f.write_str(e),
        }
    }
}

/// Send one request to a running alacritree and wait for its reply.
///
/// The exchange runs on a worker thread because named pipes have no receive
/// timeout (`set_recv_timeout` is an error on Windows), so the bound has to
/// come from this side.  A request that times out leaves its thread parked on
/// the read until the app answers or dies — only reachable when the app is
/// already wedged, and both clients are short-lived processes.
pub fn send_request(
    socket: Option<&Path>,
    request: &IpcRequest,
    timeout: Duration,
) -> Result<Value, SendError> {
    let socket = socket.map(Path::to_path_buf);
    let request = request.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("alacritree-ipc-client".into())
        .spawn(move || {
            let _ = tx.send(exchange(socket.as_deref(), &request));
        })
        .map_err(|e| SendError::Failed(e.to_string()))?;

    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Err(SendError::Failed("alacritree did not reply in time".to_string())))
}

fn exchange(socket: Option<&Path>, request: &IpcRequest) -> Result<Value, SendError> {
    let stream = find_socket(socket).map_err(|_| SendError::NoInstance)?;
    exchange_on(stream, request).map_err(SendError::Failed)
}

fn exchange_on(stream: Stream, request: &IpcRequest) -> Result<Value, String> {
    let mut writer = &stream;
    let body = serde_json::to_string(request).map_err(|e| e.to_string())?;
    writer.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    writer.write_all(b"\n").map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;

    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| format!("no reply from alacritree: {e}"))?;
    let value: Value =
        serde_json::from_str(&reply).map_err(|e| format!("malformed IPC reply: {e}"))?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(err.to_string());
    }
    value.get("ok").cloned().ok_or_else(|| "malformed IPC reply".to_string())
}

/// Same resolution order as alacritty's `find_socket`: explicit path, then the
/// environment variable, then a scan of the socket directory.
fn find_socket(explicit: Option<&Path>) -> std::io::Result<Stream> {
    if let Some(path) = explicit {
        return connect(path);
    }
    if let Some(path) = std::env::var_os(SOCKET_ENV) {
        if let Ok(stream) = connect(Path::new(&path)) {
            return Ok(stream);
        }
    }
    for entry in std::fs::read_dir(socket_dir())?.flatten() {
        let path = entry.path();
        let is_socket_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("alacritree-") && n.ends_with(".sock"));
        if !is_socket_name {
            continue;
        }
        match connect(&path) {
            Ok(stream) => return Ok(stream),
            // Nothing listening means a crashed predecessor left the socket behind.
            Err(_) => unlink_socket(&path),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no alacritree socket"))
}

/// A unix socket outlives the process that bound it and has to be unlinked by
/// hand.  A named pipe is a kernel object that disappears once its last handle
/// closes, so Windows has nothing to clean up — and the path is not a file that
/// could be removed anyway.
#[cfg(unix)]
fn unlink_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(not(unix))]
fn unlink_socket(_path: &Path) {}

/// A busy pipe (every instance taken, before the listener has created the next
/// one) blocks inside `connect` rather than failing, so the only failure a
/// caller sees here is a socket with nothing behind it.  `send_request` bounds
/// the wait.
fn connect(path: &Path) -> std::io::Result<Stream> {
    let name = path.to_path_buf().to_fs_name::<GenericFilePath>()?;
    Stream::connect(name)
}

fn socket_path() -> PathBuf {
    socket_dir().join(format!("alacritree-{}.sock", std::process::id()))
}

/// `$XDG_RUNTIME_DIR/alacritree` with a tmpdir fallback, mirroring alacritty's
/// `socket_dir` (which also falls back to tmp on macOS).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("alacritree"))
        .and_then(|path| std::fs::create_dir_all(&path).ok().map(|_| path))
        .unwrap_or_else(std::env::temp_dir)
}

/// The named-pipe filesystem, which is also a directory: listing it is how a
/// client that did not inherit `ALACRITREE_SOCKET` finds a running instance.
#[cfg(windows)]
pub fn socket_dir() -> PathBuf {
    PathBuf::from(r"\\.\pipe\")
}

/// A listener on a name no other test in this binary will bind.
#[cfg(test)]
pub fn listen_for_test(
    label: &str,
    ctx: egui::Context,
) -> std::io::Result<(SocketHandle, Receiver<AppCall>)> {
    listen_at(socket_dir().join(format!("alacritree-test-{label}.sock")), ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::SESSION_ID_ENV;

    #[test]
    fn wslenv_gains_the_socket_exactly_once() {
        let ours = format!("{SOCKET_ENV}:{SESSION_ID_ENV}");
        assert_eq!(wslenv_with_alacritree_vars(None), ours);
        assert_eq!(wslenv_with_alacritree_vars(Some("")), ours);
        assert_eq!(wslenv_with_alacritree_vars(Some("LESS:FOO/p")), format!("LESS:FOO/p:{ours}"));
        // Already listed — with or without conversion flags — is not repeated.
        assert_eq!(wslenv_with_alacritree_vars(Some(&ours)), ours);
        let flagged = format!("{SOCKET_ENV}/u:LESS");
        assert_eq!(
            wslenv_with_alacritree_vars(Some(&flagged)),
            format!("{flagged}:{SESSION_ID_ENV}")
        );
    }

    /// Shells in a distro read their own id from the environment; like the
    /// socket, it only crosses wsl.exe if listed.
    #[test]
    fn wslenv_gains_the_session_id_exactly_once() {
        assert_eq!(wslenv_with_alacritree_vars(None), format!("{SOCKET_ENV}:{SESSION_ID_ENV}"));
        let flagged = format!("{SESSION_ID_ENV}/u");
        assert_eq!(wslenv_with_alacritree_vars(Some(&flagged)), format!("{flagged}:{SOCKET_ENV}"));
    }

    /// The client/server round trip over whatever transport the platform uses:
    /// framing, dispatch to the app thread, and the reply.  Discovery by
    /// scanning the socket directory is deliberately not tested — the scan
    /// would happily find a real alacritree running on the same machine.
    #[test]
    fn round_trip_over_the_socket() {
        let (handle, rx) = spawn_listener(egui::Context::default()).expect("listener");

        let app = std::thread::spawn(move || {
            let call = rx.recv().expect("request reached the app thread");
            assert!(matches!(call.request, IpcRequest::ListSessions));
            call.reply_tx.send(Ok(json!({ "sessions": [] }))).expect("reply");
        });

        let reply =
            send_request(Some(handle.path()), &IpcRequest::ListSessions, Duration::from_secs(10))
                .expect("reply from the listener");
        assert_eq!(reply, json!({ "sessions": [] }));
        app.join().unwrap();

        // The advertised path has to be connectable: it is how a shell running
        // inside a session reaches its own instance.
        assert_eq!(std::env::var_os(SOCKET_ENV).map(PathBuf::from).as_deref(), Some(handle.path()));
    }
}
