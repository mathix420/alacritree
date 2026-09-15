use super::*;
use crate::config::AttachMode;
use crate::multiplexer::{CreatedPane, Launch};

pub(super) struct HerdrGlue {
    /// Shared-view attaches whose herdr calls are running on the pool,
    /// adopted in `poll_herdr_attach`.
    pub(super) pending_attach: Vec<PendingHerdrAttach>,
    /// Pane creates running on the pool, adopted in `poll_herdr_create`.
    pub(super) pending_create: Vec<PendingHerdrCreate>,
    /// The shared view herdr was last focused for, and the call still on its
    /// way, both owned by `sync_herdr_view_focus`.
    pub(super) focused_view: herdr::HerdrViewSync,
    pub(super) view_focus: Option<herdr::HerdrViewFocus>,
    /// The herdr servers this app talks to: the native side plus one per
    /// running WSL distro, kept in step with which distros are up.
    pub(super) endpoints: herdr::Endpoints,
}

impl HerdrGlue {
    pub(super) fn new() -> Self {
        Self {
            pending_attach: Vec::new(),
            pending_create: Vec::new(),
            focused_view: herdr::HerdrViewSync::default(),
            view_focus: None,
            endpoints: herdr::Endpoints::default(),
        }
    }

    pub(super) fn close_session(&mut self, id: SessionId, herdr_key: Option<&herdr::HerdrKey>) {
        if let Some(key) = herdr_key {
            // A plain `retain` would drop a queued attach's waiters with it,
            // leaving a parked client to time out rather than learn why.
            let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_attach)
                .into_iter()
                .partition(|pending| &pending.key == key);
            self.pending_attach = kept;
            for pending in removed {
                for waiter in pending.waiters {
                    let _ = waiter.send(Err("the session behind this pane was closed before the \
                                             attach finished"
                        .to_string()));
                }
            }
        }
        if self.view_focus.as_ref().is_some_and(|pending| pending.session == id) {
            self.view_focus = None;
        }
        self.focused_view.closed(id, herdr_key, Instant::now());
    }
}

impl AlacritreeApp {
    /// Opens a herdr agent in a session running herdr's attach client.  The
    /// session is an ordinary shell, so nothing in the grid or input path
    /// treats it specially; only the key marks it as this agent's row.
    /// Returns whether the attach succeeded so the caller can switch
    /// `current_workspace` to `workspace` first and restore it on failure —
    /// the same replace-and-restore shape `spawn_shell_request` uses, needed
    /// here for the same reason: a refusal is only readable in the
    /// workspace it happened in.  `unlisted` stands in for a pane the
    /// listing does not carry.
    pub(super) fn attach_herdr_agent(
        &mut self,
        ctx: &Context,
        key: herdr::HerdrKey,
        unlisted: PaneTarget,
        workspace: WorkspaceKey,
        previous: WorkspaceKey,
        waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
    ) -> bool {
        if let Some(id) = self.herdr_session_for(&key) {
            self.activate_session_by_id(id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(Ok(json!({ "session_id": id })));
            }
            return true;
        }
        let target = self
            .find_herdr_agent(&key.side, &key.terminal_id)
            .map_or(unlisted, |agent| agent.target(&key.side));
        let multiplexer = Multiplexer::owning(&key);
        let attach = self.config.integrations.herdr.attach;
        if let Some(launch) = multiplexer.open_multiplexer_session(&target, attach) {
            // Nothing to ask herdr first: the pane id is the whole target,
            // and the client attaches to it directly.
            let opened =
                self.open_herdr_session(ctx, key, workspace, launch.program, launch.argv, false);
            return match opened {
                Some(id) => {
                    self.park_attach_reply(id, waiter);
                    true
                },
                None => {
                    if let Some(waiter) = waiter {
                        let message = self
                            .modals
                            .error_dialog
                            .clone()
                            .unwrap_or_else(|| "failed to attach the pane".to_string());
                        let _ = waiter.send(Err(message));
                    }
                    false
                },
            };
        }

        // Every one of herdr's app clients draws the same focused pane, so a
        // shared view shows a row's pane only while herdr is focused there.
        // The attach focuses it so the first frame is already right, and
        // `sync_herdr_view_focus` focuses it again whenever the session comes
        // back up, which is what lets a side hold one session per row.
        if let Some(pending) = self.herdr.pending_attach.iter_mut().find(|p| p.key == key) {
            pending.waiters.extend(waiter);
            return true;
        }
        // The gesture is two herdr processes whatever `async_session_spawn`
        // says, and running them from the click would hold the frame for as
        // long as herdr takes to answer.
        self.herdr.pending_attach.push(PendingHerdrAttach {
            job: None,
            target,
            key,
            workspace,
            previous,
            waiters: waiter.into_iter().collect(),
        });
        ctx.request_repaint();
        true
    }

    /// Answer an attach once the session's PTY is live.  A client that
    /// attached in order to read the pane would otherwise be handed an id
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
    /// The batch was asked for the whole set rather than for one pane, so it
    /// switches to none of them: each session files under the workspace its
    /// own pane belongs to and the workspace on screen is left alone.
    pub(super) fn attach_every_multiplexer_pane(&mut self, ctx: &Context) {
        if !self.config.integrations.herdr.enabled {
            self.modals.error_dialog = Some(HERDR_DISABLED.to_string());
            return;
        }
        let panes: Vec<(herdr::HerdrKey, String, WorkspaceKey)> = self
            .herdr_agent_listing()
            .into_iter()
            .map(|(workspace, side, agent)| {
                let key =
                    herdr::HerdrKey { side: side.clone(), terminal_id: agent.terminal_id.clone() };
                (key, agent.pane_id.clone(), workspace)
            })
            .collect();
        for (key, pane_id, workspace) in panes {
            // Naming the pane's own workspace as the one to restore makes
            // both arms of the restore no-ops, so a refusal cannot move a
            // user who navigated while the gesture was still running.
            let previous = workspace.clone();
            let unlisted = unlisted_pane_target(&key, &pane_id);
            self.attach_herdr_agent(ctx, key, unlisted, workspace, previous, None);
        }
    }

