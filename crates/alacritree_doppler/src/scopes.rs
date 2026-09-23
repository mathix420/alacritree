//! Mirror Doppler CLI scopes from a project's main checkout into its git
//! worktrees.
//!
//! Doppler binds project/config to absolute directory paths (`doppler setup`
//! writes them under `scoped:` in `~/.doppler/.doppler.yaml`), so a fresh
//! worktree starts unscoped and `doppler run` fails with "You must specify a
//! project" even though the main checkout is fully set up.  Copying the main
//! checkout's scopes — including per-subdirectory scopes in monorepos — to
//! the equivalent paths inside the worktree makes `doppler run` work there
//! out of the box.  We go through the doppler CLI instead of editing its
//! config file so we never fight its on-disk format.  Everything is
//! best-effort: no doppler binary, or nothing to copy, is a silent no-op.
//!
//! A worktree inside WSL is scoped by the distro's own doppler, under its
//! Linux path: the Windows doppler keeps a separate config file that the
//! distro's `doppler run` never reads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alacritree_common::jobs;
use alacritree_common::side::{self, Program, Ran, Side};
use alacritree_common::tools::{self, Tool};
use alacritree_common::wsl::{self, Location};

/// `enclave.*` is doppler's on-disk spelling of the `project`/`config`
/// options (a leftover from when the product was called Enclave).
const PROJECT_KEY: &str = "enclave.project";
const CONFIG_KEY: &str = "enclave.config";

type Scopes = HashMap<String, HashMap<String, serde_json::Value>>;

/// Copy every scope at or under `main_checkout` to the equivalent path under
/// `worktree`.  Scopes the worktree already defines are left untouched so a
/// deliberate per-worktree `doppler setup` (e.g. pointing at a different
/// config) survives.  Returns how many scopes were written.  Takes
/// `&jobs::Blocking` because it shells out — call it from a pool job, never
/// from the UI thread.
pub(crate) fn mirror_scopes(
    main_checkout: &Path,
    worktree: &Path,
    blocking: &jobs::Blocking,
) -> usize {
    let main_at = locate(main_checkout);
    let wt_at = locate(worktree);
    let side = side_for(&wt_at);
    // A main checkout and a worktree on different sides share no doppler
    // config, so there is nothing to copy between them.
    if side_for(&main_at) != side {
        return 0;
    }
    let main = scope_path(&main_at);
    let worktree = scope_path(&wt_at);
    if main == worktree {
        return 0;
    }
    let Some(scopes) = all_scopes(&side, blocking) else {
        return 0;
    };

    let mut written = 0;
    for (scope, options) in &scopes {
        let Some(project) = options.get(PROJECT_KEY).and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(target) = rebase_scope(scope, &main, &worktree) else {
            continue;
        };
        let already_scoped = scopes
            .get(target.to_string_lossy().as_ref())
            .is_some_and(|o| o.contains_key(PROJECT_KEY) || o.contains_key(CONFIG_KEY));
        if already_scoped {
            continue;
        }

        let project_pair = format!("project={project}");
        let config_pair =
            options.get(CONFIG_KEY).and_then(|v| v.as_str()).map(|c| format!("config={c}"));
        let mut args = vec!["configure", "set", &project_pair];
        if let Some(pair) = &config_pair {
            args.push(pair);
        }
        match run(&side, &args, Some(&target), blocking) {
            Some(_) => written += 1,
            None => log::warn!("doppler: failed to set scope for {}", target.display()),
        }
    }
    written
}

/// Drop the project/config options from every scope at or under `worktree`,
/// so deleting a worktree doesn't grow doppler's config file forever.  Other
/// options (tokens, hosts) are preserved; doppler prunes scope entries that
/// end up empty.  Returns how many scopes were cleaned.  Takes
/// `&jobs::Blocking` because it shells out — call it from a pool job, never
/// from the UI thread.
pub(crate) fn forget_scopes(worktree: &Path, blocking: &jobs::Blocking) -> usize {
    let wt_at = locate(worktree);
    let side = side_for(&wt_at);
    let worktree = scope_path(&wt_at);
    let Some(scopes) = all_scopes(&side, blocking) else {
        return 0;
    };

    let mut cleaned = 0;
    for (scope, options) in &scopes {
        if !Path::new(scope).starts_with(&worktree) {
            continue;
        }
        if !options.contains_key(PROJECT_KEY) && !options.contains_key(CONFIG_KEY) {
            continue;
        }
        let unset = ["configure", "unset", "project", "config"];
        match run(&side, &unset, Some(Path::new(scope)), blocking) {
            Some(_) => cleaned += 1,
            None => log::warn!("doppler: failed to unset scope {scope}"),
        }
    }
    cleaned
}

