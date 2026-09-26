//! Serving a request with no alacritree running.
//!
//! The sidebar is a view of `state.toml` plus what git says about each root, and
//! both outlive the window, so a request that only needs those two can be
//! answered without an app. Anything about sessions cannot: a session is a live
//! PTY owned by a process that isn't there.
//!
//! The replies are byte-for-byte the ones a running app would send, so nothing
//! downstream can tell which path answered it. That holds for rendering, for
//! `--json`, and for an agent parsing either.

use std::path::{Path, PathBuf};

use alacritree_common::jobs;
use alacritree_vcs::{Status, VcsError, VersionControl};
use serde_json::{Value, json};

use crate::ipc::protocol::{IpcRequest, IpcResult, status_json};
use crate::projects::{self, NotAProject, Project, project_json};
use crate::scratchpad;
use crate::state::{self, PersistedProject, PersistedState};
use crate::vcs::Vcs;
use crate::worktree::{self as wt, CreateConfig, CreateRequest};

pub(super) fn handle(request: &IpcRequest, config: &CreateConfig) -> IpcResult {
    let Some(path) = state::config_path() else {
        return Err("could not locate alacritree's state file".to_string());
    };
    handle_at(&path, request, config)
}

fn handle_at(state_path: &Path, request: &IpcRequest, config: &CreateConfig) -> IpcResult {
    match request {
        IpcRequest::ListProjects => Ok(json!({
            // No window means no focused workspace, the same value the app
            // reports for its home tab.
            "current_workspace": Value::Null,
            "projects": discover_all(state_path, &config.vcs)
                .iter()
                .map(project_json)
                .collect::<Vec<_>>(),
        })),
        IpcRequest::AddProject { path } => Ok(project_json(&add(state_path, path, &config.vcs))),
        IpcRequest::RemoveProject { root } => {
            remove(state_path, root).map_err(|e| e.to_string())?;
            Ok(json!({ "removed": root }))
        },
        IpcRequest::RenameProject { root, label } => {
            rename(state_path, root, label.clone()).map_err(|e| e.to_string())?;
            let renamed = discover_all(state_path, &config.vcs)
                .into_iter()
                .find(|p| p.root == *root)
                .ok_or_else(|| NotAProject(root.clone()).to_string())?;
            Ok(project_json(&renamed))
        },
        // Nothing is cached without an app, so a refresh is just a fresh look.
        // It still has to fail on a root the sidebar does not have, or it
        // would report on projects the user never added.
        IpcRequest::RefreshProject { root } => {
            let known = discover_all(state_path, &config.vcs)
                .into_iter()
                .find(|p| p.root == *root)
                .ok_or_else(|| NotAProject(root.clone()).to_string())?;
            Ok(project_json(&known))
        },
        IpcRequest::GitStatus { path } => match crate::vcs::for_path(&config.vcs, path) {
            Some(vcs) => {
                let result = jobs::on_this_thread(|blocking| vcs.status(path, None, blocking));
                Ok(match result {
                    Ok(status) => status_json(&status, None),
                    Err(e) => status_json(&Status::default(), Some(&e.to_string())),
                })
            },
            None => Ok(status_json(&Status::default(), Some("version control is disabled"))),
        },
        IpcRequest::CreateWorktree { project_root, branch } => {
            create_worktree(project_root.clone(), branch.clone(), config)
        },
        IpcRequest::ReadScratchpad { workspace } => match workspace.as_deref() {
            None | Some("current") => {
                Err("alacritree is not running; specify `home` or a workspace path".to_string())
            },
            Some("home") => scratchpad::read_json(&None).map_err(|e| e.to_string()),
            Some(path) => {
                scratchpad::read_json(&Some(PathBuf::from(path))).map_err(|e| e.to_string())
            },
        },
        IpcRequest::ListSessions
        | IpcRequest::SelectWorkspace { .. }
        | IpcRequest::CreateSession { .. }
        | IpcRequest::CloseSession { .. }
        | IpcRequest::SendText { .. }
        | IpcRequest::ReadScreen { .. }
        | IpcRequest::MoveSession { .. }
        | IpcRequest::ListMultiplexerPanes
        | IpcRequest::AttachMultiplexerPane { .. }
        | IpcRequest::CreateMultiplexerPane { .. }
        | IpcRequest::RunAction { .. } => Err("alacritree is not running".to_string()),
    }
}

fn add(state_path: &Path, path: &Path, backends: &[Vcs]) -> Project {
    let root = path.to_path_buf();
    state::mutate_at(state_path, |s| {
        if !s.projects.iter().any(|p| p.root == root) {
            s.projects.push(PersistedProject { root, expanded: true, shell: None, label: None });
        }
    });
    jobs::on_this_thread(|blocking| {
        Project::discover(path.to_path_buf(), backends, false, blocking)
    })
    .project
}

