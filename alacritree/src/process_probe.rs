//! What process is running inside a PTY.
//!
//! One question, four operating systems, and a cache that keeps the answer
//! for a beat so a frame does not walk the process table. [`ProbeHandle`] is
//! the whole interface: hand it the shell's pid and, for a shimmed WSL
//! session, the helper's probe key, and ask it for [`Signals`].
//!
//! Everything below that is per-platform: `/proc` on Linux, `sysctl` on
//! macOS, a background refresher over `sysinfo` on Windows, and the WSL
//! helper's own foreground `comm` for a session running inside a distro.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::wsl_helper::{self, WslProbe};

/// What the probe found running behind a PTY.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Signals {
    /// A recognized coding agent in the foreground, by its canonical name.
    pub(crate) agent: Option<&'static str>,
    /// Something other than the shell itself is running.
    pub(crate) foreground_job: bool,
    /// A split-managing TUI such as vim or tmux is running.
    pub(crate) nav_tui: bool,
    /// The probe could ask about this terminal at all.  False before a shell
    /// pid is known, before the Windows refresher has scanned the shell, and
    /// while a WSL session's helper gives no answer, so a caller can tell "no
    /// agent here" from "nobody could look".
    pub(crate) answered: bool,
}

/// The probe for one session, holding what it needs to ask and what it last
/// heard.
///
/// Answers are cached for [`AGENT_CACHE_TTL`], so a frame that asks three
/// times walks the process table once.
pub(crate) struct ProbeHandle {
    shell_pid: Option<u32>,
    wsl: Option<WslProbe>,
    cache: Cell<AgentCache>,
}

impl ProbeHandle {
    pub(crate) fn new(shell_pid: Option<u32>, wsl: Option<WslProbe>) -> Self {
        Self { shell_pid, wsl, cache: Cell::new(AgentCache::default()) }
    }

    pub(crate) fn set_shell_pid(&mut self, shell_pid: Option<u32>) {
        self.shell_pid = shell_pid;
        self.cache.set(AgentCache::default());
    }

    /// The shell the probe reads, for a test that drives a real one.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn shell_pid(&self) -> Option<u32> {
        self.shell_pid
    }

    /// Pretend the probe just saw a split-managing TUI, so a test about
    /// what a session does with that answer needs no process to run.
    #[cfg(test)]
    pub(crate) fn force_nav_tui_for_test(&self) {
        self.cache.set(AgentCache {
            polled_at: Some(Instant::now()),
            nav_tui: true,
            ..AgentCache::default()
        });
    }

    pub(crate) fn signals(&self) -> Signals {
        let cached = self.cache.get();
        if cached.polled_at.is_some_and(|at| at.elapsed() < AGENT_CACHE_TTL) {
            return Signals {
                agent: cached.agent_name,
                foreground_job: cached.foreground_job,
                nav_tui: cached.nav_tui,
                answered: cached.answered,
            };
        }
        let agent = self.shell_pid.and_then(foreground_agent_name);
        let (foreground_job, nav_tui, answered) = match &self.wsl {
            Some(probe) => {
                let comm = wsl_helper::foreground_comm(&probe.distro, &probe.key);
                let (foreground_job, nav_tui) = wsl_probe_signals(comm.as_deref());
                (foreground_job, nav_tui, comm.is_some())
            },
            None => (
                self.shell_pid.is_some_and(shell_has_foreground_job),
                self.shell_pid.is_some_and(foreground_nav_tui),
                self.shell_pid.is_some_and(probe_has_answered),
            ),
        };
        self.cache.set(AgentCache {
            polled_at: Some(Instant::now()),
            agent_name: agent,
            foreground_job,
            nav_tui,
            answered,
        });
        Signals { agent, foreground_job, nav_tui, answered }
    }
}

/// The pid of the shell a PTY started, where the platform will say.
pub(crate) fn shell_pid_of(pty: &alacritty_terminal::tty::Pty) -> Option<u32> {
    pty_shell_pid(pty)
}

#[derive(Clone, Copy, Default)]
struct AgentCache {
    polled_at: Option<Instant>,
    /// Recognized foreground agent, retained for status hints.
    agent_name: Option<&'static str>,
    /// Whether anything is running in the terminal beyond the shell itself.
    foreground_job: bool,
    /// Whether a split-managing TUI (vim, tmux) is running in the terminal;
    /// see [`Session::nav_tui_running`].
    nav_tui: bool,
    answered: bool,
}

const AGENT_CACHE_TTL: Duration = Duration::from_millis(1000);

/// Foreground process names recognized as agents. `comm` is kernel-truncated
/// — 15 bytes on Linux, 16 on macOS (`cursor-agent` would otherwise miss) —
/// and Windows names carry an `.exe` suffix, so matching uses `starts_with`.
const AGENT_PROCESS_NAMES: &[&str] =
    &["claude", "codex", "gemini", "aider", "cursor-agent", "continue"];

