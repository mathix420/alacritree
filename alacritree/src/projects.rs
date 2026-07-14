//! Enumerate sidebar-added directories and their git worktrees.

use std::path::PathBuf;

use git2::Repository;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub default_branch: Option<String>,
    pub worktrees: Vec<Worktree>,
    pub expanded: bool,
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
}

impl Project {
    /// Non-git roots get a single pseudo-worktree pointing at themselves so
    /// the user can still spawn a shell there from the sidebar.
    pub fn discover(root: PathBuf) -> Self {
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());

        match Repository::open(&root) {
            Ok(repo) => Self::from_repo(root, name, &repo),
            Err(_) => Project {
                worktrees: vec![Worktree {
                    name: name.clone(),
                    path: root.clone(),
                    branch: None,
                    is_main: true,
                    prunable: false,
                }],
                root,
                name,
                default_branch: None,
                expanded: true,
            },
        }
    }

    fn from_repo(root: PathBuf, name: String, repo: &Repository) -> Self {
        let main_path = repo.workdir().map(|p| p.to_path_buf()).unwrap_or_else(|| root.clone());

        let mut worktrees = Vec::new();
        worktrees.push(Worktree {
            name: "main".to_string(),
            path: main_path.clone(),
            branch: current_branch(repo),
            is_main: true,
            prunable: false,
        });

        if let Ok(names) = repo.worktrees() {
            for name in names.iter().flatten() {
                if let Ok(wt) = repo.find_worktree(name) {
                    let path = wt.path().to_path_buf();
                    let branch = Repository::open(&path)
                        .ok()
                        .and_then(|wt_repo| current_branch(&wt_repo))
                        .or_else(|| branch_from_admin_head(repo, name));
                    worktrees.push(Worktree {
                        name: name.to_string(),
                        // Directory existence, not git2's `is_prunable`, is
                        // the signal: a *locked* worktree with a missing dir
                        // is not git-prunable but still can't host a shell.
                        prunable: !path.is_dir(),
                        path,
                        branch,
                        is_main: false,
                    });
                }
            }
        }

        Project {
            default_branch: detect_default_branch(repo),
            worktrees,
            root,
            name,
            expanded: true,
        }
    }

    pub fn refresh(&mut self) {
        let updated = Project::discover(self.root.clone());
        self.worktrees = updated.worktrees;
        self.default_branch = updated.default_branch;
    }
}

fn current_branch(repo: &Repository) -> Option<String> {
    let head = repo.head().ok()?;
    if head.is_branch() {
        head.shorthand().map(|s| s.to_string())
    } else {
        // Detached HEAD: show the short OID.
        head.target().map(|oid| {
            let s = oid.to_string();
            s.chars().take(7).collect()
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{add_worktree, init_repo};

    #[test]
    fn live_worktree_is_not_prunable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        add_worktree(&repo, "feature");

        let project = Project::discover(repo_dir);
        let wt = project.worktrees.iter().find(|w| w.name == "feature").unwrap();
        assert!(!wt.prunable);
        assert_eq!(wt.branch.as_deref(), Some("feature"));
    }

    #[test]
    fn missing_dir_marks_worktree_prunable_and_keeps_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        let wt_path = add_worktree(&repo, "feature");
        std::fs::remove_dir_all(&wt_path).unwrap();

        let project = Project::discover(repo_dir);
        let wt = project.worktrees.iter().find(|w| w.name == "feature").unwrap();
        assert!(wt.prunable);
        assert_eq!(wt.branch.as_deref(), Some("feature"));
    }

    #[test]
    fn main_worktree_is_never_prunable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        init_repo(&repo_dir);

        let project = Project::discover(repo_dir);
        assert!(project.worktrees[0].is_main);
        assert!(!project.worktrees[0].prunable);
    }
}
