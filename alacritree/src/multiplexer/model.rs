//! What alacritree means by a pane some multiplexer owns, in terms every
//! multiplexer answers in.

use std::sync::mpsc::Sender;

use super::{MultiplexerKind, PaneTarget, Side};
use crate::ipc::protocol::IpcResult;
use crate::workspace::WorkspaceKey;

/// Identifies one pane across polls.  `terminal_id` is unique only within one
/// server, which is why the side and the multiplexer are part of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneKey {
    pub multiplexer: MultiplexerKind,
    pub side: Side,
    pub terminal_id: String,
}

/// The state a multiplexer reports for the agent in a pane.  An unrecognised
/// reading maps to `Unknown` so a state added later renders as a plain row
/// instead of dropping the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
}

impl PaneStatus {
    /// Word the sidebar paints for this status.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
}

/// One pane as its multiplexer reports it.  `terminal_id` is the identity
/// because `pane_id` is positional: a pane moved between workspaces gets a new
/// one.
#[derive(Debug, Clone)]
pub struct Pane {
    pub terminal_id: String,
    pub pane_id: String,
    /// The tab holding this pane.  A pane with no agent in it is reached
    /// through its tab where the multiplexer resolves targets through an agent
    /// registry.
    pub tab_id: Option<String>,
    pub kind: Option<String>,
    /// The pane's title, with any decorative agent prefix already removed.
    /// Two agents of one kind in one checkout are told apart by this and
    /// nothing else.
    pub title: Option<String>,
    /// The multiplexer's word on the agent in this pane, and `None` when it
    /// found no agent in it at all.  `Some(Unknown)` is the other half of that
    /// distinction: an agent is there and it cannot be classified.
    pub status: Option<PaneStatus>,
    /// The pane the multiplexer's own window is showing.  A shared-view attach
    /// borrows that window rather than one pane, so this is what such a
    /// session has on screen.
    pub focused: bool,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
}

impl Pane {
    /// This pane in the terms a multiplexer answers about, which is every
    /// field of it that decides how the pane is reached rather than drawn.
    pub fn target(&self, side: &Side) -> PaneTarget {
        PaneTarget {
            side: side.clone(),
            pane_id: self.pane_id.clone(),
            tab_id: self.tab_id.clone(),
            has_agent: self.status.is_some(),
        }
    }

    /// The directory the pane is working in, preferring its foreground job's.
    /// A blank report is no report.
    pub fn working_directory(&self) -> Option<&str> {
        self.foreground_cwd
            .as_deref()
            .filter(|cwd| !cwd.trim().is_empty())
            .or_else(|| self.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()))
    }
}

/// A listed pane no session holds, with the workspace it belongs under.
pub(crate) struct ListedPane<'a> {
    pub workspace: WorkspaceKey,
    pub key: PaneKey,
    pub pane: &'a Pane,
}

/// The multiplexer a pane belongs to, as a row describes it.  Named rather
/// than flagged because what a row must say (whose mark to paint, how to get
/// out, whether the attach is exclusive) varies by multiplexer rather than by
/// row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Managed {
    pub multiplexer: MultiplexerKind,
    /// The multiplexer's own detach chord, already rendered.  `None` when its
    /// config could not be read or binds detach to nothing, both of which are
    /// reasons to stay quiet rather than name a chord the user may not have.
    pub detach: Option<String>,
    /// The attach shares the multiplexer's whole view rather than one pane,
    /// so the row says so before a resize reveals it.
    pub shared_view: bool,
    /// The agent kind the multiplexer detected, spelled the way it invokes
    /// it.
    pub kind: Option<String>,
    /// The pane's own title, when it says something the kind does not.
    pub title: Option<String>,
    /// How the multiplexer draws the state it reports.  `None` when it is no
    /// longer reporting one: a pane alacritree still holds open after its
    /// multiplexer stopped listing it.
    pub mark: Option<HarnessMark>,
}

