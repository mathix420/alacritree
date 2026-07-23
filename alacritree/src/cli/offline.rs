//! Serving a request with no alacritree running.
//!
//! The sidebar is a view of `state.toml` plus what git says about each root, and
//! both outlive the window — so a request that only needs those two can be
//! answered without an app.  Anything about sessions cannot: a session is a live
//! PTY owned by a process that isn't there.
//!
//! The replies are byte-for-byte the ones a running app would send, so nothing
//! downstream — rendering, `--json`, an agent parsing either — can tell which
//! path answered it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::ipc::{IpcRequest, IpcResult};
use crate::projects::{self, Project, project_json};
use crate::state::{self, PersistedProject, PersistedState};
use crate::worktree::{self as wt, CreateRequest};
use crate::{git_status, ipc, scratchpad};

pub fn handle(request: &IpcRequest) -> IpcResult {
    let Some(path) = state::config_path() else {
        return Err("could not locate alacritree's state file".to_string());
    };
    handle_at(&path, request)
}

fn handle_at(state_path: &Path, request: &IpcRequest) -> IpcResult {
    match request {
        IpcRequest::ListProjects => Ok(json!({
            // No window means no focused workspace — the same value the app
            // reports for its home tab.
            "current_workspace": Value::Null,
            "projects": discover_all(state_path).iter().map(project_json).collect::<Vec<_>>(),
        })),
        IpcRequest::AddProject { path } => Ok(project_json(&add(state_path, path))),
        IpcRequest::RemoveProject { root } => {
            remove(state_path, root)?;
            Ok(json!({ "removed": root }))
        },
        IpcRequest::RenameProject { root, label } => {
            rename(state_path, root, label.clone())?;
            let renamed = discover_all(state_path)
                .into_iter()
                .find(|p| p.root == *root)
                .ok_or_else(|| not_a_project(root))?;
            Ok(project_json(&renamed))
        },
        // Nothing is cached without an app, so a refresh is just a fresh look —
        // but it still has to fail on a root the sidebar does not have, or it
        // would report on projects the user never added.
        IpcRequest::RefreshProject { root } => {
            let known = discover_all(state_path)
                .into_iter()
                .find(|p| p.root == *root)
                .ok_or_else(|| not_a_project(root))?;
            Ok(project_json(&known))
        },
        IpcRequest::GitStatus { path } => {
            Ok(ipc::git_status_json(&git_status::compute(path, None)))
        },
        IpcRequest::CreateWorktree { project_root, branch } => {
            create_worktree(project_root.clone(), branch.clone())
        },
        IpcRequest::ReadScratchpad { workspace } => match workspace.as_deref() {
            None | Some("current") => {
                Err("alacritree is not running; specify `home` or a workspace path".to_string())
            },
            Some("home") => scratchpad::read_json(&None),
            Some(path) => scratchpad::read_json(&Some(PathBuf::from(path))),
        },
        IpcRequest::ListSessions
        | IpcRequest::SelectWorkspace { .. }
        | IpcRequest::CreateSession { .. }
        | IpcRequest::CloseSession { .. }
        | IpcRequest::SendText { .. }
        | IpcRequest::ReadScreen { .. }
        | IpcRequest::MoveSession { .. }
        | IpcRequest::RunAction { .. } => Err("alacritree is not running".to_string()),
    }
}

fn add(state_path: &Path, path: &Path) -> Project {
    let root = path.to_path_buf();
    state::mutate_at(state_path, |s| {
        if !s.projects.iter().any(|p| p.root == root) {
            s.projects.push(PersistedProject { root, expanded: true, shell: None, label: None });
        }
    });
    Project::discover(path.to_path_buf()).project
}

fn remove(state_path: &Path, root: &Path) -> Result<(), String> {
    // `mutate_at` takes a closure that cannot fail, so the check has to happen
    // against the file we are about to mutate rather than inside the mutation.
    if !state::load_from(state_path).projects.iter().any(|p| p.root == root) {
        return Err(not_a_project(root));
    }
    let root = root.to_path_buf();
    state::mutate_at(state_path, move |s| s.projects.retain(|p| p.root != root));
    Ok(())
}

fn rename(state_path: &Path, root: &Path, label: Option<String>) -> Result<(), String> {
    // Same shape as `remove`: the existence check happens against the file
    // because the mutation closure cannot fail.
    if !state::load_from(state_path).projects.iter().any(|p| p.root == root) {
        return Err(not_a_project(root));
    }
    let label = projects::normalize_label(label);
    let root = root.to_path_buf();
    state::mutate_at(state_path, move |s| {
        if let Some(p) = s.projects.iter_mut().find(|p| p.root == root) {
            p.label = label;
        }
    });
    Ok(())
}

fn discover_all(state_path: &Path) -> Vec<Project> {
    let PersistedState { projects, .. } = state::load_from(state_path);
    projects
        .into_iter()
        .map(|p| {
            let mut project = Project::discover(p.root).project;
            project.expanded = p.expanded;
            project.label = p.label;
            project
        })
        .collect()
}

/// The app's create also asks the sidebar to re-scan afterwards; here there is
/// no sidebar to tell, and the next `project list` discovers the new worktree
/// from git anyway.
fn create_worktree(project_root: PathBuf, branch: String) -> IpcResult {
    wt::validate_branch_name(&branch)?;
    let request = CreateRequest { project_root, default_branch: None, branch, base_dir: None };
    let mut steps = Vec::new();
    let path = wt::create(&request, |step| steps.push(step.to_string()))?;
    Ok(json!({ "path": path, "steps": steps }))
}

