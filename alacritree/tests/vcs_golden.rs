//! Outputs a git user sees, pinned before git moved behind a trait.
//!
//! Every fixture is built with the git CLI under fixed identities and dates,
//! so commit ids, and with them the detached worktree's short OID, are the
//! same on every run.

use std::path::{Path, PathBuf};
use std::process::Output;

use alacritree_common::command_ext::hidden;

const UPDATE: &str = "ALACRITREE_UPDATE_GOLDEN";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_alacritree")
}

// A test has no UI thread for a blocking wait to stall.
#[allow(clippy::disallowed_methods)]
fn git(dir: &Path, args: &[&str]) {
    let out = hidden("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "golden")
        .env("GIT_AUTHOR_EMAIL", "golden@example.com")
        .env("GIT_COMMITTER_NAME", "golden")
        .env("GIT_COMMITTER_EMAIL", "golden@example.com")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", dir.join("no-global-config"))
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// The CLI with its state, config, worktrees, git config and socket all
/// inside `home`, so the developer's real profile and any running instance
/// are never reached. Windows finds the home directory through
/// `USERPROFILE`, not `HOME`, and git reads a system config outside both, so
/// each is pointed away explicitly.
// A test has no UI thread for a blocking wait to stall.
#[allow(clippy::disallowed_methods)]
fn cli(home: &Path, args: &[&str]) -> Output {
    let config = home.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let worktrees = home.join("worktrees").display().to_string().replace('\\', "/");
    std::fs::write(
        config.join("alacritree.toml"),
        format!("[workspace]\nworktree_dir = '{worktrees}'\n"),
    )
    .unwrap();
    hidden(binary())
        .arg("--socket")
        .arg(home.join("nothing-listens-here.sock"))
        .arg("--config-dir")
        .arg(&config)
        .args(args)
        .env("LOCALAPPDATA", home)
        .env("APPDATA", home)
        .env("XDG_STATE_HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("no-global-config"))
        .env_remove("ALACRITREE_SOCKET")
        .output()
        .expect("the binary runs")
}

struct Fixture {
    home: tempfile::TempDir,
    root: PathBuf,
    feature: PathBuf,
    detached: PathBuf,
    plain: PathBuf,
}

/// A clone of a bare origin whose `main` has two commits, a `feature`
/// worktree one commit ahead with staged, unstaged and untracked changes, a
/// detached worktree, and a plain folder beside them.
fn fixture() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let base = home.path().join("code");
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "a.txt"]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    std::fs::write(seed.join("b.txt"), "two\n").unwrap();
    git(&seed, &["add", "b.txt"]);
    git(&seed, &["commit", "-q", "-m", "second"]);
    git(&base, &["clone", "-q", "--bare", "seed", "origin.git"]);
    git(&base, &["clone", "-q", "origin.git", "project"]);
    let root = base.join("project");
    let feature = base.join("wt-feature");
    git(&root, &["worktree", "add", "-q", "-b", "feature", "../wt-feature"]);
    std::fs::write(feature.join("c.txt"), "three\nfour\n").unwrap();
    git(&feature, &["add", "c.txt"]);
    git(&feature, &["commit", "-q", "-m", "feature work"]);
    std::fs::write(feature.join("staged.txt"), "s\n").unwrap();
    git(&feature, &["add", "staged.txt"]);
    std::fs::write(feature.join("a.txt"), "one\nchanged\n").unwrap();
    std::fs::write(feature.join("untracked.txt"), "u\n").unwrap();
    let detached = base.join("wt-detached");
    git(&root, &["worktree", "add", "-q", "--detach", "../wt-detached", "HEAD~1"]);
    let plain = base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    Fixture { home, root, feature, detached, plain }
}

/// Temp paths differ per run and per OS, so they become `<home>` and forward
/// slashes before comparing. Windows may hand out the temp dir as an 8.3
/// short path while the CLI prints the canonical long one, so both forms are
/// replaced.
fn normalize(text: &str, home: &Path) -> String {
    let canonical = home.canonicalize().unwrap();
    let canonical = canonical.display().to_string();
    let mut out = text.to_string();
    for form in [home.display().to_string(), canonical.trim_start_matches(r"\\?\").to_string()] {
        for v in [form.replace('\\', "/"), form.replace('\\', "\\\\"), form] {
            out = out.replace(v.as_str(), "<home>");
        }
    }
    out.replace("\\\\", "/").replace('\\', "/").replace("\r\n", "\n")
}

fn assert_golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden").join(name);
    let committed = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
    if committed == actual {
        return;
    }
    if std::env::var(UPDATE).as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let (line, was, now) = committed
        .lines()
        .zip(actual.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map_or((0, "", ""), |(i, (a, b))| (i + 1, a, b));
    panic!(
        "{} changed. It pins what git users see and is regenerated only before a refactor, with \
         `{UPDATE}=1`.\n\nfirst difference at line {line}:\n  committed: {was}\n  now:       {now}",
        path.display()
    );
}

fn stdout(out: &Output, home: &Path) -> String {
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    normalize(&String::from_utf8_lossy(&out.stdout), home)
}

#[test]
fn git_status_of_a_feature_worktree() {
    let f = fixture();
    let home = f.home.path();
    let path = f.feature.to_str().unwrap();
    assert_golden("git-status.json", &stdout(&cli(home, &["--json", "git-status", path]), home));
    assert_golden("git-status.txt", &stdout(&cli(home, &["git-status", path]), home));
}

#[test]
fn git_status_of_a_detached_worktree() {
    let f = fixture();
    let home = f.home.path();
    let path = f.detached.to_str().unwrap();
    assert_golden(
        "git-status-detached.json",
        &stdout(&cli(home, &["--json", "git-status", path]), home),
    );
}

#[test]
fn git_status_of_a_plain_folder() {
    let f = fixture();
    let home = f.home.path();
    let path = f.plain.to_str().unwrap();
    assert_golden(
        "git-status-plain.json",
        &stdout(&cli(home, &["--json", "git-status", path]), home),
    );
}

#[test]
fn project_list_and_state_file() {
    let f = fixture();
    let home = f.home.path();
    for root in [&f.root, &f.plain] {
        stdout(&cli(home, &["project", "add", root.to_str().unwrap()]), home);
    }
    assert_golden("project-list.json", &stdout(&cli(home, &["--json", "project", "list"]), home));
    assert_golden("project-list.txt", &stdout(&cli(home, &["project", "list"]), home));
    let state = std::fs::read_to_string(home.join("alacritree").join("state.toml"))
        .expect("state.toml lands inside the isolated home");
    // toml writes a path holding backslashes as a literal string, so Windows
    // quotes the roots with `'` where Linux uses `"`.
    assert_golden("state.toml", &normalize(&state, home).replace('\'', "\""));
}

#[test]
fn worktree_create_reports_the_same_steps() {
    let f = fixture();
    let home = f.home.path();
    let out =
        cli(home, &["--json", "worktree", "create", f.root.to_str().unwrap(), "golden-branch"]);
    let reply: serde_json::Value = serde_json::from_str(&stdout(&out, home)).unwrap();
    let path = reply["path"].as_str().expect("the reply names the new worktree");
    assert!(path.starts_with("<home>/worktrees/"), "the worktree escaped the test's home: {path}");
    assert_golden("worktree-create-steps.json", &format!("{:#}\n", reply["steps"]));
}
