//! Working-tree status + a summary of changes vs the project's default branch.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use git2::{Delta, DiffOptions, Repository, Status, StatusOptions};

const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    pub fn glyph(&self) -> &'static str {
        match self {
            ChangeKind::Added => "A",
            ChangeKind::Modified => "M",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
            ChangeKind::Untracked => "?",
            ChangeKind::Conflicted => "!",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiffStat {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DirtyCounts {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

impl DirtyCounts {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked > 0
    }
}

/// Cheap dirty check used by the delete modal: avoids the branch-diff work
/// that `compute` does, since we only need to know whether `git worktree
/// remove` will refuse the path.
pub fn dirty_counts(path: &Path) -> DirtyCounts {
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
    let staged_mask = Status::INDEX_NEW
        | Status::INDEX_MODIFIED
        | Status::INDEX_DELETED
        | Status::INDEX_RENAMED
        | Status::INDEX_TYPECHANGE;
    let modified_mask =
        Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED | Status::WT_TYPECHANGE;
    for entry in statuses.iter() {
        let s = entry.status();
        if s.intersects(staged_mask) {
            counts.staged += 1;
        }
        if s.contains(Status::WT_NEW) {
            counts.untracked += 1;
        } else if s.intersects(modified_mask) {
            counts.modified += 1;
        }
    }
    counts
}

#[derive(Debug, Clone, Default)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub default_branch: Option<String>,
    pub default_branch_resolved: Option<String>,
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
    pub branch_diff: Vec<DiffStat>,
    pub error: Option<String>,
}

/// Background-refreshed cache.  `compute` walks the working tree and runs a
/// tree-to-tree diff against the default branch — on a large repo that can
/// take long enough to be felt as a stutter when done on the UI thread, so we
/// spawn the work on a helper thread and let `poll` adopt the result on a
/// later frame.  Callers always see the last known status immediately.
pub struct StatusCache {
    path: PathBuf,
    last: GitStatus,
    last_refreshed: Option<Instant>,
    last_hint: Option<String>,
    pending: Option<Pending>,
}

struct Pending {
    /// Hint the in-flight compute was started with, so we can tell whether
    /// the result that lands matches what the UI is currently asking for.
    hint: Option<String>,
    rx: Receiver<GitStatus>,
}

impl StatusCache {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last: GitStatus::default(),
            last_refreshed: None,
            last_hint: None,
            pending: None,
        }
    }

    /// Last branch we resolved, for callers that need it before triggering a
    /// new poll (e.g. the PR cache wants the branch name to query `gh`).
    pub fn current_branch(&self) -> Option<&str> {
        self.last.branch.as_deref()
    }

    /// Returns the most recent known status, kicking off a background refresh
    /// when stale or when the default-branch hint changed since the last
    /// completed compute.  Never blocks the caller.
    pub fn poll(&mut self, default_branch_hint: Option<&str>, ctx: &egui::Context) -> &GitStatus {
        // Drain any completed background result before deciding whether to
        // spawn another — a fresh answer shouldn't be ignored just because
        // the staleness timer also tripped.
        if let Some(pending) = &self.pending {
            if let Ok(status) = pending.rx.try_recv() {
                self.last = status;
                self.last_refreshed = Some(Instant::now());
                self.last_hint = pending.hint.clone();
                self.pending = None;
            }
        }

        let hint_changed = self.last_hint.as_deref() != default_branch_hint;
        let stale = self.last_refreshed.map_or(true, |when| when.elapsed() > REFRESH_INTERVAL);
        let needs_refresh = self.last_refreshed.is_none() || hint_changed || stale;

        if needs_refresh && self.pending.is_none() {
            self.pending = Some(spawn_compute(
                self.path.clone(),
                default_branch_hint.map(str::to_string),
                ctx.clone(),
            ));
        }

        &self.last
    }
}

fn spawn_compute(path: PathBuf, hint: Option<String>, ctx: egui::Context) -> Pending {
    let (tx, rx) = mpsc::channel();
    let worker_hint = hint.clone();
    thread::spawn(move || {
        let status = compute(&path, worker_hint.as_deref());
        let _ = tx.send(status);
        ctx.request_repaint();
    });
    Pending { hint, rx }
}

pub fn compute(path: &Path, default_branch_hint: Option<&str>) -> GitStatus {
    match compute_inner(path, default_branch_hint) {
        Ok(s) => s,
        Err(e) => GitStatus { error: Some(e.to_string()), ..Default::default() },
    }
}

fn compute_inner(path: &Path, default_branch_hint: Option<&str>) -> Result<GitStatus, git2::Error> {
    let repo = Repository::open(path)?;

    let branch = current_branch_name(&repo);
    let default_branch =
        default_branch_hint.map(|s| s.to_string()).or_else(|| detect_default_branch(&repo));

    let mut staged = Vec::new();
    let mut unstaged = Vec::new();

    let mut opts = StatusOptions::new();
    opts.include_untracked(true);
    opts.recurse_untracked_dirs(true);
    opts.renames_head_to_index(true);
    opts.renames_index_to_workdir(true);

    let statuses = repo.statuses(Some(&mut opts))?;
    for entry in statuses.iter() {
        let path_str = entry.path().unwrap_or("").to_string();
        let status = entry.status();
        if let Some(kind) = staged_kind(status) {
            staged.push(FileChange { path: path_str.clone(), kind });
        }
        if let Some(kind) = unstaged_kind(status) {
            unstaged.push(FileChange { path: path_str, kind });
        }
    }

    let (branch_diff, default_branch_resolved) = match default_branch.as_deref() {
        Some(name) => match diff_against_branch(&repo, name) {
            Ok((stats, resolved)) => (stats, Some(resolved)),
            Err(_) => (Vec::new(), None),
        },
        None => (Vec::new(), None),
    };

    Ok(GitStatus {
        branch,
        default_branch,
        default_branch_resolved,
        staged,
        unstaged,
        branch_diff,
        error: None,
    })
}

