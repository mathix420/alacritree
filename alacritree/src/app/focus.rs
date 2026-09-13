//! Sidebar cursor repair, keyboard navigation, and fuzzy-search completion.

use super::*;

pub(super) struct SidebarFocusState {
    /// `[ui] search_scope`: whether a live query stands down both panels'
    /// toggle filters.  Toggled at runtime, never persisted.
    pub(super) search_scope: SearchScope,
    /// Last reconciled snapshot, the baseline for the next cursor repair.
    pub(super) previous: Option<sidebar_focus::TreeSnapshot>,
    /// What the reconciler itself last wrote. A different value on the next
    /// pass means the user navigated through a click, session cycling, the
    /// palette, a notification, or IPC, so the anchor has been overtaken.
    pub(super) written: Option<SidebarFocusWrite>,
    /// A close verdict the reconciler still owes the terminal.
    pub(super) deferred_close: Option<DeferredClose>,
}

impl SidebarFocusState {
    pub(super) fn new(search_scope: SearchScope) -> Self {
        Self { search_scope, previous: None, written: None, deferred_close: None }
    }
}

impl AlacritreeApp {
    pub(super) fn sidebar_snapshot(
        &mut self,
        skip_worktree: Option<&Path>,
    ) -> sidebar_focus::TreeSnapshot {
        let active_workspace = self.current_workspace.as_deref();
        let active_branch = active_workspace
            .and_then(|p| self.git_panel.status.get(p))
            .and_then(|c| c.current_branch());
        let inputs = sidebar_focus::ObservedInputs::capture(
            &self.projects,
            self.session_inputs(self.observes_session_titles()),
            sidebar_focus::UiInputs {
                session_rows_always: self.session_rows_always,
                sessions_filter_counts_detached: self.sessions_filter_counts_detached,
                query: self.sidebar.filter.query(),
                toggles: self.sidebar.filter.toggle_bits(),
                toggles_apply: self
                    .sidebar
                    .filter
                    .toggles_apply(self.sidebar_focus_state.search_scope),
                pr_generation: pr_generation_for(
                    self.pr_cache.generation(),
                    any_pr_toggle_active(
                        &self.sidebar.filter,
                        self.sidebar_focus_state.search_scope,
                    ),
                ),
                active_workspace,
                active_branch,
                herdr_generation: self.herdr_generation(),
            },
        );
        let rows = self.current_project_rows();
        let live = self.session_pairs();
        let listed = self.listed_workspace_rows();
        let snapshot =
            build_sidebar_snapshot(&self.projects, &live, &listed, &rows, skip_worktree, inputs);
        // Paint reuses these until the next rebuild, so an unchanged filtering
        // frame runs no fuzzy matching at all.
        self.sidebar.rows_cache = Some(rows);
        snapshot
    }

