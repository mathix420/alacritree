//! The projects sidebar: the state only it owns, the paint pass over it, and
//! the row painters that pass draws with.

use super::*;
use crate::multiplexer::{MultiplexerKind, Pane};

pub(super) struct Sidebar {
    /// Reveals the project rows' drag grips.  A transient mode, not persisted:
    /// reordering is a rare, deliberate act, and a grip on every row the rest
    /// of the time is noise.
    pub(super) reorder_mode: bool,
    /// Fuzzy-search query and `s`/`a` toggle state for the projects panel.
    /// Transient: never persisted, never touches the `expanded` flag.
    pub(super) filter: PanelFilter,
    /// The rows and the cursor over them.  Read rows through
    /// `fresh_sidebar_model`, which rebuilds them first when they are stale.
    pub(super) model: SidebarModel,
}

impl Sidebar {
    pub(super) fn new(filter: PanelFilter) -> Self {
        Self { reorder_mode: false, filter, model: SidebarModel::default() }
    }
}

impl AlacritreeApp {
    /// Tint whichever region a drop would land on while files are hovering, so
    /// three targets do not become a guessing game.  Silent off Windows: no
    /// cursor position is available there, so the tint would be a lie.
    pub(super) fn paint_drop_hover(&self, ctx: &Context, regions: &file_drop::Regions) {
        let cfg = &self.config.ui.drop;
        if !cfg.enabled || !cfg.highlight || ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let Some(pointer) = file_drop::screen_pointer(ctx) else {
            return;
        };
        // winit's `DragOver` handler emits no event, so moving the cursor
        // mid-drag wakes nothing and the polled position would stay frozen at
        // wherever the drag entered.  This is the only place the feature drives
        // the loop, and it stops when the drag leaves or drops.
        ctx.request_repaint();
        let active_is_scratchpad =
            self.active_session_index().is_some_and(|idx| self.sessions[idx].scratchpad.is_some());
        let Some(target) = file_drop::route(Some(pointer), regions, active_is_scratchpad, cfg)
        else {
            return;
        };
        let rect = match target {
            file_drop::Target::ProjectsSidebar => match regions.sidebar {
                Some(rect) => rect,
                None => return,
            },
            file_drop::Target::Terminal | file_drop::Target::Scratchpad => regions.central,
        };
        // `Theme::accent` is already resolved (config accent, else ANSI blue);
        // `UiTheme::sidebar_accent` is the raw `Option` and would paint nothing
        // on an unconfigured palette.
        let accent = self.theme.accent;
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop_hover")))
            .rect_filled(rect, 0.0, accent.linear_multiply(0.15));
    }

    /// Rows the sidebar cursor steps over: the fuzzy/toggle-filtered set while
    /// a filter is active, the full visible set otherwise.  Only
    /// `refresh_sidebar_rows` calls this; everything else reads the cache.
    pub(super) fn build_project_rows(
        &mut self,
        listed: &sidebar_nav::ListedRows,
    ) -> Vec<SidebarRow> {
        if !self.sidebar.filter.is_filtering() {
            return sidebar_nav::visible_rows(&self.projects, listed);
        }

        let apply = self.sidebar.filter.toggles_apply(self.sidebar_focus_state.search_scope);
        let toggle_sessions = apply && self.sidebar.filter.is_toggled('s');
        let toggle_attention = apply && self.sidebar.filter.is_toggled('a');
        let pr_open = apply && self.sidebar.filter.is_toggled('o');
        let pr_draft = apply && self.sidebar.filter.is_toggled('d');
        let pr_merged = apply && self.sidebar.filter.is_toggled('m');
        let pr_closed = apply && self.sidebar.filter.is_toggled('c');
        let any_pr = pr_open || pr_draft || pr_merged || pr_closed;
        let any_toggle = any_project_toggle_active(toggle_sessions, toggle_attention, any_pr);

        // Precompute every fuzzy result before building the closures: the
        // matcher needs `&mut self.sidebar.filter`, and releasing that borrow
        // up-front lets the predicates read the rest of `&self` freely.
        let home_matches = self.sidebar.filter.matches("Home");
        let project_matches: HashMap<PathBuf, bool> = {
            let filter = &mut self.sidebar.filter;
            self.projects
                .iter()
                .map(|p| (p.root.clone(), filter.matches(p.display_name())))
                .collect()
        };
        let worktree_matches: HashMap<PathBuf, bool> = {
            let filter = &mut self.sidebar.filter;
            self.projects
                .iter()
                .flat_map(|p| p.worktrees.iter())
                .map(|wt| (wt.path.clone(), filter.matches(&wt.name)))
                .collect()
        };
        let live_branch = self
            .current_workspace
            .as_deref()
            .and_then(|p| self.git_panel.status.get(p))
            .and_then(|c| c.current_branch());
        let current_workspace = self.current_workspace.as_deref();
        // Skipped outright while the PR dimension is inert: `worktree_pr_passes`
        // would not read the map, and building it costs a path clone per
        // worktree on a call that runs whenever the panel is filtering at all.
        let pr_matches: HashMap<PathBuf, bool> = if any_pr {
            self.projects
                .iter()
                .flat_map(|p| p.worktrees.iter())
                .map(|wt| {
                    let branch = pr_status::effective_branch(wt, current_workspace, live_branch);
                    let state = self.pr_cache.state(&wt.path, branch);
                    (
                        wt.path.clone(),
                        pr_status::pr_pass(state, pr_open, pr_draft, pr_merged, pr_closed),
                    )
                })
                .collect()
        } else {
            HashMap::new()
        };

        // Child names are resolved before the matcher borrows the filter: the
        // names come off `&self` helpers and the matcher wants `&mut
        // self.sidebar.filter`, so the two cannot be live at once.  Skipped
        // outright by `search_reaches_children` with an empty query, where
        // `matches` answers true for everything and every workspace holding
        // any child would surface, and with `[ui] search_depth` at its
        // "workspaces" default, which never descends past a workspace name.
        let child_matches: HashMap<SidebarRow, bool> =
            if search_reaches_children(self.search_depth, self.sidebar.filter.query().is_empty()) {
                let names: Vec<(SidebarRow, String)> = listed
                    .values()
                    .flatten()
                    .map(|entry| {
                        let name = match entry {
                            sidebar_nav::WorkspaceEntry::Session(id) => self
                                .sessions
                                .iter()
                                .find(|s| s.id == *id)
                                .map(|s| {
                                    let activity = pane_backed_activity(
                                        s.activity(),
                                        self.session_pane_status(s),
                                    );
                                    session_row_name(&s.title, activity, self.session_pane(s))
                                })
                                .map(RowName::search_text)
                                .unwrap_or_default(),
                            sidebar_nav::WorkspaceEntry::Pane(key) => self
                                .find_pane(key)
                                .map(|pane| pane_display_name(pane).search_text())
                                .unwrap_or_default(),
                        };
                        (entry.row(), name)
                    })
                    .collect();
                let filter = &mut self.sidebar.filter;
                names.into_iter().map(|(row, name)| (row, filter.matches(&name))).collect()
            } else {
                HashMap::new()
            };

        let session_workspaces: Vec<WorkspaceKey> =
            self.sessions.iter().map(|s| s.working_directory.clone()).collect();
        let gate = |key: &WorkspaceKey| {
            project_toggles_pass(
                apply,
                toggle_sessions,
                sessions_filter_passes(
                    &session_workspaces,
                    listed,
                    key,
                    self.sessions_filter_counts_detached,
                ),
                toggle_attention,
                self.workspace_needs_attention(key),
            ) && key.as_deref().is_none_or(|path| worktree_pr_passes(any_pr, &pr_matches, path))
        };
        let project_self =
            |p: &Project| !any_toggle && project_matches.get(&p.root).copied().unwrap_or(false);
        let mut name =
            |_p: &Project, wt: &Worktree| worktree_matches.get(&wt.path).copied().unwrap_or(false);
        let children_tested = !child_matches.is_empty();
        let mut child = |entry: &sidebar_nav::WorkspaceEntry| {
            child_matches.get(&entry.row()).copied().unwrap_or(false)
        };
        let child: Option<&mut dyn FnMut(&sidebar_nav::WorkspaceEntry) -> bool> =
            if children_tested { Some(&mut child) } else { None };
        sidebar_nav::filtered_rows(&self.projects, listed, sidebar_nav::RowPredicates {
            home_gate: gate(&None),
            home_name: home_matches,
            project_self: &project_self,
            gate: &gate,
            name: &mut name,
            child,
        })
    }

