//! `alacritree doctor` — a look at everything alacritree needs but never
//! complains about.
//!
//! Most of what alacritree depends on is deliberately best-effort: a missing
//! `gh` falls back to the repo's default branch, a missing `doppler` skips
//! scope mirroring, a malformed `alacritty.toml` loads defaults, and a corrupt
//! `state.toml` opens an empty sidebar.  Every one of those is the right call in
//! the app — none of them should stop a terminal from opening — but together
//! they mean a broken setup looks exactly like a working one.  This is the one
//! place that says so out loud.
//!
//! It answers without a running instance, because "nothing happens when I run
//! it" is precisely when it gets used.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::app::{ShellDecision, shell_decision};
use crate::command_ext::CommandExt;
use crate::config::{self, Config, ConfigDiagnosis, ConfigFile, Profile, ShellConfig};
use crate::ipc::{self, IpcRequest, SendError};
use crate::state;
use crate::wsl::{self, ShellChoice};

/// An instance that is wedged should not wedge the report too.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    section: &'static str,
    name: String,
    status: Status,
    detail: String,
}

/// An external program alacritree shells out to.
struct Tool {
    program: &'static str,
    /// What stops working when it is missing.  The app never says this, so the
    /// report has to.
    consequence: &'static str,
    need: Need,
}

/// How much a missing tool matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Nothing alacritree exists for works without it.
    Required,
    /// A feature everyone uses degrades quietly.  Worth a warning anywhere.
    Optional,
    /// Only drives a feature this machine has not opted into.  Warning about it
    /// would fire on every machine that has simply never wanted it, and a report
    /// that always has a warning in it stops being read.
    Unused,
}

/// Where a program lives and what it calls itself.
struct Found {
    path: PathBuf,
    version: Option<String>,
}

pub fn run(as_json: bool, socket: Option<&Path>) -> i32 {
    let checks = report(socket);
    if as_json {
        println!("{:#}", to_json(&checks));
    } else {
        print_human(&checks);
    }
    exit_code(&checks)
}

fn report(socket: Option<&Path>) -> Vec<Check> {
    let config = config::load();

    let mut checks = binary_checks();
    checks.extend(gh_auth_check());
    checks.push(shell_check(config.shell.as_ref()));
    checks.extend(config_checks(&config::diagnose()));
    checks.extend(persisted_state_checks(&config));
    checks.extend(ipc_checks(socket, config.ipc_socket));
    checks
}

fn tools() -> Vec<Tool> {
    let mut tools = vec![
        Tool {
            program: "git",
            consequence: "worktree creation and default-branch detection fail",
            need: Need::Required,
        },
        Tool {
            program: "gh",
            consequence: "PR base branches fall back to the repo default",
            need: Need::Optional,
        },
        Tool {
            program: "doppler",
            consequence: "new worktrees do not inherit the main checkout's scopes",
            need: doppler_need(doppler_configured()),
        },
    ];
    if cfg!(target_os = "linux") {
        tools.push(Tool {
            program: "xdg-open",
            consequence: "clicked links do not open",
            need: Need::Optional,
        });
    }
    tools
}

fn binary_checks() -> Vec<Check> {
    tools().iter().map(|tool| tool_check(tool, find(tool.program))).collect()
}

fn tool_check(tool: &Tool, found: Option<Found>) -> Check {
    match found {
        Some(Found { path, version }) => {
            let version = version.unwrap_or_else(|| "unknown version".to_string());
            check("binaries", tool.program, Status::Ok, format!("{version}  {}", path.display()))
        },
        None => {
            let (status, detail) = match tool.need {
                Need::Required => (Status::Fail, format!("not on PATH — {}", tool.consequence)),
                Need::Optional => (Status::Warn, format!("not on PATH — {}", tool.consequence)),
                Need::Unused => (Status::Ok, "not installed, and unused here".to_string()),
            };
            check("binaries", tool.program, status, detail)
        },
    }
}

