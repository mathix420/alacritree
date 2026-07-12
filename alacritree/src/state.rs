//! Persists the sidebar across restarts at `$XDG_CONFIG_HOME/alacritree/state.toml`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub projects: Vec<PersistedProject>,
    #[serde(default = "default_true")]
    pub show_left_sidebar: bool,
    #[serde(default = "default_true")]
    pub show_right_sidebar: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedProject {
    pub root: PathBuf,
    #[serde(default = "default_true")]
    pub expanded: bool,
    /// Shell override: `"windows"` or `"wsl:<distro>"`.  Absent = auto by
    /// project location.
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
    let Ok(contents) = std::fs::read_to_string(&path) else {
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

pub fn save(state: &PersistedState) {
    let Some(path) = config_path() else {
        return;
    };
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
    if let Err(e) = std::fs::write(&path, body) {
        log::warn!("failed to write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                shell: Some("wsl:arch".to_string()),
            }],
            ..Default::default()
        };
        let text = toml::to_string_pretty(&state).unwrap();
        let back: PersistedState = toml::from_str(&text).unwrap();
        assert_eq!(back.projects[0].shell.as_deref(), Some("wsl:arch"));
    }
}
