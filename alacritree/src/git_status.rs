//! What removing a worktree would discard, for the delete dialog.

use std::path::Path;

use alacritree_vcs::{ChangeKind, Status};
use git2::{Repository, Status as Flags, StatusOptions};

use crate::{jobs, wsl};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DirtyCounts {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

impl DirtyCounts {
    pub(crate) fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked > 0
    }

    /// Derive the delete modal's counts from a status the git panel already
    /// polled, so opening the dialog costs no repository walk.
    pub(crate) fn from_status(status: &Status) -> Self {
        let untracked = status.working.iter().filter(|c| c.kind == ChangeKind::Untracked).count();
        let staged = status.staged.as_ref().map_or(0, Vec::len);
        Self { staged, modified: status.working.len() - untracked, untracked }
    }
}

/// Cheap dirty check used by the delete modal when the git panel has never
/// polled this worktree: avoids the branch-diff work that a status does,
/// since we only need to know whether `git worktree remove` will refuse the
/// path. Takes `&jobs::Blocking` because it shells out — call it from a pool
/// job, never from the UI thread.
pub(crate) fn dirty_counts(path: &Path, blocking: &jobs::Blocking) -> DirtyCounts {
    match wsl::classify(path) {
        wsl::Location::Wsl { distro, linux_path } => {
            dirty_counts_wsl(&distro, &linux_path, blocking)
        },
        wsl::Location::Windows(_) => dirty_counts_git2(path),
    }
}

fn dirty_counts_git2(path: &Path) -> DirtyCounts {
    let Ok(repo) = Repository::open(path) else {
        return DirtyCounts::default();
    };
    let mut opts = StatusOptions::new();
    opts.include_untracked(true);
    opts.recurse_untracked_dirs(true);
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return DirtyCounts::default();
    };
    let mut counts = DirtyCounts::default();
    let staged_mask = Flags::INDEX_NEW
        | Flags::INDEX_MODIFIED
        | Flags::INDEX_DELETED
        | Flags::INDEX_RENAMED
        | Flags::INDEX_TYPECHANGE;
    let modified_mask =
        Flags::WT_MODIFIED | Flags::WT_DELETED | Flags::WT_RENAMED | Flags::WT_TYPECHANGE;
    for entry in statuses.iter() {
        let s = entry.status();
        if s.intersects(staged_mask) {
            counts.staged += 1;
        }
        if s.contains(Flags::WT_NEW) {
            counts.untracked += 1;
        } else if s.intersects(modified_mask) {
            counts.modified += 1;
        }
    }
    counts
}

/// Counts from one porcelain-v2 round trip, run on a pool worker so a warm
/// wsl.exe call (~400 ms) never stalls paint.
fn dirty_counts_wsl(distro: &str, linux_path: &str, blocking: &jobs::Blocking) -> DirtyCounts {
    let Ok(stdout) = wsl::run_batch(
        distro,
        r#"git -C "$1" status --porcelain=v2 -z 2>/dev/null"#,
        &[linux_path],
        blocking,
    ) else {
        return DirtyCounts::default();
    };
    let (staged, unstaged) = alacritree_git::parse_status_v2_z(&stdout);
    DirtyCounts {
        staged: staged.len(),
        modified: unstaged.iter().filter(|c| c.kind != ChangeKind::Untracked).count(),
        untracked: unstaged.iter().filter(|c| c.kind == ChangeKind::Untracked).count(),
    }
}

#[cfg(test)]
mod tests {
    use alacritree_vcs::{FileChange, Head};

    use super::*;

    #[test]
    fn dirty_counts_come_from_a_status_the_panel_already_has() {
        let status = Status {
            head: Head { name: Some("main".into()), ..Head::default() },
            staged: Some(vec![FileChange { path: "a".into(), kind: ChangeKind::Added }]),
            working: vec![
                FileChange { path: "b".into(), kind: ChangeKind::Modified },
                FileChange { path: "c".into(), kind: ChangeKind::Untracked },
                FileChange { path: "d".into(), kind: ChangeKind::Untracked },
            ],
            ..Status::default()
        };
        let counts = DirtyCounts::from_status(&status);
        assert_eq!(counts.staged, 1);
        assert_eq!(counts.modified, 1);
        assert_eq!(counts.untracked, 2);
        assert!(counts.is_dirty());
    }
}
