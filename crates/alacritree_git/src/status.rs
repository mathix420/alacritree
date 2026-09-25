//! Working-tree status + a summary of changes vs the project's default branch.

use std::cell::RefCell;
use std::path::Path;

use alacritree_common::{jobs, wsl};
use alacritree_vcs::{ChangeKind, DiffStat, Dirty, FileChange, Head, Status, VcsError};
use git2::{Delta, DiffOptions, Repository, Status as Flags, StatusOptions};

use crate::default_branch::{self, Evidence, WellKnown};

pub(crate) fn status(
    path: &Path,
    default_branch_hint: Option<&str>,
    blocking: &jobs::Blocking,
) -> Result<Status, VcsError> {
    match wsl::classify(path) {
        wsl::Location::Wsl { distro, linux_path } => {
            compute_wsl(&distro, &linux_path, default_branch_hint, blocking)
        },
        wsl::Location::Windows(_) => compute_inner(path, default_branch_hint)
            .map_err(|e| VcsError::Backend { context: e.to_string(), source: Box::new(e) }),
    }
}

/// Cheap dirty check used by the delete modal when the git panel has never
/// polled this worktree: avoids the branch-diff work that a status does,
/// since we only need to know whether `git worktree remove` will refuse the
/// path. Takes `&jobs::Blocking` because it shells out — call it from a pool
/// job, never from the UI thread.
pub(crate) fn dirty(path: &Path, blocking: &jobs::Blocking) -> Result<Dirty, VcsError> {
    match wsl::classify(path) {
        wsl::Location::Wsl { distro, linux_path } => {
            dirty_counts_wsl(&distro, &linux_path, blocking)
        },
        wsl::Location::Windows(_) => dirty_counts_git2(path)
            .map_err(|e| VcsError::Backend { context: e.to_string(), source: Box::new(e) }),
    }
}

fn dirty_counts_git2(path: &Path) -> Result<Dirty, git2::Error> {
    let repo = Repository::open(path)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true);
    opts.recurse_untracked_dirs(true);
    let statuses = repo.statuses(Some(&mut opts))?;
    let mut counts = Dirty::default();
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
    Ok(counts)
}

/// Counts from one porcelain-v2 round trip, run on a pool worker so a warm
/// wsl.exe call (~400 ms) never stalls paint.
fn dirty_counts_wsl(
    distro: &str,
    linux_path: &str,
    blocking: &jobs::Blocking,
) -> Result<Dirty, VcsError> {
    let stdout = wsl::run_batch(
        distro,
        r#"git -C "$1" status --porcelain=v2 -z 2>/dev/null"#,
        &[linux_path],
        blocking,
    )
    .map_err(|e| VcsError::Unreachable(e.to_string()))?;
    let (staged, unstaged) = parse_status_v2_z(&stdout);
    Ok(Dirty {
        staged: staged.len(),
        modified: unstaged.iter().filter(|c| c.kind != ChangeKind::Untracked).count(),
        untracked: unstaged.iter().filter(|c| c.kind == ChangeKind::Untracked).count(),
    })
}

fn compute_inner(path: &Path, default_branch_hint: Option<&str>) -> Result<Status, git2::Error> {
    let repo = Repository::open(path)?;

    let head = current_head(&repo);
    let trunk = default_branch_hint.map(|s| s.to_string()).or_else(|| detect_default_branch(&repo));

    let mut staged = Vec::new();
    let mut working = Vec::new();

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
            working.push(FileChange { path: path_str, kind });
        }
    }

    let (base_diff, base) = match trunk.as_deref() {
        Some(name) => match diff_against_branch(&repo, name) {
            Ok((stats, resolved)) => (stats, Some(resolved)),
            Err(_) => (Vec::new(), None),
        },
        None => (Vec::new(), None),
    };

    Ok(Status { head, trunk, base, staged: Some(staged), working, base_diff })
}

/// The branch HEAD names, and the commit it points at either way, so a
/// detached checkout is told apart from a branch rather than showing as one.
fn current_head(repo: &Repository) -> Head {
    let Ok(head) = repo.head() else {
        return Head::default();
    };
    let revision = head.target().map(|oid| oid.to_string().chars().take(7).collect());
    let name = if head.is_branch() { head.shorthand().map(str::to_string) } else { None };
    Head { name, revision, distance: None }
}

