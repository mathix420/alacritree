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
        IpcRequest::AddProject { .. }
        | IpcRequest::RefreshProject { .. }
        | IpcRequest::RenameProject { .. } => project(value),
        IpcRequest::RemoveProject { .. } => {
            println!("removed {}", text(&value["removed"]));
        },
        IpcRequest::ListSessions => sessions(value),
        IpcRequest::ListMultiplexerPanes => multiplexer_panes(value),
        IpcRequest::AttachMultiplexerPane { .. } | IpcRequest::CreateMultiplexerPane { .. } => {
            println!("session {}", text(&value["session_id"]));
        },
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
        IpcRequest::MoveSession { .. } => match value["workspace"].as_str() {
            Some(path) => println!("moved session {} to {path}", text(&value["session_id"])),
            None => println!("moved session {} to home", text(&value["session_id"])),
        },
        IpcRequest::ReadScreen { .. } => screen(value),
        IpcRequest::ReadScratchpad { .. } => {
            print!("{}", value["content"].as_str().unwrap_or_default());
        },
        IpcRequest::GitStatus { .. } => git_status(value),
        IpcRequest::CreateWorktree { .. } => {
            println!("{}", text(&value["path"]));
        },
        IpcRequest::RunAction { .. } => {
            println!("ran {}", text(&value["action"]));
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
        let state = match s["agent"]["state"].as_str() {
            Some(state) => format!("  [{state}]"),
            None => String::new(),
        };
        let via = match s["multiplexer"]["name"].as_str() {
            Some(name) => format!("  via {name}"),
            None => String::new(),
        };
        println!(
            "{active} {}  {}  {workspace}{state}{via}{attention}",
            text(&s["id"]),
            text(&s["title"])
        );
    }
}

fn multiplexer_panes(value: &Value) {
    let panes = array(&value["panes"]);
    if panes.is_empty() {
        println!("no multiplexer panes");
        return;
    }
    for p in panes {
        // Side and terminal id lead because together they are what an attach
        // takes; the rest of the line is there to recognise the pane.
        let held = if p["session_id"].is_null() { " " } else { "*" };
        let name = p["title"].as_str().or_else(|| p["kind"].as_str()).unwrap_or("");
        let status = match p["status"].as_str() {
            Some(status) => format!("  [{status}]"),
            None => String::new(),
        };
        let workspace = p["workspace"].as_str().unwrap_or("home");
        println!(
            "{held} {}  {}  {name}{status}  {workspace}",
            text(&p["multiplexer"]["side"]),
            text(&p["multiplexer"]["terminal_id"])
        );
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
            IpcRequest::ReadScratchpad { workspace: Some("home".into()) },
            IpcRequest::ListMultiplexerPanes,
        ] {
            human(&request, &serde_json::json!({}));
        }
    }

    /// A pane nothing is attached to is the whole reason for the listing, and
    /// it carries neither a session id nor a status.
    #[test]
    fn a_pane_line_survives_an_unattached_pane_with_no_agent() {
        human(
            &IpcRequest::ListMultiplexerPanes,
            &serde_json::json!({
                "panes": [{
                    "multiplexer": { "name": "herdr", "side": "wsl:ubuntu", "terminal_id": "t1" },
                    "kind": serde_json::Value::Null,
                    "title": serde_json::Value::Null,
                    "status": serde_json::Value::Null,
                    "focused": false,
                    "workspace": serde_json::Value::Null,
                    "session_id": serde_json::Value::Null,
                }]
            }),
        );
    }

    #[test]
    fn a_session_line_names_its_agent_state_and_multiplexer() {
        human(
            &IpcRequest::ListSessions,
            &serde_json::json!({
                "sessions": [{
                    "id": 1,
                    "title": "claude",
                    "workspace": "/repo",
                    "is_active_tab": true,
                    "needs_attention": false,
                    "agent": { "name": "claude", "state": "working" },
                    "busy": serde_json::Value::Null,
                    "multiplexer": { "name": "herdr", "side": "native", "terminal_id": "t1" },
                }]
            }),
        );
    }
}
