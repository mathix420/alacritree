//! Repositories on disk for tests, built through libgit2 so callers need no
//! git2 of their own.

use std::path::{Path, PathBuf};

use git2::Repository;

/// A repository with one commit on `main`, set explicitly so the machine's
/// `init.defaultBranch` does not leak into tests.
pub fn init_repo(dir: &Path) -> PathBuf {
    init_repo_on(dir, "main")
}

/// The same with its one commit on `branch`.
pub fn init_repo_on(dir: &Path, branch: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head(branch);
    let repo = Repository::init_opts(dir, &opts).unwrap();
    let sig = git2::Signature::now("test", "test@example.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    dir.to_path_buf()
}

/// A clone of a one-commit repository, both under `dir`, so a worktree create
/// has an `origin` to resolve and fetch from without touching the network.
/// Returns the clone's root.
pub fn clone_with_origin(dir: &Path) -> PathBuf {
    let origin = init_repo(&dir.join("origin"));
    let project = dir.join("project");
    Repository::clone(origin.to_str().unwrap(), &project).unwrap();
    project
}

/// A bare clone of `src` at `dest`, the layout a project rooted at `repo.git`
/// has.
pub fn bare_clone(src: &Path, dest: &Path) -> PathBuf {
    git2::build::RepoBuilder::new().bare(true).clone(src.to_str().unwrap(), dest).unwrap();
    dest.to_path_buf()
}

/// Add a linked worktree named `name` (git2 also creates a branch `name`).
/// Returns the worktree's checkout path, a sibling of the repo directory.
pub fn add_worktree(repo: &Path, name: &str) -> PathBuf {
    let path = repo.parent().unwrap().join(format!("wt-{name}"));
    Repository::open(repo).unwrap().worktree(name, &path, None).unwrap();
    path
}

pub fn add_remote(repo: &Path, name: &str, url: &str) {
    Repository::open(repo).unwrap().remote(name, url).unwrap();
}

pub fn set_config(repo: &Path, key: &str, value: &str) {
    Repository::open(repo).unwrap().config().unwrap().set_str(key, value).unwrap();
}

/// Points HEAD at its commit directly, as `git checkout --detach` does.
pub fn detach(repo: &Path) {
    let repo = Repository::open(repo).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap().id();
    repo.set_head_detached(commit).unwrap();
}

/// Moves HEAD to `branch` behind the app's back, as a shell user would. The
/// branch is created at HEAD's commit when it does not exist yet.
pub fn switch_head(checkout: &Path, branch: &str) {
    let repo = Repository::open(checkout).unwrap();
    if repo.find_branch(branch, git2::BranchType::Local).is_err() {
        let commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch(branch, &commit, false).unwrap();
    }
    repo.set_head(&format!("refs/heads/{branch}")).unwrap();
}