/// What git2 can see about this repository's default branch, ranked by
/// [`default_branch::resolve`].
fn detect_default_branch(repo: &Repository) -> Option<String> {
    let has = |name: &str| repo.find_reference(&format!("refs/heads/{name}")).is_ok();

    let origin_head = repo
        .find_reference("refs/remotes/origin/HEAD")
        .ok()
        .and_then(|r| r.symbolic_target().map(str::to_string))
        .and_then(|t| t.strip_prefix("refs/remotes/origin/").map(str::to_string));

    let present: Vec<&str> =
        WellKnown::ALL.iter().map(|c| c.as_str()).filter(|name| has(name)).collect();

    let init_default = repo
        .config()
        .ok()
        .and_then(|cfg| cfg.get_string("init.defaultBranch").ok())
        .filter(|name| !name.is_empty() && has(name));

    default_branch::resolve(&Evidence {
        origin_head: origin_head.as_deref(),
        present,
        init_default: init_default.as_deref(),
        ..Evidence::default()
    })
}

fn staged_kind(s: Flags) -> Option<ChangeKind> {
    if s.is_conflicted() {
        return Some(ChangeKind::Conflicted);
    }
    if s.contains(Flags::INDEX_NEW) {
        return Some(ChangeKind::Added);
    }
    if s.contains(Flags::INDEX_DELETED) {
        return Some(ChangeKind::Deleted);
    }
    if s.contains(Flags::INDEX_RENAMED) {
        return Some(ChangeKind::Renamed);
    }
    if s.intersects(Flags::INDEX_MODIFIED | Flags::INDEX_TYPECHANGE) {
        return Some(ChangeKind::Modified);
    }
    None
}