    pub(super) fn show_project_sidebar(&mut self, ctx: &Context, panel_frame: Frame) -> egui::Rect {
        let view = self.project_sidebar_view(ctx);
        let paint = SidebarPaint { view: &view, icons: &self.icons };
        let theme = view.theme;
        let mut requests = SidebarRequests::default();
        let panel_resp = SidePanel::left("left_sidebar")
            .resizable(true)
            .default_width(240.0 * theme.ui_scale)
            .min_width(180.0 * theme.ui_scale)
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Sidebar rows are click targets, not selectable prose; the
                // default I-beam-and-select on labels is the wrong affordance.
                ui.style_mut().interaction.selectable_labels = false;
                apply_scrollbar_style(ui, self.config.ui.scrollbar);
                ui.horizontal(|ui| {
                    panel_header_filter_ui(
                        ui,
                        "Projects",
                        &self.sidebar.filter,
                        &paint.icons.search,
                        &theme,
                        self.sidebar.filter.toggles_apply(self.sidebar_focus_state.search_scope),
                    );
                    projects_header_buttons(ui, paint, &mut requests);
                });
                ui.separator();

                ScrollArea::vertical().show(ui, |ui| {
                    // Inter-group spacing is emitted above the group that
                    // follows, never after the last one: trailing padding
                    // makes the content measure taller than the rows on
                    // screen, which shows a scrollbar with nothing to scroll
                    // whenever the list otherwise fits the panel.
                    let mut group_gap = 0.0_f32;
                    if !view.filtering || view.membership.home {
                        paint_home_group(ui, paint, &mut requests);
                        group_gap = 2.0;
                    }

                    if self.projects.is_empty() {
                        ui.add_space(std::mem::take(&mut group_gap));
                        ui.label(
                            RichText::new("Click + to add a project.")
                                .color(theme.text_dim)
                                .small(),
                        );
                        ui.add_space(4.0);
                        ui.label(RichText::new("Ctrl+B to toggle").small().color(theme.text_muted));
                    } else if view.filtered_empty {
                        ui.add_space(std::mem::take(&mut group_gap));
                        ui.label(RichText::new("no matches").color(theme.text_dim).small());
                    }

                    for (idx, project) in self.projects.iter_mut().enumerate() {
                        if view.filtering && !view.membership.projects.contains(&project.root) {
                            continue;
                        }
                        ui.add_space(std::mem::take(&mut group_gap));
                        paint_project_header(ui, paint, idx, project, &mut requests);
                        if project.expanded || view.filtering {
                            paint_worktrees(ui, paint, idx, project, &mut requests);
                            group_gap = 4.0;
                        }
                    }
                });
            });

        self.apply_sidebar_edits(ctx, &mut requests);
        let workspace_activated = self.apply_sidebar_activations(ctx, &mut requests);
        self.poll_worktree_liveness(ctx, view.probing, &requests.drawn_worktrees);
        if self.config.ui.sidebar_click_focus {
            // A click that picks a workspace or session means "go work
            // there", so it focuses the terminal; other panel clicks focus
            // the sidebar for filter typing.  Row activations fire on the
            // release frame, after the press already focused the sidebar,
            // which is why this can't fold into the press test below.
            if workspace_activated {
                self.focus_terminal();
            } else if self.focus != PaneFocus::ProjectsSidebar
                && pressed_on_panel(ctx, &panel_resp.response)
            {
                self.focus_sidebar();
            }
        }
        panel_resp.response.rect
    }

    /// Everything the paint pass reads, gathered while `&self` helpers are
    /// still callable: the panel closure borrows `projects` mutably.
    fn project_sidebar_view(&mut self, ctx: &Context) -> SidebarView {
        // Only rows that actually paint are worth a liveness probe, and which
        // ones those are is not known until the tree, its filters and its
        // collapsed projects have all had their say.  Deciding *before* the
        // walk that this frame is not a probe frame is what keeps the other
        // ~89 frames of every 90 from collecting anything at all.
        let probing = self.config.ui.worktree_liveness
            && self.liveness_probe.is_none()
            && self.liveness.wants_probe(Instant::now());
        // The render pass cannot borrow `self.sessions`, so the dragged
        // session's own scope is resolved here: a row outside this range draws
        // no indicator and never becomes a drop.
        let drag_range: Option<(SessionId, Vec<WorkspaceKey>)> =
            egui::DragAndDrop::payload::<DraggedSession>(ctx).and_then(|dragged| {
                let (_, range) = self.reorder_range(dragged.0)?;
                Some((dragged.0, range))
            });
        let cursor_row = if self.focus == PaneFocus::ProjectsSidebar {
            self.sidebar.model.cursor().cloned()
        } else {
            None
        };
        let cursor_moved = self.sidebar.model.take_cursor_moved();

        let filtering = self.sidebar.filter.is_filtering();
        let active_now = self.sessions.active(&self.current_workspace);
        // egui keeps one scroll target per frame and the last writer wins, so the
        // two reasons to scroll are resolved here rather than by paint order.  An
        // explicit cursor move outranks following the terminal.
        let wants_follow = sidebar_nav::wants_follow(
            self.config.ui.sidebar_follow_active,
            cursor_moved,
            &self.last_followed,
            &self.current_workspace,
            active_now,
        );
        let rows: &[SidebarRow] = if filtering || wants_follow {
            self.refresh_sidebar_rows();
            self.sidebar.model.rows()
        } else {
            &[]
        };
        let follow_row = wants_follow
            .then(|| {
                let project_root = sidebar_nav::project_of(&self.projects, &self.current_workspace)
                    .map(Path::to_path_buf);
                sidebar_nav::follow_scroll_row(
                    rows,
                    &self.current_workspace,
                    active_now,
                    project_root.as_deref(),
                )
            })
            .flatten();
        if follow_row.is_some() {
            self.last_followed = (self.current_workspace.clone(), active_now);
        }

        let membership = FilterMembership::of(filtering, rows);
        let filtered_empty = filtering
            && !membership.home
            && membership.projects.is_empty()
            && membership.worktrees.is_empty();

        // Snapshot attention + agent-glyph state up-front so the `iter_mut`
        // over projects in the paint pass isn't blocked from calling back
        // into `&self` helpers.
        let mut listed = self.listed_workspace_rows();
        // The cursor can only reach a row the nav model listed, so paint keeps
        // exactly that set: a session the filter dropped would strand just
        // like an unlisted agent, so both are pruned by row membership here.
        if filtering {
            for entries in listed.values_mut() {
                entries.retain(|entry| membership.children.contains(&entry.row()));
            }
        }
        let home_rows = self.workspace_rows(&None, &listed);
        // A rendered session list carries its own per-session status; repeating
        // it on the parent row reads as noise, the same
        // rule the project row applies when expanded.  Aggregates therefore
        // apply only while the list is hidden (fewer than two sessions).
        let home_lists_sessions = WorkspaceRowData::any_session(&home_rows);
        let home_status = if home_lists_sessions {
            RowStatus::live(SessionActivity::Shell)
        } else {
            self.workspace_status(&None)
        };
        let projects = self.project_views(ctx, &listed);

        SidebarView {
            theme: self.theme,
            probing,
            reorder_mode: self.sidebar.reorder_mode,
            session_drag: self.session_drag,
            drag_range,
            cursor_row,
            cursor_moved,
            follow_row,
            filtering,
            membership,
            filtered_empty,
            home_rows,
            home_active: self.current_workspace.is_none(),
            home_status,
            projects,
            // Worktrees whose background removal is still running: their rows show
            // a spinner instead of the delete/new-shell controls.
            deleting_paths: self
                .modals
                .pending_deletes
                .iter()
                .map(|t| t.worktree_path.clone())
                .collect(),
            // Minimized creations, keyed by project index, rendered as spinner
            // placeholder rows until the finished worktree shows up on refresh.
            creating: self
                .modals
                .pending_creates
                .iter()
                .map(|c| (c.project_idx, c.branch.clone()))
                .collect(),
            distros: wsl::distros(),
            profile_names: self.config.profiles.iter().map(|p| p.name.clone()).collect(),
            // Name + command pairs for the worktree row's "Open session" menu.
            // The command is only ever shown as hover text, never painted.
            worktree_profiles: self
                .config
                .profiles
                .iter()
                .map(|p| (p.name.clone(), profile_command(p)))
                .collect(),
        }
    }

    /// One entry per project, aligned with `projects`, each holding one entry
    /// per worktree.
    fn project_views(
        &mut self,
        ctx: &Context,
        listed: &sidebar_nav::ListedRows,
    ) -> Vec<ProjectView> {
        let pr_enabled = self.config.integrations.gh.pr_status;
        let any_pr_toggle =
            any_pr_toggle_active(&self.sidebar.filter, self.sidebar_focus_state.search_scope);
        let current_workspace = self.current_workspace.as_deref();
        let live_branch = current_workspace
            .and_then(|p| self.git_panel.status.get(p))
            .and_then(|cache| cache.current_branch());
        // The same path can be a worktree of two projects, and `PrCache` is
        // keyed by path alone, so a second poller would only invalidate the
        // first's lookup and burn a `gh` process every frame.
        let mut polled: HashMap<PathBuf, Option<PrInfo>> = HashMap::new();
        let mut views = Vec::with_capacity(self.projects.len());
        for project in &self.projects {
            let mut worktrees = Vec::with_capacity(project.worktrees.len());
            for wt in &project.worktrees {
                let ws = Some(wt.path.clone());
                let rows = self.workspace_rows(&ws, listed);
                // Aggregates apply only while the session list is hidden, as
                // on the home row.
                let lists_sessions = WorkspaceRowData::any_session(&rows);
                let pr = resolve_pr_info(
                    &mut polled,
                    &wt.path,
                    should_poll_pr(pr_enabled, project.expanded, any_pr_toggle),
                    || {
                        let branch =
                            pr_status::effective_branch(wt, current_workspace, live_branch);
                        self.pr_cache.poll(&wt.path, branch, ctx)
                    },
                );
                // Rendered up front: the panel closure borrows `projects` mutably, and
                // substitution over short strings is microseconds, so no cache is kept.
                // After `pr` so `$pr` sees this frame's PR number.
                worktrees.push(WorktreeView {
                    label: self.row_labels.worktree_label(wt, pr.as_ref()),
                    status: if lists_sessions {
                        RowStatus::live(SessionActivity::Shell)
                    } else {
                        self.workspace_status(&ws)
                    },
                    is_active: current_workspace == Some(wt.path.as_path()),
                    missing: self.liveness.missing(&wt.path),
                    pr,
                    rows,
                });
            }
            views.push(ProjectView {
                label: self.row_labels.project_label(project),
                attention: self.project_needs_attention(project),
                worktrees,
            });
        }
        views
    }

    /// Applies what the paint pass recorded other than activations: project
    /// edits, session drops, and the dialogs a row opens.  A pointer release
    /// clicks one widget, so at most one of these fires per frame and their
    /// order is free.
    fn apply_sidebar_edits(&mut self, ctx: &Context, requests: &mut SidebarRequests) {
        if requests.add_project {
            self.add_project_via_dialog(ctx);
        }
        if requests.reorder_toggled {
            self.sidebar.reorder_mode = !self.sidebar.reorder_mode;
        }
        if let Some(idx) = requests.refresh {
            self.refresh_project(ctx, idx);
        }
        if let Some(req) = requests.remove.take() {
            self.modals.pending_project_remove = Some(req);
        }
        if let Some((root, insert_before)) = requests.reorder.take() {
            self.move_project(&root, insert_before);
        }
        if let Some((id, workspace, position)) = requests.session_drop.take() {
            self.apply_session_drop(id, workspace, position);
        }
        if let Some((root, expanded)) = requests.expand_toggled.take() {
            state::mutate(|s| {
                if let Some(p) = s.projects.iter_mut().find(|p| p.root == root) {
                    p.expanded = expanded;
                }
            });
        }
        if let Some(root) = requests.shell_override_changed.take() {
            self.persist_project(&root);
        }
        if let Some(root) = requests.label_cleared.take() {
            self.persist_project_label(&root);
        }
        if requests.rename.is_some() {
            self.modals.pending_rename = requests.rename.take();
        }
        if let Some(path) = requests.base_picker.take() {
            self.open_base_branch_picker(path);
        }
        if let Some(path) = requests.delete.take() {
            self.request_worktree_delete(&path);
        }
        if let Some(idx) = requests.create {
            self.modals.pending_create =
                Some(CreateState::Prompt { project_idx: idx, branch: String::new(), error: None });
        }
    }

    /// Applies the clicks that act on a workspace or session, and reports
    /// whether one of them activated a workspace.
    fn apply_sidebar_activations(&mut self, ctx: &Context, requests: &mut SidebarRequests) -> bool {
        let mut workspace_activated = false;
        if requests.home {
            self.activate_home(ctx);
            workspace_activated = true;
        }
        if let Some(path) = requests.activate.take() {
            self.activate_worktree(ctx, &path);
            workspace_activated = true;
        }
        if let Some((ws, id)) = requests.activate_session.take() {
            // A stale id (session reaped this frame) self-heals next frame:
            // active_session_index() misses and adopt_active_session picks
            // an existing shell, or the empty-workspace placeholder shows.
            self.current_workspace = ws.clone();
            self.sessions.set_active(ws, id);
            workspace_activated = true;
        }
        if let Some(id) = requests.close_session {
            self.request_close_session(ctx, id);
        }
        if let Some((ws, key, pane_id)) = requests.attach_pane.take() {
            // Switches first, same as `spawn_shell` below: a refusal
            // is only visible if the workspace it happened in is on screen.
            let switch = self.switch_for_attach(&ws, AttachFocus::Take);
            let unlisted = PaneTarget::unlisted(&key, &pane_id);
            if self.attach_pane(ctx, key, unlisted, &switch, None, AttachFocus::Take) {
                workspace_activated = true;
            } else {
                self.current_workspace = switch.from;
            }
        }
        if let Some(ws) = requests.spawn_shell.take() {
            // Spawning activates the workspace and the new session, matching
            // Ctrl+T and worktree-creation's open-on-done.  An `Err` here
            // arrived before the session record did, from a checkout git has
            // forgotten or a PTY opened inline, and hands the workspace
            // back rather than stranding the user on one with no shell, the
            // same reasoning as `activate_worktree`.  A PTY opened on a
            // worker fails after the record exists, so the switch stands and
            // `poll_pending_spawns` leaves the pane on the "no session"
            // placeholder: every workspace it could hand back to is one
            // `ensure_active_session` would spawn into and fail identically.
            let previous = std::mem::replace(&mut self.current_workspace, ws.clone());
            match self.spawn_session(ctx, ws.clone()) {
                Ok(_) => workspace_activated = true,
                Err(e) => {
                    self.current_workspace = previous;
                    self.report_spawn_failure(ctx, &ws, &e);
                },
            }
        }
        if let Some((path, name)) = requests.spawn_profile.take() {
            // Same activate-on-success and stale-row-recovery shape as
            // `spawn_shell`: a stale worktree row's `+` reaches
            // `report_spawn_failure` today, and a profile picked from the
            // same row's menu must un-grey it the same way.
            let ws = Some(path);
            let previous = std::mem::replace(&mut self.current_workspace, ws.clone());
            match self.spawn_profile_session_in(ctx, &name, ws.clone()) {
                Ok(_) => workspace_activated = true,
                Err(e) => {
                    self.current_workspace = previous;
                    self.report_spawn_failure(ctx, &ws, &e);
                },
            }
        }
        workspace_activated
    }
}

