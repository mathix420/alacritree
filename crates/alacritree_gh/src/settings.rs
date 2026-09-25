use alacritree_common::settings::moved_key;
use alacritree_common::tools::{Tool, tool_config};
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RawGh {
    /// The program to run on Windows or natively. Its own name is looked up
    /// on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty finds it
    /// by name through the distro's login shell.
    wsl_path: String,
    /// Poll `gh` for each branch's open pull request, which drives the PR row
    /// icons, the PR-state filters, and `$pr` in row templates.
    #[schemars(default = "default_pr_status")]
    pr_status: Option<bool>,
    /// Max `gh` lookups in flight at once. Unset lets the pool decide, which
    /// is one below its own background ceiling so a lookup can never take
    /// the last slot local work needs. A value lowers that; nothing raises
    /// it, because the pool's ceiling binds underneath either way.
    pr_status_concurrency: Option<usize>,
}

impl Default for RawGh {
    fn default() -> Self {
        Self {
            path: Tool::Gh.name().to_string(),
            wsl_path: String::new(),
            pr_status: None,
            pr_status_concurrency: None,
        }
    }
}

fn default_pr_status() -> bool {
    false
}

/// The deprecated `[ui]` keys `[integrations.gh]` took over. Each applies
/// only where `[integrations.gh]` omits its own.
#[derive(Debug, Default)]
pub struct MovedGhKeys {
    pub pr_status: Option<bool>,
    pub pr_status_concurrency: Option<usize>,
}

/// `[integrations.gh]`: where the GitHub CLI lives and whether the sidebar
/// asks it about pull requests.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GhConfig {
    /// The tool's own name, or a native path that runs as written.
    pub path: String,
    /// A path that runs as written inside every WSL distro, or `None` to
    /// find the tool by name there.
    pub wsl_path: Option<String>,
    /// Paint PR-status badges on worktree rows and poll `gh` for expanded
    /// projects' worktrees. Best-effort like the diff-base lookup: no `gh`,
    /// no auth, or no PR paints nothing.
    pub pr_status: bool,
    /// Max `gh` lookups in flight at once. Unset lets the pool decide, which
    /// is one below its own background ceiling so a lookup can never take the
    /// last slot local work needs. A value lowers that; nothing raises it,
    /// because the pool's ceiling binds underneath either way.
    pub pr_status_concurrency: Option<usize>,
}

impl RawGh {
    pub fn resolve(self, moved: MovedGhKeys) -> GhConfig {
        let tool = tool_config(self.path, self.wsl_path, Tool::Gh);
        GhConfig {
            path: tool.path,
            wsl_path: tool.wsl_path,
            pr_status: moved_key(
                self.pr_status,
                moved.pr_status,
                "[ui] pr_status",
                "[integrations.gh] pr_status",
            )
            .unwrap_or_else(default_pr_status),
            pr_status_concurrency: moved_key(
                self.pr_status_concurrency,
                moved.pr_status_concurrency,
                "[ui] pr_status_concurrency",
                "[integrations.gh] pr_status_concurrency",
            ),
        }
    }
}