fn remove(state_path: &Path, root: &Path) -> Result<(), NotAProject> {
    // `mutate_at` takes a closure that cannot fail, so the check has to happen
    // against the file we are about to mutate rather than inside the mutation.
    if !state::load_from(state_path).projects.iter().any(|p| p.root == root) {
        return Err(NotAProject(root.to_path_buf()));
    }
    let root = root.to_path_buf();
    state::mutate_at(state_path, move |s| s.projects.retain(|p| p.root != root));
    Ok(())
}

fn rename(state_path: &Path, root: &Path, label: Option<String>) -> Result<(), NotAProject> {
    // Same shape as `remove`: the existence check happens against the file
    // because the mutation closure cannot fail.
    if !state::load_from(state_path).projects.iter().any(|p| p.root == root) {
        return Err(NotAProject(root.to_path_buf()));
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

fn discover_all(state_path: &Path, backends: &[Vcs]) -> Vec<Project> {
    let PersistedState { projects, .. } = state::load_from(state_path);
    projects
        .into_iter()
        .map(|p| {
            let mut project = jobs::on_this_thread(|blocking| {
                Project::discover(p.root, backends, false, blocking)
            })
            .project;
            project.expanded = p.expanded;
            project.label = p.label;
            project
        })
        .collect()
}

/// The app's create also asks the sidebar to re-scan afterwards; here there is
/// no sidebar to tell, and the next `project list` discovers the new worktree
/// from git anyway.
fn create_worktree(project_root: PathBuf, branch: String, config: &CreateConfig) -> IpcResult {
    let vcs = crate::vcs::for_path(&config.vcs, &project_root)
        .ok_or_else(|| VcsError::NotARepository(project_root.clone()).to_string())?;
    vcs.validate_name(&branch).map_err(|e| e.to_string())?;
    let request = CreateRequest::new(project_root, None, branch, &config.workspace, vcs);
    let mut steps = Vec::new();
    let path = jobs::on_this_thread(|blocking| {
        wt::create(&request, config.hooks.as_slice(), |step| steps.push(step.to_string()), blocking)
    })
    .map_err(|e| e.to_string())?;
    Ok(json!({ "path": path, "steps": steps }))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use alacritree_git::test_support::clone_with_origin;

    use crate::test_util::workspace_under;

    fn serve(state_path: &Path, request: &IpcRequest) -> IpcResult {
        handle_at(state_path, request, &CreateConfig::default())
    }

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
        serve(state_path, &IpcRequest::AddProject { path: root.to_path_buf() })
    }

    fn list_projects(state_path: &Path) -> Value {
        serve(state_path, &IpcRequest::ListProjects).expect("list succeeds")
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

        serve(&state, &IpcRequest::RemoveProject { root: project.clone() }).expect("remove");

        assert!(roots(&list_projects(&state)).is_empty());
    }

    fn rename_project(state_path: &Path, root: &Path, label: Option<&str>) -> IpcResult {
        serve(state_path, &IpcRequest::RenameProject {
            root: root.to_path_buf(),
            label: label.map(str::to_string),
        })
    }

    /// The label is display state in `state.toml`, so it must stick, and
    /// persist, without a window.
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

    /// No label means back to the directory name. The same request clears,
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

        let result = serve(&state, &IpcRequest::RemoveProject { root: PathBuf::from("/nowhere") });

        assert!(result.is_err(), "removing a project that was never added reported success");
    }

    /// The CLI is one writer among several, so it must never republish a
    /// project list it read earlier. The windows follow the same rule.
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

        let result = serve(&state, &IpcRequest::ListSessions);

        assert_eq!(result, Err("alacritree is not running".to_string()));
    }

    #[test]
    fn an_offline_status_with_git_disabled_says_version_control_is_off() {
        let dir = TempDir::new().unwrap();
        let repo = alacritree_git::test_support::init_repo(dir.path());
        let config = CreateConfig { vcs: Vec::new(), ..CreateConfig::default() };

        let reply = handle_at(&state_file(&dir), &IpcRequest::GitStatus { path: repo }, &config)
            .expect("a reply");

        assert_eq!(reply["error"], "version control is disabled");
    }

    /// The CLI with no window running puts a worktree where `[workspace]`
    /// says, the same place the sidebar's "+" does.
    #[test]
    fn an_offline_create_lands_under_the_configured_worktree_dir() {
        let dir = TempDir::new().unwrap();
        let project = clone_with_origin(dir.path());
        let base = dir.path().join("worktrees");
        let request = IpcRequest::CreateWorktree { project_root: project, branch: "topic".into() };

        let config = CreateConfig { workspace: workspace_under(&base), ..CreateConfig::default() };
        let reply = handle_at(&state_file(&dir), &request, &config).expect("create succeeds");

        let path = PathBuf::from(reply["path"].as_str().expect("a path"));
        assert!(path.starts_with(&base), "{} is not under {}", path.display(), base.display());
        assert!(path.is_dir());
    }
}
