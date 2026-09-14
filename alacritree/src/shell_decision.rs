//! The precedence chain behind a new session's shell.
//!
//! The GUI spawn path and the headless `doctor` both consult it, which is why
//! it holds no `Shell` or PTY types: the code that turns a verdict into a
//! process stays in `app`.

use crate::config::Profile;
use crate::wsl::ShellChoice;

/// What shell a new session should run, decided from plain data so the
/// precedence chain stays testable off the GUI.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShellDecision {
    /// Fall through to `[terminal.shell]` / the OS default.
    ConfigShell,
    /// A shell inside this WSL distro (`wsl_shell` builds the argv).
    WslDistro(String),
    /// A named `[[ui.profiles]]` entry, verified to exist.
    Profile(String),
}

/// Precedence: project override, then WSL location, then the default
/// profile, then the config shell.  A stale override (distro unregistered,
/// profile removed from config) warns and continues down the chain rather
/// than failing the spawn.
pub(crate) fn shell_decision(
    override_choice: Option<&ShellChoice>,
    location_distro: Option<&str>,
    known_distros: &[String],
    profiles: &[Profile],
    default_profile: Option<&str>,
) -> ShellDecision {
    match override_choice {
        Some(ShellChoice::Windows) => return ShellDecision::ConfigShell,
        Some(ShellChoice::Wsl(d)) => {
            if known_distros.iter().any(|k| k == d) {
                return ShellDecision::WslDistro(d.clone());
            }
            log::warn!("shell override names unknown WSL distro `{d}`; using auto");
        },
        Some(ShellChoice::Profile(n)) => {
            if profiles.iter().any(|p| &p.name == n) {
                return ShellDecision::Profile(n.clone());
            }
            log::warn!("shell override names unknown profile `{n}`; using auto");
        },
        None => {},
    }
    if let Some(d) = location_distro {
        return ShellDecision::WslDistro(d.to_string());
    }
    if let Some(n) = default_profile {
        return ShellDecision::Profile(n.to_string());
    }
    ShellDecision::ConfigShell
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profiles() -> Vec<crate::config::Profile> {
        vec![
            crate::config::Profile {
                name: "pwsh".into(),
                program: "pwsh".into(),
                args: vec!["-NoLogo".into()],
            },
            crate::config::Profile {
                name: "ubuntu".into(),
                program: "wsl.exe".into(),
                args: vec!["-d".into(), "ubuntu".into()],
            },
        ]
    }

    #[test]
    fn override_profile_wins_over_location_and_default() {
        let d = shell_decision(
            Some(&ShellChoice::Profile("pwsh".into())),
            Some("ubuntu"),
            &["ubuntu".into()],
            &test_profiles(),
            Some("ubuntu"),
        );
        assert_eq!(d, ShellDecision::Profile("pwsh".into()));
    }

    #[test]
    fn override_windows_skips_default_profile() {
        let d = shell_decision(
            Some(&ShellChoice::Windows),
            Some("ubuntu"),
            &["ubuntu".into()],
            &test_profiles(),
            Some("pwsh"),
        );
        assert_eq!(d, ShellDecision::ConfigShell);
    }

    #[test]
    fn stale_profile_override_falls_back_to_auto() {
        // Unknown profile behaves like the unknown-distro case: warn, then
        // continue down the auto chain (location, then default profile).
        let d = shell_decision(
            Some(&ShellChoice::Profile("gone".into())),
            Some("ubuntu"),
            &["ubuntu".into()],
            &test_profiles(),
            None,
        );
        assert_eq!(d, ShellDecision::WslDistro("ubuntu".into()));

        let d = shell_decision(
            Some(&ShellChoice::Profile("gone".into())),
            None,
            &[],
            &test_profiles(),
            Some("pwsh"),
        );
        assert_eq!(d, ShellDecision::Profile("pwsh".into()));
    }

    #[test]
    fn wsl_location_beats_default_profile() {
        let d = shell_decision(
            None,
            Some("ubuntu"),
            &["ubuntu".into()],
            &test_profiles(),
            Some("pwsh"),
        );
        assert_eq!(d, ShellDecision::WslDistro("ubuntu".into()));
    }

    #[test]
    fn default_profile_applies_without_override_or_location() {
        // This is also the home-tab case: no project, no WSL location.
        let d = shell_decision(None, None, &[], &test_profiles(), Some("pwsh"));
        assert_eq!(d, ShellDecision::Profile("pwsh".into()));
    }

    #[test]
    fn no_config_means_config_shell() {
        let d = shell_decision(None, None, &[], &[], None);
        assert_eq!(d, ShellDecision::ConfigShell);
    }

    #[test]
    fn stale_wsl_override_falls_through_to_default_profile() {
        let d = shell_decision(
            Some(&ShellChoice::Wsl("gone".into())),
            None,
            &["ubuntu".into()],
            &test_profiles(),
            Some("pwsh"),
        );
        assert_eq!(d, ShellDecision::Profile("pwsh".into()));
    }
}
