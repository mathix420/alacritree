//! Create, delete, and prune git worktrees off the UI thread.
//!
//! Creation streams its progress back over an `mpsc` channel as each step
//! starts; deletion and pruning report their single result through a
//! `jobs::Job`. Both submit to the shared pool rather than spawning their
//! own thread.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};

use alacritree_checkout_hooks::{CheckoutEvent, CheckoutHook, CheckoutHooks};
use alacritree_vcs::{CreateCheckout, VcsError, VersionControl};

use crate::checkout_hooks::Hook;
use crate::config::{Config, WorkspaceConfig};
use crate::repaint::Repaint;
use crate::tools::{self, Tool};
use crate::{command_ext, jobs, wsl};

#[derive(Debug)]
pub(crate) enum Progress {
    Step(String),
    Done(Result<PathBuf, WorktreeError>),
}

/// Why creating, removing or pruning a worktree failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorktreeError {
    #[error("worktree create cancelled")]
    Cancelled,
    /// The worker unwound instead of returning. The pool records only that a
    /// panic happened, so a create's step list stops wherever it got to.
    #[error("the background worker panicked")]
    WorkerPanicked,
    #[error("failed to run git: {0}")]
    Spawn(#[source] io::Error),
    /// git ran and refused. `output` is what git printed, kept apart from
    /// `args` so a caller can read git's reason without the command line.
    #[error("git {args}: {output}")]
    Git { args: String, output: String },
    #[error("invalid worktree path")]
    NonUtf8Path,
    #[error("worktree path is outside the distro")]
    OutsideDistro,
    #[error("failed to create {}: {source}", path.display())]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("could not locate home directory")]
    NoHome,
    #[error("could not query WSL home: {0}")]
    WslHome(#[source] wsl::BatchError),
    #[error("could not determine the distro home directory")]
    EmptyWslHome,
    #[error("failed to open repository: {}", .0.message())]
    OpenRepo(#[source] git2::Error),
    #[error("failed to find worktree `{name}`: {}", source.message())]
    FindWorktree { name: String, source: git2::Error },
    #[error("failed to prune: {}", .0.message())]
    Prune(#[source] git2::Error),
    #[error(transparent)]
    Vcs(#[from] VcsError),
}

/// What a create from IPC or the offline CLI reads from config: where the
/// worktree goes, the hooks that run once it exists, and the enabled version
/// control backends.
#[derive(Clone)]
pub(crate) struct CreateConfig {
    pub(crate) workspace: WorkspaceConfig,
    pub(crate) hooks: Vec<Hook>,
    pub(crate) vcs: Vec<crate::vcs::Vcs>,
}

/// No hooks, and the backends a config without `[integrations]` enables.
impl Default for CreateConfig {
    fn default() -> Self {
        Self {
            workspace: WorkspaceConfig::default(),
            hooks: Vec::new(),
            vcs: crate::vcs::backends(&crate::config::IntegrationsConfig::default()),
        }
    }
}

impl CreateConfig {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            workspace: config.workspace.clone(),
            hooks: crate::checkout_hooks::from_config(&config.integrations),
            vcs: crate::vcs::backends(&config.integrations),
        }
    }
}

pub(crate) struct CreateRequest {
    project_root: PathBuf,
    default_branch: Option<String>,
    branch: String,
    /// Base directory to create the worktree under; `None` uses the built-in
    /// `~/.alacritree/worktrees` default.
    base_dir: Option<PathBuf>,
    vcs: crate::vcs::Vcs,
}

impl CreateRequest {
    /// The location comes from `[workspace]` here rather than from each
    /// caller, so the sidebar, IPC and the offline CLI put a worktree in the
    /// same place.
    pub(crate) fn new(
        project_root: PathBuf,
        default_branch: Option<String>,
        branch: String,
        workspace: &WorkspaceConfig,
        vcs: crate::vcs::Vcs,
    ) -> Self {
        let base_dir = workspace.base_dir_for(&project_root);
        Self { project_root, default_branch, branch, base_dir, vcs }
    }
}

/// Run [`create`] on the pool, waking the UI for each step. A worktree
/// create is user-initiated, so it runs at interactive priority. The
/// streamed progress travels over the channel; the returned `Job` carries no
/// result of its own and exists only to be held. Dropping it would cancel
/// the create before it starts.
pub(crate) fn spawn_create<H: CheckoutHook + Send + 'static>(
    req: CreateRequest,
    hooks: Vec<H>,
    repaint: impl Repaint,
) -> (Receiver<Progress>, jobs::Job<()>) {
    let (tx, rx) = mpsc::channel();
    let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
        let result = create(
            &req,
            hooks.as_slice(),
            |step| {
                let _ = tx.send(Progress::Step(step.to_string()));
                repaint.wake();
            },
            blocking,
        );
        let _ = tx.send(Progress::Done(result));
        repaint.wake();
    });
    (rx, job)
}

