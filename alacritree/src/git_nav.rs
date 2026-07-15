//! Pure row model and cursor for the git-status sidebar.
//!
//! Rows are identified by `(section, path)`, not indices: the status lists
//! refresh underneath the cursor every 1.5 s, and a file's `ChangeKind` can
//! change between refreshes (staged -> modified, say) without the row
//! changing identity. An index would silently retarget the cursor onto
//! whatever file happens to land at that position next.

use crate::git_status::{ChangeKind, DiffStat, FileChange};

/// Which list of the git panel a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSection {
    Staged,
    Unstaged,
    Branch,
}

/// A row the git-panel cursor can rest on, in render order.
#[derive(Debug, Clone)]
pub struct GitRow {
    pub section: GitSection,
    pub path: String,
    /// `None` for branch-diff rows: `DiffStat` carries no `ChangeKind`.
    pub kind: Option<ChangeKind>,
}

impl PartialEq for GitRow {
    fn eq(&self, other: &Self) -> bool {
        self.section == other.section && self.path == other.path
    }
}

/// Visible/total counts for one section, for the panel's header labels.
pub struct SectionCount {
    pub visible: usize,
    pub total: usize,
}

pub struct GitRows {
    pub rows: Vec<GitRow>,
    pub staged: SectionCount,
    pub unstaged: SectionCount,
    pub branch: SectionCount,
}

/// Filters one file-change list into rows, in list order.
///
/// Conflicted files bypass `kind_pass`: a merge conflict must stay visible
/// no matter which kind toggles are active, since hiding it is how you lose
/// track of unresolved work.
fn push_change_rows(
    changes: &[FileChange],
    section: GitSection,
    kind_pass: &dyn Fn(ChangeKind) -> bool,
    query_pass: &mut dyn FnMut(&str) -> bool,
    rows: &mut Vec<GitRow>,
) -> SectionCount {
    let mut visible = 0;
    for change in changes {
        let kind_ok = change.kind == ChangeKind::Conflicted || kind_pass(change.kind);
        if kind_ok && query_pass(&change.path) {
            rows.push(GitRow { section, path: change.path.clone(), kind: Some(change.kind) });
            visible += 1;
        }
    }
    SectionCount { visible, total: changes.len() }
}

/// Render-order rows under the active kind/query filters: Staged, then
/// Unstaged, then Branch. Branch rows have no `ChangeKind` to test against
/// `kind_pass`, so only `query_pass` applies to them.
pub fn visible_rows(
    staged: &[FileChange],
    unstaged: &[FileChange],
    branch: &[DiffStat],
    kind_pass: &dyn Fn(ChangeKind) -> bool,
    query_pass: &mut dyn FnMut(&str) -> bool,
) -> GitRows {
    let mut rows = Vec::new();
    let staged_count =
        push_change_rows(staged, GitSection::Staged, kind_pass, query_pass, &mut rows);
    let unstaged_count =
        push_change_rows(unstaged, GitSection::Unstaged, kind_pass, query_pass, &mut rows);

    let mut branch_visible = 0;
    for stat in branch {
        if query_pass(&stat.path) {
            rows.push(GitRow { section: GitSection::Branch, path: stat.path.clone(), kind: None });
            branch_visible += 1;
        }
    }

    GitRows {
        rows,
        staged: staged_count,
        unstaged: unstaged_count,
        branch: SectionCount { visible: branch_visible, total: branch.len() },
    }
}

/// The row `delta` steps away from `cursor`, clamped to the list ends.
/// `None` only when `rows` is empty.
pub fn step(rows: &[GitRow], cursor: &GitRow, delta: i32) -> Option<GitRow> {
    if rows.is_empty() {
        return None;
    }
    let pos = rows.iter().position(|r| r == cursor).unwrap_or(0);
    let last = rows.len() as i32 - 1;
    let new = (pos as i32 + delta).clamp(0, last) as usize;
    Some(rows[new].clone())
}

