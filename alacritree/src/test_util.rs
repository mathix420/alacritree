//! Shared fixtures: scratch space on disk, and a real repository with
//! worktrees.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use git2::Repository;

/// Test scratch directories are named for the process that owns them.
const SCRATCH_PREFIX: &str = "alacritree-test-scratch-";

/// A directory under the system temporary directory belonging to this test
/// process alone.
///
/// A fixture written under a fixed name is shared with every other test binary
/// running at the same time, and one this process has memory-mapped cannot be
/// rewritten by them at all: Windows fails that write with
/// `ERROR_USER_MAPPED_FILE`.  Nothing deletes these on the way out, so the
/// first caller sweeps the ones whose process has gone.
pub fn scratch_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = std::env::temp_dir();
        sweep_abandoned_scratch_dirs(&root);
        let dir = root.join(format!("{SCRATCH_PREFIX}{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    })
}

/// Best effort throughout: a directory another test process is sweeping at the
/// same moment, or one whose number a live process has since been given, is
/// left where it is.
fn sweep_abandoned_scratch_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let owner = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(SCRATCH_PREFIX))
            .and_then(|pid| pid.parse::<u32>().ok());
        if owner.is_some_and(|pid| !crate::logdir::pid_is_live(pid)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

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
