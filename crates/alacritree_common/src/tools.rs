//! The external programs alacritree runs, and where each one lives.
//!
//! Each tool has a native path and an optional WSL path, because a Windows
//! path means nothing inside a distro. A native path runs as written when it
//! differs from the tool's own name, and the name alone is found by the OS's
//! PATH search. A set WSL path runs as written inside every distro; unset,
//! the name is found through the user's login shell, since `wsl.exe --exec`
//! sees only the default system PATH, which omits per-user install dirs like
//! `~/.cargo/bin`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use strum::{EnumCount, VariantArray};

use crate::{jobs, wsl, wsl_helper};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    strum::EnumCount,
    strum::VariantArray,
    strum::IntoStaticStr,
    strum::Display,
)]
#[strum(serialize_all = "lowercase")]
pub enum Tool {
    Git,
    Gh,
    Delta,
    Doppler,
    Herdr,
    Tuicr,
    Task,
    Zellij,
}

impl Tool {
    /// The program's name, which is also its default path.
    pub fn name(self) -> &'static str {
        self.into()
    }

    /// One value per tool, indexed by discriminant, which is the shape the
    /// paths table and `configure` take.
    pub fn table<T>(mut f: impl FnMut(Tool) -> T) -> [T; Tool::COUNT] {
        std::array::from_fn(|i| f(Tool::VARIANTS[i]))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where one tool lives on each side, as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPaths {
    /// The tool's own name, or a path that runs as written.
    pub native: String,
    /// A path that runs as written inside every distro, or `None` to find
    /// the tool by name there.
    pub wsl: Option<String>,
}

impl ToolPaths {
    pub fn named(tool: Tool) -> Self {
        Self { native: tool.name().to_string(), wsl: None }
    }
}

/// `[integrations.<tool>]` for a tool with nothing to configure but where
/// it lives.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolConfig {
    /// The tool's own name, or a native path that runs as written.
    pub path: String,
    /// A path that runs as written inside every WSL distro, or `None` to
    /// find the tool by name there.
    pub wsl_path: Option<String>,
}

/// A blank path means the side's default: the tool's name natively, and
/// discovery inside WSL.
pub fn tool_config(path: String, wsl_path: String, tool: Tool) -> ToolConfig {
    ToolConfig {
        path: if path.trim().is_empty() { tool.name().to_string() } else { path },
        wsl_path: Some(wsl_path).filter(|path| !path.trim().is_empty()),
    }
}

fn configured() -> &'static RwLock<[ToolPaths; Tool::COUNT]> {
    static PATHS: OnceLock<RwLock<[ToolPaths; Tool::COUNT]>> = OnceLock::new();
    PATHS.get_or_init(|| RwLock::new(Tool::table(ToolPaths::named)))
}

