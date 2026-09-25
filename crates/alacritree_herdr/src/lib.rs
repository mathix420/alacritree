//! herdr as a multiplexer alacritree hosts panes from.
//!
//! herdr owns its own PTYs and detects the agent in each pane; alacritree
//! only asks what it has and can hand one to a shell.  Commands and listings
//! go through the `herdr` CLI, and changes arrive on an event stream read
//! through herdr's own socket bridge, so a missing binary or an absent server
//! is a silent no-op.  herdr prints success on stdout and errors on stderr,
//! which is why callers capture both.

mod cli;
mod events;
mod herdr_config;
mod host;
mod model;
mod poll;
mod settings;
mod view;
mod wire;

pub use cli::{CallError, Gesture, HerdrError};
use cli::{attaches_directly, focus_pane, program, running_session_name};
use herdr_config::settings;
pub use host::Herdr;
#[cfg(any(test, feature = "test-support"))]
pub use host::{PendingAttach, PendingCreate};
use model::unattached;
pub use model::{Listing, PollError, Settings, pane_key};
use poll::ListingReply;
pub use poll::{EndpointCache, Endpoints};
pub use settings::{AttachMode, FollowFocus, HerdrConfig, RawHerdr};
pub use view::{HerdrViewAction, HerdrViewFocus, HerdrViewSync, ViewInputs};
pub use wire::error_code;

/// herdr's ram, spelled at a private-use codepoint the app's bundled symbol
/// font draws.
pub const DEFAULT_ICON: &str = "\u{10FF00}";

impl From<HerdrError> for alacritree_multiplexer::PaneError {
    fn from(error: HerdrError) -> Self {
        Self::Backend(Box::new(error))
    }
}
