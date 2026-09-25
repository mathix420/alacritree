//! A backend that answers from a script, for tests of code that drives one
//! without a repository on disk.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{VcsKind, VersionControl};

/// Clones share the recorded requests, so a test keeps one clone and hands
/// the other to the code under test.
#[derive(Debug, Clone)]
pub struct FakeVcs {
    root: PathBuf,
    kind: VcsKind,
    calls: Arc<Mutex<Vec<String>>>,
}

impl FakeVcs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), kind: VcsKind::Git, calls: Arc::default() }
    }

    pub fn with_kind(mut self, kind: VcsKind) -> Self {
        self.kind = kind;
        self
    }

    /// Every request, in order, as `method path`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    // No trait method takes a path to record yet.
    #[allow(dead_code)]
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
