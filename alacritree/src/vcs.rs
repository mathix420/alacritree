//! The app's one door to version control: every backend it can drive, which
//! ones config enables, and which one a root belongs to.

use std::path::Path;

use alacritree_git::GitBackend;
use alacritree_vcs::{VersionControl, ambassador_impl_VersionControl};
use ambassador::Delegate;

use crate::config::IntegrationsConfig;

#[derive(Debug, Clone, Delegate)]
#[delegate(VersionControl)]
pub enum Vcs {
    Git(GitBackend),
    #[cfg(test)]
    Fake(alacritree_vcs::fake::FakeVcs),
}

/// The enabled backends, in the order they claim a root. Git comes first so a
/// colocated jj repository opens as git until its user picks otherwise.
// The app reaches backends through `for_path` once status moves behind the trait.
#[allow(dead_code)]
pub(crate) fn backends(integrations: &IntegrationsConfig) -> Vec<Vcs> {
    let mut out = Vec::new();
    if integrations.git.enabled {
        out.push(Vcs::Git(GitBackend::new(&integrations.git)));
    }
    out
}

/// The first backend that claims `root`. A claim may open the repository,
/// so callers run this off the UI thread.
// The app reaches backends through `for_path` once status moves behind the trait.
#[allow(dead_code)]
pub(crate) fn detect(backends: &[Vcs], root: &Path) -> Option<Vcs> {
    backends.iter().find(|vcs| vcs.claims(root)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritree_vcs::VcsKind;
    use alacritree_vcs::fake::FakeVcs;

    #[test]
    fn the_earlier_backend_wins_when_two_claim_a_root() {
        let backends =
            [Vcs::Fake(FakeVcs::new("/a")), Vcs::Fake(FakeVcs::new("/a").with_kind(VcsKind::Jj))];
        assert_eq!(detect(&backends, Path::new("/a")).map(|v| v.kind()), Some(VcsKind::Git));
        assert!(detect(&backends, Path::new("/c")).is_none());
    }

    #[test]
    fn the_git_variant_delegates_to_the_git_crate() {
        let vcs = Vcs::Git(alacritree_git::GitBackend::new(&alacritree_git::GitConfig::default()));
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(vcs.kind(), VcsKind::Git);
        assert!(!vcs.claims(plain.path()));
    }

    #[test]
    fn disabling_git_leaves_no_backend() {
        let mut integrations = crate::config::IntegrationsConfig::default();
        assert_eq!(backends(&integrations).len(), 1);
        integrations.git.enabled = false;
        assert!(backends(&integrations).is_empty());
    }
}
