//! The task backends this build knows, dispatched by `match` rather than a
//! vtable.

use alacritree_common::jobs::Blocking;
use alacritree_common::side::Side;
use alacritree_tasks::{Edit, TaskBackend, TaskError, ambassador_impl_TaskBackend};
use alacritree_taskwarrior::Taskwarrior;
use ambassador::Delegate;

#[derive(Debug, Clone, Delegate)]
#[delegate(TaskBackend)]
pub(crate) enum Backend {
    Taskwarrior(Taskwarrior),
}

/// Taskwarrior is the only backend so far. The hook asks it whether or not
/// the tab is on, so there is always one to ask.
impl Default for Backend {
    fn default() -> Self {
        Self::Taskwarrior(Taskwarrior::default())
    }
}

/// Applies `edits` in order and stops at the first that fails, since a later
/// edit may place a task relative to one the failed edit was to move.
pub(crate) fn apply_all(
    backend: &impl TaskBackend,
    side: &Side,
    edits: &[Edit],
    blocking: &Blocking,
) -> Result<(), TaskError> {
    edits.iter().try_for_each(|edit| backend.apply(side, edit, blocking))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritree_common::jobs;
    use alacritree_tasks::fake::{FakeBackend, task};

    #[test]
    fn edits_apply_in_order_and_stop_at_the_first_failure() {
        let store = FakeBackend::with_tasks(vec![task("a", "r")]).refusing();
        let edits = [Edit::Done("a".into()), Edit::Start("a".into())];
        let result = jobs::on_this_thread(|b| apply_all(&store, &Side::Native, &edits, b));
        assert!(result.is_err());
        assert_eq!(store.edits(), [Edit::Done("a".into())]);
    }
}
