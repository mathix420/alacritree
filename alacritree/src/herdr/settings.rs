//! Reading herdr's own configuration, so alacritree can name the chord that
//! detaches a session and draw the indicator set herdr draws.

use std::path::PathBuf;
use std::process::Stdio;

use crate::{command_ext, jobs, wsl};
use serde::Deserialize;

use super::{Indicators, Settings, Side};

const DEFAULT_PREFIX: &str = "ctrl+b";
const DEFAULT_DETACH: &str = "prefix+q";

#[derive(Deserialize, Default)]
struct RawHerdrConfig {
    #[serde(default)]
    keys: RawKeys,
    #[serde(default)]
    ui: RawUi,
}

#[derive(Deserialize, Default)]
struct RawUi {
    status_indicators: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawKeys {
    prefix: Option<String>,
    detach: Option<RawBinding>,
}

/// herdr binds an action to one key or to several.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawBinding {
    One(String),
    Many(Vec<String>),
}

impl RawBinding {
    /// The binding a hint names.  herdr's own help surface leads with the
    /// first, and a row has space for one.  An empty string is herdr's
    /// spelling for unbound.
    fn first(&self) -> Option<&str> {
        match self {
            Self::One(value) => Some(value.as_str()),
            Self::Many(values) => values.first().map(String::as_str),
        }
        .map(str::trim)
        .filter(|value| !value.is_empty())
    }
}

/// Render one `+`-joined combo the way alacritree spells chords elsewhere:
/// `ctrl+b` reads as `Ctrl+B`.
fn render_combo(raw: &str) -> String {
    raw.split('+').map(render_key).collect::<Vec<_>>().join("+")
}

fn render_key(key: &str) -> String {
    let mut chars = key.chars();
    let Some(first) = chars.next() else { return String::new() };
    if key.len() == 1 {
        return first.to_ascii_uppercase().to_string();
    }
    first.to_uppercase().chain(chars).collect()
}

/// The half of a prefix binding that follows the prefix.  A bare key keeps
/// herdr's own lowercase spelling, so the hint matches herdr's documentation
/// (`ctrl+b q`); a modified one is a combo and reads like one.
fn render_prefixed(rest: &str) -> String {
    if rest.contains('+') { render_combo(rest) } else { rest.to_string() }
}

/// The detach chord `config` binds, spelled as herdr documents it.
///
/// A config herdr itself would reject falls back to the defaults herdr would
/// then run with, so a typo anywhere in the file does not silence the hint.
/// `None` means detach is bound to nothing — the one case where there is no
/// chord to advertise.
fn detach_chord_from(config: &str) -> Option<String> {
    let parsed: RawHerdrConfig = toml::from_str(config).unwrap_or_default();
    let prefix = parsed
        .keys
        .prefix
        .as_deref()
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or(DEFAULT_PREFIX);
    let detach = match &parsed.keys.detach {
        Some(binding) => binding.first()?,
        None => DEFAULT_DETACH,
    };
    Some(match detach.strip_prefix("prefix+") {
        Some(rest) => format!("{} {}", render_combo(prefix), render_prefixed(rest)),
        None => render_combo(detach),
    })
}

fn indicators_from(config: &str) -> Indicators {
    let parsed: RawHerdrConfig = toml::from_str(config).unwrap_or_default();
    match parsed.ui.status_indicators.as_deref() {
        Some("symbols") => Indicators::Symbols,
        _ => Indicators::Dots,
    }
}

fn settings_from(config: &str) -> Settings {
    Settings { detach: detach_chord_from(config), indicators: indicators_from(config) }
}

/// Where herdr looks for its config, mirroring its own resolution so both
/// programs read the same file.  `HERDR_CONFIG_PATH` wins everywhere, then
/// `XDG_CONFIG_HOME` — herdr consults it on Windows too, before the platform
/// directory, so a Windows user with it set keeps one config under `~`.
fn native_config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("HERDR_CONFIG_PATH") {
        return Some(PathBuf::from(path));
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("herdr").join("config.toml"));
    }
    #[cfg(windows)]
    if let Ok(dir) = std::env::var("APPDATA") {
        return Some(PathBuf::from(dir).join("herdr").join("config.toml"));
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
    Some(PathBuf::from(home).join(".config").join("herdr").join("config.toml"))
}

