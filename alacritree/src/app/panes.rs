//! Sessions on panes a multiplexer owns: attaching to one, creating one, and
//! keeping the multiplexer's focus in step with the session on screen.  What
//! a multiplexer answers comes through [`MultiplexerSession`]; this file only
//! decides what the app does with the answer.

use super::*;
use crate::multiplexer::{
    AttachFocus, AttachRequest, CreateRequest, Launch, ListedPane, Managed, MultiplexerKind, Pane,
    PaneKey, ViewState,
};

impl AlacritreeApp {
    /// Opens a pane in a session running the multiplexer's attach client.
    /// The session is an ordinary shell, so nothing in the grid or input path
    /// treats it specially; only the key marks it as this pane's row.
    /// Returns whether the attach succeeded so the caller can make `switch`
    /// first and undo it on failure: a refusal is only readable in the
    /// workspace it happened in.  `unlisted` stands in for a pane the listing
    /// does not carry.
    pub(super) fn attach_pane(
        &mut self,
        ctx: &Context,
        key: PaneKey,
        unlisted: PaneTarget,
        switch: &WorkspaceSwitch,
        waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
        focus: AttachFocus,
    ) -> bool {
        if let Some(id) = self.pane_session(&key) {
            if focus.takes() {
                self.activate_session_by_id(id);
            }
            if let Some(waiter) = waiter {
                let _ = waiter.send(Ok(json!({ "session_id": id })));
            }
            return true;
        }
        let multiplexer = self.multiplexers.get(key.multiplexer);
        let target = multiplexer
            .find(&key.side, &key.terminal_id)
            .map_or(unlisted, |pane| pane.target(&key.side));
        if let Some(launch) = multiplexer.open_directly(&target) {
            // Nothing to ask the multiplexer first: the pane id is the whole
            // target, and the client attaches to it directly.
            let opened = self.open_pane_session(ctx, key, switch.to.clone(), launch, false, focus);
            return match opened {
                Some(id) => {
                    self.park_attach_reply(id, waiter);
                    true
                },
                None => {
                    let message = self
                        .modals
                        .error_dialog
                        .take()
                        .unwrap_or_else(|| "failed to attach the pane".to_string());
                    self.refuse_multiplexer_request(waiter, message, focus);
                    false
                },
            };
        }
        // The shared view is a process call or two, and running them from
        // the click would hold the frame for as long as they take.
        let request = AttachRequest {
            workspace: switch.to.clone(),
            previous: switch.from.clone(),
            waiters: waiter.into_iter().collect(),
            focus,
        };
        self.multiplexers.get_mut(key.multiplexer).queue_attach(key, target, request);
        ctx.request_repaint();
        true
    }

    /// Answer an attach once the session's PTY is live.  A client that
    /// attached to read the pane would otherwise be handed an id
    /// before anything behind it can answer.
    pub(super) fn park_attach_reply(
        &mut self,
        id: SessionId,
        waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
    ) {
        let Some(waiter) = waiter else { return };
        if let Some(waiter) = self.pending_spawns.watch(id, waiter) {
            let _ = waiter.send(Ok(json!({ "session_id": id })));
        }
    }

    /// Open a session on every listed pane no session holds yet.  The listing
    /// is the one the sidebar and the palette both draw, so this opens exactly
    /// the rows the user could have opened one at a time.
    ///
    /// The batch attaches in the background: each session files under its own
    /// pane's workspace, and neither the screen nor the multiplexer's focus
    /// moves. Focusing a herdr pane clears its notification.
    pub(super) fn attach_every_multiplexer_pane(&mut self, ctx: &Context) {
        if !self.multiplexers.any_enabled() {
            self.modals.error_dialog = Some(self.multiplexers.disabled_reason().to_string());
            return;
        }
        let panes: Vec<(PaneKey, String, WorkspaceKey)> = self
            .pane_listing()
            .into_iter()
            .map(|listed| (listed.key, listed.pane.pane_id.clone(), listed.workspace))
            .collect();
        for (key, pane_id, workspace) in panes {
            // Naming the pane's own workspace as the one to restore makes
            // both arms of the restore no-ops, so a refusal cannot move a
            // user who navigated while the gesture was still running.
            let switch = WorkspaceSwitch { to: workspace.clone(), from: workspace };
            let unlisted = PaneTarget::unlisted(&key, &pane_id);
            self.attach_pane(ctx, key, unlisted, &switch, None, AttachFocus::Leave);
        }
    }

