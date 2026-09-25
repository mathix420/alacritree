//! Where a working directory sits for the tasks scope: which side runs its
//! commands, and which repository and checkout the version control that owns
//! it places it in.

use std::path::Path;

use alacritree_common::jobs::Blocking;
use alacritree_common::side::Side;
use alacritree_common::wsl;
use alacritree_tasks::scope::Place;
use alacritree_vcs::{Located, VersionControl};

use crate::vcs::Vcs;

pub(crate) fn side_of(cwd: &Path) -> (Side, String) {
    match wsl::classify(cwd) {
        wsl::Location::Wsl { distro, linux_path } => (Side::Wsl(distro), linux_path),
        wsl::Location::Windows(path) => (Side::Native, path.display().to_string()),
    }
}

/// The first of `backends` that locates `cwd` places it.
pub(crate) fn place_for(cwd: &Path, backends: &[Vcs], b: &Blocking) -> (Side, Place) {
    let (side, _) = side_of(cwd);
    let located = backends.iter().find_map(|vcs| vcs.locate(cwd, b));
    (side, place_from(located.as_ref()))
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

fn basename(path: &Path) -> String {
    normalize(&path.to_string_lossy()).rsplit('/').next().unwrap_or_default().to_string()
}

/// The main checkout names the repository for all of them. A head that names
/// no branch, detached or sharing its branch with another checkout, is named
/// by its directory.
pub(crate) fn place_from(located: Option<&Located>) -> Place {
    let Some(located) = located else { return Place::Global };
    let name = basename(&located.main);
    // A bare repository's directory carries a `.git` suffix its name does not.
    let repo = match name.strip_suffix(".git") {
        Some(stripped) if located.bare => stripped.to_string(),
        _ => name,
    };
    let Some(checkout) = &located.checkout else { return Place::Project { repo } };
    let branch = located.head.name.clone().unwrap_or_else(|| basename(checkout));
    Place::Workspace { repo, branch }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use alacritree_vcs::{Head, Located};

    use super::*;

    fn located(main: &str, checkout: Option<&str>, branch: Option<&str>) -> Located {
        Located {
            main: PathBuf::from(main),
            bare: false,
            checkout: checkout.map(PathBuf::from),
            head: Head { name: branch.map(str::to_string), ..Head::default() },
        }
    }

    #[test]
    fn the_main_checkout_names_the_repo_and_the_current_one_the_branch() {
        let here =
            located("/src/alacritree", Some("/src/alacritree-worktrees/feat/x"), Some("feat/x"));
        assert_eq!(place_from(Some(&here)), Place::Workspace {
            repo: "alacritree".into(),
            branch: "feat/x".into()
        });
    }

    /// A detached head, or a branch another checkout also has out, names no
    /// branch, so the directory does.
    #[test]
    fn a_head_with_no_name_falls_back_to_the_directory_name() {
        let here = located("/src/r", Some("/src/wt/review"), None);
        assert_eq!(place_from(Some(&here)), Place::Workspace {
            repo: "r".into(),
            branch: "review".into()
        });
    }

    #[test]
    fn a_bare_repo_drops_its_git_suffix_and_has_no_workspace_at_its_root() {
        let bare =
            |checkout, branch| Located { bare: true, ..located("/src/proj.git", checkout, branch) };
        assert_eq!(place_from(Some(&bare(None, None))), Place::Project { repo: "proj".into() });
        let linked = bare(Some("/src/proj-main"), Some("main"));
        assert_eq!(place_from(Some(&linked)), Place::Workspace {
            repo: "proj".into(),
            branch: "main".into()
        });
    }

    /// Only a bare repository's suffix is dropped. A working copy that happens
    /// to live in a directory called `x.git` is named `x.git`, and its tasks
    /// stay on that node.
    #[test]
    fn a_working_copy_keeps_a_git_suffix_in_its_name() {
        let here = located("/src/tooling.git", Some("/src/tooling.git"), Some("main"));
        assert_eq!(place_from(Some(&here)), Place::Workspace {
            repo: "tooling.git".into(),
            branch: "main".into()
        });
    }

    #[test]
    fn windows_separators_and_trailing_slashes_still_name_the_repo() {
        let here = located("C:\\src\\r\\", Some("C:\\src\\r\\"), Some("main"));
        assert_eq!(place_from(Some(&here)), Place::Workspace {
            repo: "r".into(),
            branch: "main".into()
        });
    }

    #[test]
    fn nothing_located_is_global() {
        assert_eq!(place_from(None), Place::Global);
    }

    #[cfg(windows)]
    #[test]
    fn a_distro_path_runs_on_that_distro() {
        let (side, cwd) = side_of(Path::new(r"\\wsl.localhost\Ubuntu\home\lev\r"));
        assert_eq!(side, Side::Wsl("Ubuntu".into()));
        assert_eq!(cwd, "/home/lev/r");
    }

    #[test]
    fn a_real_repository_reports_its_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = alacritree_git::test_support::init_repo_on(&dir.path().join("myrepo"), "trunk");
        let backends = [crate::vcs::Vcs::Git(alacritree_git::GitBackend)];
        let (_, place) = crate::jobs::on_this_thread(|b| place_for(&repo, &backends, b));
        assert_eq!(place, Place::Workspace { repo: "myrepo".into(), branch: "trunk".into() });
        let (_, outside) = crate::jobs::on_this_thread(|b| place_for(dir.path(), &backends, b));
        assert_eq!(outside, Place::Global);
        let (_, disabled) = crate::jobs::on_this_thread(|b| place_for(&repo, &[], b));
        assert_eq!(disabled, Place::Global, "with no backend nothing is a repository");
    }
}