/// What the projects sidebar paints from, owned so the panel closure can
/// borrow `projects` mutably alongside it.
struct SidebarView {
    theme: Theme,
    probing: bool,
    reorder_mode: bool,
    session_drag: bool,
    /// The dragged session and the workspaces it may land in.
    drag_range: Option<(SessionId, Vec<WorkspaceKey>)>,
    cursor_row: Option<SidebarRow>,
    cursor_moved: bool,
    follow_row: Option<SidebarRow>,
    filtering: bool,
    membership: FilterMembership,
    filtered_empty: bool,
    home_rows: Vec<WorkspaceRowData>,
    home_active: bool,
    home_status: RowStatus<'static>,
    projects: Vec<ProjectView>,
    deleting_paths: HashSet<PathBuf>,
    creating: Vec<(usize, String)>,
    distros: Vec<wsl::WslDistro>,
    profile_names: Vec<String>,
    worktree_profiles: Vec<(String, String)>,
}

/// A view paired with the app's icon set.  The icons stay a borrow of their
/// own field, so the panel closure can still borrow `projects` mutably.
#[derive(Clone, Copy)]
struct SidebarPaint<'a> {
    view: &'a SidebarView,
    icons: &'a PaintedIcons,
}

/// `[ui.icons]` and each multiplexer's own glyph, with colors converted for
/// painting.
pub(super) struct PaintedIcons {
    ui: Icons<Color32>,
    panes: Vec<(MultiplexerKind, IconStyle<Color32>, BakedGlyph)>,
}

impl PaintedIcons {
    pub(super) fn new(config: &Config, multiplexers: &Multiplexers) -> Self {
        let panes = multiplexers
            .iter()
            .map(|multiplexer| {
                let (icon, default) = multiplexer.icon();
                (multiplexer.kind(), icon.map_color(rgb_to_color32), default)
            })
            .collect();
        Self { ui: config.ui.icons.map_colors(rgb_to_color32), panes }
    }

    /// The glyph marking a pane `multiplexer` owns, and its fallback.
    pub(super) fn pane(&self, multiplexer: MultiplexerKind) -> (&IconStyle<Color32>, BakedGlyph) {
        let (_, icon, default) = self
            .panes
            .iter()
            .find(|(kind, ..)| *kind == multiplexer)
            .expect("every multiplexer has an icon");
        (icon, *default)
    }
}

impl std::ops::Deref for PaintedIcons {
    type Target = Icons<Color32>;

    fn deref(&self) -> &Icons<Color32> {
        &self.ui
    }
}

impl SidebarView {
    fn scrolls(&self, is_cursor: bool) -> bool {
        is_cursor && self.cursor_moved
    }

    fn follows_home(&self) -> bool {
        self.follow_row == Some(SidebarRow::Home)
    }

    fn follows_session(&self, id: SessionId) -> bool {
        self.follow_row == Some(SidebarRow::Session(id))
    }

    // `Project`/`Worktree` rows carry a `PathBuf`; matching by reference
    // here (mirroring the `cursor_row` matches) keeps every scroll
    // check on the paint path allocation-free, follow target or not.
    fn follows_project(&self, root: &Path) -> bool {
        matches!(&self.follow_row, Some(SidebarRow::Project(r)) if r.as_path() == root)
    }

    fn follows_worktree(&self, path: &Path) -> bool {
        matches!(&self.follow_row, Some(SidebarRow::Worktree(p)) if p.as_path() == path)
    }
}

/// Membership for the active filter, resolved once so paint can skip
/// non-surviving rows.  While filtering, matched projects render their
/// matched worktrees regardless of `expanded`.  That is display-only, and
/// the flag is never written.
#[derive(Default)]
struct FilterMembership {
    home: bool,
    projects: HashSet<PathBuf>,
    worktrees: HashSet<PathBuf>,
    children: HashSet<SidebarRow>,
}

impl FilterMembership {
    fn of(filtering: bool, rows: &[SidebarRow]) -> Self {
        let mut membership = Self { home: true, ..Self::default() };
        if filtering {
            membership.home = false;
            for row in rows {
                match row {
                    SidebarRow::Home => membership.home = true,
                    SidebarRow::Project(root) => {
                        membership.projects.insert(root.clone());
                    },
                    SidebarRow::Worktree(path) => {
                        membership.worktrees.insert(path.clone());
                    },
                    SidebarRow::Session(_) | SidebarRow::Pane(_) => {
                        membership.children.insert(row.clone());
                    },
                }
            }
        }
        membership
    }
}

struct ProjectView {
    label: String,
    attention: bool,
    worktrees: Vec<WorktreeView>,
}

struct WorktreeView {
    label: String,
    pr: Option<PrInfo>,
    rows: Vec<WorkspaceRowData>,
    status: RowStatus<'static>,
    is_active: bool,
    /// What the liveness probe has seen since discovery ran, if anything.
    missing: Option<bool>,
}

/// What the paint pass asks for, applied once the panel closure has released
/// its borrows.
#[derive(Default)]
struct SidebarRequests {
    add_project: bool,
    reorder_toggled: bool,
    refresh: Option<usize>,
    remove: Option<ProjectRemoveState>,
    expand_toggled: Option<(PathBuf, bool)>,
    shell_override_changed: Option<PathBuf>,
    label_cleared: Option<PathBuf>,
    rename: Option<RenameState>,
    /// Drag-to-reorder: (dragged root, insert-before display index).
    reorder: Option<(PathBuf, usize)>,
    session_drop: Option<(SessionId, WorkspaceKey, usize)>,
    home: bool,
    activate: Option<PathBuf>,
    delete: Option<PathBuf>,
    create: Option<usize>,
    base_picker: Option<PathBuf>,
    spawn_shell: Option<WorkspaceKey>,
    spawn_profile: Option<(PathBuf, String)>,
    activate_session: Option<(WorkspaceKey, SessionId)>,
    close_session: Option<SessionId>,
    attach_pane: Option<(WorkspaceKey, PaneKey, String)>,
    /// Worktree rows painted on a probe frame, the only ones worth probing.
    drawn_worktrees: Vec<PathBuf>,
}

fn projects_header_buttons(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    requests: &mut SidebarRequests,
) {
    let theme = &paint.view.theme;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if icon_tooltip(
            styled_icon_button(
                ui,
                &paint.icons.add_project,
                DEFAULT_ADD_ICON,
                theme.text_dim,
                theme,
            ),
            "add project",
            theme.icon_tooltips,
        )
        .clicked()
        {
            requests.add_project = true;
        }
        // Lit while active: the mode is only visible as grips
        // on the rows, so the button has to say it's on.
        let (color, hint) = if paint.view.reorder_mode {
            (theme.accent, "done reordering")
        } else {
            (theme.text_dim, "reorder projects")
        };
        if icon_tooltip(
            styled_icon_button(ui, &paint.icons.reorder, DEFAULT_REORDER_ICON, color, theme),
            hint,
            theme.icon_tooltips,
        )
        .clicked()
        {
            requests.reorder_toggled = true;
        }
    });
}

/// Offers `row_rect` as a landing for the session being dragged, and records
/// the drop on release.  `slot` carries a session row's display index and id;
/// `None` is a workspace row.
fn session_drop_target(
    ui: &egui::Ui,
    paint: SidebarPaint<'_>,
    row_rect: egui::Rect,
    ws: &WorkspaceKey,
    slot: Option<(usize, SessionId)>,
    requests: &mut SidebarRequests,
) {
    let Some(dragged) = drop_candidate(paint.view.drag_range.as_ref(), ws, slot) else { return };
    let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else { return };
    if !row_rect.contains(pointer) {
        return;
    }
    let position = match slot {
        // A session row: the half the pointer is in decides.
        Some((idx, _)) => {
            if draw_drop_indicator(ui, row_rect, pointer, &paint.view.theme) {
                idx
            } else {
                idx + 1
            }
        },
        // A workspace row: its sessions start under it, so a
        // drop here means the front of that workspace.  This is
        // the only way to reach a workspace listing no session
        // rows, either an empty one or a single-session one below
        // the display threshold.
        None => {
            ui.painter().hline(
                row_rect.x_range(),
                row_rect.bottom(),
                drop_indicator_stroke(&paint.view.theme),
            );
            0
        },
    };
    if ui.input(|i| i.pointer.any_released()) {
        requests.session_drop = Some((dragged, ws.clone(), position));
        egui::DragAndDrop::clear_payload(ui.ctx());
    }
}

/// The session a drop on a row in `ws` would move: the dragged one, unless the
/// row is that session's own or `ws` lies outside its reorder range.
fn drop_candidate(
    drag_range: Option<&(SessionId, Vec<WorkspaceKey>)>,
    ws: &WorkspaceKey,
    slot: Option<(usize, SessionId)>,
) -> Option<SessionId> {
    let (dragged, range) = drag_range?;
    // The dragged row's own edges are no-ops, so offering them
    // as targets would paint a drop that does nothing.
    if slot.is_some_and(|(_, id)| id == *dragged) {
        return None;
    }
    range.contains(ws).then_some(*dragged)
}

fn paint_home_group(ui: &mut egui::Ui, paint: SidebarPaint<'_>, requests: &mut SidebarRequests) {
    let is_cursor = matches!(&paint.view.cursor_row, Some(SidebarRow::Home));
    let action = home_row(
        ui,
        paint.view.home_active,
        is_cursor,
        paint.view.scrolls(is_cursor) || paint.view.follows_home(),
        paint.view.home_status,
        paint.icons,
        &paint.view.theme,
    );
    if action.activate {
        requests.home = true;
    }
    if action.spawn {
        requests.spawn_shell = Some(None);
    }
    session_drop_target(ui, paint, action.rect, &None, None, requests);
    paint_workspace_children(ui, paint, &paint.view.home_rows, &None, requests);
}

/// The session and pane rows listed under the workspace `ws`.
fn paint_workspace_children(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    rows: &[WorkspaceRowData],
    ws: &WorkspaceKey,
    requests: &mut SidebarRequests,
) {
    // Only rows a reorder can move take a drop slot, and
    // the slot index counts those alone: a multiplexer pane's
    // place is the multiplexer's to decide, so it is neither a drag
    // subject nor a landing.
    let mut slot = 0usize;
    for row in rows {
        match row {
            WorkspaceRowData::Session(row) => {
                let is_cursor = matches!(&paint.view.cursor_row, Some(SidebarRow::Session(id)) if *id == row.id);
                let scroll = paint.view.scrolls(is_cursor) || paint.view.follows_session(row.id);
                let movable = row.managed.is_none();
                let act = session_row(
                    ui,
                    row,
                    is_cursor,
                    scroll,
                    paint.view.session_drag && movable,
                    paint.icons,
                    &paint.view.theme,
                );
                if act.activate {
                    requests.activate_session = Some((ws.clone(), row.id));
                }
                if act.close {
                    requests.close_session = Some(row.id);
                }
                if movable {
                    session_drop_target(ui, paint, act.rect, ws, Some((slot, row.id)), requests);
                    slot += 1;
                }
            },
            WorkspaceRowData::Pane(row) => {
                let is_cursor = matches!(
                    &paint.view.cursor_row,
                    Some(SidebarRow::Pane(key)) if *key == row.key
                );
                let scroll = paint.view.scrolls(is_cursor);
                let act = pane_row(ui, row, is_cursor, scroll, paint.icons, &paint.view.theme);
                if act.attach {
                    requests.attach_pane = Some((ws.clone(), row.key.clone(), row.pane_id.clone()));
                }
            },
        }
    }
}