    /// Repair the sidebar cursor against what changed since the last pass.
    /// Called twice per `update`. The first call handles input and background
    /// drains before paint. The second handles changes from
    /// `reap_exited_sessions` and paint-time clicks. A pass with nothing to
    /// do costs one `ObservedInputs` compare, which is the whole steady-state
    /// budget: there is no setting that skips this.
    pub(super) fn reconcile_sidebar_focus(&mut self, ctx: &Context) {
        if sidebar_focus_overtaken(
            &self.sidebar_focus_state.written,
            self.sidebar.cursor.as_ref(),
            &self.current_workspace,
            self.active_session.get(&self.current_workspace).copied(),
        ) {
            self.sidebar.anchor = None;
        }

        let deferred = self.sidebar_focus_state.deferred_close.take();
        let skip = deferred.as_ref().and_then(|d| d.removed_worktree.clone());

        if deferred.is_none() {
            let active_workspace = self.current_workspace.as_deref();
            let active_branch = active_workspace
                .and_then(|p| self.git_panel.status.get(p))
                .and_then(|c| c.current_branch());
            if let Some(prev) = &self.sidebar_focus_state.previous {
                let unchanged = prev.inputs.matches(
                    &self.projects,
                    self.session_inputs(self.observes_session_titles()),
                    sidebar_focus::UiInputs {
                        session_rows_always: self.session_rows_always,
                        sessions_filter_counts_detached: self.sessions_filter_counts_detached,
                        query: self.sidebar.filter.query(),
                        toggles: self.sidebar.filter.toggle_bits(),
                        toggles_apply: self
                            .sidebar
                            .filter
                            .toggles_apply(self.sidebar_focus_state.search_scope),
                        pr_generation: pr_generation_for(
                            self.pr_cache.generation(),
                            any_pr_toggle_active(
                                &self.sidebar.filter,
                                self.sidebar_focus_state.search_scope,
                            ),
                        ),
                        active_workspace,
                        active_branch,
                        herdr_generation: self.herdr_generation(),
                    },
                );
                if unchanged {
                    return;
                }
            }
        }

        let next = self.sidebar_snapshot(skip.as_deref());
        let prev = self.sidebar_focus_state.previous.take().unwrap_or_else(|| next.clone());
        let outcome = sidebar_focus::repair(
            &prev,
            &next,
            self.sidebar.cursor.as_ref(),
            self.sidebar.anchor.as_ref(),
        );

        if outcome.cursor != self.sidebar.cursor {
            self.sidebar.cursor = outcome.cursor;
            self.sidebar.cursor_moved = true;
        }
        self.sidebar.anchor = outcome.anchor;
        self.sidebar_focus_state.previous = Some(next);

        if self.config.ui.sidebar_focus.follows() {
            match (outcome.follow, deferred) {
                (Some(target), _) => self.apply_follow_target(ctx, target),
                // Nothing live to land on, so the verdict this pass took over
                // from still decides where the terminal goes.
                (None, Some(deferred)) => self.apply_close_fallback(ctx, deferred.verdict),
                (None, None) => {},
            }
        }

        self.mark_sidebar_focus_write();
    }

    /// Record the current focus triple as the reconciler's own, so the next
    /// pass does not mistake it for the user navigating.
    pub(super) fn mark_sidebar_focus_write(&mut self) {
        self.sidebar_focus_state.written = Some(SidebarFocusWrite {
            cursor: self.sidebar.cursor.clone(),
            workspace: self.current_workspace.clone(),
            active: self.active_session.get(&self.current_workspace).copied(),
        });
    }

    /// Move the terminal to a removal landing.  A workspace target adopts its
    /// active session, or its first live one when that entry went stale.
    fn apply_follow_target(&mut self, ctx: &Context, target: sidebar_focus::FollowTarget) {
        match target {
            sidebar_focus::FollowTarget::Session(id) => self.activate_session_by_id(id),
            sidebar_focus::FollowTarget::Workspace(ws) => {
                let id = self
                    .active_session
                    .get(&ws)
                    .copied()
                    .filter(|id| self.sessions.iter().any(|s| s.id == *id))
                    .or_else(|| {
                        self.sessions.iter().find(|s| s.working_directory == ws).map(|s| s.id)
                    });
                if let Some(id) = id {
                    self.activate_session_by_id(id);
                }
            },
        }
        ctx.request_repaint();
    }

    pub(super) fn apply_sidebar_nav(&mut self, ctx: &Context, key: egui::Key) {
        use egui::Key;
        let rows = self.current_project_rows();
        let cursor = match self.sidebar.cursor.clone() {
            Some(c) if rows.contains(&c) => c,
            // Stale or unseeded cursor (worktree removed, project collapsed
            // by mouse, or a filter toggle narrowing the rows out from under
            // it): land on the first row and let the next press act from
            // there. Unfiltered `rows` always leads with Home.
            _ => {
                if let Some(first) = rows.first() {
                    self.set_sidebar_cursor(first.clone());
                }
                return;
            },
        };
        match key {
            Key::ArrowUp => self.set_sidebar_cursor(sidebar_nav::step(&rows, &cursor, -1)),
            Key::ArrowDown => self.set_sidebar_cursor(sidebar_nav::step(&rows, &cursor, 1)),
            Key::ArrowRight => match &cursor {
                SidebarRow::Project(root) => {
                    let root = root.clone();
                    self.set_project_expanded(&root, true);
                },
                SidebarRow::Session(id) => {
                    let id = *id;
                    self.activate_session_by_id(id);
                    self.focus_terminal();
                },
                _ => {},
            },
            Key::ArrowLeft => match &cursor {
                SidebarRow::Project(root) => self.set_project_expanded(root, false),
                SidebarRow::Worktree(_) | SidebarRow::Session(_) | SidebarRow::HerdrAgent(..) => {
                    if let Some(target) = sidebar_nav::left_target(&rows, &cursor) {
                        self.set_sidebar_cursor(target);
                    }
                },
                SidebarRow::Home => {},
            },
            Key::Enter => self.activate_sidebar_row(ctx, &cursor),
            Key::Escape => self.focus_terminal(),
            _ => {},
        }
    }

