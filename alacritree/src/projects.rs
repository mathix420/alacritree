//! Enumerate sidebar-added directories and their git worktrees.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use git2::Repository;
use serde_json::{Value, json};

use crate::{jobs, wsl};

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    /// Derived from the root's directory name; never stored.
    pub name: String,
    /// User-set display label, shown instead of `name` when present.  Like
    /// `expanded` and `shell_override`, this is user state: discovery never
    /// sets it, and refreshes must not lose it.
    pub label: Option<String>,
    pub default_branch: Option<String>,
    pub worktrees: Vec<Worktree>,
    pub expanded: bool,
    pub shell_override: Option<crate::wsl::ShellChoice>,
    /// The distro's own `$HOME` for a WSL project, so a path can collapse to
    /// `~` without guessing the prefix from the path itself.  `None` for a
    /// native project, whose home comes from `home::home_dir()`.
    pub home: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_main: bool,
    /// The checkout directory is gone but git's worktree metadata remains
    /// (`git worktree list` still shows it as prunable). Such a row cannot
    /// host a shell and only offers cleanup.
    pub prunable: bool,
    /// Upstream state for `branch`, when the feature is enabled and the
    /// backend could answer.
    pub upstream: Option<crate::upstream::UpstreamState>,
}

/// A discovery result and whether it can be trusted to replace an existing
/// worktree list.  A backend that could not be reached returns a placeholder
/// standing in for an unknown tree, which must never overwrite what the
/// caller already knows.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub project: Project,
    pub authoritative: bool,
}

impl Discovered {
    fn found(project: Project) -> Self {
        Self { project, authoritative: true }
    }

    fn unavailable(project: Project) -> Self {
        Self { project, authoritative: false }
    }
}

/// What an in-distro discovery round trip established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WslAnswer {
    Repo,
    NotARepo,
    /// The distro could not be reached, or answered malformed — the tree is
    /// unknown rather than empty.
    Unreachable,
}

/// Decide what a discovery round trip established, kept separate from running
/// it so all three outcomes are reachable without a WSL host.
fn classify_wsl_answer(reached: bool, is_repo: bool, worktrees_parsed: usize) -> WslAnswer {
    if !reached {
        WslAnswer::Unreachable
    } else if !is_repo {
        WslAnswer::NotARepo
    } else if worktrees_parsed == 0 {
        // A repository always has at least its main checkout.
        WslAnswer::Unreachable
    } else {
        WslAnswer::Repo
    }
}

impl Project {
    /// Classify the root and discover through the owning backend: in-distro
    /// git for WSL paths, git2 for Windows paths, and a pseudo-worktree
    /// placeholder when the root is not a repository.
    pub fn discover(root: PathBuf, upstream: bool, blocking: &jobs::Blocking) -> Discovered {
        let name = display_name(&root);
        match wsl::classify(&root) {
            wsl::Location::Wsl { distro, linux_path } => {
                Self::discover_wsl(root, name, &distro, &linux_path, upstream, blocking)
            },
            // A directory that is not a repository is a fact, not a failure.
            wsl::Location::Windows(_) => match Repository::open(&root) {
                Ok(repo) => Discovered::found(Self::from_repo(root, name, &repo, upstream)),
                Err(_) => Discovered::found(Self::placeholder(root)),
            },
        }
    }

    /// Pseudo-worktree entry: what non-git roots get permanently, and what a
    /// WSL project shows until background discovery fills in worktrees.
    pub fn placeholder(root: PathBuf) -> Self {
        let name = display_name(&root);
        Project {
            worktrees: vec![Worktree {
                name: name.clone(),
                path: root.clone(),
                branch: None,
                is_main: true,
                prunable: false,
                upstream: None,
            }],
            root,
            name,
            label: None,
            default_branch: None,
            expanded: true,
            shell_override: None,
            home: None,
        }
    }

