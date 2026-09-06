//! Cursor repair for the projects sidebar, derived by observation.
//!
//! Every mutation site reporting what it removed proved unbounded — sessions
//! leave through four paths and worktrees through a background refresh — so
//! this module infers the repair from what changed instead.  The distinction
//! that matters is model versus projection: a row hidden by a filter or a
//! collapsed project still exists and the cursor climbs to an ancestor it can
//! return from, while a row gone from the model was deleted and the cursor
//! slides to a sibling.

use std::path::{Path, PathBuf};

use crate::app::WorkspaceKey;
use crate::projects::{Project, Worktree};
use crate::session::SessionId;
use crate::sidebar_nav::SidebarRow;

/// Index into a single snapshot's `nodes`.  Deliberately not stable across
/// snapshots: cross-snapshot matching goes through the row's own path/session
/// key, because the project list mutates under the cursor and an index would
/// silently retarget.
pub type NodeId = usize;

/// A node's place in the tree.  `Detached` exists because a live session whose
/// project was dropped keeps running: it must be in the model so it never
/// reads as deleted, while never being a sibling of anything and so never a
/// landing the cursor could slide onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parent {
    Root,
    Node(NodeId),
    Detached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub row: SidebarRow,
    pub parent: Parent,
}

/// Model membership plus the current projection.  `nodes` holds every
/// project, worktree, and live session regardless of expansion, listing
/// threshold, or filter; `projected` holds exactly the navigable rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeSnapshot {
    pub nodes: Vec<Node>,
    pub projected: Vec<NodeId>,
    pub inputs: ObservedInputs,
}

impl TreeSnapshot {
    pub fn find(&self, row: &SidebarRow) -> Option<NodeId> {
        self.nodes.iter().position(|n| n.row == *row)
    }

    pub fn is_projected(&self, id: NodeId) -> bool {
        self.projected.contains(&id)
    }

    pub fn row(&self, id: NodeId) -> &SidebarRow {
        &self.nodes[id].row
    }

    pub fn parent(&self, id: NodeId) -> Parent {
        self.nodes[id].parent
    }

    /// `parent`'s projected children in render order — the sibling group a
    /// slide chooses from.
    pub fn children(&self, parent: Parent) -> Vec<NodeId> {
        if parent == Parent::Detached {
            return Vec::new();
        }
        self.projected.iter().copied().filter(|&id| self.nodes[id].parent == parent).collect()
    }

    #[cfg(test)]
    pub fn is_descendant(&self, id: NodeId, ancestor: NodeId) -> bool {
        let mut cur = self.nodes[id].parent;
        while let Parent::Node(p) = cur {
            if p == ancestor {
                return true;
            }
            cur = self.nodes[p].parent;
        }
        false
    }
}

/// Where the cursor lands when `removed` leaves the model.
///
/// The removed row's parent and its ordinal among that parent's children come
/// from `prev`; the landing is chosen from the *surviving* children in `next`.
/// Resolving forward is what makes a simultaneous removal land on a survivor
/// rather than escaping to the parent, and what makes a reordered refresh land
/// on the row that actually occupies the vacated slot.
///
/// `Home` and project headers share `Parent::Root`, which makes them siblings
/// and lets the only project fall back to Home with no special case.
fn slide(prev: &TreeSnapshot, next: &TreeSnapshot, removed: NodeId) -> Option<SidebarRow> {
    let parent = prev.parent(removed);
    if parent == Parent::Detached {
        return None;
    }

    let was = prev.children(parent);
    let ordinal = was.iter().position(|&id| id == removed)?;

    let parent_in_next = match parent {
        Parent::Root => Parent::Root,
        Parent::Node(p) => Parent::Node(next.find(prev.row(p))?),
        Parent::Detached => return None,
    };
    let survivors = next.children(parent_in_next);

    if let Some(&landed) = survivors.get(ordinal) {
        return Some(next.row(landed).clone());
    }
    // The removed row was last, so the nearest preceding survivor is the new
    // last child.
    if let Some(&last) = survivors.last() {
        return Some(next.row(last).clone());
    }
    match parent {
        Parent::Node(p) => Some(prev.row(p).clone()),
        _ => None,
    }
}

/// What the terminal switches to when a removal landing has something live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowTarget {
    Session(SessionId),
    /// The caller activates this workspace's active session, or its first
    /// live one when the active entry is stale.
    Workspace(WorkspaceKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub cursor: Option<SidebarRow>,
    pub anchor: Option<SidebarRow>,
    /// Only ever `Some` for a removal landing.  The caller drops it unless
    /// `ui.sidebar_focus` is `"follow"`.
    pub follow: Option<FollowTarget>,
}

/// The nearest ancestor of `from` that `next` still projects, walking `from`'s
/// own snapshot so a removed row's chain is still readable.  Root rows have no
/// ancestor, so the first projected row — Home whenever it is visible — is the
/// last resort.
fn climb(from_tree: &TreeSnapshot, next: &TreeSnapshot, from: NodeId) -> Option<SidebarRow> {
    let mut cur = from_tree.parent(from);
    while let Parent::Node(id) = cur {
        let row = from_tree.row(id);
        if next.find(row).is_some_and(|n| next.is_projected(n)) {
            return Some(row.clone());
        }
        cur = from_tree.parent(id);
    }
    next.projected.first().map(|&id| next.row(id).clone())
}

