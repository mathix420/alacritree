//! Which terminal multiplexer owns a pane, and what alacritree asks it in
//! order to host one.
//!
//! Everything the app needs from a multiplexer goes behind
//! [`MultiplexerSession`]: listing its panes, attaching to one, creating one,
//! and keeping its focus in step with the session on screen.  Each backend
//! owns its own polling and in-flight calls, so another multiplexer is a new
//! backend crate and a new variant of the app's dispatch enum, with nothing
//! to change in the app's own logic.

// The trait's signatures are copied verbatim into the app crate by
// ambassador's delegation macro, so they name types by absolute path, and
// this crate must answer to its own name for those paths to resolve here too.
extern crate self as alacritree_multiplexer;

mod model;
#[cfg(any(test, feature = "test-support"))]
mod scripted;

use std::path::PathBuf;
use std::sync::OnceLock;

pub use alacritree_common::side::Side;
use alacritree_common::wsl;
use strum::{Display, EnumIter, EnumString, IntoEnumIterator};

pub use self::model::{
    AttachAnswer, AttachFocus, AttachRequest, CreateAnswer, CreateRequest, ListedPane, Managed,
    Pane, PaneKey, PaneStatus, ViewState, ViewStep,
};
#[cfg(any(test, feature = "test-support"))]
pub use self::scripted::{QueuedAttach, QueuedCreate, Scripted};

/// The app's name for one of its sessions.
pub type SessionId = u64;

/// A sidebar workspace: `None` is Home, and `Some` is a worktree's path.
pub type WorkspaceKey = Option<PathBuf>;

/// What a client parked on an attach or a create is eventually told: the
/// pane's description, or why there is none.
pub type Reply = Result<serde_json::Value, String>;

/// Why a multiplexer request produced no pane. The message is what the user
/// and an IPC client read.
#[derive(Debug, thiserror::Error)]
pub enum PaneError {
    #[error("{}", all_disabled_reason())]
    AllDisabled,
    #[error("{}", .0.disabled_reason())]
    Disabled(MultiplexerKind),
    #[error("`{name}` is not a multiplexer, expected {}", known_names())]
    Unknown { name: String },
    #[error("{} has no path inside the {distro} distro", path.display())]
    NoDistroPath { path: PathBuf, distro: String },
    #[error("the {0} attach did not finish")]
    AttachUnfinished(MultiplexerKind),
    #[error("the {0} pane create did not finish")]
    CreateUnfinished(MultiplexerKind),
    /// A backend's own error, which it converts into this one.  Boxed
    /// because this crate cannot name the backends' types.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),
    #[cfg(any(test, feature = "test-support"))]
    #[error("{0}")]
    Scripted(String),
}

impl PaneError {
    /// The backend's own error, when it is an `E`.
    pub fn backend<E: std::error::Error + 'static>(&self) -> Option<&E> {
        match self {
            Self::Backend(error) => error.downcast_ref(),
            _ => None,
        }
    }
}

/// Why a request naming no multiplexer is refused while every one is off.
pub fn all_disabled_reason() -> &'static str {
    static REASON: OnceLock<String> = OnceLock::new();
    REASON.get_or_init(|| {
        let tables: Vec<String> =
            MultiplexerKind::real().map(|kind| format!("[integrations.{kind}]")).collect();
        format!("every multiplexer integration is disabled ({} enabled)", tables.join(" or "))
    })
}

fn known_names() -> String {
    let known: Vec<String> = MultiplexerKind::real().map(|k| format!("`{k}`")).collect();
    known.join(" or ")
}

/// The terminal multiplexers alacritree can host a pane from.  The spelling
/// is the name on the wire and in `[integrations]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum MultiplexerKind {
    Herdr,
    Zellij,
    /// Answers from a script instead of a server, so app behaviour can be
    /// tested at the trait rather than through one multiplexer's wire
    /// format.  Left out of [`MultiplexerKind::real`], so nothing builds it,
    /// no refusal names it and no request reaches it by name.
    #[cfg(any(test, feature = "test-support"))]
    Scripted,
}

impl MultiplexerKind {
    /// Every multiplexer that can front a server, which is what a user may
    /// name and what the app builds.
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn real() -> impl Iterator<Item = Self> {
        Self::iter()
    }

    /// Every multiplexer that can front a server.  `iter` also yields the
    /// scripted one here, and that answers for nothing.
    #[cfg(any(test, feature = "test-support"))]
    pub fn real() -> impl Iterator<Item = Self> {
        Self::iter().filter(|kind| *kind != Self::Scripted)
    }

    /// Why a request that named this multiplexer is refused while it is off.
    /// Alone among the refusals, this one is worth retrying after a config
    /// change.
    pub fn disabled_reason(self) -> String {
        format!("the {self} integration is disabled ([integrations.{self}] enabled)")
    }
}

