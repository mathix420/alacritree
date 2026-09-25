//! `alacritree doctor` looks at everything alacritree needs but never
//! complains about.
//!
//! Most of what alacritree depends on is deliberately best-effort: a missing
//! `gh` falls back to the repo's default branch, a missing `doppler` skips
//! scope mirroring, a malformed `alacritty.toml` loads defaults, and a corrupt
//! `state.toml` opens an empty sidebar. Every one of those is the right call in
//! the app, since none of them should stop a terminal from opening. Together,
//! though, they mean a broken setup looks exactly like a working one. This is the
//! one place that says so out loud.
//!
//! It answers without a running instance, because "nothing happens when I run
//! it" is precisely when it gets used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use strum::VariantArray;

use crate::config::{self, Config, ConfigDiagnosis, ConfigFile, Profile, ShellConfig};
use crate::crash_log::{Verdict, classify};
use crate::diff_viewer::{Program, Viewer};
use crate::ipc::protocol::{self, IpcRequest, SendError};
use crate::multiplexer::Side;
use crate::shell_decision::{ShellDecision, shell_decision};
use crate::tasks::taskwarrior::{TaskError, Taskwarrior};
use crate::tools::locate;
use crate::wsl::{self, ShellChoice};
use crate::{command_ext, jobs, state, tools};

/// An instance that is wedged should not wedge the report too.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
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
pub(super) struct Check {
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

pub(super) fn run(
    as_json: bool,
    socket: Option<&Path>,
    config_dir: Option<&Path>,
    overrides: &[toml::Value],
) -> i32 {
    let checks = report(socket, config_dir, overrides);
    if as_json {
        println!("{:#}", to_json(&checks));
    } else {
        print_human(&checks);
    }
    exit_code(&checks)
}

fn report(
    socket: Option<&Path>,
    config_dir: Option<&Path>,
    overrides: &[toml::Value],
) -> Vec<Check> {
    let (config, _) = config::load(config_dir, overrides);
    tools::configure(config.integrations.tool_paths());

    // Rows are grouped by section on the way out, so each section has to be
    // added in one run. A section split in two prints its header twice.
    let mut checks = binary_checks();
    checks.extend(gh_auth_check());
    checks.extend(diff_viewer_check(&config.integrations.diff_viewer.viewer));
    checks.push(shell_check(config.shell.as_ref()));
    checks.extend(wsl_checks(&wsl::distros()));
    if config.integrations.taskwarrior.enabled {
        checks.extend(taskwarrior_checks(&wsl::distros()));
    }
    checks.extend(config_checks(&config::diagnose(config_dir, overrides)));
    checks.extend(persisted_state_checks(&config));
    checks.extend(ipc_checks(socket, config.ipc_socket));
    checks.extend(crash_checks());
    #[cfg(windows)]
    checks.extend(process_checks(&running_alacritree_processes()));
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
            need: doppler_need(alacritree_doppler::is_set_up()),
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
    tools().iter().map(|tool| tool_check(tool, find(&configured_program(tool.program)))).collect()
}

/// The program the configured diff viewer runs. A custom pager is a shell
/// command line git hands to a shell, not a program to look up.
fn diff_viewer_check(viewer: &Viewer) -> Option<Check> {
    let program = match viewer {
        Viewer::Pager { pager: Program::Custom { .. }, .. } => return None,
        _ => match viewer.program() {
            Program::Tool(tool) => tools::program(*tool),
            Program::Custom { path, .. } => path.clone(),
        },
    };
    let tool = Tool {
        program: "diff viewer",
        consequence: "the git panel's diff pane opens an error instead of a diff",
        need: Need::Optional,
    };
    Some(tool_check(&tool, find(&program)))
}

/// A registry tool's configured path, which `locate` resolves as a path when
/// it is one; other programs are looked up by their own name.
fn configured_program(program: &str) -> String {
    tools::Tool::VARIANTS
        .iter()
        .copied()
        .find(|tool| tool.name() == program)
        .map_or_else(|| program.to_string(), tools::program)
}

fn tool_check(tool: &Tool, found: Option<Found>) -> Check {
    match found {
        Some(Found { path, version }) => {
            let version = version.unwrap_or_else(|| "unknown version".to_string());
            check("binaries", tool.program, Status::Ok, format!("{version}  {}", path.display()))
        },
        None => {
            let (status, detail) = match tool.need {
                Need::Required => (Status::Fail, format!("not on PATH, so {}", tool.consequence)),
                Need::Optional => (Status::Warn, format!("not on PATH, so {}", tool.consequence)),
                Need::Unused => (Status::Ok, "not installed, and unused here".to_string()),
            };
            check("binaries", tool.program, status, detail)
        },
    }
}

/// Doppler scope mirroring only matters to someone who has set Doppler up.
fn doppler_need(configured: bool) -> Need {
    if configured { Need::Optional } else { Need::Unused }
}

/// `gh` present but logged out fails exactly the way a missing `gh` does,
/// silently, so it needs saying separately.
// The report's job is to run the tools it reports on, from a CLI with no
// window to stall.
#[allow(clippy::disallowed_methods)]
fn gh_auth_check() -> Option<Check> {
    let gh = tools::program(tools::Tool::Gh);
    locate(&gh)?;
    let authenticated = command_ext::hidden(&gh)
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());

