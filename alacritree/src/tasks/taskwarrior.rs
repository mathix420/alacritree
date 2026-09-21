//! The only code that runs `task`. Every call is addressed by uuid, declares
//! the UDAs itself so a taskrc without them never folds `subof:` into a
//! description, and turns off prompts, since there is no terminal to answer
//! them.

use std::io;
use std::process::Output;
use std::time::Duration;

use serde::Deserialize;

use crate::command_ext::hidden;
use crate::jobs::Blocking;
use crate::multiplexer::Side;
use crate::tools::{self, Tool};

/// Also what `alacritree task setup` writes into each side's taskrc.
pub(crate) const UDA_DECLARATIONS: [(&str, &str); 4] = [
    ("uda.subof.type", "uuid"),
    ("uda.subof.label", "Sub of"),
    ("uda.order.type", "numeric"),
    ("uda.order.label", "Order"),
];

/// taskchampion takes an immediate write lock with no busy timeout, so a
/// second writer fails at once instead of waiting its turn.
const BUSY_RETRIES: [Duration; 3] =
    [Duration::from_millis(50), Duration::from_millis(150), Duration::from_millis(400)];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Pending,
    Completed,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct Task {
    pub uuid: String,
    pub description: String,
    pub status: Status,
    pub start: Option<String>,
    pub subof: Option<String>,
    #[serde(default, deserialize_with = "integer_order")]
    pub order: Option<i64>,
    pub project: Option<String>,
    pub entry: Option<String>,
    pub modified: Option<String>,
}

