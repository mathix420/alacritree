//! Human-readable output for the CLI.
//!
//! Plain `println!` — no colour, no tables, no alignment.  The replies are
//! short (a handful of projects, a handful of sessions), and `--json` already
//! serves anyone who wants structure.

use serde_json::Value;

use crate::ipc::IpcRequest;

pub fn human(request: &IpcRequest, value: &Value) {
    match request {
        IpcRequest::ListProjects => projects(value),
        IpcRequest::AddProject { .. } | IpcRequest::RefreshProject { .. } => project(value),
        IpcRequest::RemoveProject { .. } => {
            println!("removed {}", text(&value["removed"]));
        },
        IpcRequest::ListSessions => sessions(value),
        IpcRequest::CreateSession { .. } => {
            println!("session {}", text(&value["session_id"]));
        },
        IpcRequest::CloseSession { .. } => {
            println!("closed session {}", text(&value["closed"]));
        },
        IpcRequest::SelectWorkspace { .. } => match value["workspace"].as_str() {
            Some(path) => println!("{path}"),
            None => println!("home"),
        },
        IpcRequest::SendText { .. } => {},
        IpcRequest::ReadScreen { .. } => screen(value),
        IpcRequest::GitStatus { .. } => git_status(value),
        IpcRequest::CreateWorktree { .. } => {
            println!("{}", text(&value["path"]));
        },
    }
}

fn projects(value: &Value) {
    let projects = array(&value["projects"]);
    if projects.is_empty() {
        println!("no projects — add one with `alacritree project add <PATH>`");
        return;
    }
    for p in projects {
        project(p);
    }
}

fn project(value: &Value) {
    let default_branch = value["default_branch"].as_str().unwrap_or("unknown");
    println!("{} ({})  {}", text(&value["name"]), default_branch, text(&value["root"]));
    for wt in array(&value["worktrees"]) {
        let branch = wt["branch"].as_str().unwrap_or("detached");
        println!("  {}  {}  {}", text(&wt["name"]), branch, text(&wt["path"]));
    }
}

fn sessions(value: &Value) {
    let sessions = array(&value["sessions"]);
    if sessions.is_empty() {
        println!("no sessions");
        return;
    }
    for s in sessions {
        // The active tab and an attention flag are the two things worth
        // scanning a list for; everything else is in --json.
        let active = if s["is_active_tab"].as_bool().unwrap_or(false) { "*" } else { " " };
        let attention = if s["needs_attention"].as_bool().unwrap_or(false) { " (!)" } else { "" };
        let workspace = s["workspace"].as_str().unwrap_or("home");
        println!("{active} {}  {}  {workspace}{attention}", text(&s["id"]), text(&s["title"]));
    }
}

fn screen(value: &Value) {
    for line in array(&value["lines"]) {
        println!("{}", line.as_str().unwrap_or_default());
    }
}

fn git_status(value: &Value) {
    let branch = value["branch"].as_str().unwrap_or("unknown");
    println!("on {branch}");

    let files = |label: &str, list: &Value| {
        let list = array(list);
        if list.is_empty() {
            return;
        }
        println!("{label}:");
        for f in list {
            println!("  {}  {}", text(&f["kind"]), text(&f["path"]));
        }
    };
    files("staged", &value["staged"]);
    files("unstaged", &value["unstaged"]);

    let diff = array(&value["diff_vs_default_branch"]);
    if !diff.is_empty() {
        let default_branch = value["default_branch"].as_str().unwrap_or("the default branch");
        println!("vs {default_branch}:");
        for d in diff {
            println!(
                "  +{} -{}  {}",
                text(&d["additions"]),
                text(&d["deletions"]),
                text(&d["path"])
            );
        }
    }
}

/// A JSON string without its quotes, and anything else as it appears in JSON.
/// Paths and titles print as themselves; ids print as numbers.
fn text(value: &Value) -> String {
    match value.as_str() {
        Some(s) => s.to_string(),
        None => value.to_string(),
    }
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` is what keeps paths from printing with quotes around them.
    #[test]
    fn strings_print_without_their_json_quotes() {
        assert_eq!(text(&Value::String("/repo/x".into())), "/repo/x");
        assert_eq!(text(&serde_json::json!(7)), "7");
    }

    /// A missing or null field is normal — a detached worktree has no branch —
    /// and must not panic the renderer.
    #[test]
    fn absent_fields_do_not_panic() {
        for request in [
            IpcRequest::ListProjects,
            IpcRequest::ListSessions,
            IpcRequest::GitStatus { path: "/repo".into() },
            IpcRequest::ReadScreen { session_id: 1, scrollback_lines: 0 },
        ] {
            human(&request, &serde_json::json!({}));
        }
    }
}
