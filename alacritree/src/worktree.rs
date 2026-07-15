//! Create and delete git worktrees on a background thread, streaming progress
//! back to the UI via an `mpsc` channel.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::command_ext::CommandExt;
use crate::wsl;

#[derive(Debug, Clone)]
pub enum Progress {
    Step(String),
    Done(Result<PathBuf, String>),
}

pub struct CreateRequest {
    pub project_root: PathBuf,
    pub default_branch: Option<String>,
    pub branch: String,
    /// Base directory to create the worktree under; `None` uses the built-in
    /// `~/.alacritree/worktrees` default.
    pub base_dir: Option<PathBuf>,
}

/// git-check-ref-format rules, abridged: no whitespace/control chars, no
/// `..`, `~`, `^`, `:`, `?`, `*`, `[`, `\`, `@{`; can't start with `-` or `.`,
/// or end with `.` or `.lock`.
pub fn validate_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Branch name is empty.".into());
    }
    if name.starts_with('-') {
        return Err("Branch name cannot start with `-`.".into());
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err("Branch name cannot start or end with `.`.".into());
    }
    if name.ends_with(".lock") {
        return Err("Branch name cannot end with `.lock`.".into());
    }
    if name.contains("..") || name.contains("@{") {
        return Err("Branch name cannot contain `..` or `@{`.".into());
    }
    for c in name.chars() {
        if c.is_whitespace() {
            return Err("Branch name cannot contain whitespace.".into());
        }
        if (c as u32) < 0x20 || c == '\u{7f}' {
            return Err("Branch name contains a control character.".into());
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Err(format!("Branch name cannot contain `{c}`."));
        }
    }
    Ok(())
}

/// Run [`create`] on a background thread, waking the UI for each step.
pub fn spawn_create(req: CreateRequest, ctx: egui::Context) -> Receiver<Progress> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = create(&req, |step| {
            let _ = tx.send(Progress::Step(step.to_string()));
            ctx.request_repaint();
        });
        let _ = tx.send(Progress::Done(result));
        ctx.request_repaint();
    });
    rx
}

/// Create the worktree on the calling thread, reporting each step as it starts.
///
/// Nothing here needs a window, so callers without one (the CLI, with no
/// running app to talk to) drive this directly rather than through
/// [`spawn_create`].
pub fn create(req: &CreateRequest, mut on_step: impl FnMut(&str)) -> Result<PathBuf, String> {
    let send = &mut on_step;

    send("Syncing with remote…");
    if !has_remote(&req.project_root, "origin") {
        return Err("no `origin` remote configured".into());
    }

    // The cached `default_branch` is a hint; if it's missing or stale (e.g.
    // user has a global `init.defaultBranch=master` but the repo's actual
    // default is `main`), ask origin what its HEAD really points to.
    let (base, base_ref) = resolve_base_branch(&req.project_root, req.default_branch.as_deref())
        .map_err(|attempts| {
            format!("could not determine base branch (tried: {})", attempts.join(", "))
        })?;
    send(&format!("Verifying base branch `{base}`"));

    send("Fetching latest changes…");
    run_git(&req.project_root, &["fetch", "origin", &base])?;

    send("Creating git worktree…");
    let target = pick_worktree_path(&req.project_root, &req.branch, req.base_dir.as_deref())?;
    let target_arg = git_path_arg(&req.project_root, &target)?;
    run_git(&req.project_root, &["worktree", "add", &target_arg, "-b", &req.branch, &base_ref])?;

    send("Copying LLM configurations…");
    let copied = copy_llm_configs(&req.project_root, &target);
    if copied > 0 {
        send(&format!("Copied {copied} LLM config item(s)"));
    }

    // Pre-flip Claude Code's BEL setting so the user doesn't have to
    // configure each worktree by hand.  Other keys in the file are preserved.
    if let Err(e) = enable_claude_terminal_bell(&target) {
        log::warn!("failed to write Claude bell config in {}: {e}", target.display());
    } else {
        send("Enabled Claude Code terminal bell");
    }

    let linked = crate::doppler::mirror_scopes(&req.project_root, &target);
    if linked > 0 {
        send(&format!("Linked {linked} Doppler scope(s)"));
    }

    Ok(target)
}

fn enable_claude_terminal_bell(worktree_root: &Path) -> std::io::Result<()> {
    let dir = worktree_root.join(".claude");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.local.json");

    let mut value: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };
    if !value.is_object() {
        value = serde_json::json!({});
    }
    value["preferredNotifChannel"] = serde_json::json!("terminal_bell");

    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, pretty)
}

