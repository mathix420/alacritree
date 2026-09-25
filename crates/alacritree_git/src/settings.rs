use alacritree_common::tools::{Tool, tool_config};
use schemars::JsonSchema;
use serde::Deserialize;

/// `[integrations.git]`: where git lives on each side, whether git
/// repositories are recognized at all, and whether their rows say so.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GitConfig {
    pub path: String,
    pub wsl_path: Option<String>,
    pub enabled: bool,
    pub show_icon: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        RawGit::default().resolve()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawGit {
    /// The program to run on Windows or natively. Its own name is
    /// looked up on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty
    /// finds it by name through the distro's login shell.
    wsl_path: String,
    /// Recognize git repositories. Off, every git project is a plain folder:
    /// no worktree rows, no git panel and no pull request badges.
    enabled: bool,
    /// Draw git's icon on the project rows of git repositories.
    show_icon: bool,
}

impl Default for RawGit {
    fn default() -> Self {
        Self {
            path: Tool::Git.name().to_string(),
            wsl_path: String::new(),
            enabled: true,
            show_icon: false,
        }
    }
}

impl RawGit {
    pub fn resolve(self) -> GitConfig {
        let tool = tool_config(self.path, self.wsl_path, Tool::Git);
        GitConfig {
            path: tool.path,
            wsl_path: tool.wsl_path,
            enabled: self.enabled,
            show_icon: self.show_icon,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_is_on_and_draws_no_icon_by_default() {
        let config = RawGit::default().resolve();
        assert!(config.enabled);
        assert!(!config.show_icon);
        assert_eq!(config.path, "git");
        assert_eq!(config.wsl_path, None);
    }
}