/// Doppler scope mirroring only matters to someone who uses Doppler, and the
/// only evidence of that is its config file — the CLI writes one on first
/// `doppler setup`, and `doppler.rs` reads scopes straight out of it.
fn doppler_need(configured: bool) -> Need {
    if configured { Need::Optional } else { Need::Unused }
}

fn doppler_configured() -> bool {
    home::home_dir().is_some_and(|home| home.join(".doppler").join(".doppler.yaml").is_file())
}

/// `gh` present but logged out fails exactly the way a missing `gh` does —
/// silently — so it needs saying separately.
fn gh_auth_check() -> Option<Check> {
    locate("gh")?;
    let authenticated = Command::new("gh")
        .hide_console()
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());

    Some(if authenticated {
        check("binaries", "gh auth", Status::Ok, "authenticated")
    } else {
        let detail = "not authenticated — PR base branches fall back to the repo default";
        check("binaries", "gh auth", Status::Warn, detail)
    })
}

/// A configured shell that cannot be resolved takes every session with it: the
/// PTY spawn fails and the terminal dies as soon as it opens.
fn shell_check(shell: Option<&ShellConfig>) -> Check {
    let Some(shell) = shell else {
        return check("binaries", "shell", Status::Ok, "the system default");
    };
    match locate(&shell.program) {
        Some(path) => check("binaries", "shell", Status::Ok, path.display().to_string()),
        None => {
            let detail = format!("{} is not on PATH — sessions cannot start", shell.program);
            check("binaries", "shell", Status::Fail, detail)
        },
    }
}

fn config_checks(diagnosis: &ConfigDiagnosis) -> Vec<Check> {
    let mut checks: Vec<Check> = diagnosis.files.iter().map(config_file_check).collect();
    checks.push(match &diagnosis.schema_error {
        Some(e) => {
            let detail = format!("every setting in both files is ignored: {e}");
            check("config", "schema", Status::Fail, detail)
        },
        None => check("config", "schema", Status::Ok, "settings load"),
    });
    checks
}

fn config_file_check(file: &ConfigFile) -> Check {
    let name = format!("{}.toml", file.stem);
    match (&file.path, &file.error) {
        (Some(path), Some(e)) => {
            let detail = format!("ignored, using defaults ({})\n{e}", path.display());
            check("config", name, Status::Fail, detail)
        },
        (Some(path), None) => check("config", name, Status::Ok, path.display().to_string()),
        (None, _) => check("config", name, Status::Ok, "not found — built-in defaults"),
    }
}

fn persisted_state_checks(config: &Config) -> Vec<Check> {
    let Some(path) = state::config_path() else {
        let detail = "no config directory — the sidebar cannot persist";
        return vec![check("state", "state.toml", Status::Fail, detail)];
    };
    let distros: Vec<String> = wsl::distros().into_iter().map(|d| d.name).collect();
    state_checks(&path, &distros, &config.profiles)
}

fn state_checks(path: &Path, distros: &[String], profiles: &[Profile]) -> Vec<Check> {
    if let Some(e) = state::parse_error(path) {
        let detail = format!("unreadable, the sidebar opens empty ({})\n{e}", path.display());
        return vec![check("state", "state.toml", Status::Fail, detail)];
    }

    let projects = state::load_from(path).projects;
    let plural = if projects.len() == 1 { "" } else { "s" };
    let summary = format!("{} project{plural}  {}", projects.len(), path.display());
    let mut checks = vec![check("state", "state.toml", Status::Ok, summary)];

    for project in &projects {
        let Some(raw) = project.shell.as_deref() else {
            continue;
        };
        if let Some(problem) = ignored_override(raw, distros, profiles) {
            let detail = format!("{}: {problem}", project.root.display());
            checks.push(check("state", "shell override", Status::Warn, detail));
        }
    }

    for project in projects.iter().filter(|p| !p.root.is_dir()) {
        let detail = format!("{} no longer exists", project.root.display());
        checks.push(check("state", "project root", Status::Warn, detail));
    }
    checks
}

