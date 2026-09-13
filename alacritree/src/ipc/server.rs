//! The listener side of the socket.
//!
//! Threading: the listener accepts on its own thread and spawns one thread
//! per connection.  Requests that touch app state are forwarded to the UI
//! thread as [`AppCall`]s, drained once per frame. The accompanying
//! [`Repaint::wake`] is what wakes an idle UI loop, same contract as
//! `EventProxy`. Requests that would stall a frame (git status walks,
//! worktree creation with its `git fetch`) run directly on the connection
//! thread instead.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, ToFsName};
use serde_json::json;

#[cfg(unix)]
use super::protocol::connect;
use super::protocol::{
    IpcRequest, IpcResult, SOCKET_ENV, git_status_json, socket_dir, unlink_socket,
};
use crate::repaint::Repaint;
use crate::worktree::{self as wt, CreateRequest, Progress};
use crate::{git_status, jobs};

/// Absolute path to the running binary.  A shell can exec the CLI through it
/// without a PATH lookup — which is the only reliable way in a distro, where
/// the Windows binary is reachable through interop but is not on `$PATH`.
pub const EXE_ENV: &str = "ALACRITREE_EXE";

/// How long a connection waits for the UI thread before giving up — long
/// enough for a busy frame, short enough that a wedged app doesn't hang
/// clients forever.
const APP_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long this process will hold a pool worker for one create request.
/// The server's own limit, not a mirror of any client's: a client with a
/// different timeout changes nothing here.
///
/// It sits under the 300s the CLI and the MCP bridge allow so that an overrun
/// is reported by the side that knows why it overran.  A client's timer starts
/// when it sends; this one starts after the request has been received, parsed
/// and validated, so on an equal budget the client always gives up first and
/// this message is never seen.
const IPC_CREATE_BUDGET: Duration = Duration::from_secs(240);

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

pub fn spawn_listener(repaint: impl Repaint) -> std::io::Result<(SocketHandle, Receiver<AppCall>)> {
    let listener = listen_at(socket_path(), repaint)?;

    // Advertise the socket to child PTYs, like alacritty does with
    // ALACRITTY_SOCKET.  Startup runs before the first session spawns, so
    // no other thread is reading the environment concurrently.
    unsafe { std::env::set_var(SOCKET_ENV, listener.0.path()) };

    match std::env::current_exe() {
        Ok(exe) => unsafe { std::env::set_var(EXE_ENV, exe) },
        Err(e) => log::warn!("cannot advertise {EXE_ENV}: {e}"),
    }

    // Only WSLENV-listed variables cross the wsl.exe boundary — in either
    // direction.  Listing the socket lets programs in a distro find this
    // instance, whether they read the variable themselves or exec the
    // Windows CLI through interop (which inherits the distro's view); the
    // session id lets them name their own session in requests; the binary
    // path lets them exec the CLI at all, since the Windows image is not on
    // the distro's `$PATH`.
    #[cfg(windows)]
    unsafe {
        std::env::set_var(
            "WSLENV",
            wslenv_with_alacritree_vars(std::env::var("WSLENV").ok().as_deref()),
        )
    };

    Ok(listener)
}

/// `WSLENV` extended with the variables alacritree exports — [`SOCKET_ENV`],
/// [`crate::session::SESSION_ID_ENV`] and [`EXE_ENV`] — preserving whatever
/// the user already shares across the boundary.
///
/// Only the binary path carries a conversion flag: `/p` has WSL rewrite it
/// into the distro's view of the drive, honouring whatever automount root
/// that distro uses.  A pipe name and an id are not paths.
#[cfg(any(test, windows))]
fn wslenv_with_alacritree_vars(current: Option<&str>) -> String {
    let mut wslenv = current.unwrap_or("").to_string();
    for (name, flags) in [(SOCKET_ENV, ""), (crate::session::SESSION_ID_ENV, ""), (EXE_ENV, "/p")] {
        let listed = wslenv.split(':').any(|entry| entry.split('/').next() == Some(name));
        if !listed {
            if !wslenv.is_empty() {
                wslenv.push(':');
            }
            wslenv.push_str(name);
            wslenv.push_str(flags);
        }
    }
    wslenv
}

/// Bind the socket and start accepting on it. Advertising the path to child
/// processes is left to [`spawn_listener`].
fn listen_at(
    path: PathBuf,
    repaint: impl Repaint,
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
            let repaint = repaint.clone();
            std::thread::Builder::new()
                .name("alacritree-ipc-conn".into())
                .spawn(move || handle_connection(stream, tx, repaint))
                .ok();
        }
    })?;

    Ok((SocketHandle { path }, rx))
}

