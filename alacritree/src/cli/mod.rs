//! `alacritree <command>` — the terminal-side skin over the IPC surface.
//!
//! Every command is one [`IpcRequest`], the same enum the MCP bridge speaks, so
//! an agent that shells out reaches exactly the surface an agent with an MCP
//! client does.  Running with no subcommand opens the window as before.
//!
//! Dispatch is hybrid: a request goes to a running alacritree if one is
//! listening, and otherwise to [`offline`], which serves what it can from
//! `state.toml` and git directly.  Commands that are meaningless without a
//! window (anything about sessions) fail there rather than pretending.

mod doctor;
mod offline;
mod render;

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use crate::ipc::{self, IpcRequest, SendError};

#[derive(Debug, Parser)]
#[command(name = "alacritree", version, about = "Alacritty fork with worktree-aware sidebars")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Print the raw JSON reply instead of a human summary.
    #[arg(long, global = true)]
    json: bool,

    /// Talk to the instance listening on this socket rather than finding one.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run as an MCP server over stdio, bridging to a running instance.
    Mcp,

    /// Projects in the sidebar.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },

    /// Terminal sessions.  Needs a running alacritree.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },

    /// The focused workspace.  Needs a running alacritree.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },

    /// Branch, staged/unstaged files, and the diff against the default branch.
    GitStatus {
        /// Worktree or repository path.
        path: PathBuf,
    },

    /// Git worktrees.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },

    /// Check the external tools, config and state alacritree depends on.
    Doctor,

    /// Write a shell completion script to stdout.
    Completions { shell: Shell },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List the projects in the sidebar with their worktrees.
    List,
    /// Add a project to the sidebar.
    Add { path: PathBuf },
    /// Remove a project from the sidebar.  Touches no files.
    Remove { root: PathBuf },
    /// Re-scan a project's worktrees and default branch.
    Refresh { root: PathBuf },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List sessions across all workspaces.
    List,
    /// Open a shell session and print its id.
    Create {
        /// Worktree path; omit for the home workspace.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
    },
    /// Close a session, terminating whatever runs in it.
    Close { session_id: u64 },
    /// Write text to a session exactly as if typed.
    SendText {
        session_id: u64,
        text: String,
        /// Append a carriage return, submitting the line.
        ///
        /// A shell passes argv through verbatim, so a trailing `\r` in the text
        /// arrives as a backslash and an `r` — the command would be typed and
        /// never run.  (An MCP client has no such problem: JSON decodes the
        /// escape for it.)
        #[arg(long)]
        enter: bool,
    },
    /// Print a session's terminal contents.
    ReadScreen {
        session_id: u64,
        /// History lines to include above the visible screen.
        #[arg(long, value_name = "N", default_value_t = 0)]
        scrollback: usize,
    },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Focus a workspace.  Omit the path for the home workspace.
    Select { path: Option<PathBuf> },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    /// Create a worktree on a new branch, off the project's default branch.
    Create { project_root: PathBuf, branch: String },
}

/// Run the CLI, or hand back to the caller to open a window.
///
/// `Some(code)` is a process exit code; `None` means no subcommand was given
/// and this invocation is a plain `alacritree`.
pub fn run(cli: Cli) -> Option<i32> {
    let request = match cli.command? {
        Command::Completions { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
            return Some(0);
        },
        Command::Mcp => {
            crate::mcp::run(cli.socket);
            return Some(0);
        },
        // Diagnosing the machine is not something a running instance can answer:
        // the report has to be truthful when there is nothing to ask.
        Command::Doctor => return Some(doctor::run(cli.json, cli.socket.as_deref())),
        other => to_request(other),
    };

    Some(execute(&request, cli.socket.as_deref(), cli.json))
}

