//! A backend that answers from a script, for tests of code that drives one
//! without a repository on disk.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alacritree_common::jobs::Blocking;

use crate::{
    Base, CreateCheckout, Created, DiffTarget, Dirty, Liveness, Probe, RemoveCheckout, Repository,
    Status, VcsError, VcsKind, VersionControl,
};

/// Clones share the recorded requests, so a test keeps one clone and hands
/// the other to the code under test.
#[derive(Debug, Clone)]
pub struct FakeVcs {
    root: PathBuf,
    kind: VcsKind,
    calls: Arc<Mutex<Vec<String>>>,
    status: Option<Status>,
    dirty: Dirty,
    repository: Option<Repository>,
    probe: Option<Probe>,
    names: Vec<String>,
    refuse_prepare: bool,
    refuse_removal: bool,
    range: String,
}

impl FakeVcs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            kind: VcsKind::Git,
            calls: Arc::default(),
            status: None,
            dirty: Dirty::default(),
            repository: None,
            probe: None,
            names: Vec::new(),
            refuse_prepare: false,
            refuse_removal: false,
            range: String::new(),
        }
    }

    pub fn with_kind(mut self, kind: VcsKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_status(mut self, status: Status) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_dirty(mut self, dirty: Dirty) -> Self {
        self.dirty = dirty;
        self
    }

    pub fn with_repository(mut self, repository: Repository) -> Self {
        self.repository = Some(repository);
        self
    }

    pub fn with_probe(mut self, probe: Probe) -> Self {
        self.probe = Some(probe);
        self
    }

    pub fn with_names(mut self, names: Vec<String>) -> Self {
        self.names = names;
        self
    }

    /// `prepare_checkout` answers that no `origin` remote exists.
    pub fn refusing_prepare(mut self) -> Self {
        self.refuse_prepare = true;
        self
    }

    /// What `review_range` answers for every base.
    pub fn with_range(mut self, range: &str) -> Self {
        self.range = range.to_string();
        self
    }

    /// `remove_checkout` answers that the checkout holds unsaved work.
    pub fn refusing_removal(mut self) -> Self {
        self.refuse_removal = true;
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

    fn dirty(&self, checkout: &Path, _: &Blocking) -> Result<Dirty, VcsError> {
        self.record("dirty", checkout);
        Ok(self.dirty)
    }

    fn discover(
        &self,
        root: &Path,
        _: &[PathBuf],
        _: bool,
        _: &Blocking,
    ) -> Result<Repository, VcsError> {
        self.record("discover", root);
        self.repository.clone().ok_or_else(|| VcsError::NotARepository(root.to_path_buf()))
    }

    fn probe(&self, checkout: &Path) -> Probe {
        self.record("probe", checkout);
        self.probe.clone().unwrap_or(Probe { liveness: Liveness::Present, head: None })
    }

    fn names(&self, checkout: &Path, _: &Blocking) -> Result<Vec<String>, VcsError> {
        self.record("names", checkout);
        Ok(self.names.clone())
    }

    fn validate_name(&self, _: &str) -> Result<(), VcsError> {
        Ok(())
    }

    fn prepare_checkout(
        &self,
        main: &Path,
        _: Option<&str>,
        on_step: &mut dyn FnMut(&str),
        _: &Blocking,
    ) -> Result<Base, VcsError> {
        self.record("prepare", main);
        if self.refuse_prepare {
            return Err(VcsError::NoRemote { remote: "origin".into() });
        }
        on_step("Preparing");
        Ok(Base { name: "main".into(), revision: "origin/main".into() })
    }

    /// Creates the directory, so the app's steps that copy into it can run.
    fn create_checkout(&self, req: &CreateCheckout, _: &Blocking) -> Result<Created, VcsError> {
        self.record("create", &req.target);
        std::fs::create_dir_all(&req.target).map_err(|e| VcsError::Backend {
            context: format!("could not create {}", req.target.display()),
            source: Box::new(e),
        })?;
        Ok(Created::default())
    }

    fn remove_checkout(&self, req: &RemoveCheckout, _: &Blocking) -> Result<(), VcsError> {
        self.record("remove", &req.checkout.path);
        if self.refuse_removal {
            return Err(VcsError::Unsaved { message: "the checkout holds unsaved work".into() });
        }
        Ok(())
    }

    fn diff_args(&self, _: &DiffTarget) -> Vec<String> {
        vec!["diff".into()]
    }

    fn review_range(&self, _: &str) -> String {
        self.range.clone()
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
