//! Persists the sidebar across restarts at `$XDG_CONFIG_HOME/alacritree/state.toml`.
//!
//! The file is shared by every running alacritree, so it is never written from a
//! snapshot: [`mutate`] re-reads it, applies one change, and writes it back.  A
//! window that dumped its whole in-memory state would republish a project list
//! it read at startup, deleting whatever another window has added since.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub projects: Vec<PersistedProject>,
    #[serde(default = "default_true")]
    pub show_left_sidebar: bool,
    #[serde(default = "default_true")]
    pub show_right_sidebar: bool,
}

/// The `default_true` attributes above only speak for a file that omits the
/// field.  A first run has no file at all and lands here instead, so deriving
/// this would open alacritree with both sidebars hidden.
impl Default for PersistedState {
    fn default() -> Self {
        Self { projects: Vec::new(), show_left_sidebar: true, show_right_sidebar: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedProject {
    pub root: PathBuf,
    #[serde(default = "default_true")]
    pub expanded: bool,
    /// Shell override: `"windows"`, `"wsl:<distro>"`, or `"profile:<name>"`.
    /// Absent = auto by project location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

fn default_true() -> bool {
    true
}

pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("alacritree").join("state.toml"))
}

/// Per-user config base: XDG on Unix, the roaming app-data dir on Windows
/// (which has neither `$XDG_CONFIG_HOME` nor `$HOME`).
#[cfg(not(windows))]
fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config"))
}

#[cfg(windows)]
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .or_else(home::home_dir)
}

pub fn load() -> PersistedState {
    let Some(path) = config_path() else {
        return PersistedState::default();
    };
    load_from(&path)
}

/// Apply one change to the state on disk.
///
/// The state is re-read first, so a window only overwrites the fields it
/// actually touched: hiding a sidebar must not republish the project list this
/// window happened to read at startup.
pub fn mutate(change: impl FnOnce(&mut PersistedState)) {
    let Some(path) = config_path() else {
        return;
    };
    mutate_at(&path, change);
}

pub fn load_from(path: &Path) -> PersistedState {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return PersistedState::default();
    };
    match toml::from_str(&contents) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("failed to parse {}: {e}", path.display());
            PersistedState::default()
        },
    }
}

/// Why [`load_from`] would fall back to an empty state, if it would.
///
/// A corrupt file is never allowed to stop alacritree from starting, so
/// `load_from` logs it and hands back the default — which makes a lost project
/// list indistinguishable from a fresh install.  A file that isn't there yet is
/// a fresh install, and reports no error.
pub fn parse_error(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    toml::from_str::<PersistedState>(&contents).err().map(|e| e.to_string())
}

pub fn mutate_at(path: &Path, change: impl FnOnce(&mut PersistedState)) {
    let mut state = load_from(path);
    change(&mut state);
    save_to(path, &state);
}

