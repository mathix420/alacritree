//! The git status panel: the state only it owns, the paint pass over it, the
//! keyboard navigation and filter that drive its cursor, and the diff panes
//! its rows open.

use super::*;
use crate::diff_viewer::{
    self, DiffRequest, DiffSource, Launch, Program, Section, Target, diff_key,
};
use crate::tools::{self, Tool};

/// The toggle identities the git panel accepts: modified, deleted, untracked.
pub(super) const GIT_FILTER_TOGGLES: &[char] = &['m', 'd', 'u'];

pub(super) struct GitPanel {
    /// Fuzzy-search query and `m`/`d`/`u` change-kind toggle state for the git
    /// panel.  Transient: never persisted.
    pub(super) filter: PanelFilter,
    /// Git-panel cursor, identified by `(section, path)`.  Rebuilt every render
    /// pass from `rows`, so it survives the 1.5 s status refresh.
    pub(super) cursor: Option<git_nav::GitRow>,
    /// One-shot: scroll the git cursor row into view on the next paint.
    pub(super) cursor_moved: bool,
    /// Render-order git rows the cursor steps over, refreshed by the render pass.
    pub(super) rows: Vec<git_nav::GitRow>,
    /// Resolved default-branch ref backing the git panel's branch-diff rows,
    /// refreshed by the render pass so Enter opens the same diff a click would.
    pub(super) branch_base: Option<String>,
    /// The focus toggle opened a hidden git sidebar; returning focus closes it
    /// again so a keyboard round trip leaves the layout untouched.
    pub(super) auto_shown: bool,
    pub(super) status: HashMap<PathBuf, StatusCache>,
    /// Per-worktree override of the git panel's diff base, keyed by worktree
    /// path.  Mirrors `state.toml`; written through `state::set_base_branch`.
    pub(super) base_branch_overrides: HashMap<PathBuf, String>,
}

impl GitPanel {
    pub(super) fn new(base_branch_overrides: HashMap<PathBuf, String>) -> Self {
        Self {
            status: HashMap::new(),
            filter: PanelFilter::new(GIT_FILTER_TOGGLES),
            cursor: None,
            cursor_moved: false,
            rows: Vec::new(),
            branch_base: None,
            auto_shown: false,
            base_branch_overrides,
        }
    }
}

struct GitSidebarView {
    theme: Theme,
    path: PathBuf,
    workspace_home: Option<String>,
    status: GitStatus,
    pr_info: Option<PrInfo>,
    branch_base: Option<String>,
    active_diff_key: Option<String>,
    filtering: bool,
    staged_count: SectionCount,
    unstaged_count: SectionCount,
    branch_count: SectionCount,
    staged_visible: HashSet<String>,
    unstaged_visible: HashSet<String>,
    branch_visible: HashSet<String>,
    /// The section header button's label.
    review_label: String,
    /// The whole-section review each header offers, when enabled and available.
    staged_review: Option<Target>,
    unstaged_review: Option<Target>,
    branch_review: Option<Target>,
    cursor_row: Option<git_nav::GitRow>,
    cursor_moved: bool,
}

#[derive(Default)]
struct GitSidebarRequests {
    diff: Option<Target>,
    open_picker: Option<PathBuf>,
}

impl AlacritreeApp {
    /// Arrow/Enter/Escape navigation while the git sidebar owns keyboard
    /// focus.  Same event-drain shape as `handle_sidebar_nav`: consumes only
    /// unmodified nav keys, leaving modifier-bound shortcuts for
    /// `handle_shortcuts`.
    pub(super) fn handle_git_sidebar_nav(&mut self, ctx: &Context) {
        let filter = &mut self.git_panel.filter;
        let shortcuts = &self.shortcuts;
        let steps: Vec<SidebarNavStep> = ctx.input_mut(|i| {
            let mut steps = Vec::new();
            let text_keys = keys_paired_with_text(&i.events);
            let mut idx = 0;
            i.events.retain(|ev| {
                let produced_text = text_keys[idx];
                idx += 1;
                match ev {
                    egui::Event::Text(text) => match filter.on_text(text) {
                        Some(outcome) => {
                            steps.push(SidebarNavStep::Filter(outcome));
                            false
                        },
                        None => true,
                    },
                    egui::Event::Key { key, pressed: true, modifiers, .. } => drain_search_or_nav(
                        &mut steps,
                        filter,
                        shortcuts,
                        *key,
                        *modifiers,
                        produced_text,
                    ),
                    _ => true,
                }
            });
            steps
        });
        for step in steps {
            match step {
                SidebarNavStep::Filter(outcome) => self.apply_git_filter_outcome(ctx, outcome),
                SidebarNavStep::Nav(key) => self.apply_git_sidebar_nav(ctx, key),
                SidebarNavStep::SearchAction(action) => {
                    self.dispatch_action(ctx, BindingAction::Named(action), ActionOrigin::Keyboard);
                },
            }
        }
    }

    fn apply_git_filter_outcome(&mut self, _ctx: &Context, outcome: panel_filter::Outcome) {
        use panel_filter::Outcome;
        match outcome {
            Outcome::FilterChanged => self.after_git_filter_changed(),
            Outcome::Consumed => {},
            Outcome::MoveCursor(delta) => self.move_git_cursor(delta),
            Outcome::LeavePanel => self.focus_terminal(),
        }
    }

    /// Repair the git cursor after the row set narrows or widens: recompute the
    /// filtered rows from the cached status so the next key event acts on them,
    /// then keep the cursor where it is when still visible, else fall to the
    /// first surviving row.
    pub(super) fn after_git_filter_changed(&mut self) {
        self.recompute_git_rows();
        let next = git_nav::ensure_cursor(&self.git_panel.rows, self.git_panel.cursor.as_ref());
        if next.as_ref() != self.git_panel.cursor.as_ref() {
            self.git_panel.cursor = next;
            self.git_panel.cursor_moved = true;
        }
    }

    fn move_git_cursor(&mut self, delta: i32) {
        let cursor = match self.git_panel.cursor.clone() {
            Some(c) if self.git_panel.rows.contains(&c) => c,
            _ => {
                if let Some(first) = self.git_panel.rows.first().cloned() {
                    self.set_git_cursor(first);
                }
                return;
            },
        };
        if let Some(row) = git_nav::step(&self.git_panel.rows, &cursor, delta) {
            self.set_git_cursor(row);
        }
    }