fn current_branch_name(repo: &Repository) -> Option<String> {
    let head = repo.head().ok()?;
    if head.is_branch() {
        head.shorthand().map(|s| s.to_string())
    } else {
        head.target().map(|oid| oid.to_string().chars().take(7).collect())
    }
}

/// Mirrors `projects::detect_default_branch` — see that function for the
/// rationale behind the ordering.
fn detect_default_branch(repo: &Repository) -> Option<String> {
    if let Ok(reference) = repo.find_reference("refs/remotes/origin/HEAD") {
        if let Some(target) = reference.symbolic_target() {
            if let Some(name) = target.strip_prefix("refs/remotes/origin/") {
                return Some(name.to_string());
            }
        }
    }
    for c in ["main", "master", "trunk", "develop"] {
        if repo.find_reference(&format!("refs/heads/{c}")).is_ok() {
            return Some(c.to_string());
        }
    }
    if let Ok(cfg) = repo.config() {
        if let Ok(name) = cfg.get_string("init.defaultBranch") {
            if !name.is_empty() && repo.find_reference(&format!("refs/heads/{name}")).is_ok() {
                return Some(name);
            }
        }
    }
    None
}

fn staged_kind(s: Status) -> Option<ChangeKind> {
    if s.is_conflicted() {
        return Some(ChangeKind::Conflicted);
    }
    if s.contains(Status::INDEX_NEW) {
        return Some(ChangeKind::Added);
    }
    if s.contains(Status::INDEX_DELETED) {
        return Some(ChangeKind::Deleted);
    }
    if s.contains(Status::INDEX_RENAMED) {
        return Some(ChangeKind::Renamed);
    }
    if s.intersects(Status::INDEX_MODIFIED | Status::INDEX_TYPECHANGE) {
        return Some(ChangeKind::Modified);
    }
    None
}

fn unstaged_kind(s: Status) -> Option<ChangeKind> {
    if s.contains(Status::WT_NEW) {
        return Some(ChangeKind::Untracked);
    }
    if s.contains(Status::WT_DELETED) {
        return Some(ChangeKind::Deleted);
    }
    if s.contains(Status::WT_RENAMED) {
        return Some(ChangeKind::Renamed);
    }
    if s.intersects(Status::WT_MODIFIED | Status::WT_TYPECHANGE) {
        return Some(ChangeKind::Modified);
    }
    None
}

/// Diff against the merge base, not the branch tip, so local-only commits
/// still appear when the default branch hasn't moved.
fn diff_against_branch(
    repo: &Repository,
    branch: &str,
) -> Result<(Vec<DiffStat>, String), git2::Error> {
    let (base_commit, resolved) = resolve_base_commit(repo, branch)?;
    let head_commit = repo.head()?.peel_to_commit()?;

    let merge_base_oid = repo.merge_base(base_commit.id(), head_commit.id())?;
    let merge_base_commit = repo.find_commit(merge_base_oid)?;

    let base_tree = merge_base_commit.tree()?;
    let head_tree = head_commit.tree()?;

    let mut opts = DiffOptions::new();
    opts.include_untracked(false)
        .recurse_untracked_dirs(false)
        // We only need +/- counts, never the surrounding code, so asking
        // libgit2 to emit zero context (and no inter-hunk padding) trims a
        // material amount of streaming work on diffs with many small hunks.
        .context_lines(0)
        .interhunk_lines(0);
    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut opts))?;

    // Single foreach pass: `file_cb` seeds a `DiffStat` per changed file and
    // `line_cb` bumps additions/deletions on the most-recently-seeded entry.
    // libgit2 calls `file_cb` once per file and then streams that file's
    // lines before moving on, so tracking "current index" is sufficient.
    //
    // This replaces a `Patch::from_diff(diff, i)` loop that, for every file,
    // re-fetched both blobs and re-ran the diff algorithm just so a
    // throw-away `line_stats()` could count +/- — easily the dominant cost
    // on branches with hundreds of changes.
    struct Accum {
        stats: Vec<DiffStat>,
        current: Option<usize>,
    }
    let accum = RefCell::new(Accum { stats: Vec::new(), current: None });

    diff.foreach(
        &mut |delta, _| {
            let mut a = accum.borrow_mut();
            if matches!(delta.status(), Delta::Unmodified | Delta::Ignored) {
                a.current = None;
                return true;
            }
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            a.current = Some(a.stats.len());
            a.stats.push(DiffStat { path, additions: 0, deletions: 0 });
            true
        },
        None,
        None,
        Some(&mut |_delta, _hunk, line| {
            let mut a = accum.borrow_mut();
            if let Some(idx) = a.current {
                match line.origin() {
                    '+' => a.stats[idx].additions += 1,
                    '-' => a.stats[idx].deletions += 1,
                    _ => {},
                }
            }
            true
        }),
    )?;

    Ok((accum.into_inner().stats, resolved))
}

fn resolve_base_commit<'a>(
    repo: &'a Repository,
    branch: &str,
) -> Result<(git2::Commit<'a>, String), git2::Error> {
    let candidates = [format!("refs/remotes/origin/{branch}"), format!("refs/heads/{branch}")];
    for refname in &candidates {
        if let Ok(reference) = repo.find_reference(refname) {
            if let Ok(commit) = reference.peel_to_commit() {
                return Ok((commit, refname.clone()));
            }
        }
    }
    Err(git2::Error::from_str(&format!("default branch '{branch}' not found")))
}
