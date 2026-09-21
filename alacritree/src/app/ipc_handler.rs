//! The app side of the IPC channel.  Requests run on the UI thread inside
//! `update` so every request sees and mutates app state the same way user
//! input does; the connection thread blocks on `reply_tx` meanwhile.

use super::*;
use crate::ipc::route::{AppRequest, DeferredRequest, FrameRequest};

impl AlacritreeApp {
    pub(super) fn start_ipc(
        ctx: &Context,
        config: &Config,
    ) -> (Option<ipc::server::SocketHandle>, Option<Receiver<ipc::server::AppCall>>) {
        // Before the first PTY spawn so children inherit ALACRITREE_SOCKET.
        if config.ipc_socket {
            match ipc::server::spawn_listener(ctx.clone(), config.workspace.clone()) {
                Ok((handle, rx)) => {
                    log::info!("IPC socket: {}", handle.path().display());
                    (Some(handle), Some(rx))
                },
                Err(e) => {
                    log::warn!("failed to create IPC socket: {e}");
                    (None, None)
                },
            }
        } else {
            (None, None)
        }
    }

    /// One session as the IPC reply describes it.  `agent`, `busy` and
    /// `multiplexer` are nullable because a plain shell has no agent, a
    /// multiplexer-backed session has no foreground job of its own to probe,
    /// and a session owning its PTY belongs to no multiplexer.
    pub(super) fn session_json(&self, session: &AppSession, is_active_tab: bool) -> Value {
        let key = session.pane_key.as_ref();
        let activity = self.session_activity(session);
        let done = session.done || self.session_pane_status(session) == Some(PaneStatus::Done);
        json!({
            "id": session.id,
            "title": session.title,
            "workspace": session.working_directory,
            "kind": match &session.kind {
                SessionKind::Shell => "shell",
                SessionKind::Diff { .. } => "diff",
                SessionKind::Scratchpad { .. } => "scratchpad",
                SessionKind::Tasks => "tasks",
            },
            "columns": session.size.columns,
            "lines": session.size.screen_lines,
            "is_active_tab": is_active_tab,
            "needs_attention": session.needs_attention,
            "agent": activity_json(activity, done),
            "busy": key.is_none().then(|| session.is_busy()),
            "multiplexer": key.map(|key| self.session_multiplexer_json(key)),
        })
    }

    pub(super) fn process_ipc_calls(&mut self, ctx: &Context) {
        let Some(rx) = &self.ipc_rx else { return };
        let calls: Vec<ipc::server::AppCall> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        for call in calls {
            let ipc::server::AppCall { request, reply_tx } = call;
            match request {
                AppRequest::Deferred(request) => self.defer_ipc_request(ctx, request, reply_tx),
                AppRequest::Frame(request) => {
                    let name: &'static str = (&request).into();
                    let started = std::time::Instant::now();
                    let result = self.handle_ipc_request(ctx, request);
                    crate::frame_log::note_if_slow("ipc request", name, started.elapsed());
                    // A send error means the client gave up waiting, so there
                    // is nothing to do.
                    let _ = reply_tx.send(result);
                },
            }
        }
    }

    /// Each of these answers `reply_tx` itself once its work lands, which is
    /// why it takes the channel rather than returning a result.
    fn defer_ipc_request(
        &mut self,
        ctx: &Context,
        request: DeferredRequest,
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
    ) {
        match request {
            DeferredRequest::RefreshProject { root } => {
                self.defer_project_refresh(ctx, root, reply_tx)
            },
            DeferredRequest::AddProject { path } => self.defer_project_add(ctx, path, reply_tx),
            DeferredRequest::CreateSession { workspace } => {
                self.defer_create_session(ctx, workspace, reply_tx)
            },
            DeferredRequest::AttachMultiplexerPane { multiplexer, side, terminal_id, no_focus } => {
                let focus = AttachFocus::requested(no_focus);
                self.defer_attach_multiplexer_pane(
                    ctx,
                    (multiplexer.as_deref(), &side, &terminal_id),
                    reply_tx,
                    focus,
                );
            },
            DeferredRequest::CreateMultiplexerPane { multiplexer, side, workspace, no_focus } => {
                let focus = AttachFocus::requested(no_focus);
                self.defer_create_multiplexer_pane(
                    ctx,
                    (multiplexer.as_deref(), side.as_deref()),
                    workspace,
                    reply_tx,
                    focus,
                );
            },
        }
    }

