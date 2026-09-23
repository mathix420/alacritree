use alacritree_common::tools::{Tool, tool_config};
use serde::Deserialize;

/// `[integrations.doppler]`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RawDoppler {
    /// The program to run on Windows or natively. Its own name is looked up
    /// on PATH; any other value runs as written.
    path: String,
    /// The program to run inside every WSL distro, as written. Empty finds it
    /// by name through the distro's login shell.
    wsl_path: String,
    /// Copy the main checkout's Doppler scopes into each new worktree, and
    /// drop them again when the worktree is removed.
    enabled: bool,
}

impl Default for RawDoppler {
    fn default() -> Self {
        Self { path: Tool::Doppler.name().to_string(), wsl_path: String::new(), enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DopplerConfig {
    pub path: String,
    pub wsl_path: Option<String>,
    pub enabled: bool,
}

impl RawDoppler {
    pub fn resolve(self) -> DopplerConfig {
        let tool = tool_config(self.path, self.wsl_path, Tool::Doppler);
        DopplerConfig { path: tool.path, wsl_path: tool.wsl_path, enabled: self.enabled }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doppler_is_enabled_and_found_by_name_by_default() {
        assert_eq!(RawDoppler::default().resolve(), DopplerConfig {
            path: "doppler".into(),
            wsl_path: None,
            enabled: true,
        });
    }

    #[test]
    fn a_table_can_turn_doppler_off() {
        let raw: RawDoppler = toml::from_str("enabled = false").unwrap();
        assert!(!raw.resolve().enabled);
    }
}