/// The directory a new pane opens in, spelled where the multiplexer resolves
/// it: the distro's own path on a WSL side, the Windows path on the native
/// one. `None` leaves the choice to the multiplexer. A workspace with no
/// spelling inside the distro is an `Err`, since a pane opened anywhere else
/// would still have its session filed under it.
pub fn cwd_for(
    side: &Side,
    workspace: Option<&std::path::Path>,
) -> Result<Option<String>, PaneError> {
    let Some(path) = workspace else { return Ok(None) };
    match side {
        Side::Native => Ok(Some(path.display().to_string())),
        Side::Wsl(distro) => wsl::windows_to_linux(path).map(Some).ok_or_else(|| {
            PaneError::NoDistroPath { path: path.to_path_buf(), distro: distro.clone() }
        }),
    }
}

/// A pane alacritree wants a session on, in the terms the multiplexer that
/// owns it uses.  `pane_id` is positional and changes when a pane moves,
/// which is why it is not the pane's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTarget {
    pub side: Side,
    pub pane_id: String,
    /// Whether the multiplexer reports an agent in this pane.  Some of them
    /// resolve a target through an agent registry, which holds nothing for a
    /// pane running a plain shell, so such a pane is reached another way.
    pub has_agent: bool,
}

impl PaneTarget {
    /// A pane the listing no longer carries.  Claiming an agent is in it
    /// keeps every caller on the path it took before the pane went.
    pub fn unlisted(key: &PaneKey, pane_id: &str) -> Self {
        Self { side: key.side.clone(), pane_id: pane_id.to_string(), has_agent: true }
    }
}

/// A program and its argv, ready to be a session's shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub argv: Vec<String>,
}

/// A pane a multiplexer has just made.  Every id comes back because each
/// answers a different question: `terminal_id` is the identity a session is
/// keyed on and survives the pane moving, `pane_id` is what an attach is
/// pointed at, and `tab_id` is how a pane with no agent in it is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPane {
    pub terminal_id: String,
    pub pane_id: String,
    pub tab_id: String,
}

/// What the app asks of a multiplexer.  Every process call runs on the job
/// pool from inside the implementation, so none of these blocks a frame.
#[ambassador::delegatable_trait]
pub trait MultiplexerSession {
    /// Whether the user has this multiplexer turned on.  Off stops the
    /// polling too, since the subprocesses are the whole cost.
    fn enabled(&self) -> bool;

    /// The glyph a row names this multiplexer with, as configured.  The app
    /// supplies the fallback for a config that leaves it blank, since only
    /// the app knows which glyphs its bundled font covers.
    fn icon(&self) -> &::alacritree_common::settings::IconStyle;

    /// Refresh the listing on this multiplexer's own clock.  `attached` says
    /// whether a session holds a pane on a side.
    fn poll(&mut self, attached: &dyn Fn(&::alacritree_multiplexer::Side) -> bool);

    /// One number standing for everything the listing draws, so a frame can
    /// tell a change with a `u64` compare.
    fn generation(&self) -> u64;

    /// Every pane the listing carries, in the order it was polled.
    fn panes(&self) -> Vec<(&::alacritree_multiplexer::Side, &::alacritree_multiplexer::Pane)>;

    /// Where a pane sits in `panes`.
    fn pane_index(&self, side: &::alacritree_multiplexer::Side, terminal_id: &str)
    -> Option<usize>;

    fn pane_count(&self) -> usize;

    /// The pane behind `(side, terminal_id)`, while the listing still has it.
    fn find(
        &self,
        side: &::alacritree_multiplexer::Side,
        terminal_id: &str,
    ) -> Option<&::alacritree_multiplexer::Pane>;

    /// What the listing last said about a pane a session holds, and whether
    /// that is still current.  A pane the displayed listing drops is still
    /// described here while it lives.
    fn retained(
        &self,
        side: &::alacritree_multiplexer::Side,
        terminal_id: &str,
    ) -> Option<(&::alacritree_multiplexer::Pane, bool)>;

    /// The panes no session in `claimed` holds, each with the workspace it
    /// belongs under.  A pane matching no workspace is left out unless the
    /// user asked to see those under Home.
    fn listed(
        &self,
        claimed: &[::alacritree_multiplexer::PaneKey],
        workspaces: &[::std::path::PathBuf],
    ) -> Vec<::alacritree_multiplexer::ListedPane<'_>>;

    /// The side a create that named none happens on, when only one server is
    /// answering.  `Err` names every side it could have meant.
    fn default_side(
        &self,
    ) -> Result<::alacritree_multiplexer::Side, ::alacritree_multiplexer::PaneError>;

    /// When the listing last showed `terminal_id` gone from a side that
    /// reported it after `bound_at`.
    fn gone_since(
        &self,
        side: &::alacritree_multiplexer::Side,
        terminal_id: &str,
        bound_at: ::std::time::Instant,
    ) -> Option<::std::time::Instant>;

    /// Where a pane lives, in the fields an attach takes back.
    fn pane_json(
        &self,
        side: &::alacritree_multiplexer::Side,
        terminal_id: &str,
        pane: Option<&::alacritree_multiplexer::Pane>,
    ) -> ::serde_json::Value;

    /// How a row describes a pane on `side`.  `pane` is `None` once the
    /// listing stops carrying it.
    fn managed(
        &self,
        side: &::alacritree_multiplexer::Side,
        pane: Option<&::alacritree_multiplexer::Pane>,
    ) -> ::alacritree_multiplexer::Managed;

