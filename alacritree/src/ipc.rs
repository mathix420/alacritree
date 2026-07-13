//! IPC socket for driving alacritree from outside the process (`alacritree
//! mcp` and anything else that speaks the protocol).
//!
//! Mirrors alacritty's `polling/ipc.rs`: a unix socket in the runtime dir,
//! advertised through the `ALACRITREE_SOCKET` environment variable (so
//! processes running *inside* an alacritree session find their own instance),
//! one newline-delimited JSON request per connection, one JSON reply line
//! back.  Unlike alacritty we need replies with data, so every request gets a
//! `{"ok": …}` / `{"error": …}` response instead of fire-and-forget.
//!
//! Threading: the listener accepts on its own thread and spawns one thread
//! per connection.  Requests that touch app state are forwarded to the UI
//! thread as [`AppCall`]s (drained once per frame — the accompanying
//! `request_repaint` is what wakes an idle egui loop, same contract as
//! `EventProxy`).  Requests that would stall a frame (git status walks,
//! worktree creation with its `git fetch`) run directly on the connection
//! thread instead.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SOCKET_ENV: &str = "ALACRITREE_SOCKET";

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
    ReadScreen {
        session_id: u64,
        #[serde(default)]
        scrollback_lines: usize,
    },
    RefreshProject {
        root: PathBuf,
    },
    GitStatus {
        path: PathBuf,
    },
    CreateWorktree {
        project_root: PathBuf,
        branch: String,
    },
}

pub type IpcResult = Result<Value, String>;

/// One request en route to the UI thread, with the channel the connection
/// thread is blocking on for the reply.
pub struct AppCall {
    pub request: IpcRequest,
    pub reply_tx: Sender<IpcResult>,
}

/// Owns the socket file; dropping it (app shutdown) unlinks the path so
/// clients don't find a dead socket.
pub struct SocketHandle {
    path: PathBuf,
}

impl SocketHandle {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for SocketHandle {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(not(unix))]
pub fn spawn_listener(
    _ctx: egui::Context,
) -> std::io::Result<(SocketHandle, std::sync::mpsc::Receiver<AppCall>)> {
    // Mirrors upstream: alacritty's IPC is also unix-only.
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "IPC requires unix sockets"))
}

#[cfg(unix)]
pub use unix::{send_request, spawn_listener};

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{AppCall, IpcRequest, IpcResult, SOCKET_ENV, SocketHandle};
    use crate::git_status::{self, ChangeKind, GitStatus};
    use crate::worktree::{self as wt, CreateRequest, Progress};

    /// How long a connection waits for the UI thread before giving up — long
    /// enough for a busy frame, short enough that a wedged app doesn't hang
    /// clients forever.
    const APP_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn spawn_listener(
        ctx: egui::Context,
    ) -> std::io::Result<(SocketHandle, Receiver<AppCall>)> {
        let path = socket_dir().join(format!("alacritree-{}.sock", std::process::id()));
        // A leftover file at our pid (crashed predecessor) blocks bind; only
        // remove it once we've confirmed nothing is listening.
        if path.exists() && UnixStream::connect(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)?;

        // Advertise the socket to child PTYs, like alacritty does with
        // ALACRITTY_SOCKET.  Startup runs before the first session spawns, so
        // no other thread is reading the environment concurrently.
        unsafe { std::env::set_var(SOCKET_ENV, &path) };

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("alacritree-ipc".into()).spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
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

    fn handle_connection(stream: UnixStream, app_tx: Sender<AppCall>, ctx: egui::Context) {
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
        let mut stream = &stream;
        let _ = stream.write_all(reply.to_string().as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    }

    fn dispatch(request: IpcRequest, app_tx: &Sender<AppCall>, ctx: &egui::Context) -> IpcResult {
        match request {
            // `compute` walks the working tree — the same work StatusCache
            // pushes to a background thread — so keep it off the UI thread.
            IpcRequest::GitStatus { path } => {
                Ok(git_status_json(&git_status::compute(&path, None)))
            },
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
    /// worker resolve the base from `origin/HEAD` itself.
    fn create_worktree(
        project_root: PathBuf,
        branch: String,
        app_tx: &Sender<AppCall>,
        ctx: &egui::Context,
    ) -> IpcResult {
        wt::validate_branch_name(&branch)?;
        let req =
            CreateRequest { project_root: project_root.clone(), default_branch: None, branch };
        let rx = wt::spawn_create(req, ctx.clone());
        let mut steps = Vec::new();
        loop {
            match rx.recv() {
                Ok(Progress::Step(s)) => steps.push(s),
                Ok(Progress::Done(Ok(path))) => {
                    // Best-effort: if the project is in the sidebar, show the
                    // new worktree without waiting for a manual refresh.
                    let _ =
                        call_app(IpcRequest::RefreshProject { root: project_root }, app_tx, ctx);
                    return Ok(json!({ "path": path, "steps": steps }));
                },
                Ok(Progress::Done(Err(e))) => return Err(e),
                Err(_) => return Err("worktree creation worker died".to_string()),
            }
        }
    }

    fn git_status_json(status: &GitStatus) -> Value {
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

    // --- Client side (used by `alacritree mcp`) -----------------------------

    /// Send one request to a running alacritree and wait for its reply.
    pub fn send_request(
        socket: Option<&Path>,
        request: &IpcRequest,
        timeout: Duration,
    ) -> IpcResult {
        let stream = find_socket(socket)
            .map_err(|e| format!("no running alacritree instance found: {e}"))?;
        stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;

        let mut writer = &stream;
        let body = serde_json::to_string(request).map_err(|e| e.to_string())?;
        writer.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        writer.write_all(b"\n").map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        // Like `alacritty msg`: close the write end so the server sees EOF.
        stream.shutdown(Shutdown::Write).map_err(|e| e.to_string())?;

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

    /// Same resolution order as alacritty's `find_socket`: explicit path,
    /// then the environment variable, then a scan of the socket dir (removing
    /// sockets nothing listens on, since those are leftovers from crashes).
    fn find_socket(explicit: Option<&Path>) -> std::io::Result<UnixStream> {
        if let Some(path) = explicit {
            return UnixStream::connect(path);
        }
        if let Some(path) = std::env::var_os(SOCKET_ENV) {
            if let Ok(stream) = UnixStream::connect(&path) {
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
            match UnixStream::connect(&path) {
                Ok(stream) => return Ok(stream),
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                },
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no alacritree socket"))
    }

    /// `$XDG_RUNTIME_DIR/alacritree` with a tmpdir fallback, mirroring
    /// alacritty's `socket_dir` (which also falls back to tmp on macOS).
    fn socket_dir() -> PathBuf {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|dir| PathBuf::from(dir).join("alacritree"))
            .and_then(|path| std::fs::create_dir_all(&path).ok().map(|_| path))
            .unwrap_or_else(std::env::temp_dir)
    }
}
