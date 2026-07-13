//! `alacritree mcp` — a Model Context Protocol server over stdio.
//!
//! Bridges MCP tool calls to a running alacritree instance through the IPC
//! socket (see `ipc.rs`), so an LLM can inspect projects/worktrees, drive
//! terminal sessions, and read their output.  Register it with e.g.
//! `claude mcp add alacritree -- alacritree mcp`.
//!
//! The MCP stdio transport is newline-delimited JSON-RPC 2.0.  The handful
//! of methods a tools-only server needs is small enough that speaking the
//! protocol directly beats pulling an SDK (and its async runtime) into a
//! crate that is otherwise fully synchronous.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::ipc::{self, IpcRequest};

pub fn run(socket: Option<PathBuf>) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_message(&error_response(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            },
        };
        // Requests without an id are notifications (initialized, cancelled…)
        // and must not be answered.
        let Some(id) = message.get("id").cloned() else { continue };
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = message.get("params");

        let response = match method {
            "initialize" => result_response(id, initialize_result(params)),
            "ping" => result_response(id, json!({})),
            "tools/list" => result_response(id, json!({ "tools": tool_definitions() })),
            "tools/call" => tool_call_response(id, params, socket.as_deref()),
            other => error_response(id, -32601, &format!("method not found: {other}")),
        };
        write_message(&response);
    }
}

fn write_message(message: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(message.to_string().as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize_result(params: Option<&Value>) -> Value {
    // Echo the client's protocol version: this server only uses the baseline
    // feature set (tools), which every revision to date supports.
    let version = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or("2025-06-18");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "alacritree", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn tool_call_response(id: Value, params: Option<&Value>, socket: Option<&Path>) -> Value {
    let name = params.and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or_default();
    let arguments = params
        .and_then(|p| p.get("arguments"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Tool names are the serde tags of `IpcRequest`, so name + arguments
    // deserialize straight into a request.
    let mut tagged = arguments;
    tagged.insert("type".to_string(), Value::String(name.to_string()));
    let request: IpcRequest = match serde_json::from_value(Value::Object(tagged)) {
        Ok(r) => r,
        Err(e) => return error_response(id, -32602, &format!("invalid tool call: {e}")),
    };

    match ipc::send_request(socket, &request, timeout_for(&request)) {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            result_response(id, json!({ "content": [{ "type": "text", "text": text }] }))
        },
        // Tool-level failure (not a protocol error): the model should see the
        // message and adapt, e.g. call list_projects after "unknown worktree".
        Err(e) => result_response(
            id,
            json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
        ),
    }
}

fn timeout_for(request: &IpcRequest) -> Duration {
    match request {
        // Runs `git fetch` against origin.
        IpcRequest::CreateWorktree { .. } => Duration::from_secs(300),
        // Walks the working tree; large repos take a while cold.
        IpcRequest::GitStatus { .. } => Duration::from_secs(60),
        _ => Duration::from_secs(15),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_projects",
            "description": "List the projects in alacritree's sidebar, each with its git worktrees (name, path, branch) and default branch. Also reports which workspace is currently focused. Worktree paths from here are what the other tools accept.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "list_sessions",
            "description": "List terminal sessions across all workspaces: id, title, workspace path (null = the home workspace), kind (shell or diff pane), grid size, whether it is its workspace's active tab, and whether it flagged for attention (bell / agent finished).",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "select_workspace",
            "description": "Focus a workspace in the alacritree window, like clicking it in the sidebar. Pass a worktree path from list_projects, or omit path for the home workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Worktree path; omit for the home workspace." },
                },
            },
        },
        {
            "name": "create_session",
            "description": "Open a new terminal (shell) session and return its id. workspace must be a worktree path known to the sidebar; omit it for the home workspace. The session becomes its workspace's active tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Worktree path; omit for the home workspace." },
                },
            },
        },
        {
            "name": "close_session",
            "description": "Close a terminal session, terminating whatever is running in it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "integer" },
                },
                "required": ["session_id"],
            },
        },
        {
            "name": "send_text",
            "description": "Write text to a session's terminal exactly as if typed. Control characters pass through (\"\\u0003\" is Ctrl-C); end with \"\\r\" to submit a shell command line. Use read_screen afterwards to see the result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "integer" },
                    "text": { "type": "string" },
                },
                "required": ["session_id", "text"],
            },
        },
        {
            "name": "read_screen",
            "description": "Read a session's terminal contents as lines of text (top to bottom), plus the cursor position (as indices into those lines) and the window title. scrollback_lines prepends up to that many history lines above the visible screen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "integer" },
                    "scrollback_lines": { "type": "integer", "description": "History lines to include above the visible screen (default 0)." },
                },
                "required": ["session_id"],
            },
        },
        {
            "name": "git_status",
            "description": "Git status for a worktree path: current branch, staged and unstaged files, and per-file +/- line counts against the merge base with the default branch (what alacritree's git sidebar shows).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Worktree (or repository) path." },
                },
                "required": ["path"],
            },
        },
        {
            "name": "create_worktree",
            "description": "Create a new git worktree with a new branch in a project, like the sidebar's + button: fetches origin, branches from the project's default branch, and copies LLM config files into the new worktree. Slow — waits on git fetch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project_root": { "type": "string", "description": "Project root path from list_projects." },
                    "branch": { "type": "string", "description": "New branch name (also names the worktree directory)." },
                },
                "required": ["project_root", "branch"],
            },
        },
        {
            "name": "refresh_project",
            "description": "Re-scan a project's worktrees and default branch (after changing worktrees outside alacritree).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Project root path from list_projects." },
                },
                "required": ["root"],
            },
        },
    ])
}