/// Pids in the tree rooted at `root` (inclusive), from a `(pid, parent)`
/// snapshot.  Root-inclusive so a session whose spawned program *is* the
/// agent still matches.  Parent links in a snapshot can be stale or cyclic
/// (pid reuse), so the walk tracks visited pids.
#[cfg(any(test, windows))]
fn process_tree_pids(procs: &[(u32, Option<u32>)], root: u32) -> Vec<u32> {
    use std::collections::HashSet;
    let mut tree = vec![root];
    let mut visited: HashSet<u32> = tree.iter().copied().collect();
    let mut cursor = 0;
    while cursor < tree.len() {
        let parent = tree[cursor];
        cursor += 1;
        for &(pid, ppid) in procs {
            if ppid == Some(parent) && visited.insert(pid) {
                tree.push(pid);
            }
        }
    }
    tree
}

/// Match process names against the agent list. Lowercased `starts_with`,
/// mirroring the Linux `comm` match while tolerating Windows' `.exe`
/// suffix and case-insensitive filenames.
#[cfg(any(test, windows))]
fn agent_name_by_name(names: impl IntoIterator<Item = impl AsRef<str>>) -> Option<&'static str> {
    names.into_iter().find_map(|n| {
        let n = n.as_ref().to_ascii_lowercase();
        AGENT_PROCESS_NAMES.iter().find(|name| n.starts_with(*name)).copied()
    })
}

/// TUIs that manage their own splits and cooperate with FocusLeft/
/// FocusRight (vim, nvim, tmux, zellij, herdr): the key is forwarded while
/// one runs, and the TUI calls `alacritree action FocusLeft` or `FocusRight`
/// over IPC once it has no split left in that direction.  Matches Linux
/// `comm` values (`tmux: client`) and Windows image names (`nvim.exe`)
/// alike.  gvim stays out because it owns its own window.  The WSL helper's
/// `PROBE` script carries the same list.
#[cfg(any(test, target_os = "linux", target_os = "macos", windows))]
fn is_nav_tui_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["nvim", "vim", "tmux", "zellij", "herdr"].iter().any(|tui| n.starts_with(tui))
}

/// FocusLeft/FocusRight passthrough decision for a shimmed WSL session: the
/// helper's cached foreground `comm`, matched like the native Linux probe.
/// Unknown means no TUI — the keys move panel focus.  Gated the same as
/// `is_nav_tui_name` (plus its `not(...)` fallback below) since `Session`
/// always carries a `wsl_probe` field, so `process_probe`'s match on it must
/// compile everywhere, even though a shimmed session only exists on Windows.
#[cfg(any(test, target_os = "linux", windows))]
fn wsl_nav_tui(comm: Option<&str>) -> bool {
    comm.is_some_and(is_nav_tui_name)
}

#[cfg(not(any(test, target_os = "linux", windows)))]
fn wsl_nav_tui(_comm: Option<&str>) -> bool {
    // Same gap as the agent probe: macOS isn't wired up yet.
    false
}

/// `(foreground_job, nav_tui)` for a shimmed WSL session, from the helper's
/// cached foreground `comm`.  Any comm at all means a job owns the tty — the
/// helper reports nothing for an idle shell.  The Windows descendant probe
/// can't stand in here: wsl.exe keeps plumbing children alive for the life
/// of the session, so it reads every idle WSL shell as busy.
fn wsl_probe_signals(comm: Option<&str>) -> (bool, bool) {
    (comm.is_some(), wsl_nav_tui(comm))
}

/// Match full command lines against the agent list — picks up
/// `node ...\claude-code\cli.js`-style wrappers that hide behind their
/// runtime's name, same as the Linux cmdline pass.
#[cfg(any(test, windows))]
fn agent_name_by_cmdline(cmds: impl IntoIterator<Item = impl AsRef<str>>) -> Option<&'static str> {
    cmds.into_iter().find_map(|c| {
        let c = c.as_ref().to_ascii_lowercase();
        AGENT_PROCESS_NAMES.iter().find(|name| c.contains(*name)).copied()
    })
}

#[cfg(unix)]
fn pty_shell_pid(pty: &alacritty_terminal::tty::Pty) -> Option<u32> {
    Some(pty.child().id())
}

#[cfg(windows)]
fn pty_shell_pid(pty: &alacritty_terminal::tty::Pty) -> Option<u32> {
    // Under ConPTY the PTY child *is* the shell; everything the user runs
    // is spawned beneath it.
    pty.child_watcher().pid().map(std::num::NonZeroU32::get)
}

#[cfg(not(any(unix, windows)))]
fn pty_shell_pid(_pty: &alacritty_terminal::tty::Pty) -> Option<u32> {
    None
}

/// Match a foreground process against the agent list: `comm` first (cheap),
/// then anywhere in `cmdline` — picks up `node /path/to/agent-cli.js`-style
/// wrappers that hide behind their runtime's name.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn agent_name_for(comm: Option<&str>, cmdline: Option<&str>) -> Option<&'static str> {
    let comm_trim = comm.map(str::trim).unwrap_or("");
    let by_comm = AGENT_PROCESS_NAMES.iter().find(|name| comm_trim.starts_with(*name)).copied();
    if by_comm.is_some() {
        return by_comm;
    }
    if let Some(cmd) = cmdline {
        let name = AGENT_PROCESS_NAMES.iter().find(|name| cmd.contains(*name)).copied();
        if name.is_some() {
            return name;
        }
        log::debug!("foreground process not matched: comm={comm_trim:?} cmdline={cmd:?}");
    }
    None
}

