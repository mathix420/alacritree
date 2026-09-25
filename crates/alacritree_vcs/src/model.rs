//! What every backend reports, in words none of them owns: a checkout is a
//! git worktree, a jj workspace or a Mercurial share.

use std::path::PathBuf;

/// The backends, as `state.toml`, menus and wire fields spell them.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::Display,
    strum::EnumIter,
)]
#[strum(serialize_all = "lowercase")]
pub enum VcsKind {
    Git,
    Jj,
    Hg,
}

/// One repository as discovery found it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repository {
    /// Display name of the trunk, e.g. `main`. `None` when nothing resolves.
    pub trunk: Option<String>,
    /// The main checkout first.
    pub checkouts: Vec<Checkout>,
    /// The distro's `$HOME` for a WSL repository. It rides the discovery round
    /// trip because a second `wsl.exe` call costs about 400 ms.
    pub home: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// What the backend accepts back to remove this checkout: a git
    /// worktree's admin name. `main` for the main checkout, which
    /// `list_projects` reports as the worktree's `name`.
    pub name: String,
    pub path: PathBuf,
    pub head: Head,
    pub is_main: bool,
    /// The directory is gone but the backend still records the checkout.
    pub gone: bool,
    pub upstream: Option<UpstreamState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Head {
    /// The branch or bookmark, when one applies. PR lookup and the tasks
    /// scope use this and nothing else.
    pub name: Option<String>,
    /// A short revision id, e.g. 7 hex digits of a git commit.
    pub revision: Option<String>,
    /// Commits from `name` to the head. Git never sets it, since a branch is
    /// its head. A jj bookmark can sit several commits behind `@`.
    pub distance: Option<u32>,
}

impl Head {
    /// `name`, else `revision`: the one field the sidebar, `$branch` and the
    /// IPC `branch` field have always shown.
    pub fn label(&self) -> Option<&str> {
        self.name.as_deref().or(self.revision.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamState {
    Level { upstream: String },
    Diverged { upstream: String, ahead: usize, behind: usize },
    Gone { upstream: String },
    Untracked,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub head: Head,
    pub trunk: Option<String>,
    /// The revision the base diff ran against, e.g.
    /// `refs/remotes/origin/main`. `None` when no base resolved, and
    /// `base_diff` is then empty.
    pub base: Option<String>,
    /// `None` when the backend has no staging area.
    pub staged: Option<Vec<FileChange>>,
    pub working: Vec<FileChange>,
    pub base_diff: Vec<DiffStat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffStat {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

/// What removing a checkout would discard. `staged` is 0 for a backend with
/// no staging area.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dirty {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

impl Dirty {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Present,
    Missing,
    Unknown,
}

/// One filesystem look at a checkout: no process, no library open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub liveness: Liveness,
    /// What `Head::label` would read from disk right now. `None` when it
    /// cannot be read cheaply, and the sidebar then waits for discovery.
    pub head: Option<String>,
}

/// Which checkout and head a directory belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub main: PathBuf,
    pub checkout: PathBuf,
    pub head: Head,
}

/// The base a new checkout starts from, as `prepare_checkout` resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// What steps and errors call it, e.g. `main`.
    pub name: String,
    /// What the backend starts the checkout from, e.g. `origin/main`.
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCheckout {
    pub main: PathBuf,
    /// Chosen by the app from `[workspace]`. The backend creates it.
    pub target: PathBuf,
    /// The new branch.
    pub name: String,
    pub base: Base,
}

/// What `create_checkout` made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Created {
    /// The repository will not list this checkout again, so the app has to
    /// record it. False for every backend that exists today.
    pub record: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveCheckout {
    pub main: PathBuf,
    pub checkout: Checkout,
    /// Remove even with unsaved work. Ignored for a gone checkout.
    pub force: bool,
    /// Also delete `checkout.head.name`.
    pub delete_name: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffScope {
    Staged,
    Working,
    Base { base: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffTarget {
    pub scope: DiffScope,
    /// One file, or the whole scope when `None`.
    pub file: Option<String>,
    /// The file is untracked, which git diffs against `/dev/null`.
    pub untracked: bool,
}

/// The URLs a forge needs to find a branch's pull request: the repository
/// PRs are opened against, and where the branch is pushed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Remotes {
    pub origin_url: Option<String>,
    pub push_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_is_labelled_by_its_name_when_it_has_one() {
        let head =
            Head { name: Some("feature".into()), revision: Some("abc1234".into()), distance: None };
        assert_eq!(head.label(), Some("feature"));
    }

    #[test]
    fn a_detached_head_is_labelled_by_its_revision() {
        let head = Head { name: None, revision: Some("abc1234".into()), distance: None };
        assert_eq!(head.label(), Some("abc1234"));
    }

    #[test]
    fn kinds_spell_as_state_toml_writes_them() {
        assert_eq!(<&str>::from(VcsKind::Git), "git");
        assert_eq!("hg".parse::<VcsKind>().unwrap(), VcsKind::Hg);
    }
}