    /// End every session attached to a multiplexer pane.  The panes keep
    /// running and their rows come back unattached, so this destroys nothing.
    pub(super) fn detach_every_multiplexer_pane(&mut self, ctx: &Context) {
        if !self.config.integrations.herdr.enabled {
            self.modals.error_dialog = Some(HERDR_DISABLED.to_string());
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
        self.sessions.iter().filter(|s| s.herdr_key.is_some()).map(|s| s.id).collect()
    }

    pub(super) fn detach_sessions(&mut self, ctx: &Context, ids: &[SessionId]) {
        for id in ids {
            self.close_session(ctx, *id);
        }
    }

    /// Ask the multiplexer for a pane and open a session on it once it
    /// answers.  The two halves cannot be one call: herdr is a process, and
    /// the pane an attach needs does not exist until it answers.
    pub(super) fn create_multiplexer_pane(
        &mut self,
        ctx: &Context,
        side: herdr::Side,
        workspace: WorkspaceKey,
        waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
    ) {
        let cwd = match multiplexer_cwd(&side, workspace.as_deref()) {
            Ok(cwd) => cwd,
            Err(e) => {
                self.refuse_herdr_create(waiter, e);
                return;
            },
        };
        // `Multiplexer::owning` resolves a multiplexer from a pane's key,
        // and a pane nothing has made yet has no key, so the create names the
        // one it is asking.
        let multiplexer = Multiplexer::from(Herdr);
        let asked = side.clone();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
            multiplexer.create_pane(&asked, cwd)
        });
        self.herdr.pending_create.push(PendingHerdrCreate { job, side, workspace, waiter });
        ctx.request_repaint();
    }

    /// Adopt the creates herdr has answered, handing each pane to the same
    /// attach a click takes.  The workspace is switched to first, so the
    /// session and any refusal are both readable where they were asked for.
    pub(super) fn poll_herdr_create(&mut self, ctx: &Context) {
        if self.herdr.pending_create.is_empty() {
            return;
        }
        let pending = self.herdr.pending_create.remove(0);
        match pending.job.poll() {
            Some(Ok(pane)) => {
                // `tab create` starts a shell, so nothing is in the pane for
                // herdr's agent registry to resolve until an agent starts
                // there, and until then the tab is its only handle.
                let unlisted = PaneTarget {
                    side: pending.side.clone(),
                    pane_id: pane.pane_id,
                    tab_id: Some(pane.tab_id),
                    has_agent: false,
                };
                let key = herdr::HerdrKey { side: pending.side, terminal_id: pane.terminal_id };
                let previous =
                    std::mem::replace(&mut self.current_workspace, pending.workspace.clone());
                if !self.attach_herdr_agent(
                    ctx,
                    key,
                    unlisted,
                    pending.workspace,
                    previous.clone(),
                    pending.waiter,
                ) {
                    self.current_workspace = previous;
                }
            },
            Some(Err(e)) => self.refuse_herdr_create(pending.waiter, e),
            None if pending.job.failed() => {
                self.refuse_herdr_create(
                    pending.waiter,
                    "the herdr pane create did not finish".to_string(),
                );
            },
            None => self.herdr.pending_create.insert(0, pending),
        }
    }

    /// Report a create that made no pane, whether the multiplexer refused it
    /// or it was refused before the multiplexer was asked.  Nothing has
    /// switched workspace yet, since that waits for the pane to land, so a
    /// refusal leaves the user where they are and only has to be readable.
    pub(super) fn refuse_herdr_create(
        &mut self,
        waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
        message: String,
    ) {
        if let Some(waiter) = waiter {
            let _ = waiter.send(Err(message.clone()));
        }
        self.modals.error_dialog = Some(message);
    }

    /// Adopt the shared-view attaches whose herdr calls have landed.  Each
    /// session opens in the workspace its own click came from, which that
    /// click switched to before handing the gesture over.
    pub(super) fn poll_herdr_attach(&mut self, ctx: &Context) {
        if self.herdr.view_focus.is_some() || self.herdr.pending_attach.is_empty() {
            return;
        }
        // Attach gestures and session switches both change herdr's global
        // focus, so only one may be in flight.
        let mut pending = self.herdr.pending_attach.remove(0);
        if let Some(job) = &pending.job {
            match job.poll() {
                Some(Ok(launch)) => {
                    // The open takes the workspace by value, so the arm keeps
                    // its own copy to judge the restore against afterwards.
                    let switched_to = pending.workspace.clone();
                    let waiters = std::mem::take(&mut pending.waiters);
                    match self.open_herdr_session(
                        ctx,
                        pending.key,
                        pending.workspace,
                        launch.program,
                        launch.argv,
                        true,
                    ) {
                        Some(id) => {
                            for waiter in waiters {
                                self.park_attach_reply(id, Some(waiter));
                            }
                        },
                        None => {
                            self.restore_after_failed_attach(&switched_to, pending.previous);
                            let message = self.modals.error_dialog.clone().unwrap_or_default();
                            for waiter in waiters {
                                let _ = waiter.send(Err(message.clone()));
                            }
                        },
                    }
                },
                Some(Err(e)) => {
                    self.restore_after_failed_attach(&pending.workspace, pending.previous);
                    for waiter in std::mem::take(&mut pending.waiters) {
                        let _ = waiter.send(Err(e.clone()));
                    }
                    self.modals.error_dialog = Some(e);
                },
                None if job.failed() => {
                    self.restore_after_failed_attach(&pending.workspace, pending.previous);
                    let message = "the herdr attach did not finish".to_string();
                    for waiter in std::mem::take(&mut pending.waiters) {
                        let _ = waiter.send(Err(message.clone()));
                    }
                    self.modals.error_dialog = Some(message);
                },
                None => self.herdr.pending_attach.insert(0, pending),
            }
        } else {
            let name = self.herdr_session_name(&pending.key.side);
            let side = pending.key.side.clone();
            let target = self
                .find_herdr_agent(&side, &pending.key.terminal_id)
                .map_or_else(|| pending.target.clone(), |agent| agent.target(&side));
            let multiplexer = Multiplexer::owning(&pending.key);
            pending.job = Some(jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
                multiplexer.shared_view_gesture(&target, name)
            }));
            self.herdr.pending_attach.insert(0, pending);
        }
        if self.herdr.pending_attach.first().is_some_and(|pending| pending.job.is_none()) {
            ctx.request_repaint();
        }
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
    /// the pane the gesture just focused, so herdr is already where the new
    /// session's row says it is and no second focus is owed.
    ///
    /// `shared_view` is the caller's to say, since it chose the client: the
    /// listing may no longer say what it said then, or may not carry the pane
    /// at all.
    pub(super) fn open_herdr_session(
        &mut self,
        ctx: &Context,
        key: herdr::HerdrKey,
        workspace: WorkspaceKey,
        program: String,
        argv: Vec<String>,
        shared_view: bool,
    ) -> Option<SessionId> {
        let (argv, probe) = match herdr_attach_probe(&key.side, &program, &argv) {
            Some((wrapped, probe)) => (wrapped, Some(probe)),
            None => (argv, None),
        };
        // `alacritty_terminal::tty::Shell`'s fields are crate-private, so
        // this goes through the constructor rather than a struct literal.
        let shell = Shell::new(program, argv);
        match self.spawn_session_with_shell(ctx, workspace, Some(shell), probe) {
            Ok(id) => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                    session.bind_herdr(key.clone(), shared_view);
                }
                if shared_view {
                    self.herdr.focused_view.attached(id, Some(&key), Instant::now());
                }
                Some(id)
            },
            Err(e) => {
                self.modals.error_dialog = Some(format!("failed to attach herdr agent: {e}"));
                None
            },
        }
    }

    /// Local session switches focus herdr; a settled view follows later
    /// focus changes made inside herdr. Listings started before our focus
    /// landed cannot reverse the user's session selection.
    pub(super) fn sync_herdr_view_focus(&mut self, ctx: &Context) {
        let active = self.active_session_index().map(|index| &self.sessions[index]);
        let key = active.and_then(|session| session.herdr_key.clone());
        let selection = active.map(|session| (session.id, key.as_ref(), session.herdr_shared_view));
        let attentive = self.focus == PaneFocus::Terminal
            && !self.is_modal_open()
            && !self.palette.is_open()
            && ctx.input(|input| input.viewport().focused).unwrap_or(true);
        let action = self.herdr.focused_view.next(herdr::ViewInputs {
            active: selection,
            follow: self.config.integrations.herdr.follow_focus,
            caches: self.herdr.endpoints.caches(),
            attentive,
            busy: self.herdr.view_focus.is_some() || !self.herdr.pending_attach.is_empty(),
            now: Instant::now(),
            last_direct_input: self.last_direct_input,
        });
        if let Some(pending) = self.herdr.view_focus.take() {
            if !self.sessions.iter().any(|session| session.id == pending.session) {
                ctx.request_repaint();
                return;
            }
            match pending.job.poll() {
                Some(result) => {
                    let succeeded = result.is_ok();
                    if let Err(e) = result {
                        log::warn!("{e}");
                    }
                    if succeeded {
                        self.herdr.focused_view.moved_focus(&pending.key, Instant::now());
                    }
                    self.herdr.focused_view.settled(pending.session, succeeded, Instant::now());
                },
                None if pending.job.failed() => {
                    self.herdr.focused_view.settled(pending.session, false, Instant::now());
                },
                None => self.herdr.view_focus = Some(pending),
            }
            if self.herdr.view_focus.is_none() {
                ctx.request_repaint();
            }
            return;
        }
        match action {
            Some(herdr::HerdrViewAction::Focus(id)) => {
                let Some(key) = key else { return };
                let multiplexer = Multiplexer::owning(&key);
                let Some(focus) =
                    self.herdr_focus_target(&key).map(|target| multiplexer.focus_args(&target))
                else {
                    return;
                };
                let stamp = key.clone();
                let job = jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
                    multiplexer.focus_pane(&key.side, &focus)
                });
                self.herdr.view_focus =
                    Some(herdr::HerdrViewFocus { session: id, key: stamp, job });
            },
            Some(herdr::HerdrViewAction::Follow(key)) => self.follow_herdr_view(ctx, key),
            None => {},
        }
    }

    pub(super) fn reconcile_herdr_sessions(&mut self, ctx: &Context) {
        if !self.config.integrations.herdr.enabled {
            return;
        }
        let mut index = 0;
        while index < self.sessions.len() {
            let session = &self.sessions[index];
            let evidence = session.herdr_key.as_ref().zip(session.herdr_bound_at).and_then(
                |(key, bound_at)| {
                    self.herdr
                        .endpoints
                        .caches()
                        .iter()
                        .find(|cache| cache.side() == &key.side)
                        .and_then(herdr::EndpointCache::inventory)
                        .filter(|inventory| {
                            inventory.sampled_at > bound_at
                                && !inventory.terminal_ids.contains(&key.terminal_id)
                        })
                        .map(|inventory| (key, bound_at, inventory.sampled_at))
                },
            );
            if let Some((key, bound_at, sampled_at)) = evidence {
                let id = session.id;
                log::debug!(
                    "herdr removal session={id} side={:?} terminal_id={} bound_at={bound_at:?} \
                     sampled_at={sampled_at:?}",
                    key.side,
                    key.terminal_id
                );
                self.close_session(ctx, id);
            } else {
                index += 1;
            }
        }
    }

    pub(super) fn follow_herdr_view(&mut self, ctx: &Context, key: herdr::HerdrKey) {
        let Some(id) = self.herdr_follow_target(ctx, &key) else {
            // herdr keeps reporting this pane as focused, so a target the app
            // cannot reach is proposed again on every poll until the refusal
            // is on the trail.
            self.herdr.focused_view.moved_focus(&key, Instant::now());
            return;
        };
        self.activate_session_by_id(id);
        self.reveal_search_row(&SidebarRow::Session(id));
        self.set_sidebar_cursor(SidebarRow::Session(id));
        self.focus_terminal();
        self.herdr.focused_view.attached(id, Some(&key), Instant::now());
    }

    /// The session showing `key`, opening one if the row is attachable.
    pub(super) fn herdr_follow_target(
        &mut self,
        ctx: &Context,
        key: &herdr::HerdrKey,
    ) -> Option<SessionId> {
        if let Some(id) = self.herdr_session_for(key) {
            return Some(id);
        }
        let workspace = self.herdr_row_workspace(&key.side, &key.terminal_id)?;
        let shared_view = !self.herdr_attaches_directly(key);
        let (program, argv) = if !shared_view {
            let agent = self.find_herdr_agent(&key.side, &key.terminal_id)?;
            let attach = self.config.integrations.herdr.attach;
            // The branch already asked the question the multiplexer answers
            // here, so the `None` is unreachable; not following the pane is
            // the right answer anyway if the two ever disagree.
            let launch = Multiplexer::owning(key)
                .open_multiplexer_session(&agent.target(&key.side), attach)?;
            (launch.program, launch.argv)
        } else {
            let name = self.herdr_session_name(&key.side)?;
            key.side.command(&herdr::program(&key.side), &["session", "attach", &name])
        };
        self.open_herdr_session(ctx, key.clone(), workspace, program, argv, shared_view)?;
        self.herdr_session_for(key)
    }

    /// `listed_herdr_agents` against this frame's own state.  Empty while herdr
    /// is disabled, so a caller never has to ask twice.
    pub(super) fn herdr_agent_listing(&self) -> Vec<(WorkspaceKey, &herdr::Side, &herdr::Agent)> {
        if !self.config.integrations.herdr.enabled {
            return Vec::new();
        }
        let claimed: Vec<herdr::HerdrKey> =
            self.sessions.iter().filter_map(|s| s.herdr_key.clone()).collect();
        let workspaces = herdr_workspaces(&self.projects, |path| self.liveness.missing(path));
        listed_herdr_agents(
            self.herdr.endpoints.caches(),
            &claimed,
            &workspaces,
            self.config.integrations.herdr.show_unmatched,
        )
    }

    /// Where a pane sits in herdr's own listing, counted across endpoints in
    /// the order they are polled.  `None` for a pane no endpoint lists.
    pub(super) fn herdr_pane_index(&self, key: &herdr::HerdrKey) -> Option<usize> {
        let mut before = 0;
        for cache in self.herdr.endpoints.caches() {
            if cache.side() == &key.side {
                let at = cache.agents().iter().position(|a| a.terminal_id == key.terminal_id)?;
                return Some(before + at);
            }
            before += cache.agents().len();
        }
        None
    }

    /// The workspace a herdr row is currently listed under, for the keyboard
    /// path: `SidebarRow::HerdrAgent` itself carries no workspace, unlike a
    /// click, which already knows which panel section it landed in.
    pub(super) fn herdr_row_workspace(
        &self,
        side: &herdr::Side,
        terminal_id: &str,
    ) -> Option<WorkspaceKey> {
        let wanted = sidebar_nav::WorkspaceEntry::Agent(side.clone(), terminal_id.to_string());
        self.listed_workspace_rows()
            .into_iter()
            .find(|(_, entries)| entries.contains(&wanted))
            .map(|(ws, _)| ws)
    }

    /// Where herdr's focus goes for the pane a session is bound to.  The
    /// displayed listing drops a pane with no agent in it unless panes are
    /// shown, so a bound shell pane is found through what the side's full
    /// listing last said about it.
    pub(super) fn herdr_focus_target(&self, key: &herdr::HerdrKey) -> Option<PaneTarget> {
        let cache = self.herdr.endpoints.caches().iter().find(|cache| cache.side() == &key.side)?;
        cache
            .agents()
            .iter()
            .find(|agent| agent.terminal_id == key.terminal_id)
            .or_else(|| cache.attachment_pane(&key.terminal_id).map(|pane| &pane.agent))
            .map(|agent| agent.target(&key.side))
    }

    /// The agent behind `(side, terminal_id)`, if its endpoint still has it
    /// cached.  A stale key (the agent exited between poll and paint, or an
    /// Enter that outraced this frame's own listing) yields no row rather
    /// than a panic; the next poll drops it from the listing for good.
    pub(super) fn find_herdr_agent(
        &self,
        side: &herdr::Side,
        terminal_id: &str,
    ) -> Option<&herdr::Agent> {
        self.herdr
            .endpoints
            .caches()
            .iter()
            .find(|cache| cache.side() == side)
            .and_then(|cache| cache.agents().iter().find(|a| a.terminal_id == terminal_id))
    }

    /// herdr's word on a session's agent: `Some` only while this session is
    /// attached to one the endpoint listing still carries.
    pub(super) fn session_herdr_status(&self, session: &AppSession) -> Option<herdr::Status> {
        self.session_herdr_agent(session).and_then(|agent| agent.status)
    }

    /// The agent this session is attached to, while the endpoint listing
    /// still carries it.  herdr watches the pane from outside, so it is the
    /// authority on both what the pane is called and what it is doing.
    pub(super) fn session_herdr_agent(&self, session: &AppSession) -> Option<&herdr::Agent> {
        let key = session.herdr_key.as_ref()?;
        self.find_herdr_agent(&key.side, &key.terminal_id)
    }

    /// Whether herdr reports an agent in the pane `key` names.  A pane the
    /// listing no longer carries answers true, as does a session that is not
    /// herdr's at all: with nothing to read, the agent-registry answer is the
    /// one that keeps every caller on the path it took before the pane went.
    pub(super) fn herdr_pane_has_agent(&self, key: Option<&herdr::HerdrKey>) -> bool {
        key.and_then(|key| self.find_herdr_agent(&key.side, &key.terminal_id))
            .is_none_or(|agent| agent.status.is_some())
    }

    /// Whether opening this pane's row attaches to the pane on its own.
    pub(super) fn herdr_attaches_directly(&self, key: &herdr::HerdrKey) -> bool {
        Multiplexer::owning(key).attaches_directly(
            &key.side,
            self.config.integrations.herdr.attach,
            self.herdr_pane_has_agent(Some(key)),
        )
    }

    /// What the endpoint learned this side's herdr session is called.
    pub(super) fn herdr_session_name(&self, side: &herdr::Side) -> Option<String> {
        self.herdr
            .endpoints
            .caches()
            .iter()
            .find(|cache| cache.side() == side)
            .and_then(herdr::EndpointCache::session_name)
    }

    /// Where a herdr-backed session lives, looked up from the key the session
    /// carries.
    pub(super) fn session_multiplexer_json(&self, key: &herdr::HerdrKey) -> Value {
        multiplexer_json(
            &key.side,
            &key.terminal_id,
            self.herdr_session_name(&key.side),
            self.find_herdr_agent(&key.side, &key.terminal_id),
        )
    }

    /// Every pane herdr reports, on every side, attached or not.  Unlike the
    /// sidebar this hides nothing: `show_unmatched` decides what is worth
    /// drawing, and a caller naming a pane by its id is not browsing.
    pub(super) fn multiplexer_panes_json(&self) -> Value {
        if !self.config.integrations.herdr.enabled {
            return json!({ "panes": [] });
        }
        let workspaces = herdr_workspaces(&self.projects, |path| self.liveness.missing(path));
        let mut panes = Vec::new();
        for cache in self.herdr.endpoints.caches() {
            let side = cache.side();
            let session = cache.session_name();
            for agent in cache.agents() {
                let key =
                    herdr::HerdrKey { side: side.clone(), terminal_id: agent.terminal_id.clone() };
                panes.push(json!({
                    "multiplexer": multiplexer_json(
                        side,
                        &agent.terminal_id,
                        session.clone(),
                        Some(agent),
                    ),
                    "kind": agent.kind,
                    "title": agent.title,
                    "status": agent.status.map(|status| status.label()),
                    "focused": agent.focused,
                    "workspace": Multiplexer::owning(&key).match_workspace(agent, side, &workspaces),
                    "session_id": self.herdr_session_for(&key),
                }));
            }
        }
        json!({ "panes": panes })
    }

    /// The session already attached to this agent, if one is open.
    pub(super) fn herdr_session_for(&self, key: &herdr::HerdrKey) -> Option<SessionId> {
        self.sessions.iter().find(|s| s.herdr_key.as_ref() == Some(key)).map(|s| s.id)
    }

    /// herdr's detach chord on `side`, once that endpoint's config read has
    /// landed.
    pub(super) fn herdr_settings(&self, side: &herdr::Side) -> herdr::Settings {
        self.herdr
            .endpoints
            .caches()
            .iter()
            .find(|cache| cache.side() == side)
            .map(herdr::EndpointCache::settings)
            .unwrap_or_default()
    }

    /// What supervises `session`, when anything does.  Derived per frame
    /// rather than stored, so a config read that lands later, or a herdr that
    /// stops listing the agent, reaches the row without a second source of
    /// truth to keep in step.
    pub(super) fn session_managed(&self, session: &AppSession) -> Option<Managed> {
        let key = session.herdr_key.as_ref()?;
        let agent = self.session_herdr_agent(session);
        let mut managed = Managed::herdr(
            &key.side,
            &self.herdr_settings(&key.side),
            self.config.integrations.herdr.attach,
            agent,
        );
        // The listing answers what opening the pane now would give; this
        // session's client was settled when it attached.
        managed.shared_view = session.herdr_shared_view;
        Some(managed)
    }

    /// One number standing for every endpoint's rendered state, so the
    /// sidebar's per-frame comparison stays a `u64` compare.
    pub(super) fn herdr_generation(&self) -> u64 {
        self.herdr.endpoints.generation()
    }

    /// Refreshes the herdr endpoints on their own clock; a no-op per endpoint
    /// until its poll interval elapses.  Disabled stops the polling, not just
    /// the rows: the subprocesses are the whole cost of the feature, so an
    /// opt-out that kept running them would opt out of nothing.
    pub(super) fn poll_herdr_endpoints(&mut self) {
        if !self.config.integrations.herdr.enabled {
            return;
        }
        self.herdr.endpoints.poll(
            self.config.integrations.herdr.poll_interval,
            herdr::Listing::wanted(self.config.integrations.herdr.show_panes),
            |side| {
                self.sessions
                    .iter()
                    .any(|session| session.herdr_key.as_ref().is_some_and(|key| &key.side == side))
            },
        );
    }

    /// Attach to a pane the way its sidebar row does, holding the reply until
    /// the session behind it can be read.  A refusal names what went wrong
    /// precisely enough to act on: a side that names no server, a pane no
    /// endpoint is reporting, and an integration that is switched off are
    /// three different situations, and only the last is worth retrying after
    /// a config change.
    pub(super) fn defer_attach_multiplexer_pane(
        &mut self,
        ctx: &Context,
        side: &str,
        terminal_id: &str,
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
    ) {
        if !self.config.integrations.herdr.enabled {
            let _ = reply_tx.send(Err(HERDR_DISABLED.to_string()));
            return;
        }
        let Some(parsed_side) = herdr::Side::parse(side) else {
            let _ = reply_tx.send(Err(not_a_side(side)));
            return;
        };
        let Some(agent) = self.find_herdr_agent(&parsed_side, terminal_id) else {
            let _ = reply_tx.send(Err(format!(
                "no pane `{terminal_id}` on {side}, see list_multiplexer_panes"
            )));
            return;
        };
        let pane_id = agent.pane_id.clone();
        let key =
            herdr::HerdrKey { side: parsed_side.clone(), terminal_id: terminal_id.to_string() };
        let workspaces = herdr_workspaces(&self.projects, |path| self.liveness.missing(path));
        let workspace = Multiplexer::owning(&key).match_workspace(agent, &parsed_side, &workspaces);

        let previous = std::mem::replace(&mut self.current_workspace, workspace.clone());
        let unlisted = unlisted_pane_target(&key, &pane_id);
        if !self.attach_herdr_agent(ctx, key, unlisted, workspace, previous.clone(), Some(reply_tx))
        {
            self.current_workspace = previous;
        }
    }

    /// Create a pane and open a session on it.  An omitted side is the one
    /// the active session's own pane belongs to, since a user asking for
    /// another pane while looking at one means another like it; with no herdr
    /// session in front of them there is no such answer, so a machine
    /// reaching more than one server has to say which.
    pub(super) fn defer_create_multiplexer_pane(
        &mut self,
        ctx: &Context,
        side: Option<&str>,
        workspace: Option<PathBuf>,
        reply_tx: mpsc::Sender<ipc::protocol::IpcResult>,
    ) {
        if !self.config.integrations.herdr.enabled {
            let _ = reply_tx.send(Err(HERDR_DISABLED.to_string()));
            return;
        }
        let side = match side {
            Some(name) => match herdr::Side::parse(name) {
                Some(side) => side,
                None => {
                    let _ = reply_tx.send(Err(not_a_side(name)));
                    return;
                },
            },
            None => match self.default_multiplexer_side() {
                Ok(side) => side,
                Err(e) => {
                    let _ = reply_tx.send(Err(e));
                    return;
                },
            },
        };
        // Resolved before herdr is asked, so a path naming no worktree never
        // leaves a pane behind in the multiplexer.
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
        self.create_multiplexer_pane(ctx, side, workspace, Some(reply_tx));
    }

    /// The side a create that named none happens on: the one the active
    /// session's own pane belongs to, and failing that the one endpoint a
    /// server is answering on.  `Err` names every side it could have meant,
    /// so a caller can retry saying which.
    pub(super) fn default_multiplexer_side(&self) -> Result<herdr::Side, String> {
        let focused = self
            .active_session_index()
            .and_then(|idx| self.sessions[idx].herdr_key.as_ref())
            .map(|key| key.side.clone());
        if let Some(side) = focused {
            return Ok(side);
        }
        // A cache holds a sample time only while it still holds a listing, so
        // a side whose rows are gone is not offered as the one a create meant.
        let answering: Vec<&herdr::Side> = self
            .herdr
            .endpoints
            .caches()
            .iter()
            .filter(|cache| cache.sampled_at().is_some())
            .map(herdr::EndpointCache::side)
            .collect();
        match answering.as_slice() {
            [side] => Ok((*side).clone()),
            [] => Err("no herdr server is answering; start one, or name a side".to_string()),
            sides => Err(format!(
                "no herdr session is focused and {} are answering; name one",
                sides.iter().map(|side| side.name()).collect::<Vec<_>>().join(" and ")
            )),
        }
    }
}

