//! The service that hosts a repository's pull requests. The sidebar paints a
//! branch's PR state, and the git panel diffs against its base branch rather
//! than the repository's default.

// The trait's signatures are copied verbatim into the app crate by
// ambassador's delegation macro, so they name types by absolute path, and
// this crate must answer to its own name for those paths to resolve here too.
extern crate self as alacritree_forge;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

/// A pull request's lifecycle, folded to what the sidebar paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrInfo {
    pub number: u64,
    /// The branch the pull request merges into, which is what the git panel
    /// diffs against.
    pub base_branch: String,
    pub url: String,
    pub state: PrState,
}

/// A checkout and the branch it has out, which is what a pull request is
/// looked up by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub path: PathBuf,
    pub branch: String,
    /// Read by the checkout's version control before the lookup. `None` when
    /// it could not be read from here, such as inside a WSL distro.
    pub remotes: Option<alacritree_vcs::Remotes>,
}

/// One answer per checkout asked about, keyed by path because two
/// repositories can hold the same branch name. `Ok(None)` is a real answer,
/// the branch has no pull request, and a checkout missing from the map reads
/// the same way.
pub type PullRequests = HashMap<PathBuf, Result<Option<PrInfo>, ForgeError>>;

/// `program` is the forge's command line client, as its tool table names it.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("could not run {program}")]
    Spawn {
        program: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} failed ({status})")]
    Failed { program: &'static str, status: ExitStatus },
    #[error("{program} answered with something other than a pull request list")]
    Malformed { program: &'static str },
    #[error("could not run {program} inside WSL")]
    Wsl {
        program: &'static str,
        #[source]
        source: alacritree_common::wsl::BatchError,
    },
}

#[ambassador::delegatable_trait]
pub trait RemoteForge {
    /// The pull request each of `heads` has. Blocks on the network, so it
    /// runs on a pool worker, and one call covers a whole frame's worth of
    /// lookups so a backend can batch those that share a repository.
    fn pull_requests(
        &self,
        heads: Vec<::alacritree_forge::Head>,
        blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::alacritree_forge::PullRequests;
}
