//! Where each request runs.  This is the one place that decides it: every
//! [`IpcRequest`] variant lands in exactly one of the enums below, and each
//! handler matches only the enum it owns, so a new request is placed here once
//! rather than claimed by one handler and refused by the others.

use std::path::PathBuf;

use strum::IntoStaticStr;

use super::protocol::IpcRequest;

#[derive(Debug, PartialEq)]
pub(crate) enum Route {
    Connection(ConnectionRequest),
    App(AppRequest),
}

/// Requests that would stall a frame (a working-tree walk, a worktree create
/// with its `git fetch`), so the connection thread serves them itself.
#[derive(Debug, PartialEq, IntoStaticStr)]
pub(crate) enum ConnectionRequest {
    GitStatus { path: PathBuf },
    CreateWorktree { project_root: PathBuf, branch: String },
}

/// Requests that need app state, so they cross to the UI thread.
#[derive(Debug, PartialEq)]
pub(crate) enum AppRequest {
    Deferred(DeferredRequest),
    Frame(FrameRequest),
}

/// Requests whose reply waits for work that outlives the frame they arrive
/// in, so the handler keeps the reply channel until that work lands.
#[derive(Debug, PartialEq, IntoStaticStr)]
pub(crate) enum DeferredRequest {
    /// Waits for background discovery, so the reply lists the worktrees
    /// found rather than the stale list.
    RefreshProject { root: PathBuf },
    /// Waits for background discovery of the new project.
    AddProject { path: PathBuf },
    /// Waits for the PTY: a client that creates a session to write
    /// to it would otherwise be told the id before anything can receive what
    /// it writes.
    CreateSession { workspace: Option<PathBuf> },
    /// Waits for the attached session's PTY.
    AttachMultiplexerPane {
        multiplexer: Option<String>,
        side: String,
        terminal_id: String,
        no_focus: bool,
    },
    /// Waits until the created pane's session can be read.
    CreateMultiplexerPane {
        multiplexer: Option<String>,
        side: Option<String>,
        workspace: Option<PathBuf>,
        no_focus: bool,
    },
}

/// Requests answered within the frame that drains them.
#[derive(Debug, PartialEq, IntoStaticStr)]
pub(crate) enum FrameRequest {
    ListProjects,
    ListSessions,
    SelectWorkspace { path: Option<PathBuf> },
    CloseSession { session_id: u64 },
    SendText { session_id: u64, text: String },
    MoveSession { session_id: u64, path: PathBuf },
    ReadScreen { session_id: u64, scrollback_lines: usize },
    ReadScratchpad { workspace: Option<String> },
    RemoveProject { root: PathBuf },
    RenameProject { root: PathBuf, label: Option<String> },
    ListMultiplexerPanes,
    RunAction { action: String },
}

impl From<DeferredRequest> for AppRequest {
    fn from(request: DeferredRequest) -> Self {
        Self::Deferred(request)
    }
}

impl From<FrameRequest> for AppRequest {
    fn from(request: FrameRequest) -> Self {
        Self::Frame(request)
    }
}

impl From<IpcRequest> for Route {
    fn from(request: IpcRequest) -> Self {
        use ConnectionRequest as Conn;
        use DeferredRequest as Deferred;
        use FrameRequest as Frame;
        use IpcRequest as Req;

        match request {
            Req::GitStatus { path } => Self::Connection(Conn::GitStatus { path }),
            Req::CreateWorktree { project_root, branch } => {
                Self::Connection(Conn::CreateWorktree { project_root, branch })
            },

            Req::RefreshProject { root } => Self::App(Deferred::RefreshProject { root }.into()),
            Req::AddProject { path } => Self::App(Deferred::AddProject { path }.into()),
            Req::CreateSession { workspace } => {
                Self::App(Deferred::CreateSession { workspace }.into())
            },
            Req::AttachMultiplexerPane { multiplexer, side, terminal_id, no_focus } => Self::App(
                Deferred::AttachMultiplexerPane { multiplexer, side, terminal_id, no_focus }.into(),
            ),
            Req::CreateMultiplexerPane { multiplexer, side, workspace, no_focus } => Self::App(
                Deferred::CreateMultiplexerPane { multiplexer, side, workspace, no_focus }.into(),
            ),

            Req::ListProjects => Self::App(Frame::ListProjects.into()),
            Req::ListSessions => Self::App(Frame::ListSessions.into()),
            Req::SelectWorkspace { path } => Self::App(Frame::SelectWorkspace { path }.into()),
            Req::CloseSession { session_id } => {
                Self::App(Frame::CloseSession { session_id }.into())
            },
            Req::SendText { session_id, text } => {
                Self::App(Frame::SendText { session_id, text }.into())
            },
            Req::MoveSession { session_id, path } => {
                Self::App(Frame::MoveSession { session_id, path }.into())
            },
            Req::ReadScreen { session_id, scrollback_lines } => {
                Self::App(Frame::ReadScreen { session_id, scrollback_lines }.into())
            },
            Req::ReadScratchpad { workspace } => {
                Self::App(Frame::ReadScratchpad { workspace }.into())
            },
            Req::RemoveProject { root } => Self::App(Frame::RemoveProject { root }.into()),
            Req::RenameProject { root, label } => {
                Self::App(Frame::RenameProject { root, label }.into())
            },
            Req::ListMultiplexerPanes => Self::App(Frame::ListMultiplexerPanes.into()),
            Req::RunAction { action } => Self::App(Frame::RunAction { action }.into()),
        }
    }
}

#[cfg(test)]
impl Route {
    fn name(&self) -> &'static str {
        match self {
            Self::Connection(request) => request.into(),
            Self::App(AppRequest::Deferred(request)) => request.into(),
            Self::App(AppRequest::Frame(request)) => request.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator;

    use super::*;

    /// A route arm that built the wrong variant (an add answered as a
    /// refresh, say) still compiles, since both carry a path.  The variant
    /// name surviving the trip is what catches it.
    #[test]
    fn every_request_keeps_its_name_through_routing() {
        for request in IpcRequest::iter() {
            let name: &'static str = (&request).into();
            assert_eq!(Route::from(request).name(), name);
        }
    }

    /// Where the slow requests run is the point of routing, so it is pinned
    /// rather than left to whichever arm a later edit touches.
    #[test]
    fn slow_git_work_stays_on_the_connection_thread() {
        let status = IpcRequest::GitStatus { path: PathBuf::from("/repo") };
        assert!(matches!(Route::from(status), Route::Connection(_)));

        let create =
            IpcRequest::CreateWorktree { project_root: PathBuf::from("/repo"), branch: "b".into() };
        assert!(matches!(Route::from(create), Route::Connection(_)));
    }

    #[test]
    fn a_session_create_waits_for_its_pty() {
        let create = IpcRequest::CreateSession { workspace: None };
        assert!(matches!(
            Route::from(create),
            Route::App(AppRequest::Deferred(DeferredRequest::CreateSession { .. }))
        ));
    }
}
