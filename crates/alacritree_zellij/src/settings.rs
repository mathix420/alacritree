use std::time::Duration;

use alacritree_common::settings::{IconStyle, RawIconStyle};
use alacritree_common::tools::{Tool, tool_config};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::DEFAULT_ICON;

/// `[integrations.zellij]`: whether alacritree lists the panes of running
/// zellij sessions in the sidebar, where a new one opens, and the glyph that
/// marks them.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ZellijConfig {
    /// The native zellij binary.
    pub path: String,
    /// The zellij binary inside WSL, or `None` to find it by name there.
    pub wsl_path: Option<String>,
    /// The glyph on a zellij pane's sidebar row and palette entry.
    pub icon: IconStyle,
    /// Discover zellij sessions and list their panes in the sidebar.
    pub enabled: bool,
    /// How often each side's zellij sessions are re-listed.
    pub poll_interval: Duration,
    /// List panes whose working directory matches no worktree, under Home.
    pub show_unmatched: bool,
    /// The session a new pane opens in.  `None` takes the one session
    /// running on the side, and refuses when there are several.
    pub session: Option<String>,
}

impl Default for ZellijConfig {
    fn default() -> Self {
        RawZellij::default().resolve()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawZellij {
    /// The program to run on Windows or natively. Its own name is looked up
    /// on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty finds it
    /// by name through the distro's login shell.
    wsl_path: String,
    /// The glyph on a zellij pane's sidebar row and palette entry. A bare
    /// string sets the glyph; a table also styles its color, weight, slant
    /// and size, the way `[ui.icons]` keys do. The default draws a hexagon
    /// from the bundled symbol font. An ordinary character such as `"⬡"` is
    /// drawn by your own fonts instead.
    #[schemars(default = "default_icon")]
    icon: Option<RawIconStyle>,
    /// List the panes of every running zellij session in the sidebar.
    /// Opening one attaches to its whole session with the pane focused, since
    /// zellij has no attach for a single pane.
    enabled: bool,
    /// How often each side's zellij sessions are re-listed.
    poll_interval_ms: u64,
    /// List panes whose working directory matches no worktree, under Home.
    show_unmatched: bool,
    /// The session a new pane opens in. Empty takes the one session running
    /// on that side and refuses when there are several.
    session: String,
}

impl Default for RawZellij {
    fn default() -> Self {
        Self {
            path: Tool::Zellij.name().to_string(),
            wsl_path: String::new(),
            icon: None,
            enabled: false,
            poll_interval_ms: 2000,
            show_unmatched: true,
            session: String::new(),
        }
    }
}

fn default_icon() -> RawIconStyle {
    RawIconStyle::Glyph(DEFAULT_ICON.to_string())
}

impl RawZellij {
    pub fn resolve(self) -> ZellijConfig {
        let tool = tool_config(self.path, self.wsl_path, Tool::Zellij);
        ZellijConfig {
            path: tool.path,
            wsl_path: tool.wsl_path,
            icon: self.icon.unwrap_or_else(default_icon).into(),
            enabled: self.enabled,
            poll_interval: Duration::from_millis(self.poll_interval_ms),
            show_unmatched: self.show_unmatched,
            session: Some(self.session).filter(|session| !session.trim().is_empty()),
        }
    }
}
