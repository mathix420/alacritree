//! Pure cursor model for keyboard navigation of the projects sidebar.
//!
//! Rows are identified by stable keys (project root / worktree path), not
//! indices: the project list mutates underneath the cursor (git-status
//! refresh, worktree add/remove), and an index would silently retarget.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::app::WorkspaceKey;
use crate::projects::{Project, Worktree};
use crate::session::SessionId;

/// A row the sidebar cursor can rest on, in render order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarRow {
    Home,
    /// Project header, keyed by the project root.
    Project(PathBuf),
    /// Worktree row, keyed by the worktree path.
    Worktree(PathBuf),
    /// Session row, keyed by its stable session id.  Only present when its
    /// workspace lists sessions in the sidebar.
    Session(SessionId),
}

/// The session rows each workspace currently *displays*, keyed by workspace.
/// The caller owns the listing rule (threshold, config overrides); taking the
/// resolved listing keeps the cursor model unable to drift from the paint
/// pass.
pub type ListedSessions = HashMap<WorkspaceKey, Vec<SessionId>>;

fn push_session_rows(rows: &mut Vec<SidebarRow>, sessions: &ListedSessions, ws: &WorkspaceKey) {
    if let Some(ids) = sessions.get(ws) {
        rows.extend(ids.iter().copied().map(SidebarRow::Session));
    }
}

/// Every row the sidebar currently renders, in render order: Home first,
/// then each project's header followed by its worktrees when expanded, with
/// each workspace's listed session rows directly after its own row.
pub fn visible_rows(projects: &[Project], sessions: &ListedSessions) -> Vec<SidebarRow> {
    let mut rows = vec![SidebarRow::Home];
    push_session_rows(&mut rows, sessions, &None);
    for p in projects {
        rows.push(SidebarRow::Project(p.root.clone()));
        if p.expanded {
            for wt in &p.worktrees {
                rows.push(SidebarRow::Worktree(wt.path.clone()));
                push_session_rows(&mut rows, sessions, &Some(wt.path.clone()));
            }
        }
    }
    rows
}

/// The row `delta` steps away from `cursor`, clamped to the list ends.
/// A cursor no longer in `rows` (worktree removed, project collapsed) falls
/// back to Home rather than guessing a neighbor.
pub fn step(rows: &[SidebarRow], cursor: &SidebarRow, delta: i32) -> SidebarRow {
    let Some(pos) = rows.iter().position(|r| r == cursor) else {
        return SidebarRow::Home;
    };
    let last = rows.len() as i32 - 1;
    let new = (pos as i32 + delta).clamp(0, last) as usize;
    rows[new].clone()
}

/// The row owning `cursor` — the standard tree-view "Left jumps to parent"
/// idiom.  A worktree's parent is its project header; a session's parent is
/// the worktree (or Home) row it's listed under.  `None` for Home and
/// project cursors.
pub fn left_target(rows: &[SidebarRow], cursor: &SidebarRow) -> Option<SidebarRow> {
    let pos = rows.iter().position(|r| r == cursor)?;
    match cursor {
        SidebarRow::Worktree(_) => {
            rows[..pos].iter().rev().find(|r| matches!(r, SidebarRow::Project(_))).cloned()
        },
        SidebarRow::Session(_) => rows[..pos]
            .iter()
            .rev()
            .find(|r| matches!(r, SidebarRow::Worktree(_) | SidebarRow::Home))
            .cloned(),
        _ => None,
    }
}

/// The nearest project header strictly after `cursor` — the PageDown-style
/// project jump.  `None` when no header follows or the cursor has vanished
/// from `rows` (the caller reseats it, as `step` callers do).
pub fn next_project(rows: &[SidebarRow], cursor: &SidebarRow) -> Option<SidebarRow> {
    let pos = rows.iter().position(|r| r == cursor)?;
    rows[pos + 1..].iter().find(|r| matches!(r, SidebarRow::Project(_))).cloned()
}

/// The nearest project header strictly before `cursor`.
pub fn previous_project(rows: &[SidebarRow], cursor: &SidebarRow) -> Option<SidebarRow> {
    let pos = rows.iter().position(|r| r == cursor)?;
    rows[..pos].iter().rev().find(|r| matches!(r, SidebarRow::Project(_))).cloned()
}

