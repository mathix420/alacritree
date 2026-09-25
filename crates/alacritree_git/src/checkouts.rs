//! Creating git worktrees, and the branch names a new one can start from.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use alacritree_common::tools::{self, Tool};
use alacritree_common::{command_ext, jobs, wsl};
use alacritree_vcs::{Base, CreateCheckout, Created, RemoveCheckout, VcsError};

use crate::default_branch::{self, Evidence, WellKnown};

/// The first half of a worktree create, up to the step the app picks the
/// target path in.
pub(crate) fn prepare(
    main: &Path,
    trunk_hint: Option<&str>,
    on_step: &mut dyn FnMut(&str),
    blocking: &jobs::Blocking,
) -> Result<Base, VcsError> {
    // A cancel that lands between children has nothing to kill, so each step
    // asks before starting rather than running for a caller that is gone.
    macro_rules! bail_if_cancelled {
        () => {
            if blocking.cancelled() {
                return Err(VcsError::Cancelled { what: "worktree create" });
            }
        };
    }

    bail_if_cancelled!();
    on_step("Syncing with remote…");
    if !has_remote(main, "origin") {
        return Err(VcsError::NoRemote { remote: "origin".into() });
    }

    // The cached `default_branch` is a hint; if it's missing or stale (e.g.
    // user has a global `init.defaultBranch=master` but the repo's actual
    // default is `main`), ask origin what its HEAD really points to.
    let resolved = resolve_base_branch(main, trunk_hint, blocking);
    // `resolve_base_branch` can fail because its own `ls-remote` was
    // cancelled mid-flight; check before turning that failure into a
    // misleading "could not determine base branch" for a caller that is
    // actually just gone.
    bail_if_cancelled!();
    let (base, base_ref) = resolved.map_err(|tried| VcsError::NoBase { tried })?;
    on_step(&format!("Verifying base branch `{base}`"));

    bail_if_cancelled!();
    on_step("Fetching latest changes…");
    run_git_cancellable(blocking, main, &["fetch", "origin", &base])?;

    bail_if_cancelled!();
    on_step("Creating git worktree…");
    Ok(Base { name: base, revision: base_ref })
}

/// `git worktree add` on a new branch from the prepared base.
pub(crate) fn create(req: &CreateCheckout) -> Result<Created, VcsError> {
    let target = git_path_arg(&req.main, &req.target)?;
    run_git(&req.main, &["worktree", "add", &target, "-b", &req.name, &req.base.revision])?;
    Ok(Created::default())
}

/// A live checkout goes through `git worktree remove`, and a gone one is
/// pruned. Either way its branch goes too when asked.
pub(crate) fn remove(req: &RemoveCheckout) -> Result<(), VcsError> {
    let checkout = &req.checkout;
    if checkout.gone {
        prune_worktree(&req.main, &checkout.name)?;
    } else {
        let path_arg = git_path_arg(&req.main, &checkout.path)?;
        let mut args: Vec<&str> = vec!["worktree", "remove"];
        if req.force {
            args.push("--force");
        }
        args.push(&path_arg);
        run_git(&req.main, &args).map_err(|e| match e {
            VcsError::Failed { command, stderr } if refused_for_unsaved_work(&stderr) => {
                VcsError::Unsaved { message: format!("{command}: {stderr}") }
            },
            e => e,
        })?;
    }
    if req.delete_name
        && let Some(branch) = &checkout.head.name
    {
        // Branch may already be gone (e.g. detached HEAD), so ignore errors.
        let _ = run_git(&req.main, &["branch", "-D", branch]);
    }
    Ok(())
}

/// Remove the git metadata of a worktree whose checkout directory is gone
/// (git calls these *prunable*). Uses git2's per-worktree prune rather than
/// shelling out to `git worktree prune`, which would sweep every stale
/// worktree in the repo instead of just the one the user asked about.
fn prune_worktree(main: &Path, worktree_name: &str) -> Result<(), VcsError> {
    let repo = git2::Repository::open(main)
        .map_err(|e| git2_error(format!("failed to open repository: {}", e.message()), e))?;
    let wt = repo.find_worktree(worktree_name).map_err(|e| {
        git2_error(format!("failed to find worktree `{worktree_name}`: {}", e.message()), e)
    })?;
    // Default prune options refuse valid or locked worktrees. That is exactly
    // the safety we want if the directory reappeared since discovery; the
    // error surfaces to the caller.
    wt.prune(None).map_err(|e| git2_error(format!("failed to prune: {}", e.message()), e))
}

