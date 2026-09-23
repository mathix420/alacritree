//! The checkout hooks this build knows, dispatched by `match` rather than a
//! vtable, and the one place that decides which of them a config turns on.

use alacritree_checkout_hooks::{CheckoutHook, CommandHook, Outcome, ambassador_impl_CheckoutHook};
use alacritree_doppler::DopplerHook;
use ambassador::Delegate;

use crate::config::IntegrationsConfig;

#[derive(Debug, Clone, Delegate)]
#[delegate(CheckoutHook)]
pub(crate) enum Hook {
    Doppler(DopplerHook),
    Command(CommandHook),
}

/// Built-in hooks first, in a fixed order, then the user's in name order, so
/// the progress steps read the same on every create.
pub(crate) fn from_config(integrations: &IntegrationsConfig) -> Vec<Hook> {
    let mut hooks = Vec::new();
    if integrations.doppler.enabled {
        hooks.push(Hook::Doppler(DopplerHook));
    }
    hooks.extend(integrations.checkout_hooks.iter().cloned().map(Hook::Command));
    hooks
}

/// Hand each outcome to `line` as one progress line: a hook's own report at
/// `Info`, or the error that stopped it at `Warn`, so a caller that logs
/// rather than shows the lines still surfaces a failing hook.  Hooks with
/// nothing to say add nothing.
pub(crate) fn report(outcomes: Vec<Outcome>, mut line: impl FnMut(log::Level, &str)) {
    for outcome in outcomes {
        match outcome {
            Ok(Some(text)) => line(log::Level::Info, &text),
            Ok(None) => {},
            Err(e) => line(log::Level::Warn, &format!("Hook failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doppler_joins_the_list_only_when_enabled() {
        let mut integrations = crate::config::IntegrationsConfig::default();
        assert!(matches!(from_config(&integrations)[..], [Hook::Doppler(_)]));
        integrations.doppler.enabled = false;
        assert!(from_config(&integrations).is_empty());
    }

    #[test]
    fn report_forwards_lines_and_names_failures() {
        use alacritree_checkout_hooks::HookError;
        let outcomes = vec![
            Ok(Some("Linked 2 Doppler scope(s)".to_string())),
            Ok(None),
            Err(HookError::Spawn { hook: "mise".into(), source: std::io::Error::other("x") }),
        ];
        let mut lines = Vec::new();
        report(outcomes, |level, l| lines.push((level, l.to_string())));
        assert_eq!(lines, [
            (log::Level::Info, "Linked 2 Doppler scope(s)".to_string()),
            (log::Level::Warn, "Hook failed: could not run mise".to_string()),
        ]);
    }

    #[test]
    fn command_hooks_follow_the_built_in_ones() {
        let mut integrations = crate::config::IntegrationsConfig::default();
        integrations.checkout_hooks = vec![alacritree_checkout_hooks::CommandHook {
            name: "mise".into(),
            program: alacritree_common::side::Program {
                native: "mise".into(),
                wsl: None,
                name: "mise".into(),
            },
            on_created: vec!["trust".into()],
            on_opened: Vec::new(),
            on_removed: Vec::new(),
        }];
        assert!(matches!(from_config(&integrations)[..], [Hook::Doppler(_), Hook::Command(_)]));
    }
}
