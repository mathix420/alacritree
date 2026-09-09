//! Surface agents running under a herdr server in the sidebar.
//!
//! herdr owns its own PTYs and detects the agent in each pane; alacritree
//! only asks what it has and can hand one to a shell.  Everything here goes
//! through the `herdr` CLI rather than its socket, so a missing binary or an
//! absent server is a silent no-op and no wire protocol is pinned.  herdr
//! prints success on stdout and errors on stderr, which is why callers
//! capture both.

mod cli;
mod model;
mod poll;
mod settings;
mod view;
mod wire;

pub(in crate::herdr) use poll::ListingReply;

pub use cli::{
    HerdrAttachResult, attach_args, attaches_directly, focus_args, focus_pane, focus_pane_args,
    herdr_attach_gesture, running_session_name,
};
pub use model::{
    Agent, HerdrKey, Indicators, Listing, PollError, Settings, Side, Status, match_workspace,
    unattached,
};
pub use poll::{EndpointCache, Endpoints};
pub use settings::settings;
pub use view::{HerdrViewAction, HerdrViewFocus, HerdrViewSync};
pub use wire::error_code;