/// Numeric UDAs export as JSON numbers that may carry a fraction.
fn integer_order<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(|n| n.round() as i64))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TaskError {
    Missing { program: String },
    Failed { stderr: String },
    Io(String),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { program } => write!(f, "taskwarrior not found: {program}"),
            Self::Failed { stderr } => write!(f, "task failed: {}", stderr.trim()),
            Self::Io(e) => write!(f, "task could not run: {e}"),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Taskwarrior {
    side: Side,
    program: String,
    env: Vec<(String, String)>,
}

impl Taskwarrior {
    pub(crate) fn for_side(side: Side, blocking: &Blocking) -> Self {
        let program = match &side {
            Side::Native => tools::program(Tool::Task),
            Side::Wsl(distro) => tools::wsl_in_job(Tool::Task, distro, blocking),
        };
        Self { side, program, env: Vec::new() }
    }

    pub(crate) fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// One attempt, no overrides: what `task` itself would do with `args`.
    fn spawn(&self, args: &[String], blocking: &Blocking) -> Result<Output, TaskError> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (program, argv) = self.side.command(&self.program, &refs);
        let mut cmd = hidden(program);
        cmd.args(argv).envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let output = match blocking.run_cancellable(&mut cmd) {
            Ok(output) => output,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(TaskError::Missing { program: self.program.clone() });
            },
            Err(e) => return Err(TaskError::Io(e.to_string())),
        };
        // A WSL login shell reports a missing program as 127, not ENOENT.
        if output.status.code() == Some(127) {
            return Err(TaskError::Missing { program: self.program.clone() });
        }
        Ok(output)
    }

    // Runs on a pool worker, where waiting out a busy lock is the point.
    fn run(&self, args: &[String], blocking: &Blocking) -> Result<Output, TaskError> {
        let mut argv: Vec<String> =
            UDA_DECLARATIONS.iter().map(|(k, v)| format!("rc.{k}={v}")).collect();
        argv.push("rc.confirmation=off".into());
        argv.extend(args.iter().cloned());
        let mut retries = BUSY_RETRIES.iter();
        loop {
            let output = self.spawn(&argv, blocking)?;
            if output.status.success() {
                return Ok(output);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            match retries.next() {
                Some(delay) if is_busy(&stderr) => std::thread::sleep(*delay),
                _ => return Err(TaskError::Failed { stderr }),
            }
        }
    }

    pub(crate) fn export(&self, filter: &[String], b: &Blocking) -> Result<Vec<Task>, TaskError> {
        let mut args = filter.to_vec();
        args.push("export".into());
        let output = self.run(&args, b)?;
        serde_json::from_slice(&output.stdout).map_err(|e| TaskError::Io(e.to_string()))
    }

    /// `--` stops taskwarrior reading `+tag` or `due:` out of the text.
    pub(crate) fn add(
        &self,
        project: &str,
        description: &str,
        subof: Option<&str>,
        order: i64,
        b: &Blocking,
    ) -> Result<String, TaskError> {
        let mut args = vec![
            "rc.verbose=new-uuid".to_string(),
            "add".into(),
            format!("project:{project}"),
            format!("order:{order}"),
        ];
        args.extend(subof.map(|s| format!("subof:{s}")));
        args.push("--".into());
        args.push(description.to_string());
        let output = self.run(&args, b)?;
        created_uuid(&String::from_utf8_lossy(&output.stdout))
            .ok_or_else(|| TaskError::Failed { stderr: "add printed no uuid".into() })
    }

    pub(crate) fn modify(
        &self,
        uuid: &str,
        mods: &[String],
        b: &Blocking,
    ) -> Result<(), TaskError> {
        let mut args = vec![uuid.to_string(), "modify".into()];
        args.extend(mods.iter().cloned());
        self.run(&args, b).map(drop)
    }

    pub(crate) fn describe(&self, uuid: &str, text: &str, b: &Blocking) -> Result<(), TaskError> {
        self.modify(uuid, &["--".into(), text.to_string()], b)
    }

    fn verb(&self, uuid: &str, verb: &str, b: &Blocking) -> Result<(), TaskError> {
        self.run(&[uuid.to_string(), verb.to_string()], b).map(drop)
    }

    pub(crate) fn done(&self, uuid: &str, b: &Blocking) -> Result<(), TaskError> {
        self.verb(uuid, "done", b)
    }

    pub(crate) fn undone(&self, uuid: &str, b: &Blocking) -> Result<(), TaskError> {
        self.modify(uuid, &["status:pending".into()], b)
    }

    pub(crate) fn start(&self, uuid: &str, b: &Blocking) -> Result<(), TaskError> {
        self.verb(uuid, "start", b)
    }

    pub(crate) fn stop(&self, uuid: &str, b: &Blocking) -> Result<(), TaskError> {
        self.verb(uuid, "stop", b)
    }

    pub(crate) fn delete(&self, uuid: &str, b: &Blocking) -> Result<(), TaskError> {
        self.verb(uuid, "delete", b)
    }

    /// Skips `run` on purpose: its overrides would make every declaration
    /// look present, and setup needs to know what the taskrc holds.
    pub(crate) fn rc_value(&self, key: &str, b: &Blocking) -> Result<Option<String>, TaskError> {
        let output = self.spawn(&["_get".to_string(), format!("rc.{key}")], b)?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }

    pub(crate) fn set_config(&self, key: &str, value: &str, b: &Blocking) -> Result<(), TaskError> {
        self.run(&["config".into(), key.to_string(), value.to_string()], b).map(drop)
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
    use crate::jobs;

    /// A private taskwarrior, so the user's rc, data and hooks never run.
    /// Taskwarrior 3 has no Windows build, so a Windows host without one uses
    /// the default distro's, with the paths carried in through `WSLENV`.
    fn private() -> Option<(tempfile::TempDir, Taskwarrior)> {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("taskrc"), "").expect("an empty taskrc");
        let native = Some((Side::Native, dir.path().to_str().unwrap().to_string()));
        let wsl = crate::wsl::distros()
            .into_iter()
            .find(|d| d.is_default)
            .and_then(|d| Some((Side::Wsl(d.name), crate::wsl::windows_to_linux(dir.path())?)));
        let tw = [native, wsl].into_iter().flatten().find_map(|(side, root)| {
            let is_wsl = matches!(side, Side::Wsl(_));
            let mut tw = jobs::on_this_thread(|b| Taskwarrior::for_side(side, b))
                .with_env("TASKRC", &format!("{root}/taskrc"))
                .with_env("TASKDATA", &format!("{root}/data"));
            if is_wsl {
                tw = tw.with_env("WSLENV", "TASKRC:TASKDATA");
            }
            jobs::on_this_thread(|b| tw.export(&[], b)).is_ok().then_some(tw)
        })?;
        Some((dir, tw))
    }

    fn find(tw: &Taskwarrior, uuid: &str) -> Option<Task> {
        jobs::on_this_thread(|b| tw.export(&[], b)).unwrap().into_iter().find(|t| t.uuid == uuid)
    }

    #[test]
    fn add_returns_the_uuid_and_round_trips_subof_and_order() {
        let Some((_dir, tw)) = private() else { return };
        let parent = jobs::on_this_thread(|b| tw.add("r.main", "ship", None, 1024, b)).unwrap();
        let child =
            jobs::on_this_thread(|b| tw.add("r.main", "spec", Some(&parent), 2048, b)).unwrap();
        let task = find(&tw, &child).expect("child exported");
        assert_eq!(task.subof.as_deref(), Some(parent.as_str()));
        assert_eq!(task.order, Some(2048));
        assert_eq!(task.project.as_deref(), Some("r.main"));
        assert_eq!(task.status, Status::Pending);
    }

    #[test]
    fn description_with_attribute_syntax_is_verbatim() {
        let Some((_dir, tw)) = private() else { return };
        let text = "fix project:x +tag due:tomorrow";
        let uuid = jobs::on_this_thread(|b| tw.add("r", text, None, 1024, b)).unwrap();
        let task = find(&tw, &uuid).unwrap();
        assert_eq!(task.description, text);
        assert_eq!(task.project.as_deref(), Some("r"));
    }

    #[test]
    fn describe_replaces_the_text_verbatim() {
        let Some((_dir, tw)) = private() else { return };
        let uuid = jobs::on_this_thread(|b| tw.add("r", "old", None, 1024, b)).unwrap();
        jobs::on_this_thread(|b| tw.describe(&uuid, "new +not-a-tag", b)).unwrap();
        assert_eq!(find(&tw, &uuid).unwrap().description, "new +not-a-tag");
    }

    #[test]
    fn every_verb_works_by_uuid_without_prompting() {
        let Some((_dir, tw)) = private() else { return };
        let uuid = jobs::on_this_thread(|b| tw.add("r", "t", None, 1024, b)).unwrap();
        jobs::on_this_thread(|b| tw.start(&uuid, b)).unwrap();
        assert!(find(&tw, &uuid).unwrap().start.is_some());
        jobs::on_this_thread(|b| tw.stop(&uuid, b)).unwrap();
        assert!(find(&tw, &uuid).unwrap().start.is_none());
        jobs::on_this_thread(|b| tw.done(&uuid, b)).unwrap();
        assert_eq!(find(&tw, &uuid).unwrap().status, Status::Completed);
        jobs::on_this_thread(|b| tw.undone(&uuid, b)).unwrap();
        assert_eq!(find(&tw, &uuid).unwrap().status, Status::Pending);
        jobs::on_this_thread(|b| tw.modify(&uuid, &["order:7".into()], b)).unwrap();
        assert_eq!(find(&tw, &uuid).unwrap().order, Some(7));
        jobs::on_this_thread(|b| tw.delete(&uuid, b)).unwrap();
        assert_eq!(find(&tw, &uuid).unwrap().status, Status::Other);
    }

    #[test]
    fn set_config_is_visible_to_rc_value_and_overrides_are_not() {
        let Some((_dir, tw)) = private() else { return };
        let before = jobs::on_this_thread(|b| tw.rc_value("uda.subof.type", b)).unwrap();
        assert_eq!(before, None, "the adapter's own overrides must not count as declared");
        jobs::on_this_thread(|b| tw.set_config("uda.subof.type", "uuid", b)).unwrap();
        let after = jobs::on_this_thread(|b| tw.rc_value("uda.subof.type", b)).unwrap();
        assert_eq!(after.as_deref(), Some("uuid"));
    }

    #[test]
    fn a_missing_program_is_reported_as_missing() {
        let mut tw = jobs::on_this_thread(|b| Taskwarrior::for_side(Side::Native, b));
        tw.program = "alacritree-no-such-task-binary".into();
        let err = jobs::on_this_thread(|b| tw.export(&[], b)).unwrap_err();
        assert!(matches!(err, TaskError::Missing { .. }), "{err}");
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