    /// Rebuild `rows` from the cached status under the active filter,
    /// without polling.  The render pass recomputes the same way from a fresh
    /// poll; this keeps the row set current between frames so a filter change
    /// and a following key event in the same batch agree on the rows.
    pub(super) fn recompute_git_rows(&mut self) {
        let Some(path) = self.active_session_path() else {
            self.git_panel.rows.clear();
            return;
        };
        let Some(status) = self.git_panel.status.get(&path).map(|c| c.last().clone()) else {
            self.git_panel.rows.clear();
            return;
        };
        self.git_panel.rows = self.filtered_git_rows(&status).rows;
    }

    /// Apply the git panel's kind toggles and fuzzy query to a status snapshot.
    /// With no kind toggle active every kind passes; otherwise the active
    /// toggles union (`m`: Modified/Renamed, `d`: Deleted, `u`: Untracked/Added).
    /// Conflicted rows and the branch-diff section are handled by `visible_rows`.
    fn filtered_git_rows(&mut self, status: &GitStatus) -> git_nav::GitRows {
        let apply = self.git_panel.filter.toggles_apply(self.sidebar_focus_state.search_scope);
        let m = apply && self.git_panel.filter.is_toggled('m');
        let d = apply && self.git_panel.filter.is_toggled('d');
        let u = apply && self.git_panel.filter.is_toggled('u');
        let kind_pass = move |k: ChangeKind| git_toggles_pass(m, d, u, k);
        let filter = &mut self.git_panel.filter;
        let mut query_pass = |path: &str| filter.matches(path);
        git_nav::visible_rows(
            &status.staged,
            &status.unstaged,
            &status.branch_diff,
            &kind_pass,
            &mut query_pass,
        )
    }

    fn apply_git_sidebar_nav(&mut self, ctx: &Context, key: egui::Key) {
        use egui::Key;
        let cursor = match self.git_panel.cursor.clone() {
            Some(c) if self.git_panel.rows.contains(&c) => c,
            // Stale or unseeded cursor (status refreshed the row out from under
            // it): land on the first row and let the next press act from there.
            _ => {
                if let Some(first) = self.git_panel.rows.first().cloned() {
                    self.set_git_cursor(first);
                }
                return;
            },
        };
        match key {
            Key::ArrowUp => {
                if let Some(row) = git_nav::step(&self.git_panel.rows, &cursor, -1) {
                    self.set_git_cursor(row);
                }
            },
            Key::ArrowDown => {
                if let Some(row) = git_nav::step(&self.git_panel.rows, &cursor, 1) {
                    self.set_git_cursor(row);
                }
            },
            Key::Enter => {
                if let Some(req) =
                    git_row_diff_request(&cursor, self.git_panel.branch_base.as_deref())
                {
                    self.open_diff(ctx, Target::Row(req));
                }
            },
            Key::Escape => self.focus_terminal(),
            _ => {},
        }
    }

    fn set_git_cursor(&mut self, row: git_nav::GitRow) {
        if self.git_panel.cursor.as_ref() != Some(&row) {
            self.git_panel.cursor = Some(row);
            self.git_panel.cursor_moved = true;
        }
    }

    fn project_default_branch_for(&self, path: &Path) -> Option<String> {
        for project in &self.projects {
            for wt in &project.worktrees {
                if wt.path == path {
                    return project.default_branch.clone();
                }
            }
        }
        None
    }