/// The project's own row: its controls, cursor, reorder drop and context menu.
fn paint_project_header(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    idx: usize,
    project: &mut Project,
    requests: &mut SidebarRequests,
) {
    let theme = &paint.view.theme;
    let proj_attention = paint.view.projects.get(idx).is_some_and(|p| p.attention);
    // Bubble attention up to the project row only when the
    // project is collapsed.  Once expanded, the actual
    // worktree rows already show the dot, and doubling it
    // on the parent reads as noise.
    let show_proj_dot = proj_attention && !project.expanded;
    // Cloned out before the row closures borrow `project`
    // mutably: the trailing closure needs them for the
    // remove-confirmation prompt.
    let project_root = project.root.clone();
    let project_name = project.display_name().to_string();
    let mut expand_clicked = false;
    let mut name_resp: Option<egui::Response> = None;
    let row_rect = row_with_trailing(
        ui,
        |ui| {
            let (clicked, resp) = project_row_title(ui, paint, idx, project);
            expand_clicked = clicked;
            name_resp = Some(resp);
        },
        |ui| {
            project_row_controls(
                ui,
                paint,
                idx,
                &project_root,
                &project_name,
                show_proj_dot,
                requests,
            )
        },
    );
    if expand_clicked {
        project.expanded = !project.expanded;
        requests.expand_toggled = Some((project.root.clone(), project.expanded));
    }
    let header_is_cursor =
        matches!(&paint.view.cursor_row, Some(SidebarRow::Project(r)) if *r == project.root);
    let header_rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), row_rect.y_range());
    if header_is_cursor {
        paint_cursor_outline(ui, header_rect, theme);
    }
    if paint.view.scrolls(header_is_cursor) || paint.view.follows_project(&project.root) {
        ui.scroll_to_rect(header_rect, theme.scroll_align);
    }

    // Drop target for a reorder drag.  Detected against the
    // raw payload rather than a `dnd_drop_zone` widget so no
    // extra hover-sensing rect steals the row buttons' own
    // hover highlight.
    if let Some(dragged) = egui::DragAndDrop::payload::<DraggedProject>(ui.ctx()) {
        let pointer = ui.input(|i| i.pointer.interact_pos());
        if let Some(pointer) =
            pointer.filter(|p| row_rect.contains(*p) && dragged.0 != project.root)
        {
            let before = draw_drop_indicator(ui, row_rect, pointer, theme);
            if ui.input(|i| i.pointer.any_released()) {
                let insert_before = if before { idx } else { idx + 1 };
                requests.reorder = Some((dragged.0.clone(), insert_before));
                egui::DragAndDrop::clear_payload(ui.ctx());
            }
        }
    }

    // Right-click: rename the project, and choose which
    // shell its sessions use.
    if let Some(resp) = name_resp {
        resp.context_menu(|ui| project_context_menu(ui, paint, project, requests));
    }
}

/// The project row's grip, expand arrow and name.  Reports whether the arrow
/// was clicked, with the name's response for the context menu.
fn project_row_title(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    idx: usize,
    project: &Project,
) -> (bool, egui::Response) {
    let theme = &paint.view.theme;
    let icons = &paint.icons;
    ui.spacing_mut().item_spacing.x = ICON_CLUSTER_SPACING;
    if paint.view.reorder_mode {
        drag_handle(ui, theme).dnd_set_drag_payload(DraggedProject(project.root.clone()));
    }
    let (arrow_style, arrow_default, arrow_hint) = if project.expanded {
        (&icons.project_expanded, DEFAULT_PROJECT_EXPANDED_ICON, "collapse project")
    } else {
        (&icons.project_collapsed, DEFAULT_PROJECT_COLLAPSED_ICON, "expand project")
    };
    let expand_clicked = icon_tooltip(
        styled_icon_button(ui, arrow_style, arrow_default, theme.text_dim, theme),
        arrow_hint,
        theme.icon_tooltips,
    )
    .clicked();
    let name = paint.view.projects.get(idx).map_or(project.display_name(), |p| p.label.as_str());
    let (resp, galley) = truncating_label(
        ui,
        RichText::new(name).strong().small().color(theme.text),
        theme.text,
        egui::Sense::click(),
    );
    (expand_clicked, name_tooltip(resp, name, galley.elided, theme.sidebar_tooltips))
}

/// The project row's trailing buttons: remove, refresh, new worktree, and the
/// attention mark.
fn project_row_controls(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    idx: usize,
    root: &Path,
    name: &str,
    show_attention: bool,
    requests: &mut SidebarRequests,
) {
    let theme = &paint.view.theme;
    let icons = &paint.icons;
    if icon_tooltip(
        styled_icon_button(ui, &icons.remove_project, DEFAULT_CLOSE_ICON, theme.text_muted, theme),
        "remove from sidebar",
        theme.icon_tooltips,
    )
    .clicked()
    {
        requests.remove =
            Some(ProjectRemoveState { root: root.to_path_buf(), name: name.to_string() });
    }
    if icon_tooltip(
        styled_icon_button(ui, &icons.refresh, DEFAULT_REFRESH_ICON, theme.text_muted, theme),
        "refresh worktrees",
        theme.icon_tooltips,
    )
    .clicked()
    {
        requests.refresh = Some(idx);
    }
    if icon_tooltip(
        styled_icon_button(ui, &icons.new_worktree, DEFAULT_ADD_ICON, theme.text_muted, theme),
        "create new worktree",
        theme.icon_tooltips,
    )
    .clicked()
    {
        requests.create = Some(idx);
    }
    if show_attention {
        icon_tooltip(attention_mark(ui, icons, theme), ATTENTION_HINT, theme.icon_tooltips);
    }
}

fn project_context_menu(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    project: &mut Project,
    requests: &mut SidebarRequests,
) {
    if ui.button("Rename\u{2026}").clicked() {
        requests.rename = Some(RenameState {
            root: project.root.clone(),
            label: project.display_name().to_string(),
        });
        ui.close_menu();
    }
    if project.label.is_some() && ui.button("Reset name").clicked() {
        project.label = None;
        requests.label_cleared = Some(project.root.clone());
        ui.close_menu();
    }
    // The shell picker is hidden when there is
    // nothing to choose (no distros, no profiles)
    // so minimal setups see only the rename.
    if !paint.view.distros.is_empty() || !paint.view.profile_names.is_empty() {
        ui.separator();
        ui.label(RichText::new("Open in\u{2026}").color(paint.view.theme.text_muted).small());
        shell_override_menu(ui, paint, project, requests);
    }
}

/// The shell choices: automatic, the Windows shell, each WSL distro and each
/// profile, with the current override marked.
fn shell_override_menu(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    project: &mut Project,
    requests: &mut SidebarRequests,
) {
    let mark = |selected: bool| if selected { "• " } else { "   " };
    let auto = project.shell_override.is_none();
    if ui.button(format!("{}Auto (by location)", mark(auto))).clicked() {
        project.shell_override = None;
        requests.shell_override_changed = Some(project.root.clone());
        ui.close_menu();
    }
    let win = matches!(project.shell_override, Some(ShellChoice::Windows));
    if ui.button(format!("{}Windows shell", mark(win))).clicked() {
        project.shell_override = Some(ShellChoice::Windows);
        requests.shell_override_changed = Some(project.root.clone());
        ui.close_menu();
    }
    for distro in &paint.view.distros {
        let selected = matches!(
            &project.shell_override,
            Some(ShellChoice::Wsl(name)) if name == &distro.name
        );
        if ui.button(format!("{}WSL ({})", mark(selected), distro.name)).clicked() {
            project.shell_override = Some(ShellChoice::Wsl(distro.name.clone()));
            requests.shell_override_changed = Some(project.root.clone());
            ui.close_menu();
        }
    }
    for name in &paint.view.profile_names {
        let selected = matches!(
            &project.shell_override,
            Some(ShellChoice::Profile(n)) if n == name
        );
        if ui.button(format!("{}Profile: {}", mark(selected), name)).clicked() {
            project.shell_override = Some(ShellChoice::Profile(name.clone()));
            requests.shell_override_changed = Some(project.root.clone());
            ui.close_menu();
        }
    }
}

/// The worktree rows under an expanded or filter-matched project, then the
/// placeholders for its minimized creations.
fn paint_worktrees(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    idx: usize,
    project: &Project,
    requests: &mut SidebarRequests,
) {
    let states = paint.view.projects.get(idx).map_or(&[][..], |p| p.worktrees.as_slice());
    for (wt, state) in project.worktrees.iter().zip(states) {
        if paint.view.filtering && !paint.view.membership.worktrees.contains(&wt.path) {
            continue;
        }
        paint_worktree(ui, paint, wt, state, requests);
    }
    for (_, branch) in paint.view.creating.iter().filter(|(pi, _)| *pi == idx) {
        creating_row(ui, branch, paint.icons, &paint.view.theme);
    }
}

fn paint_worktree(
    ui: &mut egui::Ui,
    paint: SidebarPaint<'_>,
    wt: &Worktree,
    state: &WorktreeView,
    requests: &mut SidebarRequests,
) {
    let is_cursor =
        matches!(&paint.view.cursor_row, Some(SidebarRow::Worktree(p)) if *p == wt.path);
    let scroll = paint.view.scrolls(is_cursor) || paint.view.follows_worktree(&wt.path);
    let is_deleting = paint.view.deleting_paths.contains(&wt.path);
    // A `\\wsl.localhost\` stat boots the distro's
    // 9P server, so probing one would restart a VM
    // the user had shut down and hold it resident
    // for as long as its worktrees are listed.
    // WSL rows keep discovery's word.
    if paint.view.probing && matches!(wsl::classify(&wt.path), wsl::Location::Windows(_)) {
        requests.drawn_worktrees.push(wt.path.clone());
    }
    let action = worktree_row(ui, &WorktreeRowView {
        wt,
        missing: state.missing,
        display_name: &state.label,
        pr: state.pr.as_ref(),
        is_active: state.is_active,
        is_cursor,
        scroll_into_view: scroll,
        status: state.status,
        deleting: is_deleting,
        profiles: &paint.view.worktree_profiles,
        icons: paint.icons,
        theme: &paint.view.theme,
    });
    if action.activate {
        requests.activate = Some(wt.path.clone());
    }
    if action.delete {
        requests.delete = Some(wt.path.clone());
    }
    if action.spawn {
        requests.spawn_shell = Some(Some(wt.path.clone()));
    }
    if action.set_base {
        requests.base_picker = Some(wt.path.clone());
    }
    if let Some(name) = action.spawn_profile {
        requests.spawn_profile = Some((wt.path.clone(), name));
    }
    let ws = Some(wt.path.clone());
    session_drop_target(ui, paint, action.rect, &ws, None, requests);
    paint_workspace_children(ui, paint, &state.rows, &ws, requests);
}

pub(super) struct HomeAction {
    activate: bool,
    spawn: bool,
    /// Full-width row rect, for a drop target to test the pointer against.
    rect: egui::Rect,
}

pub(super) fn home_row(
    ui: &mut egui::Ui,
    is_active: bool,
    is_cursor: bool,
    scroll_into_view: bool,
    status: RowStatus<'_>,
    icons: &Icons<Color32>,
    theme: &Theme,
) -> HomeAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut spawn_clicked = false;
    let mut spawn_rect: Option<egui::Rect> = None;
    let mut hints = IconHints::default();
    // The leading and trailing groups run as sibling closures, so the status
    // slot's hint travels out separately and joins the rest afterwards.
    let mut status_hint = None;
    let frame = Frame::default().inner_margin(Margin { left: 6, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            row_with_trailing(
                ui,
                |ui| {
                    status_hint = paint_row_status_icon(
                        ui,
                        theme,
                        icons,
                        status,
                        &icons.home,
                        DEFAULT_HOME_ICON,
                        is_active,
                    );
                    ui.label(
                        RichText::new("Home")
                            .color(if is_active { theme.text } else { theme.text_dim })
                            .strong()
                            .small(),
                    );
                },
                |ui| {
                    let btn = styled_icon_button(
                        ui,
                        &icons.new_session,
                        DEFAULT_ADD_ICON,
                        theme.text_muted,
                        theme,
                    );
                    hints.add(btn.rect, "new shell");
                    spawn_rect = Some(btn.rect);
                    if btn.clicked() {
                        spawn_clicked = true;
                    }
                },
            );
        })
        .response
        .interact(egui::Sense::click());
    if let Some((rect, hint)) = status_hint {
        hints.add(rect, hint);
    }
    // The row carries no name tooltip of its own, so the icons' hints are the
    // only thing a hover here has to say.
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| resp);

    // Same z-order recovery as worktree_row: the retroactive frame interact
    // shadows the inner button, so route clicks inside its rect to spawn.
    if resp.clicked() && !spawn_clicked {
        if let (Some(rect), Some(pos)) = (spawn_rect, resp.interact_pointer_pos()) {
            if rect.contains(pos) {
                spawn_clicked = true;
            }
        }
    }

    let bg = if is_active {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        Color32::TRANSPARENT
    };
    if bg != Color32::TRANSPARENT {
        let rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
        ui.painter().set(bg_idx, egui::Shape::rect_filled(rect, 0.0, bg));
    }
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if scroll_into_view {
        ui.scroll_to_rect(full_rect, theme.scroll_align);
    }
    HomeAction { activate: resp.clicked() && !spawn_clicked, spawn: spawn_clicked, rect: full_rect }
}