/// Create the worktree on the calling thread, reporting each step as it starts.
///
/// Nothing here needs a window, so callers without one (the CLI, with no
/// running app to talk to) drive this directly through [`jobs::on_this_thread`]
/// rather than through [`spawn_create`].
pub(crate) fn create<H: CheckoutHooks + ?Sized>(
    req: &CreateRequest,
    hooks: &H,
    mut on_step: impl FnMut(&str),
    blocking: &jobs::Blocking,
) -> Result<PathBuf, WorktreeError> {
    let send = &mut on_step;
    // A cancel that lands between children has nothing to kill, so each step
    // asks before starting rather than running for a caller that is gone.
    macro_rules! bail_if_cancelled {
        () => {
            if blocking.cancelled() {
                return Err(WorktreeError::Cancelled);
            }
        };
    }

    let base = req
        .vcs
        .prepare_checkout(&req.project_root, req.default_branch.as_deref(), &mut *send, blocking)
        .map_err(create_error)?;
    let target =
        pick_worktree_path(&req.project_root, &req.branch, req.base_dir.as_deref(), blocking)?;
    let checkout = CreateCheckout {
        main: req.project_root.clone(),
        target: target.clone(),
        name: req.branch.clone(),
        base,
    };
    req.vcs.create_checkout(&checkout, blocking).map_err(create_error)?;

    bail_if_cancelled!();
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

    bail_if_cancelled!();
    let event = CheckoutEvent { main: &req.project_root, checkout: &target };
    crate::checkout_hooks::report(hooks.created(&event, blocking), |_, line| send(line));

    Ok(target)
}

/// The backend's cancellation is the app's, so callers see one cancel
/// whichever side noticed it.
fn create_error(error: VcsError) -> WorktreeError {
    match error {
        VcsError::Cancelled { .. } => WorktreeError::Cancelled,
        error => WorktreeError::Vcs(error),
    }
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
            let mut cmd = command_ext::hidden(tools::program(Tool::Git));
            cmd.arg("-C").arg(path);
            cmd
        },
        wsl::Location::Wsl { distro, linux_path } => {
            let mut cmd = wsl::command(&distro, None);
            cmd.arg(tools::wsl_program(Tool::Git)).arg("-C").arg(linux_path);
            cmd
        },
    }
}

/// The form of `path` git receives as an argument: Linux for WSL repos
/// (in-distro git can't resolve UNC paths), the Windows string otherwise.
fn git_path_arg(repo: &Path, path: &Path) -> Result<String, WorktreeError> {
    match wsl::classify(repo) {
        wsl::Location::Windows(_) => {
            Ok(path.to_str().ok_or(WorktreeError::NonUtf8Path)?.to_string())
        },
        wsl::Location::Wsl { .. } => {
            wsl::windows_to_linux(path).ok_or(WorktreeError::OutsideDistro)
        },
    }
}

#[allow(clippy::disallowed_methods)] // Running git is this function's job.
fn run_git(cwd: &Path, args: &[&str]) -> Result<(), WorktreeError> {
    let output = git_command(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(WorktreeError::Spawn)?;
    git_succeeded(args, output).map(drop)
}

/// `output` when git exited cleanly. Otherwise what git said on stderr, or on
/// stdout when stderr was empty.
fn git_succeeded(args: &[&str], output: Output) -> Result<Output, WorktreeError> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let said = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
    Err(WorktreeError::Git { args: args.join(" "), output: said.to_string() })
}

