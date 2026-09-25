//! Where a branch's pull request lives, read from the repository's config.

use std::path::Path;

use alacritree_vcs::Remotes;

/// `origin`'s URL, which says which checkouts share a repository, and the
/// URL `branch` pushes to, which says whose pull request it can have. Either
/// is `None` for a missing or unreadable remote.
pub(crate) fn remotes(checkout: &Path, branch: &str) -> Remotes {
    let Ok(repo) = git2::Repository::open(checkout) else { return Remotes::default() };
    let url = |name: &str| repo.find_remote(name).ok()?.url().map(str::to_string);
    Remotes { origin_url: url("origin"), push_url: url(&push_remote(&repo, branch)) }
}

/// The remote `git push` sends `branch` to, in git's own order. `.` names the
/// local repository, which pushes nowhere, so it reads as unset.
fn push_remote(repo: &git2::Repository, branch: &str) -> String {
    let Ok(config) = repo.config() else { return "origin".to_string() };
    [
        format!("branch.{branch}.pushRemote"),
        "remote.pushDefault".into(),
        format!("branch.{branch}.remote"),
    ]
    .iter()
    .filter_map(|key| config.get_string(key).ok())
    .find(|name| !name.is_empty() && name != ".")
    .unwrap_or_else(|| "origin".to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use alacritree_vcs::{Remotes, VersionControl};

    use crate::test_support::{add_remote, init_repo, set_config};
    use crate::{GitBackend, GitConfig};

    fn remotes(repo: &Path, branch: &str) -> Remotes {
        GitBackend::new(&GitConfig::default()).remotes(repo, branch)
    }

    #[test]
    fn a_branch_push_remote_wins_over_origin() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        add_remote(&repo, "origin", "https://github.com/up/repo.git");
        add_remote(&repo, "fork", "https://github.com/me/repo.git");
        set_config(&repo, "branch.main.pushRemote", "fork");
        let remotes = remotes(&repo, "main");
        assert_eq!(remotes.origin_url.as_deref(), Some("https://github.com/up/repo.git"));
        assert_eq!(remotes.push_url.as_deref(), Some("https://github.com/me/repo.git"));
    }

    #[test]
    fn without_a_push_setting_the_branch_pushes_to_origin() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        add_remote(&repo, "origin", "gh:me/repo.git");
        let remotes = remotes(&repo, "main");
        assert_eq!(remotes.origin_url.as_deref(), Some("gh:me/repo.git"));
        assert_eq!(remotes.push_url.as_deref(), Some("gh:me/repo.git"));
    }

    #[test]
    fn push_default_wins_over_the_branch_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        add_remote(&repo, "origin", "https://github.com/up/repo.git");
        add_remote(&repo, "fork", "https://github.com/me/repo.git");
        add_remote(&repo, "other", "https://github.com/other/repo.git");
        set_config(&repo, "remote.pushDefault", "fork");
        set_config(&repo, "branch.main.remote", "other");
        assert_eq!(
            remotes(&repo, "main").push_url.as_deref(),
            Some("https://github.com/me/repo.git")
        );
    }

    #[test]
    fn the_branch_upstream_is_the_last_resort_before_origin() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        add_remote(&repo, "origin", "https://github.com/up/repo.git");
        add_remote(&repo, "other", "https://github.com/other/repo.git");
        set_config(&repo, "branch.main.remote", "other");
        assert_eq!(
            remotes(&repo, "main").push_url.as_deref(),
            Some("https://github.com/other/repo.git")
        );
    }

    /// `.` names the local repository, which pushes nowhere.
    #[test]
    fn a_local_push_remote_reads_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        add_remote(&repo, "origin", "https://github.com/up/repo.git");
        set_config(&repo, "branch.main.remote", ".");
        assert_eq!(
            remotes(&repo, "main").push_url.as_deref(),
            Some("https://github.com/up/repo.git")
        );
    }

    #[test]
    fn a_folder_that_is_no_repository_has_no_remotes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(remotes(dir.path(), "main"), Remotes::default());
    }
}