    pub(super) fn open_base_branch_picker(&mut self, worktree: PathBuf) {
        let detected = self.project_default_branch_for(&worktree);
        let job_worktree = worktree.clone();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
            crate::worktree::list_branches(&job_worktree, blocking)
        });
        self.modals.pending_base_branch = Some(BaseBranchPicker {
            worktree,
            query: String::new(),
            branches: None,
            branches_job: Some(job),
            detected,
            cursor: 0,
        });
    }

    pub(super) fn apply_base_branch(&mut self, worktree: PathBuf, branch: Option<String>) {
        match &branch {
            Some(b) => {
                self.git_panel.base_branch_overrides.insert(worktree.clone(), b.clone());
            },
            None => {
                self.git_panel.base_branch_overrides.remove(&worktree);
            },
        }
        // The next `StatusCache::poll` sees the changed hint and recomputes;
        // nothing to invalidate by hand.
        state::mutate(|s| state::set_base_branch(s, &worktree, branch));
    }

    fn git_sidebar_view(&mut self, ctx: &Context) -> Option<GitSidebarView> {
        let theme = self.theme;
        let active_diff_key = self.active_diff_key();
        let path = match self.active_session_path() {
            Some(p) => p,
            None => {
                // No workspace, no rows: keep the cursor model from
                // acting on stale rows left by a previous workspace.
                self.git_panel.rows.clear();
                self.git_panel.branch_base = None;
                return None;
            },
        };
        let workspace_home = self.workspace_home(&path);

        let project_default = self.project_default_branch_for(&path);
        let cache = self
            .git_panel
            .status
            .entry(path.clone())
            .or_insert_with(|| StatusCache::new(path.clone()));

        // Use whatever branch the cache already knows to query the PR
        // cache without waiting for a fresh compute. The first frame may
        // be `None`, which `pr_cache.poll` handles by returning early.
        let cached_branch = cache.current_branch().map(str::to_string);
        let pr_info = self.pr_cache.poll(&path, cached_branch.as_deref(), ctx);
        let effective_default = effective_base_branch(
            self.git_panel.base_branch_overrides.get(&path).map(String::as_str),
            pr_info.as_ref().map(|p| p.base_branch.as_str()),
            project_default.as_deref(),
        );
        // Clone the non-blocking poll result before cursor repair mutates `self`.
        let status = cache.poll(effective_default.as_deref(), ctx).clone();

        // Prefer the resolved ref (e.g. `refs/remotes/origin/main`) so
        // the cursor's Enter-to-diff matches the branch section's rows.
        let git_branch_base =
            status.default_branch_resolved.clone().or_else(|| status.default_branch.clone());
        let diff_viewer = &self.config.integrations.diff_viewer;
        let review = |section: Section| {
            let target = Target::Section(section);
            (diff_viewer.section_buttons && diff_viewer::opens(&diff_viewer.viewer, &target))
                .then_some(target)
        };
        let staged_review = review(Section::Staged);
        let unstaged_review = review(Section::Unstaged);
        let branch_review =
            git_branch_base.clone().and_then(|base| review(Section::Branch { base }));
        let review_label = diff_viewer.button_icon.clone();
        let filtering = self.git_panel.filter.is_filtering();
        let filtered = self.filtered_git_rows(&status);
        let staged_count = filtered.staged;
        let unstaged_count = filtered.unstaged;
        let branch_count = filtered.branch;
        self.git_panel.rows = filtered.rows;
        let mut staged_visible: HashSet<String> = HashSet::new();
        let mut unstaged_visible: HashSet<String> = HashSet::new();
        let mut branch_visible: HashSet<String> = HashSet::new();
        for row in &self.git_panel.rows {
            match row.section {
                GitSection::Staged => &mut staged_visible,
                GitSection::Unstaged => &mut unstaged_visible,
                GitSection::Branch => &mut branch_visible,
            }
            .insert(row.path.clone());
        }
        self.git_panel.branch_base = git_branch_base.clone();
        if self.focus == PaneFocus::GitSidebar {
            let mut repaired =
                git_nav::ensure_cursor(&self.git_panel.rows, self.git_panel.cursor.as_ref());
            // An unseeded cursor lands on the row backing the open diff
            // when there is one, so focusing the panel points at what
            // the user is already looking at.
            if self.git_panel.cursor.is_none() {
                if let Some(active) = active_diff_key.as_deref() {
                    if let Some(row) = self.git_panel.rows.iter().find(|r| {
                        git_row_diff_request(r, git_branch_base.as_deref())
                            .is_some_and(|req| diff_key(&req) == active)
                    }) {
                        repaired = Some(row.clone());
                    }
                }
            }
            self.git_panel.cursor = repaired;
        }
        let cursor_row =
            if self.focus == PaneFocus::GitSidebar { self.git_panel.cursor.clone() } else { None };
        let cursor_moved = std::mem::take(&mut self.git_panel.cursor_moved);

        Some(GitSidebarView {
            theme,
            path,
            workspace_home,
            status,
            pr_info,
            branch_base: git_branch_base,
            active_diff_key,
            filtering,
            staged_count,
            unstaged_count,
            branch_count,
            staged_visible,
            unstaged_visible,
            branch_visible,
            review_label,
            staged_review,
            unstaged_review,
            branch_review,
            cursor_row,
            cursor_moved,
        })
    }

    pub(super) fn show_git_sidebar(&mut self, ctx: &Context, panel_frame: Frame) -> egui::Rect {
        let view = self.git_sidebar_view(ctx);
        let theme = self.theme;
        let mut requests = GitSidebarRequests::default();
        let panel_resp = SidePanel::right("right_sidebar")
            .resizable(true)
            .default_width(300.0 * theme.ui_scale)
            .min_width(220.0 * theme.ui_scale)
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Sidebar rows are click targets, not selectable prose; the
                // default I-beam-and-select on labels is the wrong affordance.
                ui.style_mut().interaction.selectable_labels = false;
                apply_scrollbar_style(ui, self.config.ui.scrollbar);
                ui.horizontal(|ui| {
                    panel_header_filter_ui(
                        ui,
                        "Git",
                        &self.git_panel.filter,
                        &self.icons.search,
                        &theme,
                        self.git_panel.filter.toggles_apply(self.sidebar_focus_state.search_scope),
                    );
                });
                ui.separator();

                match &view {
                    Some(view) => paint_git_sidebar_status(ui, view, &mut requests),
                    None => {
                        ScrollArea::vertical().show(ui, |ui| {
                            ui.label(
                                RichText::new("Open a worktree from the left sidebar.")
                                    .color(theme.text_dim)
                                    .small(),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Ctrl+G to toggle").small().color(theme.text_muted),
                            );
                        });
                    },
                }
            });
        self.apply_git_sidebar_requests(ctx, requests);
        if self.config.ui.sidebar_click_focus
            && self.focus != PaneFocus::GitSidebar
            && pressed_on_panel(ctx, &panel_resp.response)
        {
            self.focus_git_sidebar();
        }
        panel_resp.response.rect
    }

    fn apply_git_sidebar_requests(&mut self, ctx: &Context, requests: GitSidebarRequests) {
        if let Some(request) = requests.diff {
            self.open_diff(ctx, request);
        }
        if let Some(path) = requests.open_picker {
            self.open_base_branch_picker(path);
        }
    }

    /// Choosing a row or section either opens, replaces, or closes the
    /// workspace's single diff pane. Dropping the old `Session` sends
    /// `Msg::Shutdown` to the event loop and exits the viewer cleanly.
    fn open_diff(&mut self, ctx: &Context, target: Target) {
        let Some(workspace) = self.current_workspace.clone() else {
            return;
        };
        let new_key = target.key();
        let existing = self
            .sessions
            .iter()
            .find(|s| {
                s.working_directory.as_deref() == Some(&workspace)
                    && matches!(&s.kind, SessionKind::Diff { .. })
            })
            .map(|s| (s.id, matches!(&s.kind, SessionKind::Diff { key } if key == &new_key)));
        if let Some((id, true)) = existing {
            self.close_session(ctx, id);
            return;
        }
        let Some(launch) = diff_viewer::plan(&self.config.integrations.diff_viewer.viewer, &target)
        else {
            return;
        };
        if let Some((id, _)) = existing {
            self.sessions.remove(&[id], self.config.ui.sidebar_focus);
        }

        let (program, args) = match wsl::classify(&workspace) {
            wsl::Location::Wsl { distro, .. } => wsl_diff_command(ctx, &distro, &workspace, launch),
            wsl::Location::Windows(_) => native_diff_command(launch),
        };
        let title = match &target {
            Target::Row(req) => format!(
                "diff: {}",
                path_style::render(&req.file, self.config.ui.path_style.diff_title, None)
            ),
            Target::Section(section) => format!("diff: {}", section.label()),
        };
        let (size, cell_size) = self.next_spawn_geometry();
        let (session, request) = Session::pending_command(
            ctx.clone(),
            &self.config,
            Some(workspace.clone()),
            size,
            cell_size,
            program,
            args,
            title,
            SessionKind::Diff { key: new_key },
        );
        match self.open_session(session, request) {
            Ok(id) => {
                self.sessions.set_active(Some(workspace), id);
            },
            Err(e) => {
                self.modals.error_dialog = Some(format!("failed to open diff: {e}"));
            },
        }
    }

    /// Key of the diff currently displayed in this workspace, if any.  Used by
    /// the sidebar to highlight the originating row so the toggle-on-reclick
    /// behavior is discoverable.
    fn active_diff_key(&self) -> Option<String> {
        self.sessions.iter().find_map(|s| {
            if s.working_directory != self.current_workspace {
                return None;
            }
            if let SessionKind::Diff { key } = &s.kind { Some(key.clone()) } else { None }
        })
    }
}

