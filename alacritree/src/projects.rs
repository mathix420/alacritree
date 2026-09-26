//! Enumerate sidebar-added directories and their git worktrees.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{Value, json};

use alacritree_common::{jobs, wsl};
use alacritree_vcs::{Checkout, Head, VcsError, VersionControl};

use crate::vcs::Vcs;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    /// Derived from the root's directory name; never stored.
    pub name: String,
    /// User-set display label, shown instead of `name` when present.  Like
    /// `expanded` and `shell_override`, this is user state: discovery never
    /// sets it, and refreshes must not lose it.
    pub label: Option<String>,
    /// The backend that owns the repository, or `None` for a plain folder.
    pub vcs: Option<Vcs>,
    pub trunk: Option<String>,
    pub checkouts: Vec<Checkout>,
    pub expanded: bool,
    pub shell_override: Option<alacritree_common::wsl::ShellChoice>,
    /// The distro's own `$HOME` for a WSL project, so a path can collapse to
    /// `~` without guessing the prefix from the path itself.  `None` for a
    /// native project, whose home comes from `home::home_dir()`.
    pub home: Option<String>,
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

impl Project {
    /// Ask each enabled backend in turn. `claims` is skipped because it would
    /// open each repository a second time, and startup runs this on the UI
    /// thread for every native project.
    pub fn discover(
        root: PathBuf,
        backends: &[Vcs],
        upstream: bool,
        blocking: &jobs::Blocking,
    ) -> Discovered {
        for vcs in backends {
            match vcs.discover(&root, &[], upstream, blocking) {
                Ok(repo) => {
                    return Discovered::found(Self::from_repository(root, vcs.clone(), repo));
                },
                // A directory that is not a repository is a fact, not a failure.
                Err(VcsError::NotARepository(_)) => continue,
                Err(VcsError::Unreachable(e)) => {
                    log::warn!("WSL discovery failed for {}: {e}", root.display());
                    return Discovered::unavailable(Self::placeholder(root));
                },
                Err(e) => {
                    log::warn!("discovery failed for {}: {e}", root.display());
                    return Discovered::unavailable(Self::placeholder(root));
                },
            }
        }
        Discovered::found(Self::placeholder(root))
    }

    fn from_repository(root: PathBuf, vcs: Vcs, repo: alacritree_vcs::Repository) -> Self {
        Project {
            name: display_name(&root),
            vcs: Some(vcs),
            trunk: repo.trunk,
            checkouts: repo.checkouts,
            home: repo.home,
            ..Self::placeholder(root)
        }
    }

    /// Pseudo-worktree entry: what non-git roots get permanently, and what a
    /// WSL project shows until background discovery fills in worktrees.
    pub fn placeholder(root: PathBuf) -> Self {
        let name = display_name(&root);
        Project {
            checkouts: vec![Checkout {
                name: name.clone(),
                path: root.clone(),
                head: Head::default(),
                is_main: true,
                gone: false,
                upstream: None,
            }],
            root,
            name,
            label: None,
            vcs: None,
            trunk: None,
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
        let mut checkouts = found.project.checkouts;
        let stranded: Vec<Checkout> = self
            .checkouts
            .drain(..)
            .filter(|wt| {
                occupied.contains(&wt.path) && !checkouts.iter().any(|fresh| fresh.path == wt.path)
            })
            .map(|wt| Checkout { gone: true, upstream: None, ..wt })
            .collect();
        checkouts.extend(stranded);
        self.checkouts = checkouts;
        self.vcs = found.project.vcs;
        self.trunk = found.project.trunk;
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
        "default_branch": project.trunk,
        "worktrees": project
            .checkouts
            .iter()
            .map(|wt| json!({
                "name": wt.name,
                "path": wt.path,
                "branch": wt.head.label(),
                "is_main": wt.is_main,
            }))
            .collect::<Vec<_>>(),
    })
}

/// A root the sidebar holds no project at.
#[derive(Debug, thiserror::Error)]
#[error("{} is not a project in the sidebar", .0.display())]
pub(crate) struct NotAProject(pub(crate) PathBuf);

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