/// Why a project's shell override is not being honoured, if it isn't.
///
/// A stale override never fails a spawn: `shell_decision` logs it and carries on
/// down the precedence chain, so the project quietly opens the automatic shell
/// instead of the one it was pinned to.  A value that does not even parse is
/// dropped earlier still, when the sidebar loads.
///
/// The verdict comes from `shell_decision` itself rather than from a second copy
/// of its rules, so this cannot drift from the behaviour it reports on.  Neither
/// a location distro nor a default profile is offered to it: both are fallbacks
/// the chain reaches *after* the override, and passing them would mask an
/// override that had already been passed over.
fn ignored_override(raw: &str, distros: &[String], profiles: &[Profile]) -> Option<String> {
    let Some(choice) = ShellChoice::parse(raw) else {
        return Some(format!("`{raw}` is not a shell override — the automatic shell is used"));
    };

    match (&choice, shell_decision(Some(&choice), None, distros, profiles, None)) {
        // Pinning to Windows *is* a decision to use the config shell.
        (ShellChoice::Windows, _) => None,
        (ShellChoice::Wsl(distro), ShellDecision::ConfigShell) => {
            Some(format!("WSL distro `{distro}` is not installed — the automatic shell is used"))
        },
        (ShellChoice::Profile(name), ShellDecision::ConfigShell) => {
            Some(format!("no `[[ui.profiles]]` entry named `{name}` — the automatic shell is used"))
        },
        _ => None,
    }
}

fn ipc_checks(socket: Option<&Path>, enabled: bool) -> Vec<Check> {
    let mut checks = Vec::new();

    if !enabled {
        let detail = "disabled in config — the CLI and MCP cannot reach a running window";
        checks.push(check("ipc", "ipc_socket", Status::Warn, detail));
    }
    checks.push(check("ipc", "socket dir", Status::Ok, ipc::socket_dir().display().to_string()));

    checks.push(match ipc::send_request(socket, &IpcRequest::ListProjects, PROBE_TIMEOUT) {
        Ok(_) => check("ipc", "instance", Status::Ok, "answering"),
        // Nothing running is not a fault: the CLI serves projects, git status
        // and worktrees from disk when no window is up.
        Err(SendError::NoInstance) => {
            check("ipc", "instance", Status::Ok, "none running — offline commands still work")
        },
        Err(SendError::Failed(e)) => {
            check("ipc", "instance", Status::Warn, format!("running but not answering: {e}"))
        },
    });
    checks
}