pub(super) struct WorktreeAction {
    activate: bool,
    delete: bool,
    spawn: bool,
    set_base: bool,
    /// Name of the profile picked from the row's "Open session" menu, if any.
    spawn_profile: Option<String>,
    /// Full-width row rect, for a drop target to test the pointer against.
    rect: egui::Rect,
}

/// Sidebar placeholder for a worktree whose creation the user minimized: a
/// spinner stands in until `poll_pending_creates` refreshes the project and the
/// real worktree row takes its place.  Indentation and the leading glyph match
/// `worktree_row` so it lines up with its future sibling.
fn creating_row(ui: &mut egui::Ui, branch: &str, icons: &Icons<Color32>, theme: &Theme) {
    let s = theme.ui_scale;
    let frame = Frame::default().inner_margin(Margin { left: 16, right: 0, top: 3, bottom: 3 });
    frame.show(ui, |ui| {
        row_with_trailing(
            ui,
            |ui| {
                let (glyph, font, color) = resolve_icon(
                    &icons.worktree,
                    DEFAULT_WORKTREE_ICON,
                    theme.text_muted,
                    10.0,
                    10.0,
                    theme,
                );
                ui.label(RichText::new(glyph).color(color).font(font));
                let (resp, galley) = truncating_label(
                    ui,
                    RichText::new(branch).color(theme.text_muted).small(),
                    theme.text_muted,
                    egui::Sense::hover(),
                );
                let _ = name_tooltip(resp, branch, galley.elided, theme.sidebar_tooltips);
            },
            |ui| {
                braille_loader(ui, 12.0 * s, theme.accent);
            },
        );
    });
}

/// Badge glyph, color, and tooltip word for a PR state.
fn pr_badge<'a>(
    icons: &'a Icons<Color32>,
    theme: &Theme,
    state: PrState,
) -> (&'a IconStyle<Color32>, BakedGlyph, Color32, &'static str) {
    match state {
        PrState::Open => (&icons.pr_open, DEFAULT_PR_OPEN_ICON, theme.pr_open, "open"),
        PrState::Draft => (&icons.pr_draft, DEFAULT_PR_DRAFT_ICON, theme.pr_draft, "draft"),
        PrState::Merged => (&icons.pr_merged, DEFAULT_PR_MERGED_ICON, theme.pr_merged, "merged"),
        PrState::Closed => (&icons.pr_closed, DEFAULT_PR_CLOSED_ICON, theme.pr_closed, "closed"),
    }
}

/// Badge style, color, and tooltip for an upstream state.  The tooltip names
/// the upstream ref because the glyph cannot.
pub(super) fn upstream_badge<'a>(
    icons: &'a Icons<Color32>,
    theme: &Theme,
    state: &UpstreamState,
) -> (&'a IconStyle<Color32>, BakedGlyph, Color32, String) {
    match state {
        UpstreamState::Level { upstream } => (
            &icons.upstream_level,
            DEFAULT_UPSTREAM_LEVEL_ICON,
            theme.upstream_level,
            format!("tracks {upstream}"),
        ),
        UpstreamState::Diverged { upstream, ahead, behind } => (
            &icons.upstream_diverged,
            DEFAULT_UPSTREAM_DIVERGED_ICON,
            theme.upstream_diverged,
            format!("tracks {upstream} — {ahead} ahead, {behind} behind"),
        ),
        UpstreamState::Gone { upstream } => (
            &icons.upstream_gone,
            DEFAULT_UPSTREAM_GONE_ICON,
            theme.upstream_gone,
            format!("{upstream} is missing locally"),
        ),
        UpstreamState::Untracked => (
            &icons.upstream_untracked,
            DEFAULT_UPSTREAM_UNTRACKED_ICON,
            theme.upstream_untracked,
            "no upstream configured".to_string(),
        ),
    }
}

/// Width the worktree row's context menu is held to, so a long profile name
/// wraps onto a second line instead of stretching the popup to fit it.
const WORKTREE_MENU_MAX_WIDTH: f32 = 220.0;

/// What one worktree row paints from.
pub(super) struct WorktreeRowView<'a> {
    pub(super) wt: &'a Worktree,
    // What the liveness probe has seen since discovery ran, if anything.
    // `Some` overrides `wt.prunable` in both directions; `None` leaves it
    // standing.  Kept out of the flag itself because that also picks between
    // `git worktree remove` and a prune, and a probe must never decide that.
    pub(super) missing: Option<bool>,
    pub(super) display_name: &'a str,
    pub(super) pr: Option<&'a PrInfo>,
    pub(super) is_active: bool,
    pub(super) is_cursor: bool,
    pub(super) scroll_into_view: bool,
    pub(super) status: RowStatus<'static>,
    pub(super) deleting: bool,
    // Shell profiles offered in the row's "Open session" menu: `.0` is the
    // profile name (spawned and shown as the button label), `.1` is the
    // command shown on hover.
    pub(super) profiles: &'a [(String, String)],
    pub(super) icons: &'a Icons<Color32>,
    pub(super) theme: &'a Theme,
}

/// The worktree row's trailing buttons: whether each was clicked, and the rect
/// it occupies for routing a click the row response shadowed.
#[derive(Default)]
struct WorktreeRowClicks {
    delete: bool,
    delete_rect: Option<egui::Rect>,
    spawn: bool,
    spawn_rect: Option<egui::Rect>,
}

impl WorktreeRowClicks {
    fn route_shadowed_click(&mut self, row_clicked: bool, pointer: Option<egui::Pos2>) {
        if row_clicked && !self.delete && !self.spawn {
            if let Some(pos) = pointer {
                if self.delete_rect.is_some_and(|r| r.contains(pos)) {
                    self.delete = true;
                } else if self.spawn_rect.is_some_and(|r| r.contains(pos)) {
                    self.spawn = true;
                }
            }
        }
    }

    fn activates(&self, deleting: bool, row_clicked: bool) -> bool {
        !deleting && row_clicked && !self.delete && !self.spawn
    }
}

/// The delete and new-shell buttons, then the PR and upstream badges.
fn worktree_row_controls(
    ui: &mut egui::Ui,
    row: &WorktreeRowView,
    prunable: bool,
    hints: &mut IconHints,
) -> WorktreeRowClicks {
    let (theme, icons) = (row.theme, row.icons);
    let mut clicks = WorktreeRowClicks::default();
    // Mid-removal the row is inert: swap its controls for a
    // spinner so the user sees the delete is in flight.
    if row.deleting {
        braille_loader(ui, 12.0 * theme.ui_scale, theme.accent);
        return clicks;
    }
    if !row.wt.is_main {
        let hover = if prunable { "prune worktree" } else { "delete worktree and branch" };
        let btn = styled_icon_button(
            ui,
            &icons.delete_worktree,
            DEFAULT_CLOSE_ICON,
            theme.text_muted,
            theme,
        );
        hints.add(btn.rect, hover);
        clicks.delete_rect = Some(btn.rect);
        clicks.delete = btn.clicked();
    }
    let btn = styled_icon_button(ui, &icons.new_session, DEFAULT_ADD_ICON, theme.text_muted, theme);
    hints.add(btn.rect, "new shell");
    clicks.spawn_rect = Some(btn.rect);
    clicks.spawn = btn.clicked();
    if let Some(info) = row.pr {
        let (style, default_glyph, color, word) = pr_badge(icons, theme, info.state);
        let rect = paint_badge(ui, theme, style, default_glyph, color);
        hints.add(rect, format!("PR #{} — {word}", info.number));
    }
    if let Some(state) = row.wt.upstream.as_ref() {
        let (style, default_glyph, color, tip) = upstream_badge(icons, theme, state);
        let rect = paint_badge(ui, theme, style, default_glyph, color);
        hints.add(rect, tip);
    }
    clicks
}

/// Paint one status-sized badge glyph and return the rect it claimed.
fn paint_badge(
    ui: &mut egui::Ui,
    theme: &Theme,
    style: &IconStyle<Color32>,
    default_glyph: BakedGlyph,
    color: Color32,
) -> egui::Rect {
    let (glyph, font, color) = resolve_icon(style, default_glyph, color, 10.0, 10.0, theme);
    let (rect, _) = ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, font, color);
    rect
}

/// The status icon and the name, returning the icon's hint and whether the
/// name was elided.
fn worktree_row_name(
    ui: &mut egui::Ui,
    row: &WorktreeRowView,
    prunable: bool,
) -> (Option<(egui::Rect, String)>, bool) {
    let (theme, icons) = (row.theme, row.icons);
    let (default_icon, default_glyph) = if row.wt.is_main {
        (&icons.worktree_main, DEFAULT_WORKTREE_MAIN_ICON)
    } else {
        (&icons.worktree, DEFAULT_WORKTREE_ICON)
    };
    let name_color = if prunable || row.deleting {
        theme.text_muted
    } else if row.is_active {
        theme.text
    } else {
        theme.text_dim
    };
    let status_hint = paint_row_status_icon(
        ui,
        theme,
        icons,
        row.status,
        default_icon,
        default_glyph,
        row.is_active,
    );
    let (_, galley) = truncating_label(
        ui,
        RichText::new(row.display_name).small().color(name_color),
        name_color,
        egui::Sense::hover(),
    );
    (status_hint, galley.elided)
}

/// The row's framed content with its tooltips attached, and the trailing
/// buttons' clicks.
fn worktree_row_frame(
    ui: &mut egui::Ui,
    row: &WorktreeRowView,
    prunable: bool,
) -> (egui::Response, WorktreeRowClicks) {
    let theme = row.theme;
    let mut hints = IconHints::default();
    let mut clicks = WorktreeRowClicks::default();
    let mut name_elided = false;
    // The leading and trailing groups run as sibling closures, so the status
    // slot's hint travels out separately and joins the rest afterwards.
    let mut status_hint = None;
    // right: 0 keeps the worktree `×` at the same x as the project row's `×`,
    // which has no frame margin and sits flush against the panel's outer padding.
    let frame = Frame::default().inner_margin(Margin { left: 16, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            row_with_trailing(
                ui,
                |ui| (status_hint, name_elided) = worktree_row_name(ui, row, prunable),
                |ui| clicks = worktree_row_controls(ui, row, prunable, &mut hints),
            );
        })
        .response
        .interact(egui::Sense::click());
    if let Some((rect, hint)) = status_hint {
        hints.add(rect, hint);
    }
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| {
        if prunable {
            resp.on_hover_text("worktree directory is missing — × prunes it")
        } else {
            name_tooltip(resp, row.display_name, name_elided, theme.sidebar_tooltips)
        }
    });
    (resp, clicks)
}

pub(super) fn worktree_row(ui: &mut egui::Ui, row: &WorktreeRowView) -> WorktreeAction {
    let theme = row.theme;
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    // Discovery's word, corrected by whatever the probe has seen since.  The
    // main worktree is never offered for pruning, so it never greys either.
    let prunable = worktree_looks_gone(row.wt, row.missing);
    let (resp, mut clicks) = worktree_row_frame(ui, row, prunable);

    // Frame allocates its space at end-of-show, so its retroactive `interact`
    // registers *after* the inner button in egui's z-order — meaning clicks on
    // the × land on this row response, not the button.  Recover by routing
    // clicks whose position falls inside the button rect to delete.
    clicks.route_shadowed_click(resp.clicked(), resp.interact_pointer_pos());

    let (set_base, spawn_profile) = worktree_row_menu(&resp, row);

    let bg = if row.is_active {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        Color32::TRANSPARENT
    };
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if bg != Color32::TRANSPARENT {
        ui.painter().set(bg_idx, egui::Shape::rect_filled(full_rect, 0.0, bg));
    }
    if row.is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if row.scroll_into_view {
        ui.scroll_to_rect(full_rect, theme.scroll_align);
    }
    WorktreeAction {
        // A prunable row is still worth clicking when shells are homed there;
        // `activate_worktree` turns the ones that aren't into the prune hint.
        activate: clicks.activates(row.deleting, resp.clicked()),
        delete: clicks.delete,
        spawn: clicks.spawn,
        set_base,
        spawn_profile,
        rect: full_rect,
    }
}

