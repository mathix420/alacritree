//! WSL awareness: distro enumeration, Windows ↔ Linux path translation, and
//! `wsl.exe` command construction.  The only module that knows WSL exists —
//! everything else dispatches on `Location` or hands this module argv to
//! wrap.  On non-Windows builds (and Windows without WSL) `distros()` is
//! empty and `classify` never returns `Wsl`, so all WSL code paths are
//! dormant without cfg-gating at call sites.

use crate::command_ext::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Per-project shell override, persisted in state.toml as `"windows"`,
/// `"wsl:<distro>"`, or `"profile:<name>"`.  Absent means auto-by-location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellChoice {
    Windows,
    Wsl(String),
    Profile(String),
}

impl ShellChoice {
    pub fn parse(s: &str) -> Option<Self> {
        if s == "windows" {
            return Some(Self::Windows);
        }
        if let Some(d) = s.strip_prefix("wsl:").filter(|d| !d.is_empty()) {
            return Some(Self::Wsl(d.to_string()));
        }
        s.strip_prefix("profile:").filter(|n| !n.is_empty()).map(|n| Self::Profile(n.to_string()))
    }

    pub fn to_state_string(&self) -> String {
        match self {
            Self::Windows => "windows".to_string(),
            Self::Wsl(distro) => format!("wsl:{distro}"),
            Self::Profile(name) => format!("profile:{name}"),
        }
    }
}

/// Where a path physically lives.  `linux_path` is the path as seen from
/// inside the distro, always with forward slashes and a leading `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Windows(PathBuf),
    Wsl { distro: String, linux_path: String },
}

/// The distro-side directory Windows drives are mounted under.  Set once at
/// startup from `[wsl] automount_root`; `/mnt` is WSL's default.
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

