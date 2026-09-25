//! The forges this build knows, dispatched by `match` rather than a vtable.

use alacritree_forge::{RemoteForge, ambassador_impl_RemoteForge};
use alacritree_gh::GhForge;
use ambassador::Delegate;

#[derive(Debug, Clone, Delegate)]
#[delegate(RemoteForge)]
pub(crate) enum Forge {
    Gh(GhForge),
}

/// gh is the only forge so far. The git panel's diff base asks it whether
/// or not `pr_status` is on, so there is always one to ask.
impl Default for Forge {
    fn default() -> Self {
        Self::Gh(GhForge)
    }
}