#[cfg(test)]
mod tests {
    use super::*;
    use alacritree_git::test_support::{add_worktree, init_repo};

    #[test]
    fn refresh_keeps_worktrees_when_discovery_is_not_authoritative() {
        let mut project = Project::placeholder(PathBuf::from("/nonexistent-root"));
        project.trunk = Some("develop".to_string());
        project.checkouts = vec![
            Checkout {
                name: "main".to_string(),
                path: PathBuf::from("/nonexistent-root"),
                head: Head { name: None, ..Head::default() },
                is_main: true,
                gone: false,
                upstream: None,
            },
            Checkout {
                name: "feature".to_string(),
                path: PathBuf::from("/nonexistent-root-feature"),
                head: Head { name: Some("feature".to_string()), ..Head::default() },
                is_main: false,
                gone: false,
                upstream: None,
            },
        ];

        let before = project.checkouts.clone();
        project.apply(
            Discovered {
                project: Project::placeholder(project.root.clone()),
                authoritative: false,
            },
            &HashSet::new(),
        );

        assert_eq!(project.checkouts.len(), before.len());
        assert_eq!(project.checkouts[1].name, "feature");
        assert_eq!(
            project.trunk.as_deref(),
            Some("develop"),
            "an unreachable backend must not erase the known default branch either"
        );
    }

    #[test]
    fn apply_adopts_an_authoritative_result() {
        let mut project = Project::placeholder(PathBuf::from("/root"));
        let mut fresh = Project::placeholder(PathBuf::from("/root"));
        fresh.checkouts.clear();
        fresh.trunk = Some("main".to_string());

        project.apply(Discovered { project: fresh, authoritative: true }, &HashSet::new());

        assert!(project.checkouts.is_empty());
        assert_eq!(project.trunk.as_deref(), Some("main"));
    }

    /// `git worktree prune` drops the worktree from discovery while its shells
    /// keep running.  Dropping the row too would leave them with nowhere to be
    /// reached from.
    #[test]
    fn apply_keeps_a_dropped_worktree_that_still_holds_sessions() {
        let mut project = Project::placeholder(PathBuf::from("/repo"));
        project.checkouts = vec![Checkout {
            name: "gone".to_string(),
            path: PathBuf::from("/repo-worktrees/gone"),
            head: Head { name: Some("feature".to_string()), ..Head::default() },
            is_main: false,
            gone: false,
            upstream: None,
        }];

        let mut fresh = Project::placeholder(PathBuf::from("/repo"));
        fresh.checkouts.clear();
        let occupied = HashSet::from([PathBuf::from("/repo-worktrees/gone")]);

        project.apply(Discovered::found(fresh), &occupied);

        let kept = project.checkouts.first().expect("the row survives its checkout");
        assert_eq!(kept.path, PathBuf::from("/repo-worktrees/gone"));
        assert_eq!(kept.head.name.as_deref(), Some("feature"), "the branch still names the row");
        assert!(kept.gone, "but it can no longer host a new shell");
    }

    #[test]
    fn apply_drops_a_worktree_once_its_last_session_is_gone() {
        let mut project = Project::placeholder(PathBuf::from("/repo"));
        project.checkouts = vec![Checkout {
            name: "gone".to_string(),
            path: PathBuf::from("/repo-worktrees/gone"),
            head: Head { name: None, ..Head::default() },
            is_main: false,
            gone: true,
            upstream: None,
        }];

        let mut fresh = Project::placeholder(PathBuf::from("/repo"));
        fresh.checkouts.clear();

        project.apply(Discovered::found(fresh), &HashSet::new());

        assert!(project.checkouts.is_empty());
    }

    /// A distro that is stopped answers no discovery. The project keeps the
    /// rows it had rather than turning into a plain folder.
    #[test]
    fn an_unreachable_repository_is_not_authoritative() {
        let fake = alacritree_vcs::fake::FakeVcs::new("/r").unreachable("the distro is stopped");
        let found = jobs::on_this_thread(|b| {
            Project::discover(PathBuf::from("/r"), &[crate::vcs::Vcs::Fake(fake)], false, b)
        });
        assert!(!found.authoritative);
    }