    /// Confirm the focused sidebar's fuzzy search: leave search and land the
    /// cursor on the highlighted row, scrolled into view, keeping focus in the
    /// sidebar. Selecting a row never activates it. A subsequent browsing
    /// `Enter` activates it. No-op unless the focused panel is in search mode.
    pub(super) fn sidebar_search_confirm(&mut self) {
        match self.focus {
            PaneFocus::ProjectsSidebar
                if self.sidebar.filter.mode() == panel_filter::Mode::Search =>
            {
                let acted = self.sidebar.cursor.clone();
                if let Some(row) = acted.as_ref() {
                    self.reveal_search_row(row);
                }
                self.finish_project_search_at(acted);
            },
            PaneFocus::GitSidebar if self.git_panel.filter.mode() == panel_filter::Mode::Search => {
                let cursor = self.git_panel.cursor.clone();
                self.finish_git_search_at(cursor);
            },
            _ => {},
        }
    }

    /// Expand the project owning a worktree/session row so the row outlives the
    /// search exit: search lists matched children whatever their project's
    /// `expanded` flag says, so a child under a collapsed project would vanish
    /// the moment the query clears. Expand-only, and headers are left alone, so
    /// confirming a project row never toggles it.
    pub(super) fn reveal_search_row(&mut self, row: &SidebarRow) {
        let root = {
            let session_workspace = |id: SessionId| {
                self.sessions.iter().find(|s| s.id == id).map(|s| s.working_directory.clone())
            };
            search_reveal_root(&self.projects, session_workspace, row)
        };
        if let Some(root) = root {
            self.set_project_expanded(&root, true);
        }
    }

    /// Leave search and land the projects cursor on `requested`, falling back
    /// through `ensure_cursor` when it no longer renders. The scroll is forced
    /// rather than keyed on the cursor changing: restoring the unfiltered list
    /// can move the very same row far off-screen.
    fn finish_project_search_at(&mut self, requested: Option<SidebarRow>) {
        self.sidebar.filter.exit_search();
        let rows = self.current_project_rows();
        self.sidebar.cursor = sidebar_nav::ensure_cursor(&rows, requested.as_ref());
        self.sidebar.cursor_moved = true;
    }

    /// Git-panel counterpart of `finish_project_search_at`.
    fn finish_git_search_at(&mut self, requested: Option<git_nav::GitRow>) {
        self.git_panel.filter.exit_search();
        self.recompute_git_rows();
        self.git_panel.cursor = git_nav::ensure_cursor(&self.git_panel.rows, requested.as_ref());
        self.git_panel.cursor_moved = true;
    }

    /// Cancel the focused sidebar's fuzzy search, staying in the sidebar with the
    /// cursor on the seed row (active session / workspace, else Home). No-op
    /// unless the focused panel is in search mode.
    pub(super) fn sidebar_search_cancel(&mut self) {
        match self.focus {
            PaneFocus::ProjectsSidebar
                if self.sidebar.filter.mode() == panel_filter::Mode::Search =>
            {
                let seed = sidebar_nav::seed(
                    &self.projects,
                    self.current_workspace.as_deref(),
                    &self.listed_workspace_rows(),
                    self.active_session.get(&self.current_workspace).copied(),
                );
                self.finish_project_search_at(Some(seed));
            },
            PaneFocus::GitSidebar if self.git_panel.filter.mode() == panel_filter::Mode::Search => {
                let cursor = self.git_panel.cursor.clone();
                self.finish_git_search_at(cursor);
            },
            _ => {},
        }
    }

