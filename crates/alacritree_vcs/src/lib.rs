//! The version control integration: what alacritree asks of a repository,
//! answered by git today and by jj or Mercurial later.
//!
//! A capability one backend lacks is absent from its answer, never a second
//! trait: the app dispatches through one enum, which cannot ask whether a
//! variant also implements something else. So `Status::staged` is `None` for
//! a backend without a staging area.

// The trait's signatures are copied verbatim into the app crate by
// ambassador's delegation macro, so they name types by absolute path, and
// this crate must answer to its own name for those paths to resolve here too.
extern crate self as alacritree_vcs;

mod model;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

pub use model::*;

#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("failed to run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{command}: {stderr}")]
    Failed { command: String, stderr: String },
    /// Removal refused because it would discard work. The dialog offers to
    /// force it.
    #[error("{message}")]
    Unsaved { message: String },
    #[error("no `{remote}` remote configured")]
    NoRemote { remote: String },
    #[error("could not determine base branch (tried: {})", tried.join(", "))]
    NoBase { tried: Vec<String> },
    #[error("{0}")]
    InvalidName(String),
    /// A path the backend cannot hand to its tool, e.g. one outside the
    /// repository's WSL distro.
    #[error("{0}")]
    BadPath(String),
    #[error("{} is not a repository", .0.display())]
    NotARepository(std::path::PathBuf),
    /// The repository could not be reached, e.g. its WSL distro is down. The
    /// caller keeps what it last knew.
    #[error("{0}")]
    Unreachable(String),
    #[error("{what} cancelled")]
    Cancelled { what: &'static str },
    /// A library error, carried without this crate depending on the library.
    #[error("{context}")]
    Backend {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[ambassador::delegatable_trait]
pub trait VersionControl {
    /// Whether `root` looks like this backend's repository. A cheap pre-check
    /// that may answer true when unsure. `discover` has the final word.
    fn claims(&self, root: &::std::path::Path) -> bool;

    fn kind(&self) -> ::alacritree_vcs::VcsKind;

    /// Must not record history. A backend whose every command writes (jj
    /// snapshots the working copy) reads without writing here.
    fn status(
        &self,
        checkout: &::std::path::Path,
        base_hint: ::std::option::Option<&str>,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<::alacritree_vcs::Status, ::alacritree_vcs::VcsError>;

    /// Cheaper than `status`: no base diff.
    fn dirty(
        &self,
        checkout: &::std::path::Path,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<::alacritree_vcs::Dirty, ::alacritree_vcs::VcsError>;

    /// `recorded` holds checkouts the app recorded because `Created::record`
    /// asked it to. The backend keeps the ones that still belong to `root`.
    /// Git ignores it.
    fn discover(
        &self,
        root: &::std::path::Path,
        recorded: &[::std::path::PathBuf],
        upstream: bool,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<::alacritree_vcs::Repository, ::alacritree_vcs::VcsError>;

    /// Runs on the probe worker for rows the sidebar draws. A filesystem
    /// read, never a process.
    fn probe(&self, checkout: &::std::path::Path) -> ::alacritree_vcs::Probe;

    /// Names the base picker offers, locals first.
    fn names(
        &self,
        checkout: &::std::path::Path,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<::std::vec::Vec<::std::string::String>, ::alacritree_vcs::VcsError>;

    fn validate_name(&self, name: &str) -> ::std::result::Result<(), ::alacritree_vcs::VcsError>;

    /// Resolves the base and fetches it, reporting each step as it starts.
    /// The last report names the step `create_checkout` runs under, since
    /// the app picks the target path inside that step, between the calls.
    fn prepare_checkout(
        &self,
        main: &::std::path::Path,
        trunk_hint: ::std::option::Option<&str>,
        on_step: &mut dyn FnMut(&str),
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<::alacritree_vcs::Base, ::alacritree_vcs::VcsError>;

    /// Creates `req.target` on a new name, starting from `req.base`.
    fn create_checkout(
        &self,
        req: &::alacritree_vcs::CreateCheckout,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<::alacritree_vcs::Created, ::alacritree_vcs::VcsError>;

    /// A live checkout is removed and a gone one is forgotten.
    fn remove_checkout(
        &self,
        req: &::alacritree_vcs::RemoveCheckout,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::result::Result<(), ::alacritree_vcs::VcsError>;

    /// Arguments after the program that print `target` as a unified diff.
    fn diff_args(
        &self,
        target: &::alacritree_vcs::DiffTarget,
    ) -> ::std::vec::Vec<::std::string::String>;

    /// What a Direct viewer template's `{range}` becomes for a base review.
    fn review_range(&self, base: &str) -> ::std::string::String;

    /// The URLs a forge needs for `name`'s pull request. A local config read,
    /// never a network call.
    fn remotes(&self, checkout: &::std::path::Path, name: &str) -> ::alacritree_vcs::Remotes;

    /// Which checkout, and which head, `dir` belongs to, walking up from it.
    fn locate(
        &self,
        dir: &::std::path::Path,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::std::option::Option<::alacritree_vcs::Located>;
}