/// What a removal landing offers the terminal.  A workspace row with no live
/// session yields `None`: spawning a shell the user did not ask for is not
/// this module's job.
fn follow_target(next: &TreeSnapshot, landing: &SidebarRow) -> Option<FollowTarget> {
    match landing {
        SidebarRow::Session(id) => Some(FollowTarget::Session(*id)),
        SidebarRow::Project(_) | SidebarRow::HerdrAgent(..) => None,
        SidebarRow::Home | SidebarRow::Worktree(_) => {
            let id = next.find(landing)?;
            let has_session = next.nodes.iter().any(|node| {
                matches!(node.row, SidebarRow::Session(_)) && node.parent == Parent::Node(id)
            });
            if !has_session {
                return None;
            }
            let ws = match landing {
                SidebarRow::Worktree(path) => Some(path.clone()),
                _ => None,
            };
            Some(FollowTarget::Workspace(ws))
        },
    }
}

/// Repair the cursor against what changed between two snapshots.
///
/// The row under repair is the anchor when one is set — a climb parks the
/// visible cursor on an ancestor while the user's real position waits in the
/// anchor, so judging removal by the visible cursor would never notice a
/// hidden row being deleted.  Cursor, anchor, and terminal resolve together so
/// the caller cannot apply them out of order.
pub fn repair(
    prev: &TreeSnapshot,
    next: &TreeSnapshot,
    cursor: Option<&SidebarRow>,
    anchor: Option<&SidebarRow>,
) -> Repair {
    // The anchor belongs to one filter episode.  Nothing is filtering, so the
    // episode is over however it ended — confirmed, cancelled, or widened.
    let anchor = anchor.filter(|_| next.inputs.is_filtering());

    if let Some(a) = anchor {
        match next.find(a) {
            // Visible again: the user gets their row back.
            Some(id) if next.is_projected(id) => {
                return Repair { cursor: Some(a.clone()), anchor: None, follow: None };
            },
            // Still hidden: leave it parked and repair the visible cursor.
            Some(_) => {},
            // Deleted while out of sight; there is nothing left to restore.
            None => {
                return repair_visible(prev, next, cursor, None);
            },
        }
    }

    repair_visible(prev, next, cursor, anchor)
}

fn repair_visible(
    prev: &TreeSnapshot,
    next: &TreeSnapshot,
    cursor: Option<&SidebarRow>,
    anchor: Option<&SidebarRow>,
) -> Repair {
    let unchanged = Repair { cursor: cursor.cloned(), anchor: anchor.cloned(), follow: None };

    let Some(c) = cursor else {
        return unchanged;
    };

    match next.find(c) {
        Some(id) if next.is_projected(id) => unchanged,
        // Still in the model, so a filter or a collapse hid it: climb, and
        // remember the deepest row the user actually chose.
        Some(id) => Repair {
            cursor: climb(next, next, id),
            anchor: Some(anchor.cloned().unwrap_or_else(|| c.clone())),
            follow: None,
        },
        None => {
            let Some(removed) = prev.find(c) else {
                return Repair {
                    cursor: next.projected.first().map(|&id| next.row(id).clone()),
                    anchor: None,
                    follow: None,
                };
            };
            let landing = slide(prev, next, removed)
                .filter(|row| next.find(row).is_some_and(|id| next.is_projected(id)));
            match landing {
                Some(row) => {
                    let follow = follow_target(next, &row);
                    Repair { cursor: Some(row), anchor: None, follow }
                },
                // The slide target is itself hidden — a removal that also
                // changed what the filter keeps.  Fall through to the climb.
                None => Repair { cursor: climb(prev, next, removed), anchor: None, follow: None },
            }
        },
    }
}

#[derive(Default)]
pub struct SnapshotBuilder {
    nodes: Vec<Node>,
    projected: Vec<NodeId>,
}

impl SnapshotBuilder {
    pub fn push(&mut self, row: SidebarRow, parent: Parent, projected: bool) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node { row, parent });
        if projected {
            self.projected.push(id);
        }
        id
    }

    pub fn finish(self, inputs: ObservedInputs) -> TreeSnapshot {
        TreeSnapshot { nodes: self.nodes, projected: self.projected, inputs }
    }
}

/// One live session, borrowed for the per-frame comparison.
#[derive(Debug, Clone, Copy)]
pub struct SessionInput<'a> {
    pub workspace: &'a WorkspaceKey,
    pub id: SessionId,
    pub attention: bool,
}

/// Sidebar UI inputs that change the projection without changing the model.
/// `toggles` is a bitmask rather than a slice so the comparison never
/// allocates.
#[derive(Debug, Clone, Copy)]
pub struct UiInputs<'a> {
    pub session_rows_always: bool,
    pub query: &'a str,
    pub toggles: u32,
    /// Whether the toggles narrow rows this frame.  A search scope that stands
    /// them down changes the projection while `toggles` itself holds still.
    pub toggles_apply: bool,
    /// Advances when a PR lookup is banked or invalidated.  Fed as `0` unless a
    /// PR filter is active, so a completion cannot invalidate a projection it
    /// could not have changed.
    pub pr_generation: u64,
    /// The workspace whose live branch `active_branch` describes.  Without it a
    /// switch between two worktrees whose caches hold the same branch string
    /// moves every PR lookup key while nothing observed changes.
    pub active_workspace: Option<&'a Path>,
    pub active_branch: Option<&'a str>,
    /// Advances when a herdr poll changes something a row draws.  Agent churn
    /// no row shows deliberately does not move it, so an idle agent repainting
    /// does not rebuild the tree.
    pub herdr_generation: u64,
}