/// Map a scope path from the main checkout's subtree to the worktree's.
/// Component-wise, so `/repo-other` never matches a `/repo` prefix.
fn rebase_scope(scope: &str, main: &Path, worktree: &Path) -> Option<PathBuf> {
    let rel = Path::new(scope).strip_prefix(main).ok()?;
    if rel.as_os_str().is_empty() { Some(worktree.to_path_buf()) } else { Some(worktree.join(rel)) }
}

/// Where doppler runs for a checkout at `location`.
fn side_for(location: &Location) -> Side {
    Side::from_location(location)
}

/// The path doppler keys a scope by on the checkout's own side.
fn scope_path(location: &Location) -> PathBuf {
    PathBuf::from(side::spelling(location))
}

/// A checkout's location, canonicalized first so a symlinked or relative
/// path names the same scope as the one `doppler setup` wrote.
fn locate(path: &Path) -> Location {
    wsl::classify(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Doppler as configured, plus where the resident helper already found it
/// inside the checkout's distro.
fn doppler_on(side: &Side, blocking: &jobs::Blocking) -> Program {
    let wsl = match side {
        Side::Native => None,
        Side::Wsl { distro } => tools::wsl_located(Tool::Doppler, distro, blocking),
    };
    Program { native: tools::program(Tool::Doppler), wsl, name: Tool::Doppler.name().into() }
}

/// Every scope in doppler's config file on `side`, keyed by absolute
/// directory path.
fn all_scopes(side: &Side, blocking: &jobs::Blocking) -> Option<Scopes> {
    let stdout = run(side, &["configure", "--all", "--json"], None, blocking)?;
    serde_json::from_slice(&stdout).ok()
}

/// Run doppler on `side` with `args`, returning stdout on success and `None`
/// on any failure. That includes the binary not being installed there, which
/// is the common case and must stay quiet.
fn run(
    side: &Side,
    args: &[&str],
    scope: Option<&Path>,
    blocking: &jobs::Blocking,
) -> Option<Vec<u8>> {
    let mut argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    argv.push("--no-check-version".into());
    if let Some(scope) = scope {
        argv.push("--scope".into());
        argv.push(scope.to_string_lossy().into_owned());
    }
    match side::run(side, &doppler_on(side, blocking), None, &argv, blocking) {
        Ok(Ran::Finished(output)) if output.status.success() => Some(output.stdout),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use alacritree_common::wsl::Location;

    use super::*;

    #[test]
    fn rebases_root_scope_to_worktree_root() {
        let target = rebase_scope("/repo", Path::new("/repo"), Path::new("/wt"));
        assert_eq!(target, Some(PathBuf::from("/wt")));
    }

    #[test]
    fn rebases_subdirectory_scopes() {
        let target = rebase_scope("/repo/apps/web", Path::new("/repo"), Path::new("/wt"));
        assert_eq!(target, Some(PathBuf::from("/wt/apps/web")));
    }

    #[test]
    fn ignores_scopes_outside_the_main_checkout() {
        assert_eq!(rebase_scope("/elsewhere", Path::new("/repo"), Path::new("/wt")), None);
        // Sibling with a shared string prefix must not match.
        assert_eq!(rebase_scope("/repo-other", Path::new("/repo"), Path::new("/wt")), None);
    }

    fn in_distro(linux: &str) -> Location {
        Location::Wsl { distro: "Ubuntu".into(), linux_path: linux.into() }
    }

    #[test]
    fn a_wsl_checkout_is_scoped_by_its_linux_path() {
        assert_eq!(scope_path(&in_distro("/home/u/wt")), PathBuf::from("/home/u/wt"));
        assert_eq!(
            scope_path(&Location::Windows(PathBuf::from("/srv/wt"))),
            PathBuf::from("/srv/wt")
        );
    }

    #[test]
    fn a_wsl_checkout_runs_the_distros_doppler() {
        assert_eq!(side_for(&in_distro("/home/u/wt")), Side::Wsl { distro: "Ubuntu".into() });
        assert_eq!(side_for(&Location::Windows(PathBuf::from("C:/wt"))), Side::Native);
    }

    #[test]
    fn scopes_rebase_between_linux_paths() {
        let target = rebase_scope(
            "/home/u/repo/apps/web",
            &scope_path(&in_distro("/home/u/repo")),
            &scope_path(&in_distro("/home/u/wt")),
        );
        assert_eq!(target, Some(PathBuf::from("/home/u/wt/apps/web")));
    }
}