    /// End every session attached to a multiplexer pane.  The panes keep
    /// running and their rows come back unattached, so this destroys nothing.
    pub(super) fn detach_every_multiplexer_pane(&mut self, ctx: &Context) {
        if !self.multiplexers.any_enabled() {
            self.modals.error_dialog = Some(self.multiplexers.disabled_reason().to_string());
            return;
        }
        let ids = self.multiplexer_session_ids();
        if ids.is_empty() {
            return;
        }
        // `confirm_session_detach` governs one detach, and asking it per
        // session would put the same dialog in front of the user once per
        // row; the batch is one gesture and asks once.
        if self.config.ui.confirm_session_detach {
            self.modals.pending_detach_all = Some(ids);
        } else {
            self.detach_sessions(ctx, &ids);
        }
    }

    /// Every session holding a multiplexer pane, as ids: closing mutates
    /// `self.sessions`, so the set a detach acts on has to be taken before
    /// the first close rather than walked as it shrinks.
    pub(super) fn multiplexer_session_ids(&self) -> Vec<SessionId> {
        self.sessions.iter().filter(|s| s.pane_key.is_some()).map(|s| s.id).collect()
    }

    pub(super) fn detach_sessions(&mut self, ctx: &Context, ids: &[SessionId]) {
        for id in ids {
            self.close_session(ctx, *id);
        }
    }

    /// Ask a multiplexer for a pane and open a session on it once it answers.
    /// The two halves cannot be one call: the multiplexer is a process, and
    /// the pane an attach needs does not exist until it answers.
    pub(super) fn create_multiplexer_pane(
        &mut self,
        ctx: &Context,
        (multiplexer, side): (MultiplexerKind, Side),
        workspace: WorkspaceKey,
        waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
        focus: AttachFocus,
    ) {
        let cwd = match crate::multiplexer::cwd_for(&side, workspace.as_deref()) {
            Ok(cwd) => cwd,
            Err(e) => {
                self.refuse_multiplexer_request(waiter, e, focus);
                return;
            },
        };
        let request = CreateRequest { workspace, waiter, focus };
        self.multiplexers.get_mut(multiplexer).queue_create(side, cwd, request);
        ctx.request_repaint();
    }

    /// Adopt the creates the multiplexers have answered, handing each pane to
    /// the same attach a click takes.  The workspace is switched to first, so
    /// the session and any refusal are both readable where they were asked
    /// for.
    pub(super) fn poll_pane_creates(&mut self, ctx: &Context) {
        for at in 0..self.multiplexers.len() {
            let kind = self.multiplexers.kind_at(at);
            let Some(answer) = self.multiplexers.get_mut(kind).poll_create() else { continue };
            let request = answer.request;
            match answer.pane {
                Ok(pane) => {
                    // A new pane starts a shell, so nothing is in it for an
                    // agent registry to resolve until an agent starts there.
                    let unlisted = PaneTarget {
                        side: answer.side.clone(),
                        pane_id: pane.pane_id,
                        has_agent: false,
                    };
                    let key = self.multiplexers.get(kind).key(&answer.side, &pane.terminal_id);
                    let switch = self.switch_for_attach(&request.workspace, request.focus);
                    if !self.attach_pane(ctx, key, unlisted, &switch, request.waiter, request.focus)
                        && request.focus.takes()
                    {
                        self.current_workspace = switch.from;
                    }
                },
                Err(e) => self.refuse_multiplexer_request(request.waiter, e, request.focus),
            }
        }
    }

    /// Report an attach or create that failed to every client waiting on it,
    /// and to the user. A request that left focus reaches only its clients
    /// when one is still listening, since it asked not to interrupt the user.
    pub(super) fn refuse_multiplexer_request(
        &mut self,
        waiters: impl IntoIterator<Item = mpsc::Sender<ipc::protocol::IpcResult>>,
        message: String,
        focus: AttachFocus,
    ) {
        let mut answered = false;
        for waiter in waiters {
            answered |= waiter.send(Err(message.clone())).is_ok();
        }
        if focus.takes() || !answered {
            self.modals.error_dialog = Some(message);
        }
    }