/// The row's context menu, returning whether "Set base branch" was picked and
/// the profile picked from "Open session", if any.
fn worktree_row_menu(resp: &egui::Response, row: &WorktreeRowView) -> (bool, Option<String>) {
    let mut set_base_clicked = false;
    let mut spawn_profile_clicked: Option<String> = None;
    resp.context_menu(|ui| {
        if ui.button("Set base branch…").clicked() {
            set_base_clicked = true;
            ui.close_menu();
        }
        if !row.profiles.is_empty() {
            ui.separator();
            ui.label(RichText::new("Open session").color(row.theme.text_muted).small());
            ui.set_max_width(WORKTREE_MENU_MAX_WIDTH);
            for (i, (name, command)) in row.profiles.iter().enumerate() {
                let btn = ui.button(profile_menu_label(i + 1, name));
                if btn.on_hover_text(command.as_str()).clicked() {
                    spawn_profile_clicked = Some(name.clone());
                    ui.close_menu();
                }
            }
        }
    });
    (set_base_clicked, spawn_profile_clicked)
}

pub(super) struct SessionRowAction {
    activate: bool,
    close: bool,
    /// Full-width row rect, for a drop target to test the pointer against.
    rect: egui::Rect,
}

/// `draggable` makes the whole row the drag handle rather than adding a grip:
/// a session row is a tab, where a project row's own controls are what a click
/// there is usually for.
pub(super) fn session_row(
    ui: &mut egui::Ui,
    row: &SessionRowData,
    is_cursor: bool,
    scroll_into_view: bool,
    draggable: bool,
    icons: &PaintedIcons,
    theme: &Theme,
) -> SessionRowAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut close_clicked = false;
    let mut hints = IconHints::default();
    let mut close_rect: Option<egui::Rect> = None;
    let mut title_elided = false;
    // The leading and trailing groups run as sibling closures, so the leading
    // slots' hints travel out separately and join the rest afterwards.
    let mut status_hint = None;
    let mut managed_slot = None;
    // One indent level deeper than worktree rows (16); right: 0 keeps the ×
    // at the same x as the other rows' trailing icons.
    let frame = Frame::default().inner_margin(Margin { left: 28, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            let title_color = if row.is_active { theme.text } else { theme.text_dim };
            row_with_trailing(
                ui,
                |ui| {
                    status_hint = paint_row_status_icon(
                        ui,
                        theme,
                        icons,
                        RowStatus {
                            pinged: row.needs_attention,
                            done: row.done,
                            activity: row.activity,
                            managed: row.managed.as_ref(),
                        },
                        &icons.session,
                        DEFAULT_SESSION_ICON,
                        row.is_active,
                    );
                    if let Some(managed) = &row.managed {
                        let rect = paint_managed_mark(ui, icons, managed, theme, theme.text_muted);
                        managed_slot = Some((rect, managed_tooltip(managed)));
                    }
                    let (_, galley) = truncating_label(
                        ui,
                        row_name_text(ui, &row.name, title_color, theme.text_muted),
                        title_color,
                        egui::Sense::hover(),
                    );
                    title_elided = galley.elided;
                },
                |ui| {
                    let btn = styled_icon_button(
                        ui,
                        &icons.close_session,
                        DEFAULT_CLOSE_ICON,
                        theme.text_muted,
                        theme,
                    );
                    hints.add(btn.rect, close_button_hint(row.managed.is_some()));
                    close_rect = Some(btn.rect);
                    if btn.clicked() {
                        close_clicked = true;
                    }
                },
            );
        })
        .response
        .interact(if draggable { egui::Sense::click_and_drag() } else { egui::Sense::click() });
    if let Some((rect, hint)) = status_hint {
        hints.add(rect, hint);
    }
    if let Some((rect, hint)) = managed_slot {
        hints.add(rect, hint);
    }
    // A managed row answers with the harness's own sentence wherever the
    // pointer is not on an icon: the row the user attached is the one they
    // ask how to leave, and the name tooltip cannot say it.
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| match &row.managed {
        Some(managed) if theme.icon_tooltips => resp.on_hover_text(managed_tooltip(managed)),
        _ => name_tooltip(resp, &row.name.text, title_elided, theme.sidebar_tooltips),
    });

    // Frame allocates its space at end-of-show, so its retroactive `interact`
    // registers *after* the inner button in egui's z-order — meaning clicks on
    // the × land on this row response, not the button.  Recover by routing
    // clicks whose position falls inside the button rect to close.
    if resp.clicked() && !close_clicked {
        if let (Some(rect), Some(pos)) = (close_rect, resp.interact_pointer_pos()) {
            if rect.contains(pos) {
                close_clicked = true;
            }
        }
    }

    let bg = if row.is_displayed {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        Color32::TRANSPARENT
    };
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if bg != Color32::TRANSPARENT {
        ui.painter().set(bg_idx, egui::Shape::rect_filled(full_rect, 0.0, bg));
    }
    if is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if scroll_into_view {
        ui.scroll_to_rect(full_rect, theme.scroll_align);
    }
    if draggable {
        resp.dnd_set_drag_payload(DraggedSession(row.id));
    }
    SessionRowAction {
        activate: resp.clicked() && !close_clicked,
        close: close_clicked,
        rect: full_rect,
    }
}

/// A row's name as two spans: the context, then the identity.  Nothing
/// separates them but weight — a punctuation mark here would spell out a
/// relationship the colours already show, and the status word one used to
/// join only repeated the mark two slots to its left.
///
/// One `LayoutJob` rather than two labels, for the reasons `path_text` gives:
/// no `item_spacing` gap, no second response competing for the row's click,
/// and elision that measures the whole stream.
fn row_name_text(
    ui: &egui::Ui,
    name: &RowName,
    text_color: Color32,
    context_color: Color32,
) -> egui::WidgetText {
    let Some(context) = &name.context else {
        return RichText::new(&name.text).small().color(text_color).into();
    };
    let size = egui::TextStyle::Small.resolve(ui.style()).size;
    // A hand-built job does not inherit the ui's text valign the way RichText
    // does, so it must be carried across or the text sits off-centre against
    // the marks beside it.
    let valign = ui.text_valign();
    let mut job = egui::text::LayoutJob::default();
    for (text, color) in [(format!("{context} "), context_color), (name.text.clone(), text_color)] {
        job.append(&text, 0.0, egui::TextFormat {
            font_id: egui::FontId::new(size, egui::FontFamily::Proportional),
            color,
            valign,
            ..Default::default()
        });
    }
    job.into()
}

/// Paint the harness mark and return the rect it claimed, so the caller can
/// hang the hint on it.
fn paint_managed_mark(
    ui: &mut egui::Ui,
    icons: &PaintedIcons,
    managed: &Managed,
    theme: &Theme,
    color: Color32,
) -> egui::Rect {
    // 10.0 is what the status marks beside it use.  Both multiplexer glyphs
    // are fitted to a capital M's box when the baked face is built, so the
    // same size puts them on one optical line with `◇` and `●`.
    let (icon, default) = icons.pane(managed.multiplexer);
    let (glyph, font, glyph_color) = resolve_icon(icon, default, color, 10.0, 10.0, theme);
    ui.label(RichText::new(glyph).color(glyph_color).font(font)).rect
}

/// A multiplexer's pane nothing is attached to.  Drawn in `theme.text_dim`
/// because it is listed but not live, the same weight `worktree_gone` gives a
/// row whose checkout has been removed.  An attached pane has an ordinary
/// session row instead, so no pane is ever drawn twice.
///
/// Not draggable and carries no drop-target rect: such a pane has no position
/// in the session order to reorder into.
fn pane_row(
    ui: &mut egui::Ui,
    row: &PaneRowData,
    is_cursor: bool,
    scroll_into_view: bool,
    icons: &PaintedIcons,
    theme: &Theme,
) -> PaneRowAction {
    // Reserve a slot *before* the label so the hover bg paints beneath it.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let frame = Frame::default().inner_margin(Margin { left: 28, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            row_with_trailing(
                ui,
                |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
                    if let Some(status) = row.managed.status {
                        paint_status_mark(ui, ShownState::from(status), icons, rect, theme);
                    }
                    paint_managed_mark(ui, icons, &row.managed, theme, theme.text_dim);
                    let text = row_name_text(ui, &row.name, theme.text_dim, theme.text_muted);
                    let _ = truncating_label(ui, text, theme.text_dim, egui::Sense::hover());
                },
                |_ui| {},
            );
        })
        .response
        .interact(egui::Sense::click());
    let resp =
        if theme.icon_tooltips { resp.on_hover_text(managed_tooltip(&row.managed)) } else { resp };

    let bg = if resp.hovered() { theme.row_hover_bg } else { Color32::TRANSPARENT };
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if bg != Color32::TRANSPARENT {
        ui.painter().set(bg_idx, egui::Shape::rect_filled(full_rect, 0.0, bg));
    }
    if is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if scroll_into_view {
        ui.scroll_to_rect(full_rect, None);
    }
    PaneRowAction { attach: resp.clicked() }
}

impl AlacritreeApp {
    fn toggle_project_filter(&mut self, action: NamedAction) {
        if let Some(key) = project_filter_identity(action) {
            self.sidebar.filter.toggle(key);
        }
    }
}

impl Action for action::SidebarTop {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.fresh_sidebar_model().move_cursor(Step::First);
    }
}

impl Action for action::SidebarBottom {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.fresh_sidebar_model().move_cursor(Step::Last);
    }
}

impl Action for action::SidebarNextProject {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.fresh_sidebar_model().move_cursor(Step::Project(1));
    }
}

impl Action for action::SidebarPreviousProject {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.fresh_sidebar_model().move_cursor(Step::Project(-1));
    }
}

impl Action for action::DeleteSelected {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        match app.sidebar.model.cursor().cloned() {
            Some(SidebarRow::Session(id)) => app.request_close_session(ctx, id),
            Some(SidebarRow::Worktree(path)) => app.request_worktree_delete(&path),
            Some(SidebarRow::Project(root)) => {
                if let Some(p) = app.projects.iter().find(|p| p.root == root) {
                    app.modals.pending_project_remove =
                        Some(ProjectRemoveState { name: p.display_name().to_string(), root });
                }
            },
            Some(SidebarRow::Home) | Some(SidebarRow::Pane(_)) | None => {},
        }
    }
}

impl Action for action::RenameSelected {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        // Only project rows carry an editable label; sessions and
        // worktrees take their names from the terminal title and the
        // `[ui] worktree_name` template.
        if let Some(SidebarRow::Project(root)) = app.sidebar.model.cursor().cloned() {
            if let Some(p) = app.projects.iter().find(|p| p.root == root) {
                app.modals.pending_rename =
                    Some(RenameState { root, label: p.display_name().to_string() });
            }
        }
    }
}

impl Action for action::ToggleProjectExpanded {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        let Some(cursor) = app.sidebar.model.cursor().cloned() else {
            return;
        };
        let root = {
            let session_workspace = |id: SessionId| {
                app.sessions.iter().find(|s| s.id == id).map(|s| s.working_directory.clone())
            };
            row_project_root(&app.projects, session_workspace, &cursor)
        };
        if let Some(root) = root {
            let expanded = app.projects.iter().find(|p| p.root == root).is_some_and(|p| p.expanded);
            app.set_project_expanded(&root, !expanded);
            // Collapsing hides the cursored child; move the cursor to
            // the header so it doesn't point at a now-invisible row.
            if expanded && !matches!(cursor, SidebarRow::Project(_)) {
                app.sidebar.model.set_cursor(SidebarRow::Project(root));
            }
        }
    }
}

impl Action for action::ClearProjectFilters {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.sidebar.filter.clear_toggles();
    }
}

impl Action for action::ToggleLeftSidebar {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.show_left_sidebar = !app.show_left_sidebar;
        // A deliberate visibility change opts out of the auto-shown
        // round trip, and a hidden sidebar cannot keep keyboard focus.
        app.sidebar_auto_shown = false;
        if !app.show_left_sidebar && app.focus == PaneFocus::ProjectsSidebar {
            app.focus = PaneFocus::Terminal;
        }
        app.persist_sidebars();
    }
}