/// Where the cursor lands when the sidebar gains focus: the active session's
/// row when it's currently listed in the sidebar, otherwise the current
/// workspace's row, its project header when that project is collapsed, or
/// Home.
pub fn seed(
    projects: &[Project],
    current_workspace: Option<&Path>,
    sessions: &ListedSessions,
    active: Option<SessionId>,
) -> SidebarRow {
    if let Some(id) = active {
        let ws: WorkspaceKey = current_workspace.map(Path::to_path_buf);
        // Session rows of a worktree only show while its project is expanded.
        let shown = match &ws {
            None => true,
            Some(path) => {
                projects.iter().any(|p| p.expanded && p.worktrees.iter().any(|wt| wt.path == *path))
            },
        };
        if shown && sessions.get(&ws).is_some_and(|ids| ids.contains(&id)) {
            return SidebarRow::Session(id);
        }
    }
    let Some(path) = current_workspace else {
        return SidebarRow::Home;
    };
    for p in projects {
        if p.worktrees.iter().any(|wt| wt.path == path) {
            return if p.expanded {
                SidebarRow::Worktree(path.to_path_buf())
            } else {
                SidebarRow::Project(p.root.clone())
            };
        }
    }
    SidebarRow::Home
}

/// The three predicates that decide whether a row survives an active filter.
/// `worktree` is `FnMut` because the fuzzy matcher it wraps needs `&mut self`.
pub struct RowPredicates<'a> {
    pub home: bool,
    pub project_self: &'a dyn Fn(&Project) -> bool,
    pub worktree: &'a mut dyn FnMut(&Project, &Worktree) -> bool,
}

/// Render-order rows under an active filter. Projects are force-expanded (a
/// filter that hides its own results is useless); a header survives when it
/// matches itself or keeps at least one visible worktree.  Surviving
/// workspace rows keep their listed session rows — the filter matches
/// workspaces, not sessions.
pub fn filtered_rows(
    projects: &[Project],
    sessions: &ListedSessions,
    preds: RowPredicates<'_>,
) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    if preds.home {
        rows.push(SidebarRow::Home);
        push_session_rows(&mut rows, sessions, &None);
    }
    for p in projects {
        let self_matches = (preds.project_self)(p);
        let mut visible_worktrees: Vec<SidebarRow> = Vec::new();
        for wt in p.worktrees.iter().filter(|wt| (preds.worktree)(p, wt)) {
            visible_worktrees.push(SidebarRow::Worktree(wt.path.clone()));
            push_session_rows(&mut visible_worktrees, sessions, &Some(wt.path.clone()));
        }
        if self_matches || !visible_worktrees.is_empty() {
            rows.push(SidebarRow::Project(p.root.clone()));
            rows.extend(visible_worktrees);
        }
    }
    rows
}