#[cfg(target_os = "linux")]
fn foreground_agent_name(shell_pid: u32) -> Option<&'static str> {
    let tpgid = read_tpgid(shell_pid)?;
    if tpgid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{tpgid}/comm")).ok();
    let cmdline = read_cmdline(tpgid as u32);
    agent_name_for(comm.as_deref(), cmdline.as_deref())
}

#[cfg(target_os = "linux")]
fn read_cmdline(pid: u32) -> Option<String> {
    // `cmdline` is NUL-separated argv; rendering with spaces is good enough
    // for substring matching and human-readable logging.
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let s: String = bytes.iter().map(|&b| if b == 0 { ' ' } else { b as char }).collect();
    Some(s.trim().to_string())
}

/// `/proc/<pid>/stat` is `pid (comm) state ppid pgrp session tty_nr tpgid …`.
/// `comm` may contain spaces and unmatched parens, so split on the *last* `)`
/// before tokenizing the rest.
#[cfg(any(target_os = "linux", test))]
fn stat_pgrp_tpgid(stat: &str) -> Option<(i32, i32)> {
    let close = stat.rfind(')')?;
    let after = &stat[close + 1..];
    // After `comm`: state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5).
    let mut fields = after.split_whitespace();
    let pgrp = fields.nth(2)?.parse::<i32>().ok()?;
    let tpgid = fields.nth(2)?.parse::<i32>().ok()?;
    Some((pgrp, tpgid))
}

/// `comm` (between the first `(` and last `)`) and `pgrp` from a
/// `/proc/<pid>/stat` line — the two fields the foreground-group scan matches
/// on.  `comm` is kernel-truncated to 15 bytes, so nav-TUI names match with
/// `starts_with`, and it may itself contain spaces and parens, so the split is
/// on the *last* `)`.
#[cfg(any(target_os = "linux", test))]
fn stat_comm_pgrp(stat: &str) -> Option<(&str, i32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    // After `comm`: state(0) ppid(1) pgrp(2).
    let pgrp = stat[close + 1..].split_whitespace().nth(2)?.parse::<i32>().ok()?;
    Some((comm, pgrp))
}

#[cfg(target_os = "linux")]
fn read_tpgid(shell_pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{shell_pid}/stat")).ok()?;
    stat_pgrp_tpgid(&stat).map(|(_, tpgid)| tpgid)
}

/// The shell is its own foreground process group when idle; the terminal's
/// foreground group differing from the shell's own group means a job owns
/// the terminal right now.
#[cfg(target_os = "linux")]
fn shell_has_foreground_job(shell_pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{shell_pid}/stat")) else {
        return false;
    };
    stat_pgrp_tpgid(&stat).is_some_and(|(pgrp, tpgid)| tpgid > 0 && tpgid != pgrp)
}

/// The macOS analogue of the `/proc` reads: `proc_pidinfo(PROC_PIDTBSDINFO)`
/// returns a `proc_bsdinfo` carrying the process group (`pbi_pgid`), the
/// terminal's foreground group (`e_tpgid`), and the command name
/// (`pbi_comm`) — the same fields Linux takes from `/proc/<pid>/stat` and
/// `comm`.
#[cfg(target_os = "macos")]
fn bsdinfo_for_pid(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    // A vanished or inaccessible pid reports fewer bytes than the struct.
    (written == size).then_some(info)
}

#[cfg(target_os = "macos")]
fn pgid_tpgid(pid: u32) -> Option<(u32, u32)> {
    let info = bsdinfo_for_pid(pid)?;
    Some((info.pbi_pgid, info.e_tpgid))
}

/// A process without a controlling terminal reports `e_tpgid` as 0 or as
/// `-1` wrapped into the unsigned field; neither names a foreground group.
#[cfg(target_os = "macos")]
fn foreground_group(tpgid: u32) -> Option<u32> {
    (tpgid != 0 && tpgid != u32::MAX).then_some(tpgid)
}

/// `pbi_comm` is kernel-truncated to 16 bytes, like the 15-byte Linux `comm`
/// — which is why the agent map matches with `starts_with`.
#[cfg(target_os = "macos")]
fn comm_for_pid(pid: u32) -> Option<String> {
    let info = bsdinfo_for_pid(pid)?;
    let bytes: Vec<u8> = info.pbi_comm.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

/// `KERN_PROCARGS2` is readable for same-user processes, which the shell's
/// foreground job always is.
#[cfg(target_os = "macos")]
fn read_cmdline(pid: u32) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size = 0usize;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    procargs2_cmdline(&buf)
}

/// `KERN_PROCARGS2` layout: native-endian `argc`, the executable path, NUL
/// padding, then argv and environment strings, all NUL-terminated.  Taking
/// exactly `argc` strings keeps the environment out; spaces join the args
/// the same way the Linux `/proc/<pid>/cmdline` read renders them.
#[cfg(target_os = "macos")]
fn procargs2_cmdline(buf: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|&n| n > 0)?;
    let after_exec = buf[4..].iter().position(|&b| b == 0).map(|nul| &buf[4 + nul..])?;
    let args_start = after_exec.iter().position(|&b| b != 0)?;
    let joined = after_exec[args_start..]
        .split(|&b| b == 0)
        .take(argc)
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    let joined = joined.trim().to_string();
    (!joined.is_empty()).then_some(joined)
}