fn not_a_project(root: &Path) -> String {
    format!("{} is not a project in the sidebar", root.display())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn state_file(dir: &TempDir) -> PathBuf {
        dir.path().join("state.toml")
    }

    fn roots(reply: &Value) -> Vec<String> {
        reply["projects"]
            .as_array()
            .expect("a project list")
            .iter()
            .map(|p| p["root"].as_str().expect("a root").to_string())
            .collect()
    }

    fn add_project(state_path: &Path, root: &Path) -> IpcResult {
        handle_at(state_path, &IpcRequest::AddProject { path: root.to_path_buf() })
    }

    fn list_projects(state_path: &Path) -> Value {
        handle_at(state_path, &IpcRequest::ListProjects).expect("list succeeds")
    }

    /// The point of the whole offline path: an agent can set alacritree up
    /// before anyone has opened it.
    #[test]
    fn a_project_added_with_no_app_running_is_listed() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);
        let project = dir.path().join("repo");
        std::fs::create_dir(&project).unwrap();

        add_project(&state, &project).expect("add succeeds");

        assert_eq!(roots(&list_projects(&state)), vec![project.display().to_string()]);
    }

    /// Matches the folder picker, which silently ignores a project already in
    /// the sidebar rather than adding a second copy of it.
    #[test]
    fn adding_a_project_twice_does_not_duplicate_it() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);
        let project = dir.path().join("repo");
        std::fs::create_dir(&project).unwrap();

        add_project(&state, &project).expect("first add");
        add_project(&state, &project).expect("second add");

        assert_eq!(roots(&list_projects(&state)).len(), 1);
    }

    #[test]
    fn removing_a_project_takes_it_off_the_list() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);
        let project = dir.path().join("repo");
        std::fs::create_dir(&project).unwrap();
        add_project(&state, &project).expect("add");

        handle_at(&state, &IpcRequest::RemoveProject { root: project.clone() }).expect("remove");

        assert!(roots(&list_projects(&state)).is_empty());
    }

    fn rename_project(state_path: &Path, root: &Path, label: Option<&str>) -> IpcResult {
        handle_at(
            state_path,
            &IpcRequest::RenameProject {
                root: root.to_path_buf(),
                label: label.map(str::to_string),
            },
        )
    }

    /// The label is display state in `state.toml`, so it must stick — and
    /// persist — without a window.
    #[test]
    fn renaming_a_project_changes_its_listed_name() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);
        let project = dir.path().join("repo");
        std::fs::create_dir(&project).unwrap();
        add_project(&state, &project).expect("add");

        let renamed = rename_project(&state, &project, Some("Work")).expect("rename");
        assert_eq!(renamed["name"], "Work");

        assert_eq!(list_projects(&state)["projects"][0]["name"], "Work");
    }

    /// No label means back to the directory name — the same request clears,
    /// so no second verb is needed anywhere on the surface.
    #[test]
    fn renaming_without_a_label_restores_the_directory_name() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);
        let project = dir.path().join("repo");
        std::fs::create_dir(&project).unwrap();
        add_project(&state, &project).expect("add");
        rename_project(&state, &project, Some("Work")).expect("rename");

        let cleared = rename_project(&state, &project, None).expect("clear");

        assert_eq!(cleared["name"], "repo");
        assert_eq!(cleared["label"], Value::Null);
    }

    /// Whitespace is not a name; a blank label behaves like no label at all.
    #[test]
    fn a_blank_label_falls_back_to_the_directory_name() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);
        let project = dir.path().join("repo");
        std::fs::create_dir(&project).unwrap();
        add_project(&state, &project).expect("add");

        let renamed = rename_project(&state, &project, Some("   ")).expect("rename");

        assert_eq!(renamed["name"], "repo");
        assert_eq!(renamed["label"], Value::Null);
    }

    #[test]
    fn renaming_an_unknown_project_is_an_error() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);

        let result = rename_project(&state, &PathBuf::from("/nowhere"), Some("Work"));

        assert!(result.is_err(), "renaming a project that was never added reported success");
    }

    /// A typo'd path must not report success; the caller has to learn the
    /// sidebar never had it.
    #[test]
    fn removing_an_unknown_project_is_an_error() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);

        let result =
            handle_at(&state, &IpcRequest::RemoveProject { root: PathBuf::from("/nowhere") });

        assert!(result.is_err(), "removing a project that was never added reported success");
    }

    /// The CLI is one writer among several, so it must never republish a
    /// project list it read earlier — the same rule the windows follow.
    #[test]
    fn adding_a_project_keeps_the_ones_already_on_disk() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);
        let theirs = dir.path().join("theirs");
        let ours = dir.path().join("ours");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::create_dir(&ours).unwrap();
        add_project(&state, &theirs).expect("their add");

        add_project(&state, &ours).expect("our add");

        assert_eq!(roots(&list_projects(&state)).len(), 2);
    }

    /// A session is a live PTY owned by a process that isn't there.  Reporting
    /// an empty session list would read as "no sessions are open", which is a
    /// different claim from "nothing can answer that".
    #[test]
    fn session_commands_say_alacritree_is_not_running() {
        let dir = TempDir::new().unwrap();
        let state = state_file(&dir);

        let result = handle_at(&state, &IpcRequest::ListSessions);

        assert_eq!(result, Err("alacritree is not running".to_string()));
    }
}