    /// Adopt the shared-view attaches whose calls have landed.  Each session
    /// opens in the workspace its own click came from, which that click
    /// switched to before handing the gesture over.
    pub(super) fn poll_pane_attaches(&mut self, ctx: &Context) {
        for at in 0..self.multiplexers.len() {
            let kind = self.multiplexers.kind_at(at);
            let (answer, starting) = self.multiplexers.get_mut(kind).poll_attach();
            if starting {
                ctx.request_repaint();
            }
            let Some(answer) = answer else { continue };
            let request = answer.request;
            match answer.launch {
                Ok(launch) => {
                    // The open takes the workspace by value, so the arm keeps
                    // its own copy to judge the restore against afterwards.
                    let switched_to = request.workspace.clone();
                    let opened = self.open_pane_session(
                        ctx,
                        answer.key,
                        request.workspace,
                        launch,
                        true,
                        request.focus,
                    );
                    match opened {
                        Some(id) => {
                            for waiter in request.waiters {
                                self.park_attach_reply(id, Some(waiter));
                            }
                        },
                        None => {
                            self.restore_after_failed_attach(&switched_to, request.previous);
                            let message = self.modals.error_dialog.take().unwrap_or_default();
                            self.refuse_multiplexer_request(
                                request.waiters,
                                message,
                                request.focus,
                            );
                        },
                    }
                },
                Err(e) => {
                    self.restore_after_failed_attach(&request.workspace, request.previous);
                    self.refuse_multiplexer_request(request.waiters, e, request.focus);
                },
            }
        }
    }

    /// Switch to where an attach lands when it takes focus. An attach that
    /// leaves focus stays put, the same way the batch attach avoids moving
    /// the user.
    pub(super) fn switch_for_attach(
        &mut self,
        workspace: &WorkspaceKey,
        focus: AttachFocus,
    ) -> WorkspaceSwitch {
        let from = match focus {
            AttachFocus::Take => std::mem::replace(&mut self.current_workspace, workspace.clone()),
            AttachFocus::Leave => workspace.clone(),
        };
        WorkspaceSwitch { to: workspace.clone(), from }
    }

    pub(super) fn restore_after_failed_attach(
        &mut self,
        switched_to: &WorkspaceKey,
        previous: WorkspaceKey,
    ) {
        self.current_workspace =
            workspace_after_failed_attach(&self.current_workspace, switched_to, previous);
    }