/// `git` primed to run against `cwd`'s repo: `git -C <cwd>` for Windows
/// paths, the same command inside the owning distro for WSL paths.  Path
/// *arguments* for WSL repos must already be Linux paths (`git_path_arg`).
fn git_command(cwd: &Path) -> Command {
    match wsl::classify(cwd) {
        wsl::Location::Windows(path) => {
            let mut cmd = Command::new("git");
            cmd.hide_console().arg("-C").arg(path);
            cmd
        },
        wsl::Location::Wsl { distro, linux_path } => {
            let mut cmd = wsl::command(&distro, None);
            cmd.arg("git").arg("-C").arg(linux_path);
            cmd
        },
    }
}

/// The form of `path` git receives as an argument: Linux for WSL repos
/// (in-distro git can't resolve UNC paths), the Windows string otherwise.
fn git_path_arg(repo: &Path, path: &Path) -> Result<String, String> {
    match wsl::classify(repo) {
        wsl::Location::Windows(_) => Ok(path.to_str().ok_or("invalid worktree path")?.to_string()),
        wsl::Location::Wsl { .. } => wsl::windows_to_linux(path)
            .ok_or_else(|| "worktree path is outside the distro".to_string()),
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = git_command(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let msg = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
    Err(format!("git {}: {msg}", args.join(" ")))
}

fn has_remote(cwd: &Path, name: &str) -> bool {
    git_command(cwd)
        .args(["remote", "get-url", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve the base branch dynamically.  Asks origin first via
/// `git ls-remote --symref HEAD` — the only source that reflects the
/// upstream's *current* default branch.  The caller's hint comes from
/// `refs/remotes/origin/HEAD`, which can lag if the upstream default was
/// renamed since the last sync; trusting it would feed a defunct branch
/// name to `git fetch`.  Falls back to the hint and then to common names
/// when the remote is unreachable.  Returns `(branch_name, ref_to_use)`
/// where `ref_to_use` is what `git worktree add -b … <ref>` should branch
/// from (prefer `origin/<branch>` so we start from the fetched remote tip).
/// On total failure, returns the list of names we tried.
fn resolve_base_branch(cwd: &Path, hint: Option<&str>) -> Result<(String, String), Vec<String>> {
    let mut tried: Vec<String> = Vec::new();

    let try_branch = |name: &str, tried: &mut Vec<String>| -> Option<(String, String)> {
        if tried.iter().any(|t| t == name) {
            return None;
        }
        tried.push(name.to_string());
        if rev_parse_verify(cwd, &format!("origin/{name}")) {
            return Some((name.to_string(), format!("origin/{name}")));
        }
        if rev_parse_verify(cwd, name) {
            return Some((name.to_string(), name.to_string()));
        }
        None
    };

    if let Some(remote_head) = query_origin_head(cwd) {
        if let Some(found) = try_branch(&remote_head, &mut tried) {
            return Ok(found);
        }
    }

    if let Some(name) = hint {
        if let Some(found) = try_branch(name, &mut tried) {
            return Ok(found);
        }
    }

    for candidate in ["main", "master", "trunk", "develop"] {
        if let Some(found) = try_branch(candidate, &mut tried) {
            return Ok(found);
        }
    }

    Err(tried)
}

fn rev_parse_verify(cwd: &Path, name: &str) -> bool {
    git_command(cwd)
        .args(["rev-parse", "--verify", "--quiet", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ask origin which branch HEAD points to.  Output looks like:
///   ref: refs/heads/main\tHEAD
///   <sha>\tHEAD
/// We pull the `refs/heads/<name>` from the symref line.
fn query_origin_head(cwd: &Path) -> Option<String> {
    let output = git_command(cwd)
        .args(["ls-remote", "--symref", "origin", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            let target = rest.split_whitespace().next()?;
            if let Some(name) = target.strip_prefix("refs/heads/") {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Worktrees live under `<base>/<project>-<hash>/<branch>`.  `base` defaults
/// to `~/.alacritree/worktrees` so worktrees don't clutter the repo's parent
/// directory and stay grouped per app; a configured `workspace.worktree_dir`
/// relocates them.  The path hash disambiguates same-named repos in different
/// locations.
fn pick_worktree_path(repo: &Path, branch: &str, base: Option<&Path>) -> Result<PathBuf, String> {
    let parent = project_worktree_dir(repo, base)?;
    std::fs::create_dir_all(&parent)
        .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    let safe_branch: String =
        branch.chars().map(|c| if c == '/' || c.is_whitespace() { '-' } else { c }).collect();
    let mut candidate = parent.join(&safe_branch);
    let mut suffix = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{safe_branch}-{suffix}"));
        suffix += 1;
    }
    Ok(candidate)
}

/// Worktrees live under `<base>/<project>-<hash>/`.  `base` is the configured
/// `[workspace]` override when set; otherwise `<home>/.alacritree/worktrees`,
/// using the *distro's* home for WSL repos so the worktree stays on the Linux
/// filesystem next to its repo instead of crossing onto 9P-mounted NTFS.  The
/// path hash disambiguates same-named repos in different locations.
fn project_worktree_dir(repo: &Path, base: Option<&Path>) -> Result<PathBuf, String> {
    let base = match base {
        Some(dir) => dir.to_path_buf(),
        None => {
            let home = match wsl::classify(repo) {
                wsl::Location::Windows(_) => {
                    home::home_dir().ok_or_else(|| "could not locate home directory".to_string())?
                },
                wsl::Location::Wsl { distro, .. } => {
                    let stdout = wsl::run_batch(&distro, r#"printf '%s' "$HOME""#, &[])
                        .map_err(|e| format!("could not query WSL home: {e}"))?;
                    let linux_home = String::from_utf8_lossy(&stdout).trim().to_string();
                    if linux_home.is_empty() {
                        return Err("could not determine the distro home directory".into());
                    }
                    wsl::linux_to_windows(&linux_home, &distro)
                },
            };
            home.join(".alacritree").join("worktrees")
        },
    };
    let canonical = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let project_name = canonical
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".to_string());

    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = hasher.finish() as u32;

    Ok(base.join(format!("{project_name}-{hash:08x}")))
}

/// Filenames/dirs at the project root that look like AI assistant config.
const LLM_CONFIG_NAMES: &[&str] = &[
    "CLAUDE.md",
    "CLAUDE.local.md",
    ".claude",
    ".clauderc",
    "AGENTS.md",
    ".cursorrules",
    ".cursor",
    ".aider.conf.yml",
    ".aiderignore",
    ".copilot-instructions.md",
    ".github/copilot-instructions.md",
    ".windsurfrules",
    ".roomodes",
    ".roo",
    ".codeium",
    ".continue",
];

fn copy_llm_configs(src_root: &Path, dst_root: &Path) -> usize {
    let mut copied = 0;
    for name in LLM_CONFIG_NAMES {
        let src = src_root.join(name);
        if !src.exists() {
            continue;
        }
        let dst = dst_root.join(name);
        if dst.exists() {
            continue;
        }
        match copy_path(&src, &dst) {
            Ok(()) => copied += 1,
            Err(e) => log::warn!("failed to copy {}: {e}", src.display()),
        }
    }
    copied
}

#[cfg(test)]
#[cfg(windows)]
mod windows_tests {
    use super::*;

    #[test]
    fn git_path_arg_windows_repo_passes_path_through() {
        let repo = Path::new(r"C:\x");
        let path = Path::new(r"C:\x\y");
        assert_eq!(git_path_arg(repo, path).as_deref(), Ok(r"C:\x\y"));
    }

    #[test]
    fn git_path_arg_wsl_repo_translates_worktree_path() {
        let repo = Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj");
        let path = Path::new(r"\\wsl.localhost\kali-linux\home\lev\wt");
        assert_eq!(git_path_arg(repo, path).as_deref(), Ok("/home/lev/wt"));
    }

    #[test]
    fn git_path_arg_wsl_repo_errors_outside_distro_mapping() {
        let repo = Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj");
        let path = Path::new("wt");
        assert!(git_path_arg(repo, path).is_err());
    }
}

fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let child_dst = dst.join(entry.file_name());
            copy_path(&entry.path(), &child_dst)?;
        }
        Ok(())
    } else if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    } else {
        Ok(())
    }
}

pub fn delete_worktree(
    project_root: &Path,
    worktree_path: &Path,
    branch: Option<&str>,
    force: bool,
) -> Result<(), String> {
    let path_arg = git_path_arg(project_root, worktree_path)?;
    // Resolve before removal: canonicalize needs the directory to still
    // exist, and the doppler cleanup below runs after git has deleted it.
    let scope_root =
        std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_arg);
    run_git(project_root, &args)?;
    if let Some(branch) = branch {
        // Branch may already be gone (e.g. detached HEAD) — ignore errors here.
        let _ = run_git(project_root, &["branch", "-D", branch]);
    }
    let cleaned = crate::doppler::forget_scopes(&scope_root);
    if cleaned > 0 {
        log::info!("dropped {cleaned} doppler scope(s) under {}", scope_root.display());
    }
    Ok(())
}

/// A worktree removal to run on a background thread: either delete a live
/// checkout ([`delete_worktree`]) or prune the leftover metadata of one whose
/// directory is already gone ([`prune_worktree`]).
pub enum DeleteJob {
    Remove { worktree_path: PathBuf, branch: Option<String>, force: bool },
    Prune { worktree_name: String, branch: Option<String>, delete_branch: bool },
}

/// Run a [`DeleteJob`] off the UI thread, waking the window when it finishes.
/// The git shellouts and doppler cleanup are slow enough to stutter paint, so
/// the caller confirms the dialog, hands the work here, and adopts the result
/// (an error to surface, or nothing) from the returned channel.
pub fn spawn_delete(
    project_root: PathBuf,
    job: DeleteJob,
    ctx: egui::Context,
) -> Receiver<Result<(), String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = match job {
            DeleteJob::Remove { worktree_path, branch, force } => {
                delete_worktree(&project_root, &worktree_path, branch.as_deref(), force)
            },
            DeleteJob::Prune { worktree_name, branch, delete_branch } => {
                prune_worktree(&project_root, &worktree_name, branch.as_deref(), delete_branch)
            },
        };
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    rx
}

/// Remove the git metadata of a worktree whose checkout directory is gone
/// (git calls these *prunable*). Uses git2's per-worktree prune rather than
/// shelling out to `git worktree prune`, which would sweep every stale
/// worktree in the repo instead of just the one the user asked about.
pub fn prune_worktree(
    project_root: &Path,
    worktree_name: &str,
    branch: Option<&str>,
    delete_branch: bool,
) -> Result<(), String> {
    let repo = git2::Repository::open(project_root)
        .map_err(|e| format!("failed to open repository: {}", e.message()))?;
    let wt = repo
        .find_worktree(worktree_name)
        .map_err(|e| format!("failed to find worktree `{worktree_name}`: {}", e.message()))?;
    // Default prune options refuse valid or locked worktrees — exactly the
    // safety we want if the directory reappeared since discovery; the error
    // surfaces to the caller.
    wt.prune(None).map_err(|e| format!("failed to prune: {}", e.message()))?;
    if delete_branch {
        if let Some(branch) = branch {
            // Branch may already be gone — ignore errors, as delete_worktree does.
            let _ = run_git(project_root, &["branch", "-D", branch]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{add_worktree, init_repo};

    fn abs(tail: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\{tail}"))
        } else {
            PathBuf::from(format!("/{tail}"))
        }
    }

    #[test]
    fn base_dir_replaces_default_worktree_parent() {
        let base = abs("wt-base");
        let dir = project_worktree_dir(Path::new("repo"), Some(&base)).unwrap();
        assert!(dir.starts_with(&base), "{} not under {}", dir.display(), base.display());
        let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(leaf.starts_with("repo-"), "leaf {leaf:?} should keep <project>-<hash> layout");
    }

    #[test]
    fn no_base_dir_falls_back_to_home_default() {
        let dir = project_worktree_dir(Path::new("repo"), None).unwrap();
        let expected = home::home_dir().unwrap().join(".alacritree").join("worktrees");
        assert!(dir.starts_with(&expected), "{} not under {}", dir.display(), expected.display());
    }

    #[test]
    fn spawn_delete_removes_a_live_worktree_off_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        let wt_path = add_worktree(&repo, "feature");
        assert!(wt_path.is_dir());

        let job = DeleteJob::Remove {
            worktree_path: wt_path.clone(),
            branch: Some("feature".to_string()),
            force: false,
        };
        let result = spawn_delete(repo_dir, job, egui::Context::default()).recv().unwrap();

        assert!(result.is_ok(), "delete failed: {result:?}");
        assert!(!wt_path.exists(), "worktree directory should be gone");
        assert!(repo.find_worktree("feature").is_err());
        assert!(repo.find_branch("feature", git2::BranchType::Local).is_err());
    }

    #[test]
    fn prune_removes_stale_metadata_and_keeps_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        let wt_path = add_worktree(&repo, "stale");
        std::fs::remove_dir_all(&wt_path).unwrap();

        prune_worktree(&repo_dir, "stale", Some("stale"), false).unwrap();

        assert!(repo.find_worktree("stale").is_err());
        assert!(repo.find_branch("stale", git2::BranchType::Local).is_ok());
    }

    #[test]
    fn prune_deletes_branch_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        let wt_path = add_worktree(&repo, "stale");
        std::fs::remove_dir_all(&wt_path).unwrap();

        prune_worktree(&repo_dir, "stale", Some("stale"), true).unwrap();

        assert!(repo.find_worktree("stale").is_err());
        assert!(repo.find_branch("stale", git2::BranchType::Local).is_err());
    }

    #[test]
    fn prune_refuses_a_live_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = init_repo(&repo_dir);
        add_worktree(&repo, "live");

        assert!(prune_worktree(&repo_dir, "live", Some("live"), false).is_err());
        assert!(repo.find_worktree("live").is_ok());
        assert!(repo.find_branch("live", git2::BranchType::Local).is_ok());
    }
}