    /// Cancel the focused sidebar's fuzzy search and return focus to the
    /// terminal. No-op unless the focused panel is in search mode.
    pub(super) fn sidebar_search_cancel_to_terminal(&mut self) {
        match self.focus {
            PaneFocus::ProjectsSidebar
                if self.sidebar.filter.mode() == panel_filter::Mode::Search =>
            {
                self.sidebar.filter.exit_search();
                self.focus_terminal();
            },
            PaneFocus::GitSidebar if self.git_panel.filter.mode() == panel_filter::Mode::Search => {
                self.git_panel.filter.exit_search();
                self.recompute_git_rows();
                self.focus_terminal();
            },
            _ => {},
        }
    }
}

impl Action for action::SidebarSearchConfirm {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.sidebar_search_confirm();
    }
}

impl Action for action::SidebarSearchCancel {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.sidebar_search_cancel();
    }
}

impl Action for action::SidebarSearchCancelToTerminal {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.sidebar_search_cancel_to_terminal();
    }
}

impl Action for action::ToggleSearchScope {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.sidebar_focus_state.search_scope = match app.sidebar_focus_state.search_scope {
            SearchScope::Filtered => SearchScope::All,
            SearchScope::All => SearchScope::Filtered,
        };
    }
}

/// The cache generation the reconciler observes.  Held at `0` unless a PR
/// filter is active, so a banked result only invalidates a row set that
/// actually depends on PR state.
pub(super) fn pr_generation_for(generation: u64, any_pr_toggle_active: bool) -> u64 {
    if any_pr_toggle_active { generation } else { 0 }
}

/// Step the lockstep index over the rows a skipped worktree owns.
///
/// The projection is built before the deletion is known, so it still lists
/// the worktree with everything under it.  Leaving the index parked on a row
/// no node will match again would mark every later node unprojected, and the
/// cursor repair reads an unprojected row as one that has gone away.
pub(super) fn skip_projected_rows(
    rows: &[SidebarRow],
    next_row: &mut usize,
    listed: &sidebar_nav::ListedRows,
    path: &Path,
) {
    if rows.get(*next_row) != Some(&SidebarRow::Worktree(path.to_path_buf())) {
        return;
    }
    *next_row += 1;
    for entry in listed.get(&Some(path.to_path_buf())).map_or(&[][..], Vec::as_slice) {
        if rows.get(*next_row) != Some(&entry.row()) {
            break;
        }
        *next_row += 1;
    }
}