    fn defer_project_refresh(
        &mut self,
        ctx: &Context,
        root: PathBuf,
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
    ) {
        let Some(idx) = self.projects.iter().position(|p| p.root == root) else {
            let _ =
                reply_tx.send(Err(format!("{} is not a project in the sidebar", root.display())));
            return;
        };
        self.refresh_project(ctx, idx);
        if let Some(reply_tx) = self.project_refreshes.watch(&root, reply_tx) {
            let _ = reply_tx.send(Ok(project_json(&self.projects[idx])));
        }
    }

    /// A project that is already in the sidebar is answered from the list as
    /// it stands; a new one goes in as a placeholder and its discovery runs on
    /// a worker, so the reply waits for that rather than describing worktrees
    /// nothing has looked for yet.
    fn defer_project_add(
        &mut self,
        ctx: &Context,
        path: PathBuf,
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
    ) {
        self.add_project_off_thread(ctx, path.clone());
        let Some(idx) = self.projects.iter().position(|p| p.root == path) else {
            let _ = reply_tx.send(Err(format!("{} could not be added", path.display())));
            return;
        };
        if let Some(reply_tx) = self.project_refreshes.watch(&path, reply_tx) {
            let _ = reply_tx.send(Ok(project_json(&self.projects[idx])));
        }
    }

    fn defer_create_session(
        &mut self,
        ctx: &Context,
        workspace: Option<PathBuf>,
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
    ) {
        let workspace = match workspace {
            None => None,
            Some(p) => match self.known_worktree_path(&p) {
                Some(known) => Some(known),
                None => {
                    let _ = reply_tx.send(Err(unknown_worktree(&p)));
                    return;
                },
            },
        };
        let id = match self.spawn_session(ctx, workspace) {
            Ok(id) => id,
            // `defer_create_session` answers the client itself, so a failure
            // the frame can still see has to be sent rather than returned.
            Err(e) => {
                let _ = reply_tx.send(Err(format!("failed to spawn shell: {e}")));
                return;
            },
        };
        // Nothing is opening for this id when the gate is off, since
        // `spawn_session` attaches inline before returning: `watch` hands
        // the channel straight back and it is answered the same way the
        // gate-off path answers it.
        if let Some(reply_tx) = self.pending_spawns.watch(id, reply_tx) {
            let _ = reply_tx.send(Ok(json!({ "session_id": id })));
        }
    }