    /// Open the session that runs an attach client.  A shared view starts on
    /// the pane the gesture just focused, so the multiplexer is already where
    /// the new session's row says it is and no second focus is owed.
    ///
    /// `shared_view` is the caller's to say, since it chose the client: the
    /// listing may no longer say what it said then, or may not carry the pane
    /// at all.
    pub(super) fn open_pane_session(
        &mut self,
        ctx: &Context,
        key: PaneKey,
        workspace: WorkspaceKey,
        launch: Launch,
        shared_view: bool,
        focus: AttachFocus,
    ) -> Option<SessionId> {
        let (argv, probe) = match multiplexer_attach_probe(&key.side, &launch.program, &launch.argv)
        {
            Some((wrapped, probe)) => (wrapped, Some(probe)),
            None => (launch.argv, None),
        };
        let shell = ShellCommand::new(launch.program, argv);
        let kept_tab = self
            .sessions
            .active(&workspace)
            .filter(|id| !focus.takes() && self.sessions.iter().any(|s| s.id == *id));
        match self.spawn_session_with_shell(ctx, workspace.clone(), Some(shell), probe) {
            Ok(id) => {
                if let Some(kept) = kept_tab {
                    self.sessions.set_active(workspace, kept);
                }
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                    session.bind_pane(key.clone(), shared_view);
                }
                if shared_view && focus.takes() {
                    self.multiplexers.get_mut(key.multiplexer).view_attached(id, &key);
                }
                Some(id)
            },
            Err(e) => {
                self.modals.error_dialog =
                    Some(format!("failed to attach {} agent: {e}", key.multiplexer));
                None
            },
        }
    }

    /// Local session switches focus the multiplexer; a settled view follows
    /// later focus changes made inside it.
    pub(super) fn sync_pane_views(&mut self, ctx: &Context) {
        let attentive = self.focus == PaneFocus::Terminal
            && !self.is_modal_open()
            && !self.palette.is_open()
            && ctx.input(|input| input.viewport().focused).unwrap_or(true);
        let active = self.active_session_index();
        let last_direct_input = self.last_direct_input;
        let sessions = &self.sessions;
        let is_open = |id: SessionId| sessions.iter().any(|session| session.id == id);
        let mut follow = None;
        let mut repaint = false;
        for multiplexer in self.multiplexers.iter_mut() {
            let kind = multiplexer.kind();
            let active = active.map(|index| {
                let session = &sessions[index];
                let owned = session.pane_key.as_ref().filter(|key| key.multiplexer == kind);
                (session.id, owned, session.shared_view)
            });
            let step = multiplexer.sync_view(ViewState {
                active,
                attentive,
                now: Instant::now(),
                last_direct_input,
                is_open: &is_open,
            });
            repaint |= step.repaint;
            follow = follow.or(step.follow);
        }
        if repaint {
            ctx.request_repaint();
        }
        if let Some(key) = follow {
            self.follow_pane_view(ctx, key);
        }
    }

    /// Close the sessions whose pane the multiplexer has since reported gone.
    pub(super) fn reconcile_pane_sessions(&mut self, ctx: &Context) {
        let mut index = 0;
        while index < self.sessions.len() {
            let session = &self.sessions[index];
            let evidence =
                session.pane_key.as_ref().zip(session.pane_bound_at).and_then(|(key, bound_at)| {
                    let multiplexer = self.multiplexers.get(key.multiplexer);
                    multiplexer
                        .enabled()
                        .then(|| multiplexer.gone_since(&key.side, &key.terminal_id, bound_at))
                        .flatten()
                        .map(|sampled_at| (key, bound_at, sampled_at))
                });
            if let Some((key, bound_at, sampled_at)) = evidence {
                let id = session.id;
                log::debug!(
                    "pane removal session={id} multiplexer={} side={:?} terminal_id={} \
                     bound_at={bound_at:?} sampled_at={sampled_at:?}",
                    key.multiplexer,
                    key.side,
                    key.terminal_id
                );
                self.close_session(ctx, id);
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn follow_pane_view(&mut self, ctx: &Context, key: PaneKey) {
        let Some(id) = self.pane_follow_target(ctx, &key) else {
            // The multiplexer keeps reporting this pane as focused, so a
            // target the app cannot reach is proposed again on every poll
            // until the refusal is on the trail.
            self.multiplexers.get_mut(key.multiplexer).view_refused(&key);
            return;
        };
        self.activate_session_by_id(id);
        self.reveal_search_row(&SidebarRow::Session(id));
        self.sidebar.model.set_cursor(SidebarRow::Session(id));
        self.focus_terminal();
        self.multiplexers.get_mut(key.multiplexer).view_attached(id, &key);
    }

    /// The session showing `key`, opening one if the row is attachable.
    pub(super) fn pane_follow_target(&mut self, ctx: &Context, key: &PaneKey) -> Option<SessionId> {
        if let Some(id) = self.pane_session(key) {
            return Some(id);
        }
        let workspace = self.pane_row_workspace(key)?;
        let shared_view = !self.pane_attaches_directly(key);
        let multiplexer = self.multiplexers.get(key.multiplexer);
        let launch = if shared_view {
            multiplexer.shared_view(key)?
        } else {
            let pane = multiplexer.find(&key.side, &key.terminal_id)?;
            // The branch already asked the question the multiplexer answers
            // here, so the `None` is unreachable; not following the pane is
            // the right answer anyway if the two ever disagree.
            multiplexer.open_directly(&pane.target(&key.side))?
        };
        self.open_pane_session(
            ctx,
            key.clone(),
            workspace,
            launch,
            shared_view,
            AttachFocus::Take,
        )?;
        self.pane_session(key)
    }

    /// Every listed pane no session holds, with the workspace it belongs
    /// under.  The sidebar and the palette both read this, so a pane hidden
    /// from one is hidden from the other by construction.
    pub(super) fn pane_listing(&self) -> Vec<ListedPane<'_>> {
        let claimed: Vec<PaneKey> =
            self.sessions.iter().filter_map(|s| s.pane_key.clone()).collect();
        let workspaces = pane_workspaces(&self.projects, |path| self.liveness.missing(path));
        self.multiplexers.listed(&claimed, &workspaces)
    }

    /// The workspace a pane's row is currently listed under, for the keyboard
    /// path: `SidebarRow::Pane` itself carries no workspace, unlike a click,
    /// which already knows which panel section it landed in.
    pub(super) fn pane_row_workspace(&self, key: &PaneKey) -> Option<WorkspaceKey> {
        let wanted = sidebar_nav::WorkspaceEntry::Pane(key.clone());
        self.listed_workspace_rows()
            .into_iter()
            .find(|(_, entries)| entries.contains(&wanted))
            .map(|(ws, _)| ws)
    }

    /// The pane `key` names, if its multiplexer still lists it.  A stale key
    /// (the pane closed between poll and paint, or an Enter that outraced
    /// this frame's own listing) yields no row rather than a panic; the next
    /// poll drops it from the listing for good.
    pub(super) fn find_pane(&self, key: &PaneKey) -> Option<&Pane> {
        self.multiplexers.find(key)
    }

    /// The pane this session is attached to, while its multiplexer still
    /// lists it.  The multiplexer watches the pane from outside, so it is the
    /// authority on both what the pane is called and what it is doing.
    pub(super) fn session_pane(&self, session: &AppSession) -> Option<&Pane> {
        self.find_pane(session.pane_key.as_ref()?)
    }

    pub(super) fn session_pane_status(&self, session: &AppSession) -> Option<PaneStatus> {
        self.session_pane(session).and_then(|pane| pane.status)
    }

    /// Whether the multiplexer reports an agent in the pane `key` names.  A
    /// pane the listing no longer carries answers true, as does a session
    /// that holds no pane: with nothing to read, the agent-registry answer is
    /// the one that keeps every caller on the path it took before the pane
    /// went.
    pub(super) fn pane_has_agent(&self, key: Option<&PaneKey>) -> bool {
        key.and_then(|key| self.find_pane(key)).is_none_or(|pane| pane.status.is_some())
    }

    /// Whether opening this pane's row attaches to the pane on its own.
    pub(super) fn pane_attaches_directly(&self, key: &PaneKey) -> bool {
        self.multiplexers
            .get(key.multiplexer)
            .attaches_directly(&key.side, self.pane_has_agent(Some(key)))
    }

    /// Where a multiplexer-backed session lives, looked up from the key the
    /// session carries.
    pub(super) fn session_multiplexer_json(&self, key: &PaneKey) -> Value {
        self.multiplexers.get(key.multiplexer).pane_json(
            &key.side,
            &key.terminal_id,
            self.find_pane(key),
        )
    }

    /// Every pane each multiplexer reports, on every side, attached or not.
    /// Unlike the sidebar this hides nothing: `show_unmatched` decides what
    /// is worth drawing, and a caller naming a pane by its id is not
    /// browsing.
    pub(super) fn multiplexer_panes_json(&self) -> Value {
        let workspaces = pane_workspaces(&self.projects, |path| self.liveness.missing(path));
        let mut panes = Vec::new();
        for multiplexer in self.multiplexers.iter().filter(|m| m.enabled()) {
            for (side, pane) in multiplexer.panes() {
                let key = multiplexer.key(side, &pane.terminal_id);
                panes.push(json!({
                    "multiplexer": multiplexer.pane_json(side, &pane.terminal_id, Some(pane)),
                    "kind": pane.kind,
                    "title": pane.title,
                    "status": pane.status.map(|status| status.label()),
                    "focused": pane.focused,
                    "workspace": pane.workspace(side, &workspaces),
                    "session_id": self.pane_session(&key),
                }));
            }
        }
        json!({ "panes": panes })
    }

    /// The session already attached to this pane, if one is open.
    pub(super) fn pane_session(&self, key: &PaneKey) -> Option<SessionId> {
        self.sessions.iter().find(|s| s.pane_key.as_ref() == Some(key)).map(|s| s.id)
    }

    /// How a listed pane's row describes it.
    pub(super) fn pane_managed(&self, key: &PaneKey, pane: &Pane) -> Managed {
        self.multiplexers.get(key.multiplexer).managed(&key.side, Some(pane))
    }

    /// What supervises `session`, when anything does.  Derived per frame
    /// rather than stored, so a config read that lands later, or a
    /// multiplexer that stops listing the pane, reaches the row without a
    /// second source of truth to keep in step.
    pub(super) fn session_managed(&self, session: &AppSession) -> Option<Managed> {
        let key = session.pane_key.as_ref()?;
        let mut managed =
            self.multiplexers.get(key.multiplexer).managed(&key.side, self.session_pane(session));
        // The listing answers what opening the pane now would give; this
        // session's client was settled when it attached.
        managed.shared_view = session.shared_view;
        Some(managed)
    }

    /// One number standing for every multiplexer's rendered state, so the
    /// sidebar's per-frame comparison stays a `u64` compare.
    pub(super) fn panes_generation(&self) -> u64 {
        self.multiplexers.generation()
    }

    /// Refreshes each multiplexer's listing on its own clock.
    pub(super) fn poll_multiplexers(&mut self) {
        let sessions = &self.sessions;
        for multiplexer in self.multiplexers.iter_mut() {
            let kind = multiplexer.kind();
            multiplexer.poll(&|side| {
                sessions.iter().any(|session| {
                    session
                        .pane_key
                        .as_ref()
                        .is_some_and(|key| key.multiplexer == kind && &key.side == side)
                })
            });
        }
    }

    /// Attach to a pane the way its sidebar row does, holding the reply until
    /// the session behind it can be read.  A refusal names what went wrong
    /// precisely enough to act on: a side that names no server, a pane no
    /// multiplexer is reporting, and an integration that is switched off are
    /// three different situations, and only the last is worth retrying after
    /// a config change.  `multiplexer` narrows the search to the one named.
    pub(super) fn defer_attach_multiplexer_pane(
        &mut self,
        ctx: &Context,
        (multiplexer, side, terminal_id): (Option<&str>, &str, &str),
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
        focus: AttachFocus,
    ) {
        let only = match self.multiplexers.requested(multiplexer) {
            Ok(only) => only,
            Err(e) => {
                let _ = reply_tx.send(Err(e));
                return;
            },
        };
        let Some(parsed_side) = Side::parse(side) else {
            let _ = reply_tx.send(Err(not_a_side(side)));
            return;
        };
        let Some((key, pane)) = self.multiplexers.locate(only, &parsed_side, terminal_id) else {
            let _ = reply_tx.send(Err(format!(
                "no pane `{terminal_id}` on {side}, see list_multiplexer_panes"
            )));
            return;
        };
        let pane_id = pane.pane_id.clone();
        let workspaces = pane_workspaces(&self.projects, |path| self.liveness.missing(path));
        let workspace = pane.workspace(&parsed_side, &workspaces);

        let switch = self.switch_for_attach(&workspace, focus);
        let unlisted = PaneTarget::unlisted(&key, &pane_id);
        if !self.attach_pane(ctx, key, unlisted, &switch, Some(reply_tx), focus) && focus.takes() {
            self.current_workspace = switch.from;
        }
    }

    /// Create a pane and open a session on it.  An omitted side is the one
    /// the active session's own pane belongs to, since a user asking for
    /// another pane while looking at one means another like it; with no
    /// multiplexer-backed session in front of them there is no such answer,
    /// so a machine reaching more than one server has to say which.
    pub(super) fn defer_create_multiplexer_pane(
        &mut self,
        ctx: &Context,
        (multiplexer, side): (Option<&str>, Option<&str>),
        workspace: Option<PathBuf>,
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
        focus: AttachFocus,
    ) {
        let only = match self.multiplexers.requested(multiplexer) {
            Ok(only) => only,
            Err(e) => {
                let _ = reply_tx.send(Err(e));
                return;
            },
        };
        let named = match side {
            Some(name) => match Side::parse(name) {
                Some(side) => Some(side),
                None => {
                    let _ = reply_tx.send(Err(not_a_side(name)));
                    return;
                },
            },
            None => None,
        };
        let target = match self.create_target(only, named) {
            Ok(target) => target,
            Err(e) => {
                let _ = reply_tx.send(Err(e));
                return;
            },
        };
        // Resolved before the multiplexer is asked, so a path naming no
        // worktree never leaves a pane behind in it.
        let workspace = match workspace {
            None => self.current_workspace.clone(),
            Some(p) => match self.known_worktree_path(&p) {
                Some(known) => Some(known),
                None => {
                    let _ = reply_tx.send(Err(unknown_worktree(&p)));
                    return;
                },
            },
        };
        self.create_multiplexer_pane(ctx, target, workspace, Some(reply_tx), focus);
    }

    /// Where a create lands: the multiplexer and side the active session's
    /// own pane belongs to, and failing that the first multiplexer enabled,
    /// on the one side a server is answering on.  `only` holds both to the
    /// multiplexer a caller named.  `Err` names every side it could have
    /// meant, so a caller can retry saying which.
    pub(super) fn create_target(
        &self,
        only: Option<MultiplexerKind>,
        named: Option<Side>,
    ) -> Result<(MultiplexerKind, Side), String> {
        let focused = self
            .active_session_index()
            .and_then(|idx| self.sessions[idx].pane_key.as_ref())
            .filter(|key| self.multiplexers.get(key.multiplexer).enabled())
            .filter(|key| only.is_none_or(|kind| kind == key.multiplexer));
        if let Some(key) = focused {
            return Ok((key.multiplexer, named.unwrap_or_else(|| key.side.clone())));
        }
        let default = match only {
            Some(kind) => Some(self.multiplexers.get(kind)),
            None => self.multiplexers.default_enabled(),
        };
        let Some(multiplexer) = default else {
            return Err(self.multiplexers.disabled_reason().to_string());
        };
        let side = match named {
            Some(side) => side,
            None => multiplexer.default_side()?,
        };
        Ok((multiplexer.kind(), side))
    }
}

impl Action for action::NewMultiplexerPane {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        if !app.multiplexers.any_enabled() {
            app.modals.error_dialog = Some(app.multiplexers.disabled_reason().to_string());
            return;
        }
        match app.create_target(None, None) {
            Ok(target) => {
                let workspace = app.current_workspace.clone();
                app.create_multiplexer_pane(ctx, target, workspace, None, AttachFocus::Take);
            },
            Err(e) => app.modals.error_dialog = Some(e),
        }
    }
}

