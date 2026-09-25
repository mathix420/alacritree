//! The projects sidebar's row list and cursor, with one owner.
//!
//! The rows are memoised on the `ObservedInputs` fingerprint the reconciler
//! compares, so keyboard paths, paint and the reconciler read one list.
//! Building them needs the whole app, so `stale_rows` hands out the token
//! `fill_rows` requires, and a rebuild always records the inputs it answers
//! for.

use std::path::Path;

use crate::projects::Project;
use crate::session::SessionId;
use crate::sidebar_focus::{
    self, FollowTarget, ObservedInputs, Parent, SessionInput, SnapshotBuilder, TreeSnapshot,
    UiInputs,
};
use crate::sidebar_nav::{self, ListedRows, SidebarRow, WorkspaceEntry};
use crate::workspace::WorkspaceKey;

/// Everything outside the row list that decides it, borrowed for one check.
///
/// `ObservedInputs::capture` and `ObservedInputs::matches` answer for the same
/// frame only when they are fed the same inputs, so both are reached through
/// one value of this type rather than through two argument lists that could
/// drift apart.  `sessions` is cloned for each pass, which is why it has to be
/// a cheap, `Clone` iterator rather than a collected list: the unchanged path
/// runs every frame and must not allocate.
#[derive(Clone)]
pub struct SidebarInputs<'a, S> {
    pub projects: &'a [Project],
    pub sessions: S,
    pub ui: UiInputs<'a>,
}

impl<'a, S: Iterator<Item = SessionInput<'a>> + Clone> SidebarInputs<'a, S> {
    fn matches(&self, observed: &ObservedInputs) -> bool {
        observed.matches(self.projects, self.sessions.clone(), self.ui)
    }

    fn capture(&self) -> ObservedInputs {
        ObservedInputs::capture(self.projects, self.sessions.clone(), self.ui)
    }
}

/// The cached rows no longer describe the inputs.  Carries the fingerprint the
/// rebuilt rows will answer for.
#[must_use = "rebuild the rows and hand this to `fill_rows`"]
pub struct StaleRows(ObservedInputs);

/// One cursor movement over the current rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `n` rows down, or up when negative, clamped to the ends.
    Line(i32),
    /// To the nearest project header below, or above when negative.
    Project(i32),
    First,
    Last,
    /// To the row that owns the cursor.
    Left,
}

#[derive(Debug, Default)]
pub struct SidebarModel {
    cursor: Option<SidebarRow>,
    /// One-shot: scroll the cursor row into view on the next paint.
    cursor_moved: bool,
    /// The deepest row a filter hid, restored when it becomes visible again.
    anchor: Option<SidebarRow>,
    rows: Vec<SidebarRow>,
    /// The listing `rows` was built from.  The snapshot walk reads this one
    /// rather than a fresh listing, which could disagree with `rows`.
    listed: ListedRows,
    /// The inputs `rows` answers for, `None` until the first fill.
    built_for: Option<ObservedInputs>,
    /// Advances on every fill, so the reconciler can tell whether the rows
    /// moved since its last pass without comparing the inputs a second time.
    generation: u64,
    /// The generation the last reconcile ran against, and the tree it built,
    /// the baseline for the next cursor repair.
    reconciled: Option<(u64, TreeSnapshot)>,
}

impl SidebarModel {
    pub fn cursor(&self) -> Option<&SidebarRow> {
        self.cursor.as_ref()
    }

    /// Move the cursor, raising the scroll flag only when it actually moved.
    pub fn set_cursor(&mut self, row: SidebarRow) {
        if self.cursor.as_ref() != Some(&row) {
            self.cursor = Some(row);
            self.cursor_moved = true;
        }
    }

    /// Put the cursor on `row` and scroll to it even when it is already there.
    /// For a row that moved under an unchanged cursor, and for one that is
    /// about to become visible, so the current rows cannot vouch for it.
    pub fn pin_cursor(&mut self, row: SidebarRow) {
        self.cursor = Some(row);
        self.cursor_moved = true;
    }

    /// Land the cursor on `requested`, or on the first row when the current
    /// rows do not hold it, and scroll to it either way: a row that stayed put
    /// in the list can still have moved on screen.
    pub fn seat_cursor(&mut self, requested: Option<SidebarRow>) {
        self.cursor = sidebar_nav::ensure_cursor(&self.rows, requested.as_ref());
        self.cursor_moved = true;
    }

    pub fn take_cursor_moved(&mut self) -> bool {
        std::mem::take(&mut self.cursor_moved)
    }

    /// Forget the anchor: the user navigated, so the row a filter hid is no
    /// longer where they want to return.
    pub fn drop_anchor(&mut self) {
        self.anchor = None;
    }

    pub fn rows(&self) -> &[SidebarRow] {
        &self.rows
    }