/// Worktrees live under `<base>/<project>-<hash>/<branch>`.  `base` defaults
/// to `~/.alacritree/worktrees` so worktrees don't clutter the repo's parent
/// directory and stay grouped per app; a configured `workspace.worktree_dir`
/// relocates them.  The path hash disambiguates same-named repos in different
/// locations.
fn pick_worktree_path(
    repo: &Path,
    branch: &str,
    base: Option<&Path>,
    blocking: &jobs::Blocking,
) -> Result<PathBuf, WorktreeError> {
    let parent = project_worktree_dir(repo, base, blocking)?;
    std::fs::create_dir_all(&parent)
        .map_err(|source| WorktreeError::CreateDir { path: parent.clone(), source })?;
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
fn project_worktree_dir(
    repo: &Path,
    base: Option<&Path>,
    blocking: &jobs::Blocking,
) -> Result<PathBuf, WorktreeError> {
    let base = match base {
        Some(dir) => dir.to_path_buf(),
        None => {
            let home = match wsl::classify(repo) {
                wsl::Location::Windows(_) => home::home_dir().ok_or(WorktreeError::NoHome)?,
                wsl::Location::Wsl { distro, .. } => {
                    let stdout = wsl::run_batch(&distro, r#"printf '%s' "$HOME""#, &[], blocking)
                        .map_err(WorktreeError::WslHome)?;
                    let linux_home = String::from_utf8_lossy(&stdout).trim().to_string();
                    if linux_home.is_empty() {
                        return Err(WorktreeError::EmptyWslHome);
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
        assert_eq!(git_path_arg(repo, path).unwrap(), r"C:\x\y");
    }

    #[test]
    fn git_path_arg_wsl_repo_translates_worktree_path() {
        let repo = Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj");
        let path = Path::new(r"\\wsl.localhost\kali-linux\home\lev\wt");
        assert_eq!(git_path_arg(repo, path).unwrap(), "/home/lev/wt");
    }

    #[test]
    fn git_path_arg_wsl_repo_errors_outside_distro_mapping() {
        let repo = Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj");
        let path = Path::new("wt");
        assert!(matches!(git_path_arg(repo, path), Err(WorktreeError::OutsideDistro)));
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

pub(crate) fn delete_worktree<H: CheckoutHooks + ?Sized>(
    project_root: &Path,
    worktree_path: &Path,
    branch: Option<&str>,
    force: bool,
    hooks: &H,
    blocking: &jobs::Blocking,
) -> Result<(), WorktreeError> {
    let path_arg = git_path_arg(project_root, worktree_path)?;
    // Resolve before removal: canonicalize needs the directory to still
    // exist, and the checkout hooks below run after git has deleted it.
    let scope_root =
        std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_arg);
    run_git(project_root, &args)?;
    if let Some(branch) = branch {
        // Branch may already be gone (e.g. detached HEAD), so ignore errors.
        let _ = run_git(project_root, &["branch", "-D", branch]);
    }
    let event = CheckoutEvent { main: project_root, checkout: &scope_root };
    crate::checkout_hooks::report(hooks.removed(&event, blocking), |level, line| {
        log::log!(level, "{line} (removed {})", scope_root.display())
    });
    Ok(())
}

/// A worktree removal to run on a background thread: either delete a live
/// checkout ([`delete_worktree`]) or prune the leftover metadata of one whose
/// directory is already gone ([`prune_worktree`]).
pub(crate) enum DeleteJob {
    Remove { worktree_path: PathBuf, branch: Option<String>, force: bool },
    Prune { worktree_name: String, branch: Option<String>, delete_branch: bool },
}

/// Run a [`DeleteJob`] on the pool, waking the window when it finishes. The
/// git shellouts and checkout hooks are slow enough to stutter paint, so the
/// caller confirms the dialog, hands the work here, and adopts the result (an
/// error to surface, or nothing) from the returned handle. The sidebar row
/// shows a spinner until it lands, so this runs at interactive priority.
pub(crate) fn spawn_delete<H: CheckoutHook + Send + 'static>(
    project_root: PathBuf,
    job: DeleteJob,
    hooks: Vec<H>,
    repaint: impl Repaint,
) -> jobs::Job<Result<(), WorktreeError>> {
    jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
        let result = match job {
            DeleteJob::Remove { worktree_path, branch, force } => {
                let hooks = hooks.as_slice();
                delete_worktree(
                    &project_root,
                    &worktree_path,
                    branch.as_deref(),
                    force,
                    hooks,
                    blocking,
                )
            },
            DeleteJob::Prune { worktree_name, branch, delete_branch } => {
                prune_worktree(&project_root, &worktree_name, branch.as_deref(), delete_branch)
            },
        };
        repaint.wake();
        result
    })
}

/// Remove the git metadata of a worktree whose checkout directory is gone
/// (git calls these *prunable*). Uses git2's per-worktree prune rather than
/// shelling out to `git worktree prune`, which would sweep every stale
/// worktree in the repo instead of just the one the user asked about.
fn prune_worktree(
    project_root: &Path,
    worktree_name: &str,
    branch: Option<&str>,
    delete_branch: bool,
) -> Result<(), WorktreeError> {
    let repo = git2::Repository::open(project_root).map_err(WorktreeError::OpenRepo)?;
    let wt = repo.find_worktree(worktree_name).map_err(|source| WorktreeError::FindWorktree {
        name: worktree_name.to_string(),
        source,
    })?;
    // Default prune options refuse valid or locked worktrees. That is exactly
    // the safety we want if the directory reappeared since discovery; the
    // error surfaces to the caller.
    wt.prune(None).map_err(WorktreeError::Prune)?;
    if delete_branch {
        if let Some(branch) = branch {
            // Ignore errors as delete_worktree does. The branch may be gone.
            let _ = run_git(project_root, &["branch", "-D", branch]);
        }
    }
    Ok(())
}

#[cfg(test)]
// Fixtures drive real processes and wait on them; no frame is pending.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::repaint::Recorder;
    use crate::test_util::{add_worktree, init_repo};
    use alacritree_checkout_hooks::fake::{Event, FakeHook};
    use alacritree_git::test_support::clone_with_origin;
    use alacritree_vcs::fake::FakeVcs;

    fn git() -> crate::vcs::Vcs {
        crate::vcs::Vcs::Git(alacritree_git::GitBackend::new(&alacritree_git::GitConfig::default()))
    }

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
        let dir = jobs::on_this_thread(|b| project_worktree_dir(Path::new("repo"), Some(&base), b))
            .unwrap();
        assert!(dir.starts_with(&base), "{} not under {}", dir.display(), base.display());
        let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(leaf.starts_with("repo-"), "leaf {leaf:?} should keep <project>-<hash> layout");
    }

    #[test]
    fn no_base_dir_falls_back_to_home_default() {
        let dir =
            jobs::on_this_thread(|b| project_worktree_dir(Path::new("repo"), None, b)).unwrap();
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
        let repaint = Recorder::default();
        let handle = spawn_delete(repo_dir, job, Vec::<FakeHook>::new(), repaint.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = loop {
            if let Some(result) = handle.poll() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline, "the delete never landed");
            thread::yield_now();
        };

        assert!(result.is_ok(), "delete failed: {result:?}");
        assert_eq!(repaint.wakes(), 1, "the finished delete should wake the UI");
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

    /// `create` must stop between steps when its handle is gone.  Killing a
    /// registered child only covers the steps that have one; the local steps
    /// would otherwise run to completion for a worktree nobody is waiting for.
    #[test]
    fn create_stops_between_steps_once_cancelled() {
        let repo = tempfile::tempdir().expect("temp dir");
        let req = CreateRequest {
            project_root: repo.path().to_path_buf(),
            default_branch: Some("main".into()),
            branch: "topic".into(),
            base_dir: None,
            vcs: git(),
        };
        let (tx, rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
            // Both halves of this handshake are load-bearing.  Without the
            // started signal, a flag set while the task is still queued hits the
            // pre-start check, the task is skipped, `tx` drops unsent, and the
            // assertion below reports a disconnect.  Without the gate, the task
            // can race past the first bail before the flag lands and fail on the
            // missing remote instead.
            let _ = started_tx.send(());
            let _ = gate_rx.recv();
            let _ = tx.send(create(&req, &[] as &[FakeHook], |_| {}, blocking));
        });
        started_rx.recv_timeout(Duration::from_secs(5)).expect("the job never started");
        drop(job);
        let _ = gate_tx.send(());
        let result = rx.recv_timeout(Duration::from_secs(10));
        match result {
            Ok(Err(WorktreeError::Cancelled)) => {},
            Ok(Err(e)) => panic!("create failed for the wrong reason: {e}"),
            Ok(Ok(path)) => panic!("create finished a worktree nobody was waiting for: {path:?}"),
            Err(e) => panic!("create never returned: {e}"),
        }
    }

    /// A caller that gave up before the hooks must not have them started only
    /// for the cancel to kill each one.
    #[test]
    fn create_runs_no_hook_once_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let project = clone_with_origin(tmp.path());
        let req = CreateRequest {
            project_root: project,
            default_branch: None,
            branch: "abandoned".into(),
            base_dir: Some(tmp.path().join("worktrees")),
            vcs: git(),
        };
        let hook = FakeHook::reporting("Linked 1 fake scope");
        let job_hook = hook.clone();
        let (tx, rx) = mpsc::channel();
        let (reached_tx, reached_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
            // Park on the last step before the hooks until the handle is gone.
            let on_step = |step: &str| {
                if step.starts_with("Enabled Claude Code") {
                    let _ = reached_tx.send(());
                    let _ = gate_rx.recv();
                }
            };
            let _ = tx.send(create(&req, &[job_hook][..], on_step, blocking));
        });
        reached_rx.recv_timeout(Duration::from_secs(30)).expect("create never reached the hooks");
        drop(job);
        let _ = gate_tx.send(());
        let result = rx.recv_timeout(Duration::from_secs(10)).expect("create never returned");
        assert!(matches!(result, Err(WorktreeError::Cancelled)), "{result:?}");
        assert_eq!(hook.events(), []);
    }

    #[test]
    fn create_hands_the_new_checkout_to_every_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let project = clone_with_origin(tmp.path());
        let req = CreateRequest {
            project_root: project.clone(),
            default_branch: None,
            branch: "hooked".into(),
            base_dir: Some(tmp.path().join("worktrees")),
            vcs: git(),
        };
        let hook = FakeHook::reporting("Linked 1 fake scope");
        let mut steps = Vec::new();
        let target = jobs::on_this_thread(|b| {
            create(&req, &[hook.clone()][..], |s| steps.push(s.to_string()), b)
        })
        .expect("create succeeds");
        assert_eq!(hook.events(), [Event::Created { main: project, checkout: target }]);
        assert!(steps.iter().any(|s| s == "Linked 1 fake scope"), "{steps:?}");
    }

    #[test]
    fn a_failing_hook_shows_in_the_steps_and_does_not_fail_the_create() {
        let tmp = tempfile::tempdir().unwrap();
        let project = clone_with_origin(tmp.path());
        let req = CreateRequest {
            project_root: project,
            default_branch: None,
            branch: "hook-fails".into(),
            base_dir: Some(tmp.path().join("worktrees")),
            vcs: git(),
        };
        let mut steps = Vec::new();
        let result = jobs::on_this_thread(|b| {
            create(&req, &[FakeHook::failing()][..], |s| steps.push(s.to_string()), b)
        });
        assert!(result.is_ok(), "{result:?}");
        assert!(
            steps.iter().any(|s| s.starts_with("Hook failed: could not run fake")),
            "{steps:?}"
        );
    }

    /// The target path is picked only once the base resolved, so a create
    /// that fails early leaves no directory behind.
    #[test]
    fn a_failed_prepare_picks_no_target_path() {
        let tmp = tempfile::tempdir().unwrap();
        let base_dir = tmp.path().join("worktrees");
        let fake = FakeVcs::new("/r").refusing_prepare();
        let req = CreateRequest {
            project_root: PathBuf::from("/r"),
            default_branch: None,
            branch: "topic".into(),
            base_dir: Some(base_dir.clone()),
            vcs: crate::vcs::Vcs::Fake(fake.clone()),
        };
        let result = jobs::on_this_thread(|b| create(&req, &[] as &[FakeHook], |_| {}, b));
        assert!(matches!(result, Err(WorktreeError::Vcs(VcsError::NoRemote { .. }))), "{result:?}");
        assert!(!base_dir.exists(), "the target's parent was created before the base resolved");
        assert_eq!(fake.calls(), ["prepare /r"]);
    }

    #[test]
    fn hooks_run_after_the_backend_creates_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = FakeVcs::new("/r");
        let hook = FakeHook::silent();
        let req = CreateRequest {
            project_root: PathBuf::from("/r"),
            default_branch: None,
            branch: "topic".into(),
            base_dir: Some(tmp.path().to_path_buf()),
            vcs: crate::vcs::Vcs::Fake(fake.clone()),
        };
        let target = jobs::on_this_thread(|b| create(&req, &[hook.clone()][..], |_| {}, b))
            .expect("create succeeds");
        assert_eq!(fake.calls(), [
            "prepare /r".to_string(),
            format!("create {}", target.display())
        ]);
        assert_eq!(hook.events(), [Event::Created { main: PathBuf::from("/r"), checkout: target }]);
    }

    /// A hook may key its state by canonical path, and a removed directory
    /// can no longer be canonicalized, so the hook must get the path resolved
    /// first.
    #[cfg(unix)]
    #[test]
    fn removal_hands_hooks_the_path_resolved_before_git_deleted_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        let repo = crate::test_util::init_repo(&repo_dir);
        let wt_path = crate::test_util::add_worktree(&repo, "linked");
        let canonical = wt_path.canonicalize().unwrap();
        let link = tmp.path().join("via-link");
        std::os::unix::fs::symlink(&wt_path, &link).unwrap();
        let hook = FakeHook::silent();
        jobs::on_this_thread(|b| {
            delete_worktree(&repo_dir, &link, Some("linked"), true, &[hook.clone()][..], b)
        })
        .expect("delete succeeds");
        assert_eq!(hook.events(), [Event::Removed { main: repo_dir, checkout: canonical }]);
    }
}