impl Action for action::AttachAllMultiplexerPanes {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.attach_every_multiplexer_pane(ctx);
    }
}

impl Action for action::DetachAllMultiplexerPanes {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.detach_every_multiplexer_pane(ctx);
    }
}

/// Where the user lands when an attach fails after switching them.  The job
/// answers frames later, so a switch made in between is theirs and outranks
/// the restore: `previous` is handed back only while `current` is still the
/// workspace the attach moved them to.
pub(super) fn workspace_after_failed_attach(
    current: &WorkspaceKey,
    switched_to: &WorkspaceKey,
    previous: WorkspaceKey,
) -> WorkspaceKey {
    if current == switched_to { previous } else { current.clone() }
}

/// The workspaces a pane may be matched against.  A checkout that looks gone
/// offers none, which is what makes a pane working there unmatched rather
/// than parked under a row that can only refuse it.  `missing` is the
/// liveness cache's word for a path, `None` where it has none, so the row's
/// grey and this list agree about the same directory.
pub(super) fn pane_workspaces(
    projects: &[Project],
    missing: impl Fn(&Path) -> Option<bool>,
) -> Vec<PathBuf> {
    projects
        .iter()
        .flat_map(|p| p.worktrees.iter())
        .filter(|wt| !worktree_looks_gone(wt, missing(&wt.path)))
        .map(|wt| wt.path.clone())
        .collect()
}