#[cfg(test)]
thread_local! {
    static VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Records one examined record.  Compiled out of release builds entirely —
/// the counter exists so the linearity of `matches` can be asserted without a
/// wall-clock threshold, which on a shared runner is either flaky or blind.
#[inline(always)]
fn visit() {
    #[cfg(test)]
    VISITS.with(|v| v.set(v.get() + 1));
}

#[cfg(test)]
pub fn visits() -> usize {
    VISITS.with(|v| v.get())
}

#[cfg(test)]
pub fn reset_visits() {
    VISITS.with(|v| v.set(0));
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProjectInput {
    root: PathBuf,
    name: String,
    expanded: bool,
    worktrees: Vec<WorktreeInput>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorktreeInput {
    path: PathBuf,
    name: String,
    prunable: bool,
    branch: Option<String>,
}

/// A [`WorktreeInput`] and a live [`Worktree`] reduced to the same borrowed
/// shape.  The compare path runs every frame and must not allocate, so the two
/// are matched through this rather than by cloning one into the other's type —
/// and a field added to `WorktreeInput` without a view of it stops compiling
/// instead of silently dropping out of the comparison.
#[derive(PartialEq, Eq)]
struct WorktreeView<'a> {
    path: &'a Path,
    name: &'a str,
    prunable: bool,
    branch: Option<&'a str>,
}

impl WorktreeInput {
    fn view(&self) -> WorktreeView<'_> {
        WorktreeView {
            path: &self.path,
            name: &self.name,
            prunable: self.prunable,
            branch: self.branch.as_deref(),
        }
    }
}

impl<'a> From<&'a Worktree> for WorktreeView<'a> {
    fn from(wt: &'a Worktree) -> Self {
        Self { path: &wt.path, name: &wt.name, prunable: wt.prunable, branch: wt.branch.as_deref() }
    }
}

/// Everything the snapshot is a function of.  Captured on rebuild, compared
/// borrowed on every other frame so the steady state allocates nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedInputs {
    projects: Vec<ProjectInput>,
    sessions: Vec<(WorkspaceKey, SessionId, bool)>,
    session_rows_always: bool,
    query: String,
    toggles: u32,
    toggles_apply: bool,
    pr_generation: u64,
    active_workspace: Option<PathBuf>,
    active_branch: Option<String>,
    herdr_generation: u64,
}

