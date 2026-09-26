//! `alacritree hook` as a harness runs it: a fresh process, a payload on
//! stdin, JSON or nothing on stdout, and exit 0 whatever happens.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use alacritree_common::command_ext::hidden;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_alacritree")
}

// A test has no UI thread for a blocking wait to stall.
#[allow(clippy::disallowed_methods)]
fn succeeds(program: &str, args: &[&str]) -> bool {
    hidden(program).args(args).output().is_ok_and(|o| o.status.success())
}

/// Runs a setup step that must succeed, and fails with what it printed when
/// it does not. wsl.exe prints its own errors on stdout, in UTF-16 unless
/// `WSL_UTF8` is set.
#[allow(clippy::disallowed_methods)]
fn run(program: &str, args: &[&str]) {
    let out = hidden(program).env("WSL_UTF8", "1").args(args).output().expect("setup runs");
    assert!(
        out.status.success(),
        "{program} {args:?} failed ({}): {}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository, a private taskrc and a private config dir, so nothing the
/// developer owns is read or written. Taskwarrior 3 has no Windows build, so
/// a host without a native one keeps the repository and taskrc inside the
/// default distro, where the hook's own side detection sends `task`.
struct Sandbox {
    state: tempfile::TempDir,
    _work: tempfile::TempDir,
    repo: PathBuf,
    env: Vec<(&'static str, String)>,
}

fn with_task() -> Option<Sandbox> {
    let state = tempfile::tempdir().unwrap();
    if succeeds("task", &["--version"]) {
        let work = tempfile::tempdir().unwrap();
        let repo = work.path().join("myrepo");
        std::fs::create_dir(&repo).unwrap();
        run("git", &["-C", repo.to_str()?, "init", "-q", "-b", "main"]);
        std::fs::write(work.path().join("taskrc"), "").unwrap();
        let root = work.path().to_str()?.to_string();
        let env = vec![("TASKRC", format!("{root}/taskrc")), ("TASKDATA", format!("{root}/data"))];
        return Some(Sandbox { state, _work: work, repo, env });
    }
    let distro = alacritree_common::wsl::distros().into_iter().find(|d| d.is_default)?.name;
    if !succeeds("wsl.exe", &["-d", &distro, "-e", "sh", "-lc", "command -v task"]) {
        return None;
    }
    let tmp = PathBuf::from(format!(r"\\wsl.localhost\{distro}\tmp"));
    let work = tempfile::Builder::new().prefix("alacritree-hook").tempdir_in(tmp).ok()?;
    let root = alacritree_common::wsl::windows_to_linux(work.path())?;
    let repo = work.path().join("myrepo");
    std::fs::create_dir(&repo).unwrap();
    let linux_repo = format!("{root}/myrepo");
    let init = ["-d", &distro, "-e", "git", "-C", &linux_repo, "init", "-q", "-b", "main"];
    run("wsl.exe", &init);
    std::fs::write(work.path().join("taskrc"), "").unwrap();
    let env = vec![
        ("TASKRC", format!("{root}/taskrc")),
        ("TASKDATA", format!("{root}/data")),
        ("WSLENV", "TASKRC:TASKDATA".to_string()),
    ];
    Some(Sandbox { state, _work: work, repo, env })
}

/// A repository on this machine and no taskwarrior settings at all.
fn without_task() -> Sandbox {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("myrepo");
    std::fs::create_dir(&repo).unwrap();
    succeeds("git", &["-C", repo.to_str().unwrap(), "init", "-q", "-b", "main"]);
    Sandbox { state, _work: work, repo, env: Vec::new() }
}

fn fixture(name: &str, cwd: &Path) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hook").join(name);
    let raw = std::fs::read_to_string(path).unwrap();
    raw.replace("\"CWD\"", &serde_json::to_string(cwd.to_str().unwrap()).unwrap())
}

fn hook(sandbox: &Sandbox, args: &[&str], stdin: &str) -> Output {
    let home = sandbox.state.path();
    #[allow(clippy::disallowed_methods)]
    let mut child = hidden(binary())
        .args(args)
        .envs(sandbox.env.iter().map(|(k, v)| (*k, v.as_str())))
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_STATE_HOME", home)
        .env("APPDATA", home)
        .env("LOCALAPPDATA", home)
        .env("HOME", home)
        .current_dir(&sandbox.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    #[allow(clippy::disallowed_methods)]
    child.wait_with_output().unwrap()
}

#[test]
fn session_start_prints_one_json_object_for_both_harnesses() {
    for (harness, file) in
        [("claude", "claude-session-start.json"), ("codex", "codex-session-start.json")]
    {
        let Some(sandbox) = with_task() else { return };
        let args = ["hook", "session-start", "--harness", harness];
        let out = hook(&sandbox, &args, &fixture(file, &sandbox.repo));
        assert!(out.status.success());
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!("stdout is JSON ({e}): {}", String::from_utf8_lossy(&out.stderr))
        });
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "SessionStart");
        let ctx = json["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(ctx.contains(&format!("myrepo.main.{harness}-")), "{ctx}");
    }
}

/// A harness inside WSL reports its cwd as a Linux path, and the Windows
/// binary it runs through interop starts in the same directory's UNC form.
#[test]
fn a_linux_cwd_from_inside_wsl_names_the_workspace() {
    let Some(sandbox) = with_task() else { return };
    let Some(linux) = alacritree_common::wsl::windows_to_linux(&sandbox.repo) else { return };
    if !linux.starts_with('/') || !cfg!(windows) {
        return;
    }
    let args = ["hook", "session-start", "--harness", "claude"];
    let out = hook(&sandbox, &args, &fixture("claude-session-start.json", Path::new(&linux)));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ctx = json["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("myrepo.main.claude-"), "{ctx}");
}

#[test]
fn an_unchanged_list_is_injected_once() {
    for (harness, file) in
        [("claude", "claude-user-prompt-submit.json"), ("codex", "codex-user-prompt-submit.json")]
    {
        let Some(sandbox) = with_task() else { return };
        let args = ["hook", "user-prompt-submit", "--harness", harness];
        let first = hook(&sandbox, &args, &fixture(file, &sandbox.repo));
        assert!(!first.stdout.is_empty(), "the first prompt sees the list");
        let second = hook(&sandbox, &args, &fixture(file, &sandbox.repo));
        assert!(second.status.success());
        assert!(second.stdout.is_empty(), "an unchanged list is not repeated");
    }
}

#[test]
fn a_missing_task_binary_prints_nothing_and_exits_zero() {
    let sandbox = without_task();
    let args = [
        "-o",
        "integrations.taskwarrior.path='alacritree-no-such-task-binary'",
        "-o",
        "integrations.taskwarrior.wsl_path='alacritree-no-such-task-binary'",
        "hook",
        "session-start",
        "--harness",
        "claude",
    ];
    let out = hook(&sandbox, &args, &fixture("claude-session-start.json", &sandbox.repo));
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
}

/// A configured command answers the hook in taskwarrior's place, guide and
/// lists both.
#[cfg(unix)]
#[test]
fn a_task_command_answers_the_hook() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = without_task();
    let bin = sandbox.state.path().join("todo");
    std::fs::write(&bin, "#!/bin/sh\ncat \"$(dirname \"$0\")/tasks.json\"\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tasks = r#"[{"id":"t1","description":"from the command","status":"pending","project":"myrepo.main"},
        {"id":"t2","description":"another repo","status":"pending","project":"other"}]"#;
    std::fs::write(sandbox.state.path().join("tasks.json"), tasks).unwrap();
    let path = format!("integrations.tasks.command.path='{}'", bin.display());
    let args = [
        "-o",
        "integrations.tasks.command.enabled=true",
        "-o",
        &path,
        "-o",
        "integrations.tasks.command.list=['list']",
        "-o",
        "integrations.tasks.command.agent_guide='Use todo for {project}.'",
        "hook",
        "session-start",
        "--harness",
        "claude",
    ];
    let out = hook(&sandbox, &args, &fixture("claude-session-start.json", &sandbox.repo));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("stdout is JSON ({e}): {}", String::from_utf8_lossy(&out.stderr))
    });
    let ctx = json["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(ctx.starts_with("Use todo for myrepo.main.claude-"), "{ctx}");
    assert!(ctx.contains("from the command"), "{ctx}");
    assert!(!ctx.contains("another repo"), "{ctx}");
}

#[test]
fn garbage_stdin_exits_zero() {
    let sandbox = without_task();
    for stdin in ["", "not json", "{}", "[1,2]"] {
        let out = hook(&sandbox, &["hook", "session-start", "--harness", "codex"], stdin);
        assert!(out.status.success(), "{stdin:?}");
        if !out.stdout.is_empty() {
            serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("JSON when printed");
        }
    }
}