fn execute(request: &IpcRequest, socket: Option<&Path>, as_json: bool) -> i32 {
    match dispatch(request, socket) {
        Ok(value) => {
            if as_json {
                println!("{:#}", value);
            } else {
                render::human(request, &value);
            }
            0
        },
        // In JSON mode the error goes to stdout as JSON too, so a caller parses
        // one stream and never has to interleave two.
        Err(e) if as_json => {
            println!("{:#}", serde_json::json!({ "error": e.to_string() }));
            1
        },
        Err(e) => {
            eprintln!("alacritree: {e}");
            1
        },
    }
}

/// Ask a running alacritree, falling back to serving the request ourselves.
fn dispatch(request: &IpcRequest, socket: Option<&Path>) -> Result<serde_json::Value, SendError> {
    match ipc::send_request(socket, request, timeout_for(request)) {
        Err(SendError::NoInstance) => offline::handle(request).map_err(SendError::Failed),
        result => result,
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

fn to_request(command: Command) -> IpcRequest {
    match command {
        Command::Project { command } => match command {
            ProjectCommand::List => IpcRequest::ListProjects,
            ProjectCommand::Add { path } => IpcRequest::AddProject { path: absolute(path) },
            ProjectCommand::Remove { root } => IpcRequest::RemoveProject { root: absolute(root) },
            ProjectCommand::Refresh { root } => IpcRequest::RefreshProject { root: absolute(root) },
        },
        Command::Session { command } => match command {
            SessionCommand::List => IpcRequest::ListSessions,
            SessionCommand::Create { workspace } => {
                IpcRequest::CreateSession { workspace: workspace.map(absolute) }
            },
            SessionCommand::Close { session_id } => IpcRequest::CloseSession { session_id },
            SessionCommand::SendText { session_id, text, enter } => {
                let text = if enter { text + "\r" } else { text };
                IpcRequest::SendText { session_id, text }
            },
            SessionCommand::ReadScreen { session_id, scrollback } => {
                IpcRequest::ReadScreen { session_id, scrollback_lines: scrollback }
            },
        },
        Command::Workspace { command } => match command {
            WorkspaceCommand::Select { path } => {
                IpcRequest::SelectWorkspace { path: path.map(absolute) }
            },
        },
        Command::GitStatus { path } => IpcRequest::GitStatus { path: absolute(path) },
        Command::Worktree { command } => match command {
            WorktreeCommand::Create { project_root, branch } => {
                IpcRequest::CreateWorktree { project_root: absolute(project_root), branch }
            },
        },
        // None of these reach an alacritree, so none has a request to build.
        Command::Completions { .. } | Command::Mcp | Command::Doctor => {
            unreachable!("handled before dispatch")
        },
    }
}

/// Make a path absolute without resolving symlinks or touching the disk.
///
/// A shell hands us `.` or `../repo`, but the sidebar stores what the folder
/// picker gave it, which is always absolute — so a relative path would match
/// nothing.  `canonicalize` would also work, except on Windows it returns a
/// `\\?\` path that matches neither the stored root nor anything a user would
/// recognise in output.
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_for(args: &[&str]) -> IpcRequest {
        let cli = Cli::try_parse_from(args).expect("parses");
        to_request(cli.command.expect("a subcommand"))
    }

    /// clap's own structural check: conflicting flags, bad defaults, a `global`
    /// on a positional, and so on.  Cheap, and catches things at test time that
    /// otherwise panic in a user's shell.
    #[test]
    fn the_command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_command_maps_to_its_request() {
        assert!(matches!(
            request_for(&["alacritree", "project", "list"]),
            IpcRequest::ListProjects
        ));
        assert!(matches!(
            request_for(&["alacritree", "session", "list"]),
            IpcRequest::ListSessions
        ));
        assert!(matches!(
            request_for(&["alacritree", "session", "close", "7"]),
            IpcRequest::CloseSession { session_id: 7 }
        ));
        assert!(matches!(
            request_for(&["alacritree", "session", "send-text", "7", "ls"]),
            IpcRequest::SendText { session_id: 7, text } if text == "ls"
        ));
        assert!(matches!(
            request_for(&["alacritree", "session", "read-screen", "7", "--scrollback", "50"]),
            IpcRequest::ReadScreen { session_id: 7, scrollback_lines: 50 }
        ));
        assert!(matches!(
            request_for(&["alacritree", "worktree", "create", ".", "topic"]),
            IpcRequest::CreateWorktree { branch, .. } if branch == "topic"
        ));
    }

    /// The shell hands us argv verbatim, so a user who writes `'ls\r'` sends a
    /// backslash and an `r` — the command is typed into the terminal and never
    /// runs.  `--enter` is the only way to submit a line from a shell.
    #[test]
    fn enter_submits_the_line_and_is_off_by_default() {
        assert!(matches!(
            request_for(&["alacritree", "session", "send-text", "1", "ls", "--enter"]),
            IpcRequest::SendText { text, .. } if text == "ls\r"
        ));
        assert!(matches!(
            request_for(&["alacritree", "session", "send-text", "1", "ls"]),
            IpcRequest::SendText { text, .. } if text == "ls"
        ));
    }

    /// `read-screen` without `--scrollback` asks for the visible screen, not
    /// for however much history the session happens to hold.
    #[test]
    fn read_screen_defaults_to_no_scrollback() {
        assert!(matches!(
            request_for(&["alacritree", "session", "read-screen", "1"]),
            IpcRequest::ReadScreen { scrollback_lines: 0, .. }
        ));
    }

    /// Omitting the path means the home workspace — a distinct target, not a
    /// missing argument.
    #[test]
    fn workspace_select_without_a_path_means_home() {
        assert!(matches!(
            request_for(&["alacritree", "workspace", "select"]),
            IpcRequest::SelectWorkspace { path: None }
        ));
    }

    /// The sidebar stores absolute roots, so a relative path from a shell has to
    /// be made absolute before it can match one.
    #[test]
    fn relative_paths_are_made_absolute() {
        let IpcRequest::AddProject { path } = request_for(&["alacritree", "project", "add", "."])
        else {
            panic!("expected an add_project request");
        };
        assert!(path.is_absolute(), "{} is not absolute", path.display());
    }

    /// No subcommand is not an error: it is how the window gets opened.
    #[test]
    fn no_subcommand_opens_the_window() {
        let cli = Cli::try_parse_from(["alacritree"]).expect("parses");
        assert!(cli.command.is_none());
        assert_eq!(run(cli), None);
    }

    /// With an app listening, the request must reach it — and the offline path
    /// must stay out of it.  Falling back while a window is open would edit
    /// `state.toml` behind the app's back, where the change would not show in
    /// the sidebar until the next restart.
    ///
    /// The request is deliberately a read-only one.  `offline::handle` resolves
    /// the *real* `state.toml` — the user's — so a test that fell through to it
    /// with a mutating request would edit the config of whoever ran the suite.
    #[test]
    fn a_running_app_answers_instead_of_the_offline_path() {
        let (socket, requests) =
            ipc::listen_for_test("cli-online", egui::Context::default()).expect("listener");

        let app = std::thread::spawn(move || {
            let call = requests.recv().expect("the request reached the app");
            call.reply_tx
                .send(Ok(serde_json::json!({ "projects": "answered by the app" })))
                .unwrap();
        });

        let reply = dispatch(&IpcRequest::ListProjects, Some(socket.path())).expect("a reply");

        // The offline path would answer with the real project list, so this
        // sentinel is only reachable through the socket.
        assert_eq!(reply["projects"], "answered by the app");
        app.join().unwrap();
    }

    /// The fallback triggers on nothing listening, not on an error message, so
    /// a socket with no app behind it must report exactly that.
    #[test]
    fn a_dead_socket_means_no_instance() {
        let dead = std::env::temp_dir().join("alacritree-not-listening.sock");

        let result =
            ipc::send_request(Some(&dead), &IpcRequest::ListProjects, Duration::from_secs(5));

        assert_eq!(result, Err(SendError::NoInstance));
    }
}