fn git2_error(context: String, source: git2::Error) -> VcsError {
    VcsError::Backend { context, source: Box::new(source) }
}

/// `git worktree remove` refuses a tree with work in it, and that refusal is
/// the authority on whether removing would lose anything. `contains modified
/// or untracked files` is git's current wording, `is dirty` what git 2.17
/// said before the rewording.
///
/// git prints `fatal: '<path>' <reason>`, and a user-chosen path that spells
/// out a fragment must not turn an unrelated failure into a false "needs
/// --force" prompt. git's reason always follows the path's closing quote, so
/// only the text after the last `'` is read.
fn refused_for_unsaved_work(output: &str) -> bool {
    let tail = output.rsplit_once("fatal:").map_or(output, |(_, tail)| tail);
    let reason = tail.rsplit_once('\'').map_or(tail, |(_, after)| after).to_ascii_lowercase();
    reason.contains("contains modified or untracked files, use --force")
        || reason.contains("is dirty, use --force")
}

fn spawn_error(source: std::io::Error) -> VcsError {
    VcsError::Spawn { program: "git".into(), source }
}

/// Why a branch name is not one git would accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BranchNameError {
    #[error("Branch name is empty.")]
    Empty,
    #[error("Branch name cannot start with `-`.")]
    LeadingDash,
    #[error("Branch name cannot start or end with `.`.")]
    EdgeDot,
    #[error("Branch name cannot end with `.lock`.")]
    LockSuffix,
    #[error("Branch name cannot contain `..` or `@{{`.")]
    ReservedSequence,
    #[error("Branch name cannot contain whitespace.")]
    Whitespace,
    #[error("Branch name contains a control character.")]
    Control,
    #[error("Branch name cannot contain `{0}`.")]
    Forbidden(char),
}

/// git-check-ref-format rules, abridged: no whitespace/control chars, no
/// `..`, `~`, `^`, `:`, `?`, `*`, `[`, `\`, `@{`; can't start with `-` or `.`,
/// or end with `.` or `.lock`.
pub(crate) fn validate_branch_name(name: &str) -> Result<(), BranchNameError> {
    if name.is_empty() {
        return Err(BranchNameError::Empty);
    }
    if name.starts_with('-') {
        return Err(BranchNameError::LeadingDash);
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err(BranchNameError::EdgeDot);
    }
    if name.ends_with(".lock") {
        return Err(BranchNameError::LockSuffix);
    }
    if name.contains("..") || name.contains("@{") {
        return Err(BranchNameError::ReservedSequence);
    }
    for c in name.chars() {
        if c.is_whitespace() {
            return Err(BranchNameError::Whitespace);
        }
        if (c as u32) < 0x20 || c == '\u{7f}' {
            return Err(BranchNameError::Control);
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Err(BranchNameError::Forbidden(c));
        }
    }
    Ok(())
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
fn git_path_arg(repo: &Path, path: &Path) -> Result<String, VcsError> {
    match wsl::classify(repo) {
        wsl::Location::Windows(_) => Ok(path
            .to_str()
            .ok_or_else(|| VcsError::BadPath("invalid worktree path".into()))?
            .to_string()),
        wsl::Location::Wsl { .. } => wsl::windows_to_linux(path)
            .ok_or_else(|| VcsError::BadPath("worktree path is outside the distro".into())),
    }
}

#[allow(clippy::disallowed_methods)] // Running git is this function's job.
fn run_git(cwd: &Path, args: &[&str]) -> Result<(), VcsError> {
    let output = git_command(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(spawn_error)?;
    git_succeeded(args, output).map(drop)
}

/// `run_git`, for a call a cancel is allowed to end.  Progress goes to a
/// pipe, where git suppresses it, so the output stays small enough that the
/// undrained pipes cannot fill.
fn run_git_cancellable(
    blocking: &jobs::Blocking,
    cwd: &Path,
    args: &[&str],
) -> Result<(), VcsError> {
    let mut cmd = git_command(cwd);
    let output = blocking
        .run_cancellable(cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()))
        .map_err(spawn_error)?;
    git_succeeded(args, output).map(drop)
}

/// `output` when git exited cleanly. Otherwise what git said on stderr, or on
/// stdout when stderr was empty.
fn git_succeeded(args: &[&str], output: Output) -> Result<Output, VcsError> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let said = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
    Err(VcsError::Failed { command: format!("git {}", args.join(" ")), stderr: said.to_string() })
}