    /// One wsl.exe round trip answers everything discovery needs; sections
    /// are split on `wsl::SECTION_SEP`.  A round trip that never landed yields
    /// the same pseudo-worktree a non-git folder gets, but marked
    /// non-authoritative so it cannot overwrite a known worktree list.
    fn discover_wsl(
        root: PathBuf,
        name: String,
        distro: &str,
        linux_path: &str,
        upstream: bool,
        blocking: &jobs::Blocking,
    ) -> Discovered {
        let upstream_arg = if upstream { "1" } else { "0" };
        let batch = wsl::run_batch(distro, DISCOVER_SCRIPT, &[linux_path, upstream_arg], blocking)
            .map_err(|e| {
                log::warn!("WSL discovery failed for {}: {e}", root.display());
            });
        let sections = batch.as_ref().map(|s| wsl::split_sections(s)).unwrap_or_default();
        let text = |i: usize| {
            sections
                .get(i)
                .map(|s| String::from_utf8_lossy(s).trim().to_string())
                .unwrap_or_default()
        };

        let records = parse_worktree_list_z(sections.get(1).copied().unwrap_or_default());
        let upstreams =
            crate::upstream::parse_for_each_ref(sections.get(6).copied().unwrap_or_default());
        let worktrees: Vec<Worktree> = records
            .iter()
            .enumerate()
            .map(|(i, rec)| {
                let path = wsl::linux_to_windows(&rec.path, distro);
                // Same rendering as the git2 arm: branch name, or the short
                // OID when detached.
                let branch = rec
                    .branch
                    .clone()
                    .or_else(|| rec.head.as_ref().map(|h| h.chars().take(7).collect()));
                let wt_name = if i == 0 { "main".to_string() } else { display_name(&path) };
                let prunable = i != 0 && crate::worktree_liveness::is_gone(&path);
                let upstream = rec.branch.as_deref().and_then(|b| upstreams.get(b).cloned());
                Worktree { name: wt_name, path, branch, is_main: i == 0, prunable, upstream }
            })
            .collect();

        match classify_wsl_answer(batch.is_ok(), text(0) == "yes", worktrees.len()) {
            WslAnswer::Unreachable => Discovered::unavailable(Self::placeholder(root)),
            WslAnswer::NotARepo => Discovered::found(Self::placeholder(root)),
            WslAnswer::Repo => Discovered::found(Project {
                default_branch: default_branch_from_batch(&text(2), &text(3), &text(4)),
                worktrees,
                root,
                name,
                label: None,
                expanded: true,
                shell_override: None,
                home: Some(text(5)).filter(|h| !h.is_empty()),
            }),
        }
    }

    fn from_repo(root: PathBuf, name: String, repo: &Repository, upstream: bool) -> Self {
        let main_path = repo.workdir().map(|p| p.to_path_buf()).unwrap_or_else(|| root.clone());

        let upstreams =
            if upstream { crate::upstream::map_from_repo(repo) } else { HashMap::new() };
        let lookup = |branch: &Option<String>, detached: bool| {
            if detached { None } else { branch.as_deref().and_then(|b| upstreams.get(b).cloned()) }
        };

        let mut worktrees = Vec::new();
        let (branch, detached) = current_branch(repo);
        let upstream = lookup(&branch, detached);
        worktrees.push(Worktree {
            name: "main".to_string(),
            path: main_path.clone(),
            branch,
            is_main: true,
            prunable: false,
            upstream,
        });

        if let Ok(names) = repo.worktrees() {
            for name in names.iter().flatten() {
                if let Ok(wt) = repo.find_worktree(name) {
                    let path = wt.path().to_path_buf();
                    let (branch, detached) = match Repository::open(&path)
                        .ok()
                        .map(|wt_repo| current_branch(&wt_repo))
                    {
                        Some((Some(branch), detached)) => (Some(branch), detached),
                        _ => (branch_from_admin_head(repo, name), false),
                    };
                    let upstream = lookup(&branch, detached);
                    worktrees.push(Worktree {
                        name: name.to_string(),
                        // The checkout's own `.git`, not git2's `is_prunable`: a
                        // *locked* worktree with a missing checkout is not
                        // git-prunable but still cannot host a shell, and a
                        // half-finished remove leaves the directory behind
                        // without it.
                        prunable: crate::worktree_liveness::is_gone(&path),
                        path,
                        branch,
                        is_main: false,
                        upstream,
                    });
                }
            }
        }

        Project {
            default_branch: detect_default_branch(repo),
            worktrees,
            root,
            name,
            label: None,
            expanded: true,
            shell_override: None,
            home: None,
        }
    }

