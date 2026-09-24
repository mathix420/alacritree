//! Hooks the user defines. Each is a program and one argv template per
//! event, in the same shape as the custom diff viewer.

use std::collections::BTreeMap;

use alacritree_common::jobs::Blocking;
use alacritree_common::side::{self, Program, Ran, Side};
use alacritree_common::wsl;
use serde::Deserialize;

use crate::{Checkout, CheckoutHook, HookError, Outcome};

/// `[integrations.checkout_hooks]`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RawCheckoutHooks {
    /// Programs to run when a worktree is created, first opened, or removed,
    /// keyed by a name of your choice. A table rather than a list, so a hook
    /// defined in alacritty.toml can be changed or disabled by name from
    /// alacritree.toml.
    pub command: BTreeMap<String, RawCommandHook>,
}

/// One hook: the program to run, and its arguments for each worktree event.
// `path` is required, so defaults are per field rather than from a `Default`
// impl.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RawCommandHook {
    /// Run this hook.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// The program to run on Windows or natively. A bare name is looked up
    /// on PATH.
    pub path: String,
    /// The program to run inside a WSL distro for a worktree there, as
    /// written. Empty looks up the file name of `path`, without directory or
    /// extension, through the distro's login shell, and a distro where that
    /// finds nothing skips the hook. A Windows `path` is never run there.
    #[serde(default)]
    pub wsl_path: String,
    /// Arguments when alacritree creates a worktree. `{checkout}` is the new
    /// worktree and `{main}` the project's main checkout. Empty skips it.
    #[serde(default)]
    pub on_created: Vec<String>,
    /// Arguments the first time this process opens a shell in a worktree,
    /// including ones created outside alacritree. Runs again after a restart,
    /// so the command must be safe to repeat. Empty skips it.
    #[serde(default)]
    pub on_opened: Vec<String>,
    /// Arguments after a worktree is removed. Runs in the main checkout,
    /// since the worktree is gone. Empty skips it.
    #[serde(default)]
    pub on_removed: Vec<String>,
}

fn enabled_by_default() -> bool {
    true
}

/// The name a distro's login shell finds `path` by. A native path, possibly
/// a Windows one, means nothing inside the distro.
fn lookup_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map_or_else(|| path.to_string(), |stem| stem.to_string_lossy().into_owned())
}

impl RawCheckoutHooks {
    /// Enabled hooks only, in name order, so the progress steps read the
    /// same on every create.
    pub fn resolve(self) -> Vec<CommandHook> {
        self.command
            .into_iter()
            .filter(|(_, raw)| raw.enabled)
            .map(|(name, raw)| CommandHook {
                name,
                program: Program {
                    name: lookup_name(&raw.path),
                    native: raw.path,
                    wsl: Some(raw.wsl_path).filter(|p| !p.trim().is_empty()),
                },
                on_created: raw.on_created,
                on_opened: raw.on_opened,
                on_removed: raw.on_removed,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CommandHook {
    pub name: String,
    pub program: Program,
    pub on_created: Vec<String>,
    pub on_opened: Vec<String>,
    pub on_removed: Vec<String>,
}

/// Fill `{checkout}` and `{main}` with each path as the checkout's side
/// spells it. Each template word stays one argument.
fn expand(template: &[String], event: &Checkout<'_>) -> Vec<String> {
    let spell = |path| {
        let spelled = side::spelling(&wsl::classify(path));
        if cfg!(windows) { without_verbatim(&spelled) } else { spelled }
    };
    let checkout = spell(event.checkout);
    let main = spell(event.main);
    template
        .iter()
        .map(|word| word.replace("{checkout}", &checkout).replace("{main}", &main))
        .collect()
}

/// Removal is handed a canonicalized path, which on Windows carries a `\\?\`
/// prefix that the other events' paths lack and many programs reject.
fn without_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    }
}

fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).lines().next().unwrap_or_default().trim().to_string()
}

impl CommandHook {
    fn run(
        &self,
        template: &[String],
        event: &Checkout<'_>,
        cwd: &std::path::Path,
        blocking: &Blocking,
    ) -> Outcome {
        if template.is_empty() {
            return Ok(None);
        }
        let side = Side::of(event.checkout);
        let cwd_spelled = std::path::PathBuf::from(side::spelling(&wsl::classify(cwd)));
        let cwd = if side == Side::Native { cwd } else { cwd_spelled.as_path() };
        self.outcome(side::run(&side, &self.program, Some(cwd), &expand(template, event), blocking))
    }

    fn outcome(&self, ran: std::io::Result<Ran>) -> Outcome {
        match ran {
            Err(source) => Err(HookError::Spawn { hook: self.name.clone(), source }),
            Ok(Ran::Missing) => {
                log::debug!(
                    "checkout hook {}: {} is not installed on this side",
                    self.name,
                    self.program.name
                );
                Ok(None)
            },
            Ok(Ran::TimedOut) => Err(HookError::TimedOut { hook: self.name.clone() }),
            Ok(Ran::Finished(output)) if output.status.success() => {
                Ok(Some(format!("Ran {}", self.name)))
            },
            Ok(Ran::Finished(output)) => Err(HookError::Failed {
                hook: self.name.clone(),
                status: output.status,
                stderr: first_line(&output.stderr),
            }),
        }
    }
}

