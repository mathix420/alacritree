//! A backend that answers from a script, for tests of code that drives one
//! without a repository on disk.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alacritree_common::jobs::Blocking;

use crate::{Status, VcsError, VcsKind, VersionControl};

/// Clones share the recorded requests, so a test keeps one clone and hands
/// the other to the code under test.
#[derive(Debug, Clone)]
pub struct FakeVcs {
    root: PathBuf,
    kind: VcsKind,
    calls: Arc<Mutex<Vec<String>>>,
    status: Option<Status>,
}

impl FakeVcs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), kind: VcsKind::Git, calls: Arc::default(), status: None }
    }

    pub fn with_kind(mut self, kind: VcsKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_status(mut self, status: Status) -> Self {
        self.status = Some(status);
        self
    }

    /// Every request, in order, as `method path`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self, method: &str, path: &Path) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{method} {}", path.display()));
    }
}

impl VersionControl for FakeVcs {
    fn claims(&self, root: &Path) -> bool {
        root == self.root
    }

    fn kind(&self) -> VcsKind {
        self.kind
    }

    fn status(&self, checkout: &Path, _: Option<&str>, _: &Blocking) -> Result<Status, VcsError> {
        self.record("status", checkout);
        self.status.clone().ok_or_else(|| VcsError::NotARepository(checkout.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_claims_only_its_root() {
        let fake = FakeVcs::new("/repo");
        assert!(fake.claims(Path::new("/repo")));
        assert!(!fake.claims(Path::new("/elsewhere")));
    }
}