/// Cursor fallback: unchanged when still visible, else the first row, else
/// `None` when the panel has nothing to show.
pub fn ensure_cursor(rows: &[GitRow], cursor: Option<&GitRow>) -> Option<GitRow> {
    match cursor {
        Some(c) if rows.contains(c) => Some(c.clone()),
        _ => rows.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, kind: ChangeKind) -> FileChange {
        FileChange { path: path.to_string(), kind }
    }

    fn stat(path: &str) -> DiffStat {
        DiffStat { path: path.to_string(), additions: 1, deletions: 0 }
    }

    fn row(section: GitSection, path: &str, kind: Option<ChangeKind>) -> GitRow {
        GitRow { section, path: path.to_string(), kind }
    }

    const ALL: &dyn Fn(ChangeKind) -> bool = &|_| true;
    const NONE: &dyn Fn(ChangeKind) -> bool = &|_| false;

    #[test]
    fn rows_preserve_section_order_and_counts() {
        let staged = vec![change("a.rs", ChangeKind::Added)];
        let unstaged = vec![change("b.rs", ChangeKind::Modified)];
        let branch = vec![stat("c.rs")];
        let mut query_pass = |_: &str| true;

        let result = visible_rows(&staged, &unstaged, &branch, ALL, &mut query_pass);

        assert_eq!(
            result.rows,
            vec![
                row(GitSection::Staged, "a.rs", Some(ChangeKind::Added)),
                row(GitSection::Unstaged, "b.rs", Some(ChangeKind::Modified)),
                row(GitSection::Branch, "c.rs", None),
            ]
        );
        assert_eq!((result.staged.visible, result.staged.total), (1, 1));
        assert_eq!((result.unstaged.visible, result.unstaged.total), (1, 1));
        assert_eq!((result.branch.visible, result.branch.total), (1, 1));
    }

    #[test]
    fn kind_filter_applies_to_staged_and_unstaged_but_not_branch() {
        let staged = vec![change("a.rs", ChangeKind::Added)];
        let unstaged = vec![change("b.rs", ChangeKind::Modified)];
        let branch = vec![stat("c.rs")];
        let mut query_pass = |_: &str| true;

        let result = visible_rows(&staged, &unstaged, &branch, NONE, &mut query_pass);

        assert_eq!(result.rows, vec![row(GitSection::Branch, "c.rs", None)]);
        assert_eq!((result.staged.visible, result.staged.total), (0, 1));
        assert_eq!((result.unstaged.visible, result.unstaged.total), (0, 1));
        assert_eq!((result.branch.visible, result.branch.total), (1, 1));
    }

    #[test]
    fn conflicted_rows_bypass_the_kind_filter() {
        let staged = vec![change("conflict.rs", ChangeKind::Conflicted)];
        let unstaged: Vec<FileChange> = Vec::new();
        let branch: Vec<DiffStat> = Vec::new();
        let mut query_pass = |_: &str| true;

        let result = visible_rows(&staged, &unstaged, &branch, NONE, &mut query_pass);

        assert_eq!(
            result.rows,
            vec![row(GitSection::Staged, "conflict.rs", Some(ChangeKind::Conflicted))]
        );
        assert_eq!((result.staged.visible, result.staged.total), (1, 1));
    }

    #[test]
    fn query_filters_all_sections() {
        let staged =
            vec![change("keep.rs", ChangeKind::Added), change("drop.rs", ChangeKind::Added)];
        let unstaged =
            vec![change("keep.rs", ChangeKind::Modified), change("drop.rs", ChangeKind::Modified)];
        let branch = vec![stat("keep.rs"), stat("drop.rs")];
        let mut query_pass = |path: &str| path.starts_with("keep");

        let result = visible_rows(&staged, &unstaged, &branch, ALL, &mut query_pass);

        assert_eq!(
            result.rows,
            vec![
                row(GitSection::Staged, "keep.rs", Some(ChangeKind::Added)),
                row(GitSection::Unstaged, "keep.rs", Some(ChangeKind::Modified)),
                row(GitSection::Branch, "keep.rs", None),
            ]
        );
        assert_eq!((result.staged.visible, result.staged.total), (1, 2));
        assert_eq!((result.unstaged.visible, result.unstaged.total), (1, 2));
        assert_eq!((result.branch.visible, result.branch.total), (1, 2));
    }

    #[test]
    fn step_clamps_and_ensure_cursor_falls_back_to_first() {
        let rows = vec![
            row(GitSection::Staged, "a.rs", Some(ChangeKind::Added)),
            row(GitSection::Unstaged, "b.rs", Some(ChangeKind::Modified)),
        ];

        assert_eq!(step(&rows, &rows[0], 1), Some(rows[1].clone()));
        // Clamp: no wrap past either end.
        assert_eq!(step(&rows, &rows[1], 1), Some(rows[1].clone()));
        assert_eq!(step(&rows, &rows[0], -1), Some(rows[0].clone()));
        // Empty rows: None regardless of cursor.
        assert_eq!(step(&[], &rows[0], 1), None);

        // Still visible: unchanged.
        assert_eq!(ensure_cursor(&rows, Some(&rows[1])), Some(rows[1].clone()));
        // Vanished cursor and no cursor both fall back to the first row.
        let gone = row(GitSection::Branch, "gone.rs", None);
        assert_eq!(ensure_cursor(&rows, Some(&gone)), Some(rows[0].clone()));
        assert_eq!(ensure_cursor(&rows, None), Some(rows[0].clone()));
        // Empty rows always fall back to None.
        assert_eq!(ensure_cursor(&[], Some(&rows[0])), None);
        assert_eq!(ensure_cursor(&[], None), None);
    }

    #[test]
    fn cursor_identity_ignores_kind_changes() {
        let staged_kind = row(GitSection::Staged, "a.rs", Some(ChangeKind::Added));
        let modified_kind = row(GitSection::Staged, "a.rs", Some(ChangeKind::Modified));
        assert_eq!(staged_kind, modified_kind);

        let different_section = row(GitSection::Unstaged, "a.rs", Some(ChangeKind::Added));
        assert_ne!(staged_kind, different_section);

        let different_path = row(GitSection::Staged, "b.rs", Some(ChangeKind::Added));
        assert_ne!(staged_kind, different_path);
    }
}