fn native_program(program: &Program) -> String {
    match program {
        Program::Tool(tool) => tools::program(*tool),
        Program::Custom { path, .. } => path.clone(),
    }
}

fn native_pager_value(program: &Program, args: &[String]) -> String {
    match program {
        Program::Tool(tool) => diff_viewer::executable_pager_command(&tools::program(*tool), args),
        Program::Custom { path, .. } => diff_viewer::pager_command(path, args),
    }
}

fn native_diff_command(launch: Launch) -> (String, Vec<String>) {
    match launch {
        Launch::Pager { pager, pager_args, git_args } => diff_viewer::native_pager_command(
            &tools::program(Tool::Git),
            &native_pager_value(&pager, &pager_args),
            &git_args,
        ),
        Launch::Direct { program, args } => (native_program(&program), args),
    }
}

fn wsl_program(ctx: &Context, distro: &str, program: &Program) -> Option<String> {
    match program {
        Program::Tool(tool) => {
            let repaint = ctx.clone();
            tools::wsl_resolved(*tool, distro, move || repaint.request_repaint())
        },
        Program::Custom { wsl_path, .. } => wsl_path.clone(),
    }
}

fn program_name(program: &Program) -> &str {
    match program {
        Program::Tool(tool) => tool.name(),
        Program::Custom { path, .. } => path,
    }
}

fn wsl_pager_value(program: &Program, path: &str, args: &[String]) -> String {
    match program {
        Program::Tool(_) => diff_viewer::executable_pager_command(path, args),
        Program::Custom { .. } => diff_viewer::pager_command(path, args),
    }
}

fn wsl_diff_command(
    ctx: &Context,
    distro: &str,
    workspace: &Path,
    launch: Launch,
) -> (String, Vec<String>) {
    let git = tools::wsl_program(Tool::Git);
    match launch {
        Launch::Pager { pager, pager_args, git_args } => match wsl_program(ctx, distro, &pager) {
            Some(path) => {
                let pager = wsl_pager_value(&pager, &path, &pager_args);
                diff_viewer::wsl_pager_command(distro, workspace, &git, &pager, &git_args)
            },
            None => {
                let pager = diff_viewer::pager_command(program_name(&pager), &pager_args);
                diff_viewer::wsl_pager_command_login(distro, workspace, &git, &pager, &git_args)
            },
        },
        Launch::Direct { program, args } => match wsl_program(ctx, distro, &program) {
            Some(path) => diff_viewer::wsl_direct_command(distro, workspace, &path, &args),
            None => diff_viewer::wsl_direct_command_login(
                distro,
                workspace,
                program_name(&program),
                &args,
            ),
        },
    }
}

fn paint_git_sidebar_status(
    ui: &mut egui::Ui,
    view: &GitSidebarView,
    requests: &mut GitSidebarRequests,
) {
    let theme = &view.theme;
    let status = &view.status;
    ScrollArea::vertical().show(ui, |ui| {
        if let Some(err) = &status.error {
            ui.label(RichText::new(err).color(view.theme.error).small());
            return;
        }

        path_header_label(
            ui,
            &wsl::display_path(&view.path),
            theme.text_muted,
            theme,
            theme.path_style.git_header,
            view.workspace_home.as_deref(),
        );
        paint_git_branch_header(ui, view, requests);
        let mut section_gap = 10.0_f32;
        paint_staged_section(ui, view, requests, &mut section_gap);
        paint_unstaged_section(ui, view, requests, &mut section_gap);
        paint_branch_section(ui, view, requests, &mut section_gap);
    });
}

fn paint_git_branch_header(
    ui: &mut egui::Ui,
    view: &GitSidebarView,
    requests: &mut GitSidebarRequests,
) {
    let theme = &view.theme;
    let Some(branch) = &view.status.branch else { return };
    // Pin the base label so a long branch cannot widen the sidebar.
    let default =
        view.status.default_branch.as_deref().filter(|default| *default != branch.as_str());
    row_with_trailing(
        ui,
        |ui| {
            ui.label(RichText::new("on").color(theme.text_muted).small());
            ui.add(
                egui::Label::new(RichText::new(branch).color(theme.accent).small().strong())
                    .truncate(),
            );
        },
        |ui| {
            if let Some(default) = default {
                // right_to_left: default sits rightmost, `vs` to its left.
                let resp = icon_tooltip(
                    ui.add(
                        egui::Label::new(RichText::new(default).color(theme.text_dim).small())
                            .truncate()
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand),
                    "Set the branch this panel diffs against",
                    theme.icon_tooltips,
                );
                if resp.clicked() {
                    requests.open_picker = Some(view.path.clone());
                }
                ui.label(RichText::new("vs").color(theme.text_muted).small());
            }
        },
    );
}

fn paint_staged_section(
    ui: &mut egui::Ui,
    view: &GitSidebarView,
    requests: &mut GitSidebarRequests,
    section_gap: &mut f32,
) {
    let review = ReviewButton::for_target(view, view.staged_review.as_ref());
    let clicked = section(
        ui,
        &view.theme,
        "Staged",
        &view.staged_count,
        view.filtering,
        section_gap,
        review,
        |ui| {
            for file in &view.status.staged {
                if !view.staged_visible.contains(&file.path) {
                    continue;
                }
                let request = DiffRequest { file: file.path.clone(), source: DiffSource::Staged };
                let is_active = view.active_diff_key.as_deref() == Some(&diff_key(&request));
                let response = file_row(ui, file, &view.theme, is_active);
                if response.clicked() {
                    requests.diff = Some(Target::Row(request));
                }
                paint_git_row_cursor(
                    ui,
                    &response,
                    &view.cursor_row,
                    GitSection::Staged,
                    &file.path,
                    view.cursor_moved,
                    &view.theme,
                );
            }
        },
    );
    if clicked {
        requests.diff = view.staged_review.clone();
    }
}

