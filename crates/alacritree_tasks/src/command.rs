//! A task store the user defines: one program and one argument list per
//! operation, in the same shape as the custom diff viewer and command
//! checkout hooks.

use alacritree_common::jobs::Blocking;
use alacritree_common::side::{self, Program, Ran, Side};
use serde::Deserialize;

use crate::{Edit, Filter, Task, TaskBackend, TaskError};

/// `[integrations.tasks]`: which task store runs, when it is not taskwarrior.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TasksConfig {
    /// `None` leaves the lists in taskwarrior.
    pub command: Option<TaskCommand>,
}

impl Default for TasksConfig {
    fn default() -> Self {
        RawTasks::default().resolve()
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RawTasks {
    /// A task store run through a program of your own, in place of
    /// taskwarrior.
    command: RawTaskCommand,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RawTaskCommand {
    /// Keep task lists through this command instead of taskwarrior, and show
    /// them in a tab (`OpenTasks`, Ctrl+~). Agents read them through
    /// `alacritree hook`. Needs `path`.
    enabled: bool,
    /// The program to run on Windows or natively. A bare name is looked up
    /// on PATH.
    path: String,
    /// The program to run inside a WSL distro for a project there, as
    /// written. Empty looks up the file name of `path`, without directory or
    /// extension, through the distro's login shell. A Windows `path` is never
    /// run there.
    wsl_path: String,
    /// Arguments that print the pending and completed tasks as a JSON array,
    /// in the shape `alacritree-tasks.json` describes. That schema is
    /// attached to each release. alacritree keeps the tasks under the project
    /// nodes it shows.
    list: Vec<String>,
    /// Arguments that add a task. `{project}` is its node, `{description}`
    /// its text, `{parent}` the id it nests under, empty at the top level,
    /// and `{order}` its place among its siblings.
    add: Vec<String>,
    /// Arguments that nest task `{id}` under `{parent}`, empty for the top
    /// level, at `{order}` among its new siblings.
    #[serde(rename = "move")]
    move_to: Vec<String>,
    /// Arguments that give task `{id}` the place `{order}` among its
    /// siblings.
    reorder: Vec<String>,
    /// Arguments that replace the text of task `{id}` with `{description}`.
    describe: Vec<String>,
    /// Arguments that complete task `{id}`.
    done: Vec<String>,
    /// Arguments that make completed task `{id}` pending again.
    undone: Vec<String>,
    /// Arguments that mark work on task `{id}` begun.
    start: Vec<String>,
    /// Arguments that mark work on task `{id}` stopped.
    stop: Vec<String>,
    /// Arguments that delete task `{id}`.
    delete: Vec<String>,
    /// What an agent is told ahead of its lists. `{project}` is the node it
    /// writes its own tasks to.
    agent_guide: String,
}

impl Default for RawTaskCommand {
    fn default() -> Self {
        Self {
            enabled: false,
            path: String::new(),
            wsl_path: String::new(),
            list: Vec::new(),
            add: Vec::new(),
            move_to: Vec::new(),
            reorder: Vec::new(),
            describe: Vec::new(),
            done: Vec::new(),
            undone: Vec::new(),
            start: Vec::new(),
            stop: Vec::new(),
            delete: Vec::new(),
            agent_guide: "Task list. Write your own tasks to project {project}.\n".to_string(),
        }
    }
}

impl RawTasks {
    pub fn resolve(self) -> TasksConfig {
        let raw = self.command;
        let path = raw.path.trim();
        let command = match (raw.enabled, path.is_empty()) {
            (false, _) => None,
            (true, true) => {
                log::warn!(
                    "[integrations.tasks.command] is enabled without a path; using taskwarrior"
                );
                None
            },
            (true, false) => Some(TaskCommand {
                program: Program {
                    name: lookup_name(path),
                    native: path.to_string(),
                    wsl: Some(raw.wsl_path.trim().to_string()).filter(|p| !p.is_empty()),
                },
                templates: Templates {
                    list: raw.list,
                    add: raw.add,
                    move_to: raw.move_to,
                    reorder: raw.reorder,
                    describe: raw.describe,
                    done: raw.done,
                    undone: raw.undone,
                    start: raw.start,
                    stop: raw.stop,
                    delete: raw.delete,
                },
                agent_guide: raw.agent_guide,
            }),
        };
        TasksConfig { command }
    }
}

/// The name a distro's login shell finds `path` by. A native path, possibly
/// a Windows one, means nothing inside the distro.
fn lookup_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map_or_else(|| path.to_string(), |stem| stem.to_string_lossy().into_owned())
}

/// One argument list per operation. An empty one means the store cannot do
/// that, and asking it to fails.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Templates {
    pub list: Vec<String>,
    pub add: Vec<String>,
    pub move_to: Vec<String>,
    pub reorder: Vec<String>,
    pub describe: Vec<String>,
    pub done: Vec<String>,
    pub undone: Vec<String>,
    pub start: Vec<String>,
    pub stop: Vec<String>,
    pub delete: Vec<String>,
}

/// The command backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskCommand {
    pub program: Program,
    pub templates: Templates,
    pub agent_guide: String,
}