pub fn save_to(path: &Path, state: &PersistedState) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("failed to create {}: {e}", parent.display());
            return;
        }
    }
    let body = match toml::to_string_pretty(state) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("failed to serialize state: {e}");
            return;
        },
    };

    // Another window may be reading the file right now, and `fs::write`
    // truncates before it writes.  Renaming a fully-written sibling into place
    // is atomic, so a reader sees either the old file or the new one.
    let tmp = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp, body) {
        log::warn!("failed to write {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        log::warn!("failed to replace {}: {e}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn project(root: &str) -> PersistedProject {
        PersistedProject { root: PathBuf::from(root), expanded: true, shell: None }
    }

    fn state_file(dir: &TempDir) -> PathBuf {
        dir.path().join("state.toml")
    }

    fn roots(state: &PersistedState) -> Vec<PathBuf> {
        state.projects.iter().map(|p| p.root.clone()).collect()
    }

    /// Two windows share one state file.  The window that hides a sidebar took
    /// its copy of the project list at startup, so writing that copy back would
    /// delete every project the other window has added since — with Ctrl+B, an
    /// action that has nothing to do with projects.
    #[test]
    fn hiding_a_sidebar_keeps_a_project_another_window_added() {
        let dir = TempDir::new().unwrap();
        let path = state_file(&dir);

        // The other window adds a project while this one is running.
        save_to(
            &path,
            &PersistedState { projects: vec![project("/repo/theirs")], ..Default::default() },
        );

        // This window, whose startup snapshot predates that project, hides a
        // sidebar.
        mutate_at(&path, |s| s.show_left_sidebar = false);

        let state = load_from(&path);
        assert_eq!(
            roots(&state),
            vec![PathBuf::from("/repo/theirs")],
            "hiding a sidebar deleted a project another window had added",
        );
        assert!(!state.show_left_sidebar, "the sidebar toggle was not persisted");
    }

    /// The mirror image: a project this window adds must survive whatever the
    /// other window's stale snapshot contains.
    #[test]
    fn adding_a_project_keeps_the_ones_already_on_disk() {
        let dir = TempDir::new().unwrap();
        let path = state_file(&dir);
        save_to(
            &path,
            &PersistedState { projects: vec![project("/repo/theirs")], ..Default::default() },
        );

        mutate_at(&path, |s| s.projects.push(project("/repo/ours")));

        assert_eq!(
            roots(&load_from(&path)),
            vec![PathBuf::from("/repo/theirs"), PathBuf::from("/repo/ours"),]
        );
    }

    /// Re-reading the file must not resurrect a project the user deleted — the
    /// case a "merge the union of both lists" fix would get wrong.
    #[test]
    fn removing_a_project_deletes_it() {
        let dir = TempDir::new().unwrap();
        let path = state_file(&dir);
        save_to(
            &path,
            &PersistedState {
                projects: vec![project("/repo/keep"), project("/repo/drop")],
                ..Default::default()
            },
        );

        mutate_at(&path, |s| s.projects.retain(|p| p.root != PathBuf::from("/repo/drop")));

        assert_eq!(roots(&load_from(&path)), vec![PathBuf::from("/repo/keep")]);
    }

    #[test]
    fn mutating_a_missing_file_creates_it() {
        let dir = TempDir::new().unwrap();
        let path = state_file(&dir);

        mutate_at(&path, |s| s.projects.push(project("/repo/first")));

        assert_eq!(roots(&load_from(&path)), vec![PathBuf::from("/repo/first")]);
    }

    /// A first run has no state file at all, and must not come up with both
    /// sidebars hidden.  The `default_true` attributes only speak for a file
    /// that omits the field; an absent file goes through `Default`.
    #[test]
    fn a_first_run_shows_both_sidebars() {
        let dir = TempDir::new().unwrap();

        let state = load_from(&state_file(&dir));

        assert!(state.show_left_sidebar, "the left sidebar is hidden on a first run");
        assert!(state.show_right_sidebar, "the right sidebar is hidden on a first run");
    }

    /// The temporary file `save_to` renames into place is not left behind.
    #[test]
    fn saving_leaves_no_temporary_file() {
        let dir = TempDir::new().unwrap();
        let path = state_file(&dir);

        save_to(&path, &PersistedState::default());

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n != "state.toml")
            .collect();
        assert!(leftovers.is_empty(), "save left {leftovers:?} behind");
    }

    #[test]
    fn shell_field_is_optional_and_round_trips() {
        // Old state files (no `shell`) still parse.
        let old = "[[projects]]\nroot = 'C:/x'\n";
        let state: PersistedState = toml::from_str(old).unwrap();
        assert_eq!(state.projects[0].shell, None);

        let state = PersistedState {
            projects: vec![PersistedProject {
                root: PathBuf::from("C:/x"),
                expanded: true,
                shell: Some("wsl:kali-linux".to_string()),
            }],
            ..Default::default()
        };
        let text = toml::to_string_pretty(&state).unwrap();
        let back: PersistedState = toml::from_str(&text).unwrap();
        assert_eq!(back.projects[0].shell.as_deref(), Some("wsl:kali-linux"));
    }
}
