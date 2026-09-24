//! Doppler as a checkout hook. A new worktree gets the main checkout's
//! scopes, and a removed one gives them back. See `scopes` for why doppler
//! needs this at all.

mod scopes;
mod settings;

use alacritree_checkout_hooks::{Checkout, CheckoutHook, Outcome};
use alacritree_common::jobs::Blocking;

pub use settings::{DopplerConfig, RawDoppler, is_set_up};

/// Best-effort throughout. No doppler binary, or nothing to copy, reports
/// nothing rather than an error, as the create flow always has.
#[derive(Debug, Clone, Copy, Default)]
pub struct DopplerHook;

impl DopplerHook {
    fn mirror(event: &Checkout<'_>, blocking: &Blocking) -> Outcome {
        let linked = scopes::mirror_scopes(event.main, event.checkout, blocking);
        Ok((linked > 0).then(|| format!("Linked {linked} Doppler scope(s)")))
    }
}

impl CheckoutHook for DopplerHook {
    fn on_created(&self, event: &Checkout<'_>, blocking: &Blocking) -> Outcome {
        Self::mirror(event, blocking)
    }

    /// Covers worktrees created outside alacritree, which otherwise hit
    /// "Doppler Error: You must specify a project".
    fn on_opened(&self, event: &Checkout<'_>, blocking: &Blocking) -> Outcome {
        Self::mirror(event, blocking)
    }

    fn on_removed(&self, event: &Checkout<'_>, blocking: &Blocking) -> Outcome {
        let dropped = scopes::forget_scopes(event.checkout, blocking);
        Ok((dropped > 0).then(|| format!("Dropped {dropped} Doppler scope(s)")))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use alacritree_checkout_hooks::{Checkout, CheckoutHook};
    use alacritree_common::jobs;
    use alacritree_common::tools::{self, Tool, ToolPaths};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A `doppler` that answers `configure --all --json` from a file and logs
    /// every other invocation, set as the configured doppler path.
    struct FakeDoppler {
        _dir: tempfile::TempDir,
        log: PathBuf,
        _config: std::sync::MutexGuard<'static, ()>,
        restore: [ToolPaths; <Tool as strum::EnumCount>::COUNT],
    }

    impl FakeDoppler {
        fn answering(scopes_json: &str) -> Self {
            let config = tools::test_configuration_lock().lock().unwrap_or_else(|e| e.into_inner());
            let restore = tools::test_configuration();
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("scopes.json");
            let log = dir.path().join("calls.log");
            std::fs::write(&state, scopes_json).unwrap();
            let script = dir.path().join("doppler");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nif [ \"$1 $2\" = \"configure --all\" ]; then cat '{}'; else echo \
                     \"$@\" >> '{}'; fi\n",
                    state.display(),
                    log.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut paths = restore.clone();
            paths[Tool::Doppler as usize] =
                ToolPaths { native: script.to_string_lossy().into_owned(), wsl: None };
            tools::configure(paths);
            Self { _dir: dir, log, _config: config, restore }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for FakeDoppler {
        fn drop(&mut self) {
            tools::configure(self.restore.clone());
        }
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(main.join("apps/web")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let main = main.canonicalize().unwrap();
        let wt = wt.canonicalize().unwrap();
        (tmp, main, wt)
    }

    fn created(main: &Path, wt: &Path) -> alacritree_checkout_hooks::Outcome {
        let e = Checkout { main, checkout: wt };
        jobs::on_this_thread(|b| DopplerHook.on_created(&e, b))
    }

    #[test]
    fn creating_a_worktree_mirrors_each_main_checkout_scope() {
        let (_tmp, main, wt) = dirs();
        let scopes = format!(
            r#"{{"{m}": {{"enclave.project": "api", "enclave.config": "dev"}},
                "{m}/apps/web": {{"enclave.project": "web"}},
                "/elsewhere": {{"enclave.project": "other"}}}}"#,
            m = main.display()
        );
        let doppler = FakeDoppler::answering(&scopes);
        let outcome = created(&main, &wt).expect("doppler hook never errors");
        assert_eq!(outcome.as_deref(), Some("Linked 2 Doppler scope(s)"));
        let mut calls = doppler.calls();
        calls.sort();
        assert_eq!(calls, [
            format!(
                "configure set project=api config=dev --no-check-version --scope {}",
                wt.display()
            ),
            format!(
                "configure set project=web --no-check-version --scope {}/apps/web",
                wt.display()
            ),
        ]);
    }

    #[test]
    fn nothing_to_mirror_reports_nothing() {
        let (_tmp, main, wt) = dirs();
        let _doppler = FakeDoppler::answering("{}");
        assert_eq!(created(&main, &wt).unwrap(), None);
    }

    /// An old CLI or a login banner can put anything on stdout; the hook must
    /// stay silent rather than fail the create.
    #[test]
    fn output_that_is_not_json_reports_nothing() {
        let (_tmp, main, wt) = dirs();
        let doppler = FakeDoppler::answering("Welcome to Doppler!\nnot json");
        assert_eq!(created(&main, &wt).unwrap(), None);
        assert!(doppler.calls().is_empty());
    }

    #[test]
    fn removing_a_worktree_forgets_its_scopes() {
        let (_tmp, main, wt) = dirs();
        let scopes = format!(r#"{{"{}": {{"enclave.project": "api"}}}}"#, wt.display());
        let doppler = FakeDoppler::answering(&scopes);
        let e = Checkout { main: &main, checkout: &wt };
        let outcome = jobs::on_this_thread(|b| DopplerHook.on_removed(&e, b)).unwrap();
        assert_eq!(outcome.as_deref(), Some("Dropped 1 Doppler scope(s)"));
        assert_eq!(doppler.calls(), [format!(
            "configure unset project config --no-check-version --scope {}",
            wt.display()
        )]);
    }

    #[test]
    fn a_missing_doppler_reports_nothing() {
        let (_tmp, main, wt) = dirs();
        let _lock = tools::test_configuration_lock().lock().unwrap_or_else(|e| e.into_inner());
        let restore = tools::test_configuration();
        let mut paths = restore.clone();
        paths[Tool::Doppler as usize] =
            ToolPaths { native: "/nonexistent/doppler".into(), wsl: None };
        tools::configure(paths);
        let outcome = created(&main, &wt);
        tools::configure(restore);
        assert_eq!(outcome.unwrap(), None);
    }
}