impl TaskCommand {
    /// The template for `edit` and the values its placeholders take.
    fn call(&self, edit: &Edit) -> (&[String], Vec<(&'static str, String)>) {
        let t = &self.templates;
        let id = |id: &String| vec![("id", id.clone())];
        let parent = |parent: &Option<String>| parent.clone().unwrap_or_default();
        match edit {
            Edit::Add { project, description, parent: p, order } => (&t.add, vec![
                ("project", project.clone()),
                ("description", description.clone()),
                ("parent", parent(p)),
                ("order", order.to_string()),
            ]),
            Edit::Move { id, parent: p, order } => (&t.move_to, vec![
                ("id", id.clone()),
                ("parent", parent(p)),
                ("order", order.to_string()),
            ]),
            Edit::Reorder { id, order } => {
                (&t.reorder, vec![("id", id.clone()), ("order", order.to_string())])
            },
            Edit::Describe { id, description } => {
                (&t.describe, vec![("id", id.clone()), ("description", description.clone())])
            },
            Edit::Done(i) => (&t.done, id(i)),
            Edit::Undone(i) => (&t.undone, id(i)),
            Edit::Start(i) => (&t.start, id(i)),
            Edit::Stop(i) => (&t.stop, id(i)),
            Edit::Delete(i) => (&t.delete, id(i)),
        }
    }

    /// Standard output of a run that exited 0.
    fn run(
        &self,
        side: &Side,
        operation: &'static str,
        args: &[String],
        blocking: &Blocking,
    ) -> Result<Vec<u8>, TaskError> {
        if args.is_empty() {
            return Err(TaskError::Unsupported { operation });
        }
        let program = || self.program.native.clone();
        match side::run(side, &self.program, None, args, blocking) {
            Err(source) => Err(TaskError::Spawn { program: program(), source }),
            Ok(Ran::Missing) => Err(TaskError::Missing { program: program() }),
            Ok(Ran::TimedOut) => Err(TaskError::TimedOut { program: program() }),
            Ok(Ran::Finished(output)) if output.status.success() => Ok(output.stdout),
            Ok(Ran::Finished(output)) => Err(TaskError::Failed {
                program: program(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
        }
    }
}

impl TaskBackend for TaskCommand {
    fn list(
        &self,
        side: &Side,
        filter: &Filter,
        blocking: &Blocking,
    ) -> Result<Vec<Task>, TaskError> {
        let stdout = self.run(side, "list", &self.templates.list, blocking)?;
        let tasks: Vec<Task> = serde_json::from_slice(&stdout).map_err(|source| {
            TaskError::Malformed { program: self.program.native.clone(), source }
        })?;
        Ok(tasks.into_iter().filter(|t| filter.matches(t)).collect())
    }

    fn apply(&self, side: &Side, edit: &Edit, blocking: &Blocking) -> Result<(), TaskError> {
        let (template, values) = self.call(edit);
        let operation: &'static str = edit.into();
        self.run(side, operation, &expand(template, &values), blocking).map(drop)
    }

    fn agent_guide(&self, project: &str) -> String {
        expand_word(&self.agent_guide, &[("project", project.to_string())])
    }
}

fn expand(template: &[String], values: &[(&str, String)]) -> Vec<String> {
    template.iter().map(|word| expand_word(word, values)).collect()
}

/// One pass over `word`, so a value that itself contains `{id}` stays as
/// typed. A brace naming no placeholder is kept.
fn expand_word(word: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let filled = values.iter().find_map(|(name, value)| {
            let after = tail.strip_prefix(name)?.strip_prefix('}')?;
            Some((value, after))
        });
        match filled {
            Some((value, after)) => {
                out.push_str(value);
                rest = after;
            },
            None => {
                out.push('{');
                rest = tail;
            },
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::NodeMatch;
    use alacritree_common::jobs;

    fn config(toml: &str) -> TasksConfig {
        toml::from_str::<RawTasks>(toml).expect("valid TOML").resolve()
    }

    fn command(templates: Templates) -> TaskCommand {
        TaskCommand {
            program: Program { native: "sh".into(), wsl: None, name: "sh".into() },
            templates,
            agent_guide: String::new(),
        }
    }

    #[cfg(unix)]
    fn sh(script: &str) -> Vec<String> {
        vec!["-c".into(), script.into(), "sh".into()]
    }

    #[test]
    fn the_command_is_off_unless_enabled_with_a_path() {
        assert_eq!(config("").command, None);
        assert_eq!(config("[command]\npath = 'tasks'\n").command, None);
        assert_eq!(config("[command]\nenabled = true\n").command, None);
        let on = config("[command]\nenabled = true\npath = '/opt/bin/tasks.exe'\nlist = ['ls']\n");
        let on = on.command.expect("enabled with a path");
        assert_eq!(on.program.name, "tasks", "a distro looks the program up by its file stem");
        assert_eq!(on.templates.list, ["ls"]);
    }

    #[test]
    fn move_is_spelled_as_the_operation() {
        let on = config("[command]\nenabled = true\npath = 't'\nmove = ['mv', '{id}']\n");
        assert_eq!(on.command.unwrap().templates.move_to, ["mv", "{id}"]);
    }

    #[test]
    fn placeholders_fill_in_one_pass() {
        let values = [("id", "a".to_string()), ("description", "fix {id} now".to_string())];
        let words = ["{id}".to_string(), "--text={description}".into(), "{nope}".into()];
        assert_eq!(expand(&words, &values), ["a", "--text=fix {id} now", "{nope}"]);
    }

    #[test]
    fn a_top_level_move_has_an_empty_parent() {
        let c = command(Templates {
            move_to: vec!["{id}".into(), "{parent}".into(), "{order}".into()],
            ..Templates::default()
        });
        let (template, values) = c.call(&Edit::Move { id: "a".into(), parent: None, order: 3 });
        assert_eq!(expand(template, &values), ["a", "", "3"]);
    }

    #[test]
    fn an_operation_without_arguments_is_unsupported() {
        let c = command(Templates::default());
        let err = jobs::on_this_thread(|b| c.apply(&Side::Native, &Edit::Done("a".into()), b));
        assert!(matches!(err, Err(TaskError::Unsupported { operation: "done" })), "{err:?}");
    }

    #[test]
    fn the_guide_names_the_project() {
        let c = TaskCommand {
            agent_guide: "Use `todo {project}`.".into(),
            ..command(Templates::default())
        };
        assert_eq!(c.agent_guide("r.main"), "Use `todo r.main`.");
    }

    #[cfg(unix)]
    #[test]
    fn a_listing_is_read_as_tasks_and_filtered() {
        let json = r#"[{"id":"1","description":"mine","status":"pending","project":"r.main"},
            {"id":"2","description":"other","status":"completed","project":"s"}]"#;
        let c = command(Templates {
            list: sh(&format!("printf '%s' '{json}'")),
            ..Templates::default()
        });
        let filter = Filter { nodes: vec![NodeMatch::Subtree("r".into())] };
        let tasks = jobs::on_this_thread(|b| c.list(&Side::Native, &filter, b)).unwrap();
        assert_eq!(tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["1"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_listing_that_is_not_tasks_is_malformed() {
        let c = command(Templates { list: sh("echo nope"), ..Templates::default() });
        let filter = Filter { nodes: vec![NodeMatch::Exact("global".into())] };
        let err = jobs::on_this_thread(|b| c.list(&Side::Native, &filter, b)).unwrap_err();
        assert!(matches!(err, TaskError::Malformed { .. }), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn an_edit_runs_with_its_values_and_a_failure_carries_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let seen = tmp.path().join("seen");
        let mut done = sh(&format!("printf '%s' \"$1\" > '{}'", seen.display()));
        done.push("{id}".into());
        let c = command(Templates {
            done,
            delete: sh("echo 'no such task' >&2; exit 3"),
            ..Templates::default()
        });
        jobs::on_this_thread(|b| c.apply(&Side::Native, &Edit::Done("a b".into()), b)).unwrap();
        assert_eq!(std::fs::read_to_string(seen).unwrap(), "a b");
        let err = jobs::on_this_thread(|b| c.apply(&Side::Native, &Edit::Delete("a".into()), b));
        assert_eq!(err.unwrap_err().to_string(), "sh failed: no such task");
    }

    #[test]
    fn a_missing_program_is_reported_as_missing() {
        let c = TaskCommand {
            program: Program {
                native: "alacritree-no-such-program".into(),
                wsl: None,
                name: "alacritree-no-such-program".into(),
            },
            ..command(Templates { done: vec!["{id}".into()], ..Templates::default() })
        };
        let err = jobs::on_this_thread(|b| c.apply(&Side::Native, &Edit::Done("a".into()), b));
        assert!(matches!(err, Err(TaskError::Missing { .. })), "{err:?}");
    }
}
