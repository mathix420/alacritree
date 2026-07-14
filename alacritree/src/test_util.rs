//! Shared fixtures for tests that need a real repository with worktrees.

use std::path::{Path, PathBuf};

use git2::Repository;

/// Initialize a repository with one empty commit so worktrees can be added.
pub fn init_repo(dir: &Path) -> Repository {
    std::fs::create_dir_all(dir).unwrap();
    let repo = Repository::init(dir).unwrap();
    {
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    }
    repo
}

/// Add a linked worktree named `name` (git2 also creates a branch `name`).
/// Returns the worktree's checkout path, a sibling of the repo directory.
pub fn add_worktree(repo: &Repository, name: &str) -> PathBuf {
    let path = repo.workdir().unwrap().parent().unwrap().join(format!("wt-{name}"));
    repo.worktree(name, &path, None).unwrap();
    path
}