fn paint_unstaged_section(
    ui: &mut egui::Ui,
    view: &GitSidebarView,
    requests: &mut GitSidebarRequests,
    section_gap: &mut f32,
) {
    let review = ReviewButton::for_target(view, view.unstaged_review.as_ref());
    let clicked = section(
        ui,
        &view.theme,
        "Unstaged",
        &view.unstaged_count,
        view.filtering,
        section_gap,
        review,
        |ui| {
            for file in &view.status.unstaged {
                if !view.unstaged_visible.contains(&file.path) {
                    continue;
                }
                let source = unstaged_diff_source(Some(file.kind));
                let request = DiffRequest { file: file.path.clone(), source };
                let is_active = view.active_diff_key.as_deref() == Some(&diff_key(&request));
                let response = file_row(ui, file, &view.theme, is_active);
                if response.clicked() {
                    requests.diff = Some(Target::Row(request));
                }
                paint_git_row_cursor(
                    ui,
                    &response,
                    &view.cursor_row,
                    GitSection::Unstaged,
                    &file.path,
                    view.cursor_moved,
                    &view.theme,
                );
            }
        },
    );
    if clicked {
        requests.diff = view.unstaged_review.clone();
    }
}

fn paint_branch_section(
    ui: &mut egui::Ui,
    view: &GitSidebarView,
    requests: &mut GitSidebarRequests,
    section_gap: &mut f32,
) {
    if view.status.branch_diff.is_empty() {
        return;
    }
    let base_label = match &view.status.default_branch {
        Some(branch) => format!("Changes vs {branch}"),
        None => "Changes vs default".to_string(),
    };
    let count_label = section_count_label(&view.branch_count, view.filtering);

    ui.add_space(std::mem::take(section_gap));
    let review = ReviewButton::for_target(view, view.branch_review.as_ref());
    let clicked = section_header(ui, &view.theme, review, |ui| {
        ui.label(RichText::new(&base_label).color(view.theme.text).strong().small());
        if let Some(pr) = &view.pr_info {
            ui.label(RichText::new("·").color(view.theme.text_muted).small());
            ui.hyperlink_to(
                RichText::new(format!("PR #{}", pr.number))
                    .color(view.theme.accent)
                    .small()
                    .strong(),
                &pr.url,
            );
        }
        ui.label(RichText::new(count_label).color(view.theme.text_muted).small());
    });
    if clicked {
        requests.diff = view.branch_review.clone();
    }
    ui.add_space(2.0);
    for stat in &view.status.branch_diff {
        if !view.branch_visible.contains(&stat.path) {
            continue;
        }
        let Some(source) = branch_diff_source(view.branch_base.as_deref()) else {
            let response = branch_diff_row(ui, stat, &view.theme, false);
            paint_git_row_cursor(
                ui,
                &response,
                &view.cursor_row,
                GitSection::Branch,
                &stat.path,
                view.cursor_moved,
                &view.theme,
            );
            continue;
        };
        let request = DiffRequest { file: stat.path.clone(), source };
        let is_active = view.active_diff_key.as_deref() == Some(&diff_key(&request));
        let response = branch_diff_row(ui, stat, &view.theme, is_active);
        if response.clicked() {
            requests.diff = Some(Target::Row(request));
        }
        paint_git_row_cursor(
            ui,
            &response,
            &view.cursor_row,
            GitSection::Branch,
            &stat.path,
            view.cursor_moved,
            &view.theme,
        );
    }
}

struct ReviewButton<'a> {
    label: &'a str,
    active: bool,
}

impl<'a> ReviewButton<'a> {
    fn for_target(view: &'a GitSidebarView, target: Option<&Target>) -> Option<Self> {
        let target = target?;
        let active = view.active_diff_key.as_deref() == Some(target.key().as_str());
        Some(Self { label: &view.review_label, active })
    }
}

fn section_header(
    ui: &mut egui::Ui,
    theme: &Theme,
    review: Option<ReviewButton>,
    leading: impl FnOnce(&mut egui::Ui),
) -> bool {
    let Some(button) = review else {
        ui.horizontal(leading);
        return false;
    };
    let mut clicked = false;
    row_with_trailing(ui, leading, |ui| {
        let color = if button.active { theme.text } else { theme.text_muted };
        let response = icon_tooltip(
            ui.add(
                egui::Label::new(RichText::new(button.label).color(color).small())
                    .selectable(false)
                    .sense(egui::Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand),
            "Review this section in the diff viewer",
            theme.icon_tooltips,
        );
        clicked = response.clicked();
    });
    clicked
}

/// Render a git section, skipping empty content and avoiding trailing spacing.
#[allow(clippy::too_many_arguments)]
fn section<R>(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: &str,
    count: &SectionCount,
    filtering: bool,
    gap: &mut f32,
    review: Option<ReviewButton>,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> bool {
    if count.total == 0 {
        return false;
    }
    ui.add_space(std::mem::take(gap));
    let label = section_count_label(count, filtering);
    let clicked = section_header(ui, theme, review, |ui| {
        ui.label(RichText::new(title).color(theme.text).strong().small());
        ui.label(RichText::new(label).color(theme.text_muted).small());
    });
    ui.add_space(2.0);
    add_contents(ui);
    *gap = 10.0;
    clicked
}

pub(super) fn file_row(
    ui: &mut egui::Ui,
    change: &FileChange,
    theme: &Theme,
    is_active: bool,
) -> egui::Response {
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();
    let row_h = ui.spacing().interact_size.y;
    let color = match change.kind {
        ChangeKind::Added | ChangeKind::Untracked => theme.git.added,
        ChangeKind::Modified => theme.git.modified,
        ChangeKind::Deleted => theme.git.deleted,
        ChangeKind::Renamed => theme.git.renamed,
        ChangeKind::Conflicted => theme.git.conflicted,
    };
    let path_color = if is_active { theme.text } else { theme.text_dim };
    let mut path_galley = None;
    let mut hints = IconHints::default();
    // Reserve the full row so short paths do not shrink the click target.
    let resp = ui
        .allocate_ui_with_layout(
            egui::vec2(ui.available_width(), row_h),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_height(row_h);
                // Labels default to `Sense::click_and_drag` for text selection;
                // hit testing picks the smallest covering widget, so a clickable
                // label inside our row would eat clicks before the row sees
                // them. Opt out of selection on every label that lives inside
                // a clickable row so the click falls through.
                let badge = ui.add(
                    egui::Label::new(
                        RichText::new(change.kind.glyph()).color(color).monospace().small(),
                    )
                    .selectable(false),
                );
                hints.add(badge.rect, change.kind.label());
                let (_, galley) = git_path_label(ui, &change.path, path_color, theme);
                path_galley = Some(galley);
                fill_row(ui);
            },
        )
        .response
        .interact(egui::Sense::click());
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| {
        git_path_tooltip(resp, path_galley.as_deref(), theme)
    });
    paint_row_bg(ui, &resp, bg_idx, panel_x, theme, is_active);
    resp
}

