//! Task lists kept in taskwarrior, and the only code that runs `task`. Every
//! call is addressed by uuid, declares the UDAs itself so a taskrc without
//! them never folds `subof:` into a description, and turns off prompts, since
//! there is no terminal to answer them.

mod settings;

use std::io;
use std::process::{Output, Stdio};
use std::time::Duration;

use alacritree_common::command_ext::hidden;
use alacritree_common::jobs::Blocking;
use alacritree_common::side::Side;
use alacritree_common::tools::{self, Tool};
use alacritree_common::wsl;
use alacritree_tasks::{Edit, Filter, NodeMatch, Status, Task, TaskBackend, TaskError};
use serde::Deserialize;

pub use self::settings::{RawTaskwarrior, TaskwarriorConfig};

/// A task's parent is the `subof` UDA and its place among siblings the
/// `order` one. Also what `alacritree task setup` writes into each side's
/// taskrc.
const UDA_DECLARATIONS: [(&str, &str); 4] = [
    ("uda.subof.type", "uuid"),
    ("uda.subof.label", "Sub of"),
    ("uda.order.type", "numeric"),
    ("uda.order.label", "Order"),
];

/// taskchampion takes an immediate write lock with no busy timeout, so a
/// second writer fails at once instead of waiting its turn.
const BUSY_RETRIES: [Duration; 3] =
    [Duration::from_millis(50), Duration::from_millis(150), Duration::from_millis(400)];

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExportedStatus {
    Pending,
    Completed,
    #[serde(other)]
    Other,
}

/// A task as `task export` prints it.
#[derive(Deserialize)]
struct Exported {
    uuid: String,
    description: String,
    status: ExportedStatus,
    start: Option<String>,
    subof: Option<String>,
    #[serde(default, deserialize_with = "integer_order")]
    order: Option<i64>,
    project: Option<String>,
    entry: Option<String>,
    modified: Option<String>,
}

/// Numeric UDAs export as JSON numbers that may carry a fraction.
fn integer_order<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(|n| n.round() as i64))
}

impl Exported {
    /// `None` for a deleted, waiting or recurring task, which no listing
    /// shows.
    fn into_task(self) -> Option<Task> {
        let status = match self.status {
            ExportedStatus::Pending => Status::Pending,
            ExportedStatus::Completed => Status::Completed,
            ExportedStatus::Other => return None,
        };
        Some(Task {
            id: self.uuid,
            description: self.description,
            status,
            started: self.start.is_some(),
            parent: self.subof,
            order: self.order,
            project: self.project,
            entry: self.entry,
            modified: self.modified,
        })
    }
}

/// The taskwarrior backend. `task` is looked up on each call, on the side the
/// call names.
#[derive(Clone, Debug, Default)]
pub struct Taskwarrior {
    env: Vec<(String, String)>,
}