/// Canonical spelling for a project root: WSL paths re-emitted through
/// `linux_to_windows`, so `\\wsl$\` and `\\wsl.localhost\` inputs converge
/// on one form and path equality holds across discovery refreshes.
pub fn normalize_root(path: PathBuf) -> PathBuf {
    match classify(&path) {
        Location::Wsl { distro, linux_path } => linux_to_windows(&linux_path, &distro),
        Location::Windows(_) => path,
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslDistro {
    pub name: String,
    pub is_default: bool,
}

/// Docker/Rancher register utility distros the user never shells into.
#[cfg(any(windows, test))]
fn is_utility_distro(name: &str) -> bool {
    name.starts_with("docker-desktop") || name.starts_with("rancher-desktop")
}

/// Registered distros, default first-classed.  Registry is the primary
/// source (no process spawn, knows the default); `wsl -l -q` is the
/// fallback when the key is unreadable.  Empty means WSL features stay
/// dormant.
///
/// Cached for the process lifetime: one caller is a per-frame UI path (the
/// sidebar), and the CLI fallback spawns `wsl.exe` — without caching, every
/// repaint would probe the registry or shell out. A distro registered or
/// unregistered after startup is picked up only on restart; that's an
/// acceptable trade since mid-session registration churn is rare, and a
/// stale entry just falls through the existing spawn-failure/degrade paths.
#[cfg(windows)]
pub fn distros() -> Vec<WslDistro> {
    static DISTROS: OnceLock<Vec<WslDistro>> = OnceLock::new();
    DISTROS
        .get_or_init(|| match registry_distros() {
            Some(list) if !list.is_empty() => list,
            _ => cli_distros(),
        })
        .clone()
}

#[cfg(not(windows))]
pub fn distros() -> Vec<WslDistro> {
    Vec::new()
}

#[cfg(windows)]
fn registry_distros() -> Option<Vec<WslDistro>> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let lxss = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Lxss")
        .ok()?;
    let default_guid: String = lxss.get_value("DefaultDistribution").unwrap_or_default();
    let mut out = Vec::new();
    for guid in lxss.enum_keys().flatten() {
        let Ok(subkey) = lxss.open_subkey(&guid) else { continue };
        let Ok(name) = subkey.get_value::<String, _>("DistributionName") else { continue };
        if is_utility_distro(&name) {
            continue;
        }
        out.push(WslDistro { is_default: guid == default_guid, name });
    }
    Some(out)
}

#[cfg(windows)]
fn cli_distros() -> Vec<WslDistro> {
    let output = command_bare()
        .args(["-l", "-q"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => parse_distro_list(&o.stdout),
        _ => Vec::new(),
    }
}

/// `wsl -l -q` lists the default distro first.  Output is UTF-8 when
/// WSL_UTF8=1 is honored (WSL 0.64.0+); older versions emit UTF-16LE,
/// detected by the NUL bytes ASCII names acquire in that encoding.
#[cfg(any(windows, test))]
fn parse_distro_list(stdout: &[u8]) -> Vec<WslDistro> {
    let text = if stdout.contains(&0) {
        let units: Vec<u16> =
            stdout.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(stdout).into_owned()
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_utility_distro(line))
        .enumerate()
        .map(|(i, name)| WslDistro { name: name.to_string(), is_default: i == 0 })
        .collect()
}

/// `wsl.exe -d <distro> [--cd <dir>] --exec` with the console window
/// suppressed and wsl.exe's own messages forced to UTF-8 (they are UTF-16LE
/// otherwise; the relayed Linux byte stream is unaffected).  Callers append
/// the argv to run — `--exec` passes it verbatim to the process, skipping
/// the user's shell and rc files (per-invocation rc sourcing is a known
/// latency trap).  `--cd` natively accepts Windows, UNC, and Linux paths.
pub fn command(distro: &str, cd: Option<&Path>) -> Command {
    let mut cmd = command_bare();
    cmd.arg("-d").arg(distro);
    if let Some(dir) = cd {
        cmd.arg("--cd").arg(dir);
    }
    cmd.arg("--exec");
    cmd
}

fn command_bare() -> Command {
    let mut cmd = Command::new("wsl.exe");
    cmd.hide_console().env("WSL_UTF8", "1");
    cmd
}

/// Program + args for a session whose shell runs inside `distro`.  No
/// `--exec`: wsl.exe launches the distro's own default login shell, which
/// is the contract — we never guess shells.
pub fn shell_invocation(distro: &str, workdir: &Path) -> (String, Vec<String>) {
    (
        "wsl.exe".to_string(),
        vec![
            "-d".to_string(),
            distro.to_string(),
            "--cd".to_string(),
            workdir.to_string_lossy().into_owned(),
        ],
    )
}

/// Separates the outputs of the individual commands a batch script runs.
/// Scripts emit it between sections via `sep() { printf '\n@@ALACRITREE@@\n'; }`;
/// NUL-delimited porcelain payloads pass through untouched because the
/// separator is matched as raw bytes, and the leading newline absorbs the
/// section's own trailing newline when it has one.
pub const SECTION_SEP: &[u8] = b"\n@@ALACRITREE@@\n";

/// Run `script` through `sh -c` inside `distro`, with `args` bound to
/// `$1..`.  Rides the resident helper's pipe when it is up; otherwise one
/// wsl.exe round trip (~400 ms warm on a dev machine, seconds while the VM
/// cold-boots) — callers batch every query for a repo into a single script
/// and must never call this on the UI thread.
pub fn run_batch(distro: &str, script: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    // A request the helper may have executed is never re-run as a one-shot
    // (batch scripts have side effects); only a transport that failed
    // before the write falls through to the spawn below.
    if let Some(result) = crate::wsl_helper::try_run(distro, script, args) {
        return result;
    }
    let output = command(distro, None)
        .arg("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run wsl.exe: {e}"))?;
    // Scripts guard individual commands with `2>/dev/null || true`-style
    // fallbacks; a hard failure with no stdout means wsl.exe itself refused
    // (deregistered distro, WSL not installed).
    if !output.status.success() && output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() { "wsl.exe failed".to_string() } else { stderr });
    }
    Ok(output.stdout)
}

