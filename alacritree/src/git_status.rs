//! Working-tree status + a summary of changes vs the project's default branch.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use git2::{Delta, DiffOptions, Repository, Status, StatusOptions};

use crate::{jobs, wsl};

const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

/// Long enough that no healthy compute reaches it, short enough that a
/// frozen panel is recorded while the process that froze it is still alive.
const STALL_WARNING: Duration = Duration::from_secs(120);

/// What the panel shows for a compute whose worker unwound.  The panic itself
/// is logged from the pool; the row only needs to stop claiming knowledge it
/// does not have.
const WORKER_DIED: &str = "the background worker did not finish";

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

    /// What the glyph stands for, for readers who do not know porcelain.
    pub fn label(&self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
            ChangeKind::Untracked => "untracked",
            ChangeKind::Conflicted => "conflicted",
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

    /// Derive the delete modal's counts from a status the git panel already
    /// polled, so opening the dialog costs no repository walk.
    pub fn from_status(status: &GitStatus) -> Self {
        let untracked = status.unstaged.iter().filter(|c| c.kind == ChangeKind::Untracked).count();
        Self { staged: status.staged.len(), modified: status.unstaged.len() - untracked, untracked }
    }
}

/// Cheap dirty check used by the delete modal when the git panel has never
/// polled this worktree: avoids the branch-diff work that `compute` does,
/// since we only need to know whether `git worktree remove` will refuse the
/// path. Takes `&jobs::Blocking` because it shells out — call it from a pool
/// job, never from the UI thread.
pub fn dirty_counts(path: &Path, blocking: &jobs::Blocking) -> DirtyCounts {
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
    let (staged, unstaged) = parse_status_v2_z(&stdout);
    DirtyCounts {
        staged: staged.len(),
        modified: unstaged.iter().filter(|c| c.kind != ChangeKind::Untracked).count(),
        untracked: unstaged.iter().filter(|c| c.kind == ChangeKind::Untracked).count(),
    }
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
    job: jobs::Job<GitStatus>,
    /// When the compute was spawned, so a caller can tell a slow one from
    /// one that will never answer.
    started: Instant,
    /// Set once the stall warning has been logged, so a frozen panel
    /// repainting at monitor rate records the freeze once rather than on
    /// every frame.
    warned: bool,
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

    /// The most recent known status without triggering a refresh, for callers
    /// that need to re-derive rows between polls (e.g. re-filtering on a
    /// keystroke).
    pub fn last(&self) -> &GitStatus {
        &self.last
    }

    /// How long the in-flight compute has been running, or `None` when
    /// nothing is in flight.  A compute that never returns pins `pending`,
    /// and `poll` will not spawn another while it does, so the panel keeps
    /// rendering whatever it last held.
    pub fn stalled_for(&self) -> Option<Duration> {
        self.pending.as_ref().map(|pending| pending.started.elapsed())
    }

    /// Whether a compute has landed and actually knows the tree. A cache
    /// entry exists the moment the git panel first renders a workspace,
    /// before its first background compute finishes — `last()` answers
    /// `GitStatus::default()` (all-zero counts) until then, which callers
    /// must not read as "known clean". A compute that landed but failed
    /// (`error: Some(..)`, e.g. the repository could not be opened) is the
    /// same "don't know" case — as is a compute whose worker unwound, which
    /// is banked the same way — it still sets `last_refreshed` so `poll`
    /// doesn't retry every frame, but it answers `false` here too.
    pub fn has_status(&self) -> bool {
        self.last_refreshed.is_some() && self.last.error.is_none()
    }

    /// Returns the most recent known status, kicking off a background refresh
    /// when stale or when the default-branch hint changed since the last
    /// completed compute.  Never blocks the caller.
    pub fn poll(&mut self, default_branch_hint: Option<&str>, ctx: &egui::Context) -> &GitStatus {
        // Drain any completed background result before deciding whether to
        // spawn another — a fresh answer shouldn't be ignored just because
        // the staleness timer also tripped.
        if let Some(pending) = &self.pending {
            if let Some(status) = pending.job.poll() {
                self.last = status;
                self.last_refreshed = Some(Instant::now());
                self.last_hint = pending.hint.clone();
                self.pending = None;
            } else if pending.job.failed() {
                // A panicked compute reports no status, and merely forgetting
                // it leaves the cache looking never-refreshed: the next poll
                // starts another, and the pool wakes a frame at every job end,
                // so a compute that fails every time would respawn at frame
                // rate.  Bank it as the failure it is, on the clock a landed
                // error already uses, and the retry lands one interval later
                // like any other.
                self.last =
                    GitStatus { error: Some(WORKER_DIED.to_string()), ..Default::default() };
                self.last_refreshed = Some(Instant::now());
                self.last_hint = pending.hint.clone();
                self.pending = None;
            }
        }

        // Nothing healthy takes this long: the resident transport caps a
        // request and the fallback is a single wsl.exe round trip.  Past it
        // the panel is frozen on a stale answer rather than waiting on a
        // slow one, and that difference is invisible from outside.
        if let Some(stalled) = self.stalled_for() {
            if stalled > STALL_WARNING {
                if let Some(pending) = self.pending.as_mut() {
                    if !pending.warned {
                        pending.warned = true;
                        log::warn!(
                            "git status for {} has been computing for {:.0}s; the panel is \
                             showing a stale result",
                            self.path.display(),
                            stalled.as_secs_f64()
                        );
                    }
                }
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
    let worker_hint = hint.clone();
    let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
        let status = compute(&path, worker_hint.as_deref(), blocking);
        ctx.request_repaint();
        status
    });
    Pending { hint, job, started: Instant::now(), warned: false }
}

pub fn compute(
    path: &Path,
    default_branch_hint: Option<&str>,
    blocking: &jobs::Blocking,
) -> GitStatus {
    match wsl::classify(path) {
        wsl::Location::Wsl { distro, linux_path } => {
            compute_wsl(&distro, &linux_path, default_branch_hint, blocking)
        },
        wsl::Location::Windows(_) => match compute_inner(path, default_branch_hint) {
            Ok(s) => s,
            Err(e) => GitStatus { error: Some(e.to_string()), ..Default::default() },
        },
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

/// Sections: 0 current branch (short OID when detached), 1 porcelain-v2
/// status, 2 effective default branch (the hint, or detection replicating
/// `detect_default_branch`), 3 the resolved base ref (origin-first, like
/// `resolve_base_commit`), 4 numstat against the merge base (`...` = git's
/// merge-base triple-dot, preserving `diff_against_branch` semantics).
const STATUS_SCRIPT: &str = r#"
p="$1"; hint="$2"
sep() { printf '\n@@ALACRITREE@@\n'; }
git -C "$p" symbolic-ref --short HEAD 2>/dev/null || git -C "$p" rev-parse --short=7 HEAD 2>/dev/null
sep
git -C "$p" status --porcelain=v2 -z 2>/dev/null
sep
if [ -z "$hint" ]; then
  h=$(git -C "$p" symbolic-ref refs/remotes/origin/HEAD 2>/dev/null)
  h="${h#refs/remotes/origin/}"
  if [ -z "$h" ]; then
    for c in main master trunk develop; do
      if git -C "$p" rev-parse --verify --quiet "refs/heads/$c" >/dev/null 2>&1; then h="$c"; break; fi
    done
  fi
  if [ -z "$h" ]; then
    c=$(git -C "$p" config init.defaultBranch 2>/dev/null)
    if [ -n "$c" ] && git -C "$p" rev-parse --verify --quiet "refs/heads/$c" >/dev/null 2>&1; then h="$c"; fi
  fi
  hint="$h"
fi
printf '%s' "$hint"
sep
base=""
if [ -n "$hint" ]; then
  for ref in "refs/remotes/origin/$hint" "refs/heads/$hint"; do
    if git -C "$p" rev-parse --verify --quiet "$ref" >/dev/null 2>&1; then base="$ref"; break; fi
  done
fi
printf '%s' "$base"
sep
if [ -n "$base" ]; then git -C "$p" diff --numstat -z "$base...HEAD" 2>/dev/null; fi
"#;

/// One wsl.exe round trip per refresh tick.  Runs on `spawn_compute`'s
/// worker thread, so the ~400 ms round trip never blocks paint.
fn compute_wsl(
    distro: &str,
    linux_path: &str,
    hint: Option<&str>,
    blocking: &jobs::Blocking,
) -> GitStatus {
    let stdout =
        match wsl::run_batch(distro, STATUS_SCRIPT, &[linux_path, hint.unwrap_or("")], blocking) {
            Ok(s) => s,
            Err(e) => return GitStatus { error: Some(e), ..Default::default() },
        };
    let sections = wsl::split_sections(&stdout);
    let text = |i: usize| {
        sections.get(i).map(|s| String::from_utf8_lossy(s).trim().to_string()).unwrap_or_default()
    };

    let branch = Some(text(0)).filter(|s| !s.is_empty());
    if branch.is_none() {
        return GitStatus {
            error: Some(format!("could not open repository at {linux_path}")),
            ..Default::default()
        };
    }
    let (staged, unstaged) = parse_status_v2_z(sections.get(1).copied().unwrap_or_default());
    let default_branch = Some(text(2)).filter(|s| !s.is_empty());
    let default_branch_resolved = Some(text(3)).filter(|s| !s.is_empty());
    let branch_diff = if default_branch_resolved.is_some() {
        parse_numstat_z(sections.get(4).copied().unwrap_or_default())
    } else {
        Vec::new()
    };
    GitStatus {
        branch,
        default_branch,
        default_branch_resolved,
        staged,
        unstaged,
        branch_diff,
        error: None,
    }
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

/// Map porcelain-v2 `XY` state chars to the sidebar's kinds.  X is the
/// index-vs-HEAD (staged) side, Y the worktree-vs-index (unstaged) side;
/// `.` means unchanged on that side.  Mirrors `staged_kind`/`unstaged_kind`.
fn staged_kind_v2(x: char) -> Option<ChangeKind> {
    match x {
        'A' => Some(ChangeKind::Added),
        'D' => Some(ChangeKind::Deleted),
        'R' | 'C' => Some(ChangeKind::Renamed),
        'M' | 'T' => Some(ChangeKind::Modified),
        _ => None,
    }
}

fn unstaged_kind_v2(y: char) -> Option<ChangeKind> {
    match y {
        'D' => Some(ChangeKind::Deleted),
        'R' | 'C' => Some(ChangeKind::Renamed),
        'M' | 'T' | 'A' => Some(ChangeKind::Modified),
        _ => None,
    }
}

/// Parse `git status --porcelain=v2 -z` into the same (staged, unstaged)
/// split the git2 arm produces.  Records are NUL-terminated; rename records
/// (`2 …`) are followed by an extra NUL-separated token holding the rename
/// source, which the sidebar doesn't show.
fn parse_status_v2_z(bytes: &[u8]) -> (Vec<FileChange>, Vec<FileChange>) {
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut tokens = bytes.split(|&b| b == 0);
    while let Some(token) = tokens.next() {
        if token.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(token);
        let Some((kind, rest)) = line.split_once(' ') else { continue };
        match kind {
            // `1 XY sub mH mI mW hH hI path` — path is the 8th field and may
            // contain spaces, so bound the split.
            "1" => {
                let mut fields = rest.splitn(8, ' ');
                let xy = fields.next().unwrap_or("..");
                if let Some(path) = fields.nth(6) {
                    push_xy(xy, path.to_string(), &mut staged, &mut unstaged);
                }
            },
            // `2 XY sub mH mI mW hH hI Xscore path` + NUL + origPath.
            "2" => {
                let mut fields = rest.splitn(9, ' ');
                let xy = fields.next().unwrap_or("..");
                let path = fields.nth(7).map(str::to_string);
                let _orig = tokens.next();
                if let Some(path) = path {
                    push_xy(xy, path, &mut staged, &mut unstaged);
                }
            },
            // `u XY sub m1 m2 m3 mW h1 h2 h3 path` — conflicts land in the
            // staged list, matching the git2 arm.
            "u" => {
                if let Some(path) = rest.splitn(10, ' ').nth(9) {
                    staged
                        .push(FileChange { path: path.to_string(), kind: ChangeKind::Conflicted });
                }
            },
            "?" => {
                unstaged.push(FileChange { path: rest.to_string(), kind: ChangeKind::Untracked })
            },
            _ => {},
        }
    }
    (staged, unstaged)
}

fn push_xy(xy: &str, path: String, staged: &mut Vec<FileChange>, unstaged: &mut Vec<FileChange>) {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    if let Some(kind) = staged_kind_v2(x) {
        staged.push(FileChange { path: path.clone(), kind });
    }
    if let Some(kind) = unstaged_kind_v2(y) {
        unstaged.push(FileChange { path, kind });
    }
}

/// Parse `git diff --numstat -z`: `added TAB deleted TAB path NUL`, except
/// renames, where the path field is empty and `src NUL dst NUL` follow.
/// Binary files report `-` counts, mapped to 0 (matching the git2 arm,
/// which never sees text lines for them either).
fn parse_numstat_z(bytes: &[u8]) -> Vec<DiffStat> {
    let mut stats = Vec::new();
    let mut tokens = bytes.split(|&b| b == 0);
    while let Some(token) = tokens.next() {
        if token.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(token);
        let mut fields = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let additions = added.parse().unwrap_or(0);
        let deletions = deleted.parse().unwrap_or(0);
        let path = if path.is_empty() {
            let _src = tokens.next();
            match tokens.next() {
                Some(dst) => String::from_utf8_lossy(dst).into_owned(),
                None => continue,
            }
        } else {
            path.to_string()
        };
        stats.push(DiffStat { path, additions, deletions });
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_poll_reports_without_blocking_its_caller() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A bare `Repository::init` leaves HEAD unborn (no commit for it to
        // point at), and `compute` never reports a branch for that; give it
        // one so the background result has a branch to land.
        let repo = crate::test_util::init_repo(dir.path());
        drop(repo);

        let ctx = egui::Context::default();
        let mut cache = StatusCache::new(dir.path().to_path_buf());
        // The first poll has nothing banked and must return anyway.
        let started = Instant::now();
        let _ = cache.poll(None, &ctx);
        assert!(started.elapsed() < Duration::from_millis(50), "poll blocked its caller");

        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.last().branch.is_none() && Instant::now() < deadline {
            let _ = cache.poll(None, &ctx);
            std::thread::yield_now();
        }
        assert!(cache.last().branch.is_some(), "the background compute never landed");
    }

    /// A panicked compute must not wedge the cache: without clearing
    /// `pending` on `Job::failed`, `needs_refresh && self.pending.is_none()`
    /// would refuse every future refresh for this worktree.
    #[test]
    fn a_failed_compute_clears_pending_so_a_future_poll_is_not_blocked() {
        let mut cache = StatusCache::new(PathBuf::from("/doesnt/matter"));
        let job = jobs::pool()
            .spawn(jobs::Priority::Background, |_: &jobs::Blocking| -> GitStatus {
                panic!("boom")
            });
        cache.pending = Some(Pending { hint: None, job, started: Instant::now(), warned: false });

        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.pending.is_some() {
            let _ = cache.poll(None, &ctx);
            assert!(Instant::now() < deadline, "pending was never cleared after the job failed");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_compute_that_never_answers_is_reported_as_stalled() {
        let mut cache = StatusCache::new(PathBuf::from("/nonexistent"));
        assert_eq!(cache.stalled_for(), None, "nothing in flight yet");

        // A gated worker rather than one that parks forever: the test drops
        // the sender before returning, so the worker exits instead of
        // costing one of the pool's fixed slots for the rest of the process.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let job = jobs::pool().spawn(
            jobs::Priority::Background,
            move |_: &jobs::Blocking| -> GitStatus {
                let _ = release_rx.recv();
                GitStatus::default()
            },
        );

        // Backdated past STALL_WARNING rather than slept past it, so the
        // warn-once assertions below need no sleep of their own.
        let started = Instant::now()
            .checked_sub(STALL_WARNING + Duration::from_secs(1))
            .expect("the process has not been up for STALL_WARNING yet");
        cache.pending = Some(Pending { hint: None, job, started, warned: false });

        let stalled = cache.stalled_for().expect("a held compute is in flight");
        assert!(stalled > STALL_WARNING);

        let ctx = egui::Context::default();
        let pending_warned =
            |cache: &StatusCache| cache.pending.as_ref().expect("still in flight").warned;
        assert!(!pending_warned(&cache), "not warned before the first poll");
        let _ = cache.poll(None, &ctx);
        assert!(pending_warned(&cache), "a stall past STALL_WARNING must be logged");
        let _ = cache.poll(None, &ctx);
        assert!(pending_warned(&cache), "the warning must not repeat on every frame");

        let _ = release_tx.send(());
    }

    /// The regression this guards: a failure that leaves the cache looking
    /// never-refreshed is spawned again by the very next poll, and the pool
    /// wakes a frame at every job end — so a compute that panics every time
    /// would respawn at frame rate, burning a worker for as long as the panel
    /// is open.  A compute that fails must not be retried more often than one
    /// that succeeds.
    #[test]
    fn a_failed_compute_backs_off_as_far_as_a_successful_one() {
        let job = jobs::pool()
            .spawn(jobs::Priority::Background, |_: &jobs::Blocking| -> GitStatus {
                panic!("boom")
            });
        // Latch the failure before the cache sees it, so the poll below reads
        // a settled job rather than racing the worker.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.failed() {
            assert!(job.poll().is_none(), "a panicking job never reports a value");
            assert!(Instant::now() < deadline, "the failure was never observed");
            std::thread::yield_now();
        }

        let mut cache = StatusCache::new(PathBuf::from("/doesnt/matter"));
        cache.pending = Some(Pending { hint: None, job, started: Instant::now(), warned: false });
        let ctx = egui::Context::default();

        let _ = cache.poll(None, &ctx);
        assert!(cache.pending.is_none(), "the poll that banks a failure must not start another");
        assert!(!cache.has_status(), "a compute that failed knows nothing about the tree");
        let _ = cache.poll(None, &ctx);
        assert!(cache.pending.is_none(), "nor may the frames that follow it inside the interval");
    }

    /// The regression this guards: a cache entry exists from the moment the
    /// git panel first renders a workspace, before its first compute lands
    /// -- `has_status` must read `false` for that entry so a caller deciding
    /// whether to trust `last()` doesn't mistake "never checked" for "known
    /// clean" (an all-zero `GitStatus::default()`).
    #[test]
    fn has_status_is_false_until_a_compute_lands() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let repo = crate::test_util::init_repo(dir.path());
        drop(repo);

        let ctx = egui::Context::default();
        let mut cache = StatusCache::new(dir.path().to_path_buf());
        assert!(!cache.has_status(), "a fresh cache has never completed a compute");

        let deadline = Instant::now() + Duration::from_secs(10);
        while !cache.has_status() && Instant::now() < deadline {
            let _ = cache.poll(None, &ctx);
            std::thread::yield_now();
        }
        assert!(cache.has_status(), "the background compute never landed");
    }

    /// The regression this guards: a compute that lands but fails to open
    /// the repository still sets `last_refreshed` (so `poll` doesn't retry
    /// every frame), which must not let `has_status` read it as a known,
    /// clean tree -- a caller deciding whether to force a destructive action
    /// needs "don't know" to stay "don't know" through this path too.
    #[test]
    fn has_status_is_false_for_an_errored_compute() {
        // Not a git repository, so `compute` lands an error rather than a
        // status.
        let dir = tempfile::tempdir().expect("a temp dir");
        let ctx = egui::Context::default();
        let mut cache = StatusCache::new(dir.path().to_path_buf());

        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.last().error.is_none() && Instant::now() < deadline {
            let _ = cache.poll(None, &ctx);
            std::thread::yield_now();
        }
        assert!(cache.last().error.is_some(), "the background compute never landed an error");
        assert!(!cache.has_status(), "an errored compute must not read as a known status");
    }

    #[test]
    fn dirty_counts_come_from_a_status_the_panel_already_has() {
        let status = GitStatus {
            branch: Some("main".into()),
            default_branch: None,
            default_branch_resolved: None,
            staged: vec![FileChange { path: "a".into(), kind: ChangeKind::Added }],
            unstaged: vec![
                FileChange { path: "b".into(), kind: ChangeKind::Modified },
                FileChange { path: "c".into(), kind: ChangeKind::Untracked },
                FileChange { path: "d".into(), kind: ChangeKind::Untracked },
            ],
            branch_diff: Vec::new(),
            error: None,
        };
        let counts = DirtyCounts::from_status(&status);
        assert_eq!(counts.staged, 1);
        assert_eq!(counts.modified, 1);
        assert_eq!(counts.untracked, 2);
        assert!(counts.is_dirty());
    }

    #[test]
    fn parses_porcelain_v2_z() {
        let bytes = b"1 .M N... 100644 100644 100644 aaaa bbbb src/main.rs\0\
1 A. N... 000000 100644 100644 0000 1111 new.rs\0\
2 R. N... 100644 100644 100644 cccc dddd R100 renamed.rs\0old-name.rs\0\
u UU N... 100644 100644 100644 100644 e1 e2 e3 conflicted.rs\0\
? untracked with space.txt\0";
        let (staged, unstaged) = parse_status_v2_z(bytes);

        let staged_pairs: Vec<(&str, ChangeKind)> =
            staged.iter().map(|c| (c.path.as_str(), c.kind)).collect();
        assert_eq!(staged_pairs, vec![
            ("new.rs", ChangeKind::Added),
            ("renamed.rs", ChangeKind::Renamed),
            ("conflicted.rs", ChangeKind::Conflicted),
        ]);

        let unstaged_pairs: Vec<(&str, ChangeKind)> =
            unstaged.iter().map(|c| (c.path.as_str(), c.kind)).collect();
        assert_eq!(unstaged_pairs, vec![
            ("src/main.rs", ChangeKind::Modified),
            ("untracked with space.txt", ChangeKind::Untracked),
        ]);
    }

    #[test]
    fn parses_numstat_z() {
        // Ordinary, rename (empty path + src/dst tokens), binary (- counts).
        let bytes = b"3\t1\tsrc/lib.rs\0\
2\t0\t\0old.rs\0new.rs\0\
-\t-\tassets/icon.png\0";
        let stats = parse_numstat_z(bytes);
        assert_eq!(stats.len(), 3);
        assert_eq!(
            (stats[0].path.as_str(), stats[0].additions, stats[0].deletions),
            ("src/lib.rs", 3, 1)
        );
        assert_eq!(
            (stats[1].path.as_str(), stats[1].additions, stats[1].deletions),
            ("new.rs", 2, 0)
        );
        assert_eq!(
            (stats[2].path.as_str(), stats[2].additions, stats[2].deletions),
            ("assets/icon.png", 0, 0)
        );
    }
}