impl Managed {
    /// What the multiplexer calls this pane: the agent kind backquoted as the
    /// command it is, and the title quoted as the words it is.
    pub(crate) fn pane_name(&self) -> Option<String> {
        match (&self.kind, &self.title) {
            (Some(kind), Some(title)) => Some(format!("`{kind}` \"{title}\"")),
            (Some(kind), None) => Some(format!("`{kind}`")),
            (None, Some(title)) => Some(format!("\"{title}\"")),
            (None, None) => None,
        }
    }
}

/// The mark a multiplexer paints for the state it reports, in its own
/// vocabulary.  Resolved once per row, so a pane reads the same whether it is
/// listed or attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HarnessMark {
    pub glyph: &'static str,
    pub tone: StateTone,
    /// The multiplexer's own word for this state, for the hover text.
    pub label: &'static str,
}

/// What a multiplexer means by a state's color.  Named rather than carried as
/// a `Color32` so the palette stays alacritree's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateTone {
    Blocked,
    Working,
    Done,
    Idle,
    /// The multiplexer reported something alacritree does not recognise.
    Unclear,
}

/// Whether opening a pane's session brings it in front of the user.  Every
/// gesture a person makes takes focus; a script attaching in the background
/// leaves it, so the workspace on screen, each workspace's active tab and the
/// multiplexer's own focus all stay where they were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachFocus {
    Take,
    Leave,
}

impl AttachFocus {
    pub(crate) fn requested(no_focus: bool) -> Self {
        if no_focus { Self::Leave } else { Self::Take }
    }

    pub(crate) fn takes(self) -> bool {
        self == Self::Take
    }
}

/// The side of an attach the app keeps: where its session opens, where a
/// refusal hands the user back to, and the clients waiting on it.  A queued
/// attach carries it unread and hands it back with the answer.
pub(crate) struct AttachRequest {
    pub workspace: WorkspaceKey,
    /// Where to hand the user back when the attach fails.  A queued attach
    /// answers frames after the switch, so the caller cannot restore the
    /// workspace itself the way a direct attach lets it.
    pub previous: WorkspaceKey,
    /// Clients parked on this attach.  There is nothing to answer them with
    /// until the queued attach resolves.
    pub waiters: Vec<Sender<IpcResult>>,
    /// Taken when any request merged into this one asked for it.
    pub focus: AttachFocus,
}

/// A queued attach the multiplexer has answered: the command its session
/// runs, or why there is none.
pub(crate) struct AttachAnswer {
    pub key: PaneKey,
    pub request: AttachRequest,
    pub launch: Result<super::Launch, String>,
}

/// The app's side of a pane create.  One waiter, not a list: nothing merges
/// two creates, since the pane they would be merged on has no identity until
/// the multiplexer answers.
pub(crate) struct CreateRequest {
    pub workspace: WorkspaceKey,
    pub waiter: Option<Sender<IpcResult>>,
    pub focus: AttachFocus,
}

/// A create the multiplexer has answered.  The attach it turns into is the
/// ordinary one, so the pane goes back to the app's attach path.
pub(crate) struct CreateAnswer {
    pub side: Side,
    pub request: CreateRequest,
    pub pane: Result<super::CreatedPane, String>,
}

/// What the view sync needs to know about the app this frame.
pub(crate) struct ViewState<'a> {
    /// The active session, the pane it holds when this multiplexer owns one,
    /// and whether that session shares the multiplexer's whole view.
    pub active: Option<(crate::session::SessionId, Option<&'a PaneKey>, bool)>,
    /// The window is focused, the terminal has pane focus, and neither a
    /// modal nor the palette is open.
    pub attentive: bool,
    pub now: std::time::Instant,
    pub last_direct_input: Option<std::time::Instant>,
    /// Whether a session is still open.  A focus call for one that closed
    /// meanwhile has nothing left to report to.
    pub is_open: &'a dyn Fn(crate::session::SessionId) -> bool,
}

/// What the view sync asks of the app after a frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ViewStep {
    /// A pane the user moved to inside the multiplexer, for the app to follow.
    pub follow: Option<PaneKey>,
    pub repaint: bool,
}
