//! WSL awareness: distro enumeration, Windows ↔ Linux path translation, and
//! `wsl.exe` command construction.  The only module that knows WSL exists —
//! everything else dispatches on `Location` or hands this module argv to
//! wrap.  On non-Windows builds (and Windows without WSL) `distros()` is
//! empty and `classify` never returns `Wsl`, so all WSL code paths are
//! dormant without cfg-gating at call sites.

use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::OnceLock;

/// Where a path physically lives.  `linux_path` is the path as seen from
/// inside the distro, always with forward slashes and a leading `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Windows(PathBuf),
    Wsl { distro: String, linux_path: String },
}

/// The distro-side directory Windows drives are mounted under.  Set once at
/// startup from `[ui.wsl] automount_root`; `/mnt` is WSL's default.
static AUTOMOUNT_ROOT: OnceLock<String> = OnceLock::new();

pub fn set_automount_root(root: String) {
    let _ = AUTOMOUNT_ROOT.set(root);
}

fn automount_root() -> &'static str {
    AUTOMOUNT_ROOT.get().map(String::as_str).unwrap_or("/mnt")
}

/// Classify by UNC prefix: `\\wsl$\<distro>\…` and `\\wsl.localhost\<distro>\…`
/// (and their `\\?\UNC\…` verbatim forms) are WSL; everything else is Windows.
pub fn classify(path: &Path) -> Location {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Location::Windows(path.to_path_buf());
    };
    let (server, share) = match prefix.kind() {
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => (server, share),
        _ => return Location::Windows(path.to_path_buf()),
    };
    let server = server.to_string_lossy();
    if !server.eq_ignore_ascii_case("wsl$") && !server.eq_ignore_ascii_case("wsl.localhost") {
        return Location::Windows(path.to_path_buf());
    }
    let mut linux_path = String::new();
    for component in components {
        if let Component::Normal(segment) = component {
            linux_path.push('/');
            linux_path.push_str(&segment.to_string_lossy());
        }
    }
    if linux_path.is_empty() {
        linux_path.push('/');
    }
    Location::Wsl { distro: share.to_string_lossy().into_owned(), linux_path }
}

/// Translate a Linux path reported by git inside `distro` to the Windows
/// path the rest of the app uses: `<automount_root>/<drive>/…` becomes a
/// drive path, anything else a `\\wsl.localhost\<distro>\…` UNC path.
pub fn linux_to_windows(linux: &str, distro: &str) -> PathBuf {
    linux_to_windows_with(linux, distro, automount_root())
}

fn linux_to_windows_with(linux: &str, distro: &str, automount_root: &str) -> PathBuf {
    let root = automount_root.trim_end_matches('/');
    if let Some(rest) = linux.strip_prefix(root) {
        // The root must end at a segment boundary — "/mnta/…" is not under "/mnt".
        if rest.starts_with('/') {
            let mut segments = rest.split('/').filter(|s| !s.is_empty());
            if let Some(first) = segments.next() {
                let mut chars = first.chars();
                if let (Some(letter), None) = (chars.next(), chars.next()) {
                    if letter.is_ascii_alphabetic() {
                        let mut out = format!("{}:\\", letter.to_ascii_uppercase());
                        out.push_str(&segments.collect::<Vec<_>>().join("\\"));
                        return PathBuf::from(out);
                    }
                }
            }
        }
    }
    let mut out = format!(r"\\wsl.localhost\{distro}");
    for segment in linux.split('/').filter(|s| !s.is_empty()) {
        out.push('\\');
        out.push_str(segment);
    }
    PathBuf::from(out)
}

/// Translate a Windows path to what git inside a distro can resolve:
/// WSL UNC paths strip to their Linux part; drive paths map under the
/// automount root; anything else (non-WSL UNC shares) is untranslatable.
pub fn windows_to_linux(path: &Path) -> Option<String> {
    windows_to_linux_with(path, automount_root())
}

fn windows_to_linux_with(path: &Path, automount_root: &str) -> Option<String> {
    if let Location::Wsl { linux_path, .. } = classify(path) {
        return Some(linux_path);
    }
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return None;
    };
    let drive = match prefix.kind() {
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d,
        _ => return None,
    };
    let root = automount_root.trim_end_matches('/');
    let mut out = format!("{root}/{}", (drive as char).to_ascii_lowercase());
    for component in components {
        if let Component::Normal(segment) = component {
            out.push('/');
            out.push_str(&segment.to_string_lossy());
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[cfg(windows)]
    #[test]
    fn classifies_wsl_localhost_unc() {
        let loc = classify(Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj"));
        assert_eq!(
            loc,
            Location::Wsl {
                distro: "kali-linux".to_string(),
                linux_path: "/home/lev/proj".to_string(),
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn classifies_wsl_dollar_unc() {
        let loc = classify(Path::new(r"\\wsl$\Ubuntu\srv"));
        assert_eq!(
            loc,
            Location::Wsl { distro: "Ubuntu".to_string(), linux_path: "/srv".to_string() }
        );
    }

    #[cfg(windows)]
    #[test]
    fn classifies_distro_root() {
        let loc = classify(Path::new(r"\\wsl.localhost\kali-linux"));
        assert_eq!(
            loc,
            Location::Wsl { distro: "kali-linux".to_string(), linux_path: "/".to_string() }
        );
    }

    #[cfg(windows)]
    #[test]
    fn classifies_drive_and_non_wsl_unc_as_windows() {
        assert!(matches!(classify(Path::new(r"C:\Users\Lev")), Location::Windows(_)));
        assert!(matches!(classify(Path::new(r"\\server\share\x")), Location::Windows(_)));
    }

    #[test]
    fn linux_home_path_maps_to_unc() {
        let p = linux_to_windows_with("/home/lev/proj", "kali-linux", "/mnt");
        assert_eq!(p, PathBuf::from(r"\\wsl.localhost\kali-linux\home\lev\proj"));
    }

    #[test]
    fn linux_automount_path_maps_to_drive() {
        let p = linux_to_windows_with("/mnt/c/Users/Lev", "kali-linux", "/mnt");
        assert_eq!(p, PathBuf::from(r"C:\Users\Lev"));
        let p = linux_to_windows_with("/drives/d/x", "kali-linux", "/drives");
        assert_eq!(p, PathBuf::from(r"D:\x"));
    }

    #[test]
    fn automount_prefix_must_be_a_whole_segment() {
        // "/mnta/…" must not match root "/mnt", and a multi-char segment
        // after the root is a directory, not a drive letter.
        let p = linux_to_windows_with("/mnta/c/x", "kali", "/mnt");
        assert_eq!(p, PathBuf::from(r"\\wsl.localhost\kali\mnta\c\x"));
        let p = linux_to_windows_with("/mnt/cd/x", "kali", "/mnt");
        assert_eq!(p, PathBuf::from(r"\\wsl.localhost\kali\mnt\cd\x"));
    }

    #[cfg(windows)]
    #[test]
    fn drive_path_maps_to_automount() {
        assert_eq!(
            windows_to_linux_with(Path::new(r"C:\Users\Lev"), "/mnt").as_deref(),
            Some("/mnt/c/Users/Lev")
        );
        assert_eq!(
            windows_to_linux_with(Path::new(r"D:\x y\z"), "/drives").as_deref(),
            Some("/drives/d/x y/z")
        );
    }

    #[cfg(windows)]
    #[test]
    fn wsl_unc_maps_back_to_linux() {
        assert_eq!(
            windows_to_linux_with(Path::new(r"\\wsl.localhost\kali-linux\home\lev"), "/mnt")
                .as_deref(),
            Some("/home/lev")
        );
    }
}