pub(super) fn branch_diff_row(
    ui: &mut egui::Ui,
    stat: &crate::git_status::DiffStat,
    theme: &Theme,
    is_active: bool,
) -> egui::Response {
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();
    let row_h = ui.spacing().interact_size.y;
    let added = theme.git.added;
    let removed = theme.git.deleted;
    let path_color = if is_active { theme.text } else { theme.text_dim };
    let mut path_galley = None;

    // Same shape as row_with_trailing (right_to_left wrapping a left_to_right)
    // so +/- counts pin to the right edge while the path truncates cleanly;
    // `set_min_height` + `fill_row` push the hit box to the full row size.
    let resp = ui
        .allocate_ui_with_layout(
            egui::vec2(ui.available_width(), row_h),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.set_min_height(row_h);
                if stat.deletions > 0 {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("-{}", stat.deletions))
                                .color(removed)
                                .small()
                                .monospace(),
                        )
                        .selectable(false),
                    );
                }
                if stat.additions > 0 {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("+{}", stat.additions))
                                .color(added)
                                .small()
                                .monospace(),
                        )
                        .selectable(false),
                    );
                }
                let remaining = ui.available_width();
                if remaining > 0.0 {
                    ui.allocate_ui_with_layout(
                        egui::vec2(remaining, row_h),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_min_height(row_h);
                            let (_, galley) = git_path_label(ui, &stat.path, path_color, theme);
                            path_galley = Some(galley);
                            fill_row(ui);
                        },
                    );
                }
            },
        )
        .response
        .interact(egui::Sense::click());
    let resp = git_path_tooltip(resp, path_galley.as_deref(), theme);
    paint_row_bg(ui, &resp, bg_idx, panel_x, theme, is_active);
    resp
}

/// The git panel's header path. It stays selectable although the panel turns
/// label selection off. Being a header rather than a row, it keeps
/// `egui::Label`'s own elided-text tooltip instead of answering to
/// `[ui] sidebar_tooltips`.
pub(super) fn path_header_label(
    ui: &mut egui::Ui,
    path: &str,
    base: Color32,
    theme: &Theme,
    style: PathStyle,
    home: Option<&str>,
) -> egui::Response {
    let text = path_text(ui, path, base, theme, style, egui::FontFamily::Proportional, home);
    ui.add(egui::Label::new(text).truncate().selectable(true))
}

/// A git panel row's path, laid out rather than added as an `egui::Label` so
/// its tooltip is the row's to give: the label covers only the text, and a
/// pointer sweeping down the panel spends most of its time past the end of
/// short paths, where a label-borne tooltip would go quiet.  The row passes the
/// galley back through `git_path_tooltip` once it has its full-width response.
pub(super) fn git_path_label(
    ui: &mut egui::Ui,
    path: &str,
    base: Color32,
    theme: &Theme,
) -> (egui::Response, Arc<egui::Galley>) {
    let text = path_text(
        ui,
        path,
        base,
        theme,
        theme.path_style.git_rows,
        egui::FontFamily::Proportional,
        None,
    );
    truncating_label(ui, text, base, egui::Sense::hover())
}

/// Offer the row's own response the path its label painted, once the row has
/// one to hang it off.
fn git_path_tooltip(
    resp: egui::Response,
    galley: Option<&egui::Galley>,
    theme: &Theme,
) -> egui::Response {
    match galley {
        Some(galley) => name_tooltip(resp, galley.text(), galley.elided, theme.sidebar_tooltips),
        None => resp,
    }
}

/// Outline the git row the keyboard cursor rests on, matched by section+path so
/// it survives the status refresh.  Full-width rect from the panel plus the
/// row's `y_range`, mirroring the project rows.
fn paint_git_row_cursor(
    ui: &egui::Ui,
    resp: &egui::Response,
    cursor: &Option<git_nav::GitRow>,
    section: GitSection,
    path: &str,
    scroll_into_view: bool,
    theme: &Theme,
) {
    if !matches!(cursor, Some(c) if c.section == section && c.path == path) {
        return;
    }
    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), resp.rect.y_range());
    paint_cursor_outline(ui, rect, theme);
    if scroll_into_view {
        ui.scroll_to_rect(rect, theme.scroll_align);
    }
}

impl AlacritreeApp {
    fn open_review(&mut self, ctx: &Context, action: NamedAction) {
        if let Some(section) = review_section(action, self.cached_branch_base().as_deref()) {
            self.open_diff(ctx, Target::Section(section));
        }
    }

    /// The base resolved by the latest status, so ReviewBranch works while the
    /// git sidebar is hidden.
    fn cached_branch_base(&self) -> Option<String> {
        let path = self.active_session_path()?;
        let status = self.git_panel.status.get(&path)?.last();
        status.default_branch_resolved.clone().or_else(|| status.default_branch.clone())
    }

    fn toggle_git_filter(&mut self, action: NamedAction) {
        if let Some(key) = git_filter_identity(action) {
            self.git_panel.filter.toggle(key);
            self.after_git_filter_changed();
        }
    }
}

impl Action for action::SetBaseBranch {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        let target = base_branch_target(
            app.focus == PaneFocus::ProjectsSidebar,
            app.sidebar.model.cursor(),
            |id| app.sessions.iter().find(|s| s.id == id).map(|s| s.working_directory.clone()),
            &app.current_workspace,
        );
        if let Some(path) = target {
            app.open_base_branch_picker(path);
        }
    }
}

impl Action for action::ClearGitFilters {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.git_panel.filter.clear_toggles();
        app.after_git_filter_changed();
    }
}

impl Action for action::ToggleRightSidebar {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.show_right_sidebar = !app.show_right_sidebar;
        // A deliberate visibility change opts out of the auto-shown
        // round trip, and a hidden sidebar cannot keep keyboard focus.
        app.git_panel.auto_shown = false;
        if !app.show_right_sidebar && app.focus == PaneFocus::GitSidebar {
            app.focus = PaneFocus::Terminal;
        }
        app.persist_sidebars();
    }
}

impl Action for action::FocusGitSidebar {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        if app.focus != PaneFocus::GitSidebar {
            app.focus_git_sidebar();
        } else {
            app.focus_terminal();
        }
    }
}

