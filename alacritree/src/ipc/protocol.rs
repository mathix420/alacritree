//! What a client and a running alacritree say to each other, and how a client
//! finds one. The CLI and the MCP bridge need only this half, so reaching the
//! request types does not pull in the listener or the app thread behind it.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::git_status::{self, ChangeKind, GitStatus};

pub(crate) const SOCKET_ENV: &str = "ALACRITREE_SOCKET";

/// Everything a client can ask of a running alacritree.  Tagged so the wire
/// format is `{"type": "list_sessions", …fields}` — the MCP bridge builds
/// these directly from tool names + arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(strum::EnumIter, strum::IntoStaticStr))]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum IpcRequest {
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
    /// Read the auto-saved Markdown document, independently of whether its
    /// editor tab is open. `None`/`"current"` resolves on the UI thread;
    /// `"home"` and absolute worktree paths are explicit targets.
    ReadScratchpad {
        #[serde(default)]
        workspace: Option<String>,
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
    /// Every pane the multiplexer integration has detected, whether or not a
    /// session is attached to one.  A caller reaches an unattached pane no
    /// other way: `ListSessions` describes only what alacritree already
    /// holds.
    ListMultiplexerPanes,
    /// Open a session on a detected multiplexer pane, the way clicking its
    /// sidebar row does.  `side` and `terminal_id` are what
    /// `ListMultiplexerPanes` reports.  The pane id is deliberately not the
    /// target: it is positional and changes when a pane moves.
    ///
    /// `no_focus` opens the session behind the scenes: the workspace on
    /// screen, each workspace's active tab and the multiplexer's own focus
    /// all stay where they were.  `multiplexer` names the one to search, as
    /// `ListMultiplexerPanes` spells it; omitted, every enabled one is.
    AttachMultiplexerPane {
        #[serde(default)]
        multiplexer: Option<String>,
        side: String,
        terminal_id: String,
        #[serde(default)]
        no_focus: bool,
    },
    /// Open a new pane in the multiplexer and a session on it.  `side` and
    /// `workspace` both default: an omitted side picks the one the active
    /// session already belongs to, and an omitted workspace opens the pane
    /// in the focused one. `no_focus` means what it does for
    /// `AttachMultiplexerPane`.  An omitted `multiplexer` is the active
    /// session's, and failing that the first one enabled.
    CreateMultiplexerPane {
        #[serde(default)]
        multiplexer: Option<String>,
        #[serde(default)]
        side: Option<String>,
        #[serde(default)]
        workspace: Option<PathBuf>,
        #[serde(default)]
        no_focus: bool,
    },
    /// Run a named key-binding action (`FocusLeft`, `ToggleLeftSidebar`, …)
    /// as if its key had been pressed.  `bindings::parse_action` defines the
    /// accepted names, so every action a key can be bound to is reachable
    /// over the socket without a dedicated request.
    RunAction {
        action: String,
    },
}

/// A request's answer. The wire carries a refusal as plain text, so the error
/// is the message the client shows, and a handler turns its typed error into
/// that text only when it answers.
pub(crate) type IpcResult = Result<Value, String>;

pub(crate) fn git_status_json(status: &GitStatus) -> Value {
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

/// Why a request did not produce a reply.
///
/// [`NoInstance`](SendError::NoInstance) is kept apart from every other failure
/// because it is not really an error: it is how the CLI learns there is no app
/// to talk to, and falls back to serving the request itself.  Distinguishing it
/// by matching on an error message would break the day someone rewords one.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SendError {
    #[error("no running alacritree instance found")]
    NoInstance,
    #[error("alacritree did not reply in time")]
    TimedOut,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("no reply from alacritree: {0}")]
    NoReply(#[source] std::io::Error),
    #[error("malformed IPC reply: {0}")]
    MalformedReply(#[source] serde_json::Error),
    #[error("malformed IPC reply")]
    MissingOk,
    /// What the answering side said went wrong. The wire carries it as plain
    /// text, so it reaches here as the message the user reads.
    #[error("{0}")]
    Refused(String),
}

/// Send one request to a running alacritree and wait for its reply.
///
/// The exchange runs on a worker thread because named pipes have no receive
/// timeout (`set_recv_timeout` is an error on Windows), so the bound has to
/// come from this side.  A request that times out leaves its thread parked on
/// the read until the app answers or dies — only reachable when the app is
/// already wedged, and both clients are short-lived processes.
pub(crate) fn send_request(
    socket: Option<&Path>,
    request: &IpcRequest,
    timeout: Duration,
) -> Result<Value, SendError> {
    let socket = socket.map(Path::to_path_buf);
    let request = request.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("alacritree-ipc-client".into()).spawn(move || {
        let _ = tx.send(exchange(socket.as_deref(), &request));
    })?;

    rx.recv_timeout(timeout).unwrap_or(Err(SendError::TimedOut))
}

/// Where a client's requests go. Production sends them over a running
/// alacritree's socket, and tests hand them to the listener's dispatch
/// in-process.
pub(crate) trait Transport {
    fn send(&self, request: &IpcRequest, timeout: Duration) -> Result<Value, SendError>;
}

/// A running alacritree's socket: the given path, or the one discovery finds.
pub(crate) struct LocalSocket<'a>(pub Option<&'a Path>);

