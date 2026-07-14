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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::command_ext::CommandExt;

/// `enclave.*` is doppler's on-disk spelling of the `project`/`config`
/// options (a leftover from when the product was called Enclave).
const PROJECT_KEY: &str = "enclave.project";
const CONFIG_KEY: &str = "enclave.config";

type Scopes = HashMap<String, HashMap<String, serde_json::Value>>;

/// Copy every scope at or under `main_checkout` to the equivalent path under
/// `worktree`.  Scopes the worktree already defines are left untouched so a
/// deliberate per-worktree `doppler setup` (e.g. pointing at a different
/// config) survives.  Returns how many scopes were written.
pub fn mirror_scopes(main_checkout: &Path, worktree: &Path) -> usize {
    let Some(scopes) = all_scopes() else {
        return 0;
    };
    let main = canonical(main_checkout);
    let worktree = canonical(worktree);
    if main == worktree {
        return 0;
    }

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
        match run(&args, Some(&target)) {
            Some(_) => written += 1,
            None => log::warn!("doppler: failed to set scope for {}", target.display()),
        }
    }
    written
}

/// Drop the project/config options from every scope at or under `worktree`,
/// so deleting a worktree doesn't grow doppler's config file forever.  Other
/// options (tokens, hosts) are preserved; doppler prunes scope entries that
/// end up empty.  Returns how many scopes were cleaned.
pub fn forget_scopes(worktree: &Path) -> usize {
    let Some(scopes) = all_scopes() else {
        return 0;
    };
    let worktree = canonical(worktree);

    let mut cleaned = 0;
    for (scope, options) in &scopes {
        if !Path::new(scope).starts_with(&worktree) {
            continue;
        }
        if !options.contains_key(PROJECT_KEY) && !options.contains_key(CONFIG_KEY) {
            continue;
        }
        match run(&["configure", "unset", "project", "config"], Some(Path::new(scope))) {
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

/// Every scope in doppler's config file, keyed by absolute directory path.
fn all_scopes() -> Option<Scopes> {
    let stdout = run(&["configure", "--all", "--json"], None)?;
    serde_json::from_slice(&stdout).ok()
}

/// Run doppler with `args`, returning stdout on success and `None` on any
/// failure — including the binary not being installed, which is the common
/// case and must stay quiet.
fn run(args: &[&str], scope: Option<&Path>) -> Option<Vec<u8>> {
    let mut cmd = Command::new("doppler");
    cmd.hide_console()
        .args(args)
        .arg("--no-check-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(scope) = scope {
        cmd.arg("--scope").arg(scope);
    }
    let output = cmd.output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
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
}