    Some(if authenticated {
        check("binaries", "gh auth", Status::Ok, "authenticated")
    } else {
        let detail = "not authenticated, so PR base branches fall back to the repo default";
        check("binaries", "gh auth", Status::Warn, detail)
    })
}

/// One wsl.exe round trip per distro, in parallel and on a deadline: a distro
/// whose VM is cold takes seconds to answer and one that is wedged never
/// does, and a report that hangs is worse than one that says it could not
/// tell.
const WSL_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a probe of one distro found. It has a path per [`tools::Tool`], by discriminant, or
/// why the distro could not be asked.
type Probe = Result<Vec<Option<String>>, ProbeError>;

#[derive(Debug, thiserror::Error)]
enum ProbeError {
    #[error(transparent)]
    Batch(#[from] wsl::BatchError),
    #[error("no answer in {}s", WSL_PROBE_TIMEOUT.as_secs())]
    NoAnswer,
}

/// What each installed distro can actually do for alacritree.  Nothing else
/// reports on the inside of a distro: git, gh and delta are resolved there
/// silently, and their absence looks exactly like a repository with nothing
/// to say.  Empty when WSL is not installed, which is also every non-Windows
/// machine.
fn wsl_checks(distros: &[wsl::WslDistro]) -> Vec<Check> {
    if distros.is_empty() {
        return Vec::new();
    }
    let names: Vec<String> = distros
        .iter()
        .map(|d| if d.is_default { format!("{} (default)", d.name) } else { d.name.clone() })
        .collect();

    let probes = probe_distros(distros);
    let mut checks = vec![check("wsl", "distros", Status::Ok, names.join(", "))];
    checks.extend(probes.iter().map(|(name, probe)| wsl_distro_check(name, probe)));
    checks
}

/// Probes every registry tool, since each is one alacritree runs for a project
/// inside the distro.
fn probe_distros(distros: &[wsl::WslDistro]) -> Vec<(String, Probe)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let names = tools::Tool::table(tools::Tool::name);
    for distro in distros {
        let tx = tx.clone();
        let name = distro.name.clone();
        let names = names;
        std::thread::spawn(move || {
            let probe = jobs::on_this_thread(|blocking| wsl::probe_tools(&name, &names, blocking))
                .map_err(ProbeError::from);
            let _ = tx.send((name, probe));
        });
    }
    drop(tx);

    let deadline = Instant::now() + WSL_PROBE_TIMEOUT;
    let mut answered: HashMap<String, Probe> = HashMap::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok((name, probe)) => {
                answered.insert(name, probe);
            },
            Err(_) => break,
        }
    }

    distros
        .iter()
        .map(|d| {
            let probe = answered.remove(&d.name).unwrap_or(Err(ProbeError::NoAnswer));
            (d.name.clone(), probe)
        })
        .collect()
}

fn wsl_distro_check(name: &str, probe: &Probe) -> Check {
    let found = match probe {
        Ok(found) => found,
        // Not alacritree's fault, but every project inside the distro is
        // unreadable until it starts.
        Err(e) => return check("wsl", name, Status::Warn, format!("unreachable: {e}")),
    };

    let mut present = Vec::new();
    let mut missing = Vec::new();
    for &tool in tools::Tool::VARIANTS {
        match tool_path(found, tool) {
            Some(path) => present.push(format!("{} {path}", tool.name())),
            None => missing.push(tool.name()),
        }
    }
    let mut detail =
        if present.is_empty() { "nothing found".to_string() } else { present.join(", ") };
    if !missing.is_empty() {
        detail.push_str(&format!("; no {}", missing.join(", ")));
    }

    // git is what reads a project that lives in the distro; without it the
    // sidebar lists the worktree and can say nothing else about it.
    let status =
        if tool_path(found, tools::Tool::Git).is_some() { Status::Ok } else { Status::Warn };
    check("wsl", name, status, detail)
}