impl CheckoutHook for CommandHook {
    fn on_created(&self, event: &Checkout<'_>, blocking: &Blocking) -> Outcome {
        self.run(&self.on_created, event, event.checkout, blocking)
    }

    fn on_opened(&self, event: &Checkout<'_>, blocking: &Blocking) -> Outcome {
        self.run(&self.on_opened, event, event.checkout, blocking)
    }

    /// The worktree is gone, so the command runs in the main checkout.
    fn on_removed(&self, event: &Checkout<'_>, blocking: &Blocking) -> Outcome {
        self.run(&self.on_removed, event, event.main, blocking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritree_common::jobs;
    use std::path::Path;

    fn hook(program: &str, on_created: &[&str]) -> CommandHook {
        CommandHook {
            name: "test".into(),
            program: Program { native: program.into(), wsl: None, name: program.into() },
            on_created: on_created.iter().map(|s| s.to_string()).collect(),
            on_opened: Vec::new(),
            on_removed: Vec::new(),
        }
    }

    fn created(hook: &CommandHook, main: &Path, checkout: &Path) -> Outcome {
        let e = Checkout { main, checkout };
        jobs::on_this_thread(|b| hook.on_created(&e, b))
    }

    /// A path with spaces or quotes must stay one argument. The program runs
    /// directly, never through a shell that would split it.
    #[test]
    fn placeholders_expand_to_exactly_one_argument_each() {
        let e = Checkout { main: Path::new("/src/my repo"), checkout: Path::new("/wt/it's ü") };
        let args = expand(&["trust".into(), "{checkout}".into(), "--from={main}".into()], &e);
        assert_eq!(args, ["trust", "/wt/it's ü", "--from=/src/my repo"]);
    }

    #[test]
    fn an_empty_template_skips_the_event() {
        let h = hook("alacritree-no-such-program", &[]);
        assert_eq!(created(&h, Path::new("/m"), Path::new("/c")).unwrap(), None);
    }

    #[test]
    fn a_missing_program_is_skipped() {
        let h = hook("alacritree-no-such-program", &["x"]);
        assert_eq!(created(&h, Path::new("/m"), Path::new("/c")).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn success_reports_the_hook_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("seen");
        let h = hook("sh", &[
            "-c",
            &format!("printf %s \"$1\" > '{}'", out.display()),
            "sh",
            "{checkout}",
        ]);
        let outcome = created(&h, Path::new("/m"), tmp.path()).unwrap();
        assert_eq!(outcome.as_deref(), Some("Ran test"));
        assert_eq!(std::fs::read_to_string(out).unwrap(), tmp.path().to_string_lossy());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_zero_exit_fails_with_the_first_stderr_line() {
        let tmp = tempfile::tempdir().unwrap();
        let h = hook("sh", &["-c", "echo 'not trusted' >&2; echo more >&2; exit 2"]);
        let err = created(&h, Path::new("/m"), tmp.path()).unwrap_err();
        match err {
            HookError::Failed { hook, status, stderr } => {
                assert_eq!(hook, "test");
                assert_eq!(status.code(), Some(2));
                assert_eq!(stderr, "not trusted");
            },
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A hook killed for running too long says so, rather than reading as a
    /// program that could not start.
    #[test]
    fn a_hook_past_its_limit_fails_as_timed_out() {
        let err = hook("sleep", &["30"]).outcome(Ok(Ran::TimedOut)).unwrap_err();
        assert!(matches!(err, HookError::TimedOut { ref hook } if hook == "test"), "{err:?}");
        assert_eq!(err.to_string(), "test did not finish within 300s");
    }

    /// Removal hands over the path Windows canonicalized, which carries a
    /// verbatim prefix that create and open never show and that many
    /// programs reject.
    #[test]
    fn verbatim_prefixes_are_dropped() {
        assert_eq!(without_verbatim(r"\\?\C:\src\wt"), r"C:\src\wt");
        assert_eq!(without_verbatim(r"\\?\UNC\server\share\wt"), r"\\server\share\wt");
        assert_eq!(without_verbatim(r"C:\src\wt"), r"C:\src\wt");
        assert_eq!(without_verbatim("/home/u/wt"), "/home/u/wt");
    }

    #[test]
    fn resolve_keeps_enabled_hooks_sorted_by_name() {
        let raw: RawCheckoutHooks = toml::from_str(
            r#"
            [command.zeta]
            path = "z"
            [command.alpha]
            path = "a"
            on_created = ["{checkout}"]
            [command.off]
            path = "o"
            enabled = false
            "#,
        )
        .unwrap();
        let hooks = raw.resolve();
        let names: Vec<_> = hooks.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert_eq!(hooks[0].program, Program { native: "a".into(), wsl: None, name: "a".into() });
        assert_eq!(hooks[0].on_created, ["{checkout}"]);
    }

    /// Inside a distro the login shell looks the program up by name; a
    /// Windows path handed to it would never be found.
    #[test]
    fn a_path_is_looked_up_in_a_distro_by_its_file_stem() {
        let raw: RawCheckoutHooks =
            toml::from_str("[command.mise]\npath = '/opt/tools/mise.exe'").unwrap();
        assert_eq!(raw.resolve()[0].program.name, "mise");
    }

    #[test]
    fn a_hook_without_a_path_is_a_parse_error() {
        assert!(toml::from_str::<RawCheckoutHooks>("[command.x]\non_created = []").is_err());
    }
}
