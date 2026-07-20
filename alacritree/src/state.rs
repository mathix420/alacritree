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
    /// Per-worktree override of the branch the git panel diffs against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_branches: Vec<PersistedBaseBranch>,
}

/// The `default_true` attributes above only speak for a file that omits the
/// field.  A first run has no file at all and lands here instead, so deriving
/// this would open alacritree with both sidebars hidden.
impl Default for PersistedState {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            show_left_sidebar: true,
            show_right_sidebar: true,
            base_branches: Vec::new(),
        }
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
    /// Display label shown instead of the directory name.  Absent = derive
    /// the name from the root, as before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedBaseBranch {
    pub worktree: PathBuf,
    pub branch: String,
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

/// Reorder `state.projects` to follow `order` (a list of roots).  Roots absent
/// from `order` — a project another window added that this one never loaded —
/// keep their existing relative order at the end, so a reorder here never drops
/// them.  Stable, so equal keys (all the disk-only roots) hold their order.
pub fn reorder_projects(state: &mut PersistedState, order: &[PathBuf]) {
    state.projects.sort_by_key(|p| order.iter().position(|r| *r == p.root).unwrap_or(usize::MAX));
}

/// Set or clear one worktree's base-branch override.  Entries are pruned in
/// the same pass, but only when the filesystem definitively says the
/// worktree is gone — the filesystem is the truth every window shares, so
/// pruning here can't delete another window's live entry the way pruning
/// against one window's project list could.  A metadata error that isn't
/// "not found" (permission denied, an unreachable network or WSL mount with
/// its distro asleep) is not proof the worktree is gone, so those entries
/// are kept rather than silently dropped.
pub fn set_base_branch(state: &mut PersistedState, worktree: &Path, branch: Option<String>) {
    state.base_branches.retain(|b| b.worktree != worktree && !definitely_gone(&b.worktree));
    if let Some(branch) = branch {
        state.base_branches.push(PersistedBaseBranch { worktree: worktree.to_path_buf(), branch });
    }
}

/// True only when the filesystem gives a conclusive answer that `path` is
/// not a live worktree directory.  Any other metadata error (permission,
/// an unreachable mount) is inconclusive, not a "gone" verdict.
fn definitely_gone(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => !m.is_dir(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
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
        PersistedProject { root: PathBuf::from(root), expanded: true, shell: None, label: None }
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
    fn reorder_follows_the_given_root_order() {
        let mut state = PersistedState {
            projects: vec![project("/a"), project("/b"), project("/c")],
            ..Default::default()
        };
        let order = vec![PathBuf::from("/c"), PathBuf::from("/a"), PathBuf::from("/b")];
        reorder_projects(&mut state, &order);
        assert_eq!(
            roots(&state),
            vec![PathBuf::from("/c"), PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    /// A project another window added but this one never loaded is not in the
    /// reorder's `order` list; it must survive at the end rather than vanish.
    #[test]
    fn reorder_keeps_disk_only_projects_at_the_end() {
        let mut state = PersistedState {
            projects: vec![project("/theirs1"), project("/a"), project("/theirs2"), project("/b")],
            ..Default::default()
        };
        // This window only knows /b and /a, in that order.
        let order = vec![PathBuf::from("/b"), PathBuf::from("/a")];
        reorder_projects(&mut state, &order);
        assert_eq!(
            roots(&state),
            vec![
                PathBuf::from("/b"),
                PathBuf::from("/a"),
                PathBuf::from("/theirs1"),
                PathBuf::from("/theirs2"),
            ],
            "known projects lead in the new order; disk-only ones trail in their old order",
        );
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
                label: None,
            }],
            ..Default::default()
        };
        let text = toml::to_string_pretty(&state).unwrap();
        let back: PersistedState = toml::from_str(&text).unwrap();
        assert_eq!(back.projects[0].shell.as_deref(), Some("wsl:kali-linux"));
    }

    #[test]
    fn label_field_is_optional_and_round_trips() {
        // Old state files (no `label`) still parse.
        let old = "[[projects]]\nroot = 'C:/x'\n";
        let state: PersistedState = toml::from_str(old).unwrap();
        assert_eq!(state.projects[0].label, None);

        let state = PersistedState {
            projects: vec![PersistedProject {
                root: PathBuf::from("C:/x"),
                expanded: true,
                shell: None,
                label: Some("Work".to_string()),
            }],
            ..Default::default()
        };
        let text = toml::to_string_pretty(&state).unwrap();
        let back: PersistedState = toml::from_str(&text).unwrap();
        assert_eq!(back.projects[0].label.as_deref(), Some("Work"));
    }

    #[test]
    fn base_branch_field_is_optional_and_round_trips() {
        // Old state files (no `base_branches`) still parse.
        let old = "[[projects]]\nroot = 'C:/x'\n";
        let state: PersistedState = toml::from_str(old).unwrap();
        assert!(state.base_branches.is_empty());

        let state = PersistedState {
            base_branches: vec![PersistedBaseBranch {
                worktree: PathBuf::from("C:/repo/wt"),
                branch: "develop".to_string(),
            }],
            ..Default::default()
        };
        let text = toml::to_string_pretty(&state).unwrap();
        let back: PersistedState = toml::from_str(&text).unwrap();
        assert_eq!(back.base_branches[0].branch, "develop");
        assert_eq!(back.base_branches[0].worktree, PathBuf::from("C:/repo/wt"));
    }

    #[test]
    fn set_base_branch_replaces_and_clears() {
        let dir = TempDir::new().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let mut state = PersistedState::default();

        set_base_branch(&mut state, &wt, Some("develop".to_string()));
        assert_eq!(state.base_branches.len(), 1);
        set_base_branch(&mut state, &wt, Some("release".to_string()));
        assert_eq!(state.base_branches.len(), 1, "a second set must replace, not append");
        assert_eq!(state.base_branches[0].branch, "release");
        set_base_branch(&mut state, &wt, None);
        assert!(state.base_branches.is_empty(), "None clears the override");
    }

    /// Worktrees deleted on disk take their overrides with them the next time
    /// any override is written — the filesystem is the truth every window shares.
    #[test]
    fn set_base_branch_prunes_entries_for_vanished_worktrees() {
        let dir = TempDir::new().unwrap();
        let alive = dir.path().join("alive");
        std::fs::create_dir(&alive).unwrap();
        let mut state = PersistedState {
            base_branches: vec![PersistedBaseBranch {
                worktree: dir.path().join("gone"),
                branch: "develop".to_string(),
            }],
            ..Default::default()
        };

        set_base_branch(&mut state, &alive, Some("main".to_string()));

        let worktrees: Vec<_> = state.base_branches.iter().map(|b| b.worktree.clone()).collect();
        assert_eq!(worktrees, vec![alive]);
    }

    /// A path that exists but isn't a directory (e.g. clobbered by a file) is
    /// just as definitively gone as a missing path.
    #[test]
    fn set_base_branch_prunes_entries_whose_path_is_a_file_not_a_dir() {
        let dir = TempDir::new().unwrap();
        let alive = dir.path().join("alive");
        std::fs::create_dir(&alive).unwrap();
        let file_path = dir.path().join("not_a_dir");
        std::fs::write(&file_path, b"").unwrap();
        let mut state = PersistedState {
            base_branches: vec![PersistedBaseBranch {
                worktree: file_path,
                branch: "develop".to_string(),
            }],
            ..Default::default()
        };

        set_base_branch(&mut state, &alive, Some("main".to_string()));

        let worktrees: Vec<_> = state.base_branches.iter().map(|b| b.worktree.clone()).collect();
        assert_eq!(worktrees, vec![alive]);
    }
}