/// Where a probe put `tool`, by tool rather than by index, so
/// [`tools::Tool`]'s variants can be reordered without silently renaming
/// everyone's results.
fn tool_path(found: &[Option<String>], tool: tools::Tool) -> Option<&str> {
    found.get(tool as usize)?.as_deref()
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
            let detail = format!("{} is not on PATH, so sessions cannot start", shell.program);
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
        (None, _) => check("config", name, Status::Ok, "not found, using built-in defaults"),
    }
}

fn persisted_state_checks(config: &Config) -> Vec<Check> {
    let Some(path) = state::config_path() else {
        let detail = "no config directory, so the sidebar cannot persist";
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
        return Some(format!("`{raw}` is not a shell override, so the automatic shell is used"));
    };

    match (&choice, shell_decision(Some(&choice), None, distros, profiles, None)) {
        // Pinning to Windows *is* a decision to use the config shell.
        (ShellChoice::Windows, _) => None,
        (ShellChoice::Wsl(distro), ShellDecision::ConfigShell) => {
            Some(format!("WSL distro `{distro}` is not installed, so the automatic shell is used"))
        },
        (ShellChoice::Profile(name), ShellDecision::ConfigShell) => Some(format!(
            "no `[[ui.profiles]]` entry named `{name}`, so the automatic shell is used"
        )),
        _ => None,
    }
}

fn ipc_checks(socket: Option<&Path>, enabled: bool) -> Vec<Check> {
    let mut checks = Vec::new();

    if !enabled {
        let detail = "disabled in config, so the CLI and MCP cannot reach a running window";
        checks.push(check("ipc", "ipc_socket", Status::Warn, detail));
    }
    checks.push(check(
        "ipc",
        "socket dir",
        Status::Ok,
        protocol::socket_dir().display().to_string(),
    ));

    checks.push(match protocol::send_request(socket, &IpcRequest::ListProjects, PROBE_TIMEOUT) {
        Ok(_) => check("ipc", "instance", Status::Ok, "answering"),
        // Nothing running is not a fault: the CLI serves projects, git status
        // and worktrees from disk when no window is up.
        Err(SendError::NoInstance) => {
            check("ipc", "instance", Status::Ok, "none running, but offline commands still work")
        },
        Err(e) => check("ipc", "instance", Status::Warn, format!("running but not answering: {e}")),
    });
    checks
}

fn crash_checks() -> Vec<Check> {
    match crate::logdir::log_dir() {
        Some(dir) => crash_checks_in(&dir),
        None => vec![check(
            "crashes",
            "log directory",
            Status::Fail,
            "no log directory on this platform".to_string(),
        )],
    }
}

fn crash_checks_in(dir: &Path) -> Vec<Check> {
    if !dir.exists() {
        return vec![check("crashes", "artifacts", Status::Ok, "none recorded".to_string())];
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        let detail = format!("cannot read {}", dir.display());
        return vec![check("crashes", "log directory", Status::Fail, detail)];
    };

    let mut crashed = 0usize;
    let mut indeterminate = 0usize;
    let mut running = 0usize;
    let mut clean = 0usize;
    let mut newest: Option<(u128, String)> = None;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = crate::logdir::parse_name("crash-", name) else { continue };

        let verdict = classify(&entry.path(), id.pid);
        match verdict {
            Verdict::Crashed => crashed += 1,
            Verdict::Indeterminate => indeterminate += 1,
            Verdict::Running => running += 1,
            Verdict::Clean => clean += 1,
        }
        if newest.as_ref().is_none_or(|(start, _)| id.start > *start) {
            newest = Some((id.start, name.to_string()));
        }
    }

    let total = crashed + indeterminate + running + clean;
    if total == 0 {
        return vec![check("crashes", "artifacts", Status::Ok, "none recorded".to_string())];
    }

    let status = if crashed > 0 || indeterminate > 0 { Status::Warn } else { Status::Ok };
    let newest = newest.map(|(_, n)| n).unwrap_or_default();
    let detail = format!(
        "{total} artifacts: {crashed} crashed, {indeterminate} indeterminate, {running} running, \
         {clean} clean; newest {newest}"
    );
    vec![check("crashes", "artifacts", status, detail)]
}

