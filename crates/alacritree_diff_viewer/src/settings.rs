use alacritree_common::settings::ClosedSet;
use alacritree_common::tools::{Tool, ToolConfig, tool_config};
use schemars::JsonSchema;
use serde::Deserialize;
use strum::{EnumIter, IntoStaticStr};

use crate::{Program, Templates, Viewer};

/// Which viewer `[integrations.diff_viewer]` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, EnumIter, IntoStaticStr)]
#[serde(into = "&'static str")]
#[strum(serialize_all = "snake_case")]
pub enum DiffViewerPreset {
    #[default]
    Delta,
    Tuicr,
    Custom,
}

/// `[integrations.diff_viewer]`: what the git panel's diff pane runs, and
/// whether its section headers offer a whole-section review.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DiffViewerConfig {
    pub viewer: Viewer,
    pub section_buttons: bool,
    pub button_icon: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawDelta {
    /// The program to run on Windows or natively. Its own name is
    /// looked up on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty
    /// finds it by name through the distro's login shell.
    wsl_path: String,
}

impl Default for RawDelta {
    fn default() -> Self {
        Self { path: Tool::Delta.name().to_string(), wsl_path: String::new() }
    }
}

impl RawDelta {
    /// `[ui] delta_path` was one path used on both sides. It still fills each
    /// side `[integrations.delta]` leaves at its default, because raw config
    /// structs accept unknown keys and dropping it would lose the override.
    pub fn resolve(self, deprecated_path: Option<String>) -> ToolConfig {
        let mut delta = tool_config(self.path, self.wsl_path, Tool::Delta);
        let Some(old) = deprecated_path.filter(|old| !old.trim().is_empty()) else {
            return delta;
        };
        let native_default = delta.path == Tool::Delta.name();
        let wsl_default = delta.wsl_path.is_none();
        if native_default || wsl_default {
            log::warn!("[ui] delta_path is deprecated; set [integrations.delta] path and wsl_path");
        }
        if native_default {
            delta.path = old.clone();
        }
        if wsl_default {
            delta.wsl_path = Some(old);
        }
        delta
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawTuicr {
    /// The program to run on Windows or natively. Its own name is
    /// looked up on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty
    /// finds it by name through the distro's login shell.
    wsl_path: String,
}

impl Default for RawTuicr {
    fn default() -> Self {
        Self { path: Tool::Tuicr.name().to_string(), wsl_path: String::new() }
    }
}

impl RawTuicr {
    pub fn resolve(self) -> ToolConfig {
        tool_config(self.path, self.wsl_path, Tool::Tuicr)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawDiffViewer {
    /// "delta" pipes git's diff through delta. "tuicr" opens tuicr's review
    /// TUI, which saves each comment for agents to read. "custom" runs
    /// `[integrations.diff_viewer.custom]`.
    preset: ClosedSet<DiffViewerPreset>,
    /// Draw a button on each git panel section header that opens the whole
    /// section in the viewer. The ReviewStaged, ReviewUnstaged and
    /// ReviewBranch actions work either way.
    section_buttons: bool,
    /// The glyph or word the section header button shows.
    button_icon: String,
    /// The viewer `preset = "custom"` runs.
    custom: RawCustomDiffViewer,
}

impl Default for RawDiffViewer {
    fn default() -> Self {
        Self {
            preset: ClosedSet::default(),
            section_buttons: false,
            button_icon: "review".to_string(),
            custom: RawCustomDiffViewer::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawCustomDiffViewer {
    /// Pager mode: a command git runs as `core.pager` for the panel's own
    /// `git diff`. Set this or `path`, never both.
    pager: String,
    /// Pager mode inside WSL: the command git runs as `core.pager` there.
    /// Empty runs `pager` through the distro's login shell.
    wsl_pager: String,
    /// Direct mode: a program that renders the diff itself, run with the
    /// argument list below that matches what was chosen. An empty list makes
    /// that row kind or section open nothing.
    path: String,
    /// Direct mode inside WSL: the program to run there, as written. Empty
    /// runs `path` through the distro's login shell, which finds a bare name.
    wsl_path: String,
    /// Arguments for a staged row. `{file}` is the row's path.
    staged: Vec<String>,
    /// Arguments for an unstaged row. `{file}` is the row's path.
    unstaged: Vec<String>,
    /// Arguments for an untracked row. `{file}` is the row's path.
    untracked: Vec<String>,
    /// Arguments for a `Changes vs` row. `{file}` is the row's path and
    /// `{base}` the branch it diffs against.
    branch: Vec<String>,
    /// Arguments for the Staged section header.
    staged_scope: Vec<String>,
    /// Arguments for the Unstaged section header.
    unstaged_scope: Vec<String>,
    /// Arguments for the `Changes vs` section header. `{base}` is the branch
    /// it diffs against.
    branch_scope: Vec<String>,
}

impl RawDiffViewer {
    pub fn resolve(self) -> DiffViewerConfig {
        let preset = self.preset.get();
        let viewer = match preset {
            DiffViewerPreset::Delta => Viewer::delta(),
            DiffViewerPreset::Tuicr => Viewer::tuicr(),
            DiffViewerPreset::Custom => self.custom.resolve().unwrap_or_else(|why| {
                log::warn!("[integrations.diff_viewer.custom] {why}; using the delta preset");
                Viewer::delta()
            }),
        };
        let button_icon = if self.button_icon.trim().is_empty() {
            RawDiffViewer::default().button_icon
        } else {
            self.button_icon
        };
        DiffViewerConfig { viewer, section_buttons: self.section_buttons, button_icon }
    }
}

impl RawCustomDiffViewer {
    fn resolve(self) -> Result<Viewer, &'static str> {
        let pager = self.pager.trim().to_string();
        let path = self.path.trim().to_string();
        let wsl = |value: String| Some(value.trim().to_string()).filter(|value| !value.is_empty());
        match (pager.is_empty(), path.is_empty()) {
            (false, true) => Ok(Viewer::Pager {
                pager: Program::Custom { path: pager, wsl_path: wsl(self.wsl_pager) },
                args: Vec::new(),
            }),
            (true, false) => Ok(Viewer::Direct {
                program: Program::Custom { path, wsl_path: wsl(self.wsl_path) },
                templates: Templates {
                    staged: self.staged,
                    unstaged: self.unstaged,
                    untracked: self.untracked,
                    branch: self.branch,
                    staged_scope: self.staged_scope,
                    unstaged_scope: self.unstaged_scope,
                    branch_scope: self.branch_scope,
                },
            }),
            (false, false) => Err("sets both pager and path"),
            (true, true) => Err("sets neither pager nor path"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff_viewer(toml: &str) -> DiffViewerConfig {
        toml::from_str::<RawDiffViewer>(toml).expect("valid TOML").resolve()
    }

    #[test]
    fn the_diff_viewer_defaults_to_delta_without_section_buttons() {
        let viewer = diff_viewer("");
        assert_eq!(viewer.viewer, Viewer::delta());
        assert!(!viewer.section_buttons);
        assert_eq!(viewer.button_icon, "review");
    }

    #[test]
    fn a_preset_names_its_viewer_and_an_unknown_one_is_delta() {
        let tuicr =
            diff_viewer("preset = \"tuicr\"\nsection_buttons = true\nbutton_icon = \"R\"\n");
        assert_eq!(tuicr.viewer, Viewer::tuicr());
        assert!(tuicr.section_buttons);
        assert_eq!(tuicr.button_icon, "R");

        assert_eq!(diff_viewer("preset = \"meld\"\n").viewer, Viewer::delta());
    }

    #[test]
    fn a_custom_viewer_needs_exactly_one_mode() {
        let pager = diff_viewer(
            "preset = \"custom\"\n[custom]\npager = \"delta --side-by-side\"\nwsl_pager = \
             \"/usr/bin/delta --side-by-side\"\n",
        );
        assert_eq!(pager.viewer, Viewer::Pager {
            pager: Program::Custom {
                path: "delta --side-by-side".to_string(),
                wsl_path: Some("/usr/bin/delta --side-by-side".to_string()),
            },
            args: Vec::new(),
        });

        let direct = diff_viewer(
            "preset = \"custom\"\n[custom]\npath = \"difft\"\nwsl_path = \"  \"\nstaged = \
             [\"--staged\", \"{file}\"]\n",
        );
        assert_eq!(direct.viewer, Viewer::Direct {
            program: Program::Custom { path: "difft".to_string(), wsl_path: None },
            templates: Templates {
                staged: vec!["--staged".to_string(), "{file}".to_string()],
                ..Templates::default()
            },
        });

        for both_or_neither in ["[custom]\npager = \"delta\"\npath = \"difft\"\n", "[custom]\n"] {
            let toml = format!("preset = \"custom\"\n{both_or_neither}");
            assert_eq!(diff_viewer(&toml).viewer, Viewer::delta());
        }
    }
}
