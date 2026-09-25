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
    let mut hooks: Vec<Hook> = integrations.doppler.hook().map(Hook::Doppler).into_iter().collect();
    hooks.extend(integrations.checkout_hooks.iter().cloned().map(Hook::Command));
    hooks
}

/// Hand each outcome to `line` as one progress line: a hook's own report at
/// `Info`, or the error that stopped it at `Warn`, so a caller that logs
/// rather than shows the lines still surfaces a failing hook. Hooks with
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
    fn report_forwards_lines_and_names_failures() {
        use alacritree_checkout_hooks::HookError;
        let outcomes = vec![
            Ok(Some("Ran direnv".to_string())),
            Ok(None),
            Err(HookError::Spawn { hook: "mise".into(), source: std::io::Error::other("x") }),
        ];
        let mut lines = Vec::new();
        report(outcomes, |level, l| lines.push((level, l.to_string())));
        assert_eq!(lines, [
            (log::Level::Info, "Ran direnv".to_string()),
            (log::Level::Warn, "Hook failed: could not run mise".to_string()),
        ]);
    }

    fn exiting_with(name: &str, code: u8) -> Hook {
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), format!("exit {code}")])
        } else {
            ("sh", vec!["-c".to_string(), format!("exit {code}")])
        };
        Hook::Command(alacritree_checkout_hooks::CommandHook {
            name: name.into(),
            program: alacritree_common::side::Program {
                native: program.into(),
                wsl: None,
                name: program.into(),
            },
            on_created: args,
            on_opened: Vec::new(),
            on_removed: Vec::new(),
        })
    }

    /// The app's enum reaches each backend through ambassador's cross-crate
    /// delegation, in list order, and a failing hook does not stop the next.
    #[test]
    fn hooks_run_in_order_past_a_failure() {
        use alacritree_checkout_hooks::{CheckoutEvent, CheckoutHooks, HookError};

        let dir = tempfile::tempdir().unwrap();
        let hooks = [exiting_with("broken", 3), exiting_with("after", 0)];
        let event = CheckoutEvent { main: dir.path(), checkout: dir.path() };
        let outcomes = crate::jobs::on_this_thread(|b| hooks[..].created(&event, b));
        assert!(
            matches!(&outcomes[0], Err(HookError::Failed { hook, .. }) if hook == "broken"),
            "{:?}",
            outcomes[0]
        );
        assert_eq!(outcomes[1].as_ref().unwrap(), &Some("Ran after".to_string()));
    }

    #[test]
    fn command_hooks_follow_the_built_in_ones() {
        let integrations = crate::config::IntegrationsConfig {
            checkout_hooks: vec![alacritree_checkout_hooks::CommandHook {
                name: "mise".into(),
                program: alacritree_common::side::Program {
                    native: "mise".into(),
                    wsl: None,
                    name: "mise".into(),
                },
                on_created: vec!["trust".into()],
                on_opened: Vec::new(),
                on_removed: Vec::new(),
            }],
            ..Default::default()
        };
        assert!(matches!(from_config(&integrations)[..], [Hook::Doppler(_), Hook::Command(_)]));
    }
}
