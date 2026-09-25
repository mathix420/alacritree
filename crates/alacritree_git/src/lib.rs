//! Git behind the version control trait. Native paths go through libgit2,
//! and WSL paths through one batched `sh` script per question, so a status
//! refresh costs one `wsl.exe` round trip rather than one per git command.

mod default_branch;
mod settings;

use std::path::Path;

use alacritree_vcs::{VcsKind, VersionControl};

#[doc(hidden)]
pub use default_branch::{Evidence, WellKnown, resolve, shell_ranking};
pub use settings::{GitConfig, RawGit};

/// The resolved config travels with the value, so a project's `Vcs` is
/// cheap to clone and answers without a config lookup.
#[derive(Debug, Clone)]
pub struct GitBackend {
    // Commands read the program path once status moves behind the trait.
    #[allow(dead_code)]
    config: GitConfig,
}

impl GitBackend {
    pub fn new(config: &GitConfig) -> Self {
        Self { config: config.clone() }
    }
}

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
}