impl Action for action::NewMultiplexerPane {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        if !app.config.integrations.herdr.enabled {
            app.modals.error_dialog = Some(HERDR_DISABLED.to_string());
            return;
        }
        match app.default_multiplexer_side() {
            Ok(side) => {
                let workspace = app.current_workspace.clone();
                app.create_multiplexer_pane(ctx, side, workspace, None);
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

/// Where the user lands when a herdr attach fails after switching them.  The
/// job answers frames later, so a switch made in between is theirs and
/// outranks the restore: `previous` is handed back only while `current` is
/// still the workspace the attach moved them to.
pub(super) fn workspace_after_failed_attach(
    current: &WorkspaceKey,
    switched_to: &WorkspaceKey,
    previous: WorkspaceKey,
) -> WorkspaceKey {
    if current == switched_to { previous } else { current.clone() }
}

/// The external supervisor a pane belongs to.  Named rather than flagged
/// because a second harness would otherwise add a parallel boolean to every
/// row, and because what a row must say — whose mark to paint, how to get
/// out, whether the attach is exclusive — varies by harness rather than by
/// row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Managed {
    /// What the tooltip calls it.
    pub(super) harness: &'static str,
    /// The harness's own detach chord, already rendered.  `None` when its
    /// config could not be read or binds detach to nothing, both of which are
    /// reasons to stay quiet rather than name a chord the user may not have.
    pub(super) detach: Option<String>,
    /// The attach shares the harness's whole view rather than one pane, so
    /// the row says so before a resize reveals it.
    pub(super) shared_view: bool,
    /// The agent kind the harness detected, spelled the way it invokes it.
    pub(super) kind: Option<String>,
    /// The pane's own title, when it says something the kind does not.
    pub(super) title: Option<String>,
    /// How the harness draws the state it reports.  `None` when it is no
    /// longer reporting one — a pane alacritree still holds open after its
    /// harness stopped listing it.
    pub(super) mark: Option<HarnessMark>,
}

impl Managed {
    /// `agent` is herdr's current word on the pane, and `None` once it stops
    /// reporting one — a pane alacritree still holds open after its harness
    /// let go of it, which has a harness and a way out but no state or name.
    pub(super) fn herdr(
        side: &herdr::Side,
        settings: &herdr::Settings,
        attach: AttachMode,
        agent: Option<&herdr::Agent>,
    ) -> Self {
        let kind = agent.and_then(|a| a.kind.clone());
        let title =
            agent.and_then(|a| a.title.clone()).filter(|t| Some(t.as_str()) != kind.as_deref());
        Self {
            harness: "herdr",
            detach: settings.detach.clone(),
            shared_view: !Multiplexer::from(Herdr).attaches_directly(
                side,
                attach,
                agent.is_none_or(|a| a.status.is_some()),
            ),
            mark: agent
                .and_then(|a| a.status)
                .map(|status| herdr_mark(status, settings.indicators)),
            kind,
            title,
        }
    }

    /// What the harness calls this pane: the agent kind backquoted as the
    /// command it is, and the title quoted as the words it is.
    pub(super) fn pane_name(&self) -> Option<String> {
        match (&self.kind, &self.title) {
            (Some(kind), Some(title)) => Some(format!("`{kind}` \"{title}\"")),
            (Some(kind), None) => Some(format!("`{kind}`")),
            (None, Some(title)) => Some(format!("\"{title}\"")),
            (None, None) => None,
        }
    }
}

/// The mark a harness paints for the state it reports, in that harness's own
/// vocabulary.  Resolved once per row, so a pane reads the same whether it is
/// listed or attached — the two are drawn by different painters, and attaching
/// must not repaint a pane in a language it does not speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HarnessMark {
    pub(super) glyph: &'static str,
    pub(super) tone: StateTone,
    /// The harness's own word for this state, for the hover text.
    pub(super) label: &'static str,
}

/// What a harness means by a state's color.  Named rather than carried as a
/// `Color32` so the palette stays alacritree's and a row snapshot stays free
/// of the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StateTone {
    Blocked,
    Working,
    Done,
    Idle,
    /// The harness reported something alacritree does not recognise.
    Unclear,
}