    /// Whether opening a pane's row attaches to that pane on its own rather
    /// than sharing the multiplexer's whole view.
    fn attaches_directly(&self, side: &::alacritree_multiplexer::Side, has_agent: bool) -> bool;

    /// The command that opens a session already showing `target`, when this
    /// multiplexer can hand one pane over.  `None` means the pane is reachable
    /// only by sharing the whole view, which `queue_attach` prepares.
    fn open_directly(
        &self,
        target: &::alacritree_multiplexer::PaneTarget,
    ) -> Option<::alacritree_multiplexer::Launch>;

    /// The command that shares the whole view holding `key`'s pane as it
    /// stands, with no focus call first.
    fn shared_view(
        &self,
        key: &::alacritree_multiplexer::PaneKey,
    ) -> Option<::alacritree_multiplexer::Launch>;

    /// Start preparing a shared view of `target`.  A second request for the
    /// same pane joins the first.
    fn queue_attach(
        &mut self,
        key: ::alacritree_multiplexer::PaneKey,
        target: ::alacritree_multiplexer::PaneTarget,
        request: ::alacritree_multiplexer::AttachRequest,
    );

    /// The first queued attach, once it has an answer.  `repaint` is set when
    /// a queued attach still has to start.
    fn poll_attach(&mut self) -> (Option<::alacritree_multiplexer::AttachAnswer>, bool);

    /// Start opening a pane on `side` in `cwd`, spelled in the side's own
    /// terms.
    fn queue_create(
        &mut self,
        side: ::alacritree_multiplexer::Side,
        cwd: Option<String>,
        request: ::alacritree_multiplexer::CreateRequest,
    );

    /// The first queued create, once it has an answer.
    fn poll_create(&mut self) -> Option<::alacritree_multiplexer::CreateAnswer>;

    /// Keep the multiplexer's focus on the session on screen, and report a
    /// move the user made inside the multiplexer for the app to follow.
    fn sync_view(
        &mut self,
        state: ::alacritree_multiplexer::ViewState<'_>,
    ) -> ::alacritree_multiplexer::ViewStep;

    /// A session now shows `key`'s pane after an attach or a follow.
    fn view_attached(
        &mut self,
        id: ::alacritree_multiplexer::SessionId,
        key: &::alacritree_multiplexer::PaneKey,
    );

    /// The app would not follow to `key`, so the same move is not proposed
    /// again.
    fn view_refused(&mut self, key: &::alacritree_multiplexer::PaneKey);

    /// A session closed.  `key` is the pane it held, when this multiplexer
    /// owns it; clients waiting on an attach to that pane are told why.
    fn session_closed(
        &mut self,
        id: ::alacritree_multiplexer::SessionId,
        key: Option<&::alacritree_multiplexer::PaneKey>,
    );
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    /// A client reads a multiplexer's name off a reply and may send it back,
    /// so the two spellings have to agree.  `herdr` is lowercase because that
    /// is the string already on the wire.
    #[test]
    fn a_multiplexer_reads_back_as_the_name_it_spelled() {
        assert_eq!(MultiplexerKind::Herdr.to_string(), "herdr");
        assert_eq!(MultiplexerKind::Zellij.to_string(), "zellij");
        for kind in MultiplexerKind::iter() {
            assert_eq!(MultiplexerKind::from_str(&kind.to_string()), Ok(kind));
        }
    }

    /// The multiplexer resolves the directory where it runs, so a WSL side is
    /// handed the distro's own spelling of the workspace and never the
    /// Windows path the sidebar holds.
    #[cfg(windows)]
    #[test]
    fn a_new_pane_opens_in_the_workspace_spelled_for_its_own_side() {
        let workspace = PathBuf::from(r"\\wsl.localhost\ubuntu\home\dev\repo");
        assert_eq!(
            cwd_for(&Side::Wsl("ubuntu".into()), Some(&workspace)).unwrap(),
            Some("/home/dev/repo".to_string())
        );
        assert_eq!(
            cwd_for(&Side::Native, Some(&workspace)).unwrap(),
            Some(workspace.display().to_string())
        );
    }

    /// The home workspace names no directory, so the multiplexer picks its own
    /// default rather than being handed an empty path.
    #[test]
    fn a_new_pane_in_the_home_workspace_names_no_directory() {
        assert_eq!(cwd_for(&Side::Native, None).unwrap(), None);
        assert_eq!(cwd_for(&Side::Wsl("ubuntu".into()), None).unwrap(), None);
    }

    /// A backend's error reads as the backend wrote it, and the backend can
    /// still tell which of its own errors a refusal carries.
    #[test]
    fn a_backend_error_keeps_its_message_and_its_type() {
        let error = PaneError::Backend(Box::new(std::io::Error::other("no server")));
        assert_eq!(error.to_string(), "no server");
        assert!(error.backend::<std::io::Error>().is_some());
        assert!(PaneError::AllDisabled.backend::<std::io::Error>().is_none());
    }
}
