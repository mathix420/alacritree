use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};

use crate::config::Config;

#[derive(Clone)]
pub struct EventProxy {
    ctx: egui::Context,
    sender: mpsc::Sender<TermEvent>,
}

impl EventProxy {
    pub fn new(ctx: egui::Context) -> (Self, mpsc::Receiver<TermEvent>) {
        let (sender, receiver) = mpsc::channel();
        (Self { ctx, sender }, receiver)
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.sender.send(event);
        self.ctx.request_repaint();
    }
}

#[derive(Copy, Clone, Debug)]
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TermSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self { columns: columns.max(1), screen_lines: screen_lines.max(1) }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

pub type SessionId = u64;

/// What this session is showing.  Shells are persistent; Diff panes are
/// throwaway — replaced when the user clicks a different file in the git
/// sidebar, and reaped on the user's `q` inside delta.  The key disambiguates
/// (file, source) so the sidebar can highlight the active row and toggle the
/// pane closed on a repeat click.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SessionKind {
    Shell,
    Diff { key: String },
}

/// PTY child + parsed terminal state.  The read/write loop is on its own
/// thread and survives workspace switches, so running processes aren't killed.
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub working_directory: Option<PathBuf>,
    pub kind: SessionKind,
    pub size: TermSize,
    pub cell_size: (f32, f32),
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub events: mpsc::Receiver<TermEvent>,
    /// Latched attention flag, cleared when the user views this session.
    pub needs_attention: bool,
    /// Sub-cell wheel residue (logical points), retained across frames so that
    /// trackpad pixel-deltas accumulate into whole-line scrolls instead of
    /// being dropped when each frame's delta is smaller than a cell.
    pub accumulated_scroll: (f64, f64),
    /// Shell pid spawned for this PTY.  Used to walk to the foreground
    /// process group when identifying which agent is running.  None on
    /// platforms where we don't yet capture it.
    shell_pid: Option<u32>,
    /// Cached result of the last foreground-process probe — refreshed on a
    /// timer instead of polling the process table every frame.  `Cell` is
    /// enough since `Session` isn't `Sync` and the values are `Copy`.
    agent_cache: Cell<AgentCache>,
    notifier: Notifier,
    sender: EventLoopSender,
    exited: bool,
}

#[derive(Clone, Copy, Default)]
struct AgentCache {
    polled_at: Option<Instant>,
    /// Static glyph for the foreground process if it's a recognized agent.
    process_glyph: Option<char>,
}

const AGENT_CACHE_TTL: Duration = Duration::from_millis(1000);

/// Map a foreground process name (`/proc/<pid>/comm` on Linux, image name
/// on Windows) to its static sidebar glyph.  Compared with `starts_with`:
/// Linux `comm` is kernel-truncated to 15 bytes (`cursor-agent` would
/// otherwise miss) and Windows names carry an `.exe` suffix.
const AGENT_PROCESS_GLYPHS: &[(&str, char)] = &[
    ("claude", '✳'),
    ("codex", '◇'),
    ("gemini", '✦'),
    ("aider", '▲'),
    ("cursor-agent", '❖'),
    ("continue", '⊕'),
];

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

/// Match process names against the agent map.  Lowercased `starts_with`,
/// mirroring the Linux `comm` match while tolerating Windows' `.exe`
/// suffix and case-insensitive filenames.
#[cfg(any(test, windows))]
fn agent_glyph_by_name(names: impl IntoIterator<Item = impl AsRef<str>>) -> Option<char> {
    names.into_iter().find_map(|n| {
        let n = n.as_ref().to_ascii_lowercase();
        AGENT_PROCESS_GLYPHS.iter().find(|(name, _)| n.starts_with(name)).map(|(_, g)| *g)
    })
}

/// Match full command lines against the agent map — picks up
/// `node ...\claude-code\cli.js`-style wrappers that hide behind their
/// runtime's name, same as the Linux cmdline pass.
#[cfg(any(test, windows))]
fn agent_glyph_by_cmdline(cmds: impl IntoIterator<Item = impl AsRef<str>>) -> Option<char> {
    cmds.into_iter().find_map(|c| {
        let c = c.as_ref().to_ascii_lowercase();
        AGENT_PROCESS_GLYPHS.iter().find(|(name, _)| c.contains(name)).map(|(_, g)| *g)
    })
}