    fn handle_ipc_request(
        &mut self,
        ctx: &Context,
        request: FrameRequest,
    ) -> ipc::protocol::IpcResult {
        use FrameRequest as Req;
        match request {
            Req::ListProjects => Ok(json!({
                "current_workspace": self.current_workspace,
                "projects": self.projects.iter().map(project_json).collect::<Vec<_>>(),
            })),
            Req::ListSessions => {
                let sessions: Vec<Value> = self
                    .sessions
                    .iter()
                    .map(|s| {
                        let active = self.sessions.active(&s.working_directory) == Some(s.id);
                        self.session_json(s, active)
                    })
                    .collect();
                Ok(json!({ "current_workspace": self.current_workspace, "sessions": sessions }))
            },
            Req::SelectWorkspace { path } => match path {
                None => {
                    self.activate_home(ctx);
                    Ok(json!({ "workspace": Value::Null }))
                },
                Some(p) => {
                    let known = self.known_worktree_path(&p).ok_or_else(|| unknown_worktree(&p))?;
                    self.activate_worktree(ctx, &known);
                    Ok(json!({ "workspace": known }))
                },
            },
            Req::CloseSession { session_id } => {
                if !self.sessions.iter().any(|s| s.id == session_id) {
                    return Err(format!("no session with id {session_id}"));
                }
                self.close_session(ctx, session_id);
                Ok(json!({ "closed": session_id }))
            },
            Req::MoveSession { session_id, path } => {
                let target =
                    self.workspace_for_path(&path).ok_or_else(|| unknown_worktree(&path))?;
                let workspace = self.move_session_to_key(session_id, Some(target))?;
                // A silent re-grouping produces no PTY events, so nothing
                // else would wake the next paint.
                ctx.request_repaint();
                Ok(json!({ "session_id": session_id, "workspace": workspace }))
            },
            Req::SendText { session_id, text } => {
                let idx = self
                    .sessions
                    .iter()
                    .position(|s| s.id == session_id)
                    .ok_or_else(|| format!("no session with id {session_id}"))?;
                let written = text.len();
                if let Some(editor) = self.sessions[idx].scratchpad.as_mut() {
                    editor.insert_at_cursor(ctx, session_id, &text);
                } else {
                    let session = &mut self.sessions[idx];
                    paste::on_terminal_input_start(session);
                    session.write(text.into_bytes());
                }
                Ok(json!({ "bytes_written": written }))
            },
            Req::ReadScreen { session_id, scrollback_lines } => {
                let session = self
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .ok_or_else(|| format!("no session with id {session_id}"))?;
                let snapshot = session.screen_snapshot(scrollback_lines);
                Ok(json!({
                    "title": session.title,
                    "lines": snapshot.lines,
                    "cursor": { "line": snapshot.cursor_line, "column": snapshot.cursor_column },
                    "scrollback_available": snapshot.history_size,
                }))
            },
            Req::ReadScratchpad { workspace } => {
                let workspace = match workspace.as_deref() {
                    None | Some("current") => self.current_workspace.clone(),
                    Some("home") => None,
                    Some(path) => Some(
                        self.known_worktree_path(Path::new(path))
                            .ok_or_else(|| unknown_worktree(Path::new(path)))?,
                    ),
                };
                scratchpad::read_json(&workspace)
            },
            Req::RemoveProject { root } => {
                let idx =
                    self.projects.iter().position(|p| p.root == root).ok_or_else(|| {
                        format!("{} is not a project in the sidebar", root.display())
                    })?;
                Ok(json!({ "removed": self.remove_project(idx) }))
            },
            Req::RenameProject { root, label } => {
                let idx = self.rename_project(&root, label)?;
                Ok(project_json(&self.projects[idx]))
            },
            Req::ListMultiplexerPanes => Ok(self.multiplexer_panes_json()),
            Req::RunAction { action } => match crate::bindings::parse_action(&action) {
                BindingAction::Unsupported(name) => Err(format!("unknown action `{name}`")),
                parsed => {
                    self.dispatch_action(ctx, parsed, ActionOrigin::Ipc);
                    Ok(json!({ "action": action }))
                },
            },
        }
    }

    /// Like [`Self::known_worktree_path`], but a path anywhere *inside* a
    /// worktree's subtree counts — a mover reports its cwd, which is usually
    /// a subdirectory, not the worktree root itself.
    fn workspace_for_path(&self, path: &Path) -> Option<PathBuf> {
        let worktrees: Vec<PathBuf> =
            self.projects.iter().flat_map(|p| &p.worktrees).map(|wt| wt.path.clone()).collect();
        owning_worktree(&worktrees, path)
            .or_else(|| path.canonicalize().ok().and_then(|c| owning_worktree(&worktrees, &c)))
    }
}

/// `SessionActivity` as the reply spells it.  A plain shell is null rather
/// than an object, so a consumer testing for presence needs no second field.
///
/// `state` is the live reading, except that a finished turn nobody has looked
/// at yet reads `done`, the word herdr uses for the same thing.
fn activity_json(activity: SessionActivity, done: bool) -> Value {
    match activity {
        SessionActivity::Shell => Value::Null,
        SessionActivity::Agent { name, live } => json!({
            "name": name,
            "state": if done { "done" } else { live.label() },
        }),
    }
}
