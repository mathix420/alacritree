use alacritree_common::settings::{ClosedSet, IconStyle, RawIconStyle, moved_key};
use alacritree_common::tools::{Tool, tool_config};
use schemars::JsonSchema;
use serde::Deserialize;
use strum::{EnumIter, IntoStaticStr};

use crate::DEFAULT_ICON;

/// What opening a herdr agent row attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, EnumIter, IntoStaticStr)]
#[serde(into = "&'static str")]
#[strum(serialize_all = "snake_case")]
pub enum AttachMode {
    /// The agent's own pane.
    #[default]
    Agent,
    /// The herdr session that pane belongs to, with the pane focused.
    Session,
}

/// Whether a focus change made inside herdr may move alacritree, and from
/// which sessions.  Following moves the keyboard, so the default is the
/// narrower rule: only a session that is already showing herdr's view
/// follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, EnumIter, IntoStaticStr)]
#[serde(into = "&'static str")]
#[strum(serialize_all = "snake_case")]
pub enum FollowFocus {
    /// herdr never moves alacritree.  alacritree still tells herdr where to
    /// point when the user picks a row.
    Off,
    /// Follow only while the active session is herdr-backed.
    #[default]
    Herdr,
    /// Follow from a native session too, on any reachable side.
    Always,
}

/// `[integrations.herdr]`: whether alacritree lists agents running under a
/// herdr server in the sidebar, what opening one attaches to, and the glyph
/// that marks them. A probe with no herdr binary or server present costs
/// nothing.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct HerdrConfig {
    /// The native herdr binary, as `[integrations.herdr] path` names it.
    pub path: String,
    /// The herdr binary inside WSL, or `None` to find it by name there.
    pub wsl_path: Option<String>,
    /// The glyph on a herdr pane's sidebar row and palette entry.
    pub icon: IconStyle,
    /// Discover herdr servers and list their agents in the sidebar.
    pub enabled: bool,
    /// List panes whose working directory matches no worktree, under Home.
    pub show_unmatched: bool,
    /// List every pane a herdr server owns, not only the ones it detected an
    /// agent in.
    pub show_panes: bool,
    /// What a row opens.  Honoured per side; the native side of a Windows
    /// host attaches to the session whatever this says.
    pub attach: AttachMode,
    /// Whether a focus change inside herdr moves alacritree, and from which
    /// sessions.
    pub follow_focus: FollowFocus,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        RawHerdr::default().resolve(None)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawHerdr {
    /// The program to run on Windows or natively. Its own name is looked up
    /// on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty finds it
    /// by name through the distro's login shell.
    wsl_path: String,
    /// The glyph on a herdr pane's sidebar row and palette entry. A bare
    /// string sets the glyph; a table also styles its color, weight, slant
    /// and size, the way `[ui.icons]` keys do. The default draws a ram's head
    /// from the bundled symbol font. An ordinary character such as `"◫"` is
    /// drawn by your own fonts instead.
    #[schemars(default = "default_icon")]
    icon: Option<RawIconStyle>,
    /// Discover herdr servers and list their agents in the sidebar.  Inert
    /// when no herdr binary or server is present.
    ///
    /// Changes arrive on herdr's event stream, read through `herdr
    /// remote-api-bridge`.  0.9.1 has it and 0.8.2 does not; a herdr without
    /// it lists nothing.
    enabled: bool,
    /// List panes whose working directory matches no worktree, under Home.
    show_unmatched: bool,
    /// List every pane a herdr server owns, not only the ones it detected an
    /// agent in.  A pane running a plain shell gets a row named by its own
    /// title, carrying no status, and opening it shares herdr's view of the
    /// tab that holds it rather than attaching to the pane.
    ///
    /// Needs a herdr that knows `pane list` (0.8.2 does).  An older one
    /// answers with a usage error, which reads as no herdr on that side.  A
    /// side that has never answered is then abandoned for the process
    /// lifetime; one that answered before this was turned on keeps retrying
    /// and recovers when it goes back off.
    show_panes: bool,
    /// Whether opening a row attaches to that agent's pane directly
    /// ("agent") or to the herdr session around it with the pane focused
    /// ("session").
    ///
    /// "session" hands the mouse to herdr's own client, where a selection
    /// joins soft-wrapped rows and copy mode works; a direct attach is
    /// repainted row by row, so the host terminal sees every wrap as a line
    /// break.  Honoured per side: the native side of a Windows host always
    /// attaches to the session, because herdr implements no direct attach
    /// there.
    attach: ClosedSet<AttachMode>,
    /// Whether a focus change made inside herdr moves alacritree to the
    /// matching session.
    ///
    /// "off" never moves it.  "herdr" moves it only while the active session
    /// is already showing herdr's view, which is what an unmodified config
    /// has always done.  "always" also moves it from a plain native session,
    /// on any reachable side, after a gap in typing.
    follow_focus: ClosedSet<FollowFocus>,
}

impl Default for RawHerdr {
    fn default() -> Self {
        Self {
            path: Tool::Herdr.name().to_string(),
            wsl_path: String::new(),
            icon: None,
            enabled: true,
            show_unmatched: true,
            show_panes: false,
            attach: ClosedSet::default(),
            follow_focus: ClosedSet::default(),
        }
    }
}

fn default_icon() -> RawIconStyle {
    RawIconStyle::Glyph(DEFAULT_ICON.to_string())
}

impl RawHerdr {
    /// `old_icon` is the deprecated `[ui.icons] herdr`, which applies only
    /// where this table omits its own.
    pub fn resolve(self, old_icon: Option<RawIconStyle>) -> HerdrConfig {
        let tool = tool_config(self.path, self.wsl_path, Tool::Herdr);
        HerdrConfig {
            path: tool.path,
            wsl_path: tool.wsl_path,
            icon: moved_key(self.icon, old_icon, "[ui.icons] herdr", "[integrations.herdr] icon")
                .unwrap_or_else(default_icon)
                .into(),
            enabled: self.enabled,
            show_unmatched: self.show_unmatched,
            show_panes: self.show_panes,
            attach: self.attach.get(),
            follow_focus: self.follow_focus.get(),
        }
    }
}