impl ObservedInputs {
    pub fn capture<'a>(
        projects: &[Project],
        sessions: impl Iterator<Item = SessionInput<'a>>,
        ui: UiInputs<'_>,
    ) -> Self {
        Self {
            projects: projects
                .iter()
                .map(|p| ProjectInput {
                    root: p.root.clone(),
                    name: p.display_name().to_string(),
                    expanded: p.expanded,
                    worktrees: p
                        .worktrees
                        .iter()
                        .map(|wt| WorktreeInput {
                            path: wt.path.clone(),
                            name: wt.name.clone(),
                            prunable: wt.prunable,
                            branch: wt.branch.clone(),
                        })
                        .collect(),
                })
                .collect(),
            sessions: sessions.map(|s| (s.workspace.clone(), s.id, s.attention)).collect(),
            session_rows_always: ui.session_rows_always,
            query: ui.query.to_string(),
            toggles: ui.toggles,
            toggles_apply: ui.toggles_apply,
            pr_generation: ui.pr_generation,
            active_workspace: ui.active_workspace.map(Path::to_path_buf),
            active_branch: ui.active_branch.map(str::to_string),
            herdr_generation: ui.herdr_generation,
        }
    }

    /// Whether a filter is narrowing the tree.  The anchor exists only for the
    /// duration of one filter episode, so this is what ends it.
    pub fn is_filtering(&self) -> bool {
        !self.query.is_empty() || self.toggles != 0
    }

    /// Whether every observed input still holds.  Allocation-free: this runs
    /// on every frame the sidebar is live.
    pub fn matches<'a>(
        &self,
        projects: &[Project],
        sessions: impl Iterator<Item = SessionInput<'a>>,
        ui: UiInputs<'_>,
    ) -> bool {
        if self.session_rows_always != ui.session_rows_always
            || self.query != ui.query
            || self.toggles != ui.toggles
            || self.toggles_apply != ui.toggles_apply
            || self.pr_generation != ui.pr_generation
            || self.active_workspace.as_deref() != ui.active_workspace
            || self.active_branch.as_deref() != ui.active_branch
            || self.herdr_generation != ui.herdr_generation
        {
            return false;
        }
        if self.projects.len() != projects.len() {
            return false;
        }
        for (was, now) in self.projects.iter().zip(projects) {
            visit();
            if was.root != now.root
                || was.name != now.display_name()
                || was.expanded != now.expanded
                || was.worktrees.len() != now.worktrees.len()
            {
                return false;
            }
            for (wt_was, wt_now) in was.worktrees.iter().zip(&now.worktrees) {
                visit();
                if wt_was.view() != WorktreeView::from(wt_now) {
                    return false;
                }
            }
        }
        let mut seen = 0usize;
        for s in sessions {
            visit();
            match self.sessions.get(seen) {
                Some((ws, id, attention))
                    if ws == s.workspace && *id == s.id && *attention == s.attention =>
                {
                    seen += 1;
                },
                _ => return false,
            }
        }
        seen == self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_project(p: &str) -> SidebarRow {
        SidebarRow::Project(PathBuf::from(p))
    }

    fn row_worktree(p: &str) -> SidebarRow {
        SidebarRow::Worktree(PathBuf::from(p))
    }

    fn ui(query: &str, toggles: u32) -> UiInputs<'_> {
        UiInputs {
            session_rows_always: false,
            query,
            toggles,
            toggles_apply: true,
            pr_generation: 0,
            active_workspace: None,
            active_branch: None,
            herdr_generation: 0,
        }
    }

    fn ui_full<'a>(
        query: &'a str,
        toggles: u32,
        toggles_apply: bool,
        pr_generation: u64,
        active_workspace: Option<&'a Path>,
        active_branch: Option<&'a str>,
    ) -> UiInputs<'a> {
        UiInputs {
            session_rows_always: false,
            query,
            toggles,
            toggles_apply,
            pr_generation,
            active_workspace,
            active_branch,
            herdr_generation: 0,
        }
    }

    /// Each new field is a way the row set moves without any older field
    /// moving. Missing one leaves the sidebar showing a stale projection.
    #[test]
    fn each_new_ui_input_invalidates_the_snapshot() {
        let projects: Vec<Project> = Vec::new();
        let none: [SessionInput<'_>; 0] = [];
        let wt = PathBuf::from("/repo/wt");

        let base = ui_full("q", 0b01, true, 7, Some(&wt), Some("main"));
        let captured = ObservedInputs::capture(&projects, none.iter().copied(), base);

        assert!(captured.matches(&projects, none.iter().copied(), base), "control");

        for changed in [
            ui_full("q", 0b01, false, 7, Some(&wt), Some("main")),
            ui_full("q", 0b01, true, 8, Some(&wt), Some("main")),
            ui_full("q", 0b01, true, 7, None, Some("main")),
            ui_full("q", 0b01, true, 7, Some(&wt), Some("feature")),
        ] {
            assert!(
                !captured.matches(&projects, none.iter().copied(), changed),
                "a changed input reported unchanged: {changed:?}"
            );
        }
    }

    #[test]
    fn a_herdr_generation_bump_invalidates_the_snapshot() {
        let inputs = ObservedInputs::capture(&[], std::iter::empty(), ui("", 0));
        let mut moved = ui("", 0);
        moved.herdr_generation = 1;
        assert!(inputs.matches(&[], std::iter::empty(), ui("", 0)));
        assert!(!inputs.matches(&[], std::iter::empty(), moved));
    }

    #[test]
    fn a_worktree_branch_change_invalidates_the_snapshot() {
        use crate::sidebar_nav::tests::project;

        let mut a = project("/a", true, &["/a/wt1"]);
        a.worktrees[0].branch = Some("main".to_string());
        let base = ObservedInputs::capture(&[a.clone()], std::iter::empty(), ui("", 0));

        let mut changed = a.clone();
        changed.worktrees[0].branch = Some("feature".to_string());

        assert!(!base.matches(&[changed], std::iter::empty(), ui("", 0)));
    }

    /// home, project /a expanded with worktree /a/wt1 holding sessions 1 and 2.
    fn snapshot() -> TreeSnapshot {
        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        let a = b.push(row_project("/a"), Parent::Root, true);
        let wt1 = b.push(row_worktree("/a/wt1"), Parent::Node(a), true);
        b.push(SidebarRow::Session(1), Parent::Node(wt1), true);
        b.push(SidebarRow::Session(2), Parent::Node(wt1), true);
        b.finish(ObservedInputs::default())
    }

    #[test]
    fn find_matches_across_snapshots_by_stable_key() {
        let s = snapshot();
        let id = s.find(&row_worktree("/a/wt1")).expect("worktree is in the model");
        assert_eq!(*s.row(id), row_worktree("/a/wt1"));
        assert_eq!(s.find(&row_worktree("/a/gone")), None);
    }

    #[test]
    fn unprojected_nodes_stay_in_the_model() {
        let mut b = SnapshotBuilder::default();
        let a = b.push(row_project("/a"), Parent::Root, true);
        b.push(row_worktree("/a/wt1"), Parent::Node(a), false);
        let s = b.finish(ObservedInputs::default());

        let wt = s.find(&row_worktree("/a/wt1")).expect("collapsed worktrees stay in the model");
        assert!(!s.is_projected(wt), "a collapsed worktree is not navigable");
    }

    #[test]
    fn a_detached_node_is_in_the_model_but_is_nobodys_sibling() {
        let mut b = SnapshotBuilder::default();
        let home = b.push(SidebarRow::Home, Parent::Root, true);
        b.push(SidebarRow::Session(9), Parent::Detached, false);
        let s = b.finish(ObservedInputs::default());

        assert!(s.find(&SidebarRow::Session(9)).is_some(), "an orphan session is not deleted");
        assert!(
            !s.children(Parent::Root).contains(&s.find(&SidebarRow::Session(9)).unwrap()),
            "an orphan must never be a root sibling — it would be a legal slide landing"
        );
        assert_eq!(s.children(Parent::Root), vec![home]);
    }

    #[test]
    fn children_are_projected_only_and_in_render_order() {
        let s = snapshot();
        let wt1 = s.find(&row_worktree("/a/wt1")).unwrap();
        let kids = s.children(Parent::Node(wt1));
        assert_eq!(
            kids.iter().map(|&id| s.row(id).clone()).collect::<Vec<_>>(),
            vec![SidebarRow::Session(1), SidebarRow::Session(2)]
        );
    }

    #[test]
    fn is_filtering_tracks_the_query_and_the_toggle_bits() {
        assert!(!ObservedInputs::capture(&[], std::iter::empty(), ui("", 0)).is_filtering());
        assert!(ObservedInputs::capture(&[], std::iter::empty(), ui("x", 0)).is_filtering());
        assert!(ObservedInputs::capture(&[], std::iter::empty(), ui("", 0b10)).is_filtering());
    }

    #[test]
    fn every_observed_input_in_isolation_triggers_a_rebuild() {
        let session = |ws: &'static WorkspaceKey, id, attention| SessionInput {
            workspace: ws,
            id,
            attention,
        };
        static HOME: WorkspaceKey = None;

        let base = ObservedInputs::capture(&[], [session(&HOME, 1, false)].into_iter(), ui("", 0));

        assert!(base.matches(&[], [session(&HOME, 1, false)].into_iter(), ui("", 0)));

        // Each UI input on its own.
        assert!(!base.matches(&[], [session(&HOME, 1, false)].into_iter(), ui("x", 0)));
        assert!(!base.matches(&[], [session(&HOME, 1, false)].into_iter(), ui("", 0b01)));
        assert!(!base.matches(
            &[],
            [session(&HOME, 1, false)].into_iter(),
            UiInputs {
                session_rows_always: true,
                query: "",
                toggles: 0,
                toggles_apply: true,
                pr_generation: 0,
                active_workspace: None,
                active_branch: None,
                herdr_generation: 0,
            },
        ));

        // Each session input on its own: attention, id, count.
        assert!(!base.matches(&[], [session(&HOME, 1, true)].into_iter(), ui("", 0)));
        assert!(!base.matches(&[], [session(&HOME, 2, false)].into_iter(), ui("", 0)));
        assert!(!base.matches(&[], std::iter::empty(), ui("", 0)));
        assert!(!base.matches(
            &[],
            [session(&HOME, 1, false), session(&HOME, 2, false)].into_iter(),
            ui("", 0),
        ));
    }

    #[test]
    fn project_shape_changes_trigger_a_rebuild() {
        use crate::sidebar_nav::tests::project;

        let a = vec![project("/a", true, &["/a/wt1", "/a/wt2"])];
        let base = ObservedInputs::capture(&a, std::iter::empty(), ui("", 0));
        assert!(base.matches(&a, std::iter::empty(), ui("", 0)));

        // Expansion, worktree set, worktree order, root, and count each count.
        assert!(!base.matches(
            &[project("/a", false, &["/a/wt1", "/a/wt2"])],
            std::iter::empty(),
            ui("", 0)
        ));
        assert!(!base.matches(&[project("/a", true, &["/a/wt1"])], std::iter::empty(), ui("", 0)));
        assert!(!base.matches(
            &[project("/a", true, &["/a/wt2", "/a/wt1"])],
            std::iter::empty(),
            ui("", 0)
        ));
        assert!(!base.matches(
            &[project("/b", true, &["/a/wt1", "/a/wt2"])],
            std::iter::empty(),
            ui("", 0)
        ));
        assert!(!base.matches(&[], std::iter::empty(), ui("", 0)));
    }

    /// The reference tree from the design spec.
    fn reference_tree() -> TreeSnapshot {
        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);

        let p1 = b.push(row_project("/p1"), Parent::Root, true);
        let p1wt1 = b.push(row_worktree("/p1/wt1"), Parent::Node(p1), true);
        b.push(SidebarRow::Session(11), Parent::Node(p1wt1), true);
        b.push(SidebarRow::Session(12), Parent::Node(p1wt1), true);
        let p1wt2 = b.push(row_worktree("/p1/wt2"), Parent::Node(p1), true);
        b.push(SidebarRow::Session(21), Parent::Node(p1wt2), true);
        b.push(SidebarRow::Session(22), Parent::Node(p1wt2), true);
        b.push(SidebarRow::Session(23), Parent::Node(p1wt2), true);

        let p2 = b.push(row_project("/p2"), Parent::Root, true);
        let p2wt1 = b.push(row_worktree("/p2/wt1"), Parent::Node(p2), true);
        b.push(SidebarRow::Session(31), Parent::Node(p2wt1), true);
        b.push(SidebarRow::Session(32), Parent::Node(p2wt1), true);
        b.push(row_worktree("/p2/wt2"), Parent::Node(p2), true);
        b.push(row_worktree("/p2/wt3"), Parent::Node(p2), true);

        b.finish(ObservedInputs::default())
    }

    /// The reference tree with every row in `drop` and their descendants absent
    /// from the model — what a deletion leaves behind.
    fn reference_tree_without(drop: &[SidebarRow]) -> TreeSnapshot {
        let full = reference_tree();
        let dropped: Vec<NodeId> =
            drop.iter().map(|r| full.find(r).expect("row is in the reference tree")).collect();
        let gone = |id: NodeId| dropped.iter().any(|&d| d == id || full.is_descendant(id, d));

        let mut b = SnapshotBuilder::default();
        let mut remap = std::collections::HashMap::new();
        for (old, node) in full.nodes.iter().enumerate() {
            if gone(old) {
                continue;
            }
            let parent = match node.parent {
                Parent::Node(p) => Parent::Node(remap[&p]),
                other => other,
            };
            let new = b.push(node.row.clone(), parent, full.is_projected(old));
            remap.insert(old, new);
        }
        b.finish(ObservedInputs::default())
    }

    fn slide_from(
        prev: &TreeSnapshot,
        next: &TreeSnapshot,
        row: &SidebarRow,
    ) -> Option<SidebarRow> {
        slide(prev, next, prev.find(row).expect("row is in the previous model"))
    }

    #[test]
    fn a_last_child_slides_back_to_its_previous_sibling() {
        let prev = reference_tree();
        let next = reference_tree_without(&[SidebarRow::Session(12)]);
        assert_eq!(
            slide_from(&prev, &next, &SidebarRow::Session(12)),
            Some(SidebarRow::Session(11))
        );
    }

    #[test]
    fn a_middle_child_slides_forward_into_the_vacated_slot() {
        let prev = reference_tree();
        let next = reference_tree_without(&[SidebarRow::Session(22)]);
        assert_eq!(
            slide_from(&prev, &next, &SidebarRow::Session(22)),
            Some(SidebarRow::Session(23))
        );
    }

    #[test]
    fn a_middle_worktree_slides_to_the_next_worktree() {
        let prev = reference_tree();
        let next = reference_tree_without(&[row_worktree("/p2/wt2")]);
        assert_eq!(
            slide_from(&prev, &next, &row_worktree("/p2/wt2")),
            Some(row_worktree("/p2/wt3"))
        );
    }

    #[test]
    fn a_removed_worktree_carries_its_sessions_out_of_the_slot() {
        let prev = reference_tree();
        let next = reference_tree_without(&[row_worktree("/p1/wt1")]);
        // /p1/wt1 owns sessions 11 and 12; the slot must hold /p1/wt2, never a
        // session orphaned by the removal.
        assert_eq!(
            slide_from(&prev, &next, &row_worktree("/p1/wt1")),
            Some(row_worktree("/p1/wt2"))
        );
    }

    #[test]
    fn two_siblings_removed_at_once_still_land_on_a_survivor() {
        let prev = reference_tree();
        // Sessions 22 and 23 both go; 21 survives and must catch the cursor
        // instead of the parent worktree.
        let next = reference_tree_without(&[SidebarRow::Session(22), SidebarRow::Session(23)]);
        assert_eq!(
            slide_from(&prev, &next, &SidebarRow::Session(22)),
            Some(SidebarRow::Session(21))
        );
    }

    #[test]
    fn a_reorder_alongside_a_removal_takes_the_row_now_in_the_slot() {
        let prev = reference_tree();
        // A background refresh reinstalls /p2's worktrees in a different order
        // while wt2 disappears: the row now occupying wt2's ordinal is wt1.
        let next = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            let p2 = b.push(row_project("/p2"), Parent::Root, true);
            b.push(row_worktree("/p2/wt3"), Parent::Node(p2), true);
            b.push(row_worktree("/p2/wt1"), Parent::Node(p2), true);
            b.finish(ObservedInputs::default())
        };

        assert_eq!(
            slide_from(&prev, &next, &row_worktree("/p2/wt2")),
            Some(row_worktree("/p2/wt1")),
            "the vacated ordinal wins, not whichever row used to follow"
        );
    }

    #[test]
    fn an_only_child_falls_back_to_its_parent() {
        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        let p = b.push(row_project("/a"), Parent::Root, true);
        let wt = b.push(row_worktree("/a/wt1"), Parent::Node(p), true);
        b.push(SidebarRow::Session(7), Parent::Node(wt), true);
        let prev = b.finish(ObservedInputs::default());

        let without_session = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            let p = b.push(row_project("/a"), Parent::Root, true);
            b.push(row_worktree("/a/wt1"), Parent::Node(p), true);
            b.finish(ObservedInputs::default())
        };
        assert_eq!(
            slide_from(&prev, &without_session, &SidebarRow::Session(7)),
            Some(row_worktree("/a/wt1"))
        );

        let without_worktree = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            b.push(row_project("/a"), Parent::Root, true);
            b.finish(ObservedInputs::default())
        };
        assert_eq!(
            slide_from(&prev, &without_worktree, &row_worktree("/a/wt1")),
            Some(row_project("/a"))
        );
    }

    #[test]
    fn top_level_rows_are_siblings_of_home() {
        let prev = reference_tree();

        // A middle project takes the next project.
        let next = reference_tree_without(&[row_project("/p1")]);
        assert_eq!(slide_from(&prev, &next, &row_project("/p1")), Some(row_project("/p2")));

        // The last project falls back to the previous one.
        let next = reference_tree_without(&[row_project("/p2")]);
        assert_eq!(slide_from(&prev, &next, &row_project("/p2")), Some(row_project("/p1")));

        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        b.push(row_project("/only"), Parent::Root, true);
        let single = b.finish(ObservedInputs::default());
        let bare = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            b.finish(ObservedInputs::default())
        };
        // The only project has no project sibling, so Home stands in.
        assert_eq!(slide_from(&single, &bare, &row_project("/only")), Some(SidebarRow::Home));
    }

    #[test]
    fn a_detached_row_has_no_slide() {
        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        b.push(SidebarRow::Session(9), Parent::Detached, false);
        let prev = b.finish(ObservedInputs::default());

        let next = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            b.finish(ObservedInputs::default())
        };

        assert_eq!(slide_from(&prev, &next, &SidebarRow::Session(9)), None);
    }

    /// Inputs standing for "a query is narrowing the tree", so the anchor's
    /// filter episode is open.  `ObservedInputs::default()` is *not* filtering,
    /// which would retire the anchor on sight.
    fn filtering() -> ObservedInputs {
        ObservedInputs::capture(&[], std::iter::empty(), ui("wt", 0))
    }

    /// The reference tree with /p1/wt2 and its sessions hidden by a filter.
    fn reference_tree_filtered() -> TreeSnapshot {
        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        let p1 = b.push(row_project("/p1"), Parent::Root, true);
        let p1wt1 = b.push(row_worktree("/p1/wt1"), Parent::Node(p1), true);
        b.push(SidebarRow::Session(11), Parent::Node(p1wt1), true);
        b.push(SidebarRow::Session(12), Parent::Node(p1wt1), true);
        let p1wt2 = b.push(row_worktree("/p1/wt2"), Parent::Node(p1), false);
        b.push(SidebarRow::Session(21), Parent::Node(p1wt2), false);
        b.push(SidebarRow::Session(22), Parent::Node(p1wt2), false);
        b.push(SidebarRow::Session(23), Parent::Node(p1wt2), false);
        b.finish(filtering())
    }

    #[test]
    fn a_visible_cursor_is_left_alone() {
        let t = reference_tree();
        let r = repair(&t, &t, Some(&SidebarRow::Session(11)), None);
        assert_eq!(r.cursor, Some(SidebarRow::Session(11)));
        assert_eq!(r.anchor, None);
        assert_eq!(r.follow, None);
    }

    #[test]
    fn an_unrelated_removal_does_not_move_the_cursor() {
        let prev = reference_tree();
        let next = reference_tree_without(&[SidebarRow::Session(31)]);

        let r = repair(&prev, &next, Some(&SidebarRow::Session(11)), None);
        assert_eq!(r.cursor, Some(SidebarRow::Session(11)));
        assert_eq!(r.follow, None);
    }

    #[test]
    fn collapsing_a_project_climbs_rather_than_slides() {
        let prev = reference_tree();
        // /p1 collapses: its worktrees and their sessions leave the projection
        // but stay in the model.
        let next = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            let p1 = b.push(row_project("/p1"), Parent::Root, true);
            let wt1 = b.push(row_worktree("/p1/wt1"), Parent::Node(p1), false);
            b.push(SidebarRow::Session(11), Parent::Node(wt1), false);
            b.push(SidebarRow::Session(12), Parent::Node(wt1), false);
            b.finish(filtering())
        };

        let r = repair(&prev, &next, Some(&SidebarRow::Session(12)), None);
        assert_eq!(
            r.cursor,
            Some(row_project("/p1")),
            "the collapsed header is the nearest visible ancestor"
        );
        assert_eq!(r.anchor, Some(SidebarRow::Session(12)), "expanding again must restore the row");
        assert_eq!(r.follow, None, "collapsing is a projection change, so nothing follows");
    }

    #[test]
    fn dropping_below_the_listing_threshold_climbs_rather_than_slides() {
        // Two sessions under /a/wt1 are listed; one is closed, so the survivor
        // falls below the threshold and stops being a row while staying live.
        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        let p = b.push(row_project("/a"), Parent::Root, true);
        let wt = b.push(row_worktree("/a/wt1"), Parent::Node(p), true);
        b.push(SidebarRow::Session(1), Parent::Node(wt), true);
        b.push(SidebarRow::Session(2), Parent::Node(wt), true);
        let prev = b.finish(filtering());

        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        let p = b.push(row_project("/a"), Parent::Root, true);
        let wt = b.push(row_worktree("/a/wt1"), Parent::Node(p), true);
        b.push(SidebarRow::Session(1), Parent::Node(wt), false);
        let next = b.finish(filtering());

        let r = repair(&prev, &next, Some(&SidebarRow::Session(1)), None);
        assert_eq!(r.cursor, Some(row_worktree("/a/wt1")));
        assert_eq!(
            r.anchor,
            Some(SidebarRow::Session(1)),
            "the session is live, so it can come back"
        );
        assert_eq!(r.follow, None);
    }

    #[test]
    fn a_filtered_out_cursor_climbs_and_anchors() {
        let prev = reference_tree();
        let next = reference_tree_filtered();

        let r = repair(&prev, &next, Some(&SidebarRow::Session(22)), None);
        assert_eq!(r.cursor, Some(row_project("/p1")), "wt2 is hidden too, so the climb continues");
        assert_eq!(r.anchor, Some(SidebarRow::Session(22)));
        assert_eq!(r.follow, None, "a filter never moves the terminal");
    }

    #[test]
    fn successive_narrowing_keeps_the_deepest_anchor() {
        let prev = reference_tree();
        let next = reference_tree_filtered();

        let r =
            repair(&prev, &next, Some(&row_worktree("/p1/wt2")), Some(&SidebarRow::Session(22)));
        assert_eq!(
            r.anchor,
            Some(SidebarRow::Session(22)),
            "the intermediate ancestor must not win"
        );
    }

    #[test]
    fn a_visible_anchor_is_restored_and_retired() {
        let prev = reference_tree_filtered();
        let mut next = reference_tree();
        next.inputs = filtering();

        let r = repair(&prev, &next, Some(&row_project("/p1")), Some(&SidebarRow::Session(22)));
        assert_eq!(r.cursor, Some(SidebarRow::Session(22)));
        assert_eq!(r.anchor, None);
        assert_eq!(r.follow, None, "restoring an anchor is a filter event, not a removal");
    }

    #[test]
    fn ending_the_filter_episode_retires_the_anchor() {
        // Confirm/cancel/Shift+Esc all clear the query.  The confirmed row here is
        // the same one the climb already chose, so nothing observable changed —
        // only the episode ending can retire the anchor.
        let prev = reference_tree_filtered();
        let next = reference_tree();
        assert!(!next.inputs.is_filtering(), "the reference tree is unfiltered");

        let r = repair(&prev, &next, Some(&row_project("/p1")), Some(&SidebarRow::Session(22)));
        assert_eq!(r.cursor, Some(row_project("/p1")), "the confirmed row stands");
        assert_eq!(r.anchor, None, "a stale anchor must not yank the cursor away later");
    }

    #[test]
    fn an_anchored_row_deleted_while_hidden_drops_the_anchor() {
        let prev = reference_tree_filtered();
        // Session 22 was hidden and anchored; it exits while out of sight.
        let mut next = reference_tree_without(&[SidebarRow::Session(22)]);
        next.inputs = filtering();

        let r = repair(&prev, &next, Some(&row_project("/p1")), Some(&SidebarRow::Session(22)));
        assert_eq!(r.anchor, None, "an anchor that left the model can never be restored");
        assert_eq!(r.cursor, Some(row_project("/p1")), "the visible cursor is still fine");
        assert_eq!(r.follow, None, "the row was not on screen, so the terminal does not chase it");
    }

    #[test]
    fn a_removed_cursor_slides_and_follows() {
        let prev = reference_tree();
        let next = reference_tree_without(&[SidebarRow::Session(22)]);

        let r = repair(&prev, &next, Some(&SidebarRow::Session(22)), None);
        assert_eq!(r.cursor, Some(SidebarRow::Session(23)));
        assert_eq!(r.follow, Some(FollowTarget::Session(23)));
    }

    #[test]
    fn a_landing_on_a_workspace_follows_its_live_session() {
        let prev = reference_tree();
        // /p2/wt2 has no sessions, so landing there offers nothing to follow.
        let next = reference_tree_without(&[row_worktree("/p2/wt3")]);
        let r = repair(&prev, &next, Some(&row_worktree("/p2/wt3")), None);
        assert_eq!(r.cursor, Some(row_worktree("/p2/wt2")));
        assert_eq!(r.follow, None);

        // Landing on a worktree that does have sessions follows the workspace.
        let next = reference_tree_without(&[row_worktree("/p1/wt2")]);
        let r = repair(&prev, &next, Some(&row_worktree("/p1/wt2")), None);
        assert_eq!(r.cursor, Some(row_worktree("/p1/wt1")));
        assert_eq!(r.follow, Some(FollowTarget::Workspace(Some(PathBuf::from("/p1/wt1")))));
    }

    #[test]
    fn a_project_landing_never_follows() {
        let mut b = SnapshotBuilder::default();
        b.push(SidebarRow::Home, Parent::Root, true);
        let p = b.push(row_project("/a"), Parent::Root, true);
        b.push(row_worktree("/a/wt1"), Parent::Node(p), true);
        let prev = b.finish(ObservedInputs::default());

        let next = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            b.push(row_project("/a"), Parent::Root, true);
            b.finish(ObservedInputs::default())
        };

        let r = repair(&prev, &next, Some(&row_worktree("/a/wt1")), None);
        assert_eq!(r.cursor, Some(row_project("/a")));
        assert_eq!(r.follow, None, "a project header is not a workspace");
    }

    #[test]
    fn a_landing_hidden_by_the_filter_falls_through_to_the_climb() {
        let prev = reference_tree();
        // Session 22 is deleted, and in the same pass the filter hides everything
        // under /p1/wt2 that could have caught the slide.
        let next = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            let p1 = b.push(row_project("/p1"), Parent::Root, true);
            let wt2 = b.push(row_worktree("/p1/wt2"), Parent::Node(p1), false);
            b.push(SidebarRow::Session(21), Parent::Node(wt2), false);
            b.push(SidebarRow::Session(23), Parent::Node(wt2), false);
            b.finish(filtering())
        };

        let r = repair(&prev, &next, Some(&SidebarRow::Session(22)), None);
        assert_eq!(r.cursor, Some(row_project("/p1")));
        assert_eq!(r.follow, None, "the climb is a filter outcome, so nothing follows");
    }

    #[test]
    fn a_cursor_gone_with_nothing_to_land_on_takes_the_first_row() {
        let prev = reference_tree();
        let next = {
            let mut b = SnapshotBuilder::default();
            b.push(SidebarRow::Home, Parent::Root, true);
            b.finish(ObservedInputs::default())
        };

        let r = repair(&prev, &next, Some(&SidebarRow::Session(22)), None);
        assert_eq!(r.cursor, Some(SidebarRow::Home));
    }
}
