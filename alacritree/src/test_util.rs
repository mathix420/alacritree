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

/// Hold `path` the way the loader holds a running exe image: write sharing
/// denied (overwrites fail with access denied), delete/rename sharing allowed.
/// One divergence from a real image: this file *can* be deleted while held,
/// a mapped image cannot — so tests may rely on rename behaviour, never on
/// delete behaviour.
#[cfg(windows)]
pub fn hold_like_a_running_image(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};

    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
        .unwrap()
}