/// herdr's state vocabulary, taken from its own `state_icon_symbol` and
/// `state_label_color`.  Which of the two sets applies is herdr's `[ui]
/// status_indicators`, so a user who picked one in herdr gets it here too.
///
/// `done` is `idle` on herdr's internal axis and a status of its own over its
/// API, which is the axis alacritree reads — so the two arrive already
/// distinguished, without the "has a human looked at it yet" bit herdr tracks
/// to tell them apart.
pub(super) fn herdr_mark(status: herdr::Status, indicators: herdr::Indicators) -> HarnessMark {
    use herdr::Indicators::{Dots, Symbols};
    use herdr::Status::{Blocked, Done, Idle, Unknown, Working};
    let (glyph, tone) = match (indicators, status) {
        (_, Idle) => ("○", StateTone::Idle),
        (_, Unknown) => ("·", StateTone::Unclear),
        (Dots, Blocked) => ("●", StateTone::Blocked),
        (Dots, Working) => ("●", StateTone::Working),
        (Dots, Done) => ("●", StateTone::Done),
        (Symbols, Blocked) => ("×", StateTone::Blocked),
        (Symbols, Working) => ("◐", StateTone::Working),
        (Symbols, Done) => ("✓", StateTone::Done),
    };
    HarnessMark { glyph, tone, label: status.label() }
}