    /// `Some` when the rows were built for different inputs, or never built.
    /// Allocation-free when they still hold.
    pub fn stale_rows<'a, S>(&self, inputs: &SidebarInputs<'a, S>) -> Option<StaleRows>
    where
        S: Iterator<Item = SessionInput<'a>> + Clone,
    {
        match &self.built_for {
            Some(built_for) if inputs.matches(built_for) => None,
            _ => Some(StaleRows(inputs.capture())),
        }
    }

    pub fn fill_rows(&mut self, stale: StaleRows, rows: Vec<SidebarRow>, listed: ListedRows) {
        self.rows = rows;
        self.listed = listed;
        self.built_for = Some(stale.0);
        self.generation += 1;
    }

    /// The cursor, when the current rows still hold it.
    ///
    /// A worktree removed, a project collapsed by mouse, or a filter toggle
    /// narrowing the rows out from under it all leave a cursor pointing at a
    /// row that is gone.  That cursor lands on the first row and this answers
    /// `None`, so the caller stops and the next press acts from there;
    /// unfiltered rows always lead with Home.
    pub fn cursor_within(&mut self) -> Option<SidebarRow> {
        if let Some(cursor) = self.cursor.clone().filter(|c| self.rows.contains(c)) {
            return Some(cursor);
        }
        if let Some(first) = self.rows.first().cloned() {
            self.set_cursor(first);
        }
        None
    }

    pub fn move_cursor(&mut self, step: Step) {
        let target = match step {
            Step::First => self.rows.first().cloned(),
            Step::Last => self.rows.last().cloned(),
            Step::Line(delta) => {
                self.cursor_within().map(|c| sidebar_nav::step(&self.rows, &c, delta))
            },
            Step::Project(delta) => self.cursor_within().and_then(|c| {
                if delta > 0 {
                    sidebar_nav::next_project(&self.rows, &c)
                } else {
                    sidebar_nav::previous_project(&self.rows, &c)
                }
            }),
            Step::Left => {
                self.cursor_within().and_then(|c| sidebar_nav::left_target(&self.rows, &c))
            },
        };
        if let Some(row) = target {
            self.set_cursor(row);
        }
    }

    /// Whether the rows moved since the last reconcile.
    pub fn needs_reconcile(&self) -> bool {
        self.reconciled.as_ref().is_none_or(|(generation, _)| *generation != self.generation)
    }

    /// Repair the cursor against what changed since the last pass, and report
    /// where the terminal should follow a removal to.
    ///
    /// `live` is every running session, whatever the listing says, and
    /// `skip_worktree` is a worktree whose deletion is committed but whose git
    /// operation has not finished.  See `build_snapshot` for both.
    pub fn reconcile(
        &mut self,
        projects: &[Project],
        live: &[(WorkspaceKey, SessionId)],
        skip_worktree: Option<&Path>,
    ) -> Option<FollowTarget> {
        let inputs = self.built_for.clone().unwrap_or_default();
        let next = build_snapshot(projects, live, &self.listed, &self.rows, skip_worktree, inputs);
        let prev = match self.reconciled.take() {
            Some((_, prev)) => prev,
            None => next.clone(),
        };
        let outcome =
            sidebar_focus::repair(&prev, &next, self.cursor.as_ref(), self.anchor.as_ref());

        if outcome.cursor != self.cursor {
            self.cursor = outcome.cursor;
            self.cursor_moved = true;
        }
        self.anchor = outcome.anchor;
        self.reconciled = Some((self.generation, next));
        outcome.follow
    }
}