/// The shell that prints a distro's herdr config.  Resolution happens inside
/// the distro because that is where the environment it depends on lives; a
/// missing file prints nothing and still exits zero, which reads as herdr's
/// defaults rather than as a distro we could not reach.
const CONFIG_SCRIPT: &str = concat!(
    r#"p=${HERDR_CONFIG_PATH:-${XDG_CONFIG_HOME:-$HOME/.config}/herdr/config.toml}; "#,
    r#"[ -f "$p" ] && cat "$p" || true"#,
);

/// herdr's config text for this side, or `None` when the side could not be
/// read at all.  An absent file is `Some("")`: herdr runs on its defaults
/// there, and so should the hint.
fn read_config(side: &Side, _blocking: &jobs::Blocking) -> Option<String> {
    match side {
        Side::Native => match std::fs::read_to_string(native_config_path()?) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
            Err(_) => None,
        },
        Side::Wsl(distro) => {
            let (program, args) = wsl::exec_invocation(distro, &["sh", "-lc", CONFIG_SCRIPT]);
            #[allow(clippy::disallowed_methods)] // Reading the distro's config is this arm's job.
            let output = command_ext::hidden(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        },
    }
}

/// What herdr's own config on this side says about leaving a pane and drawing
/// its state.  Every part is user-settable, so a row spelling out a chord the
/// user has rebound would be worse than a row that stays quiet.
pub fn settings(side: &Side, blocking: &jobs::Blocking) -> Option<Settings> {
    Some(settings_from(&read_config(side, blocking)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untouched_config_yields_herdrs_documented_chord() {
        assert_eq!(detach_chord_from(""), Some("Ctrl+B q".to_string()));
        assert_eq!(
            detach_chord_from(
                "onboarding = false
"
            ),
            Some("Ctrl+B q".to_string())
        );
    }

    #[test]
    fn a_rebound_prefix_moves_the_first_half() {
        let cfg = "[keys]
prefix = \"f12\"
";
        assert_eq!(detach_chord_from(cfg), Some("F12 q".to_string()));
    }

    #[test]
    fn a_rebound_detach_moves_the_second_half() {
        let cfg = "[keys]
detach = \"prefix+shift+d\"
";
        assert_eq!(detach_chord_from(cfg), Some("Ctrl+B Shift+D".to_string()));
    }

    #[test]
    fn a_direct_detach_binding_drops_the_prefix() {
        let cfg = "[keys]
detach = \"ctrl+alt+q\"
";
        assert_eq!(detach_chord_from(cfg), Some("Ctrl+Alt+Q".to_string()));
    }

    #[test]
    fn a_list_of_bindings_renders_the_first() {
        let cfg = "[keys]
detach = [\"prefix+q\", \"prefix+d\"]
";
        assert_eq!(detach_chord_from(cfg), Some("Ctrl+B q".to_string()));
    }

    #[test]
    fn an_unbound_detach_has_no_chord() {
        assert_eq!(
            detach_chord_from(
                "[keys]
detach = \"\"
"
            ),
            None
        );
        assert_eq!(
            detach_chord_from(
                "[keys]
detach = []
"
            ),
            None
        );
    }

    #[test]
    fn an_unparseable_config_falls_back_to_the_defaults() {
        assert_eq!(detach_chord_from("[keys"), Some("Ctrl+B q".to_string()));
    }

    #[test]
    fn the_indicator_set_follows_herdrs_own_choice() {
        assert_eq!(indicators_from(""), Indicators::Dots);
        assert_eq!(
            indicators_from(
                "[ui]
status_indicators = \"symbols\"
"
            ),
            Indicators::Symbols
        );
        assert_eq!(
            indicators_from(
                "[ui]
status_indicators = \"dots\"
"
            ),
            Indicators::Dots
        );
    }

    #[test]
    fn an_unknown_indicator_set_keeps_the_shipped_one() {
        let cfg = "[ui]
status_indicators = \"runes\"
";
        assert_eq!(indicators_from(cfg), Indicators::Dots);
    }

    #[test]
    fn settings_carry_both_halves_of_the_config() {
        let cfg = "[keys]
prefix = \"f12\"
[ui]
status_indicators = \"symbols\"
";
        assert_eq!(settings_from(cfg), Settings {
            detach: Some("F12 q".into()),
            indicators: Indicators::Symbols
        });
    }
}