/// The workspaces a herdr agent may be matched against.  A checkout that
/// looks gone offers none, which is what makes an agent working there
/// unmatched rather than parked under a row that can only refuse it.
/// `missing` is the liveness cache's word for a path, `None` where it has
/// none, so the row's grey and this list agree about the same directory.
pub(super) fn herdr_workspaces(
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

/// What a harness-managed row explains on hover, one fact per comma: the
/// state, since that is what changes; who reports it; whether the attach is
/// the harness's whole view; and what the harness calls the pane.  The way
/// out follows in parentheses, since it is an instruction rather than
/// another fact about the pane.
///
/// The same sentence serves a listed agent and an attached one.  Attaching
/// changes how alacritree draws a pane, not what there is to say about it,
/// and the chord has no other surface in alacritree — it is the harness's
/// key, not one of ours — so it has to reach the row the user is sitting in.
pub(super) fn managed_tooltip(managed: &Managed) -> String {
    let mut parts = Vec::new();
    if let Some(mark) = managed.mark {
        parts.push(mark.label.to_owned());
    }
    parts.push(managed.harness.to_owned());
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

/// A shared-view attach waiting on herdr.  The gesture answers with the argv
/// its client runs, so everything the session needs is in hand by the time it
/// opens.
pub(super) struct PendingHerdrAttach {
    pub(super) job: Option<jobs::Job<Result<Launch, String>>>,
    /// The pane to focus once the gesture runs.  A pane the listing has since
    /// dropped is focused as this said, since nothing newer says otherwise.
    pub(super) target: PaneTarget,
    pub(super) key: herdr::HerdrKey,
    pub(super) workspace: WorkspaceKey,
    /// Where to hand the user back when herdr refuses.  A shared-view
    /// attach answers frames after the switch, so the caller cannot restore
    /// the workspace itself the way a direct attach lets it.
    pub(super) previous: WorkspaceKey,
    /// Clients parked on this attach.  A shared-view attach opens its session
    /// frames after the request that asked for it, so there is nothing to
    /// answer with until `poll_herdr_attach` resolves.
    pub(super) waiters: Vec<mpsc::Sender<ipc::protocol::IpcResult>>,
}

/// A pane being created.  The attach it turns into is the ordinary one, so
/// this queue only carries the gesture: `poll_herdr_create` hands the pane it
/// names to `attach_herdr_agent` and stops there.
///
/// One waiter, not a list: nothing merges two creates, since the pane they
/// would be merged on has no identity until herdr answers.
pub(super) struct PendingHerdrCreate {
    pub(super) job: jobs::Job<Result<CreatedPane, String>>,
    pub(super) side: herdr::Side,
    pub(super) workspace: WorkspaceKey,
    pub(super) waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
}

/// A pane the listing no longer carries.  Claiming an agent is in it keeps
/// every caller on the path it took before the pane went, which is what
/// `herdr_pane_has_agent` answers for the same reason.
pub(super) fn unlisted_pane_target(key: &herdr::HerdrKey, pane_id: &str) -> PaneTarget {
    PaneTarget {
        side: key.side.clone(),
        pane_id: pane_id.to_string(),
        tab_id: None,
        has_agent: true,
    }
}

/// A name that reaches no server, as opposed to one whose server is down: a
/// caller retrying this one is retrying a typo.
pub(super) fn not_a_side(name: &str) -> String {
    format!("`{name}` is not a side, expected `native` or `wsl:<distro>`")
}

/// The directory a new pane opens in, spelled where the multiplexer resolves
/// it: the distro's own path on a WSL side, the Windows path on the native
/// one.  `None` leaves the choice to the multiplexer.  A workspace with no
/// spelling inside the distro is an `Err`, since a pane opened anywhere else
/// would still have its session filed under that workspace.
pub(super) fn multiplexer_cwd(
    side: &herdr::Side,
    workspace: Option<&Path>,
) -> Result<Option<String>, String> {
    let Some(path) = workspace else { return Ok(None) };
    match side {
        herdr::Side::Native => Ok(Some(path.display().to_string())),
        herdr::Side::Wsl(distro) => wsl::windows_to_linux(path)
            .map(Some)
            .ok_or_else(|| format!("{} has no path inside the {distro} distro", path.display())),
    }
}

/// Where a herdr pane lives, in the fields an attach takes back.  `pane_id`
/// and `tab_id` are null when the listing does not carry the pane, which
/// says the cache does not know right now rather than that the pane is
/// gone.
pub(super) fn multiplexer_json(
    side: &herdr::Side,
    terminal_id: &str,
    session: Option<String>,
    pane: Option<&herdr::Agent>,
) -> Value {
    json!({
        "name": "herdr",
        "side": side.name(),
        "session": session,
        "terminal_id": terminal_id,
        "pane_id": pane.map(|pane| pane.pane_id.clone()),
        "tab_id": pane.and_then(|pane| pane.tab_id.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_session_removes_only_its_attach_and_view_focus() {
        let mut herdr = HerdrGlue::new();
        let first_id = 1;
        let first_key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "first".into() };
        let second_key =
            herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "second".into() };
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();

        herdr.pending_attach.push(PendingHerdrAttach {
            job: None,
            target: unlisted_pane_target(&first_key, "w1:p1"),
            key: first_key.clone(),
            workspace: None,
            previous: None,
            waiters: vec![first_tx],
        });
        herdr.pending_attach.push(PendingHerdrAttach {
            job: None,
            target: unlisted_pane_target(&second_key, "w1:p2"),
            key: second_key.clone(),
            workspace: None,
            previous: None,
            waiters: vec![second_tx],
        });
        herdr.view_focus = Some(herdr::HerdrViewFocus {
            session: first_id,
            key: first_key.clone(),
            job: jobs::Job::ready(Ok(())),
        });

        herdr.close_session(first_id, Some(&first_key));

        assert!(herdr.view_focus.is_none());
        assert_eq!(herdr.pending_attach.len(), 1);
        assert_eq!(herdr.pending_attach[0].key, second_key);
        assert_eq!(
            first_rx.try_recv().unwrap(),
            Err("the session behind this pane was closed before the attach finished".to_string())
        );
        assert!(second_rx.try_recv().is_err());
    }

    /// The multiplexer resolves the directory where it runs, so a WSL side is
    /// handed the distro's own spelling of the workspace and never the
    /// Windows path the sidebar holds.
    #[cfg(windows)]
    #[test]
    fn a_new_pane_opens_in_the_workspace_spelled_for_its_own_side() {
        let workspace = PathBuf::from(r"\\wsl.localhost\ubuntu\home\dev\repo");
        assert_eq!(
            multiplexer_cwd(&herdr::Side::Wsl("ubuntu".into()), Some(&workspace)),
            Ok(Some("/home/dev/repo".to_string()))
        );
        assert_eq!(
            multiplexer_cwd(&herdr::Side::Native, Some(&workspace)),
            Ok(Some(workspace.display().to_string()))
        );
    }

    /// The home workspace names no directory, so herdr picks its own default
    /// rather than being handed an empty path.
    #[test]
    fn a_new_pane_in_the_home_workspace_names_no_directory() {
        assert_eq!(multiplexer_cwd(&herdr::Side::Native, None), Ok(None));
        assert_eq!(multiplexer_cwd(&herdr::Side::Wsl("ubuntu".into()), None), Ok(None));
    }

    /// herdr distinguishes four live states and says so on its own panes.
    /// Collapsing any pair onto one mark would make the sidebar say less
    /// about a pane than the window it came from.
    #[test]
    fn herdr_marks_keep_its_four_states_apart() {
        for set in [herdr::Indicators::Dots, herdr::Indicators::Symbols] {
            let marks: Vec<HarnessMark> = [
                herdr::Status::Blocked,
                herdr::Status::Working,
                herdr::Status::Done,
                herdr::Status::Idle,
            ]
            .into_iter()
            .map(|status| herdr_mark(status, set))
            .collect();
            for (i, a) in marks.iter().enumerate() {
                for b in &marks[i + 1..] {
                    assert_ne!(a, b, "{set:?} draws two states the same");
                }
            }
        }
    }

    /// Taken from herdr's own `state_icon_symbol`, so a pane carries one mark
    /// whether it is read in herdr or in the sidebar.
    #[test]
    fn herdr_marks_are_the_ones_herdr_paints() {
        let dots = |status| herdr_mark(status, herdr::Indicators::Dots).glyph;
        assert_eq!(dots(herdr::Status::Blocked), "●");
        assert_eq!(dots(herdr::Status::Working), "●");
        assert_eq!(dots(herdr::Status::Done), "●");
        assert_eq!(dots(herdr::Status::Idle), "○");

        let symbols = |status| herdr_mark(status, herdr::Indicators::Symbols).glyph;
        assert_eq!(symbols(herdr::Status::Blocked), "×");
        assert_eq!(symbols(herdr::Status::Working), "◐");
        assert_eq!(symbols(herdr::Status::Done), "✓");
        assert_eq!(symbols(herdr::Status::Idle), "○");
    }

    /// A status alacritree does not recognise is herdr declining to say, and
    /// the row says that rather than claiming the agent is idle.
    #[test]
    fn an_unknown_herdr_status_is_drawn_as_no_reading() {
        for set in [herdr::Indicators::Dots, herdr::Indicators::Symbols] {
            let mark = herdr_mark(herdr::Status::Unknown, set);
            assert_eq!(mark.glyph, "·");
            assert_eq!(mark.tone, StateTone::Unclear);
        }
    }

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

    /// herdr answers frames after the click, and a switch made in between is
    /// the user's own: restoring over it would pull them out of a workspace
    /// they chose.
    #[test]
    fn a_failed_attach_leaves_a_workspace_the_user_moved_to_alone() {
        let current = Some(PathBuf::from("/code/elsewhere"));
        let switched_to = Some(PathBuf::from("/code/wt"));
        assert_eq!(workspace_after_failed_attach(&current, &switched_to, None), current);
    }

    /// A session alacritree still holds open after herdr stopped listing its
    /// pane has no state and no name left to report, but it is still herdr's
    /// and the user still has to know how to leave it.
    #[test]
    fn an_unlisted_pane_still_says_how_to_leave() {
        let settings =
            herdr::Settings { detach: Some("Ctrl+B q".into()), ..herdr::Settings::default() };
        let managed =
            Managed::herdr(&herdr::Side::Wsl("d".into()), &settings, AttachMode::Agent, None);
        assert_eq!(managed_tooltip(&managed), "herdr. (detach with `Ctrl+B q`)");
    }

    /// A checkout the liveness cache calls gone offers no workspace, so the
    /// agent working in it matches nothing and lists under Home.  Matched to
    /// the removed worktree instead, its row's Enter could only refuse.
    #[test]
    fn a_gone_worktree_offers_no_workspace_to_an_agent() {
        use crate::sidebar_nav;

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"])];
        let gone = PathBuf::from("/a/wt2");
        let workspaces = herdr_workspaces(&projects, |path| Some(path == gone));
        assert_eq!(workspaces, vec![PathBuf::from("/a/wt1")]);

        let agent = herdr::Agent {
            terminal_id: "t1".into(),
            pane_id: "w1:p1".into(),
            tab_id: Some("w1:t1".into()),
            kind: None,
            title: None,
            status: Some(herdr::Status::Idle),
            focused: false,
            cwd: Some(gone.to_string_lossy().into_owned()),
            foreground_cwd: None,
        };
        assert_eq!(
            Multiplexer::from(Herdr).match_workspace(&agent, &herdr::Side::Native, &workspaces),
            None,
            "an agent under a removed checkout falls back to Home"
        );
    }
}
