//! Git behind the version control trait. Native paths go through libgit2,
//! and WSL paths through one batched `sh` script per question, so a status
//! refresh costs one `wsl.exe` round trip rather than one per git command.

mod checkouts;
mod default_branch;
mod diff;
mod discover;
mod liveness;
mod locate;
mod remotes;
mod settings;
mod status;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod upstream;

use std::path::{Path, PathBuf};

use alacritree_common::jobs::Blocking;
use alacritree_vcs::{
    Base, CreateCheckout, Created, DiffTarget, Dirty, Located, Probe, Remotes, RemoveCheckout,
    Repository, Status, VcsError, VcsKind, VersionControl,
};

pub use settings::{GitConfig, RawGit};

/// Holds no state. Every git command finds its program through
/// `alacritree_common::tools`, which `[integrations.git]` configures.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitBackend;

impl VersionControl for GitBackend {
    fn claims(&self, root: &Path) -> bool {
        match alacritree_common::wsl::classify(root) {
            // The batch that answers discovery decides; a second round trip
            // just to pre-check would double the cost of every WSL project.
            alacritree_common::wsl::Location::Wsl { .. } => true,
            alacritree_common::wsl::Location::Windows(_) => git2::Repository::open(root).is_ok(),
        }
    }

    fn kind(&self) -> VcsKind {
        VcsKind::Git
    }

    fn status(
        &self,
        checkout: &Path,
        base_hint: Option<&str>,
        blocking: &Blocking,
    ) -> Result<Status, VcsError> {
        status::status(checkout, base_hint, blocking)
    }

    fn dirty(&self, checkout: &Path, blocking: &Blocking) -> Result<Dirty, VcsError> {
        status::dirty(checkout, blocking)
    }

    fn discover(
        &self,
        root: &Path,
        _: &[PathBuf],
        upstream: bool,
        blocking: &Blocking,
    ) -> Result<Repository, VcsError> {
        discover::discover(root, upstream, blocking)
    }

    fn probe(&self, checkout: &Path) -> Probe {
        liveness::probe_checkout(checkout)
    }

    fn names(&self, checkout: &Path, blocking: &Blocking) -> Result<Vec<String>, VcsError> {
        checkouts::list_branches(checkout, blocking)
    }

    fn validate_name(&self, name: &str) -> Result<(), VcsError> {
        checkouts::validate_branch_name(name).map_err(|e| VcsError::InvalidName(e.to_string()))
    }

    fn prepare_checkout(
        &self,
        main: &Path,
        trunk_hint: Option<&str>,
        on_step: &mut dyn FnMut(&str),
        blocking: &Blocking,
    ) -> Result<Base, VcsError> {
        checkouts::prepare(main, trunk_hint, on_step, blocking)
    }

    fn create_checkout(&self, req: &CreateCheckout, _: &Blocking) -> Result<Created, VcsError> {
        checkouts::create(req)
    }

    fn remove_checkout(&self, req: &RemoveCheckout, _: &Blocking) -> Result<(), VcsError> {
        checkouts::remove(req)
    }

    fn diff_args(&self, target: &DiffTarget) -> Vec<String> {
        diff::diff_args(target)
    }

    fn review_range(&self, base: &str) -> String {
        diff::review_range(base)
    }

    fn remotes(&self, checkout: &Path, name: &str) -> Remotes {
        remotes::remotes(checkout, name)
    }

    fn locate(&self, dir: &Path, blocking: &Blocking) -> Option<Located> {
        locate::locate(dir, blocking)
    }
}