/// What a multiplexer-managed row explains on hover, one fact per comma: the
/// state, since that is what changes; who reports it; whether the attach is
/// the multiplexer's whole view; and what it calls the pane.  The way out
/// follows in parentheses, since it is an instruction rather than another
/// fact about the pane.
///
/// The same sentence serves a listed pane and an attached one.  The chord has
/// no other surface in alacritree, since it is the multiplexer's key and not
/// one of ours, so it has to reach the row the user is sitting in.
pub(super) fn managed_tooltip(managed: &Managed) -> String {
    let mut parts = Vec::new();
    if let Some(status) = managed.status {
        parts.push(status.label().to_owned());
    }
    parts.push(managed.multiplexer.to_string());
    if managed.shared_view {
        parts.push("shared view".to_owned());
    }
    parts.extend(managed.pane_name());
    let mut hint = parts.join(", ");
    hint.push('.');
    if let Some(chord) = &managed.detach {
        // Backquoted because the chord is a sequence, not one combination:
        // unquoted, "detach with Ctrl+B q" reads as a sentence whose last
        // word happens to be `q`.
        hint.push_str(&format!(" (detach with `{chord}`)"));
    }
    hint
}

/// The workspace switch an attach makes before anything can refuse it: its
/// session opens in `to`, and a refusal hands the user back to `from`. An
/// attach that leaves focus switches nowhere, so both name the same
/// workspace and every restore after it is a no-op.
pub(super) struct WorkspaceSwitch {
    pub(super) to: WorkspaceKey,
    pub(super) from: WorkspaceKey,
}