/// A live process running one of our binaries, and therefore pinning it.
#[cfg(windows)]
struct AlacritreeProcess {
    pid: u32,
    exe: Option<PathBuf>,
    bridge: bool,
}

/// Running processes pin their exe images. Builds and installs rename a
/// pinned exe aside rather than fail, so a pin is not a fault and the rows
/// are informational. When a rename does fail (antivirus, a read-only
/// volume), this section turns "Access is denied (os error 5)" into a pid
/// worth closing.
#[cfg(windows)]
fn process_checks(processes: &[AlacritreeProcess]) -> Vec<Check> {
    if processes.is_empty() {
        let detail = "no running alacritree pins a binary";
        return vec![check("processes", "images", Status::Ok, detail)];
    }
    processes
        .iter()
        .map(|p| {
            let role = if p.bridge { "mcp bridge" } else { "window" };
            let image = match &p.exe {
                Some(exe) => exe.display().to_string(),
                None => "an unreadable image path".to_string(),
            };
            let detail = format!("pid {} holds {image}", p.pid);
            check("processes", role, Status::Ok, detail)
        })
        .collect()
}

#[cfg(windows)]
fn running_alacritree_processes() -> Vec<AlacritreeProcess> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always).with_cmd(UpdateKind::Always),
    );
    let me = std::process::id();
    let mut processes: Vec<AlacritreeProcess> = sys
        .processes()
        .iter()
        // This doctor run is itself an alacritree process, but it exits with
        // the report and pins nothing worth mentioning.
        .filter(|(pid, _)| pid.as_u32() != me)
        .filter(|(_, p)| p.name().to_string_lossy().to_lowercase().starts_with("alacritree"))
        .map(|(pid, p)| AlacritreeProcess {
            pid: pid.as_u32(),
            exe: p.exe().map(Path::to_path_buf),
            bridge: p.cmd().iter().any(|arg| arg == "mcp"),
        })
        .collect();
    processes.sort_by_key(|p| p.pid);
    processes
}

fn find(program: &str) -> Option<Found> {
    let path = locate(program)?;
    let version = version_of(&path);
    Some(Found { path, version })
}