impl Transport for LocalSocket<'_> {
    fn send(&self, request: &IpcRequest, timeout: Duration) -> Result<Value, SendError> {
        send_request(self.0, request, timeout)
    }
}

fn exchange(socket: Option<&Path>, request: &IpcRequest) -> Result<Value, SendError> {
    let stream = find_socket(socket).map_err(|_| SendError::NoInstance)?;
    exchange_on(stream, request)
}

fn exchange_on(stream: Stream, request: &IpcRequest) -> Result<Value, SendError> {
    let mut writer = &stream;
    let body = serde_json::to_string(request).map_err(std::io::Error::from)?;
    writer.write_all(body.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).map_err(SendError::NoReply)?;
    let value: Value = serde_json::from_str(&reply).map_err(SendError::MalformedReply)?;
    if let Some(err) = value.get("error").and_then(Value::as_str) {
        return Err(SendError::Refused(err.to_string()));
    }
    value.get("ok").cloned().ok_or(SendError::MissingOk)
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
pub(super) fn unlink_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(not(unix))]
pub(super) fn unlink_socket(_path: &Path) {}

/// A busy pipe (every instance taken, before the listener has created the next
/// one) blocks inside `connect` rather than failing, so the only failure a
/// caller sees here is a socket with nothing behind it.  `send_request` bounds
/// the wait.
pub(super) fn connect(path: &Path) -> std::io::Result<Stream> {
    let name = path.to_path_buf().to_fs_name::<GenericFilePath>()?;
    Stream::connect(name)
}

/// `$XDG_RUNTIME_DIR/alacritree` with a tmpdir fallback, mirroring alacritty's
/// `socket_dir` (which also falls back to tmp on macOS).
#[cfg(unix)]
pub(crate) fn socket_dir() -> PathBuf {
    runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref())
        .map(|dir| dir.join("alacritree"))
        .and_then(|path| std::fs::create_dir_all(&path).ok().map(|_| path))
        .unwrap_or_else(std::env::temp_dir)
}

/// Resolve the runtime directory even when a GUI launcher strips the XDG
/// environment. Linux defines the default as `/run/user/$UID`; using the
/// effective uid keeps a desktop-launched MCP bridge in the same socket
/// directory as an alacritree window launched from a shell.
#[cfg(unix)]
fn runtime_dir(xdg_runtime_dir: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    xdg_runtime_dir.filter(|dir| !dir.is_empty()).map(PathBuf::from).or_else(platform_runtime_dir)
}

#[cfg(target_os = "linux")]
fn platform_runtime_dir() -> Option<PathBuf> {
    // SAFETY: `geteuid` takes no arguments and has no safety preconditions.
    let uid = unsafe { libc::geteuid() };
    Some(PathBuf::from("/run/user").join(uid.to_string()))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn platform_runtime_dir() -> Option<PathBuf> {
    None
}

/// The named-pipe filesystem, which is also a directory: listing it is how a
/// client that did not inherit `ALACRITREE_SOCKET` finds a running instance.
#[cfg(windows)]
pub(crate) fn socket_dir() -> PathBuf {
    PathBuf::from(r"\\.\pipe\")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn socket_discovery_survives_a_missing_xdg_runtime_dir() {
        // A GUI-launched MCP client does not necessarily forward
        // XDG_RUNTIME_DIR to stdio servers. Both processes must still derive
        // the conventional per-user directory.
        let uid = unsafe { libc::geteuid() };
        assert_eq!(runtime_dir(None), Some(PathBuf::from(format!("/run/user/{uid}"))));
    }
}