fn unstaged_kind(s: Flags) -> Option<ChangeKind> {
    if s.contains(Flags::WT_NEW) {
        return Some(ChangeKind::Untracked);
    }
    if s.contains(Flags::WT_DELETED) {
        return Some(ChangeKind::Deleted);
    }
    if s.contains(Flags::WT_RENAMED) {
        return Some(ChangeKind::Renamed);
    }
    if s.intersects(Flags::WT_MODIFIED | Flags::WT_TYPECHANGE) {
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

/// Everything one refresh tick asks a WSL distro.  `$1` is the repository and
/// `$2` a recorded base branch, empty when there is none.
///
/// The default branch is picked inside the round trip rather than back in
/// Rust, because the sections after it diff against whatever it picked.
fn status_batch() -> wsl::Batch {
    wsl::Batch::new(r#"p="$1"; hint="$2""#)
        .section("branch", r#"git -C "$p" symbolic-ref --short HEAD 2>/dev/null"#)
        .section("status", r#"git -C "$p" status --porcelain=v2 -z 2>/dev/null"#)
        .section(
            "default_branch",
            format!("{}\nhint=\"$h\"\nprintf '%s' \"$hint\"", default_branch::shell_ranking()),
        )
        .section(
            "base_ref",
            r#"base=""
if [ -n "$hint" ]; then
  for ref in "refs/remotes/origin/$hint" "refs/heads/$hint"; do
    if git -C "$p" rev-parse --verify --quiet "$ref" >/dev/null 2>&1; then base="$ref"; break; fi
  done
fi
printf '%s' "$base""#,
        )
        .section(
            "numstat",
            r#"if [ -n "$base" ]; then git -C "$p" diff --numstat -z "$base...HEAD" 2>/dev/null; fi"#,
        )
        .section("revision", r#"git -C "$p" rev-parse --short=7 HEAD 2>/dev/null"#)
}

/// One wsl.exe round trip per refresh tick.  Runs on `spawn_compute`'s
/// worker thread, so the ~400 ms round trip never blocks paint.
fn compute_wsl(
    distro: &str,
    linux_path: &str,
    hint: Option<&str>,
    blocking: &jobs::Blocking,
) -> Result<Status, VcsError> {
    let run = |script: &str, args: &[&str]| wsl::run_batch(distro, script, args, blocking);
    status_from_batch(linux_path, hint, run)
}

/// The refresh tick once the round trip is somebody else's problem, so a test
/// can hand it recorded stdout instead of a live distro.
fn status_from_batch(
    linux_path: &str,
    hint: Option<&str>,
    run: impl Fn(&str, &[&str]) -> Result<Vec<u8>, wsl::BatchError>,
) -> Result<Status, VcsError> {
    let batch = status_batch();
    let stdout = run(&batch.script(), &[linux_path, hint.unwrap_or("")])
        .map_err(|e| VcsError::Unreachable(e.to_string()))?;
    let reply = batch.read(&stdout);

    let name = Some(reply.text("branch")).filter(|s| !s.is_empty());
    let revision = Some(reply.text("revision")).filter(|s| !s.is_empty());
    if name.is_none() && revision.is_none() {
        let context = format!("could not open repository at {linux_path}");
        return Err(VcsError::Backend { source: context.clone().into(), context });
    }
    let (staged, working) = parse_status_v2_z(reply.bytes("status"));
    let trunk = Some(reply.text("default_branch")).filter(|s| !s.is_empty());
    let base = Some(reply.text("base_ref")).filter(|s| !s.is_empty());
    let base_diff =
        if base.is_some() { parse_numstat_z(reply.bytes("numstat")) } else { Vec::new() };
    Ok(Status {
        head: Head { name, revision, distance: None },
        trunk,
        base,
        staged: Some(staged),
        working,
        base_diff,
    })
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
    use alacritree_vcs::VersionControl;

    use super::*;
    use crate::{GitBackend, GitConfig};

    /// What a distro sends back for a batch, section by section, so a WSL
    /// refresh can be tested without one.
    fn recorded(sections: &[&str]) -> impl Fn(&str, &[&str]) -> Result<Vec<u8>, wsl::BatchError> {
        let mut stdout = Vec::new();
        for (i, section) in sections.iter().enumerate() {
            if i > 0 {
                stdout.extend_from_slice(wsl::SECTION_SEP);
            }
            stdout.extend_from_slice(section.as_bytes());
        }
        move |_script, _args| Ok(stdout.clone())
    }

    #[test]
    fn a_wsl_refresh_reads_every_section_of_a_full_reply() {
        let status = status_from_batch(
            "/home/lev/proj",
            None,
            recorded(&[
                "feat-x",
                "1 .M N... 100644 100644 100644 aaa bbb src/lib.rs\0",
                "main",
                "refs/remotes/origin/main",
                "3\t1\tsrc/lib.rs\0",
                "abc1234",
            ]),
        )
        .unwrap();
        assert_eq!(status.head.name.as_deref(), Some("feat-x"));
        assert_eq!(status.head.revision.as_deref(), Some("abc1234"));
        assert_eq!(status.trunk.as_deref(), Some("main"));
        assert_eq!(status.base.as_deref(), Some("refs/remotes/origin/main"));
        assert_eq!(status.base_diff.len(), 1);
        assert_eq!(status.working.len(), 1);
    }

    #[test]
    fn a_blank_branch_section_means_the_repository_could_not_be_opened() {
        let err = status_from_batch("/home/lev/proj", None, recorded(&["", "", "", "", "", ""]))
            .unwrap_err();
        assert_eq!(err.to_string(), "could not open repository at /home/lev/proj");
    }

    #[test]
    fn a_batch_that_stopped_early_still_reports_the_sections_it_reached() {
        let status = status_from_batch("/home/lev/proj", None, recorded(&["feat-x"])).unwrap();
        assert_eq!(status.head.name.as_deref(), Some("feat-x"));
        assert!(status.trunk.is_none());
        assert!(status.base_diff.is_empty(), "nothing to diff against without a base ref");
    }

    #[test]
    fn no_base_ref_leaves_the_branch_diff_empty_whatever_numstat_says() {
        let status = status_from_batch(
            "/home/lev/proj",
            None,
            recorded(&["feat-x", "", "main", "", "3\t1\tsrc/lib.rs\0"]),
        )
        .unwrap();
        assert_eq!(status.trunk.as_deref(), Some("main"));
        assert!(status.base_diff.is_empty());
    }

    #[test]
    fn a_detached_wsl_checkout_names_no_branch_but_keeps_the_revision() {
        let status = status_from_batch(
            "/home/lev/proj",
            None,
            recorded(&["", "", "main", "", "", "abc1234"]),
        )
        .unwrap();
        assert_eq!(status.head.name, None);
        assert_eq!(status.head.label(), Some("abc1234"));
    }

    #[test]
    fn a_round_trip_that_never_landed_is_reported_as_the_error_it_was() {
        let err = status_from_batch("/home/lev/proj", None, |_: &str, _: &[&str]| {
            Err(wsl::BatchError::Refused { stderr: "no distro".into() })
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "no distro");
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
    #[test]
    fn a_detached_status_names_no_branch_but_keeps_the_revision() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::test_support::init_repo(dir.path());
        crate::test_support::detach(&repo);
        let backend = GitBackend::new(&GitConfig::default());
        let status =
            alacritree_common::jobs::on_this_thread(|b| backend.status(&repo, None, b)).unwrap();
        assert_eq!(status.head.name, None);
        assert_eq!(status.head.revision.as_deref().map(str::len), Some(7));
        assert_eq!(status.staged, Some(vec![]));
    }

    #[test]
    fn a_clean_checkout_is_not_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::test_support::init_repo(dir.path());
        let backend = GitBackend::new(&GitConfig::default());
        let dirty = alacritree_common::jobs::on_this_thread(|b| backend.dirty(&repo, b)).unwrap();
        assert!(!dirty.is_dirty());
    }

    #[test]
    fn a_folder_that_is_no_repository_keeps_git2s_full_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let backend = GitBackend::new(&GitConfig::default());
        let err = alacritree_common::jobs::on_this_thread(|b| backend.status(dir.path(), None, b))
            .unwrap_err();
        assert!(err.to_string().contains("; class="), "{err}");
    }
}