impl Action for action::ToggleSidebarFocus {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        match app.focus {
            PaneFocus::Terminal => app.focus_sidebar(),
            PaneFocus::ProjectsSidebar => app.focus_terminal(),
            // Toggle stays "left <-> terminal"; from the right panel it
            // hops to the left one rather than doing nothing.
            PaneFocus::GitSidebar => app.focus_sidebar(),
        }
    }
}

impl Action for action::CloseSession {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        let cursored = if app.focus == PaneFocus::ProjectsSidebar {
            match app.sidebar.model.cursor() {
                Some(SidebarRow::Session(id)) => Some(*id),
                _ => None,
            }
        } else {
            None
        };
        let target =
            cursored.or_else(|| app.active_session_index().map(|idx| app.sessions[idx].id));
        if let Some(id) = target {
            app.request_close_session(ctx, id);
        }
    }
}

impl Action for action::FocusProjectsSidebar {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        if app.focus != PaneFocus::ProjectsSidebar {
            app.focus_sidebar();
        }
    }
}

impl Action for action::ToggleSessionsFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_project_filter((*self).into());
    }
}

impl Action for action::ToggleAttentionFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_project_filter((*self).into());
    }
}

impl Action for action::TogglePrOpenFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_project_filter((*self).into());
    }
}

impl Action for action::TogglePrDraftFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_project_filter((*self).into());
    }
}

impl Action for action::TogglePrMergedFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_project_filter((*self).into());
    }
}

impl Action for action::TogglePrClosedFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_project_filter((*self).into());
    }
}

/// Whether a workspace survives the projects panel's toggle dimension.
pub(super) fn project_toggles_pass(
    apply: bool,
    toggle_sessions: bool,
    has_sessions: bool,
    toggle_attention: bool,
    needs_attention: bool,
) -> bool {
    if !apply {
        return true;
    }
    (!toggle_sessions || has_sessions) && (!toggle_attention || needs_attention)
}

/// Whether a workspace counts as occupied for the sessions toggle: it holds a
/// live session or, when `counts_detached` is set, a listed multiplexer pane
/// nothing is attached to.  `session_workspaces` is the workspace of every
/// live session; a folded lone shell (absent from `listed` below the row
/// threshold, but still in `session_workspaces`) passes either way.
pub(super) fn sessions_filter_passes(
    session_workspaces: &[WorkspaceKey],
    listed: &sidebar_nav::ListedRows,
    key: &WorkspaceKey,
    counts_detached: bool,
) -> bool {
    session_workspaces.contains(key)
        || (counts_detached
            && listed.get(key).is_some_and(|entries| {
                entries.iter().any(|e| matches!(e, sidebar_nav::WorkspaceEntry::Pane(_)))
            }))
}

/// The toggle identities the projects panel accepts.  The PR identities exist
/// only when polling does, or every PR state would read as unknown and the
/// filters could only ever empty the panel.
pub(super) fn project_filter_toggles(pr_status: bool) -> &'static [char] {
    if pr_status { &['s', 'a', 'o', 'd', 'm', 'c'] } else { &['s', 'a'] }
}

/// The projects-panel toggle a named action flips, or `None` for an action that
/// is not one of its filters.  `PanelFilter::toggle` ignores an identity it does
/// not allow and dispatch falls through on an unmatched action, so nothing at
/// the call site can catch a wrong pairing — assert it here instead.
pub(super) fn project_filter_identity(action: NamedAction) -> Option<char> {
    match action {
        NamedAction::ToggleSessionsFilter(_) => Some('s'),
        NamedAction::ToggleAttentionFilter(_) => Some('a'),
        NamedAction::TogglePrOpenFilter(_) => Some('o'),
        NamedAction::TogglePrDraftFilter(_) => Some('d'),
        NamedAction::TogglePrMergedFilter(_) => Some('m'),
        NamedAction::TogglePrClosedFilter(_) => Some('c'),
        _ => None,
    }
}

/// Whether any toggle dimension narrows the projects panel this frame —
/// session presence, attention, or PR state. `project_self` falls back to
/// plain fuzzy matching only when this is false.
pub(super) fn any_project_toggle_active(
    toggle_sessions: bool,
    toggle_attention: bool,
    any_pr: bool,
) -> bool {
    toggle_sessions || toggle_attention || any_pr
}

/// Whether a worktree survives the projects panel's PR dimension. Inert
/// when no PR toggle is active, so a worktree passes regardless of what
/// `pr_matches` holds for it. Once a PR toggle is active, a worktree
/// missing from `pr_matches` is excluded — its PR lookup hasn't landed.
pub(super) fn worktree_pr_passes(
    any_pr: bool,
    pr_matches: &HashMap<PathBuf, bool>,
    path: &Path,
) -> bool {
    !any_pr || pr_matches.get(path).copied().unwrap_or(false)
}

/// Whether `build_project_rows` resolves session and pane names for
/// `child_matches` this frame.  `[ui] search_depth` at its "workspaces"
/// default answers false unconditionally, so no child name is ever computed
/// and a query costs what matching workspace names alone costs.
pub(super) fn search_reaches_children(depth: SearchDepth, query_is_empty: bool) -> bool {
    depth == SearchDepth::Sessions && !query_is_empty
}

/// Whether the projects panel is filtering on PR state this frame.  A toggle
/// the scope has stood down narrows nothing, so it must not pull the cache
/// generation into the reconciler or reach `gh` for a collapsed project.
pub(super) fn any_pr_toggle_active(filter: &PanelFilter, scope: SearchScope) -> bool {
    filter.toggles_apply(scope)
        && ['o', 'd', 'm', 'c'].into_iter().any(|key| filter.is_toggled(key))
}

/// Whether this worktree's PR state is polled this frame.  Collapsed projects
/// normally cost no `gh` processes, but a PR filter has to see every row or it
/// would hide worktrees for want of a lookup it declined to start.
pub(super) fn should_poll_pr(pr_enabled: bool, expanded: bool, any_pr_toggle: bool) -> bool {
    pr_enabled && (expanded || any_pr_toggle)
}

/// Drag-and-drop payload for reordering the project list.  Carries the dragged
/// project's root rather than its index so a background refresh that shifts the
/// list mid-drag can't drop onto the wrong project.
#[derive(Clone)]
pub(super) struct DraggedProject(pub(super) PathBuf);

/// Drag-and-drop payload for reordering sessions.  Carries the id rather than
/// a position so a spawn, close or reorder mid-drag can't retarget the drop.
#[derive(Clone)]
pub(super) struct DraggedSession(pub(super) SessionId);

/// Everything a sidebar session row needs, snapshotted before the panel
/// closure so rendering doesn't borrow `self.sessions`.
pub(super) struct SessionRowData {
    pub(super) id: SessionId,
    pub(super) name: RowName,
    pub(super) needs_attention: bool,
    pub(super) done: bool,
    pub(super) activity: SessionActivity,
    /// This workspace's remembered active session (accent icon).
    pub(super) is_active: bool,
    /// Active *and* the workspace is current — the session on screen
    /// (row background highlight).
    pub(super) is_displayed: bool,
    /// Set while this session is attached to a harness-managed pane, so an
    /// attached agent's row still says where it lives and how to leave.
    pub(super) managed: Option<Managed>,
}

/// One painted row under a workspace, in the order the sidebar draws them.
/// Attaching turns a pane row into a session row in place, so the two travel
/// as one list rather than as two blocks that would reorder on attach.
pub(super) enum WorkspaceRowData {
    Session(SessionRowData),
    Pane(PaneRowData),
}

impl WorkspaceRowData {
    /// Whether any of `rows` is a session of alacritree's own.  The workspace
    /// row shows aggregate attention and activity only while none is: with a
    /// list on screen, repeating its summary above it reads as noise.
    pub(super) fn any_session(rows: &[Self]) -> bool {
        rows.iter().any(|row| matches!(row, Self::Session(_)))
    }
}

/// Everything a sidebar pane row needs, snapshotted before the panel closure
/// so rendering doesn't borrow the multiplexers.
pub(super) struct PaneRowData {
    pub(super) key: PaneKey,
    pub(super) pane_id: String,
    pub(super) name: RowName,
    pub(super) managed: Managed,
}

impl PaneRowData {
    pub(super) fn new(key: PaneKey, pane: &Pane, managed: Managed) -> Self {
        Self { key, pane_id: pane.pane_id.clone(), name: pane_display_name(pane), managed }
    }
}

/// A row's name in two parts, ranked by weight rather than punctuation: the
/// identity, and the category standing in front of it as context.  `context`
/// is absent when the identity is already the category, so a row never spells
/// one thing twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RowName {
    pub(super) text: String,
    pub(super) context: Option<String>,
}

impl RowName {
    pub(super) fn plain(text: String) -> Self {
        Self { text, context: None }
    }

    /// What a text filter matches this row on.  Both parts, because the row
    /// shows both, and the context only when the row spells it out: a query
    /// naming a category an identity already carries must not match twice.
    pub(super) fn search_text(self) -> String {
        match self.context {
            Some(context) => format!("{} {}", self.text, context),
            None => self.text,
        }
    }
}

/// The name a multiplexer reports for a pane, and `None` when it reports
/// none.  The kind rides along as context unless it says the same thing as
/// the title.  What a titleless pane falls back to differs by row, so each
/// caller says so itself rather than passing its answer through here.
pub(super) fn pane_row_name(agent: &Pane) -> Option<RowName> {
    let title = agent.title.clone()?;
    let context = agent.kind.clone().filter(|kind| *kind != title);
    Some(RowName { text: title, context })
}

/// The sidebar row, the palette row and the text filter must all resolve an
/// agent's name the same way, or the filter stops matching what the other two
/// paint.  Falls back from the pane's title, to the agent's kind, to the last
/// six characters of its terminal id.  A listed row has nothing better than
/// the terminal id's tail behind the kind, so the kind takes the name rather
/// than standing in front of six characters nobody reads.
pub(super) fn pane_display_name(agent: &Pane) -> RowName {
    pane_row_name(agent).unwrap_or_else(|| {
        RowName::plain(agent.kind.clone().unwrap_or_else(|| {
            let id = &agent.terminal_id;
            let skip = id.chars().count().saturating_sub(6);
            id.chars().skip(skip).collect()
        }))
    })
}

/// Agent titles commonly lead with their own decorative mark. Once the row
/// paints a semantic agent/loader status, retaining that mark beside it would
/// reintroduce the vendor-specific icon set this status model replaces.
/// What an attached session's row is called.  On Linux and WSL an attach is
/// full passthrough, so the pane on screen is the multiplexer's and the row
/// names it the way the listed row would.  Attaching must not rename the row
/// under the user.  A pane that reports no title keeps the title its own PTY
/// set, with the kind in front of it.
pub(super) fn session_row_name(
    pty_title: &str,
    activity: SessionActivity,
    agent: Option<&Pane>,
) -> RowName {
    let Some(agent) = agent else {
        return RowName::plain(session_row_title(pty_title, activity));
    };
    pane_row_name(agent).unwrap_or_else(|| RowName {
        text: session_row_title(pty_title, activity),
        context: agent.kind.clone(),
    })
}

