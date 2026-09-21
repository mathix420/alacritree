//! `alacritree hook <event>`: the one command a harness's hook config calls.
//! It never blocks a turn. Every failure prints nothing and exits 0, and
//! the only thing it prints is one JSON object the harness adds to the
//! model's context.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::digest::stable_digest;
use crate::tasks::scope::{GLOBAL, Harness, Place, SessionRef, node, sanitize};
use crate::tasks::taskwarrior::{Status, Task, Taskwarrior};
use crate::tasks::{facts, tree};
use crate::{jobs, wsl};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Event {
    SessionStart,
    UserPromptSubmit,
}

impl Event {
    fn wire_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
        }
    }
}

#[derive(Deserialize, Default)]
struct Payload {
    session_id: Option<String>,
    cwd: Option<PathBuf>,
}

pub(crate) fn output(event: Event, context: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event.wire_name(),
            "additionalContext": context,
        }
    })
    .to_string()
}

/// The agent's own session and every scope above it. Other agents' lists
/// stay out so none picks up another's work.
fn visible_nodes(place: &Place, session: Option<&SessionRef>) -> Vec<String> {
    let mut nodes = vec![GLOBAL.to_string()];
    let repo = match place {
        Place::Global => None,
        Place::Project { repo } | Place::Workspace { repo, .. } => Some(repo.clone()),
    };
    nodes.extend(repo.map(|repo| node(&Place::Project { repo }, None)));
    if let Place::Workspace { .. } = place {
        nodes.push(node(place, None));
        if session.is_some() {
            nodes.push(node(place, session));
        }
    }
    nodes
}

pub(crate) fn context(place: &Place, session: Option<&SessionRef>, tasks: &[Task]) -> String {
    let target = node(place, session);
    let mut text = format!(
        "Task list, kept in taskwarrior. Write your own tasks to project `{target}`.\n- add: \
         `task add project:{target} order:<n> subof:<parent uuid> -- <text>` (subof is \
         optional)\n- finish: `task <uuid> done`; begin: `task <uuid> start`\n`alacritree task \
         scope` prints the project. If `subof:` or `order:` end up inside a description, run \
         `alacritree task setup` once.\n"
    );
    for scope in visible_nodes(place, session).iter().rev() {
        let in_scope: Vec<&Task> =
            tasks.iter().filter(|t| t.project.as_deref() == Some(scope.as_str())).collect();
        if in_scope.is_empty() {
            continue;
        }
        text.push_str(&format!("\n## {scope}\n"));
        for row in tree::rows(&in_scope) {
            let mark = if row.status == Status::Completed { "x" } else { " " };
            let started = if row.started { " (in progress)" } else { "" };
            let short = row.uuid.get(..8).unwrap_or(&row.uuid);
            let indent = "  ".repeat(row.depth);
            text.push_str(&format!("{indent}- [{mark}] {}{started} ({short})\n", row.text));
        }
    }
    text
}

fn digest_path(state_dir: &Path, session: &SessionRef) -> PathBuf {
    let name = format!("{}-{}.digest", session.harness.prefix(), sanitize(&session.id));
    state_dir.join("task-hooks").join(name)
}

/// A harness inside WSL reports a Linux cwd, while this Windows process
/// starts in the same directory's `\\wsl.localhost` form. The distro comes
/// from there, since WSL passes no variable naming it to Windows processes.
fn on_this_host(cwd: PathBuf, here: Option<PathBuf>) -> Option<PathBuf> {
    let linux = cfg!(windows) && cwd.to_str().is_some_and(|s| s.starts_with('/'));
    if !linux {
        return Some(cwd);
    }
    match here.as_deref().map(wsl::classify) {
        Some(wsl::Location::Wsl { distro, .. }) => {
            Some(wsl::linux_to_windows(cwd.to_str()?, &distro))
        },
        _ => here,
    }
}

/// `None` means print nothing, which is where every failure lands.
pub(crate) fn run(
    event: Event,
    harness: Harness,
    stdin: &str,
    state_dir: Option<&Path>,
) -> Option<String> {
    let payload: Payload = serde_json::from_str(stdin).unwrap_or_default();
    let here = std::env::current_dir().ok();
    let cwd = match payload.cwd {
        Some(cwd) => on_this_host(cwd, here)?,
        None => here?,
    };
    let session =
        payload.session_id.filter(|id| !id.trim().is_empty()).map(|id| SessionRef { harness, id });
    let (place, tasks) = jobs::on_this_thread(|b| {
        let (side, place) = facts::place_for(&cwd, b);
        let scopes = visible_nodes(&place, session.as_ref())
            .iter()
            .map(|n| format!("project.is:{n}"))
            .collect::<Vec<_>>()
            .join(" or ");
        let filter = [format!("({scopes})"), "(status:pending or status:completed)".to_string()];
        let tasks = Taskwarrior::for_side(side, b).export(&filter, b);
        tasks.map(|tasks| (place, tasks))
    })
    .ok()?;
    let text = context(&place, session.as_ref(), &tasks);
    // Session start records what it showed too, so the first prompt after it
    // does not repeat an unchanged list.
    if let (Some(dir), Some(session)) = (state_dir, session.as_ref()) {
        let path = digest_path(dir, session);
        let digest = format!("{:016x}", stable_digest(text.as_bytes()));
        let seen = std::fs::read_to_string(&path).is_ok_and(|s| s == digest);
        if seen && event == Event::UserPromptSubmit {
            return None;
        }
        if !seen {
            let _ = path.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(&path, digest);
        }
    }
    Some(output(event, &text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(uuid: &str, project: &str, text: &str, status: Status) -> Task {
        Task {
            uuid: uuid.into(),
            description: text.into(),
            status,
            start: None,
            subof: None,
            order: Some(1024),
            project: Some(project.into()),
            entry: None,
            modified: None,
        }
    }

    #[test]
    fn context_lists_each_visible_scope_and_names_the_write_target() {
        let place = Place::Workspace { repo: "r".into(), branch: "main".into() };
        let me = SessionRef { harness: Harness::Codex, id: "s1".into() };
        let tasks = [
            task("11111111-aaaa", "global", "global chore", Status::Pending),
            task("22222222-bbbb", "r.main", "ship it", Status::Completed),
            task("33333333-cccc", "r.main.codex-s1", "my step", Status::Pending),
            task("44444444-dddd", "r.main.claude-other", "not mine", Status::Pending),
        ];
        let text = context(&place, Some(&me), &tasks);
        assert!(text.contains("`r.main.codex-s1`"));
        assert!(text.contains("- [ ] my step (33333333)"));
        assert!(text.contains("- [x] ship it (22222222)"));
        assert!(text.contains("global chore"));
        assert!(!text.contains("not mine"), "other agents' lists stay out");
        assert!(text.contains("subof:"));
    }

    #[test]
    fn without_a_session_the_workspace_is_the_write_target() {
        let place = Place::Workspace { repo: "r".into(), branch: "main".into() };
        assert!(context(&place, None, &[]).contains("`r.main`"));
    }

    #[test]
    fn output_is_one_json_object_for_the_event() {
        let json: serde_json::Value =
            serde_json::from_str(&output(Event::UserPromptSubmit, "ctx")).unwrap();
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(json["hookSpecificOutput"]["additionalContext"], "ctx");
        assert_eq!(json.as_object().unwrap().len(), 1);
    }
}