/// Step the lockstep index over the rows a skipped worktree owns.
///
/// The projection is built before the deletion is known, so it still lists
/// the worktree with everything under it.  Leaving the index parked on a row
/// no node will match again would mark every later node unprojected, and the
/// cursor repair reads an unprojected row as one that has gone away.
fn skip_projected_rows(
    rows: &[SidebarRow],
    next_row: &mut usize,
    listed: &ListedRows,
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

/// Assemble the model arena and the projection.  `rows` is the projection,
/// exactly what the cursor steps over, and `live` is the model: every running
/// session, whatever the listing threshold or the filter says.  Building
/// membership from `listed` instead would make the last session in a workspace
/// read as deleted the moment its sibling closed.
///
/// `listed` is the listing the projection was built from.  A herdr row exists
/// only while its agent is listed, so there is no wider model to take it from,
/// and reading a second listing here could disagree with `rows`.
///
/// `skip_worktree` drops a worktree whose deletion is already committed but
/// whose git operation has not finished, so nothing lands the cursor, or a
/// new shell, inside a directory on its way out.
///
/// Nodes are pushed in exactly the order `sidebar_nav::visible_rows` emits,
/// with unprojected nodes interleaved, so one forward index into `rows`
/// classifies every node.  Asking `rows.contains` per node instead would be
/// quadratic in path comparisons on a path that runs whenever the user types.
pub(crate) fn build_snapshot(
    projects: &[Project],
    live: &[(WorkspaceKey, SessionId)],
    listed: &ListedRows,
    rows: &[SidebarRow],
    skip_worktree: Option<&Path>,
    inputs: ObservedInputs,
) -> TreeSnapshot {
    let mut b = SnapshotBuilder::default();
    let mut next_row = 0usize;
    let mut placed = vec![false; live.len()];

    // Consume `rows` in lockstep: a node is projected exactly when it is the
    // row the projection expects next.
    let push = |b: &mut SnapshotBuilder, next_row: &mut usize, row: SidebarRow, parent: Parent| {
        let projected = rows.get(*next_row) == Some(&row);
        if projected {
            *next_row += 1;
        }
        b.push(row, parent, projected)
    };
    let push_workspace = |b: &mut SnapshotBuilder,
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
        for wt in &p.checkouts {
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

    // Sessions whose workspace has no row left, a removed project or a
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::sidebar_nav::tests::project;

    fn ui(query: &str, toggles: u32) -> UiInputs<'_> {
        UiInputs {
            session_rows_always: false,
            sessions_filter_counts_detached: false,
            query,
            toggles,
            toggles_apply: true,
            pr_generation: 0,
            active_workspace: None,
            active_branch: None,
            panes_generation: 0,
        }
    }

    fn inputs<'a>(
        projects: &'a [Project],
        ui: UiInputs<'a>,
    ) -> SidebarInputs<'a, std::iter::Empty<SessionInput<'a>>> {
        SidebarInputs { projects, sessions: std::iter::empty(), ui }
    }

    fn worktree(path: &str) -> SidebarRow {
        SidebarRow::Worktree(PathBuf::from(path))
    }

    /// Fill the rows the way the app does: only when the inputs moved.
    /// Answers whether it had to build.
    fn refresh<'a, S>(
        model: &mut SidebarModel,
        inputs: &SidebarInputs<'a, S>,
        build: impl FnOnce() -> Vec<SidebarRow>,
    ) -> bool
    where
        S: Iterator<Item = SessionInput<'a>> + Clone,
    {
        let Some(stale) = model.stale_rows(inputs) else { return false };
        model.fill_rows(stale, build(), ListedRows::new());
        true
    }

    #[test]
    fn a_filter_toggle_that_hides_the_cursor_reseats_it_and_scrolls() {
        let projects = vec![project("/a", true, &["/a/wt1", "/a/wt2"])];
        let unfiltered = inputs(&projects, ui("", 0));
        let mut model = SidebarModel::default();
        refresh(&mut model, &unfiltered, || {
            sidebar_nav::visible_rows(&projects, &ListedRows::new())
        });
        model.set_cursor(worktree("/a/wt2"));
        model.reconcile(&projects, &[], None);
        model.take_cursor_moved();

        // The sessions toggle leaves only the worktree that holds one.
        let filtered = inputs(&projects, ui("", 1));
        assert!(refresh(&mut model, &filtered, || {
            vec![SidebarRow::Project(PathBuf::from("/a")), worktree("/a/wt1")]
        }));
        assert!(model.needs_reconcile());
        model.reconcile(&projects, &[], None);

        assert_eq!(model.cursor(), Some(&SidebarRow::Project(PathBuf::from("/a"))));
        assert!(model.take_cursor_moved(), "a reseat the user did not ask for must scroll");
    }

    #[test]
    fn consecutive_passes_over_unchanged_inputs_build_nothing() {
        let projects = vec![project("/a", true, &["/a/wt1"])];
        let same = inputs(&projects, ui("wt", 0));
        let mut model = SidebarModel::default();
        let build = || sidebar_nav::visible_rows(&projects, &ListedRows::new());

        assert!(refresh(&mut model, &same, build));
        model.reconcile(&projects, &[], None);

        for pass in 0..2 {
            assert!(!refresh(&mut model, &same, || panic!("pass {pass} rebuilt the rows")));
            assert!(!model.needs_reconcile(), "pass {pass} would rebuild the tree");
        }
    }

    #[test]
    fn a_stale_cursor_reseats_on_the_first_row_and_stops() {
        let projects = vec![project("/a", true, &["/a/wt1", "/a/wt2"])];
        let mut model = SidebarModel::default();
        refresh(&mut model, &inputs(&projects, ui("", 0)), || {
            sidebar_nav::visible_rows(&projects, &ListedRows::new())
        });

        for step in [Step::Line(1), Step::Project(1), Step::Left] {
            model.pin_cursor(worktree("/gone"));
            model.move_cursor(step);
            assert_eq!(model.cursor(), Some(&SidebarRow::Home), "{step:?}");
        }

        // The edges need no cursor to move from.
        model.pin_cursor(worktree("/gone"));
        model.move_cursor(Step::Last);
        assert_eq!(model.cursor(), Some(&worktree("/a/wt2")));
    }

    #[test]
    fn set_cursor_scrolls_only_on_a_move_and_pin_cursor_always() {
        let mut model = SidebarModel::default();
        model.set_cursor(SidebarRow::Home);
        assert!(model.take_cursor_moved());
        model.set_cursor(SidebarRow::Home);
        assert!(!model.take_cursor_moved());
        model.pin_cursor(SidebarRow::Home);
        assert!(model.take_cursor_moved());
    }
}