pub(super) fn session_row_title(title: &str, activity: SessionActivity) -> String {
    if activity.is_agent() {
        let trimmed = title.trim_start();
        if let Some(first) = trimmed.chars().next() {
            let rest = &trimmed[first.len_utf8()..];
            if !first.is_ascii() && rest.chars().next().is_some_and(char::is_whitespace) {
                let rest = rest.trim_start();
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
    }
    title.to_string()
}

/// The "index. name" label for a profile entry in the worktree row's "Open
/// session" menu, 1-based to match `SpawnProfile1`..`SpawnProfile9` in the
/// palette.
pub(super) fn profile_menu_label(index: usize, name: &str) -> String {
    format!("{index}. {name}")
}

pub(super) struct PaneRowAction {
    pub(super) attach: bool,
}

/// What the × on a session row does.  Ending a harness-managed session ends
/// the attach client and nothing else — the pane keeps running under the
/// harness, and the row it came from comes back — so calling that a close
/// promises a destruction that does not happen.
pub(super) fn close_button_hint(managed: bool) -> &'static str {
    if managed { "detach session" } else { "close session" }
}

/// The weight and colour every reorder drop line is drawn with, shared so the
/// project and session drags cannot drift apart.
fn drop_indicator_stroke(theme: &Theme) -> Stroke {
    Stroke::new(2.0 * theme.ui_scale, theme.accent)
}

/// Paint the line a reorder drop would land on, at the row edge nearest the
/// pointer, and report whether that edge is the top — which is what "insert
/// before this row" means for both the project and the session drag.
fn draw_drop_indicator(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    pointer: egui::Pos2,
    theme: &Theme,
) -> bool {
    let before = pointer.y < row_rect.center().y;
    let y = if before { row_rect.top() } else { row_rect.bottom() };
    ui.painter().hline(row_rect.x_range(), y, drop_indicator_stroke(theme));
    before
}

/// A grip that a project row can be dragged by to reorder it.  Drag-sensing
/// only, so a plain click still falls through to the row's other controls.
fn drag_handle(ui: &mut egui::Ui, theme: &Theme) -> egui::Response {
    let s = theme.ui_scale;
    let size = egui::vec2(12.0 * s, 16.0 * s);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::drag());
    let color = if resp.hovered() || resp.dragged() { theme.text_dim } else { theme.text_muted };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "⠿",
        egui::FontId::proportional(12.0 * s),
        color,
    );
    resp.on_hover_cursor(egui::CursorIcon::Grab)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_row_takes_no_drop_of_its_own_session() {
        let range = (7, vec![None]);
        assert_eq!(drop_candidate(Some(&range), &None, Some((0, 7))), None);
        assert_eq!(drop_candidate(Some(&range), &None, Some((1, 8))), Some(7));
    }

    #[test]
    fn a_workspace_outside_the_reorder_range_takes_no_drop() {
        let range = (7, vec![None]);
        let elsewhere = Some(PathBuf::from("/repo/other"));
        assert_eq!(drop_candidate(Some(&range), &elsewhere, None), None);
        assert_eq!(drop_candidate(Some(&range), &None, None), Some(7));
    }

    fn worktree_buttons() -> WorktreeRowClicks {
        let at = |x: f32| egui::Rect::from_min_size(egui::pos2(x, 0.0), egui::vec2(10.0, 10.0));
        WorktreeRowClicks {
            delete_rect: Some(at(100.0)),
            spawn_rect: Some(at(80.0)),
            ..Default::default()
        }
    }

    #[test]
    fn a_row_click_over_the_delete_button_deletes_instead_of_activating() {
        let mut clicks = worktree_buttons();
        clicks.route_shadowed_click(true, Some(egui::pos2(105.0, 5.0)));
        assert!(clicks.delete && !clicks.spawn);
        assert!(!clicks.activates(false, true));

        let mut clicks = worktree_buttons();
        clicks.route_shadowed_click(true, Some(egui::pos2(20.0, 5.0)));
        assert!(!clicks.delete && !clicks.spawn);
        assert!(clicks.activates(false, true));
    }

    #[test]
    fn a_row_click_over_the_spawn_button_spawns_instead_of_activating() {
        let mut clicks = worktree_buttons();
        clicks.route_shadowed_click(true, Some(egui::pos2(85.0, 5.0)));
        assert!(clicks.spawn && !clicks.delete);
        assert!(!clicks.activates(false, true));
    }

    #[test]
    fn a_button_already_clicked_keeps_the_row_click_from_rerouting() {
        let mut clicks = worktree_buttons();
        clicks.spawn = true;
        clicks.route_shadowed_click(true, Some(egui::pos2(105.0, 5.0)));
        assert!(clicks.spawn && !clicks.delete);
    }

    #[test]
    fn a_worktree_mid_removal_ignores_a_row_click() {
        let mut clicks = worktree_buttons();
        clicks.route_shadowed_click(true, Some(egui::pos2(20.0, 5.0)));
        assert!(!clicks.activates(true, true));
    }

    /// The "workspaces" depth never reaches a child, whatever the query:
    /// `child_matches` is not built, so `build_project_rows` feeds
    /// `sidebar_nav::filtered_rows` a `None` child predicate and a query
    /// naming a session matches only that session's workspace.
    #[test]
    fn search_reaches_children_stays_false_at_the_workspaces_default() {
        assert!(!search_reaches_children(SearchDepth::Workspaces, false));
        assert!(!search_reaches_children(SearchDepth::Workspaces, true));
    }

    /// The "sessions" depth resolves child names for a non-empty query, so a
    /// session or agent row can match by its own name rather than only
    /// through its workspace.
    #[test]
    fn search_reaches_children_only_with_sessions_depth_and_a_live_query() {
        assert!(search_reaches_children(SearchDepth::Sessions, false));
        assert!(!search_reaches_children(SearchDepth::Sessions, true));
    }

    #[test]
    fn profile_menu_label_numbers_from_one() {
        assert_eq!(profile_menu_label(1, "WSL"), "1. WSL");
        assert_eq!(profile_menu_label(2, "cmd"), "2. cmd");
    }

    #[test]
    fn pane_display_name_keeps_a_short_terminal_id_whole() {
        // `saturating_sub(6)` exists precisely for ids shorter than the tail
        // it takes; a plain `- 6` would panic on this one.
        let agent = Pane { terminal_id: "t1".into(), ..crate::test_util::listed_agent(None) };
        assert_eq!(pane_display_name(&agent), RowName::plain("t1".into()));
    }

    /// The filter matches what the row paints, so a query naming the category
    /// in front of an identity finds the row that shows both.
    #[test]
    fn search_text_carries_the_context_behind_the_identity() {
        let name = RowName { text: "primary".into(), context: Some("claude".into()) };
        assert_eq!(name.search_text(), "primary claude");
    }

    /// A row whose identity is already its category paints one word, so the
    /// filter searches one word rather than the same word twice.
    #[test]
    fn search_text_of_a_plain_name_is_the_name() {
        assert_eq!(RowName::plain("claude".into()).search_text(), "claude");
    }

    /// Ending a multiplexer-managed session ends the attach and leaves the pane
    /// running, so the control cannot call itself a close.
    #[test]
    fn the_close_control_is_a_detach_on_a_managed_row() {
        assert_eq!(close_button_hint(true), "detach session");
        assert_eq!(close_button_hint(false), "close session");
    }

    #[test]
    fn a_wide_search_stands_down_the_project_toggles() {
        // Toggled on, workspace fails both: excluded while the toggles apply,
        // included once a wide search stands them down.
        assert!(!project_toggles_pass(true, true, false, true, false));
        assert!(project_toggles_pass(false, true, false, true, false));
    }

    #[test]
    fn sessions_filter_counts_a_detached_agent_bucketed_under_home() {
        let listed =
            sidebar_nav::ListedRows::from([(None, vec![sidebar_nav::WorkspaceEntry::Pane(
                crate::test_util::pane_key(Side::Native, "term_home"),
            )])]);
        assert!(sessions_filter_passes(&[], &listed, &None, true));
    }

    #[test]
    fn a_pr_toggle_alone_makes_any_toggle_active() {
        assert!(!any_project_toggle_active(false, false, false));
        assert!(any_project_toggle_active(false, false, true));
    }

    #[test]
    fn worktree_pr_passes_is_inert_without_a_pr_toggle() {
        let path = PathBuf::from("/worktree");
        let mut pr_matches = HashMap::new();
        pr_matches.insert(path.clone(), false);
        assert!(worktree_pr_passes(false, &pr_matches, &path));
    }

    #[test]
    fn worktree_pr_passes_follows_the_map_once_a_pr_toggle_is_active() {
        let path = PathBuf::from("/worktree");
        let mut pr_matches = HashMap::new();
        pr_matches.insert(path.clone(), true);
        assert!(worktree_pr_passes(true, &pr_matches, &path));
        pr_matches.insert(path.clone(), false);
        assert!(!worktree_pr_passes(true, &pr_matches, &path));
    }

    #[test]
    fn worktree_pr_passes_excludes_a_worktree_missing_from_the_map() {
        let path = PathBuf::from("/worktree");
        let pr_matches: HashMap<PathBuf, bool> = HashMap::new();
        assert!(!worktree_pr_passes(true, &pr_matches, &path));
    }

    /// Dispatch cannot catch a wrong pairing: `toggle` drops an identity the
    /// panel does not allow, and an action with no arm falls through to the
    /// scroll handler.  Swapping two identities here is otherwise invisible.
    #[test]
    fn the_projects_filter_actions_map_to_their_identities() {
        for (action, identity) in [
            (NamedAction::ToggleSessionsFilter(action::ToggleSessionsFilter), Some('s')),
            (NamedAction::ToggleDetachedSessionsFilter(action::ToggleDetachedSessionsFilter), None),
            (NamedAction::ToggleAttentionFilter(action::ToggleAttentionFilter), Some('a')),
            (NamedAction::TogglePrOpenFilter(action::TogglePrOpenFilter), Some('o')),
            (NamedAction::TogglePrDraftFilter(action::TogglePrDraftFilter), Some('d')),
            (NamedAction::TogglePrMergedFilter(action::TogglePrMergedFilter), Some('m')),
            (NamedAction::TogglePrClosedFilter(action::TogglePrClosedFilter), Some('c')),
            (NamedAction::ClearProjectFilters(action::ClearProjectFilters), None),
            (NamedAction::ToggleModifiedFilter(action::ToggleModifiedFilter), None),
            (NamedAction::ToggleDeletedFilter(action::ToggleDeletedFilter), None),
            (NamedAction::ToggleUntrackedFilter(action::ToggleUntrackedFilter), None),
            (NamedAction::ToggleSearchScope(action::ToggleSearchScope), None),
            (NamedAction::RefreshPrStatus(action::RefreshPrStatus), None),
            (NamedAction::Paste(action::Paste), None),
        ] {
            assert_eq!(project_filter_identity(action), identity, "{action:?}");
            if let Some(key) = identity {
                assert!(
                    project_filter_toggles(true).contains(&key),
                    "{action:?} maps to {key}, which the panel would drop"
                );
            }
        }
    }

    #[test]
    fn the_pr_identities_exist_only_when_polling_does() {
        assert_eq!(project_filter_toggles(false), &['s', 'a']);
        assert_eq!(project_filter_toggles(true), &['s', 'a', 'o', 'd', 'm', 'c']);
    }

    /// Guards the staging dependency: the four PR actions already dispatch to
    /// `project_filter.toggle`, and `toggle` silently ignores an identity the
    /// filter does not allow — so a narrow slice here makes them dead keys.
    #[test]
    fn the_pr_actions_reach_a_configured_projects_filter() {
        let mut f = PanelFilter::new(project_filter_toggles(true));
        for key in ['o', 'd', 'm', 'c'] {
            f.toggle(key);
            assert!(f.is_toggled(key), "{key} must be a live identity");
        }
    }

    #[test]
    fn any_pr_toggle_active_ignores_the_non_pr_identities() {
        let mut f = PanelFilter::new(project_filter_toggles(true));
        assert!(!any_pr_toggle_active(&f, SearchScope::Filtered));
        f.toggle('s');
        assert!(
            !any_pr_toggle_active(&f, SearchScope::Filtered),
            "a session toggle is not a PR toggle"
        );
        f.toggle('o');
        assert!(any_pr_toggle_active(&f, SearchScope::Filtered));
    }

    /// A search under `All` stands the toggles down for row selection, so the
    /// PR dimension narrows nothing — polling collapsed projects for it and
    /// rebuilding on every banked result would both be pure cost.
    #[test]
    fn a_stood_down_pr_toggle_does_not_read_as_active() {
        let mut f = PanelFilter::new(project_filter_toggles(true));
        f.toggle('o');
        f.on_text("/");
        f.on_text("a");

        assert!(any_pr_toggle_active(&f, SearchScope::Filtered));
        assert!(!any_pr_toggle_active(&f, SearchScope::All));
    }

    #[test]
    fn a_pr_filter_reaches_into_collapsed_projects() {
        assert!(!should_poll_pr(true, false, false), "collapsed and unfiltered: no lookup");
        assert!(should_poll_pr(true, false, true), "a PR filter must see collapsed rows");
        assert!(should_poll_pr(true, true, false));
        assert!(!should_poll_pr(false, true, true), "disabled means never");
    }
}