/// Publish the configured paths of every tool, indexed by [`Tool`] discriminant.
/// Runs once at startup, before anything spawns a tool.
pub fn configure(paths: [ToolPaths; Tool::COUNT]) {
    *configured().write().unwrap_or_else(|e| e.into_inner()) = paths;
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_configuration() -> [ToolPaths; Tool::COUNT] {
    configured().read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_configuration_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn configured_paths(tool: Tool) -> ToolPaths {
    configured().read().unwrap_or_else(|e| e.into_inner())[tool as usize].clone()
}

/// The configured WSL path. It is used as written and never looked up.
fn wsl_override(tool: Tool) -> Option<String> {
    configured_paths(tool).wsl
}

/// The program to spawn natively for `tool`: the configured path, which is
/// the bare name unless it was set.
pub fn program(tool: Tool) -> String {
    configured_paths(tool).native
}

/// The absolute path of `tool` inside `distro`, for the UI thread: the
/// configured path or a path an earlier lookup found, else `None` while a
/// background lookup runs. Starts at most one lookup per distro and tool,
/// and `on_found` runs when it lands so the caller can repaint. A miss is
/// never kept, so a tool installed mid-session is found by a later call.
pub fn wsl_resolved(
    tool: Tool,
    distro: &str,
    on_found: impl FnOnce() + Send + 'static,
) -> Option<String> {
    if let Some(path) = wsl_override(tool) {
        return Some(path);
    }
    lock(lookups()).resolve(distro, tool, Box::new(on_found))
}

/// The program to name for `tool` inside a distro where a shell finds it:
/// the configured WSL path, else the bare name.
pub fn wsl_program(tool: Tool) -> String {
    wsl_override(tool).unwrap_or_else(|| tool.name().to_string())
}

/// The program to name for `tool` inside `distro` from a pool job. It uses a
/// configured WSL path, a cached lookup, the resident helper, or the bare name.
/// This runs off the UI thread because reaching the helper can start it.
pub fn wsl_in_job(tool: Tool, distro: &str, blocking: &jobs::Blocking) -> String {
    wsl_located(tool, distro, blocking).unwrap_or_else(|| tool.name().to_string())
}

/// Where `tool` is inside `distro` when that is known without a login shell:
/// the configured path, a cached lookup, or the resident helper's. `None`
/// otherwise, which [`wsl_in_job`] turns into the bare name.
pub fn wsl_located(tool: Tool, distro: &str, _blocking: &jobs::Blocking) -> Option<String> {
    if let Some(path) = wsl_override(tool) {
        return Some(path);
    }
    if let Some(path) = lock(lookups()).cached(distro, tool) {
        return Some(path);
    }
    let path = wsl_helper::capability(distro, tool.name())?;
    lock(lookups()).found.insert((distro.to_string(), tool), path.clone());
    Some(path)
}

/// Resolve `program` the way the OS would: an explicit path as itself, a bare
/// name against each directory on the search path, trying each executable
/// extension (`PATHEXT` on Windows, none elsewhere).
fn locate_in(program: &str, dirs: &[PathBuf], exts: &[String]) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    dirs.iter().find_map(|dir| {
        exts.iter().find_map(|ext| {
            let candidate = dir.join(format!("{program}{ext}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

pub fn locate(program: &str) -> Option<PathBuf> {
    locate_in(program, &search_path(), &executable_extensions())
}

/// Where `Command::new(program)` finds `program`, which is narrower than
/// [`locate`] on Windows. A shell runs a `.cmd` shim or an extensionless
/// script for a bare name, but `Command` never spawns either.
pub fn locate_spawnable(program: &str) -> Option<PathBuf> {
    locate_in(program, &search_path(), &spawnable_extensions(program))
}

fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default()
}

/// `Command` on Windows appends `.exe` to a name without an extension and
/// tries nothing else.
fn spawnable_extensions(program: &str) -> Vec<String> {
    let bare = cfg!(windows) && Path::new(program).extension().is_none();
    vec![if bare { ".exe" } else { "" }.to_string()]
}

/// The empty extension comes last on Windows too. `PATHEXT` covers `git.exe`,
/// but a bare extensionless file is still executable if it is there.
#[cfg(windows)]
fn executable_extensions() -> Vec<String> {
    let mut exts: Vec<String> = std::env::var("PATHEXT")
        .map(|v| v.split(';').map(str::to_lowercase).filter(|e| !e.is_empty()).collect())
        .unwrap_or_else(|_| vec![".exe".to_string()]);
    exts.push(String::new());
    exts
}

#[cfg(not(windows))]
fn executable_extensions() -> Vec<String> {
    vec![String::new()]
}

type Probe = Arc<dyn Fn(&str, Tool, &jobs::Blocking) -> Option<String> + Send + Sync>;

fn lookups() -> &'static Mutex<Lookups> {
    static LOOKUPS: OnceLock<Mutex<Lookups>> = OnceLock::new();
    LOOKUPS.get_or_init(|| Mutex::new(Lookups::new(Arc::new(probe_distro))))
}

/// The helper's hello resolved every tool when the helper started. A miss
/// there is not kept, because the live probe still sees a later install.
fn probe_distro(distro: &str, tool: Tool, blocking: &jobs::Blocking) -> Option<String> {
    wsl_helper::capability(distro, tool.name()).or_else(|| {
        wsl::probe_tools(distro, &[tool.name()], blocking).ok()?.into_iter().next().flatten()
    })
}

/// Paths found inside each distro, and the lookups still running.
struct Lookups {
    found: HashMap<(String, Tool), String>,
    pending: HashMap<(String, Tool), jobs::Job<Option<String>>>,
    probe: Probe,
}

impl Lookups {
    fn new(probe: Probe) -> Self {
        Self { found: HashMap::new(), pending: HashMap::new(), probe }
    }

    fn cached(&mut self, distro: &str, tool: Tool) -> Option<String> {
        self.adopt(distro, tool);
        self.found.get(&(distro.to_string(), tool)).cloned()
    }

    /// Bank a landed lookup. A found-nothing landing and a panicked lookup
    /// both clear the pending entry, so neither wedges the tool out of ever
    /// being looked up again.
    fn adopt(&mut self, distro: &str, tool: Tool) {
        let key = (distro.to_string(), tool);
        match self.pending.get(&key).map(|job| (job.poll(), job.failed())) {
            Some((Some(Some(path)), _)) => {
                self.pending.remove(&key);
                self.found.insert(key, path);
            },
            Some((Some(None), _)) | Some((None, true)) => {
                self.pending.remove(&key);
            },
            _ => {},
        }
    }

    fn resolve(
        &mut self,
        distro: &str,
        tool: Tool,
        on_found: Box<dyn FnOnce() + Send>,
    ) -> Option<String> {
        if let Some(path) = self.cached(distro, tool) {
            return Some(path);
        }
        let key = (distro.to_string(), tool);
        if !self.pending.contains_key(&key) {
            let probe = self.probe.clone();
            let distro = distro.to_string();
            let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
                let found = probe(&distro, tool, blocking);
                on_found();
                found
            });
            self.pending.insert(key, job);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use strum::{EnumCount, VariantArray};

    use super::*;

    #[test]
    fn tool_names_are_the_lowercase_program_names() {
        let names: Vec<&str> = Tool::VARIANTS.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["git", "gh", "delta", "doppler", "herdr", "tuicr", "task", "zellij"]);
        assert_eq!(Tool::Doppler.to_string(), "doppler");
    }

    #[test]
    fn the_table_is_indexed_by_discriminant() {
        assert_eq!(Tool::COUNT, Tool::VARIANTS.len());
        let table = Tool::table(|t| t);
        for (i, tool) in table.iter().enumerate() {
            assert_eq!(*tool as usize, i);
        }
    }

    #[test]
    fn an_empty_path_falls_back_to_the_tool_name() {
        let config = tool_config("  ".into(), "".into(), Tool::Doppler);
        assert_eq!(config, ToolConfig { path: "doppler".into(), wsl_path: None });
        let config = tool_config("/opt/doppler".into(), "/usr/bin/doppler".into(), Tool::Doppler);
        assert_eq!(config.wsl_path.as_deref(), Some("/usr/bin/doppler"));
    }

    fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "the lookup never landed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_lookup_runs_once_per_distro_and_tool_and_keeps_its_hit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let counted = calls.clone();
        let probe: Probe = Arc::new(move |_, _, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = released.lock().unwrap().recv();
            Some("/home/lev/.cargo/bin/delta".to_string())
        });
        let mut lookups = Lookups::new(probe);

        assert_eq!(lookups.resolve("Ubuntu", Tool::Delta, Box::new(|| {})), None);
        assert_eq!(lookups.resolve("Ubuntu", Tool::Delta, Box::new(|| {})), None);
        release.send(()).unwrap();
        wait_until(|| lookups.cached("Ubuntu", Tool::Delta).is_some());

        assert_eq!(
            lookups.resolve("Ubuntu", Tool::Delta, Box::new(|| {})).as_deref(),
            Some("/home/lev/.cargo/bin/delta")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(lookups.cached("kali-linux", Tool::Delta), None, "distros resolve apart");
    }

    #[test]
    fn a_miss_is_not_kept_so_the_next_call_looks_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let probe: Probe = Arc::new(move |_, _, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            None
        });
        let mut lookups = Lookups::new(probe);

        assert_eq!(lookups.resolve("Ubuntu", Tool::Tuicr, Box::new(|| {})), None);
        wait_until(|| {
            lookups.adopt("Ubuntu", Tool::Tuicr);
            lookups.pending.is_empty()
        });
        assert_eq!(lookups.resolve("Ubuntu", Tool::Tuicr, Box::new(|| {})), None);
        wait_until(|| calls.load(Ordering::SeqCst) == 2);
    }

    #[test]
    fn a_landed_lookup_tells_its_caller() {
        let probe: Probe = Arc::new(|_, _, _| Some("/usr/bin/git".to_string()));
        let mut lookups = Lookups::new(probe);
        let (found, heard) = mpsc::channel();
        lookups.resolve("Ubuntu", Tool::Git, Box::new(move || found.send(()).unwrap()));
        heard.recv_timeout(Duration::from_secs(5)).expect("on_found runs when the probe lands");
    }

    #[test]
    fn a_bare_name_is_found_on_the_search_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("tool.exe");
        std::fs::write(&exe, "").unwrap();

        let found = locate_in("tool", &[dir.path().to_path_buf()], &[".exe".to_string()]);

        assert_eq!(found, Some(exe));
    }

    /// Unix has no executable extension, so the empty one has to be tried too,
    /// or nothing is ever found there.
    #[test]
    fn a_bare_name_is_found_without_an_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("tool");
        std::fs::write(&exe, "").unwrap();

        let found = locate_in("tool", &[dir.path().to_path_buf()], &[String::new()]);

        assert_eq!(found, Some(exe));
    }

    #[test]
    fn a_name_that_is_not_on_the_path_is_not_found() {
        let dir = tempfile::TempDir::new().unwrap();

        assert_eq!(locate_in("tool", &[dir.path().to_path_buf()], &[String::new()]), None);
    }

    /// A configured shell is usually an absolute path (`C:\...\pwsh.exe`), which
    /// must be checked where it points rather than hunted for on the path.
    #[test]
    fn a_program_with_a_path_is_not_searched_for_on_the_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("shell");
        std::fs::write(&exe, "").unwrap();

        let found = locate_in(&exe.to_string_lossy(), &[], &[String::new()]);

        assert_eq!(found, Some(exe));
        assert_eq!(locate_in("/nowhere/shell", &[], &[String::new()]), None);
    }

    /// A shim that forwards `task` to WSL is on the search path for shells,
    /// and must not pass for a `task` that alacritree can spawn natively.
    #[cfg(windows)]
    #[test]
    fn a_shim_is_not_spawnable_and_an_exe_is() {
        let dir = tempfile::TempDir::new().unwrap();
        let dirs = [dir.path().to_path_buf()];
        std::fs::write(dir.path().join("task.cmd"), "").unwrap();
        std::fs::write(dir.path().join("task"), "").unwrap();

        assert_eq!(locate_in("task", &dirs, &spawnable_extensions("task")), None);

        let exe = dir.path().join("task.exe");
        std::fs::write(&exe, "").unwrap();
        assert_eq!(locate_in("task", &dirs, &spawnable_extensions("task")), Some(exe));
    }
}