/// Cursor fallback: unchanged when still visible, else the first row.
pub fn ensure_cursor(rows: &[SidebarRow], cursor: Option<&SidebarRow>) -> Option<SidebarRow> {
    match cursor {
        Some(c) if rows.contains(c) => Some(c.clone()),
        _ => rows.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::Worktree;

    fn no_sessions() -> ListedSessions {
        HashMap::new()
    }

    pub(super) fn project(root: &str, expanded: bool, worktrees: &[&str]) -> Project {
        Project {
            root: PathBuf::from(root),
            name: root.to_string(),
            label: None,
            default_branch: None,
            worktrees: worktrees
                .iter()
                .map(|p| Worktree {
                    name: p.to_string(),
                    path: PathBuf::from(p),
                    branch: None,
                    is_main: false,
                    prunable: false,
                })
                .collect(),
            expanded,
            shell_override: None,
            home: None,
        }
    }

    #[test]
    fn visible_rows_lists_home_then_projects_in_render_order() {
        let projects =
            vec![project("/a", true, &["/a/wt1", "/a/wt2"]), project("/b", true, &["/b/wt1"])];
        assert_eq!(
            visible_rows(&projects, &no_sessions()),
            vec![
                SidebarRow::Home,
                SidebarRow::Project(PathBuf::from("/a")),
                SidebarRow::Worktree(PathBuf::from("/a/wt1")),
                SidebarRow::Worktree(PathBuf::from("/a/wt2")),
                SidebarRow::Project(PathBuf::from("/b")),
                SidebarRow::Worktree(PathBuf::from("/b/wt1")),
            ]
        );
    }

    #[test]
    fn visible_rows_hides_worktrees_of_collapsed_projects() {
        let projects = vec![project("/a", false, &["/a/wt1"])];
        assert_eq!(
            visible_rows(&projects, &no_sessions()),
            vec![SidebarRow::Home, SidebarRow::Project(PathBuf::from("/a")),]
        );
    }

    #[test]
    fn visible_rows_with_no_projects_is_just_home() {
        assert_eq!(visible_rows(&[], &no_sessions()), vec![SidebarRow::Home]);
    }

    #[test]
    fn step_moves_and_clamps_at_both_ends() {
        let rows = visible_rows(&[project("/a", true, &["/a/wt1"])], &no_sessions());
        // Home -> Project -> Worktree
        assert_eq!(step(&rows, &SidebarRow::Home, 1), SidebarRow::Project(PathBuf::from("/a")));
        assert_eq!(
            step(&rows, &SidebarRow::Project(PathBuf::from("/a")), 1),
            SidebarRow::Worktree(PathBuf::from("/a/wt1"))
        );
        // Clamp: no wrap in either direction.
        assert_eq!(step(&rows, &SidebarRow::Home, -1), SidebarRow::Home);
        assert_eq!(
            step(&rows, &SidebarRow::Worktree(PathBuf::from("/a/wt1")), 1),
            SidebarRow::Worktree(PathBuf::from("/a/wt1"))
        );
    }

    #[test]
    fn step_from_vanished_cursor_falls_back_to_home() {
        let rows = visible_rows(&[project("/a", true, &["/a/wt1"])], &no_sessions());
        let gone = SidebarRow::Worktree(PathBuf::from("/a/removed"));
        assert_eq!(step(&rows, &gone, 1), SidebarRow::Home);
    }

    #[test]
    fn left_target_is_the_owning_project_header() {
        let rows =
            vec![project("/a", true, &["/a/wt1"]), project("/b", true, &["/b/wt1", "/b/wt2"])];
        let rows = visible_rows(&rows, &no_sessions());
        assert_eq!(
            left_target(&rows, &SidebarRow::Worktree(PathBuf::from("/b/wt2"))),
            Some(SidebarRow::Project(PathBuf::from("/b")))
        );
        // Only worktree rows have a left-jump target.
        assert_eq!(left_target(&rows, &SidebarRow::Home), None);
        assert_eq!(left_target(&rows, &SidebarRow::Project(PathBuf::from("/a"))), None);
    }

    #[test]
    fn seed_lands_on_the_current_workspace_row() {
        use std::path::Path;
        let projects = vec![project("/a", true, &["/a/wt1"]), project("/b", false, &["/b/wt1"])];
        // Expanded project: the worktree row itself.
        assert_eq!(
            seed(&projects, Some(Path::new("/a/wt1")), &no_sessions(), None),
            SidebarRow::Worktree(PathBuf::from("/a/wt1"))
        );
        // Collapsed project: its header stands in for the hidden row.
        assert_eq!(
            seed(&projects, Some(Path::new("/b/wt1")), &no_sessions(), None),
            SidebarRow::Project(PathBuf::from("/b"))
        );
        // Home workspace and unknown paths both land on Home.
        assert_eq!(seed(&projects, None, &no_sessions(), None), SidebarRow::Home);
        assert_eq!(
            seed(&projects, Some(Path::new("/nowhere")), &no_sessions(), None),
            SidebarRow::Home
        );
    }

    #[test]
    fn filtered_rows_keeps_projects_with_matching_worktrees_and_forces_expansion() {
        let projects = vec![project("/a", false, &["/a/wt1", "/a/wt2"])];
        let preds = RowPredicates {
            home: true,
            project_self: &|_p| false,
            worktree: &mut |_p, wt| wt.path == PathBuf::from("/a/wt1"),
        };
        assert_eq!(
            filtered_rows(&projects, &no_sessions(), preds),
            vec![
                SidebarRow::Home,
                SidebarRow::Project(PathBuf::from("/a")),
                SidebarRow::Worktree(PathBuf::from("/a/wt1")),
            ]
        );
    }

    #[test]
    fn filtered_rows_keeps_a_self_matching_header_without_its_worktrees() {
        let projects = vec![project("/a", true, &["/a/wt1"])];
        let preds = RowPredicates {
            home: true,
            project_self: &|p| p.root == PathBuf::from("/a"),
            worktree: &mut |_p, _wt| false,
        };
        assert_eq!(
            filtered_rows(&projects, &no_sessions(), preds),
            vec![SidebarRow::Home, SidebarRow::Project(PathBuf::from("/a"))]
        );
    }

    #[test]
    fn filtered_rows_drops_home_when_it_fails_the_predicate() {
        let projects = vec![project("/a", true, &["/a/wt1"])];
        let preds = RowPredicates {
            home: false,
            project_self: &|p| p.root == PathBuf::from("/a"),
            worktree: &mut |_p, _wt| true,
        };
        assert_eq!(
            filtered_rows(&projects, &no_sessions(), preds),
            vec![
                SidebarRow::Project(PathBuf::from("/a")),
                SidebarRow::Worktree(PathBuf::from("/a/wt1"))
            ]
        );
    }

    #[test]
    fn filtered_rows_drops_projects_matching_neither_self_nor_worktrees() {
        let projects = vec![project("/a", true, &["/a/wt1"]), project("/b", true, &["/b/wt1"])];
        let preds = RowPredicates {
            home: true,
            project_self: &|p| p.root == PathBuf::from("/a"),
            worktree: &mut |p, _wt| p.root == PathBuf::from("/a"),
        };
        assert_eq!(
            filtered_rows(&projects, &no_sessions(), preds),
            vec![
                SidebarRow::Home,
                SidebarRow::Project(PathBuf::from("/a")),
                SidebarRow::Worktree(PathBuf::from("/a/wt1")),
            ]
        );
    }

    #[test]
    fn visible_rows_interleaves_session_rows_after_their_workspace_row() {
        let projects = vec![project("/a", true, &["/a/wt1"])];
        let sessions =
            HashMap::from([(None, vec![1, 2]), (Some(PathBuf::from("/a/wt1")), vec![3, 4])]);
        assert_eq!(
            visible_rows(&projects, &sessions),
            vec![
                SidebarRow::Home,
                SidebarRow::Session(1),
                SidebarRow::Session(2),
                SidebarRow::Project(PathBuf::from("/a")),
                SidebarRow::Worktree(PathBuf::from("/a/wt1")),
                SidebarRow::Session(3),
                SidebarRow::Session(4),
            ]
        );
    }

    #[test]
    fn visible_rows_hides_session_rows_of_collapsed_projects() {
        let projects = vec![project("/a", false, &["/a/wt1"])];
        let sessions = HashMap::from([(Some(PathBuf::from("/a/wt1")), vec![3, 4])]);
        assert_eq!(
            visible_rows(&projects, &sessions),
            vec![SidebarRow::Home, SidebarRow::Project(PathBuf::from("/a"))]
        );
    }

    #[test]
    fn left_target_of_session_is_its_owning_workspace_row() {
        let projects = vec![project("/a", true, &["/a/wt1"])];
        let sessions =
            HashMap::from([(None, vec![1, 2]), (Some(PathBuf::from("/a/wt1")), vec![3, 4])]);
        let rows = visible_rows(&projects, &sessions);
        assert_eq!(left_target(&rows, &SidebarRow::Session(2)), Some(SidebarRow::Home));
        assert_eq!(
            left_target(&rows, &SidebarRow::Session(4)),
            Some(SidebarRow::Worktree(PathBuf::from("/a/wt1")))
        );
    }

    #[test]
    fn seed_lands_on_the_active_session_row_when_listed() {
        let projects = vec![project("/a", true, &["/a/wt1"])];
        let sessions = HashMap::from([(Some(PathBuf::from("/a/wt1")), vec![3, 4])]);
        assert_eq!(
            seed(&projects, Some(Path::new("/a/wt1")), &sessions, Some(4)),
            SidebarRow::Session(4)
        );
        // An unlisted active id (single-session workspace) falls back to the
        // workspace row.
        assert_eq!(
            seed(&projects, Some(Path::new("/a/wt1")), &HashMap::new(), Some(4)),
            SidebarRow::Worktree(PathBuf::from("/a/wt1"))
        );
    }

    #[test]
    fn seed_ignores_the_active_session_of_a_collapsed_project() {
        let projects = vec![project("/a", false, &["/a/wt1"])];
        let sessions = HashMap::from([(Some(PathBuf::from("/a/wt1")), vec![3, 4])]);
        assert_eq!(
            seed(&projects, Some(Path::new("/a/wt1")), &sessions, Some(3)),
            SidebarRow::Project(PathBuf::from("/a"))
        );
    }

    #[test]
    fn seed_lands_on_home_session_rows_too() {
        let sessions = HashMap::from([(None, vec![1, 2])]);
        assert_eq!(seed(&[], None, &sessions, Some(2)), SidebarRow::Session(2));
    }

    #[test]
    fn filtered_rows_appends_session_rows_to_surviving_workspace_rows() {
        let projects = vec![project("/a", false, &["/a/wt1", "/a/wt2"])];
        let sessions = HashMap::from([
            (None, vec![1]),
            (Some(PathBuf::from("/a/wt1")), vec![3]),
            (Some(PathBuf::from("/a/wt2")), vec![9]),
        ]);
        let preds = RowPredicates {
            home: true,
            project_self: &|_p| false,
            worktree: &mut |_p, wt| wt.path == PathBuf::from("/a/wt1"),
        };
        assert_eq!(
            filtered_rows(&projects, &sessions, preds),
            vec![
                SidebarRow::Home,
                SidebarRow::Session(1),
                SidebarRow::Project(PathBuf::from("/a")),
                SidebarRow::Worktree(PathBuf::from("/a/wt1")),
                SidebarRow::Session(3),
            ]
        );
    }

    #[test]
    fn filtered_rows_hides_home_session_rows_with_home() {
        let projects = vec![project("/a", true, &["/a/wt1"])];
        let sessions = HashMap::from([(None, vec![1])]);
        let preds =
            RowPredicates { home: false, project_self: &|_| true, worktree: &mut |_, _| true };
        let rows = filtered_rows(&projects, &sessions, preds);
        assert!(!rows.contains(&SidebarRow::Session(1)));
    }

    #[test]
    fn ensure_cursor_keeps_a_visible_row_and_falls_back_to_first() {
        let rows = vec![SidebarRow::Home, SidebarRow::Project(PathBuf::from("/a"))];
        // Still visible: unchanged.
        assert_eq!(
            ensure_cursor(&rows, Some(&SidebarRow::Project(PathBuf::from("/a")))),
            Some(SidebarRow::Project(PathBuf::from("/a")))
        );
        // Vanished cursor and no cursor both fall back to the first row.
        let gone = SidebarRow::Worktree(PathBuf::from("/gone"));
        assert_eq!(ensure_cursor(&rows, Some(&gone)), Some(SidebarRow::Home));
        assert_eq!(ensure_cursor(&rows, None), Some(SidebarRow::Home));
        // Empty rows always fall back to None.
        assert_eq!(ensure_cursor(&[], Some(&SidebarRow::Home)), None);
        assert_eq!(ensure_cursor(&[], None), None);
    }

    #[test]
    fn next_project_jumps_to_the_nearest_header_below() {
        let projects = vec![project("/a", true, &["/a/wt1"]), project("/b", true, &["/b/wt1"])];
        let rows = visible_rows(&projects, &no_sessions());
        assert_eq!(
            next_project(&rows, &SidebarRow::Home),
            Some(SidebarRow::Project(PathBuf::from("/a")))
        );
        assert_eq!(
            next_project(&rows, &SidebarRow::Worktree(PathBuf::from("/a/wt1"))),
            Some(SidebarRow::Project(PathBuf::from("/b")))
        );
        // No header below the last project's subtree: stay put (None).
        assert_eq!(next_project(&rows, &SidebarRow::Worktree(PathBuf::from("/b/wt1"))), None);
    }

    #[test]
    fn previous_project_jumps_to_the_nearest_header_above() {
        let projects = vec![project("/a", true, &["/a/wt1"]), project("/b", true, &["/b/wt1"])];
        let rows = visible_rows(&projects, &no_sessions());
        assert_eq!(
            previous_project(&rows, &SidebarRow::Worktree(PathBuf::from("/b/wt1"))),
            Some(SidebarRow::Project(PathBuf::from("/b")))
        );
        assert_eq!(
            previous_project(&rows, &SidebarRow::Project(PathBuf::from("/b"))),
            Some(SidebarRow::Project(PathBuf::from("/a")))
        );
        // Nothing above the first header or on Home: None.
        assert_eq!(previous_project(&rows, &SidebarRow::Project(PathBuf::from("/a"))), None);
        assert_eq!(previous_project(&rows, &SidebarRow::Home), None);
    }

    #[test]
    fn project_jumps_from_session_rows_and_vanished_cursors() {
        let projects = vec![project("/a", true, &["/a/wt1"]), project("/b", true, &["/b/wt1"])];
        let sessions = HashMap::from([(Some(PathBuf::from("/a/wt1")), vec![7])]);
        let rows = visible_rows(&projects, &sessions);
        assert_eq!(
            next_project(&rows, &SidebarRow::Session(7)),
            Some(SidebarRow::Project(PathBuf::from("/b")))
        );
        assert_eq!(
            previous_project(&rows, &SidebarRow::Session(7)),
            Some(SidebarRow::Project(PathBuf::from("/a")))
        );
        // A cursor no longer in the rows has no anchor: None, caller reseats.
        let gone = SidebarRow::Worktree(PathBuf::from("/gone"));
        assert_eq!(next_project(&rows, &gone), None);
        assert_eq!(previous_project(&rows, &gone), None);
    }
}