impl Taskwarrior {
    /// `task` on exactly `side`.
    fn on(&self, side: Side, blocking: &Blocking) -> Cli<'_> {
        let program = match &side {
            Side::Native => tools::program(Tool::Task),
            Side::Wsl(distro) => tools::wsl_in_job(Tool::Task, distro, blocking),
        };
        Cli { side, program, env: &self.env }
    }

    /// The `task` holding the tasks of a project on `side`. Taskwarrior 3 has
    /// no Windows build, so a Windows project uses the default distro's when
    /// Windows has no `task` to spawn. A shim that forwards to WSL does not
    /// count, since only a shell can run it.
    fn for_project(&self, side: &Side, blocking: &Blocking) -> Cli<'_> {
        let side = match side {
            Side::Native if cfg!(windows) => native_or_default_distro(
                tools::locate_spawnable(&tools::program(Tool::Task)).is_some(),
                wsl::distros().into_iter().find(|d| d.is_default).map(|d| d.name),
            ),
            side => side.clone(),
        };
        self.on(side, blocking)
    }

    /// Writes each UDA declaration `side`'s taskrc lacks, and returns the
    /// keys it wrote.
    pub fn declare_fields(
        &self,
        side: Side,
        blocking: &Blocking,
    ) -> Result<Vec<&'static str>, TaskError> {
        let cli = self.on(side, blocking);
        let mut written = Vec::new();
        for (key, value) in UDA_DECLARATIONS {
            if cli.rc_value(key)?.as_deref() != Some(value) {
                cli.run(&["config".into(), key.to_string(), value.to_string()])?;
                written.push(key);
            }
        }
        Ok(written)
    }

    /// Whether `side`'s taskrc gives both UDAs their types. Agents call `task`
    /// directly, and without them `subof:` and `order:` land in the
    /// description.
    pub fn fields_declared(&self, side: Side, blocking: &Blocking) -> Result<bool, TaskError> {
        let cli = self.on(side, blocking);
        for (key, value) in UDA_DECLARATIONS.iter().filter(|(key, _)| key.ends_with(".type")) {
            if cli.rc_value(key)?.as_deref() != Some(*value) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl TaskBackend for Taskwarrior {
    fn list(
        &self,
        side: &Side,
        filter: &Filter,
        blocking: &Blocking,
    ) -> Result<Vec<Task>, TaskError> {
        let Some(mut args) = filter_args(filter) else { return Ok(Vec::new()) };
        args.push("export".into());
        let cli = self.for_project(side, blocking);
        let output = cli.run(&args)?;
        let exported: Vec<Exported> = serde_json::from_slice(&output.stdout)
            .map_err(|source| TaskError::Malformed { program: cli.program.clone(), source })?;
        Ok(exported.into_iter().filter_map(Exported::into_task).collect())
    }

    fn apply(&self, side: &Side, edit: &Edit, blocking: &Blocking) -> Result<(), TaskError> {
        let cli = self.for_project(side, blocking);
        match edit {
            Edit::Add { project, description, parent, order } => {
                cli.add(project, description, parent.as_deref(), *order).map(drop)
            },
            edit => cli.run(&edit_args(edit)).map(drop),
        }
    }

    fn agent_guide(&self, project: &str) -> String {
        format!(
            "Task list, kept in taskwarrior. Write your own tasks to project `{project}`.\n- add: \
             `task add project:{project} order:<n> subof:<parent uuid> -- <text>` (subof is \
             optional)\n- finish: `task <uuid> done`; begin: `task <uuid> start`\n`alacritree \
             task scope` prints the project. If `subof:` or `order:` end up inside a description, \
             run `alacritree task setup` once.\n"
        )
    }
}

/// The filter words for `filter`, or `None` when it covers no node. A bare
/// `project:r` is a left match that would also return a repository named
/// `r-web`, so a subtree matches `r` itself and whatever sits below `r.`.
fn filter_args(filter: &Filter) -> Option<Vec<String>> {
    let nodes: Vec<String> = filter
        .nodes
        .iter()
        .map(|m| match m {
            NodeMatch::Exact(node) => format!("project.is:{node}"),
            NodeMatch::Subtree(node) => format!("project.is:{node} or project:{node}."),
        })
        .collect();
    if nodes.is_empty() {
        return None;
    }
    Some(vec![format!("({})", nodes.join(" or ")), "(status:pending or status:completed)".into()])
}

/// The arguments for any edit but `Add`, which has to read back the uuid it
/// made. `--` stops taskwarrior reading `+tag` or `due:` out of a
/// description.
fn edit_args(edit: &Edit) -> Vec<String> {
    let modify = |id: &str, mods: &[String]| {
        [id.to_string(), "modify".into()].into_iter().chain(mods.iter().cloned()).collect()
    };
    let verb = |id: &str, verb: &str| vec![id.to_string(), verb.to_string()];
    match edit {
        Edit::Add { .. } => unreachable!("an add reads back its uuid, so it has its own call"),
        Edit::Move { id, parent, order } => modify(id, &[
            format!("subof:{}", parent.as_deref().unwrap_or_default()),
            format!("order:{order}"),
        ]),
        Edit::Reorder { id, order } => modify(id, &[format!("order:{order}")]),
        Edit::Describe { id, description } => modify(id, &["--".into(), description.clone()]),
        Edit::Done(id) => verb(id, "done"),
        Edit::Undone(id) => modify(id, &["status:pending".into()]),
        Edit::Start(id) => verb(id, "start"),
        Edit::Stop(id) => verb(id, "stop"),
        Edit::Delete(id) => verb(id, "delete"),
    }
}

/// `task` as found on one side.
struct Cli<'a> {
    side: Side,
    program: String,
    env: &'a [(String, String)],
}

impl Cli<'_> {
    /// One attempt, no overrides: what `task` itself would do with `args`.
    fn spawn(&self, args: &[String]) -> Result<Output, TaskError> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (program, argv) = self.side.command(&self.program, &refs);
        // `output` drains both pipes while the child runs, which an export
        // larger than a pipe buffer needs. A closed stdin answers any prompt
        // taskwarrior still raises.
        #[allow(clippy::disallowed_methods)] // Running `task` is this function's job.
        let output = hidden(program)
            .args(argv)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .output();
        let missing = || TaskError::Missing { program: self.program.clone() };
        let output = match output {
            Ok(output) => output,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(missing()),
            Err(source) => return Err(TaskError::Spawn { program: self.program.clone(), source }),
        };
        // A WSL login shell reports a missing program as 127, not ENOENT.
        if output.status.code() == Some(127) {
            return Err(missing());
        }
        Ok(output)
    }

    // Runs on a pool worker, where waiting out a busy lock is the point.
    fn run(&self, args: &[String]) -> Result<Output, TaskError> {
        let mut argv: Vec<String> =
            UDA_DECLARATIONS.iter().map(|(k, v)| format!("rc.{k}={v}")).collect();
        argv.push("rc.confirmation=off".into());
        argv.extend(args.iter().cloned());
        let mut retries = BUSY_RETRIES.iter();
        loop {
            let output = self.spawn(&argv)?;
            if output.status.success() {
                return Ok(output);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            match retries.next() {
                Some(delay) if is_busy(&stderr) => std::thread::sleep(*delay),
                _ => return Err(TaskError::Failed { program: self.program.clone(), stderr }),
            }
        }
    }

    /// The new task's uuid.
    fn add(
        &self,
        project: &str,
        description: &str,
        parent: Option<&str>,
        order: i64,
    ) -> Result<String, TaskError> {
        let mut args = vec![
            "rc.verbose=new-uuid".to_string(),
            "add".into(),
            format!("project:{project}"),
            format!("order:{order}"),
        ];
        args.extend(parent.map(|p| format!("subof:{p}")));
        args.push("--".into());
        args.push(description.to_string());
        let output = self.run(&args)?;
        created_uuid(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| TaskError::Failed {
            program: self.program.clone(),
            stderr: "add printed no uuid".into(),
        })
    }

    /// Skips `run` on purpose: its overrides would make every declaration
    /// look present, and setup needs to know what the taskrc holds.
    fn rc_value(&self, key: &str) -> Result<Option<String>, TaskError> {
        let output = self.spawn(&["_get".to_string(), format!("rc.{key}")])?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }
}