    /// The sidebar name: the user's label when set, the directory name
    /// otherwise.
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// Adopt a discovery result.  A non-authoritative result leaves the
    /// discovered fields alone: an unreachable backend must not read as
    /// deletion.  `expanded`, `shell_override`, and `label` are user state and
    /// are never touched either way.  One list, so a field cannot be adopted by
    /// the synchronous refresh and dropped by the background one.
    /// `occupied` names the worktrees that still host a live session.  Git
    /// forgets a worktree the moment it is pruned, but its shells keep
    /// running, and a row is the only way to reach them — so a dropped
    /// worktree with sessions is kept as prunable rather than removed.
    pub fn apply(&mut self, found: Discovered, occupied: &HashSet<PathBuf>) {
        if !found.authoritative {
            return;
        }
        let mut worktrees = found.project.worktrees;
        let stranded: Vec<Worktree> = self
            .worktrees
            .drain(..)
            .filter(|wt| {
                occupied.contains(&wt.path) && !worktrees.iter().any(|fresh| fresh.path == wt.path)
            })
            .map(|wt| Worktree { prunable: true, upstream: None, ..wt })
            .collect();
        worktrees.extend(stranded);
        self.worktrees = worktrees;
        self.default_branch = found.project.default_branch;
        self.home = found.project.home;
    }
}

