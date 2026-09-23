//! Steps other tools need when alacritree creates, first opens, or removes a
//! linked worktree.  Several tools bind settings to absolute directory paths
//! (doppler scopes, `mise trust`, `direnv allow`), so a fresh worktree starts
//! without them; each hook carries one such tool's step.

// The trait's signatures are copied verbatim into the app crate by
// ambassador's delegation macro, so they name types by absolute path, and
// this crate must answer to its own name for those paths to resolve here too.
extern crate self as alacritree_checkout_hooks;

use std::path::Path;
use std::process::ExitStatus;

use alacritree_common::jobs::Blocking;

pub mod command;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;

pub use command::{CommandHook, RawCheckoutHooks, RawCommandHook};

/// A worktree event: `checkout` is the linked worktree, `main` the project's
/// main checkout it belongs to.
#[derive(Debug, Clone, Copy)]
pub struct Checkout<'a> {
    pub main: &'a Path,
    pub checkout: &'a Path,
}

/// A line for the progress UI, or nothing when the hook had nothing to do.
pub type Outcome = Result<Option<String>, HookError>;

/// `hook` is the hook's name as configured, not the program it runs: a
/// command hook's table key is what the user can find in their config.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("{hook} failed ({status}){}", if stderr.is_empty() { String::new() } else { format!(": {stderr}") })]
    Failed { hook: String, status: ExitStatus, stderr: String },
    #[error("could not run {hook}")]
    Spawn {
        hook: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{hook} did not finish within {}s", alacritree_common::side::LIMIT.as_secs())]
    TimedOut { hook: String },
}

#[ambassador::delegatable_trait]
pub trait CheckoutHook {
    /// alacritree just created `event.checkout` as a worktree of `event.main`.
    fn on_created(
        &self,
        _event: &::alacritree_checkout_hooks::Checkout<'_>,
        _blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::alacritree_checkout_hooks::Outcome {
        Ok(None)
    }

    /// This process opened its first shell in a linked worktree.  Fires again
    /// after a restart, so implementations must be idempotent.
    fn on_opened(
        &self,
        _event: &::alacritree_checkout_hooks::Checkout<'_>,
        _blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::alacritree_checkout_hooks::Outcome {
        Ok(None)
    }

    /// The worktree at `event.checkout` was removed.  The path was resolved
    /// before git deleted the directory, which cannot be canonicalized after.
    fn on_removed(
        &self,
        _event: &::alacritree_checkout_hooks::Checkout<'_>,
        _blocking: &::alacritree_common::jobs::Blocking,
    ) -> ::alacritree_checkout_hooks::Outcome {
        Ok(None)
    }
}

/// Every event run on each hook in order.  One hook failing does not stop
/// the next: each carries an unrelated tool, and a broken `mise` must not keep
/// doppler from scoping the worktree.
pub trait CheckoutHooks {
    fn created(&self, event: &Checkout<'_>, blocking: &Blocking) -> Vec<Outcome>;
    fn opened(&self, event: &Checkout<'_>, blocking: &Blocking) -> Vec<Outcome>;
    fn removed(&self, event: &Checkout<'_>, blocking: &Blocking) -> Vec<Outcome>;
}

impl<H: CheckoutHook> CheckoutHooks for [H] {
    fn created(&self, event: &Checkout<'_>, blocking: &Blocking) -> Vec<Outcome> {
        self.iter().map(|hook| hook.on_created(event, blocking)).collect()
    }

    fn opened(&self, event: &Checkout<'_>, blocking: &Blocking) -> Vec<Outcome> {
        self.iter().map(|hook| hook.on_opened(event, blocking)).collect()
    }

    fn removed(&self, event: &Checkout<'_>, blocking: &Blocking) -> Vec<Outcome> {
        self.iter().map(|hook| hook.on_removed(event, blocking)).collect()
    }
}

#[cfg(test)]
mod tests {
    /// A program that fails silently still reads as a finished sentence.
    #[test]
    fn a_failure_without_stderr_has_no_dangling_colon() {
        #[cfg(unix)]
        let status = std::os::unix::process::ExitStatusExt::from_raw(2 << 8);
        #[cfg(windows)]
        let status = std::os::windows::process::ExitStatusExt::from_raw(2);
        let failed =
            |stderr: &str| HookError::Failed { hook: "mise".into(), status, stderr: stderr.into() };
        assert_eq!(failed("").to_string(), format!("mise failed ({status})"));
        assert_eq!(failed("no config").to_string(), format!("mise failed ({status}): no config"));
    }

    use super::*;
    use crate::fake::{Event, FakeHook};
    use alacritree_common::jobs;
    use std::path::PathBuf;

    fn event() -> (PathBuf, PathBuf) {
        (PathBuf::from("/repo"), PathBuf::from("/wt"))
    }

    #[test]
    fn every_hook_sees_every_event_in_order() {
        let (main, checkout) = event();
        let hooks = [FakeHook::reporting("first"), FakeHook::reporting("second")];
        let e = Checkout { main: &main, checkout: &checkout };
        let lines: Vec<_> = jobs::on_this_thread(|b| hooks[..].created(&e, b))
            .into_iter()
            .map(|o| o.expect("fake succeeds"))
            .collect();
        assert_eq!(lines, [Some("first".to_string()), Some("second".to_string())]);
        jobs::on_this_thread(|b| hooks[..].removed(&e, b));
        let expected = |make: fn(PathBuf, PathBuf) -> Event| make(main.clone(), checkout.clone());
        for hook in &hooks {
            assert_eq!(hook.events(), [
                expected(|main, checkout| Event::Created { main, checkout }),
                expected(|main, checkout| Event::Removed { main, checkout }),
            ]);
        }
    }

    /// Each hook carries an unrelated tool; a broken one must not keep the
    /// next from running.
    #[test]
    fn a_failing_hook_does_not_stop_the_next() {
        let (main, checkout) = event();
        let hooks = [FakeHook::failing(), FakeHook::reporting("after")];
        let e = Checkout { main: &main, checkout: &checkout };
        let outcomes = jobs::on_this_thread(|b| hooks[..].opened(&e, b));
        assert!(outcomes[0].is_err());
        assert_eq!(outcomes[1].as_ref().expect("second hook ran"), &Some("after".to_string()));
        assert_eq!(hooks[1].events().len(), 1);
    }

    #[test]
    fn an_empty_list_reports_nothing() {
        let (main, checkout) = event();
        let hooks: [FakeHook; 0] = [];
        let e = Checkout { main: &main, checkout: &checkout };
        assert!(jobs::on_this_thread(|b| hooks[..].created(&e, b)).is_empty());
    }

    #[test]
    fn a_spawn_error_names_the_hook() {
        let err = HookError::Spawn { hook: "mise".into(), source: std::io::Error::other("boom") };
        assert_eq!(err.to_string(), "could not run mise");
        assert!(std::error::Error::source(&err).is_some());
    }
}