/// A name that reaches no server, as opposed to one whose server is down: a
/// caller retrying this one is retrying a typo.
pub(super) fn not_a_side(name: &str) -> String {
    format!("`{name}` is not a side, expected `native` or `wsl:<distro>`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::listed_agent;

    /// The click switched workspace before handing the gesture over, so a
    /// failure puts the user back where the click found them.
    #[test]
    fn a_failed_attach_hands_back_the_workspace_it_switched_from() {
        let switched_to = Some(PathBuf::from("/code/wt"));
        let previous = Some(PathBuf::from("/code/other"));
        assert_eq!(
            workspace_after_failed_attach(&switched_to, &switched_to, previous.clone()),
            previous
        );
    }

    /// The home tab is a workspace like any other, so an attach launched from
    /// it is restored to it rather than read as nothing to go back to.
    #[test]
    fn a_failed_attach_restores_the_home_tab() {
        let switched_to = Some(PathBuf::from("/code/wt"));
        assert_eq!(workspace_after_failed_attach(&switched_to, &switched_to, None), None);
    }

    /// The multiplexer answers frames after the click, and a switch made in
    /// between is the user's own: restoring over it would pull them out of a
    /// workspace they chose.
    #[test]
    fn a_failed_attach_leaves_a_workspace_the_user_moved_to_alone() {
        let current = Some(PathBuf::from("/code/elsewhere"));
        let switched_to = Some(PathBuf::from("/code/wt"));
        assert_eq!(workspace_after_failed_attach(&current, &switched_to, None), current);
    }

    /// A session alacritree still holds open after its multiplexer stopped
    /// listing the pane has no state and no name left to report, but it is
    /// still the multiplexer's and the user still has to know how to leave.
    #[test]
    fn an_unlisted_pane_still_says_how_to_leave() {
        let managed = Managed {
            multiplexer: MultiplexerKind::Herdr,
            detach: Some("Ctrl+B q".into()),
            shared_view: false,
            kind: None,
            title: None,
            status: None,
        };
        assert_eq!(managed_tooltip(&managed), "herdr. (detach with `Ctrl+B q`)");
    }

    /// A checkout the liveness cache calls gone offers no workspace, so the
    /// pane working in it matches nothing and lists under Home.  Matched to
    /// the removed worktree instead, its row's Enter could only refuse.
    #[test]
    fn a_gone_worktree_offers_no_workspace_to_a_pane() {
        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"])];
        let gone = PathBuf::from("/a/wt2");
        let workspaces = pane_workspaces(&projects, |path| Some(path == gone));
        assert_eq!(workspaces, vec![PathBuf::from("/a/wt1")]);

        let pane = Pane { cwd: Some(gone.to_string_lossy().into_owned()), ..listed_agent(None) };
        let matched = pane.workspace(&Side::Native, &workspaces);
        assert_eq!(matched, None, "a pane under a removed checkout falls back to Home");
    }
}