/// Resolve `program` the way the OS would: an explicit path as itself, a bare
/// name against each directory on the search path, trying each executable
/// extension (`PATHEXT` on Windows, none elsewhere).
fn locate_in(program: &str, dirs: &[PathBuf], exts: &[String]) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    dirs.iter().find_map(|dir| {
        exts.iter().find_map(|ext| {
            let candidate = dir.join(format!("{program}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

fn locate(program: &str) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    locate_in(program, &dirs, &executable_extensions())
}

/// The empty extension comes last on Windows too: `PATHEXT` covers `git.exe`,
/// but a bare extensionless file is still executable if it is there.
#[cfg(windows)]
fn executable_extensions() -> Vec<String> {
    let mut exts: Vec<String> = std::env::var("PATHEXT")
        .map(|v| v.split(';').map(str::to_lowercase).filter(|e| !e.is_empty()).collect())
        .unwrap_or_else(|_| vec![".exe".to_string()]);
    exts.push(String::new());
    exts
}

#[cfg(not(windows))]
fn executable_extensions() -> Vec<String> {
    vec![String::new()]
}

fn find(program: &str) -> Option<Found> {
    let path = locate(program)?;
    let version = version_of(&path);
    Some(Found { path, version })
}

/// A tool that is on PATH but broken (a shim, a half-installed package) fails
/// here rather than reporting a version, which is worth knowing on its own.
fn version_of(program: &Path) -> Option<String> {
    let output = Command::new(program)
        .hide_console()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

fn check(
    section: &'static str,
    name: impl Into<String>,
    status: Status,
    detail: impl Into<String>,
) -> Check {
    Check { section, name: name.into(), status, detail: detail.into() }
}

fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.status == Status::Fail))
}

fn print_human(checks: &[Check]) {
    const CONTINUATION: usize = 25;

    let mut section = "";
    for c in checks {
        if c.section != section {
            section = c.section;
            println!("{section}");
        }

        // A TOML error carries its own multi-line snippet pointing at the
        // offending column.  It is worth more than the alignment is, so it goes
        // under the row rather than into it.
        let mut lines = c.detail.lines();
        let summary = lines.next().unwrap_or_default();
        println!("  {:<4}  {:<15}  {summary}", c.status.as_str(), c.name);
        for line in lines {
            println!("{:CONTINUATION$}{line}", "");
        }
    }

    let count = |status| checks.iter().filter(|c| c.status == status).count();
    println!();
    match (count(Status::Fail), count(Status::Warn)) {
        (0, 0) => println!("no problems found"),
        (0, warnings) => println!("{warnings} warning(s), nothing broken"),
        (failures, _) => println!("{failures} problem(s) found"),
    }
}

fn to_json(checks: &[Check]) -> Value {
    let checks: Vec<Value> = checks
        .iter()
        .map(|c| {
            json!({
                "section": c.section,
                "name": c.name,
                "status": c.status.as_str(),
                "detail": c.detail,
            })
        })
        .collect();
    json!({ "ok": !checks.iter().any(|c| c["status"] == "fail"), "checks": checks })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::state::{PersistedProject, PersistedState};

    const GIT: Tool =
        Tool { program: "git", consequence: "worktree creation fails", need: Need::Required };
    const GH: Tool =
        Tool { program: "gh", consequence: "PR base branches fall back", need: Need::Optional };
    const DOPPLER: Tool =
        Tool { program: "doppler", consequence: "scopes are not mirrored", need: Need::Unused };

    fn status_of(checks: &[Check], name: &str) -> Status {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check named {name} in {:?}", names(checks)))
            .status
    }

    fn names(checks: &[Check]) -> Vec<&str> {
        checks.iter().map(|c| c.name.as_str()).collect()
    }

    fn touch(path: &Path) {
        std::fs::write(path, "").expect("write");
    }

    /// git is not optional: `worktree create` shells out to it, so a machine
    /// without it cannot do the one thing alacritree exists for.
    #[test]
    fn a_missing_required_tool_fails() {
        assert_eq!(tool_check(&GIT, None).status, Status::Fail);
    }

    /// Everything else degrades quietly and on purpose.  Reporting a missing
    /// `gh` as a failure would train people to ignore the report.
    #[test]
    fn a_missing_optional_tool_only_warns() {
        assert_eq!(tool_check(&GH, None).status, Status::Warn);
    }

    /// Doppler drives one optional feature, and most people have never wanted
    /// it.  Warning that it is absent would put a permanent warning on every
    /// machine that simply does not use Doppler — and a report that always has
    /// a warning in it is a report nobody reads.
    #[test]
    fn a_tool_this_machine_does_not_use_is_not_worth_warning_about() {
        assert_eq!(tool_check(&DOPPLER, None).status, Status::Ok);
    }

    /// The evidence is Doppler's own config file: it is written on `doppler
    /// setup`, and it is where `doppler.rs` reads scopes from.  Someone who has
    /// set Doppler up and then lost the binary does want to hear about it.
    #[test]
    fn doppler_is_only_worth_warning_about_once_it_has_been_set_up() {
        assert_eq!(doppler_need(true), Need::Optional);
        assert_eq!(doppler_need(false), Need::Unused);
    }

    /// A missing optional tool has to say what it costs, or the reader has no
    /// way to judge whether to install it.
    #[test]
    fn a_missing_tool_says_what_it_costs() {
        let detail = tool_check(&GH, None).detail;
        assert!(
            detail.contains("PR base branches fall back"),
            "{detail:?} does not say what a missing gh costs"
        );
    }

    /// The version and the path are the two things worth knowing when a tool is
    /// present but misbehaving — an old git, or a shim ahead of the real one.
    #[test]
    fn a_found_tool_reports_its_version_and_path() {
        let found =
            Found { path: PathBuf::from("/usr/bin/git"), version: Some("2.51.0".to_string()) };

        let check = tool_check(&GIT, Some(found));

        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("2.51.0"), "{:?} lacks the version", check.detail);
        assert!(check.detail.contains("/usr/bin/git"), "{:?} lacks the path", check.detail);
    }

    #[test]
    fn a_bare_name_is_found_on_the_search_path() {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join("tool.exe");
        touch(&exe);

        let found = locate_in("tool", &[dir.path().to_path_buf()], &[".exe".to_string()]);

        assert_eq!(found, Some(exe));
    }

    /// Unix has no executable extension, so the empty one has to be tried too —
    /// otherwise nothing is ever found there.
    #[test]
    fn a_bare_name_is_found_without_an_extension() {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join("tool");
        touch(&exe);

        let found = locate_in("tool", &[dir.path().to_path_buf()], &[String::new()]);

        assert_eq!(found, Some(exe));
    }

    #[test]
    fn a_name_that_is_not_on_the_path_is_not_found() {
        let dir = TempDir::new().unwrap();

        assert_eq!(locate_in("tool", &[dir.path().to_path_buf()], &[String::new()]), None);
    }

    /// A configured shell is usually an absolute path (`C:\...\pwsh.exe`), which
    /// must be checked where it points rather than hunted for on the path.
    #[test]
    fn a_program_with_a_path_is_not_searched_for_on_the_path() {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join("shell");
        touch(&exe);

        let found = locate_in(&exe.to_string_lossy(), &[], &[String::new()]);

        assert_eq!(found, Some(exe));
        assert_eq!(locate_in("/nowhere/shell", &[], &[String::new()]), None);
    }

    fn diagnosis(files: Vec<ConfigFile>, schema_error: Option<String>) -> ConfigDiagnosis {
        ConfigDiagnosis { files, schema_error }
    }

    /// The trap this whole command exists for: `config::load` logs the parse
    /// error and returns defaults, so a typo'd config behaves like no config at
    /// all.  Silence here would be a lie.
    #[test]
    fn a_config_that_does_not_parse_fails() {
        let d = diagnosis(
            vec![ConfigFile {
                stem: "alacritty",
                path: Some(PathBuf::from("/c/alacritty.toml")),
                error: Some("expected `=`".to_string()),
            }],
            None,
        );

        let checks = config_checks(&d);

        assert_eq!(status_of(&checks, "alacritty.toml"), Status::Fail);
        let detail = &checks[0].detail;
        assert!(detail.contains("expected `=`"), "{detail:?} does not quote the parse error");
    }

    /// Having no config is the default way to run alacritree, not a problem.
    #[test]
    fn a_config_that_is_absent_is_not_a_problem() {
        let d = diagnosis(vec![ConfigFile { stem: "alacritree", path: None, error: None }], None);

        assert_eq!(status_of(&config_checks(&d), "alacritree.toml"), Status::Ok);
    }

    #[test]
    fn a_config_that_parses_reports_where_it_came_from() {
        let d = diagnosis(
            vec![ConfigFile {
                stem: "alacritty",
                path: Some(PathBuf::from("/c/alacritty.toml")),
                error: None,
            }],
            None,
        );

        let checks = config_checks(&d);

        assert_eq!(status_of(&checks, "alacritty.toml"), Status::Ok);
        assert!(checks[0].detail.contains("/c/alacritty.toml"));
    }

    /// Both files can parse and still be thrown away wholesale: a value of the
    /// wrong type sends `load` down its defaults path, discarding every setting
    /// in both files.  That deserves its own check — the per-file ones are green.
    #[test]
    fn a_config_that_does_not_fit_the_schema_fails() {
        let d = diagnosis(
            vec![ConfigFile { stem: "alacritty", path: None, error: None }],
            Some("invalid type: string, expected f32".to_string()),
        );

        assert_eq!(status_of(&config_checks(&d), "schema"), Status::Fail);
    }

    #[test]
    fn a_config_that_fits_the_schema_has_nothing_to_say_about_it() {
        let d = diagnosis(vec![], None);

        assert_eq!(status_of(&config_checks(&d), "schema"), Status::Ok);
    }

    fn state_with(dir: &TempDir, roots: &[PathBuf]) -> PathBuf {
        let path = dir.path().join("state.toml");
        let projects = roots
            .iter()
            .map(|r| PersistedProject { root: r.clone(), expanded: true, shell: None })
            .collect();
        state::save_to(&path, &PersistedState { projects, ..PersistedState::default() });
        path
    }

    /// A project pinned to `shell`, on a machine with `distros` installed and
    /// `profiles` configured.
    fn state_pinned_to(dir: &TempDir, shell: &str) -> PathBuf {
        let path = dir.path().join("state.toml");
        let project = PersistedProject {
            root: dir.path().join("repo"),
            expanded: true,
            shell: Some(shell.to_string()),
        };
        let state = PersistedState { projects: vec![project], ..PersistedState::default() };
        state::save_to(&path, &state);
        path
    }

    fn profile(name: &str) -> Profile {
        Profile { name: name.to_string(), program: "bash".to_string(), args: Vec::new() }
    }

    /// The check has to survive the whole path a real override takes: written to
    /// `state.toml`, read back, and judged against this machine.
    #[test]
    fn a_project_pinned_to_an_uninstalled_distro_warns() {
        let dir = TempDir::new().unwrap();
        let path = state_pinned_to(&dir, "wsl:Ubuntu");

        let checks = state_checks(&path, &["Debian".to_string()], &[]);

        let warning = checks
            .iter()
            .find(|c| c.name == "shell override")
            .expect("a warning about the missing distro");
        assert_eq!(warning.status, Status::Warn);
        assert!(warning.detail.contains("Ubuntu"), "{:?} does not name the distro", warning.detail);
    }

    #[test]
    fn a_project_pinned_to_an_installed_distro_is_not_reported() {
        let dir = TempDir::new().unwrap();
        let path = state_pinned_to(&dir, "wsl:Ubuntu");

        let checks = state_checks(&path, &["Ubuntu".to_string()], &[]);

        assert!(!checks.iter().any(|c| c.name == "shell override"), "{:?}", names(&checks));
    }

    #[test]
    fn a_project_pinned_to_a_profile_that_was_deleted_from_config_warns() {
        let dir = TempDir::new().unwrap();
        let path = state_pinned_to(&dir, "profile:work");

        let checks = state_checks(&path, &[], &[profile("home")]);

        let warning = checks.iter().find(|c| c.name == "shell override").expect("a warning");
        assert_eq!(warning.status, Status::Warn);
        assert!(warning.detail.contains("work"), "{:?} does not name the profile", warning.detail);
    }

    #[test]
    fn a_project_pinned_to_a_profile_that_exists_is_not_reported() {
        let dir = TempDir::new().unwrap();
        let path = state_pinned_to(&dir, "profile:work");

        let checks = state_checks(&path, &[], &[profile("work")]);

        assert!(!checks.iter().any(|c| c.name == "shell override"), "{:?}", names(&checks));
    }

    /// A hand-edited `state.toml` can hold anything.  The sidebar drops a value
    /// it cannot parse the moment it loads, so the override is gone before any
    /// distro or profile is ever consulted.
    #[test]
    fn a_shell_override_that_is_not_even_a_shell_override_warns() {
        assert!(ignored_override("nonsense", &[], &[]).is_some());
    }

    /// Pinning to Windows resolves to the config shell, which every machine has.
    #[test]
    fn pinning_to_windows_is_always_honoured() {
        assert_eq!(ignored_override("windows", &[], &[]), None);
    }

    /// A project with no override at all has nothing to report — most projects
    /// are this, and a row each would bury the real warnings.
    #[test]
    fn a_project_with_no_override_is_not_reported() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let path = state_with(&dir, std::slice::from_ref(&repo));

        let checks = state_checks(&path, &[], &[]);

        assert!(!checks.iter().any(|c| c.name == "shell override"), "{:?}", names(&checks));
    }

    /// `load_from` hands back an empty state on a parse error, so a corrupt file
    /// presents as a first run — with the project list quietly gone.  A user
    /// staring at an empty sidebar needs to be told the file is broken, not
    /// shown a cheerful "0 projects".
    #[test]
    fn a_corrupt_state_file_fails_rather_than_reporting_no_projects() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.toml");
        std::fs::write(&path, "this is not toml {{").unwrap();

        assert_eq!(status_of(&state_checks(&path, &[], &[]), "state.toml"), Status::Fail);
    }

    /// No state file is a first run, which is fine.
    #[test]
    fn an_absent_state_file_is_a_first_run() {
        let dir = TempDir::new().unwrap();

        let checks = state_checks(&dir.path().join("state.toml"), &[], &[]);

        assert_eq!(status_of(&checks, "state.toml"), Status::Ok);
    }

    #[test]
    fn a_healthy_state_file_counts_its_projects() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let path = state_with(&dir, &[repo]);

        let checks = state_checks(&path, &[], &[]);

        assert_eq!(status_of(&checks, "state.toml"), Status::Ok);
        assert!(
            checks[0].detail.starts_with("1 project "),
            "{:?} does not count the project",
            checks[0].detail
        );
    }

    /// A project whose directory was deleted or moved stays in the sidebar and
    /// renders as an empty, inert row.  Naming the path is the whole fix.
    #[test]
    fn a_project_root_that_no_longer_exists_warns() {
        let dir = TempDir::new().unwrap();
        let gone = dir.path().join("gone");
        let path = state_with(&dir, std::slice::from_ref(&gone));

        let checks = state_checks(&path, &[], &[]);

        let missing = checks
            .iter()
            .find(|c| c.status == Status::Warn)
            .expect("a warning about the missing root");
        assert!(
            missing.detail.contains(&gone.display().to_string()),
            "{:?} does not name the missing root",
            missing.detail
        );
    }

    /// The exit code is what a script reads.  Warnings are the normal state of a
    /// working machine (no `doppler`, no `gh`), so only a real failure is
    /// allowed to make `doctor` non-zero.
    #[test]
    fn the_exit_code_is_nonzero_only_when_a_check_fails() {
        let ok = vec![check("a", "x", Status::Ok, ""), check("a", "y", Status::Warn, "")];
        assert_eq!(exit_code(&ok), 0);

        let bad = vec![check("a", "x", Status::Ok, ""), check("a", "z", Status::Fail, "")];
        assert_eq!(exit_code(&bad), 1);
    }

    /// `--json` is the agent-facing shape, so the top-level verdict has to agree
    /// with the exit code the shell sees.
    #[test]
    fn the_json_verdict_agrees_with_the_exit_code() {
        let bad = vec![check("a", "z", Status::Fail, "broken")];

        assert_eq!(to_json(&bad)["ok"], false);
        assert_eq!(to_json(&[check("a", "x", Status::Warn, "")])["ok"], true);
    }
}