/// Assemble the model arena and the projection.  `rows` is the projection —
/// exactly what the cursor steps over — and `live` is the model: every running
/// session, whatever the listing threshold or the filter says.  Building
/// membership from `listed` instead would make the last session in a workspace
/// read as deleted the moment its sibling closed.
///
/// `listed` is the listing the projection was built from.  A herdr row exists
/// only while its agent is listed, so there is no wider model to take it from,
/// and reading a second listing here could disagree with `rows`.
///
/// `skip_worktree` drops a worktree whose deletion is already committed but
/// whose git operation has not finished, so nothing lands the cursor — or a
/// new shell — inside a directory on its way out.
///
/// Nodes are pushed in exactly the order `sidebar_nav::visible_rows` emits,
/// with unprojected nodes interleaved, so one forward index into `rows`
/// classifies every node.  Asking `rows.contains` per node instead would be
/// quadratic in path comparisons on a path that runs whenever the user types.
pub(super) fn build_sidebar_snapshot(
    projects: &[Project],
    live: &[(WorkspaceKey, SessionId)],
    listed: &sidebar_nav::ListedRows,
    rows: &[SidebarRow],
    skip_worktree: Option<&Path>,
    inputs: sidebar_focus::ObservedInputs,
) -> sidebar_focus::TreeSnapshot {
    use sidebar_focus::Parent;
    use sidebar_nav::WorkspaceEntry;

    let mut b = sidebar_focus::SnapshotBuilder::default();
    let mut next_row = 0usize;
    let mut placed = vec![false; live.len()];

    // Consume `rows` in lockstep: a node is projected exactly when it is the
    // row the projection expects next.
    let push = |b: &mut sidebar_focus::SnapshotBuilder,
                next_row: &mut usize,
                row: SidebarRow,
                parent: Parent| {
        let projected = rows.get(*next_row) == Some(&row);
        if projected {
            *next_row += 1;
        }
        b.push(row, parent, projected)
    };
    let push_workspace = |b: &mut sidebar_focus::SnapshotBuilder,
                          next_row: &mut usize,
                          placed: &mut [bool],
                          ws: &WorkspaceKey,
                          parent: Parent| {
        let entries = listed.get(ws).map_or(&[][..], Vec::as_slice);
        for entry in entries {
            push(b, next_row, entry.row(), parent);
        }
        // A workspace lists every shell session it has or none of them, and a
        // session attached to a herdr pane is always listed, so a session
        // reaching the second arm here belongs to a workspace that listed
        // nothing at all.  It is running, so the model keeps it; it is drawn
        // nowhere, so the projection does not.
        for (i, (w, id)) in live.iter().enumerate() {
            if w != ws {
                continue;
            }
            placed[i] = true;
            if !entries.contains(&WorkspaceEntry::Session(*id)) {
                b.push(SidebarRow::Session(*id), parent, false);
            }
        }
    };

    let home_id = push(&mut b, &mut next_row, SidebarRow::Home, Parent::Root);
    push_workspace(&mut b, &mut next_row, &mut placed, &None, Parent::Node(home_id));

    for p in projects {
        let project_id =
            push(&mut b, &mut next_row, SidebarRow::Project(p.root.clone()), Parent::Root);
        for wt in &p.worktrees {
            if skip_worktree == Some(wt.path.as_path()) {
                skip_projected_rows(rows, &mut next_row, listed, &wt.path);
                continue;
            }
            let wt_id = push(
                &mut b,
                &mut next_row,
                SidebarRow::Worktree(wt.path.clone()),
                Parent::Node(project_id),
            );
            let ws = Some(wt.path.clone());
            push_workspace(&mut b, &mut next_row, &mut placed, &ws, Parent::Node(wt_id));
        }
    }

    // Sessions whose workspace has no row left — a removed project, or a
    // worktree already treated as gone.  They are running, so they belong in
    // the model; they have no place in the tree, so they are nobody's sibling.
    for (i, (_, id)) in live.iter().enumerate() {
        if !placed[i] {
            b.push(SidebarRow::Session(*id), Parent::Detached, false);
        }
    }

    debug_assert_eq!(next_row, rows.len(), "every projected row must be in the arena");
    b.finish(inputs)
}

/// The cursor, workspace, and active session the reconciler last wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SidebarFocusWrite {
    pub(super) cursor: Option<SidebarRow>,
    pub(super) workspace: WorkspaceKey,
    pub(super) active: Option<SessionId>,
}

/// Whether focus moved behind the reconciler's back.  The active session is
/// part of the comparison because the tab and session cycling actions can
/// switch sessions without leaving the workspace, changing nothing else.
/// Comparing the resulting state rather than matching on action names covers
/// every route to them — rebound keys, the command palette, MCP — at the price
/// of `ensure_active_session` and `adopt_active_session` marking their own
/// writes so their self-healing does not read as navigation.
pub(super) fn sidebar_focus_overtaken(
    written: &Option<SidebarFocusWrite>,
    cursor: Option<&SidebarRow>,
    workspace: &WorkspaceKey,
    active: Option<SessionId>,
) -> bool {
    match written {
        None => false,
        Some(w) => w.cursor.as_ref() != cursor || w.workspace != *workspace || w.active != active,
    }
}

/// A close-fallback verdict the reconciler owes the terminal, and the worktree
/// whose rows must already read as gone.  The verdict is carried rather than
/// recomputed because only `close_fallback` knows the difference between
/// staying put, hopping to the project's main checkout, and going home.
#[derive(Debug)]
pub(super) struct DeferredClose {
    pub(super) verdict: CloseFallback,
    /// Set when an asynchronous worktree deletion is in flight: `projects`
    /// still lists it, so without this the reconciler would see an intact row
    /// and could spawn a shell inside the directory being removed.  It pairs
    /// with any verdict, including a ring landing in another project.
    pub(super) removed_worktree: Option<PathBuf>,
}