    fn git_backends() -> Vec<crate::vcs::Vcs> {
        crate::vcs::backends(&crate::config::IntegrationsConfig::default())
    }

    /// `topic` is no well-known name and has no `origin/HEAD` behind it, so no
    /// trunk resolves, and the repository must still count as one.
    #[test]
    fn a_repository_without_a_detectable_trunk_is_still_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = alacritree_git::test_support::init_repo_on(dir.path(), "topic");
        let found =
            jobs::on_this_thread(|b| Project::discover(repo.clone(), &git_backends(), false, b));
        assert!(found.project.trunk.is_none());
        assert!(found.project.vcs.is_some());
        assert_eq!(found.project.checkouts[0].head.name.as_deref(), Some("topic"));
    }

    #[test]
    fn a_detached_worktree_keeps_its_revision_apart_from_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let repo = alacritree_git::test_support::init_repo(&dir.path().join("repo"));
        let wt = alacritree_git::test_support::add_worktree(&repo, "side");
        alacritree_git::test_support::detach(&wt);
        let found =
            jobs::on_this_thread(|b| Project::discover(repo.clone(), &git_backends(), false, b));
        // Git reports the long form of a temp dir Windows may hand out as an
        // 8.3 short path, so the paths compare canonicalized.
        let wt = wt.canonicalize().unwrap();
        let side = found
            .project
            .checkouts
            .iter()
            .find(|c| c.path.canonicalize().ok().as_ref() == Some(&wt))
            .unwrap();
        assert_eq!(side.head.name, None);
        assert_eq!(side.head.label().map(str::len), Some(7));
    }

    #[test]
    fn a_non_git_windows_root_is_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let found = jobs::on_this_thread(|b| {
            Project::discover(dir.path().to_path_buf(), &git_backends(), false, b)
        });
        assert!(found.authoritative, "a directory that is genuinely not a repo is the truth");
        assert_eq!(found.project.checkouts.len(), 1);
    }

    #[test]
    fn live_worktree_is_not_prunable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        add_worktree(&repo, "feature");

        let project =
            jobs::on_this_thread(|b| Project::discover(repo_dir, &git_backends(), false, b))
                .project;
        let wt = project.checkouts.iter().find(|w| w.name == "feature").unwrap();
        assert!(!wt.gone);
        assert_eq!(wt.head.name.as_deref(), Some("feature"));
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

        let project =
            jobs::on_this_thread(|b| Project::discover(repo_dir, &git_backends(), false, b))
                .project;
        let wt = project.checkouts.iter().find(|w| w.name == "feature").unwrap();
        assert!(wt.gone);
        assert_eq!(wt.head.name.as_deref(), Some("feature"));
    }

    #[test]
    fn main_worktree_is_never_prunable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        init_repo(&repo_dir);

        let project =
            jobs::on_this_thread(|b| Project::discover(repo_dir, &git_backends(), false, b))
                .project;
        assert!(project.checkouts[0].is_main);
        assert!(!project.checkouts[0].gone);
    }

    #[test]
    fn the_label_overrides_the_directory_name_for_display() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        init_repo(&repo_dir);

        let mut project =
            jobs::on_this_thread(|b| Project::discover(repo_dir, &git_backends(), false, b))
                .project;
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
        fresh.trunk = Some("main".to_string());
        fresh.home = Some("/home/lev".to_string());

        existing.apply(Discovered::found(fresh), &HashSet::new());

        assert_eq!(existing.home.as_deref(), Some("/home/lev"));
        assert_eq!(existing.trunk.as_deref(), Some("main"));
        assert_eq!(existing.label.as_deref(), Some("Work"), "a rename is user state");
        assert!(!existing.expanded, "the expand toggle is user state");
    }

    #[test]
    fn with_git_disabled_a_repository_is_a_plain_folder() {
        let dir = tempfile::tempdir().unwrap();
        let repo = alacritree_git::test_support::init_repo(dir.path());
        let found = jobs::on_this_thread(|b| Project::discover(repo.clone(), &[], false, b));
        assert!(found.project.vcs.is_none());
        assert_eq!(found.project.checkouts.len(), 1);
    }
}