impl Action for action::RefreshPrStatus {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.pr_cache.invalidate_all();
        // The poll sites run while the sidebars paint, and the palette
        // dispatches after both have; without a wake the re-query would
        // wait for whatever repaint happened to come next.
        ctx.request_repaint();
    }
}

impl Action for action::ReviewStaged {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.open_review(ctx, (*self).into());
    }
}

impl Action for action::ReviewUnstaged {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.open_review(ctx, (*self).into());
    }
}

impl Action for action::ReviewBranch {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.open_review(ctx, (*self).into());
    }
}

impl Action for action::ToggleModifiedFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_git_filter((*self).into());
    }
}

impl Action for action::ToggleDeletedFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_git_filter((*self).into());
    }
}

impl Action for action::ToggleUntrackedFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.toggle_git_filter((*self).into());
    }
}

/// The git-panel toggle a named action flips, or `None` for an action that is
/// not one of its filters.
pub(super) fn git_filter_identity(action: NamedAction) -> Option<char> {
    match action {
        NamedAction::ToggleModifiedFilter(_) => Some('m'),
        NamedAction::ToggleDeletedFilter(_) => Some('d'),
        NamedAction::ToggleUntrackedFilter(_) => Some('u'),
        _ => None,
    }
}

/// The section a Review action opens. The branch section needs a known base.
pub(super) fn review_section(action: NamedAction, base: Option<&str>) -> Option<Section> {
    match action {
        NamedAction::ReviewStaged(_) => Some(Section::Staged),
        NamedAction::ReviewUnstaged(_) => Some(Section::Unstaged),
        NamedAction::ReviewBranch(_) => Some(Section::Branch { base: base?.to_string() }),
        _ => None,
    }
}

/// Whether a git-status row survives the git panel's toggle dimension. Unlike
/// `project_toggles_pass`, standing this down needs no separate `apply` flag:
/// forcing all three toggles to `false` already makes `!any` admit every row.
pub(super) fn git_toggles_pass(m: bool, d: bool, u: bool, kind: ChangeKind) -> bool {
    let any = m || d || u;
    !any || (m && matches!(kind, ChangeKind::Modified | ChangeKind::Renamed))
        || (d && kind == ChangeKind::Deleted)
        || (u && matches!(kind, ChangeKind::Untracked | ChangeKind::Added))
}

/// The diff a git-panel cursor row would open, mirroring the render pass's
/// per-section click mapping.  `None` for a branch-diff row with no resolved
/// base, matching the render pass's unclickable base-less rows.
pub(super) fn git_row_diff_request(
    row: &git_nav::GitRow,
    base: Option<&str>,
) -> Option<DiffRequest> {
    let source = match row.section {
        GitSection::Staged => DiffSource::Staged,
        GitSection::Unstaged => unstaged_diff_source(row.kind),
        GitSection::Branch => branch_diff_source(base)?,
    };
    Some(DiffRequest { file: row.path.clone(), source })
}

/// A branch-diff row with no resolved base has nothing to diff against, so it
/// opens nothing.
fn branch_diff_source(base: Option<&str>) -> Option<DiffSource> {
    Some(DiffSource::Branch { base: base?.to_string() })
}

/// An untracked file has no index entry to diff against, so it opens as a
/// pure addition.
fn unstaged_diff_source(kind: Option<ChangeKind>) -> DiffSource {
    if kind == Some(ChangeKind::Untracked) { DiffSource::Untracked } else { DiffSource::Worktree }
}

/// Section header count: `visible of total` while a filter narrows the panel,
/// the plain total otherwise.
pub(super) fn section_count_label(count: &SectionCount, filtering: bool) -> String {
    if filtering {
        format!("{} of {}", count.visible, count.total)
    } else {
        format!("{}", count.total)
    }
}

/// The branch the git panel diffs against: the user's explicit override,
/// else the open PR's base (what GitHub will review), else the project's
/// detected default branch.
pub(super) fn effective_base_branch(
    override_branch: Option<&str>,
    pr_base: Option<&str>,
    project_default: Option<&str>,
) -> Option<String> {
    override_branch.or(pr_base).or(project_default).map(str::to_string)
}

/// The worktree a SetBaseBranch press targets: the sidebar cursor's worktree
/// while the projects sidebar owns focus (a session row resolves to its
/// workspace), otherwise the current workspace.  Home and project-header
/// cursors, and the home workspace, have no base branch to override.
pub(super) fn base_branch_target(
    sidebar_focused: bool,
    cursor: Option<&SidebarRow>,
    session_workspace: impl Fn(SessionId) -> Option<WorkspaceKey>,
    current: &WorkspaceKey,
) -> Option<PathBuf> {
    if sidebar_focused {
        return match cursor {
            Some(SidebarRow::Worktree(p)) => Some(p.clone()),
            Some(SidebarRow::Session(id)) => session_workspace(*id).flatten(),
            _ => None,
        };
    }
    current.clone()
}

/// Extend a row's bounding rect to its parent's full width so the response
/// covers the empty space past short labels, instead of just the content.
fn fill_row(ui: &mut egui::Ui) {
    let remaining = ui.available_width();
    if remaining > 0.0 {
        ui.allocate_space(egui::vec2(remaining, 0.0));
    }
}