#[allow(clippy::disallowed_methods)] // Running git is this function's job.
fn has_remote(cwd: &Path, name: &str) -> bool {
    git_command(cwd)
        .args(["remote", "get-url", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Branch names for the base-branch picker: locals first, then `origin/*`,
/// short names, in git's ref order.  Shells out through [`git_command`]
/// rather than using git2 so WSL worktrees resolve the same way everything
/// else in this module does.
#[allow(clippy::disallowed_methods)] // Running git is this function's job.
pub(crate) fn list_branches(
    cwd: &Path,
    _blocking: &jobs::Blocking,
) -> Result<Vec<String>, VcsError> {
    let args = ["for-each-ref", "--format=%(refname:short)", "refs/heads", "refs/remotes/origin"];
    let output = git_command(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(spawn_error)?;
    let output = git_succeeded(&args, output)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        // `origin/HEAD` shortens to plain `origin`, an alias, not a branch.
        .filter(|l| !l.is_empty() && *l != "origin")
        .map(str::to_string)
        .collect())
}

/// Which branch a new worktree should start from, and the ref to branch off.
///
/// `git ls-remote --symref HEAD` is the one source reflecting the upstream's
/// current default, so it answers ahead of the caller's cached
/// `refs/remotes/origin/HEAD`, which lags a rename.  Everything after that is
/// [`default_branch::resolve`].  On total failure, returns the names tried.
///
/// That `ls-remote` is the only network round trip here, and a cancel landing
/// during it must not leave the caller waiting on an unreachable remote, so it
/// goes through [`jobs::Blocking::run_cancellable`].  A cancelled query folds
/// into the same `None` an unreachable one produces: the local `rev-parse`
/// probes that follow are cheap, and the caller re-checks cancellation before
/// trusting the outcome.
fn resolve_base_branch(
    cwd: &Path,
    hint: Option<&str>,
    blocking: &jobs::Blocking,
) -> Result<(String, String), Vec<String>> {
    let mut tried: Vec<String> = Vec::new();

    // A name counts as evidence only once this repository can resolve it,
    // locally or on origin, since the caller goes on to branch from it.
    let have = |name: &str, tried: &mut Vec<String>| -> bool {
        if !tried.iter().any(|t| t == name) {
            tried.push(name.to_string());
        }
        rev_parse_verify(cwd, &format!("origin/{name}")) || rev_parse_verify(cwd, name)
    };

    // `hint` is a cached detection, not a choice, so it stands in for
    // `origin/HEAD` only when the live query cannot answer.
    let queried = query_origin_head(cwd, blocking);
    if blocking.cancelled() {
        return Err(tried);
    }
    let origin_head =
        queried.or_else(|| hint.map(str::to_string)).filter(|name| have(name, &mut tried));
    let present: Vec<&str> =
        WellKnown::ALL.iter().map(|c| c.as_str()).filter(|name| have(name, &mut tried)).collect();
    let init_default =
        config_value(cwd, "init.defaultBranch").filter(|name| have(name, &mut tried));

    let Some(branch) = default_branch::resolve(&Evidence {
        origin_head: origin_head.as_deref(),
        present,
        init_default: init_default.as_deref(),
        ..Evidence::default()
    }) else {
        return Err(tried);
    };

    // Prefer the fetched remote tip, so a stale local copy is not the base.
    let refname = if rev_parse_verify(cwd, &format!("origin/{branch}")) {
        format!("origin/{branch}")
    } else {
        branch.clone()
    };
    Ok((branch, refname))
}

/// A git config value, or `None` when it is unset or empty.
#[allow(clippy::disallowed_methods)] // Running git is this function's job.
fn config_value(cwd: &Path, key: &str) -> Option<String> {
    let output = git_command(cwd)
        .args(["config", key])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[allow(clippy::disallowed_methods)] // Running git is this function's job.
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
/// We pull the `refs/heads/<name>` from the symref line.  Runs as a
/// cancellable child: `.ok()?` folds a cancel into the same `None` a
/// network failure already produces, since `resolve_base_branch` re-checks
/// cancellation before trusting whatever it decides in response.
fn query_origin_head(cwd: &Path, blocking: &jobs::Blocking) -> Option<String> {
    let mut cmd = git_command(cwd);
    let output = blocking
        .run_cancellable(
            cmd.args(["ls-remote", "--symref", "origin", "HEAD"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
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

#[cfg(test)]
// Fixtures drive real processes and wait on them; no frame is pending.
#[allow(clippy::disallowed_methods)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;
    use std::path::Path;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use alacritree_common::{command_ext, jobs};
    use alacritree_vcs::{
        Checkout, CreateCheckout, Head, RemoveCheckout, VcsError, VersionControl,
    };

    use super::*;
    use crate::GitBackend;

    fn backend() -> GitBackend {
        GitBackend
    }

    #[test]
    fn list_branches_returns_locals_then_origin_remotes() {
        let dir = tempfile::TempDir::new().unwrap();
        let git = |args: &[&str]| {
            let status = command_ext::hidden("git")
                .current_dir(dir.path())
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-b", "main"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-m", "x"]);
        git(&["branch", "develop"]);

        let bare = tempfile::TempDir::new().unwrap();
        let git_bare = |args: &[&str]| {
            let status = command_ext::hidden("git")
                .current_dir(bare.path())
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        };
        // Pin the bare repo's HEAD to main: `set-head -a` below asks the
        // remote for its HEAD, which otherwise dangles on machines whose
        // init.defaultBranch is not main (only main is pushed).
        git_bare(&["init", "--bare", "-b", "main"]);
        let bare_path = bare.path().to_str().unwrap();
        git(&["remote", "add", "origin", bare_path]);
        git(&["push", "origin", "main"]);
        git(&["fetch", "origin"]);
        git(&["remote", "set-head", "origin", "-a"]);

        let branches = jobs::on_this_thread(|blocking| backend().names(dir.path(), blocking))
            .expect("listing succeeds");

        assert!(branches.contains(&"develop".to_string()), "{branches:?}");
        assert!(branches.contains(&"main".to_string()), "{branches:?}");
        assert!(branches.contains(&"origin/main".to_string()), "{branches:?}");
        assert!(!branches.contains(&"origin".to_string()), "HEAD alias leaked: {branches:?}");

        let last_local = branches.iter().rposition(|b| !b.starts_with("origin/")).unwrap();
        let first_remote = branches.iter().position(|b| b.starts_with("origin/")).unwrap();
        assert!(
            last_local < first_remote,
            "local branches must all precede origin/* entries: {branches:?}"
        );
    }

    #[test]
    fn list_branches_reports_a_non_repo_as_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(jobs::on_this_thread(|blocking| backend().names(dir.path(), blocking)).is_err());
    }

    /// A cancel that kills `ls-remote` must end the resolve there. Probing each
    /// candidate name afterwards is a dozen more git spawns, which a loaded
    /// machine turns into seconds the gone caller is still made to wait.
    #[test]
    fn a_cancelled_resolve_probes_no_branch_names() {
        let tmp = tempfile::tempdir().unwrap();
        let project = crate::test_support::clone_with_origin(tmp.path());
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (out_tx, out_rx) = mpsc::channel();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
            let _ = started_tx.send(());
            let _ = go_rx.recv();
            let _ = out_tx.send(resolve_base_branch(&project, Some("main"), blocking));
        });
        started_rx.recv_timeout(Duration::from_secs(5)).expect("the worker started");
        drop(job);
        go_tx.send(()).unwrap();
        let resolved = out_rx.recv_timeout(Duration::from_secs(10)).expect("resolve returned");
        assert!(resolved.is_err(), "a cancelled resolve went on to pick {resolved:?}");
    }

    /// The `ls-remote` inside `resolve_base_branch` is a second network round
    /// trip ahead of the fetch.  A cancel landing while it is still waiting on
    /// an unresponsive remote must not be left to hang the way the fetch used
    /// to before it was routed through `run_git_cancellable`.
    #[test]
    fn prepare_stops_while_resolving_the_base_branch() {
        // The waits bound a hang, not speed: git launches take seconds on a
        // loaded Windows host.
        const HANG: Duration = Duration::from_secs(30);
        // Stands in for an unreachable `origin`: accepts the connection
        // `ls-remote` opens and never answers, so the client blocks on read
        // exactly as it would against a remote that never responds.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let port = listener.local_addr().expect("listener has an address").port();
        let (conn_tx, conn_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = conn_tx.send(());
                // Drain whatever the client sends without ever replying,
                // until it closes the connection (killed or otherwise). A
                // single short read returns as soon as *any* bytes arrive
                // and would drop the connection right after the client's
                // request, well before the read it actually blocks on.
                let mut buf = [0u8; 256];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            }
        });

        let tmp = tempfile::tempdir().expect("temp dir");
        let repo_dir = tmp.path().join("repo");
        drop(crate::test_support::init_repo(&repo_dir));
        let status = git_command(&repo_dir)
            .args(["remote", "add", "origin", &format!("git://127.0.0.1:{port}/repo.git")])
            .status()
            .expect("git runs");
        assert!(status.success());

        let (tx, rx) = mpsc::channel();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
            let _ =
                tx.send(backend().prepare_checkout(&repo_dir, Some("main"), &mut |_| {}, blocking));
        });
        // Cancelling only once the fake remote has observed a connection
        // proves the job is genuinely blocked in `ls-remote`, not merely
        // queued or still on an earlier step.
        conn_rx.recv_timeout(HANG).expect("ls-remote never connected");
        drop(job);
        let result = rx.recv_timeout(HANG);
        match result {
            Ok(Err(VcsError::Cancelled { .. })) => {},
            Ok(Err(e)) => panic!("prepare failed for the wrong reason: {e}"),
            Ok(Ok(base)) => panic!("prepare resolved a base nobody was waiting for: {base:?}"),
            Err(e) => panic!("prepare never returned: {e}"),
        }
    }

    #[test]
    fn prepare_reports_the_first_four_steps_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let main = crate::test_support::clone_with_origin(dir.path());
        let mut steps = Vec::new();
        let base = jobs::on_this_thread(|b| {
            backend().prepare_checkout(&main, None, &mut |s| steps.push(s.to_string()), b)
        })
        .unwrap();
        assert_eq!(base.revision, format!("origin/{}", base.name));
        assert_eq!(steps.len(), 4, "{steps:?}");
        assert!(steps[0].starts_with("Syncing with remote"));
        assert!(steps[3].starts_with("Creating git worktree"));
    }

    #[test]
    fn create_makes_the_target_on_a_new_branch() {
        let dir = tempfile::tempdir().unwrap();
        let main = crate::test_support::clone_with_origin(dir.path());
        let base =
            jobs::on_this_thread(|b| backend().prepare_checkout(&main, None, &mut |_| {}, b))
                .unwrap();
        let req = CreateCheckout {
            main: main.clone(),
            target: dir.path().join("wt-new"),
            name: "new".into(),
            base,
        };
        jobs::on_this_thread(|b| backend().create_checkout(&req, b)).unwrap();
        assert!(req.target.join(".git").exists());
        assert!(Path::new(&main).join(".git").join("refs").join("heads").join("new").exists());
    }

    /// A checkout discovery found gone, as the delete dialog hands it over.
    fn checkout(path: &Path, name: &str, gone: bool) -> Checkout {
        Checkout {
            name: name.into(),
            path: path.to_path_buf(),
            head: Head { name: Some(name.into()), ..Head::default() },
            is_main: false,
            gone,
            upstream: None,
        }
    }

    fn remove(main: &Path, checkout: Checkout, delete_name: bool) -> Result<(), VcsError> {
        let req = RemoveCheckout { main: main.to_path_buf(), checkout, force: false, delete_name };
        jobs::on_this_thread(|b| backend().remove_checkout(&req, b))
    }

    #[test]
    fn prune_removes_stale_metadata_and_keeps_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = crate::test_support::init_repo(&tmp.path().join("repo"));
        let wt_path = crate::test_support::add_worktree(&repo_dir, "stale");
        std::fs::remove_dir_all(&wt_path).unwrap();

        remove(&repo_dir, checkout(&wt_path, "stale", true), false).unwrap();

        let repo = git2::Repository::open(&repo_dir).unwrap();
        assert!(repo.find_worktree("stale").is_err());
        assert!(repo.find_branch("stale", git2::BranchType::Local).is_ok());
    }

    #[test]
    fn prune_deletes_branch_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = crate::test_support::init_repo(&tmp.path().join("repo"));
        let wt_path = crate::test_support::add_worktree(&repo_dir, "stale");
        std::fs::remove_dir_all(&wt_path).unwrap();

        remove(&repo_dir, checkout(&wt_path, "stale", true), true).unwrap();

        let repo = git2::Repository::open(&repo_dir).unwrap();
        assert!(repo.find_worktree("stale").is_err());
        assert!(repo.find_branch("stale", git2::BranchType::Local).is_err());
    }

    /// Discovery can be stale: a directory that came back since is refused
    /// rather than swept.
    #[test]
    fn prune_refuses_a_live_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = crate::test_support::init_repo(&tmp.path().join("repo"));
        let wt_path = crate::test_support::add_worktree(&repo_dir, "live");

        assert!(remove(&repo_dir, checkout(&wt_path, "live", true), false).is_err());

        let repo = git2::Repository::open(&repo_dir).unwrap();
        assert!(repo.find_worktree("live").is_ok());
        assert!(repo.find_branch("live", git2::BranchType::Local).is_ok());
    }

    #[test]
    fn removing_a_worktree_with_untracked_files_is_refused_as_unsaved() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = crate::test_support::init_repo(&tmp.path().join("repo"));
        let wt_path = crate::test_support::add_worktree(&repo_dir, "dirty");
        std::fs::write(wt_path.join("new.txt"), "x").unwrap();

        let result = remove(&repo_dir, checkout(&wt_path, "dirty", false), false);

        assert!(matches!(result, Err(VcsError::Unsaved { .. })), "{result:?}");
        assert!(wt_path.exists());
    }

    #[test]
    fn refused_for_unsaved_work_matches_a_real_git_refusal() {
        assert!(refused_for_unsaved_work(
            "fatal: '../wt1' contains modified or untracked files, use --force to delete it"
        ));
    }

    #[test]
    fn refused_for_unsaved_work_ignores_unrelated_failures() {
        assert!(!refused_for_unsaved_work("fatal: '../wt1' is a main working tree"));
    }

    /// A worktree path that happens to contain the matched phrase must not
    /// turn an unrelated failure into a false "needs --force" prompt --
    /// `refused_for_unsaved_work` only reads the text after the closing
    /// quote of the path, never the quoted path itself.
    #[test]
    fn refused_for_unsaved_work_is_not_fooled_by_a_path_spelling_out_the_phrase() {
        let path = "../is dirty, use --force to delete it";
        assert!(!refused_for_unsaved_work(&format!(
            "fatal: '{path}' cannot be locked: filesystem error"
        )));
    }
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
        assert!(matches!(git_path_arg(repo, path), Err(VcsError::BadPath(_))));
    }
}