#[cfg(target_os = "macos")]
fn foreground_agent_name(shell_pid: u32) -> Option<&'static str> {
    let (_, tpgid) = pgid_tpgid(shell_pid)?;
    let tpgid = foreground_group(tpgid)?;
    let comm = comm_for_pid(tpgid);
    let cmdline = read_cmdline(tpgid);
    agent_name_for(comm.as_deref(), cmdline.as_deref())
}

/// Same rule as Linux: the shell is its own foreground group when idle, so
/// the terminal's foreground group differing from the shell's own group
/// means a job owns the terminal right now.
#[cfg(target_os = "macos")]
fn shell_has_foreground_job(shell_pid: u32) -> bool {
    pgid_tpgid(shell_pid)
        .is_some_and(|(pgid, tpgid)| foreground_group(tpgid).is_some_and(|t| t != pgid))
}

/// Same rule as Linux: the group leader first (nvim/tmux run directly), then
/// the rest of the foreground process group, so an editor launched by another
/// foreground program (`chezmoi edit`, `git commit`) is recognized even though
/// the launcher, not the editor, leads the group.
#[cfg(target_os = "macos")]
fn foreground_nav_tui(shell_pid: u32) -> bool {
    let Some((pgid, tpgid)) = pgid_tpgid(shell_pid) else {
        return false;
    };
    let Some(tpgid) = foreground_group(tpgid) else {
        return false;
    };
    // No foreground job: the shell owns the terminal, nothing to forward to.
    if tpgid == pgid {
        return false;
    }
    if comm_for_pid(tpgid).is_some_and(|comm| is_nav_tui_name(comm.trim())) {
        return true;
    }
    foreground_group_has_nav_tui(tpgid)
}

/// A nav TUI anywhere in foreground process group `pgid`.
///
/// `proc_listpgrppids` gives us just the stable, public PID list instead of
/// relying on Darwin's private `kinfo_proc` layout.  Command names then come
/// from the same `proc_bsdinfo` query used by the group-leader fast path.
#[cfg(target_os = "macos")]
fn foreground_group_has_nav_tui(pgid: u32) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return false;
    };

    let count = unsafe { libc::proc_listpgrppids(pgid, std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return false;
    };
    if count == 0 {
        return false;
    }

    // Leave room for processes spawned between the sizing and filling calls.
    let mut pids = vec![0 as libc::pid_t; count.saturating_add(16)];
    let Ok(buffer_size) = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())) else {
        return false;
    };
    let listed = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), buffer_size) };
    let Ok(listed) = usize::try_from(listed) else {
        return false;
    };
    pids.truncate(listed.min(pids.len()));

    pids.into_iter()
        .filter_map(|pid| u32::try_from(pid).ok().filter(|&pid| pid != 0))
        .any(|pid| comm_for_pid(pid).is_some_and(|comm| is_nav_tui_name(comm.trim())))
}

/// Windows reads the process table on a background thread, so a shell it has
/// not scanned yet has no answer rather than an empty one.
#[cfg(windows)]
fn probe_has_answered(shell_pid: u32) -> bool {
    windows_process_probe::has_scanned(shell_pid)
}

/// Every other platform reads the process table on the calling thread.
#[cfg(not(windows))]
fn probe_has_answered(_shell_pid: u32) -> bool {
    true
}

/// Windows has no foreground process group, so "a job is running" is
/// approximated as the shell having any descendant process — the same
/// approximation the agent probe uses.
#[cfg(windows)]
fn shell_has_foreground_job(shell_pid: u32) -> bool {
    windows_process_probe::probe(shell_pid).1
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn shell_has_foreground_job(_shell_pid: u32) -> bool {
    // No probe wired for this platform (BSDs would mirror the macOS sysctl).
    false
}

/// Windows has no foreground process group, so "foreground" is approximated
/// as *any* recognized agent in the shell's descendant tree. This is what the
/// status means to the user — "an agent is running here" — and it stays
/// stable while agents run their own subprocesses, where a deepest-leaf
/// heuristic would flicker.
#[cfg(windows)]
fn foreground_agent_name(shell_pid: u32) -> Option<&'static str> {
    windows_process_probe::probe(shell_pid).0
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn foreground_agent_name(_shell_pid: u32) -> Option<&'static str> {
    // No probe wired for this platform (BSDs would mirror the macOS sysctl).
    None
}

/// Whether a split-managing TUI owns the terminal.  Checks the foreground
/// group leader first (the common case: nvim/tmux run directly), then scans
/// the rest of the foreground process group, so an editor launched by another
/// foreground program — `chezmoi edit`, `git commit`, `sudoedit` — is still
/// recognized even though that launcher, not the editor, leads the group.
#[cfg(target_os = "linux")]
fn foreground_nav_tui(shell_pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{shell_pid}/stat")) else {
        return false;
    };
    let Some((pgrp, tpgid)) = stat_pgrp_tpgid(&stat) else {
        return false;
    };
    // No foreground job: the shell owns the terminal, nothing to forward to.
    if tpgid <= 0 || tpgid == pgrp {
        return false;
    }
    let tpgid = tpgid as u32;
    if std::fs::read_to_string(format!("/proc/{tpgid}/comm"))
        .is_ok_and(|comm| is_nav_tui_name(comm.trim()))
    {
        return true;
    }
    foreground_group_has_nav_tui(tpgid)
}

