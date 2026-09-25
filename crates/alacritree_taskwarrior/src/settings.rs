use alacritree_common::tools::{Tool, tool_config};
use schemars::JsonSchema;
use serde::Deserialize;

/// `[integrations.taskwarrior]`: where `task` lives on each side, and
/// whether the tasks tab is on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TaskwarriorConfig {
    pub path: String,
    pub wsl_path: Option<String>,
    pub enabled: bool,
}

impl Default for TaskwarriorConfig {
    fn default() -> Self {
        RawTaskwarrior::default().resolve()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawTaskwarrior {
    /// The program to run on Windows or natively. Its own name is looked up
    /// on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty finds it
    /// by name through the distro's login shell.
    wsl_path: String,
    /// Show task lists kept in taskwarrior in a tab (`OpenTasks`, Ctrl+~).
    /// Agents write the same lists with `task` and read them through
    /// `alacritree hook`. Off leaves the binding inert and the palette entry
    /// out.
    enabled: bool,
}

impl Default for RawTaskwarrior {
    fn default() -> Self {
        Self { path: Tool::Task.name().to_string(), wsl_path: String::new(), enabled: false }
    }
}

impl RawTaskwarrior {
    pub fn resolve(self) -> TaskwarriorConfig {
        let tool = tool_config(self.path, self.wsl_path, Tool::Task);
        TaskwarriorConfig { path: tool.path, wsl_path: tool.wsl_path, enabled: self.enabled }
    }
}