fn handle_connection(stream: Stream, app_tx: Sender<AppCall>, repaint: impl Repaint) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {},
    }
    let result = match serde_json::from_str::<IpcRequest>(&line) {
        Ok(request) => dispatch(request, &app_tx, &repaint),
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

fn dispatch(request: IpcRequest, app_tx: &Sender<AppCall>, repaint: &impl Repaint) -> IpcResult {
    match request {
        // `compute` walks the working tree — the same work StatusCache
        // pushes to a background thread — so keep it off the UI thread.
        // This is already the connection thread, not the UI thread; the
        // token just proves that plainly rather than adding a real wait.
        IpcRequest::GitStatus { path } => Ok(git_status_json(&jobs::on_this_thread(|blocking| {
            git_status::compute(&path, None, blocking)
        }))),
        IpcRequest::CreateWorktree { project_root, branch } => {
            create_worktree(project_root, branch, app_tx, repaint)
        },
        other => call_app(other, app_tx, repaint),
    }
}

fn call_app(request: IpcRequest, app_tx: &Sender<AppCall>, repaint: &impl Repaint) -> IpcResult {
    let (reply_tx, reply_rx) = mpsc::channel();
    app_tx
        .send(AppCall { request, reply_tx })
        .map_err(|_| "alacritree is shutting down".to_string())?;
    repaint.wake();
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
    repaint: &impl Repaint,
) -> IpcResult {
    wt::validate_branch_name(&branch)?;
    let req = CreateRequest {
        project_root: project_root.clone(),
        default_branch: None,
        branch,
        base_dir: None,
    };
    let (rx, job) = wt::spawn_create(req, repaint.clone());
    let outcome = drain_create(&rx, IPC_CREATE_BUDGET);
    // Dropping on every path, including the deadline, is what ends the fetch
    // and returns the worker.  Holding it would leave the pool one worker
    // smaller with nothing on screen or in the reply saying so.
    drop(job);
    match outcome {
        Ok((path, steps)) => {
            // Best-effort: if the project is in the sidebar, show the new
            // worktree without waiting for a manual refresh.
            let _ = call_app(IpcRequest::RefreshProject { root: project_root }, app_tx, repaint);
            Ok(json!({ "path": path, "steps": steps }))
        },
        Err(e) => Err(e),
    }
}

/// Collect a create's progress until it finishes or the budget runs out.
///
/// The deadline is computed once.  A per-message timeout would reset on every
/// step, so a job that keeps reporting would hold its worker past any budget.
fn drain_create(
    rx: &Receiver<Progress>,
    budget: Duration,
) -> Result<(PathBuf, Vec<String>), String> {
    let deadline = Instant::now() + budget;
    let mut steps = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Progress::Step(s)) => steps.push(s),
            Ok(Progress::Done(Ok(path))) => return Ok((path, steps)),
            Ok(Progress::Done(Err(e))) => return Err(e),
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!("worktree create exceeded {}s", budget.as_secs()));
            },
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the worktree create ended without reporting".into());
            },
        }
    }
}

fn socket_path() -> PathBuf {
    socket_dir().join(format!("alacritree-{}.sock", std::process::id()))
}

/// The listener's routing with no socket in between. A request takes the path
/// a connection thread gives it, and the ones bound for the app arrive on the
/// receiver [`InMemory::new`] returns.
#[cfg(test)]
pub struct InMemory<R: Repaint> {
    app_tx: Sender<AppCall>,
    repaint: R,
}

#[cfg(test)]
impl<R: Repaint> InMemory<R> {
    pub fn new(repaint: R) -> (Self, Receiver<AppCall>) {
        let (app_tx, app_rx) = mpsc::channel();
        (Self { app_tx, repaint }, app_rx)
    }
}