/// A tool that is on PATH but broken (a shim, a half-installed package) fails
/// here rather than reporting a version, which is worth knowing on its own.
#[allow(clippy::disallowed_methods)] // Running the tool is how its version is read.
fn version_of(program: &Path) -> Option<String> {
    let output = command_ext::hidden(program)
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

/// Agents call `task` directly, and a taskrc without the UDAs folds `subof:`
/// and `order:` into the description instead of storing them.
fn taskwarrior_checks(distros: &[wsl::WslDistro]) -> Vec<Check> {
    let sides =
        std::iter::once(Side::Native).chain(distros.iter().map(|d| Side::Wsl(d.name.clone())));
    sides
        .map(|side| {
            let name = side.name();
            let declared = jobs::on_this_thread(|b| {
                let tw = Taskwarrior::for_side(side, b);
                Ok::<_, TaskError>((
                    tw.rc_value("uda.subof.type", b)?,
                    tw.rc_value("uda.order.type", b)?,
                ))
            });
            let declared = match &declared {
                Ok((subof, order)) => Ok((subof.as_deref(), order.as_deref())),
                Err(e) => Err(e.to_string()),
            };
            uda_check(&name, declared)
        })
        .collect()
}

fn uda_check(side: &str, declared: Result<(Option<&str>, Option<&str>), String>) -> Check {
    match declared {
        Ok((Some("uuid"), Some("numeric"))) => {
            check("taskwarrior", side, Status::Ok, "subof and order declared")
        },
        Ok(_) => check(
            "taskwarrior",
            side,
            Status::Warn,
            "subof and order are not declared; run `alacritree task setup`",
        ),
        Err(e) => check("taskwarrior", side, Status::Warn, e),
    }
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
    use strum::EnumCount;
    use tempfile::TempDir;

    use super::*;
    use crate::diff_viewer::{Program, Templates, Viewer};
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

    /// A custom pager is a shell command line, not a program to look up.
    #[test]
    fn the_diff_viewer_check_looks_up_programs_only() {
        let pager = Viewer::Pager {
            pager: Program::Custom { path: "delta -s".to_string(), wsl_path: None },
            args: Vec::new(),
        };
        assert!(diff_viewer_check(&pager).is_none());

        let missing = Viewer::Direct {
            program: Program::Custom {
                path: "/definitely/not/here/tuicr".to_string(),
                wsl_path: None,
            },
            templates: Templates::default(),
        };
        let check = diff_viewer_check(&missing).expect("a direct viewer is checked");
        assert_eq!(check.name, "diff viewer");
        assert_eq!(check.status, Status::Warn);
    }

    /// Doppler drives one optional feature, and most people have never wanted
    /// it. Warning that it is absent would put a permanent warning on every
    /// machine that simply does not use Doppler, and a report that always has
    /// a warning in it is a report nobody reads.
    #[test]
    fn a_tool_this_machine_does_not_use_is_not_worth_warning_about() {
        assert_eq!(tool_check(&DOPPLER, None).status, Status::Ok);
    }

    /// Someone who has set Doppler up and then lost the binary does want to
    /// hear about it.
    #[test]
    fn doppler_is_only_worth_warning_about_once_it_has_been_set_up() {
        assert_eq!(doppler_need(true), Need::Optional);
        assert_eq!(doppler_need(false), Need::Unused);
    }

    fn probe(paths: &[Option<&str>]) -> Probe {
        Ok(paths.iter().map(|p| p.map(str::to_string)).collect())
    }

    /// Nothing to say on a machine without WSL, which includes every
    /// non-Windows one. A section of rows about an absent subsystem is noise
    /// in the report that has to stay readable.
    #[test]
    fn no_distros_means_no_wsl_section() {
        assert!(wsl_checks(&[]).is_empty());
    }

    /// The whole point of the section: git, gh and delta are resolved inside
    /// the distro and nothing else ever names the paths they resolved to.
    #[test]
    fn a_distro_reports_where_each_tool_resolved() {
        let found = probe(&[
            Some("/usr/bin/git"),
            Some("/home/lev/.local/bin/gh"),
            None,
            None,
            None,
            Some("/home/lev/.cargo/bin/tuicr"),
        ]);
        let detail = wsl_distro_check("Ubuntu", &found).detail;
        assert!(detail.contains("git /usr/bin/git"), "{detail:?}");
        assert!(detail.contains("gh /home/lev/.local/bin/gh"), "{detail:?}");
        assert!(detail.contains("tuicr /home/lev/.cargo/bin/tuicr"), "{detail:?}");
        assert!(detail.contains("no delta, doppler, herdr"), "{detail:?}");
    }

    /// A distro without git reads as a repository with nothing to report:
    /// the sidebar lists the worktree, the git panel stays empty, and no
    /// error is ever shown.
    #[test]
    fn a_distro_without_git_warns() {
        assert_eq!(
            wsl_distro_check("Ubuntu", &probe(&[None; tools::Tool::COUNT])).status,
            Status::Warn
        );
        let git_only = probe(&[Some("/usr/bin/git"), None, None, None, None, None]);
        assert_eq!(wsl_distro_check("Ubuntu", &git_only).status, Status::Ok);
    }

    #[test]
    fn uda_check_warns_until_both_are_declared() {
        assert_eq!(uda_check("native", Ok((Some("uuid"), Some("numeric")))).status, Status::Ok);
        assert_eq!(uda_check("native", Ok((Some("uuid"), None))).status, Status::Warn);
        assert_eq!(uda_check("wsl:Ubuntu", Ok((None, None))).status, Status::Warn);
        let missing = uda_check("native", Err("taskwarrior not found: task".into()));
        assert_eq!(missing.status, Status::Warn);
        assert!(missing.detail.contains("not found"), "{:?}", missing.detail);
    }

    #[test]
    fn a_distro_that_cannot_be_reached_says_why() {
        let probe: Probe = Err(ProbeError::NoAnswer);
        let check = wsl_distro_check("Ubuntu", &probe);
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("no answer in 15s"), "{:?}", check.detail);
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
    /// present but misbehaving, such as an old git or a shim ahead of the real
    /// one.
    #[test]
    fn a_found_tool_reports_its_version_and_path() {
        let found =
            Found { path: PathBuf::from("/usr/bin/git"), version: Some("2.51.0".to_string()) };

        let check = tool_check(&GIT, Some(found));

        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("2.51.0"), "{:?} lacks the version", check.detail);
        assert!(check.detail.contains("/usr/bin/git"), "{:?} lacks the path", check.detail);
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
    /// in both files. That deserves its own check, since the per-file ones are
    /// green.
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
            .map(|r| PersistedProject { root: r.clone(), expanded: true, shell: None, label: None })
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
            label: None,
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

    /// A project with no override at all has nothing to report. Most projects
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
    /// presents as a first run, with the project list quietly gone. A user
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

    #[cfg(windows)]
    fn alacritree_process(pid: u32, bridge: bool) -> AlacritreeProcess {
        AlacritreeProcess {
            pid,
            exe: Some(PathBuf::from(r"C:\target\release\alacritree.exe")),
            bridge,
        }
    }

    /// A bridge lives as long as its MCP client, not as long as the window.
    /// Telling them apart tells the user which program to close.
    #[cfg(windows)]
    #[test]
    fn a_bridge_and_a_window_are_told_apart() {
        let checks = process_checks(&[alacritree_process(7, true), alacritree_process(8, false)]);

        assert_eq!(names(&checks), vec!["mcp bridge", "window"]);
    }

    /// The row has to name the pid and the file, or the reader knows neither
    /// what to close nor which file is pinned. It stays `ok`, not `warn`:
    /// builds and installs rename a pinned exe aside rather than fail, so a
    /// running process is the normal state of a working machine, and a
    /// report that always has a warning in it stops being read.
    #[cfg(windows)]
    #[test]
    fn a_pinning_process_is_an_ok_row_naming_its_pid_and_image() {
        let checks = process_checks(&[alacritree_process(4242, true)]);

        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].detail.contains("4242"), "{:?} lacks the pid", checks[0].detail);
        assert!(
            checks[0].detail.contains(r"release\alacritree.exe"),
            "{:?} lacks the image",
            checks[0].detail
        );
    }

    /// Most doctor runs happen with nothing running; that is a clean bill of
    /// health worth stating, not a section to omit.
    #[cfg(windows)]
    #[test]
    fn no_processes_is_a_clean_ok_row() {
        let checks = process_checks(&[]);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
    }

    /// A crash last week must not make `doctor` exit nonzero in someone's script.
    /// `Fail` is reserved for crash logging being broken right now.
    #[test]
    fn a_past_crash_warns_but_does_not_fail() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("crash-1-0.log"), "t1 start v\nt2 PANIC thread=main\n")
            .unwrap();

        let checks = crash_checks_in(dir.path());

        assert!(checks.iter().any(|c| c.status == Status::Warn), "a recorded crash did not warn");
        assert_eq!(exit_code(&checks), 0, "a past crash made doctor exit nonzero");
    }

    #[test]
    fn no_artifacts_is_ok() {
        let dir = tempfile::tempdir().expect("a temp dir");

        let checks = crash_checks_in(dir.path());

        assert!(checks.iter().all(|c| c.status == Status::Ok), "an empty directory was not ok");
    }

    /// A clean shutdown is the common case and must not accumulate warnings.
    #[test]
    fn a_clean_artifact_is_ok() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("crash-1-0.log"), "t1 start v pid=0\nt2 exit ok\n").unwrap();

        let checks = crash_checks_in(dir.path());

        assert!(checks.iter().all(|c| c.status == Status::Ok), "a clean artifact warned");
    }

    /// A record written after the exit marker means a detached worker outlived the
    /// shutdown. That is a real defect, even though the process exited cleanly.
    #[test]
    fn a_record_after_the_exit_marker_warns() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let body = "t1 start v pid=0\nt2 exit ok\nt3 PANIC thread=pty-1\n";
        std::fs::write(dir.path().join("crash-1-0.log"), body).unwrap();

        let checks = crash_checks_in(dir.path());

        assert!(checks.iter().any(|c| c.status == Status::Warn), "a late worker panic was missed");
    }

    /// A truncated artifact is neither clean nor a live process; saying either
    /// would be a lie about the only evidence there is.
    #[test]
    fn a_headerless_artifact_is_indeterminate() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("crash-1-0.log"), "PANIC without a header\n").unwrap();

        let checks = crash_checks_in(dir.path());

        let text: String = checks.iter().map(|c| c.detail.clone()).collect();
        // The column label "indeterminate" appears in every non-empty directory's
        // detail, so asserting on it alone would not catch a misclassification
        // into another bucket. Pin the counts instead.
        assert!(text.contains("1 indeterminate"), "not counted as indeterminate: {text}");
        assert!(text.contains("0 crashed"), "wrongly counted as crashed: {text}");
        assert!(text.contains("0 clean"), "wrongly counted as clean: {text}");
    }
}
