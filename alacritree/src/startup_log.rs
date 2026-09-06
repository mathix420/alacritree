//! What produced this log: the build, the config files behind it, and the
//! settings they resolved to.
//!
//! A log that does not record its configuration cannot explain a run, and the
//! keys that change behaviour most leave no other trace — a session that
//! opened its PTY on a worker and one that blocked the frame print the same
//! lines apart from a single phase name.  Mirrors alacritty's startup banner,
//! which logs the version and the config paths it loaded, and ghostty's
//! `global.zig`, which adds the build's own facts.
//!
//! The settings are the effective config minus its defaults, so reading a
//! value out of the log needs no knowledge of that version's defaults and a
//! stock install writes almost nothing.

use std::path::Path;

use crate::config::{Config, ConfigFile};

/// Record the build and its configuration.  `settings` asks for the full
/// config, which belongs in a file rather than on a terminal — the caller
/// passes whether a session log is open.
///
/// Called after the log sink is filled, not before: anything logged while the
/// sink is empty reaches stderr only, and the banner's whole job is to sit at
/// the top of the file.  Config parse failures are reported here as well as by
/// `config::load`, which runs too early to reach the file.
pub fn emit(config: &Config, files: &[ConfigFile], config_dir: Option<&Path>, settings: bool) {
    log::info!("alacritree {} on {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS);
    for file in files {
        log::info!("config {}", describe(file, config_dir));
    }
    if settings {
        match config.changed_from_defaults() {
            Some(changed) => log::info!("settings {changed}"),
            None => log::info!("settings: stock"),
        }
    }
}

/// `config_dir` decides how a missing file is worded: under `--config-dir`
/// there is no search path to have looked along, and saying there was sends
/// the reader hunting through directories the run never touched.
fn describe(file: &ConfigFile, config_dir: Option<&Path>) -> String {
    match (&file.path, &file.error) {
        (Some(path), Some(error)) => {
            format!("{}.toml: {} (ignored: {error})", file.stem, path.display())
        },
        (Some(path), None) => format!("{}.toml: {}", file.stem, path.display()),
        (None, _) => match config_dir {
            Some(dir) => format!("{}.toml: not in {}", file.stem, dir.display()),
            None => format!("{}.toml: not found on the search path", file.stem),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_failed_to_parse_says_its_settings_were_dropped() {
        let file = ConfigFile {
            stem: "alacritree",
            path: Some("/home/lev/.config/alacritty/alacritree.toml".into()),
            error: Some("expected `=`".to_string()),
        };

        assert_eq!(
            describe(&file, None),
            "alacritree.toml: /home/lev/.config/alacritty/alacritree.toml (ignored: expected `=`)"
        );
    }

    #[test]
    fn a_missing_file_says_so_rather_than_printing_an_empty_path() {
        let file = ConfigFile { stem: "alacritty", path: None, error: None };

        assert_eq!(describe(&file, None), "alacritty.toml: not found on the search path");
    }

    /// Under an override there is no search path, so blaming one would send
    /// the reader hunting through directories the run never looked at.
    #[test]
    fn a_missing_file_under_an_override_names_the_directory_that_lacked_it() {
        let file = ConfigFile { stem: "alacritty", path: None, error: None };

        assert_eq!(
            describe(&file, Some(Path::new("/tmp/gate-off"))),
            "alacritty.toml: not in /tmp/gate-off"
        );
    }
}