/// The wire form of a project, shared by the running app and the CLI's
/// app-less path so a client cannot tell which one answered it.
pub fn project_json(project: &Project) -> Value {
    json!({
        "name": project.display_name(),
        "label": project.label,
        "root": project.root,
        "default_branch": project.default_branch,
        "worktrees": project
            .worktrees
            .iter()
            .map(|wt| json!({
                "name": wt.name,
                "path": wt.path,
                "branch": wt.branch,
                "is_main": wt.is_main,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Branch name, or the short OID when detached.  The flag is what callers
/// key on: the OID string is indistinguishable from a branch name.
fn current_branch(repo: &Repository) -> (Option<String>, bool) {
    let Ok(head) = repo.head() else {
        return (None, false);
    };
    if head.is_branch() {
        (head.shorthand().map(|s| s.to_string()), false)
    } else {
        (head.target().map(|oid| oid.to_string().chars().take(7).collect()), true)
    }
}

/// A prunable worktree's checkout is gone, so its HEAD can't be read via
/// `Repository::open`. Git still records it in the main repo's admin area
/// (`.git/worktrees/<name>/HEAD`) — parse the symref line from there.
fn branch_from_admin_head(repo: &Repository, worktree_name: &str) -> Option<String> {
    let head = repo.path().join("worktrees").join(worktree_name).join("HEAD");
    let contents = std::fs::read_to_string(head).ok()?;
    contents.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

/// Best-effort detection of the repository's default branch.
///
/// `refs/remotes/origin/HEAD` is the source of truth when present — it's what
/// `origin` says the default branch is.  We fall back to common local names
/// only if the remote ref is missing.  `init.defaultBranch` is checked LAST
/// and only if it names a branch that actually exists in this repo, because
/// that config is about what `git init` names new repos — not the default
/// branch of an already-cloned project (a global `init.defaultBranch=master`
/// would otherwise hijack repos whose actual default is `main` or anything
/// else).  Returns the branch name (without `refs/heads/`) or `None`.
fn detect_default_branch(repo: &Repository) -> Option<String> {
    if let Ok(reference) = repo.find_reference("refs/remotes/origin/HEAD") {
        if let Some(target) = reference.symbolic_target() {
            if let Some(name) = target.strip_prefix("refs/remotes/origin/") {
                return Some(name.to_string());
            }
        }
    }

    for candidate in ["main", "master", "trunk", "develop"] {
        if repo.find_reference(&format!("refs/heads/{candidate}")).is_ok() {
            return Some(candidate.to_string());
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

/// A label is user text: trimmed, with an empty result meaning "no label", so
/// clearing the rename field falls back to the directory name.
pub fn normalize_label(label: Option<String>) -> Option<String> {
    label.map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

fn display_name(root: &std::path::Path) -> String {
    root.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| wsl::display_path(root))
}

/// Sections: 0 repo-or-not, 1 `worktree list --porcelain -z`,
/// 2 origin/HEAD symref, 3 which common default-branch names exist,
/// 4 `init.defaultBranch` only if it names an existing branch,
/// 5 the distro's `$HOME`, 6 upstream tracking state per local branch —
/// this last command only runs when `$2` is `"1"`, since `for-each-ref` with
/// `%(upstream:track)` computes divergence for every branch even when the
/// caller only wants the parse skipped.
/// The `for-each-ref` format the upstream section emits.  A macro rather than
/// a const so the script can `concat!` it and tests can hand the identical
/// string to git, instead of asserting against a second copy that can drift.
///
/// `lstrip=2` rather than `:short`: shortening consults other ref namespaces
/// and yields `heads/<name>` for a branch whose name a tag also uses, which
/// no longer joins against the plain branch name in a worktree record.
macro_rules! upstream_format {
    () => {
        "%(refname:lstrip=2)%09%(upstream:short)%09%(upstream:track,nobracket)"
    };
}

const DISCOVER_SCRIPT: &str = concat!(
    r#"
p="$1"
sep() { printf '\n@@ALACRITREE@@\n'; }
git -C "$p" rev-parse --is-inside-work-tree >/dev/null 2>&1 && printf yes || printf no
sep
git -C "$p" worktree list --porcelain -z 2>/dev/null
sep
git -C "$p" symbolic-ref refs/remotes/origin/HEAD 2>/dev/null
sep
git -C "$p" for-each-ref --format='%(refname:short)' refs/heads/main refs/heads/master refs/heads/trunk refs/heads/develop 2>/dev/null
sep
cfg=$(git -C "$p" config init.defaultBranch 2>/dev/null)
if [ -n "$cfg" ] && git -C "$p" rev-parse --verify --quiet "refs/heads/$cfg" >/dev/null 2>&1; then printf '%s' "$cfg"; fi
sep
printf '%s' "$HOME"
sep
if [ "$2" = "1" ]; then
  LC_ALL=C git -C "$p" for-each-ref --format='"#,
    upstream_format!(),
    r#"' refs/heads/ 2>/dev/null
fi
"#
);

/// One record from `git worktree list --porcelain -z`.  The main worktree is
/// always the first record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeRecord {
    path: String,
    head: Option<String>,
    branch: Option<String>,
}

/// Parse `git worktree list --porcelain -z`: attributes are NUL-terminated
/// `label value` lines; an empty line (two consecutive NULs) ends a record.
/// `detached`/`bare`/`locked`/`prunable` labels need no handling — a
/// detached record simply carries no `branch`.
fn parse_worktree_list_z(bytes: &[u8]) -> Vec<WorktreeRecord> {
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;
    for token in bytes.split(|&b| b == 0) {
        let token = String::from_utf8_lossy(token);
        let token = token.trim_matches('\n');
        if token.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        if let Some(path) = token.strip_prefix("worktree ") {
            if let Some(record) = current.take() {
                records.push(record);
            }
            current = Some(WorktreeRecord { path: path.to_string(), head: None, branch: None });
        } else if let Some(record) = current.as_mut() {
            if let Some(sha) = token.strip_prefix("HEAD ") {
                record.head = Some(sha.to_string());
            } else if let Some(branch) = token.strip_prefix("branch ") {
                record.branch =
                    Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string());
            }
        }
    }
    if let Some(record) = current.take() {
        records.push(record);
    }
    records
}

/// Replicates `detect_default_branch`'s priority from batched output — see
/// that function for why `init.defaultBranch` comes last.
fn default_branch_from_batch(
    origin_head: &str,
    existing: &str,
    config_default: &str,
) -> Option<String> {
    if let Some(name) = origin_head.trim().strip_prefix("refs/remotes/origin/") {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    let present: Vec<&str> = existing.lines().map(str::trim).collect();
    for candidate in ["main", "master", "trunk", "develop"] {
        if present.contains(&candidate) {
            return Some(candidate.to_string());
        }
    }
    let cfg = config_default.trim();
    (!cfg.is_empty()).then(|| cfg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_ext::CommandExt as _;
    use crate::test_util::{add_worktree, init_repo};

    /// `%(refname:short)` shortens to the *unambiguous* name, so a branch that
    /// shares its name with a tag comes back as `heads/<name>`.  Worktree
    /// records carry the plain branch name, so any shortening that consults
    /// other ref namespaces breaks the join and the row loses its badge.
    #[test]
    fn the_upstream_format_keys_branches_by_plain_name_even_when_a_tag_shares_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .hide_console()
                .current_dir(dir.path())
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        git(&["init", "-b", "main"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-m", "x"]);
        git(&["branch", "release"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "tag", "-m", "collide", "release"]);

        let out =
            git(&["for-each-ref", &format!("--format={}", upstream_format!()), "refs/heads/"]);
        let map = crate::upstream::parse_for_each_ref(&out);

        assert!(
            map.contains_key("release"),
            "expected a `release` key, got {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn refresh_keeps_worktrees_when_discovery_is_not_authoritative() {
        let mut project = Project::placeholder(PathBuf::from("/nonexistent-root"));
        project.default_branch = Some("develop".to_string());
        project.worktrees = vec![
            Worktree {
                name: "main".to_string(),
                path: PathBuf::from("/nonexistent-root"),
                branch: None,
                is_main: true,
                prunable: false,
                upstream: None,
            },
            Worktree {
                name: "feature".to_string(),
                path: PathBuf::from("/nonexistent-root-feature"),
                branch: Some("feature".to_string()),
                is_main: false,
                prunable: false,
                upstream: None,
            },
        ];

        let before = project.worktrees.clone();
        project.apply(
            Discovered {
                project: Project::placeholder(project.root.clone()),
                authoritative: false,
            },
            &HashSet::new(),
        );

        assert_eq!(project.worktrees.len(), before.len());
        assert_eq!(project.worktrees[1].name, "feature");
        assert_eq!(
            project.default_branch.as_deref(),
            Some("develop"),
            "an unreachable backend must not erase the known default branch either"
        );
    }

    #[test]
    fn apply_adopts_an_authoritative_result() {
        let mut project = Project::placeholder(PathBuf::from("/root"));
        let mut fresh = Project::placeholder(PathBuf::from("/root"));
        fresh.worktrees.clear();
        fresh.default_branch = Some("main".to_string());

        project.apply(Discovered { project: fresh, authoritative: true }, &HashSet::new());

        assert!(project.worktrees.is_empty());
        assert_eq!(project.default_branch.as_deref(), Some("main"));
    }

    /// `git worktree prune` drops the worktree from discovery while its shells
    /// keep running.  Dropping the row too would leave them with nowhere to be
    /// reached from.
    #[test]
    fn apply_keeps_a_dropped_worktree_that_still_holds_sessions() {
        let mut project = Project::placeholder(PathBuf::from("/repo"));
        project.worktrees = vec![Worktree {
            name: "gone".to_string(),
            path: PathBuf::from("/repo-worktrees/gone"),
            branch: Some("feature".to_string()),
            is_main: false,
            prunable: false,
            upstream: None,
        }];

        let mut fresh = Project::placeholder(PathBuf::from("/repo"));
        fresh.worktrees.clear();
        let occupied = HashSet::from([PathBuf::from("/repo-worktrees/gone")]);

        project.apply(Discovered::found(fresh), &occupied);

        let kept = project.worktrees.first().expect("the row survives its checkout");
        assert_eq!(kept.path, PathBuf::from("/repo-worktrees/gone"));
        assert_eq!(kept.branch.as_deref(), Some("feature"), "the branch still names the row");
        assert!(kept.prunable, "but it can no longer host a new shell");
    }

    #[test]
    fn apply_drops_a_worktree_once_its_last_session_is_gone() {
        let mut project = Project::placeholder(PathBuf::from("/repo"));
        project.worktrees = vec![Worktree {
            name: "gone".to_string(),
            path: PathBuf::from("/repo-worktrees/gone"),
            branch: None,
            is_main: false,
            prunable: true,
            upstream: None,
        }];

        let mut fresh = Project::placeholder(PathBuf::from("/repo"));
        fresh.worktrees.clear();

        project.apply(Discovered::found(fresh), &HashSet::new());

        assert!(project.worktrees.is_empty());
    }

    #[test]
    fn only_a_reachable_distro_gives_an_authoritative_answer() {
        let (reached, unreachable) = (true, false);
        let (repo, not_repo) = (true, false);

        // The round trip failed: the tree is unknown, not empty.
        assert_eq!(classify_wsl_answer(unreachable, not_repo, 0), WslAnswer::Unreachable);
        // The distro answered "this is not a repository" — that is the truth.
        assert_eq!(classify_wsl_answer(reached, not_repo, 0), WslAnswer::NotARepo);
        // A repository always has at least its main checkout, so parsing none of
        // them means the round trip came back malformed.
        assert_eq!(classify_wsl_answer(reached, repo, 0), WslAnswer::Unreachable);
        assert_eq!(classify_wsl_answer(reached, repo, 2), WslAnswer::Repo);
    }

    #[test]
    fn a_non_git_windows_root_is_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let found = jobs::on_this_thread(|b| Project::discover(dir.path().to_path_buf(), false, b));
        assert!(found.authoritative, "a directory that is genuinely not a repo is the truth");
        assert_eq!(found.project.worktrees.len(), 1);
    }

    #[test]
    fn live_worktree_is_not_prunable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        add_worktree(&repo, "feature");

        let project = jobs::on_this_thread(|b| Project::discover(repo_dir, false, b)).project;
        let wt = project.worktrees.iter().find(|w| w.name == "feature").unwrap();
        assert!(!wt.prunable);
        assert_eq!(wt.branch.as_deref(), Some("feature"));
    }

    /// A distro root has no `file_name()`, so the name falls back to the whole
    /// path — which must not be the UNC spelling.
    #[cfg(windows)]
    #[test]
    fn a_rootless_path_names_itself_in_the_distros_spelling() {
        assert_eq!(display_name(std::path::Path::new(r"\\wsl.localhost\kali-linux")), "/");
    }

    #[test]
    fn missing_dir_marks_worktree_prunable_and_keeps_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        let wt_path = add_worktree(&repo, "feature");
        std::fs::remove_dir_all(&wt_path).unwrap();

        let project = jobs::on_this_thread(|b| Project::discover(repo_dir, false, b)).project;
        let wt = project.worktrees.iter().find(|w| w.name == "feature").unwrap();
        assert!(wt.prunable);
        assert_eq!(wt.branch.as_deref(), Some("feature"));
    }

    #[test]
    fn main_worktree_is_never_prunable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        init_repo(&repo_dir);

        let project = jobs::on_this_thread(|b| Project::discover(repo_dir, false, b)).project;
        assert!(project.worktrees[0].is_main);
        assert!(!project.worktrees[0].prunable);
    }

    #[test]
    fn the_label_overrides_the_directory_name_for_display() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        init_repo(&repo_dir);

        let mut project = jobs::on_this_thread(|b| Project::discover(repo_dir, false, b)).project;
        assert_eq!(project.display_name(), "repo");

        project.label = Some("Work".to_string());
        assert_eq!(project.display_name(), "Work");
        let encoded = project_json(&project);
        assert_eq!(encoded["name"], "Work");
        assert_eq!(encoded["label"], "Work");
    }

    #[test]
    fn a_blank_label_normalizes_to_none() {
        assert_eq!(normalize_label(Some("  ".to_string())), None);
        assert_eq!(normalize_label(Some(String::new())), None);
        assert_eq!(normalize_label(Some(" Work ".to_string())), Some("Work".to_string()));
        assert_eq!(normalize_label(None), None);
    }

    #[test]
    fn parses_worktree_list_porcelain_z() {
        let bytes = b"worktree /home/lev/proj\0HEAD 1234567890abcdef\0branch refs/heads/main\0\0\
worktree /home/lev/wt/feat-x\0HEAD fedcba0987654321\0branch refs/heads/feat-x\0\0\
worktree /home/lev/wt/tmp\0HEAD 0011223344556677\0detached\0\0";
        let records = parse_worktree_list_z(bytes);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].path, "/home/lev/proj");
        assert_eq!(records[0].branch.as_deref(), Some("main"));
        assert_eq!(records[1].branch.as_deref(), Some("feat-x"));
        assert_eq!(records[2].branch, None);
        assert_eq!(records[2].head.as_deref(), Some("0011223344556677"));
    }

    #[test]
    fn worktree_paths_with_spaces_survive() {
        let bytes = b"worktree /home/lev/my proj\0HEAD abc\0branch refs/heads/main\0\0";
        let records = parse_worktree_list_z(bytes);
        assert_eq!(records[0].path, "/home/lev/my proj");
    }

    /// The parser reads sections by position, so their order is a contract:
    /// each index below names the command whose output belongs there.
    #[test]
    fn the_discover_script_sections_keep_their_indices() {
        let sections: Vec<&str> = DISCOVER_SCRIPT.split("\nsep\n").collect();
        assert_eq!(sections.len(), 7, "six sep boundaries, seven sections");
        assert!(sections[5].contains(r#"printf '%s' "$HOME""#), "$HOME must stay at index 5");
        assert!(sections[6].contains("for-each-ref"), "upstream tracking is the new last section");
        assert!(sections[6].contains("LC_ALL=C"), "the track vocabulary is localized");
        assert!(
            sections[6].contains(r#"if [ "$2" = "1" ]; then"#),
            "the for-each-ref call must stay gated by the upstream flag, or the feature runs \
             even when disabled"
        );
    }

    /// A detached HEAD's `branch` holds a short OID, and a real branch can be
    /// named the same thing.  Build a worktree where that collision actually
    /// occurs and check the lookup, not just that the record has no branch —
    /// the latter holds whether or not the lookup guards against the collision.
    #[test]
    fn a_detached_worktree_gets_no_badge_even_when_its_oid_looks_like_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("t", "t@t").unwrap();
        let tree = repo.find_tree(repo.treebuilder(None).unwrap().write().unwrap()).unwrap();
        let oid = repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        // A branch named exactly like the short OID the detached row will show.
        let short: String = oid.to_string().chars().take(7).collect();
        repo.branch(&short, &repo.find_commit(oid).unwrap(), false).unwrap();
        repo.set_head_detached(oid).unwrap();

        let project =
            jobs::on_this_thread(|b| Project::discover(dir.path().to_path_buf(), true, b)).project;
        let main = &project.worktrees[0];
        assert!(
            main.upstream.is_none(),
            "a detached row must not adopt the same-named branch's state"
        );
    }

    /// Proves the `upstream` flag gates the git2 branch walk itself, not just
    /// whether the result gets painted: build a branch with a genuinely
    /// resolvable upstream, discover the same repo with the flag on and off,
    /// and require the two runs to disagree. A test that only checked the
    /// off case would pass against code that never looked anything up at all.
    #[test]
    fn the_upstream_flag_gates_whether_the_walk_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);

        let head_name = repo.head().unwrap().shorthand().unwrap().to_string();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("upstream-branch", &head_commit, false).unwrap();
        let mut head_branch = repo.find_branch(&head_name, git2::BranchType::Local).unwrap();
        head_branch.set_upstream(Some("upstream-branch")).unwrap();

        let with_flag =
            jobs::on_this_thread(|b| Project::discover(repo_dir.clone(), true, b)).project;
        let without_flag = jobs::on_this_thread(|b| Project::discover(repo_dir, false, b)).project;

        assert_eq!(
            with_flag.worktrees[0].upstream,
            Some(crate::upstream::UpstreamState::Level { upstream: "upstream-branch".to_string() }),
            "a real upstream must be found when the flag is on"
        );
        assert_eq!(
            without_flag.worktrees[0].upstream, None,
            "the same upstream must not be found when the flag is off"
        );
    }

    /// Discovery is adopted through one method by both refresh paths.  Two paths
    /// each listing fields by hand is what lets a newly discovered field land in
    /// one and be dropped by the other — and the folder-picker path, which starts
    /// from a placeholder, is the one that matters most.
    #[test]
    fn adopting_a_discovery_keeps_user_state_and_takes_the_rest() {
        let mut existing = Project::placeholder(PathBuf::from("/repo"));
        existing.label = Some("Work".to_string());
        existing.expanded = false;

        let mut fresh = Project::placeholder(PathBuf::from("/repo"));
        fresh.default_branch = Some("main".to_string());
        fresh.home = Some("/home/lev".to_string());

        existing.apply(Discovered::found(fresh), &HashSet::new());

        assert_eq!(existing.home.as_deref(), Some("/home/lev"));
        assert_eq!(existing.default_branch.as_deref(), Some("main"));
        assert_eq!(existing.label.as_deref(), Some("Work"), "a rename is user state");
        assert!(!existing.expanded, "the expand toggle is user state");
    }

    #[test]
    fn default_branch_priority_matches_git2_arm() {
        // origin/HEAD wins.
        assert_eq!(
            default_branch_from_batch("refs/remotes/origin/dev\n", "main\nmaster", "master"),
            Some("dev".to_string())
        );
        // Then common names in priority order, regardless of listing order.
        assert_eq!(default_branch_from_batch("", "develop\nmain", ""), Some("main".to_string()));
        // init.defaultBranch is last (already existence-verified by the script).
        assert_eq!(default_branch_from_batch("", "", "trunk2"), Some("trunk2".to_string()));
        assert_eq!(default_branch_from_batch("", "", ""), None);
    }
}
