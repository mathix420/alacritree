//! Pure cursor model for keyboard navigation of the projects sidebar.
//!
//! Rows are identified by stable keys (project root / worktree path), not
//! indices: the project list mutates underneath the cursor (git-status
//! refresh, worktree add/remove), and an index would silently retarget.

use std::path::{Path, PathBuf};

use crate::projects::{Project, Worktree};

/// A row the sidebar cursor can rest on, in render order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarRow {
    Home,
    /// Project header, keyed by the project root.
    Project(PathBuf),
    /// Worktree row, keyed by the worktree path.
    Worktree(PathBuf),
}

/// Every row the sidebar currently renders, in render order: Home first,
/// then each project's header followed by its worktrees when expanded.
pub fn visible_rows(projects: &[Project]) -> Vec<SidebarRow> {
    let mut rows = vec![SidebarRow::Home];
    for p in projects {
        rows.push(SidebarRow::Project(p.root.clone()));
        if p.expanded {
            rows.extend(p.worktrees.iter().map(|wt| SidebarRow::Worktree(wt.path.clone())));
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

/// The project header owning a worktree row — the standard tree-view
/// "Left jumps to parent" idiom.  `None` for Home and project cursors.
pub fn left_target(rows: &[SidebarRow], cursor: &SidebarRow) -> Option<SidebarRow> {
    if !matches!(cursor, SidebarRow::Worktree(_)) {
        return None;
    }
    let pos = rows.iter().position(|r| r == cursor)?;
    rows[..pos].iter().rev().find(|r| matches!(r, SidebarRow::Project(_))).cloned()
}

/// Where the cursor lands when the sidebar gains focus: the current
/// workspace's row, its project header when that project is collapsed,
/// Home otherwise.
pub fn seed(projects: &[Project], current_workspace: Option<&Path>) -> SidebarRow {
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
/// matches itself or keeps at least one visible worktree.
pub fn filtered_rows(projects: &[Project], preds: RowPredicates<'_>) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    if preds.home {
        rows.push(SidebarRow::Home);
    }
    for p in projects {
        let self_matches = (preds.project_self)(p);
        let visible_worktrees: Vec<SidebarRow> = p
            .worktrees
            .iter()
            .filter(|wt| (preds.worktree)(p, wt))
            .map(|wt| SidebarRow::Worktree(wt.path.clone()))
            .collect();
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
        }
    }

    #[test]
    fn visible_rows_lists_home_then_projects_in_render_order() {
        let projects =
            vec![project("/a", true, &["/a/wt1", "/a/wt2"]), project("/b", true, &["/b/wt1"])];
        assert_eq!(
            visible_rows(&projects),
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
            visible_rows(&projects),
            vec![SidebarRow::Home, SidebarRow::Project(PathBuf::from("/a")),]
        );
    }

    #[test]
    fn visible_rows_with_no_projects_is_just_home() {
        assert_eq!(visible_rows(&[]), vec![SidebarRow::Home]);
    }

    #[test]
    fn step_moves_and_clamps_at_both_ends() {
        let rows = visible_rows(&[project("/a", true, &["/a/wt1"])]);
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
        let rows = visible_rows(&[project("/a", true, &["/a/wt1"])]);
        let gone = SidebarRow::Worktree(PathBuf::from("/a/removed"));
        assert_eq!(step(&rows, &gone, 1), SidebarRow::Home);
    }

    #[test]
    fn left_target_is_the_owning_project_header() {
        let rows =
            vec![project("/a", true, &["/a/wt1"]), project("/b", true, &["/b/wt1", "/b/wt2"])];
        let rows = visible_rows(&rows);
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
            seed(&projects, Some(Path::new("/a/wt1"))),
            SidebarRow::Worktree(PathBuf::from("/a/wt1"))
        );
        // Collapsed project: its header stands in for the hidden row.
        assert_eq!(
            seed(&projects, Some(Path::new("/b/wt1"))),
            SidebarRow::Project(PathBuf::from("/b"))
        );
        // Home workspace and unknown paths both land on Home.
        assert_eq!(seed(&projects, None), SidebarRow::Home);
        assert_eq!(seed(&projects, Some(Path::new("/nowhere"))), SidebarRow::Home);
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
            filtered_rows(&projects, preds),
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
            filtered_rows(&projects, preds),
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
            filtered_rows(&projects, preds),
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
            filtered_rows(&projects, preds),
            vec![
                SidebarRow::Home,
                SidebarRow::Project(PathBuf::from("/a")),
                SidebarRow::Worktree(PathBuf::from("/a/wt1")),
            ]
        );
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
}