/// A nav TUI anywhere in foreground process group `pgid`, not just its leader.
/// A launcher stays the group leader while the editor it spawned shares its
/// group, so reading only the leader's `comm` would miss the nvim on screen.
#[cfg(target_os = "linux")]
fn foreground_group_has_nav_tui(pgid: u32) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some((comm, pgrp)) = stat_comm_pgrp(&stat) {
            if pgrp >= 0 && pgrp as u32 == pgid && is_nav_tui_name(comm.trim()) {
                return true;
            }
        }
    }
    false
}

/// Windows has no foreground process group, so a nav TUI anywhere in the
/// shell's descendant tree counts — the same approximation the agent probe
/// uses.
#[cfg(windows)]
fn foreground_nav_tui(shell_pid: u32) -> bool {
    windows_process_probe::probe(shell_pid).2
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn foreground_nav_tui(_shell_pid: u32) -> bool {
    // Same gap as the agent probe: no probe wired for this platform.
    false
}

#[cfg(windows)]
mod windows_process_probe {
    //! Background process-table scan behind the sidebar's agent signals.
    //!
    //! Enumerating the process table costs ~12 ms and fetching command lines
    //! another ~15 ms, both far too much for the UI thread that asks for them.
    //! A single refresher thread does the work for every shell the UI has
    //! asked about and publishes the results; `probe` only reads what was last
    //! published.  Sessions already tolerate an answer up to `AGENT_CACHE_TTL`
    //! old, so nothing about the displayed result changes.
    //!
    //! The scan is two-phase: names and parent pids for the whole table (one
    //! cheap system call class), command lines only for a shell's descendants
    //! and only when no name matched.
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Condvar, Mutex, PoisonError};
    use std::time::Duration;

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    use super::{agent_name_by_cmdline, agent_name_by_name, is_nav_tui_name, process_tree_pids};

    /// Slightly under `AGENT_CACHE_TTL`, so a session polling on its own clock
    /// finds a result no older than one of its own cache windows.
    pub(super) const REFRESH_INTERVAL: Duration = Duration::from_millis(900);

    /// Agent found in the shell's descendant tree, whether the shell has any
    /// descendants at all, and whether one of them is a nav TUI.
    pub(super) type Signals = (Option<&'static str>, bool, bool);

    /// Per shell: the descendant tree the last command-line scan ran on, and
    /// the agent it found there.
    pub(super) type ScannedTrees = BTreeMap<u32, (Vec<u32>, Option<&'static str>)>;

    pub(super) fn name_signals_for_tree<F>(
        tree: &[u32],
        mut name_for_pid: F,
    ) -> (Vec<String>, Option<&'static str>, bool)
    where
        F: FnMut(u32) -> Option<String>,
    {
        let names: Vec<String> = tree.iter().filter_map(|&pid| name_for_pid(pid)).collect();
        let nav_tui = names.iter().any(|n| is_nav_tui_name(n));
        let agent = agent_name_by_name(&names);
        (names, agent, nav_tui)
    }

    pub(super) fn cache_tree(tree: &[u32]) -> Vec<u32> {
        let mut cache_tree = tree.to_vec();
        // The table comes out of a hash map, so cache comparisons need an
        // order independent of its iteration order.
        cache_tree.sort_unstable();
        cache_tree
    }

    #[derive(Default)]
    struct Shared {
        /// Shells asked about since the last pass.  Taken rather than kept, so
        /// a window nobody is drawing costs nothing: with no frames there are
        /// no probes, and the refresher blocks instead of enumerating.
        wanted: BTreeSet<u32>,
        published: BTreeMap<u32, Signals>,
        refresher_running: bool,
    }

    static SHARED: Mutex<Shared> = Mutex::new(Shared {
        wanted: BTreeSet::new(),
        published: BTreeMap::new(),
        refresher_running: false,
    });
    /// Wakes the refresher when it is idling with nothing to scan.
    static WANTED: Condvar = Condvar::new();

    /// The last published signals for `shell_pid`, defaulting to "nothing
    /// running" until the refresher has seen it.  Registers the shell so the
    /// next pass covers it.
    pub(super) fn probe(shell_pid: u32) -> Signals {
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if !shared.refresher_running {
            shared.refresher_running = true;
            // Its own thread rather than `jobs::pool()`: this one runs for
            // the life of the process and spends most of it parked on a
            // `Condvar`, so a pooled slot would be held and never returned.
            // `ipc/server.rs` and `notify/mod.rs` spawn theirs the same way
            // for the same reason.
            std::thread::Builder::new()
                .name("alacritree-process-probe".into())
                .spawn(refresh_loop)
                .expect("spawn process probe thread");
        }
        let signals = shared.published.get(&shell_pid).copied();
        if shared.wanted.insert(shell_pid) {
            WANTED.notify_one();
        }
        signals.unwrap_or_default()
    }

    pub(super) fn has_scanned(shell_pid: u32) -> bool {
        SHARED.lock().unwrap_or_else(PoisonError::into_inner).published.contains_key(&shell_pid)
    }

    #[cfg(test)]
    pub(super) fn published(shell_pid: u32) -> Option<Signals> {
        SHARED.lock().unwrap_or_else(PoisonError::into_inner).published.get(&shell_pid).copied()
    }

    #[cfg(test)]
    pub(super) fn nothing_wanted() -> bool {
        SHARED.lock().unwrap_or_else(PoisonError::into_inner).wanted.is_empty()
    }

    fn refresh_loop() {
        let mut sys = System::new();
        let mut scanned = ScannedTrees::new();
        loop {
            let wanted = {
                let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
                while shared.wanted.is_empty() {
                    shared = WANTED.wait(shared).unwrap_or_else(PoisonError::into_inner);
                }
                std::mem::take(&mut shared.wanted)
            };

            // Everything below runs with no lock held: the UI thread must
            // never wait on an enumeration.
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            let table: Vec<(u32, Option<u32>)> = sys
                .processes()
                .iter()
                .map(|(pid, p)| (pid.as_u32(), p.parent().map(|pp| pp.as_u32())))
                .collect();
            let alive: BTreeSet<u32> = table.iter().map(|(pid, _)| *pid).collect();
            let published: BTreeMap<u32, Signals> = wanted
                .iter()
                .filter(|pid| alive.contains(pid))
                .map(|&pid| (pid, scan(&mut sys, &table, pid, &mut scanned)))
                .collect();
            scanned.retain(|pid, _| alive.contains(pid));

            {
                // Merged, not replaced: a shell that happened not to be asked
                // about this pass keeps its last answer instead of blinking
                // back to "nothing running".
                let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
                shared.published.retain(|pid, _| alive.contains(pid));
                shared.published.extend(published);
            }
            std::thread::sleep(REFRESH_INTERVAL);
        }
    }

    fn scan(
        sys: &mut System,
        table: &[(u32, Option<u32>)],
        shell_pid: u32,
        scanned: &mut ScannedTrees,
    ) -> Signals {
        let tree = process_tree_pids(table, shell_pid);
        let has_children = tree.len() > 1;
        let (_names, agent, nav_tui) = name_signals_for_tree(&tree, |pid| {
            sys.process(Pid::from_u32(pid))
                .map(|process| process.name().to_string_lossy().into_owned())
        });
        let pids: Vec<Pid> = tree.iter().copied().map(Pid::from_u32).collect();
        let cache_tree = cache_tree(&tree);
        if let Some(name) = agent {
            return (Some(name), has_children, nav_tui);
        }
        if let Some(name) = remembered_agent(scanned, shell_pid, &cache_tree) {
            return (name, has_children, nav_tui);
        }

        // Names missed: fetch command lines for just the tree to catch
        // agents launched through node/python shims.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        let cmds = pids
            .iter()
            .filter_map(|pid| sys.process(*pid))
            .map(|p| p.cmd().iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        let name = agent_name_by_cmdline(cmds);
        scanned.insert(shell_pid, (cache_tree, name));
        (name, has_children, nav_tui)
    }

    /// The agent remembered for `shell_pid`, or `None` when the tree has
    /// changed since it was taken and the scan has to run again.  Fetching
    /// command lines is the expensive half of a pass, and its answer can only
    /// change when the descendant set does.
    pub(super) fn remembered_agent(
        cache: &ScannedTrees,
        shell_pid: u32,
        tree: &[u32],
    ) -> Option<Option<&'static str>> {
        cache.get(&shell_pid).filter(|(scanned, _)| scanned == tree).map(|(_, name)| *name)
    }
}

#[cfg(test)]
// Fixtures drive real processes and wait on them; no frame is pending.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn the_probe_hands_the_scan_to_the_refresher() {
        let pid = std::process::id();

        let first = windows_process_probe::probe(pid);
        assert_eq!(first, (None, false, false), "the first probe had an answer to give");

        let deadline = Instant::now() + Duration::from_secs(20);
        while windows_process_probe::published(pid).is_none() {
            assert!(Instant::now() < deadline, "the background refresh never published a result");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            windows_process_probe::nothing_wanted(),
            "the refresher would keep enumerating for a window nobody is drawing"
        );
    }
    #[cfg(windows)]
    #[test]
    fn a_remembered_cmdline_scan_expires_when_the_tree_changes() {
        use std::collections::BTreeMap;

        use super::windows_process_probe::remembered_agent;

        let mut cache = BTreeMap::new();
        cache.insert(42, (vec![42, 100], None));

        assert_eq!(remembered_agent(&cache, 42, &[42, 100]), Some(None));
        assert_eq!(remembered_agent(&cache, 42, &[42, 100, 101]), None);
        assert_eq!(remembered_agent(&cache, 43, &[42, 100]), None);
    }
    #[cfg(windows)]
    #[test]
    fn a_parent_agent_wins_over_a_lower_pid_child_agent() {
        use super::windows_process_probe::name_signals_for_tree;

        let processes = [
            (15256, None, "powershell.exe"),
            (34352, Some(15256), "claude.exe"),
            (32372, Some(34352), "codex.exe"),
        ];
        let parent_links: Vec<_> =
            processes.iter().map(|&(pid, parent, _)| (pid, parent)).collect();
        let tree = process_tree_pids(&parent_links, 15256);

        let (_, agent, _) = name_signals_for_tree(&tree, |pid| {
            processes
                .iter()
                .find(|&&(candidate, ..)| candidate == pid)
                .map(|&(_, _, name)| name.to_owned())
        });

        assert_eq!(agent, Some("claude"));
    }
    #[cfg(windows)]
    #[test]
    fn a_root_agent_is_selected_without_descendants() {
        use super::windows_process_probe::name_signals_for_tree;

        let tree = process_tree_pids(&[(15256, None)], 15256);
        let (_, agent, _) = name_signals_for_tree(&tree, |_| Some("claude.exe".to_owned()));

        assert_eq!(agent, Some("claude"));
    }
    #[cfg(windows)]
    #[test]
    fn cache_tree_ignores_process_snapshot_sibling_order() {
        use super::windows_process_probe::cache_tree;

        let first =
            process_tree_pids(&[(15256, None), (34352, Some(15256)), (32372, Some(15256))], 15256);
        let second =
            process_tree_pids(&[(15256, None), (32372, Some(15256)), (34352, Some(15256))], 15256);

        assert_ne!(first, second);
        assert_eq!(cache_tree(&first), cache_tree(&second));
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_process_probe_cost() {
        let pid = std::process::id();
        windows_process_probe::probe(pid);
        // Let the refresher publish, so the reported cost is the steady state
        // rather than the empty-map one.
        std::thread::sleep(super::windows_process_probe::REFRESH_INTERVAL * 2);

        let iterations = 1000;
        let started = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(windows_process_probe::probe(pid));
        }
        let each = started.elapsed() / iterations;

        let (_, counts) = crate::alloc_count::measure(|| windows_process_probe::probe(pid));
        println!(
            "probe on the calling thread: {each:?}, {} allocations ({} KiB)",
            counts.allocs,
            counts.bytes / 1024,
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn procargs2_cmdline_joins_argv_and_skips_exec_path() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3i32.to_ne_bytes());
        buf.extend_from_slice(b"/usr/local/bin/node\0\0\0\0");
        // argv, then the environment block `take(argc)` must never reach.
        buf.extend_from_slice(b"node\0/path/claude-code/cli.js\0--continue\0HOME=/Users/x\0");
        assert_eq!(
            procargs2_cmdline(&buf).as_deref(),
            Some("node /path/claude-code/cli.js --continue")
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn procargs2_cmdline_rejects_truncated_or_empty_buffers() {
        assert_eq!(procargs2_cmdline(b""), None);
        assert_eq!(procargs2_cmdline(&0i32.to_ne_bytes()), None);
        assert_eq!(procargs2_cmdline(&3i32.to_ne_bytes()), None);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn sysctl_probe_reads_a_real_childs_comm_and_group() {
        let mut child = crate::command_ext::hidden("/bin/sleep").arg("30").spawn().unwrap();
        let comm = comm_for_pid(child.id());
        let groups = pgid_tpgid(child.id());
        child.kill().ok();
        child.wait().ok();
        assert_eq!(comm.as_deref(), Some("sleep"));
        let own_pgid = unsafe { libc::getpgrp() };
        assert_eq!(groups.map(|(pgid, _)| pgid), Some(own_pgid as u32));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_shells_foreground_job_flips_the_probe() {
        use crate::config::Config;
        use crate::repaint::Recorder;
        use crate::session::{Session, SessionKind, TermSize};

        let mut config = Config::default();
        config.env.insert("TERM".to_string(), "xterm-256color".to_string());

        let mut session = Session::spawn_command(
            Recorder::default(),
            &config,
            std::env::current_dir().ok(),
            TermSize::new(80, 24),
            (8.0, 16.0),
            "/bin/zsh".to_string(),
            // `-f` skips the user's rc files, whose plugins run transient
            // foreground jobs of their own that would race the probe.
            vec!["-f".to_string(), "-i".to_string()],
            "probe".to_string(),
            SessionKind::Shell,
        )
        .unwrap();
        let shell_pid = session.shell_pid().expect("unix PTYs always report the child pid");

        // The shell owning its terminal (tpgid == pgid) is exactly the state
        // the probe reads as "not busy".
        let start = Instant::now();
        while pgid_tpgid(shell_pid).is_none_or(|(pgid, tpgid)| tpgid != pgid) {
            assert!(start.elapsed() < Duration::from_secs(10), "shell never took the terminal");
            std::thread::sleep(Duration::from_millis(10));
        }

        // Absolute path: a `sleep` from PATH may be a differently-named
        // multi-call binary (uutils coreutils).
        session.write(b"/bin/sleep 30\n".to_vec());
        let start = Instant::now();
        loop {
            let foreground_comm = pgid_tpgid(shell_pid)
                .and_then(|(_, tpgid)| foreground_group(tpgid))
                .and_then(comm_for_pid);
            if shell_has_foreground_job(shell_pid) && foreground_comm.as_deref() == Some("sleep") {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the running job never flipped the foreground probe (last foreground comm: \
                 {foreground_comm:?})"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn stat_parse_extracts_pgrp_and_tpgid_past_a_parenthesized_comm() {
        let stat = "1234 (my (weird) shell) S 1 1234 1234 34816 5678 0 42";
        assert_eq!(stat_pgrp_tpgid(stat), Some((1234, 5678)));
    }
    #[test]
    fn stat_parse_rejects_truncated_or_malformed_lines() {
        assert_eq!(stat_pgrp_tpgid("garbage with no paren"), None);
        assert_eq!(stat_pgrp_tpgid("1 (sh) S 1 2"), None);
    }
    #[test]
    fn stat_comm_pgrp_reads_comm_and_group_for_the_group_scan() {
        // A member of a launcher's foreground group: comm is the editor, pgrp
        // is the launcher's group — how the scan spots nvim under chezmoi.
        assert_eq!(stat_comm_pgrp("42 (nvim) S 1 900 900 34816 900"), Some(("nvim", 900)));
        // comm may hold spaces and inner parens — split on the last `)`.
        assert_eq!(
            stat_comm_pgrp("7 (tmux: (server)) S 1 88 88 0 -1"),
            Some(("tmux: (server)", 88))
        );
        assert_eq!(stat_comm_pgrp("garbage with no paren"), None);
    }
    #[test]
    fn tree_walk_collects_root_and_descendants_only() {
        // 1 → {10 → {20 → 30}, 40 → 50}; rooting at 10 must exclude 40's branch.
        let procs = [
            (1, None),
            (10, Some(1)),
            (20, Some(10)),
            (30, Some(20)),
            (40, Some(1)),
            (50, Some(40)),
        ];
        let mut tree = process_tree_pids(&procs, 10);
        tree.sort_unstable();
        assert_eq!(tree, vec![10, 20, 30]);
    }
    #[test]
    fn tree_walk_includes_root_even_without_children() {
        // A session can be spawned with the agent as the shell program itself.
        assert_eq!(process_tree_pids(&[(7, None)], 7), vec![7]);
    }
    #[test]
    fn tree_walk_survives_cyclic_parent_links() {
        // Snapshot parent data can be stale (pid reuse) and form cycles.
        let procs = [(10, Some(20)), (20, Some(10))];
        let mut tree = process_tree_pids(&procs, 10);
        tree.sort_unstable();
        assert_eq!(tree, vec![10, 20]);
    }
    #[test]
    fn name_match_handles_exe_suffix_and_case() {
        assert_eq!(agent_name_by_name(["pwsh.exe", "Claude.exe"]), Some("claude"));
        assert_eq!(agent_name_by_name(["cursor-agent.exe"]), Some("cursor-agent"));
        assert_eq!(agent_name_by_name(["pwsh.exe", "git.exe"]), None);
        assert_eq!(agent_name_by_name(["not-claude.exe"]), None);
        assert_eq!(agent_name_by_name(std::iter::empty::<&str>()), None);
    }
    #[test]
    fn wsl_busy_needs_a_foreground_comm() {
        // Idle shell: the helper reports no foreground comm at all.
        assert_eq!(wsl_probe_signals(None), (false, false));
        // Any foreground job counts as busy, cooperating TUI or not.
        assert_eq!(wsl_probe_signals(Some("sleep")), (true, false));
        assert_eq!(wsl_probe_signals(Some("claude")), (true, false));
        assert_eq!(wsl_probe_signals(Some("nvim")), (true, true));
    }
    #[test]
    fn wsl_nav_tui_needs_a_known_cooperating_comm() {
        assert!(wsl_nav_tui(Some("nvim")));
        assert!(wsl_nav_tui(Some("vim")));
        assert!(wsl_nav_tui(Some("tmux: client")));
        assert!(wsl_nav_tui(Some("zellij")));
        assert!(wsl_nav_tui(Some("herdr")));
        // A shell, an agent, or an unknown probe must move panel focus —
        // losing passthrough beats losing the keys.
        assert!(!wsl_nav_tui(Some("bash")));
        assert!(!wsl_nav_tui(Some("claude")));
        assert!(!wsl_nav_tui(None));
    }
    #[test]
    fn nav_tui_name_match_covers_both_platforms_naming() {
        // Windows image names.
        assert!(is_nav_tui_name("nvim.exe"));
        assert!(is_nav_tui_name("NVIM.EXE"));
        assert!(is_nav_tui_name("vim.exe"));
        assert!(is_nav_tui_name("herdr.exe"));
        assert!(is_nav_tui_name("ZELLIJ.EXE"));
        // Linux comm values.
        assert!(is_nav_tui_name("nvim"));
        assert!(is_nav_tui_name("tmux: client"));
        assert!(is_nav_tui_name("zellij"));
        assert!(is_nav_tui_name("herdr"));
        // gvim owns its own window — it never runs inside the terminal.
        assert!(!is_nav_tui_name("gvim.exe"));
        assert!(!is_nav_tui_name("chezmoi.exe"));
        assert!(!is_nav_tui_name("pwsh.exe"));
        // A wsl.exe in the descendant tree no longer implies a cooperating
        // TUI — the Windows process table can't see the distro side, so the
        // resident helper's foreground probe is the only signal now.
        assert!(!is_nav_tui_name("wsl.exe"));
        assert!(!is_nav_tui_name("wslhost.exe"));
        assert!(!is_nav_tui_name("WSLRELAY.EXE"));
    }
    #[test]
    fn cmdline_match_catches_runtime_wrappers() {
        let cmd =
            r"node C:\Users\lev\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\cli.js";
        assert_eq!(agent_name_by_cmdline([cmd]), Some("claude"));
        assert_eq!(agent_name_by_cmdline([r"pwsh.exe -NoLogo"]), None);
    }
}