/// Resolve `delta`'s absolute path inside `distro`, as the user's login shell
/// sees it.  `wsl.exe --exec sh` only inherits the default system PATH, which
/// omits per-user install dirs like `~/.cargo/bin`; sourcing the login shell
/// (`getent passwd` → the user's shell, run with `-lc`) picks up the profile
/// that puts delta on PATH.  One wsl.exe round trip — call off the UI thread.
/// Returns `None` when delta isn't found; callers must not cache that, so an
/// install mid-session is picked up on the next attempt.
pub fn discover_delta(distro: &str) -> Option<String> {
    // The helper's hello already resolved delta through the login shell; a
    // missing capability is not a cached miss — fall through and re-check
    // live so a mid-session install is still picked up.
    if let Some(path) = crate::wsl_helper::capability_delta(distro) {
        return Some(path);
    }
    let script = r#"s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$s" ] || s=${SHELL:-/bin/sh}; exec "$s" -lc 'command -v delta'"#;
    let output = command(distro, None)
        .arg("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Split batched stdout on `SECTION_SEP`.  Always returns at least one
/// section; a script with N separators yields N+1.
pub fn split_sections(stdout: &[u8]) -> Vec<&[u8]> {
    let mut sections = Vec::new();
    let mut rest = stdout;
    while let Some(pos) = rest.windows(SECTION_SEP.len()).position(|w| w == SECTION_SEP) {
        sections.push(&rest[..pos]);
        rest = &rest[pos + SECTION_SEP.len()..];
    }
    sections.push(rest);
    sections
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

    #[cfg(windows)]
    #[test]
    fn normalize_root_converges_wsl_dollar_and_localhost() {
        let normalized = normalize_root(PathBuf::from(r"\\wsl$\kali-linux\home\lev"));
        assert_eq!(normalized, PathBuf::from(r"\\wsl.localhost\kali-linux\home\lev"));
    }

    #[cfg(windows)]
    #[test]
    fn normalize_root_leaves_windows_paths_unchanged() {
        let normalized = normalize_root(PathBuf::from(r"C:\x"));
        assert_eq!(normalized, PathBuf::from(r"C:\x"));
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

    #[test]
    fn parses_utf8_distro_list() {
        let out = b"kali-linux\nUbuntu\ndocker-desktop\n";
        let distros = parse_distro_list(out);
        assert_eq!(distros.len(), 2);
        assert_eq!(distros[0], WslDistro { name: "kali-linux".to_string(), is_default: true });
        assert_eq!(distros[1], WslDistro { name: "Ubuntu".to_string(), is_default: false });
    }

    #[test]
    fn parses_utf16_distro_list() {
        // wsl.exe older than 0.64.0 ignores WSL_UTF8 and emits UTF-16LE.
        let text = "kali-linux\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let distros = parse_distro_list(&bytes);
        assert_eq!(distros, vec![WslDistro { name: "kali-linux".to_string(), is_default: true }]);
    }

    #[test]
    fn command_builds_expected_argv() {
        let cmd = command("kali-linux", Some(Path::new(r"\\wsl.localhost\kali-linux\home")));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(cmd.get_program().to_string_lossy(), "wsl.exe");
        assert_eq!(
            args,
            vec!["-d", "kali-linux", "--cd", r"\\wsl.localhost\kali-linux\home", "--exec"]
        );
    }

    #[test]
    fn shell_invocation_has_no_exec() {
        let (program, args) = shell_invocation("kali-linux", Path::new(r"C:\proj"));
        assert_eq!(program, "wsl.exe");
        assert_eq!(args, vec!["-d", "kali-linux", "--cd", r"C:\proj"]);
    }

    #[test]
    fn splits_sections_preserving_nuls() {
        let mut input = Vec::new();
        input.extend_from_slice(b"yes");
        input.extend_from_slice(SECTION_SEP);
        input.extend_from_slice(b"a\0b\0\0c\0");
        input.extend_from_slice(SECTION_SEP);
        input.extend_from_slice(b"tail");
        let sections = split_sections(&input);
        assert_eq!(sections, vec![&b"yes"[..], &b"a\0b\0\0c\0"[..], &b"tail"[..]]);
    }

    #[test]
    fn split_handles_empty_and_missing_sections() {
        assert_eq!(split_sections(b""), vec![&b""[..]]);
        let mut input = Vec::new();
        input.extend_from_slice(SECTION_SEP);
        input.extend_from_slice(SECTION_SEP);
        assert_eq!(split_sections(&input), vec![&b""[..], &b""[..], &b""[..]]);
    }

    #[test]
    fn shell_choice_round_trips() {
        assert_eq!(ShellChoice::parse("windows"), Some(ShellChoice::Windows));
        assert_eq!(
            ShellChoice::parse("wsl:kali-linux"),
            Some(ShellChoice::Wsl("kali-linux".to_string()))
        );
        assert_eq!(ShellChoice::parse("wsl:"), None);
        assert_eq!(ShellChoice::parse("plan9"), None);
        assert_eq!(ShellChoice::Wsl("u".to_string()).to_state_string(), "wsl:u");
        assert_eq!(ShellChoice::Windows.to_state_string(), "windows");
    }

    #[test]
    fn profile_choice_round_trips() {
        assert_eq!(
            ShellChoice::parse("profile:pwsh"),
            Some(ShellChoice::Profile("pwsh".to_string()))
        );
        assert_eq!(ShellChoice::parse("profile:"), None);
        assert_eq!(ShellChoice::Profile("pwsh".to_string()).to_state_string(), "profile:pwsh");
    }

    /// Live round trip against the default distro.  Requires WSL; run
    /// manually: `cargo test -p alacritree wsl:: -- --ignored`
    #[test]
    #[ignore]
    fn run_batch_round_trips() {
        let distro = distros().into_iter().find(|d| d.is_default).expect("a default distro");
        let out = run_batch(&distro.name, r#"printf '%s' "$1""#, &["hello"]).unwrap();
        assert_eq!(out, b"hello");
    }
}