#[derive(Default)]
pub struct DrainOutcome {
    /// Set if any event in this batch warrants flagging the session: BEL, or
    /// a title transitioning out of a spinner state.
    pub attention: bool,
}

/// Heuristic for "this title looks like a working/spinner state".  Matches
/// any title containing a Braille glyph (`U+2800..=U+28FF`), which is the
/// near-universal spinner alphabet (Claude Code, oh-my-posh, ollama, cargo's
/// progress indicator, etc.).
fn is_spinner_title(title: &str) -> bool {
    title.chars().any(|c| {
        let n = c as u32;
        (0x2800..=0x28FF).contains(&n)
    })
}

/// `<glyph> <text>` titles are the universal agent-CLI shape: a non-ASCII
/// leading glyph followed by whitespace.  Plain titles (`~/foo`, `bash`)
/// fail both checks.
fn title_decorative_glyph(title: &str) -> Option<char> {
    let trimmed = title.trim_start();
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if (first as u32) < 0x80 {
        return None;
    }
    if !chars.next().is_some_and(|c| c.is_whitespace()) {
        return None;
    }
    Some(first)
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

#[cfg(target_os = "linux")]
fn foreground_process_glyph(shell_pid: u32) -> Option<char> {
    let tpgid = read_tpgid(shell_pid)?;
    if tpgid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{tpgid}/comm")).ok();
    let cmdline = read_cmdline(tpgid as u32);
    let comm_trim = comm.as_deref().map(str::trim).unwrap_or("");

    // Match `comm` first (cheap), then anywhere in `cmdline` — picks up
    // `node /path/to/agent-cli.js`-style wrappers that hide behind their
    // runtime's name.
    let by_comm =
        AGENT_PROCESS_GLYPHS.iter().find(|(name, _)| comm_trim.starts_with(name)).map(|(_, g)| *g);
    if by_comm.is_some() {
        return by_comm;
    }
    if let Some(cmd) = &cmdline {
        let glyph =
            AGENT_PROCESS_GLYPHS.iter().find(|(name, _)| cmd.contains(name)).map(|(_, g)| *g);
        if glyph.is_some() {
            return glyph;
        }
        log::debug!("foreground process not matched: comm={comm_trim:?} cmdline={cmd:?}");
    }
    None
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
#[cfg(target_os = "linux")]
fn read_tpgid(shell_pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{shell_pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let after = &stat[close + 1..];
    // After `comm`: state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5).
    after.split_whitespace().nth(5)?.parse::<i32>().ok()
}

/// Windows has no foreground process group, so "foreground" is approximated
/// as *any* recognized agent in the shell's descendant tree.  This is what
/// the glyph means to the user — "an agent is running here" — and it stays
/// stable while agents run their own subprocesses, where a deepest-leaf
/// heuristic would flicker.
#[cfg(windows)]
fn foreground_process_glyph(shell_pid: u32) -> Option<char> {
    windows_process_probe::agent_glyph_under(shell_pid)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn foreground_process_glyph(_shell_pid: u32) -> Option<char> {
    // macOS would use `libproc::proc_pidfdinfo` / `tcgetpgrp` on the master
    // FD.  Not wired up yet.
    None
}

#[cfg(windows)]
mod windows_process_probe {
    //! Shared, throttled process-table snapshot.  Every session probes at
    //! its own `AGENT_CACHE_TTL` cadence; keeping one global `System` means
    //! N sessions cost one enumeration per tick, not N.  Two-phase refresh:
    //! names + parent pids for the whole table (one cheap system call
    //! class), command lines only for the shell's descendants and only when
    //! no name matched.
    use std::sync::{Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    use super::{agent_glyph_by_cmdline, agent_glyph_by_name, process_tree_pids};

    /// Slightly under `AGENT_CACHE_TTL` so the first session to tick
    /// refreshes and the rest reuse the same table.
    const SNAPSHOT_TTL: Duration = Duration::from_millis(900);

    static SNAPSHOT: Mutex<Option<(Instant, System)>> = Mutex::new(None);

    pub(super) fn agent_glyph_under(shell_pid: u32) -> Option<char> {
        let mut guard = SNAPSHOT.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.as_ref().is_none_or(|(at, _)| at.elapsed() >= SNAPSHOT_TTL) {
            let mut sys = guard.take().map(|(_, sys)| sys).unwrap_or_default();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            *guard = Some((Instant::now(), sys));
        }
        let (_, sys) = guard.as_mut().expect("snapshot populated above");

        let table: Vec<(u32, Option<u32>)> = sys
            .processes()
            .iter()
            .map(|(pid, p)| (pid.as_u32(), p.parent().map(|pp| pp.as_u32())))
            .collect();
        let tree = process_tree_pids(&table, shell_pid);
        let tree: Vec<Pid> = tree.into_iter().map(Pid::from_u32).collect();

        let names =
            tree.iter().filter_map(|pid| sys.process(*pid)).map(|p| p.name().to_string_lossy());
        if let Some(glyph) = agent_glyph_by_name(names) {
            return Some(glyph);
        }

        // Names missed: fetch command lines for just the tree to catch
        // agents launched through node/python shims.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&tree),
            false,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        let cmds = tree
            .iter()
            .filter_map(|pid| sys.process(*pid))
            .map(|p| p.cmd().iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        agent_glyph_by_cmdline(cmds)
    }
}

impl Session {
    pub fn spawn(
        ctx: egui::Context,
        config: &Config,
        working_directory: Option<PathBuf>,
        size: TermSize,
        cell_size: (f32, f32),
    ) -> std::io::Result<Self> {
        let shell = config.shell.as_ref().map(|s| Shell::new(s.program.clone(), s.args.clone()));
        let title = working_directory
            .as_ref()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "shell".to_string());
        Self::spawn_with(
            ctx,
            config,
            working_directory,
            size,
            cell_size,
            shell,
            title,
            SessionKind::Shell,
        )
    }

    /// Spawn a session running `program args` instead of the user's shell.
    /// Used by the git sidebar to drop into `delta` for an inline diff view —
    /// once the command exits, `reap_exited_sessions` removes the tab.
    pub fn spawn_command(
        ctx: egui::Context,
        config: &Config,
        working_directory: Option<PathBuf>,
        size: TermSize,
        cell_size: (f32, f32),
        program: String,
        args: Vec<String>,
        title: String,
        kind: SessionKind,
    ) -> std::io::Result<Self> {
        Self::spawn_with(
            ctx,
            config,
            working_directory,
            size,
            cell_size,
            Some(Shell::new(program, args)),
            title,
            kind,
        )
    }

    fn spawn_with(
        ctx: egui::Context,
        config: &Config,
        working_directory: Option<PathBuf>,
        size: TermSize,
        cell_size: (f32, f32),
        shell: Option<Shell>,
        title: String,
        kind: SessionKind,
    ) -> std::io::Result<Self> {
        let window_size = window_size(size, cell_size);

        let (proxy, events) = EventProxy::new(ctx);

        let term_config = TermConfig {
            scrolling_history: config.scrolling.history,
            default_cursor_style: config.cursor_style(),
            semantic_escape_chars: config.selection.semantic_escape_chars.clone(),
            ..TermConfig::default()
        };
        let term = Term::new(term_config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let pty_options = PtyOptions {
            shell,
            working_directory: working_directory.clone(),
            drain_on_exit: false,
            env: config.env.clone(),
            // Windows has no argv: alacritty_terminal joins these args into a
            // single CreateProcess command line, quoting them only when this
            // is set.  Diff panes pass argv built in code, where an arg with a
            // space (delta's pager spec, file paths) must survive as one
            // argument; shell args from alacritty.toml stay raw to match
            // upstream alacritty.
            #[cfg(windows)]
            escape_args: matches!(kind, SessionKind::Diff { .. }),
        };

        // alacritty routes OSC 7 / signals by this id, so each session needs its own.
        let window_id = next_window_id();
        let pty = tty::new(&pty_options, window_size, window_id)?;
        let shell_pid = pty_shell_pid(&pty);

        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();

        Ok(Self {
            id: next_session_id(),
            title,
            working_directory,
            kind,
            size,
            cell_size,
            term,
            events,
            needs_attention: false,
            accumulated_scroll: (0.0, 0.0),
            shell_pid,
            agent_cache: Cell::new(AgentCache::default()),
            notifier: Notifier(sender.clone()),
            sender,
            exited: false,
        })
    }

    pub fn write(&self, bytes: Vec<u8>) {
        self.notifier.notify(bytes);
    }

    /// Pull every pending event out of the PTY channel.  Called once per frame
    /// for every session — including background ones — so bells, title
    /// changes, and child-exits from non-visible sessions don't pile up.
    pub fn drain_events(&mut self) -> DrainOutcome {
        let mut outcome = DrainOutcome::default();
        while let Ok(event) = self.events.try_recv() {
            match event {
                TermEvent::PtyWrite(s) => self.write(s.into_bytes()),
                TermEvent::Title(t) => {
                    // A spinner-shaped title transitioning to a non-spinner one
                    // is how Claude Code (and similar tools that don't ring
                    // BEL) signal "done — your turn".  Treat it like a bell.
                    if is_spinner_title(&self.title) && !is_spinner_title(&t) {
                        outcome.attention = true;
                    }
                    self.title = t;
                },
                TermEvent::ChildExit(_) => self.exited = true,
                TermEvent::Bell => outcome.attention = true,
                _ => {},
            }
        }
        outcome
    }

    pub fn resize(&mut self, size: TermSize, cell_size: (f32, f32)) {
        if size.columns == self.size.columns
            && size.screen_lines == self.size.screen_lines
            && cell_size == self.cell_size
        {
            return;
        }
        self.size = size;
        self.cell_size = cell_size;
        let ws = window_size(size, cell_size);
        let _ = self.sender.send(Msg::Resize(ws));
        self.term.lock().resize(size);
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// Sidebar glyph for the agent running here.  Identity comes from the
    /// PTY's foreground process (`/proc` on Linux); the displayed glyph
    /// prefers the title's current leading char so the agent's own spinner
    /// frames animate for free, falling back to a per-agent static glyph
    /// when the title is plain ASCII.  When proc identification yields
    /// nothing, accept a decorative title as a permissive fallback so
    /// agents we don't have in the process map still show *something*.
    pub fn agent_glyph(&self) -> Option<char> {
        let proc_glyph = self.process_agent_glyph();
        let title_glyph = title_decorative_glyph(&self.title);
        if proc_glyph.is_some() {
            return title_glyph.or(proc_glyph);
        }
        title_glyph
    }

    fn process_agent_glyph(&self) -> Option<char> {
        let cached = self.agent_cache.get();
        let fresh = cached.polled_at.is_some_and(|t| t.elapsed() < AGENT_CACHE_TTL);
        if fresh {
            return cached.process_glyph;
        }
        let glyph = self.shell_pid.and_then(foreground_process_glyph);
        self.agent_cache.set(AgentCache { polled_at: Some(Instant::now()), process_glyph: glyph });
        glyph
    }

    pub fn shutdown(&self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn window_size(size: TermSize, cell_size: (f32, f32)) -> WindowSize {
    WindowSize {
        num_lines: size.screen_lines as u16,
        num_cols: size.columns as u16,
        cell_width: cell_size.0.max(1.0) as u16,
        cell_height: cell_size.1.max(1.0) as u16,
    }
}

fn next_window_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn next_session_id() -> SessionId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(agent_glyph_by_name(["pwsh.exe", "Claude.exe"]), Some('✳'));
        assert_eq!(agent_glyph_by_name(["cursor-agent.exe"]), Some('❖'));
        assert_eq!(agent_glyph_by_name(["pwsh.exe", "git.exe"]), None);
        assert_eq!(agent_glyph_by_name(["not-claude.exe"]), None);
        assert_eq!(agent_glyph_by_name(std::iter::empty::<&str>()), None);
    }

    #[test]
    fn cmdline_match_catches_runtime_wrappers() {
        let cmd =
            r"node C:\Users\lev\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\cli.js";
        assert_eq!(agent_glyph_by_cmdline([cmd]), Some('✳'));
        assert_eq!(agent_glyph_by_cmdline([r"pwsh.exe -NoLogo"]), None);
    }
}
