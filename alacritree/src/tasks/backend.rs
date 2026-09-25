//! The task backends this build knows, dispatched by `match` rather than a
//! vtable.

use alacritree_common::jobs::Blocking;
use alacritree_common::side::Side;
use alacritree_tasks::{Edit, TaskBackend, TaskCommand, TaskError, ambassador_impl_TaskBackend};
use alacritree_taskwarrior::Taskwarrior;
use ambassador::Delegate;

use crate::config::IntegrationsConfig;

#[derive(Debug, Clone, Delegate)]
#[delegate(TaskBackend)]
// One is held per tasks tab and cloned per write, so the size spread between
// variants costs nothing.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Backend {
    Taskwarrior(Taskwarrior),
    Command(TaskCommand),
}

impl Backend {
    /// The configured command, or else taskwarrior, which the hook reads
    /// through even while the tab is off.
    pub(crate) fn from_config(integrations: &IntegrationsConfig) -> Self {
        match &integrations.tasks.command {
            Some(command) => Self::Command(command.clone()),
            None => Self::Taskwarrior(Taskwarrior::default()),
        }
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

    #[test]
    fn a_configured_command_takes_the_place_of_taskwarrior() {
        let mut integrations = IntegrationsConfig::default();
        assert!(matches!(Backend::from_config(&integrations), Backend::Taskwarrior(_)));
        let command = TaskCommand {
            program: alacritree_common::side::Program {
                native: "todo".into(),
                wsl: None,
                name: "todo".into(),
            },
            templates: Default::default(),
            agent_guide: "Use `todo {project}`.".into(),
        };
        integrations.tasks.command = Some(command);
        let backend = Backend::from_config(&integrations);
        assert_eq!(backend.agent_guide("r.main"), "Use `todo r.main`.");
    }
}