#[cfg(test)]
impl<R: Repaint> super::protocol::Transport for InMemory<R> {
    fn send(
        &self,
        request: &IpcRequest,
        _timeout: Duration,
    ) -> Result<serde_json::Value, super::protocol::SendError> {
        dispatch(request.clone(), &self.app_tx, &self.repaint)
            .map_err(super::protocol::SendError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ipc::protocol::send_request;
    use crate::repaint::Recorder;
    use crate::session::SESSION_ID_ENV;

    #[test]
    fn wslenv_gains_the_socket_exactly_once() {
        let ours = format!("{SOCKET_ENV}:{SESSION_ID_ENV}:{EXE_ENV}/p");
        assert_eq!(wslenv_with_alacritree_vars(None), ours);
        assert_eq!(wslenv_with_alacritree_vars(Some("")), ours);
        assert_eq!(wslenv_with_alacritree_vars(Some("LESS:FOO/p")), format!("LESS:FOO/p:{ours}"));
        // Already listed — with or without conversion flags — is not repeated.
        assert_eq!(wslenv_with_alacritree_vars(Some(&ours)), ours);
        let flagged = format!("{SOCKET_ENV}/u:LESS");
        assert_eq!(
            wslenv_with_alacritree_vars(Some(&flagged)),
            format!("{flagged}:{SESSION_ID_ENV}:{EXE_ENV}/p")
        );
    }

    /// The binary path is the one variable WSL must rewrite: a distro sees the
    /// Windows image through its automount root, not at `C:\…`.
    #[test]
    fn wslenv_lists_the_exe_path_for_conversion() {
        assert!(wslenv_with_alacritree_vars(None).ends_with(&format!("{EXE_ENV}/p")));
        // A user who already shares it, however flagged, keeps their spelling.
        let theirs = format!("{EXE_ENV}/up");
        assert_eq!(
            wslenv_with_alacritree_vars(Some(&theirs)),
            format!("{theirs}:{SOCKET_ENV}:{SESSION_ID_ENV}")
        );
    }

    /// Shells in a distro read their own id from the environment; like the
    /// socket, it only crosses wsl.exe if listed.
    #[test]
    fn wslenv_gains_the_session_id_exactly_once() {
        assert_eq!(
            wslenv_with_alacritree_vars(None),
            format!("{SOCKET_ENV}:{SESSION_ID_ENV}:{EXE_ENV}/p")
        );
        let flagged = format!("{SESSION_ID_ENV}/u");
        assert_eq!(
            wslenv_with_alacritree_vars(Some(&flagged)),
            format!("{flagged}:{SOCKET_ENV}:{EXE_ENV}/p")
        );
    }

    /// The client/server round trip over whatever transport the platform uses:
    /// framing, dispatch to the app thread, and the reply.  Discovery by
    /// scanning the socket directory is deliberately not tested — the scan
    /// would happily find a real alacritree running on the same machine.
    #[test]
    fn round_trip_over_the_socket() {
        let repaint = Recorder::default();
        let (handle, rx) = spawn_listener(repaint.clone()).expect("listener");

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
        assert_eq!(repaint.wakes(), 1, "a call forwarded to the app must wake it");

        // The advertised path has to be connectable: it is how a shell running
        // inside a session reaches its own instance.
        assert_eq!(std::env::var_os(SOCKET_ENV).map(PathBuf::from).as_deref(), Some(handle.path()));
        assert_eq!(
            std::env::var_os(EXE_ENV).map(PathBuf::from),
            std::env::current_exe().ok(),
            "a shell needs the binary path to exec the CLI back"
        );
    }

    /// The deadline is absolute.  A per-message timeout resets on every progress
    /// step, so a job that keeps reporting outlives the budget indefinitely: the
    /// same parked worker, reached more slowly.
    #[test]
    fn a_dribbling_create_still_ends_at_the_budget() {
        let (tx, rx) = mpsc::channel::<wt::Progress>();
        let budget = Duration::from_millis(300);
        let step = Duration::from_millis(50);
        let steps = 100u32;

        std::thread::spawn(move || {
            // Never sends `Done`; a real hung fetch reports and then stops.
            for _ in 0..steps {
                if tx.send(wt::Progress::Step("working".into())).is_err() {
                    return;
                }
                std::thread::sleep(step);
            }
        });

        let started = Instant::now();
        let outcome = drain_create(&rx, budget);
        let elapsed = started.elapsed();

        assert!(outcome.is_err(), "a create that never finished reported success");
        // The two behaviours are seconds apart: a per-message timeout runs the
        // whole dribble, an absolute deadline stops at the budget.  Splitting
        // that gap separates them without measuring `recv_timeout`'s precision.
        let dribble = step * steps;
        assert!(
            elapsed < dribble / 2,
            "the deadline reset on every step: took {elapsed:?} against a {budget:?} budget"
        );
    }
}