/// The project to expand so `row` still renders once search exits, if any.
/// Only child rows qualify: search lists matched worktrees and sessions whatever
/// their project's `expanded` flag says, so they vanish when the query clears.
/// A header is already its own row, and expanding it would turn selecting a
/// project into a toggle.
pub(super) fn search_reveal_root(
    projects: &[Project],
    session_workspace: impl Fn(SessionId) -> Option<WorkspaceKey>,
    row: &SidebarRow,
) -> Option<PathBuf> {
    if !matches!(row, SidebarRow::Worktree(_) | SidebarRow::Session(_)) {
        return None;
    }
    row_project_root(projects, session_workspace, row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_sees_a_same_workspace_session_switch() {
        let written =
            SidebarFocusWrite { cursor: Some(SidebarRow::Home), workspace: None, active: Some(1) };
        let written = Some(written);

        // The reconciler's own values still stand.
        assert!(!sidebar_focus_overtaken(&written, Some(&SidebarRow::Home), &None, Some(1)));

        // Any action that switches sessions without leaving the workspace —
        // SelectNextTab, SelectNextSession, SelectTab(n) — changes neither the
        // cursor nor the workspace, only the active session.
        assert!(sidebar_focus_overtaken(&written, Some(&SidebarRow::Home), &None, Some(2)));

        // A different workspace, and a different cursor, each count too.
        assert!(sidebar_focus_overtaken(
            &written,
            Some(&SidebarRow::Home),
            &Some(PathBuf::from("/a/wt1")),
            Some(1),
        ));
        assert!(sidebar_focus_overtaken(
            &written,
            Some(&SidebarRow::Project(PathBuf::from("/a"))),
            &None,
            Some(1),
        ));

        // Nothing written yet cannot have been overtaken.
        assert!(!sidebar_focus_overtaken(&None, Some(&SidebarRow::Home), &None, Some(1)));
    }

    #[test]
    fn snapshot_parents_agree_with_the_row_model() {
        use crate::sidebar_focus::Parent;
        use crate::sidebar_nav::{self, SidebarRow};

        // Two projects, one collapsed, with sessions under the expanded one.
        let projects = vec![
            sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"]),
            sidebar_nav::tests::project("/b", false, &["/b/wt1"]),
        ];
        let live =
            vec![(None, 1), (Some(PathBuf::from("/a/wt1")), 2), (Some(PathBuf::from("/a/wt1")), 3)];
        let listed = sidebar_nav::tests::sessions_only(HashMap::from([
            (None, vec![1]),
            (Some(PathBuf::from("/a/wt1")), vec![2, 3]),
        ]));
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        for row in &rows {
            let id = snapshot.find(row).expect("every projected row is in the model");
            let arena_parent = match snapshot.parent(id) {
                Parent::Root => None,
                Parent::Node(p) => Some(snapshot.row(p).clone()),
                Parent::Detached => panic!("a projected row is never detached: {row:?}"),
            };
            assert_eq!(
                arena_parent,
                sidebar_nav::left_target(&rows, row),
                "arena parent must agree with the row model for {row:?}"
            );
        }

        // The collapsed project's worktree is in the model but not projected.
        let hidden = snapshot
            .find(&SidebarRow::Worktree(PathBuf::from("/b/wt1")))
            .expect("collapsed worktrees stay in the model");
        assert!(!snapshot.is_projected(hidden));
    }

    #[test]
    fn a_session_below_the_listing_threshold_is_still_in_the_model() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1"])];
        // One live session in the worktree.  The real rule needs two before it
        // lists any, so this one is live but unprojected.
        let live = vec![(Some(PathBuf::from("/a/wt1")), 7)];
        let listed = {
            let mut l = sidebar_nav::ListedRows::new();
            let entries = workspace_entries(&[7], Vec::new(), false);
            assert!(entries.is_empty(), "the threshold rule must actually drop this session");
            if !entries.is_empty() {
                l.insert(Some(PathBuf::from("/a/wt1")), entries);
            }
            l
        };
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        let id = snapshot
            .find(&SidebarRow::Session(7))
            .expect("a live session is in the model whatever the listing threshold says");
        assert!(!snapshot.is_projected(id), "but it is not a navigable row");
    }

    #[test]
    fn a_session_whose_project_is_gone_is_detached_not_deleted() {
        use crate::sidebar_focus::Parent;
        use crate::sidebar_nav::{self, SidebarRow};

        // `remove_project` drops the project but keeps its sessions running.
        let projects: Vec<crate::projects::Project> = vec![];
        let live = vec![(Some(PathBuf::from("/orphan/wt1")), 5)];
        let listed = sidebar_nav::ListedRows::new();
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        let id = snapshot.find(&SidebarRow::Session(5)).expect("the session is still running");
        assert_eq!(
            snapshot.parent(id),
            Parent::Detached,
            "an orphan must not become a sibling of Home"
        );
    }

    #[test]
    fn a_worktree_being_deleted_reads_as_gone_immediately() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"])];
        let listed = sidebar_nav::ListedRows::new();
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let doomed = PathBuf::from("/a/wt2");
        let snapshot = build_sidebar_snapshot(
            &projects,
            &[],
            &listed,
            &rows,
            Some(doomed.as_path()),
            Default::default(),
        );

        assert_eq!(
            snapshot.find(&SidebarRow::Worktree(doomed)),
            None,
            "the async git delete has not finished, but the row must not read as present"
        );
        assert!(snapshot.find(&SidebarRow::Worktree(PathBuf::from("/a/wt1"))).is_some());
    }

    /// The rows below a worktree being deleted must stay navigable.
    ///
    /// The projection is built before the deletion is known, so it still
    /// lists the doomed worktree.  The builder consumes that projection in
    /// lockstep, so skipping the worktree without stepping the index leaves
    /// it parked on a row nothing will ever match again — every later node
    /// reads as unprojected, and the cursor repair treats an unprojected row
    /// as one that has gone away.
    #[test]
    fn rows_below_a_deleted_worktree_stay_navigable() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects =
            vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2", "/a/wt3"])];
        let listed = sidebar_nav::ListedRows::new();
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let doomed = PathBuf::from("/a/wt2");
        let snapshot = build_sidebar_snapshot(
            &projects,
            &[],
            &listed,
            &rows,
            Some(doomed.as_path()),
            Default::default(),
        );

        let below = snapshot
            .find(&SidebarRow::Worktree(PathBuf::from("/a/wt3")))
            .expect("the worktree below the deleted one is still in the tree");
        assert!(
            snapshot.is_projected(below),
            "a row below the one being deleted must still be navigable"
        );
    }

    /// The lockstep walk follows the listing, not the session vector.
    ///
    /// Attaching to the second pane first leaves the two sessions in the
    /// opposite order to herdr's, and a walk that trusted the vector would
    /// push them the wrong way round, match neither against the projection
    /// and trip its own assert.
    #[test]
    fn the_snapshot_walk_follows_the_listing_not_the_session_vector() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1"])];
        let wt = Some(PathBuf::from("/a/wt1"));
        // Attached in the order 9 then 4; herdr lists the panes 4 then 9.
        let live = vec![(wt.clone(), 9), (wt.clone(), 4)];
        let listed = sidebar_nav::ListedRows::from([(wt.clone(), vec![
            sidebar_nav::WorkspaceEntry::Session(4),
            sidebar_nav::WorkspaceEntry::Session(9),
        ])]);
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        assert_eq!(rows, vec![
            SidebarRow::Home,
            SidebarRow::Project(PathBuf::from("/a")),
            SidebarRow::Worktree(PathBuf::from("/a/wt1")),
            SidebarRow::Session(4),
            SidebarRow::Session(9),
        ]);
        for row in &rows {
            let id = snapshot.find(row).expect("every projected row is in the model");
            assert!(snapshot.is_projected(id), "{row:?} must stay navigable");
        }
    }

    /// The reconciler must not churn for users who never touch a PR filter:
    /// every banked result would otherwise rebuild the row set.
    #[test]
    fn the_generation_reaches_the_reconciler_only_while_filtering() {
        assert_eq!(pr_generation_for(7, false), 0);
        assert_eq!(pr_generation_for(7, true), 7);
    }
}
