//! herdr as a multiplexer alacritree hosts panes from.
//!
//! herdr owns its own PTYs and detects the agent in each pane; alacritree
//! only asks what it has and can hand one to a shell.  Everything here goes
//! through the `herdr` CLI rather than its socket, so a missing binary or an
//! absent server is a silent no-op and no wire protocol is pinned.  herdr
//! prints success on stdout and errors on stderr, which is why callers
//! capture both.

mod cli;
mod host;
mod model;
mod poll;
mod settings;
mod view;
mod wire;

pub(crate) use host::Herdr;
#[cfg(test)]
pub(crate) use host::{PendingAttach, PendingCreate};
pub(in crate::herdr) use poll::ListingReply;

use cli::{attaches_directly, focus_args, focus_pane, program, running_session_name};
pub(crate) use model::pane_key;
use model::unattached;
pub use model::{Indicators, Listing, PollError, Settings};
pub use poll::{EndpointCache, Endpoints};
use settings::settings;
pub use view::{HerdrViewAction, HerdrViewFocus, HerdrViewSync, ViewInputs};
pub use wire::error_code;
