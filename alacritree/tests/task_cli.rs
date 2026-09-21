//! `alacritree task` answers from a plain shell with no window running,
//! which is how an agent calls it.

use std::path::Path;
use std::process::Output;

use alacritree::command_ext::hidden;

// A test has no UI thread for a blocking wait to stall.
#[allow(clippy::disallowed_methods)]
fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    hidden(program)
        .args(args)
        .current_dir(dir)
        .env("APPDATA", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env_remove("CODEX_SESSION_ID")
        .env("CLAUDE_CODE_SESSION_ID", "abc.1")
        .output()
        .expect("the program runs")
}

/// A repository named `myrepo` on branch `trunk`, or `None` without git.
fn repo() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let repo = dir.path().join("myrepo");
    std::fs::create_dir(&repo).expect("the repo dir");
    let init = run(&repo, "git", &["init", "-q", "-b", "trunk"]);
    init.status.success().then_some((dir, repo))
}

#[test]
fn scope_names_the_workspace_and_the_agent_session() {
    let Some((_dir, repo)) = repo() else { return };
    let out = run(&repo, env!("CARGO_BIN_EXE_alacritree"), &["task", "scope"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "myrepo.trunk.claude-abc-1");
}

#[test]
fn scope_as_json_carries_the_project() {
    let Some((_dir, repo)) = repo() else { return };
    let out = run(&repo, env!("CARGO_BIN_EXE_alacritree"), &["--json", "task", "scope"]);
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(reply["project"], "myrepo.trunk.claude-abc-1");
}
