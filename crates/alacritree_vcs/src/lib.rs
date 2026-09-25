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
}
