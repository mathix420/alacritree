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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};

use crate::{jobs, wsl, wsl_helper};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum Tool {
    Git,
    Gh,
    Delta,
    Doppler,
    Herdr,
    Tuicr,
}

impl Tool {
    /// Declaration order, which `configure` indexes by.
    pub const ALL: [Tool; 6] =
        [Tool::Git, Tool::Gh, Tool::Delta, Tool::Doppler, Tool::Herdr, Tool::Tuicr];

    /// The program's name, which is also its default path.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Git => "git",
            Tool::Gh => "gh",
            Tool::Delta => "delta",
            Tool::Doppler => "doppler",
            Tool::Herdr => "herdr",
            Tool::Tuicr => "tuicr",
        }
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

fn configured() -> &'static RwLock<[ToolPaths; 6]> {
    static PATHS: OnceLock<RwLock<[ToolPaths; 6]>> = OnceLock::new();
    PATHS.get_or_init(|| RwLock::new(Tool::ALL.map(ToolPaths::named)))
}

/// Publish the configured paths of every tool, indexed like [`Tool::ALL`].
/// Runs once at startup, before anything spawns a tool.
pub fn configure(paths: [ToolPaths; 6]) {
    *configured().write().unwrap_or_else(|e| e.into_inner()) = paths;
}

#[cfg(test)]
pub(crate) fn test_configuration() -> [ToolPaths; 6] {
    configured().read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
pub(crate) fn test_configuration_lock() -> &'static Mutex<()> {
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
pub fn wsl_in_job(tool: Tool, distro: &str, _blocking: &jobs::Blocking) -> String {
    if let Some(path) = wsl_override(tool) {
        return path;
    }
    if let Some(path) = lock(lookups()).cached(distro, tool) {
        return path;
    }
    match wsl_helper::capability(distro, tool.name()) {
        Some(path) => {
            lock(lookups()).found.insert((distro.to_string(), tool), path.clone());
            path
        },
        None => tool.name().to_string(),
    }
}

type Probe = Arc<dyn Fn(&str, Tool, &jobs::Blocking) -> Option<String> + Send + Sync>;

fn lookups() -> &'static Mutex<Lookups> {
    static LOOKUPS: OnceLock<Mutex<Lookups>> = OnceLock::new();
    LOOKUPS.get_or_init(|| Mutex::new(Lookups::new(Arc::new(probe_distro))))
}

/// The helper's hello resolved every tool when the helper started. A miss
/// there is not a kept miss: the live probe still sees a later install.
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

    use super::*;

    fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "the lookup never landed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_helper_hello_probes_the_registry_in_order() {
        assert_eq!(crate::wsl_helper::HELLO_TOOLS, Tool::ALL.map(Tool::name));
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
}
