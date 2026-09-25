//! What a git checkout looks like on disk, read without starting a process or
//! opening the repository.

use std::path::Path;

use alacritree_vcs::{Liveness, Probe};

/// Whether `path` is still a worktree checkout, which is `.git`'s presence
/// rather than the directory's.  `git worktree remove` deletes the contents
/// first and only then the directory itself, so a remove that loses the last
/// step — the usual outcome on Windows, where a shell sitting in the directory
/// pins it — leaves an empty husk behind.  Git calls that worktree gone and
/// refuses to remove it twice ("validation failed: '<path>/.git' does not
/// exist"); stat'ing the directory would call it alive.
///
/// `metadata` rather than `exists` so the difference between "not there" and
/// "could not tell" survives: `exists` folds every error into `false`.
pub(crate) fn presence(path: &Path) -> Liveness {
    match std::fs::metadata(path.join(".git")) {
        Ok(_) => Liveness::Present,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Missing,
        Err(_) => Liveness::Unknown,
    }
}

/// [`presence`], plus the branch the checkout's `HEAD` names, read with two
/// plain file reads because opening a `git2` repository costs far more.
/// `.git` is read as a file first: a linked worktree's names its admin
/// directory, answering both questions at once, and the main checkout's
/// directory fails that read fast.
pub(crate) fn probe_checkout(path: &Path) -> Probe {
    let dot_git = path.join(".git");
    let (head_path, liveness) = match std::fs::read_to_string(&dot_git) {
        Ok(link) => match link.trim().strip_prefix("gitdir:") {
            // `join` keeps an absolute gitdir as is and resolves a relative
            // one against the checkout, the two forms git writes.
            Some(gitdir) => (path.join(gitdir.trim()).join("HEAD"), Some(Liveness::Present)),
            None => return Probe { liveness: Liveness::Present, head: None },
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Probe { liveness: Liveness::Missing, head: None };
        },
        Err(_) => (dot_git.join("HEAD"), None),
    };
    match std::fs::read_to_string(head_path) {
        Ok(head) => {
            Probe { liveness: Liveness::Present, head: head_branch(&head).map(str::to_string) }
        },
        Err(_) => Probe { liveness: liveness.unwrap_or_else(|| presence(path)), head: None },
    }
}

/// The branch a `HEAD` file names, spelled the way discovery records it: the
/// shorthand for a branch, the first seven hex digits of a detached commit.
fn head_branch(head: &str) -> Option<&str> {
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(target) => {
            let target = target.trim();
            Some(target.strip_prefix("refs/heads/").unwrap_or(target))
        },
        None if head.len() >= 40 && head.bytes().all(|b| b.is_ascii_hexdigit()) => head.get(..7),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkout_with_its_git_link_is_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /somewhere/.git/worktrees/x").unwrap();

        assert_eq!(presence(dir.path()), Liveness::Present);
    }

    /// `git worktree remove` deletes the contents and only then the directory,
    /// so on Windows a shell sitting in it leaves this behind.  Git treats the
    /// worktree as gone; stat'ing the directory would not.
    #[test]
    fn the_husk_left_by_a_half_finished_remove_is_missing() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(presence(dir.path()), Liveness::Missing);
    }

    #[test]
    fn a_checkout_deleted_outright_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        drop(dir);

        assert_eq!(presence(&path), Liveness::Missing);
    }

    #[test]
    fn a_linked_checkout_reads_head_through_its_gitdir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::test_support::init_repo(&dir.path().join("main"));
        let linked = crate::test_support::add_worktree(&repo, "topic");

        let found = probe_checkout(&linked);

        assert_eq!(found.liveness, Liveness::Present);
        assert_eq!(found.head.as_deref(), Some("topic"));
    }

    #[test]
    fn a_detached_head_reads_as_discovery_spells_it() {
        let oid = "0123456789abcdef0123456789abcdef01234567\n";
        assert_eq!(head_branch(oid), Some("0123456"));
        assert_eq!(head_branch("ref: refs/heads/feat/x\n"), Some("feat/x"));
        assert_eq!(head_branch("garbage"), None);
    }
}
