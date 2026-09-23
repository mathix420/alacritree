//! The checkout hooks this build knows, dispatched by `match` rather than a
//! vtable, and the one place that decides which of them a config turns on.

use alacritree_checkout_hooks::{CheckoutHook, Outcome, ambassador_impl_CheckoutHook};
use alacritree_doppler::DopplerHook;
use ambassador::Delegate;

use crate::config::IntegrationsConfig;

#[derive(Debug, Clone, Delegate)]
#[delegate(CheckoutHook)]
pub(crate) enum Hook {
    Doppler(DopplerHook),
}

/// Built-in hooks first, in a fixed order, so the progress steps read the
/// same on every create.
pub(crate) fn from_config(integrations: &IntegrationsConfig) -> Vec<Hook> {
    let mut hooks = Vec::new();
    if integrations.doppler.enabled {
        hooks.push(Hook::Doppler(DopplerHook));
    }
    hooks
}

/// Hand each outcome to `line` as one progress line: a hook's own report,
/// or the error that stopped it.  Hooks with nothing to say add nothing.
pub(crate) fn report(outcomes: Vec<Outcome>, mut line: impl FnMut(&str)) {
    for outcome in outcomes {
        match outcome {
            Ok(Some(text)) => line(&text),
            Ok(None) => {},
            Err(e) => line(&format!("Hook failed: {e}")),
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
        report(outcomes, |l| lines.push(l.to_string()));
        assert_eq!(lines, ["Linked 2 Doppler scope(s)", "Hook failed: could not run mise"]);
    }
}