/// With neither, native stays so the error names the program that is missing.
fn native_or_default_distro(native_found: bool, default_distro: Option<String>) -> Side {
    match default_distro {
        Some(distro) if !native_found => Side::Wsl(distro),
        _ => Side::Native,
    }
}

fn is_busy(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("database is locked") || lower.contains("sqlite_busy")
}

fn created_uuid(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("Created task ")?.trim_end_matches('.');
        (rest.len() == 36 && rest.matches('-').count() == 4).then(|| rest.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritree_common::jobs;

    /// A private taskwarrior, so the user's rc, data and hooks never run.
    /// Taskwarrior 3 has no Windows build, so a Windows host without one uses
    /// the default distro's, with the paths carried in through `WSLENV`.
    fn private() -> Option<(tempfile::TempDir, Side, Taskwarrior)> {
        type Home = Option<(tempfile::TempDir, Side, String)>;
        let native: fn() -> Home = || {
            let dir = tempfile::tempdir().expect("a temp dir");
            let root = dir.path().to_str().expect("a UTF-8 temp dir").to_string();
            Some((dir, Side::Native, root))
        };
        // On the distro's own filesystem. Taskwarrior's SQLite store on a
        // `/mnt/c` path takes seconds per write, and minutes per test once
        // the machine is busy.
        let distro: fn() -> Home = || {
            let distro = wsl::distros().into_iter().find(|d| d.is_default)?.name;
            let tmp = std::path::PathBuf::from(format!(r"\\wsl.localhost\{distro}\tmp"));
            let dir = tempfile::Builder::new().prefix("alacritree-task").tempdir_in(tmp).ok()?;
            let root = wsl::windows_to_linux(dir.path())?;
            Some((dir, Side::Wsl(distro), root))
        };
        [native, distro].into_iter().find_map(|home| {
            let (dir, side, root) = home()?;
            std::fs::write(dir.path().join("taskrc"), "").expect("an empty taskrc");
            let mut env = vec![
                ("TASKRC".to_string(), format!("{root}/taskrc")),
                ("TASKDATA".to_string(), format!("{root}/data")),
            ];
            if matches!(side, Side::Wsl(_)) {
                env.push(("WSLENV".into(), "TASKRC:TASKDATA".into()));
            }
            let tw = Taskwarrior { env };
            // Probed on exactly this side: the backend's own fallback to a
            // distro would carry a native path there. Only a missing program
            // skips; any other failure is the adapter's.
            match jobs::on_this_thread(|b| tw.on(side.clone(), b).run(&["export".into()])) {
                Err(TaskError::Missing { .. }) => None,
                result => {
                    Some(result.map(|_| (dir, side, tw)).expect("a private taskwarrior exports"))
                },
            }
        })
    }

    fn add(
        tw: &Taskwarrior,
        side: &Side,
        project: &str,
        text: &str,
        parent: Option<&str>,
    ) -> String {
        jobs::on_this_thread(|b| tw.for_project(side, b).add(project, text, parent, 1024)).unwrap()
    }

    fn find(tw: &Taskwarrior, side: &Side, id: &str) -> Option<Task> {
        let filter = Filter { nodes: vec![NodeMatch::Subtree("r".into())] };
        let tasks = jobs::on_this_thread(|b| tw.list(side, &filter, b)).unwrap();
        tasks.into_iter().find(|t| t.id == id)
    }

    fn apply(tw: &Taskwarrior, side: &Side, edit: Edit) {
        jobs::on_this_thread(|b| tw.apply(side, &edit, b)).unwrap();
    }

    #[test]
    fn add_returns_the_uuid_and_round_trips_parent_and_order() {
        let Some((_dir, side, tw)) = private() else { return };
        let parent = add(&tw, &side, "r.main", "ship", None);
        let child = add(&tw, &side, "r.main", "spec", Some(&parent));
        apply(&tw, &side, Edit::Reorder { id: child.clone(), order: 2048 });
        let task = find(&tw, &side, &child).expect("child listed");
        assert_eq!(task.parent.as_deref(), Some(parent.as_str()));
        assert_eq!(task.order, Some(2048));
        assert_eq!(task.project.as_deref(), Some("r.main"));
        assert_eq!(task.status, Status::Pending);
    }

    #[test]
    fn description_with_attribute_syntax_is_verbatim() {
        let Some((_dir, side, tw)) = private() else { return };
        let text = "fix project:x +tag due:tomorrow";
        let id = add(&tw, &side, "r", text, None);
        let task = find(&tw, &side, &id).unwrap();
        assert_eq!(task.description, text);
        assert_eq!(task.project.as_deref(), Some("r"));
    }

    #[test]
    fn describe_replaces_the_text_verbatim() {
        let Some((_dir, side, tw)) = private() else { return };
        let id = add(&tw, &side, "r", "old", None);
        apply(&tw, &side, Edit::Describe { id: id.clone(), description: "new +not-a-tag".into() });
        assert_eq!(find(&tw, &side, &id).unwrap().description, "new +not-a-tag");
    }

    #[test]
    fn every_edit_works_by_uuid_without_prompting() {
        let Some((_dir, side, tw)) = private() else { return };
        let parent = add(&tw, &side, "r", "p", None);
        let id = add(&tw, &side, "r", "t", None);
        apply(&tw, &side, Edit::Start(id.clone()));
        assert!(find(&tw, &side, &id).unwrap().started);
        apply(&tw, &side, Edit::Stop(id.clone()));
        assert!(!find(&tw, &side, &id).unwrap().started);
        apply(&tw, &side, Edit::Done(id.clone()));
        assert_eq!(find(&tw, &side, &id).unwrap().status, Status::Completed);
        apply(&tw, &side, Edit::Undone(id.clone()));
        assert_eq!(find(&tw, &side, &id).unwrap().status, Status::Pending);
        apply(&tw, &side, Edit::Move { id: id.clone(), parent: Some(parent.clone()), order: 7 });
        let moved = find(&tw, &side, &id).unwrap();
        assert_eq!((moved.parent.as_deref(), moved.order), (Some(parent.as_str()), Some(7)));
        apply(&tw, &side, Edit::Move { id: id.clone(), parent: None, order: 9 });
        assert_eq!(find(&tw, &side, &id).unwrap().parent, None);
        apply(&tw, &side, Edit::Delete(id.clone()));
        assert_eq!(find(&tw, &side, &id), None, "a deleted task is not listed");
    }

    #[test]
    fn setup_declares_what_the_check_reads_and_overrides_do_not_count() {
        let Some((_dir, side, tw)) = private() else { return };
        let declared = |b: &Blocking| tw.fields_declared(side.clone(), b).unwrap();
        assert!(!jobs::on_this_thread(declared), "the adapter's own overrides must not count");
        let written = jobs::on_this_thread(|b| tw.declare_fields(side.clone(), b)).unwrap();
        assert_eq!(written.len(), UDA_DECLARATIONS.len());
        assert!(jobs::on_this_thread(declared));
        let again = jobs::on_this_thread(|b| tw.declare_fields(side.clone(), b)).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn a_listing_larger_than_a_pipe_buffer_is_read_whole() {
        let Some((_dir, side, tw)) = private() else { return };
        let text = "x".repeat(10_000);
        for _ in 0..8 {
            add(&tw, &side, "r", &text, None);
        }
        let filter = Filter { nodes: vec![NodeMatch::Exact("r".into())] };
        let tasks = jobs::on_this_thread(|b| tw.list(&side, &filter, b)).unwrap();
        assert_eq!(tasks.len(), 8);
        assert!(tasks.iter().all(|t| t.description == text));
    }

    #[test]
    fn a_missing_program_is_reported_as_missing() {
        let cli =
            Cli { side: Side::Native, program: "alacritree-no-such-task-binary".into(), env: &[] };
        let err = cli.run(&["export".into()]).unwrap_err();
        assert!(matches!(err, TaskError::Missing { .. }), "{err}");
    }

    #[test]
    fn a_windows_project_without_task_uses_the_default_distro() {
        let ubuntu = || Some("Ubuntu".to_string());
        assert_eq!(native_or_default_distro(false, ubuntu()), Side::Wsl("Ubuntu".into()));
        assert_eq!(native_or_default_distro(true, ubuntu()), Side::Native);
        assert_eq!(native_or_default_distro(false, None), Side::Native);
    }

    #[test]
    fn a_subtree_filter_keeps_other_repos_out() {
        let filter = Filter {
            nodes: vec![NodeMatch::Subtree("r".into()), NodeMatch::Exact("global".into())],
        };
        assert_eq!(filter_args(&filter).unwrap(), [
            "(project.is:r or project:r. or project.is:global)".to_string(),
            "(status:pending or status:completed)".to_string(),
        ]);
        assert_eq!(filter_args(&Filter { nodes: Vec::new() }), None);
    }

    #[test]
    fn a_move_to_the_top_level_clears_subof() {
        let edit = Edit::Move { id: "a1".into(), parent: None, order: 1536 };
        assert_eq!(edit_args(&edit), ["a1", "modify", "subof:", "order:1536"]);
        let edit = Edit::Move { id: "b".into(), parent: Some("a".into()), order: 2048 };
        assert_eq!(edit_args(&edit), ["b", "modify", "subof:a", "order:2048"]);
    }

    /// Agents write with `task` directly, so the guide has to spell the UDAs
    /// and the project the way this backend reads them back.
    #[test]
    fn the_agent_guide_names_the_project_and_the_udas() {
        let guide = Taskwarrior::default().agent_guide("r.main.codex-s1");
        assert!(guide.contains("task add project:r.main.codex-s1 order:<n> subof:"), "{guide}");
        assert!(guide.contains("alacritree task setup"), "{guide}");
    }

    #[test]
    fn busy_stderr_is_retryable_and_others_are_not() {
        assert!(is_busy("database is locked"));
        assert!(is_busy("Error: SQLITE_BUSY"));
        assert!(!is_busy("No matches."));
    }

    #[test]
    fn the_new_uuid_is_read_from_verbose_output() {
        let out = "Created task 8e4a5a4e-1f2b-4c3d-9e8f-0a1b2c3d4e5f.\n";
        assert_eq!(created_uuid(out).as_deref(), Some("8e4a5a4e-1f2b-4c3d-9e8f-0a1b2c3d4e5f"));
        assert_eq!(created_uuid("Created task 5."), None);
    }
}