fn paint_row_bg(
    ui: &mut egui::Ui,
    resp: &egui::Response,
    bg_idx: egui::layers::ShapeIdx,
    panel_x: egui::Rangef,
    theme: &Theme,
    is_active: bool,
) {
    let bg = if is_active {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        return;
    };
    let rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    ui.painter().set(bg_idx, egui::Shape::rect_filled(rect, 0.0, bg));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_viewer::{Section, Templates, Viewer};

    #[test]
    fn a_review_action_names_its_section_and_the_branch_needs_a_base() {
        assert_eq!(
            review_section(NamedAction::ReviewStaged(action::ReviewStaged), None),
            Some(Section::Staged)
        );
        assert_eq!(
            review_section(NamedAction::ReviewUnstaged(action::ReviewUnstaged), None),
            Some(Section::Unstaged)
        );
        assert_eq!(review_section(NamedAction::ReviewBranch(action::ReviewBranch), None), None);
        assert_eq!(
            review_section(NamedAction::ReviewBranch(action::ReviewBranch), Some("main")),
            Some(Section::Branch { base: "main".to_string() })
        );
        assert_eq!(review_section(NamedAction::Paste(action::Paste), Some("main")), None);
    }

    /// An app whose current workspace shows a diff pane under `key`. The
    /// session is never spawned, so no viewer runs.
    fn app_with_diff_pane(key: &str) -> (AlacritreeApp, SessionId) {
        let workspace = PathBuf::from("C:/repo/wt");
        let (_, notify_rx) = std::sync::mpsc::channel();
        let mut app = AlacritreeApp::from_parts(
            Config::default(),
            Theme::from_config(&Config::default()),
            crate::state::PersistedState::default(),
            Vec::new(),
            (Vec::new(), crate::fonts::FaceMetrics::default()),
            notify_rx,
            (None, None),
        );
        app.config.ui.last_session_close = crate::config::LastSessionClose::Navigate;
        let (survivor, _) = Session::pending_command(
            Context::default(),
            &app.config,
            Some(workspace.clone()),
            TermSize::new(80, 24),
            (8.0, 16.0),
            "inert".to_string(),
            Vec::new(),
            "survivor".to_string(),
            SessionKind::Shell,
        );
        let survivor_id = survivor.id;
        app.sessions.push(survivor);
        let (session, _) = Session::pending_command(
            Context::default(),
            &app.config,
            Some(workspace.clone()),
            TermSize::new(80, 24),
            (8.0, 16.0),
            "git".to_string(),
            Vec::new(),
            "diff".to_string(),
            SessionKind::Diff { key: key.to_string() },
        );
        app.sessions.push(session);
        app.current_workspace = Some(workspace);
        (app, survivor_id)
    }

    fn has_diff_pane(app: &AlacritreeApp) -> bool {
        app.sessions.iter().any(|s| matches!(s.kind, SessionKind::Diff { .. }))
    }

    #[test]
    fn choosing_the_open_section_again_closes_its_pane() {
        let (mut app, survivor_id) = app_with_diff_pane("section:staged");
        let before = app.sessions.len();
        app.open_diff(&Context::default(), Target::Section(Section::Staged));
        assert_eq!(app.sessions.len(), before - 1);
        assert!(!has_diff_pane(&app));
        assert!(app.sessions.iter().any(|session| session.id == survivor_id));
        assert!(app.modals.error_dialog.is_none());
    }

    #[test]
    fn a_section_the_viewer_cannot_open_leaves_the_open_pane_alone() {
        let (mut app, _) = app_with_diff_pane("staged:a.rs");
        app.config.integrations.diff_viewer.viewer = Viewer::Direct {
            program: Program::Custom { path: "difft".to_string(), wsl_path: None },
            templates: Templates::default(),
        };
        app.open_diff(&Context::default(), Target::Section(Section::Staged));
        assert!(has_diff_pane(&app));
    }

    #[test]
    fn a_wsl_diff_uses_the_configured_wsl_git_not_the_native_git() {
        let _lock = crate::tools::test_configuration_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        use strum::EnumCount;
        struct RestoreToolConfiguration([crate::tools::ToolPaths; crate::tools::Tool::COUNT]);
        impl Drop for RestoreToolConfiguration {
            fn drop(&mut self) {
                crate::tools::configure(self.0.clone());
            }
        }
        let _restore = RestoreToolConfiguration(crate::tools::test_configuration());
        let mut configured = crate::tools::Tool::table(crate::tools::ToolPaths::named);
        configured[crate::tools::Tool::Git as usize] = crate::tools::ToolPaths {
            native: "C:/native/git.exe".to_string(),
            wsl: Some("/opt/wsl/bin/git".to_string()),
        };
        crate::tools::configure(configured);
        let launch = Launch::Pager {
            pager: Program::Custom {
                path: "delta --side-by-side".to_string(),
                wsl_path: Some("/opt/delta".to_string()),
            },
            pager_args: Vec::new(),
            git_args: vec!["diff".to_string()],
        };
        let (_, args) = wsl_diff_command(
            &Context::default(),
            "kali-linux",
            Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj"),
            launch,
        );
        assert_eq!(args[9], "/opt/wsl/bin/git");
        assert!(!args[9..].iter().any(|arg| arg == "C:/native/git.exe"));
    }

    #[test]
    fn base_branch_precedence_is_override_then_pr_then_default() {
        let f = effective_base_branch;
        assert_eq!(f(Some("develop"), Some("main"), Some("master")), Some("develop".into()));
        assert_eq!(f(None, Some("main"), Some("master")), Some("main".into()));
        assert_eq!(f(None, None, Some("master")), Some("master".into()));
        assert_eq!(f(None, None, None), None);
    }

    #[test]
    fn a_wide_search_stands_down_the_git_toggles() {
        // Toggled on for "modified" only, an untracked row fails while the
        // toggle applies and passes once a wide search stands it down.
        assert!(!git_toggles_pass(true, false, false, ChangeKind::Untracked));
        assert!(git_toggles_pass(false, false, false, ChangeKind::Untracked));
    }

    #[test]
    fn an_unstaged_untracked_file_opens_a_no_index_diff() {
        assert!(matches!(unstaged_diff_source(Some(ChangeKind::Untracked)), DiffSource::Untracked));
        assert!(matches!(unstaged_diff_source(Some(ChangeKind::Modified)), DiffSource::Worktree));
    }

    #[test]
    fn a_branch_row_without_a_base_opens_no_diff() {
        let row = git_nav::GitRow { section: GitSection::Branch, path: "a.rs".into(), kind: None };
        assert!(git_row_diff_request(&row, None).is_none());
        let request = git_row_diff_request(&row, Some("main")).expect("a base makes it clickable");
        assert!(matches!(request.source, DiffSource::Branch { base } if base == "main"));
    }

    #[test]
    fn set_base_branch_targets_the_cursored_worktree_when_sidebar_focused() {
        let wt = PathBuf::from("C:/repo/wt");
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };
        let cursor = SidebarRow::Worktree(wt.clone());
        assert_eq!(
            base_branch_target(true, Some(&cursor), none, &Some(PathBuf::from("C:/other"))),
            Some(wt)
        );
    }

    #[test]
    fn set_base_branch_ignores_home_and_project_rows() {
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(true, Some(&SidebarRow::Home), none, &None), None);
        let cursor = SidebarRow::Project(PathBuf::from("C:/repo"));
        let none2 = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(true, Some(&cursor), none2, &None), None);
    }

    #[test]
    fn set_base_branch_falls_back_to_the_current_worktree() {
        let wt = PathBuf::from("C:/repo/wt");
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(false, None, none, &Some(wt.clone())), Some(wt));
        let none2 = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(false, None, none2, &None), None, "home has no base branch");
    }
}
