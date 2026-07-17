use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, Notify, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{ClipboardType, Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use alacritty_terminal::vte::ansi::Rgb;

use crate::clipboard::Target;
use crate::colors;
use crate::config::{Config, Palette};

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
    /// Last grid cell reported to a mouse-tracking app, so pointer motion emits
    /// at most one report per cell crossed instead of one per pixel.
    pub last_report_cell: Option<Point>,
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
    /// Whether anything is running in the terminal beyond the shell itself.
    foreground_job: bool,
    /// Whether a split-managing TUI (vim, tmux) is running in the terminal;
    /// see [`Session::nav_tui_running`].
    nav_tui: bool,
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

/// Plain-text dump of a session's grid for IPC clients.
pub struct ScreenSnapshot {
    /// Requested scrollback (top) followed by the full visible screen, one
    /// string per row, trailing blanks trimmed.
    pub lines: Vec<String>,
    /// Cursor row as an index into `lines`.
    pub cursor_line: usize,
    pub cursor_column: usize,
    /// Total scrollback rows available above the visible screen.
    pub history_size: usize,
}

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

/// TUIs that manage their own splits and cooperate with FocusLeft/
/// FocusRight: the key is forwarded while one runs, and the TUI calls
/// `alacritree action Focus…` over IPC once it has no window left in that
/// direction.  Matches Linux `comm` values (`tmux: client`) and Windows
/// image names (`nvim.exe`) alike; gvim stays out — it owns its own
/// window and never runs inside the terminal.
#[cfg(any(test, target_os = "linux", windows))]
fn is_nav_tui_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("nvim") || n.starts_with("vim") || n.starts_with("tmux")
}

/// `wsl.exe` (and its `wslhost`/`wslrelay` helpers) mark a session whose
/// real process tree lives on the Linux side, where this probe cannot see.
/// Assume the inside cooperates like a nav TUI: the key is forwarded, and
/// programs in the distro hand focus back by exec'ing the Windows CLI
/// (`alacritree.exe action Focus…`) through WSL interop.
#[cfg(any(test, windows))]
fn is_wsl_boundary_name(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("wsl")
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
    /// Text the app copied with OSC 52.  Carried out to the caller rather than
    /// written here so the drain — which runs once per frame for every session
    /// — stays free of OS clipboard access.
    pub clipboard: Vec<(Target, String)>,
}

/// Bytes answering an OSC colour query, or `None` when the query has no
/// answer and the sender should be left to its own default.  `format` is the
/// terminal's own response builder, so the reply carries whatever prefix and
/// string terminator the query arrived with.
fn color_query_reply(
    index: usize,
    format: &dyn Fn(Rgb) -> String,
    runtime: &Colors,
    palette: &Palette,
) -> Option<Vec<u8>> {
    let rgb = colors::query(index, runtime, palette)?;
    Some(format(rgb).into_bytes())
}

/// Bytes answering a CSI 14 t text-area-size query.  Fed the same geometry the
/// PTY was last resized with, so the pixel answer can't drift from the cell
/// grid the child already knows about.
fn text_area_size_reply(
    format: &dyn Fn(WindowSize) -> String,
    size: TermSize,
    cell_size: (f32, f32),
) -> Vec<u8> {
    format(window_size(size, cell_size)).into_bytes()
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

/// A session "looks busy" when its foreground process is a recognized
/// agent or its title is in a spinner state — the signal the sidebar's
/// close-confirmation policy keys on.
fn looks_busy(agent_glyph: Option<char>, title: &str) -> bool {
    agent_glyph.is_some() || is_spinner_title(title)
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

/// Windows has no foreground process group, so "a job is running" is
/// approximated as the shell having any descendant process — the same
/// approximation the agent glyph uses.
#[cfg(windows)]
fn shell_has_foreground_job(shell_pid: u32) -> bool {
    windows_process_probe::probe(shell_pid).1
}

#[cfg(not(any(target_os = "linux", windows)))]
fn shell_has_foreground_job(_shell_pid: u32) -> bool {
    // No probe wired (macOS would need `tcgetpgrp` on the PTY master).
    false
}

/// Windows has no foreground process group, so "foreground" is approximated
/// as *any* recognized agent in the shell's descendant tree.  This is what
/// the glyph means to the user — "an agent is running here" — and it stays
/// stable while agents run their own subprocesses, where a deepest-leaf
/// heuristic would flicker.
#[cfg(windows)]
fn foreground_process_glyph(shell_pid: u32) -> Option<char> {
    windows_process_probe::probe(shell_pid).0
}

#[cfg(not(any(target_os = "linux", windows)))]
fn foreground_process_glyph(_shell_pid: u32) -> Option<char> {
    // macOS would use `libproc::proc_pidfdinfo` / `tcgetpgrp` on the master
    // FD.  Not wired up yet.
    None
}

/// Whether a split-managing TUI owns the terminal: the process holding the
/// foreground group is one of the recognized names.
#[cfg(target_os = "linux")]
fn foreground_nav_tui(shell_pid: u32) -> bool {
    let Some(tpgid) = read_tpgid(shell_pid) else {
        return false;
    };
    if tpgid <= 0 {
        return false;
    }
    std::fs::read_to_string(format!("/proc/{tpgid}/comm"))
        .is_ok_and(|comm| is_nav_tui_name(comm.trim()))
}

/// Windows has no foreground process group, so a nav TUI anywhere in the
/// shell's descendant tree counts — the same approximation the agent
/// glyph uses.
#[cfg(windows)]
fn foreground_nav_tui(shell_pid: u32) -> bool {
    windows_process_probe::probe(shell_pid).2
}

#[cfg(not(any(target_os = "linux", windows)))]
fn foreground_nav_tui(_shell_pid: u32) -> bool {
    // Same gap as the glyph probe: macOS isn't wired up yet.
    false
}

/// Terminal options derived from the user config.
pub fn term_config(config: &Config) -> TermConfig {
    TermConfig {
        scrolling_history: config.scrolling.history,
        default_cursor_style: config.cursor_style(),
        semantic_escape_chars: config.selection.semantic_escape_chars.clone(),
        // `Term` drops every kitty keyboard request — push, pop, and the
        // support query — unless this is set, so without it an app never gets
        // to enable the protocol and modified keys stay legacy.  alacritty
        // enables it unconditionally too (config/ui_config.rs `term_options`).
        kitty_keyboard: true,
        ..TermConfig::default()
    }
}

/// A vanished cwd would otherwise surface as the PTY backend's raw error
/// (`os error 267`, "The directory name is invalid", on Windows) — reject it
/// up front with a message the error toast can show as-is.
fn ensure_working_directory(dir: Option<&Path>) -> std::io::Result<()> {
    match dir {
        Some(d) if !d.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("working directory no longer exists: {}", d.display()),
        )),
        _ => Ok(()),
    }
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

    use super::{
        agent_glyph_by_cmdline, agent_glyph_by_name, is_nav_tui_name, is_wsl_boundary_name,
        process_tree_pids,
    };

    /// Slightly under `AGENT_CACHE_TTL` so the first session to tick
    /// refreshes and the rest reuse the same table.
    const SNAPSHOT_TTL: Duration = Duration::from_millis(900);

    static SNAPSHOT: Mutex<Option<(Instant, System)>> = Mutex::new(None);

    /// Agent glyph found in the shell's descendant tree, whether the shell
    /// has any descendants at all, and whether one of them is a nav TUI.
    pub(super) fn probe(shell_pid: u32) -> (Option<char>, bool, bool) {
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
        let has_children = tree.len() > 1;
        let tree: Vec<Pid> = tree.into_iter().map(Pid::from_u32).collect();

        let names: Vec<String> = tree
            .iter()
            .filter_map(|pid| sys.process(*pid))
            .map(|p| p.name().to_string_lossy().into_owned())
            .collect();
        let nav_tui = names.iter().any(|n| is_nav_tui_name(n) || is_wsl_boundary_name(n));
        if let Some(glyph) = agent_glyph_by_name(&names) {
            return (Some(glyph), has_children, nav_tui);
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
        (agent_glyph_by_cmdline(cmds), has_children, nav_tui)
    }
}

impl Session {
    pub fn spawn(
        ctx: egui::Context,
        config: &Config,
        working_directory: Option<PathBuf>,
        size: TermSize,
        cell_size: (f32, f32),
        shell_override: Option<Shell>,
    ) -> std::io::Result<Self> {
        // Overrides are argv built in code (`wsl.exe -d <distro> --cd <dir>`),
        // so their args need Windows quoting like diff-pane argv; config
        // shells stay raw to match upstream alacritty.
        let escape_args = shell_override.is_some();
        let shell = shell_override.or_else(|| {
            config.shell.as_ref().map(|s| Shell::new(s.program.clone(), s.args.clone()))
        });
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
            escape_args,
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
            true,
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
        escape_args: bool,
    ) -> std::io::Result<Self> {
        ensure_working_directory(working_directory.as_deref())?;
        let window_size = window_size(size, cell_size);

        let (proxy, events) = EventProxy::new(ctx);

        let term = Term::new(term_config(config), &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let mut env = config.env.clone();
        if matches!(kind, SessionKind::Diff { .. }) {
            // git hands its pager `LESS=FRX`; both of those defaults hurt a diff
            // tab. `F` (quit-if-one-screen) makes delta's `less` exit the instant
            // a diff fits the pane, so the tab is reaped before it can be read.
            // `X` (no-init) keeps `less` off the alternate screen, and without
            // `ALT_SCREEN` the wheel loses its alternate-scroll arrow keys
            // (`terminal_view::apply_scroll`) and falls back to a scrollback that
            // `-X` repaints over instead of filling, leaving the pane unscrollable.
            // A `LESS` set by the user (via `[env]`) wins.
            env.entry("LESS".to_string()).or_insert_with(|| "R".to_string());
        }

        #[cfg(not(windows))]
        let _ = escape_args;
        let pty_options = PtyOptions {
            shell,
            working_directory: working_directory.clone(),
            drain_on_exit: false,
            env,
            // Windows has no argv: alacritty_terminal joins these args into a
            // single CreateProcess command line, quoting them only when this
            // is set.  True for argv built in code (diff panes, WSL shells),
            // where an arg with a space (delta's pager spec, UNC paths) must
            // survive as one argument; shell args from alacritty.toml stay
            // raw to match upstream alacritty.
            #[cfg(windows)]
            escape_args,
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
            last_report_cell: None,
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
    pub fn drain_events(&mut self, palette: &Palette) -> DrainOutcome {
        let mut outcome = DrainOutcome::default();
        while let Ok(event) = self.events.try_recv() {
            match event {
                // OSC 4 / 10 / 11 / 12.  Programs that ask the terminal for its
                // palette (delta, vim, terminal-colorsaurus) block on the reply,
                // so leaving the query unanswered costs them a timeout on every
                // run rather than degrading gracefully.  Answered here rather
                // than in apply_term_event, which stays free of the term lock
                // the live palette sits behind.
                TermEvent::ColorRequest(index, format) => {
                    let reply = color_query_reply(
                        index,
                        format.as_ref(),
                        self.term.lock().colors(),
                        palette,
                    );
                    if let Some(bytes) = reply {
                        self.write(bytes);
                    }
                },
                // CSI 14 t.  Image protocols and TUIs that size themselves in
                // pixels block on this the same way the color queries do.
                TermEvent::TextAreaSizeRequest(format) => {
                    let reply = text_area_size_reply(format.as_ref(), self.size, self.cell_size);
                    self.write(reply);
                },
                event => {
                    if let Some(bytes) =
                        apply_term_event(event, &mut self.title, &mut self.exited, &mut outcome)
                    {
                        self.write(bytes);
                    }
                },
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

    /// A session "looks busy" when a process is running in the terminal
    /// (a foreground job on Linux, any descendant of the shell on Windows),
    /// its foreground process is a recognized agent, or its title is in a
    /// spinner state — the signal the close-confirmation policy keys on.
    pub fn is_busy(&self) -> bool {
        self.process_probe().1 || looks_busy(self.agent_glyph(), &self.title)
    }

    fn process_agent_glyph(&self) -> Option<char> {
        self.process_probe().0
    }

    /// Whether a split-managing TUI (vim, tmux) is running in this terminal
    /// — the FocusLeft/FocusRight passthrough signal.  Identity comes from
    /// the process probe rather than the terminal title: a title-based
    /// signal needs every cooperating program to publish a recognizable
    /// value, and Windows' ConPTY interleaves the console title into the
    /// stream, so a launcher touching it after vim starts clobbers vim's
    /// own title until vim re-emits it.
    pub fn nav_tui_running(&self) -> bool {
        self.process_probe().2
    }

    fn process_probe(&self) -> (Option<char>, bool, bool) {
        let cached = self.agent_cache.get();
        let fresh = cached.polled_at.is_some_and(|t| t.elapsed() < AGENT_CACHE_TTL);
        if fresh {
            return (cached.process_glyph, cached.foreground_job, cached.nav_tui);
        }
        let glyph = self.shell_pid.and_then(foreground_process_glyph);
        let foreground_job = self.shell_pid.is_some_and(shell_has_foreground_job);
        let nav_tui = self.shell_pid.is_some_and(foreground_nav_tui);
        self.agent_cache.set(AgentCache {
            polled_at: Some(Instant::now()),
            process_glyph: glyph,
            foreground_job,
            nav_tui,
        });
        (glyph, foreground_job, nav_tui)
    }

    /// Text dump of the visible screen plus up to `scrollback_lines` of
    /// history above it.  Reads the live (unscrolled) screen regardless of
    /// the user's display offset so IPC clients always see where output and
    /// the cursor actually are.
    pub fn screen_snapshot(&self, scrollback_lines: usize) -> ScreenSnapshot {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;

        let term = self.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let screen_lines = grid.screen_lines() as i32;
        let history_size = grid.history_size();
        let back = scrollback_lines.min(history_size) as i32;

        let mut lines = Vec::with_capacity((back + screen_lines) as usize);
        for line_idx in -back..screen_lines {
            let row = &grid[Line(line_idx)];
            let mut text = String::with_capacity(cols);
            for col in 0..cols {
                let cell = &row[Column(col)];
                // Spacer cells are the second half of a wide glyph (or its
                // line-wrap placeholder) — the glyph itself was already pushed.
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let ch =
                    if cell.c == '\0' || cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
                text.push(ch);
                if let Some(zerowidth) = cell.zerowidth() {
                    text.extend(zerowidth);
                }
            }
            text.truncate(text.trim_end().len());
            lines.push(text);
        }

        let cursor = grid.cursor.point;
        ScreenSnapshot {
            lines,
            cursor_line: (cursor.line.0 + back).max(0) as usize,
            cursor_column: cursor.column.0,
            history_size,
        }
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

/// Apply one terminal event, returning any bytes owed back to the PTY.  Free
/// of `Session` so the classification stays testable without spawning a shell.
fn apply_term_event(
    event: TermEvent,
    title: &mut String,
    exited: &mut bool,
    outcome: &mut DrainOutcome,
) -> Option<Vec<u8>> {
    match event {
        TermEvent::PtyWrite(s) => return Some(s.into_bytes()),
        TermEvent::Title(t) => {
            // A spinner-shaped title transitioning to a non-spinner one
            // is how Claude Code (and similar tools that don't ring
            // BEL) signal "done — your turn".  Treat it like a bell.
            if is_spinner_title(title) && !is_spinner_title(&t) {
                outcome.attention = true;
            }
            *title = t;
        },
        TermEvent::ChildExit(_) => *exited = true,
        TermEvent::Bell => outcome.attention = true,
        // OSC 52.  Apps that copy this way (Claude Code, tmux, vim) get no
        // acknowledgement, so dropping it leaves them reporting a successful
        // copy while the system clipboard keeps its previous contents.
        TermEvent::ClipboardStore(ty, text) => outcome.clipboard.push((clipboard_target(ty), text)),
        _ => {},
    }
    None
}

fn clipboard_target(ty: ClipboardType) -> Target {
    match ty {
        ClipboardType::Clipboard => Target::Clipboard,
        ClipboardType::Selection => Target::Primary,
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
    use std::sync::Mutex;

    use alacritty_terminal::Term;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    use super::*;

    /// OSC 52 is how Claude Code, tmux and vim copy.  The sequence is
    /// fire-and-forget — the app reports a successful copy either way — so a
    /// dropped `ClipboardStore` shows up only as a stale paste later.  Drives
    /// the real sequence through a real terminal into the real drain.
    #[test]
    fn osc52_copy_is_carried_out_to_the_clipboard() {
        let (proxy, events) = EventProxy::new(egui::Context::default());
        let size = TermSize::new(80, 24);
        let mut term = Term::new(TermConfig::default(), &size, proxy);

        // `OSC 52 ; c ; <base64> BEL` — copy "hello" to the clipboard.
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b]52;c;aGVsbG8=\x07");

        let event = events.try_recv().expect("terminal emitted no event for OSC 52");
        let mut outcome = DrainOutcome::default();
        let mut title = String::new();
        let mut exited = false;
        apply_term_event(event, &mut title, &mut exited, &mut outcome);

        assert_eq!(outcome.clipboard, vec![(Target::Clipboard, "hello".to_owned())]);
    }

    /// The wheel scrolls a diff pane only because its pager sits on the alternate
    /// screen: `terminal_view::apply_scroll` emits arrow keys for `ALT_SCREEN |
    /// ALTERNATE_SCROLL` and otherwise falls back to a scrollback the pager
    /// repaints over rather than fills.  git hands its pager `LESS=FRX`, whose `X`
    /// (`--no-init`) suppresses that screen, so a diff pane's `LESS` must not carry
    /// it.  Drives a real pager through a real PTY and reads the negotiated mode.
    #[cfg(unix)]
    #[test]
    fn a_diff_panes_pager_runs_on_the_alternate_screen() {
        use alacritty_terminal::term::TermMode;

        let page =
            std::env::temp_dir().join(format!("alacritree-pager-probe-{}.txt", std::process::id()));
        // Longer than the pane, so the pager has a reason to stay up and page.
        let lines: String = (0..500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&page, lines).unwrap();

        // The app exports a capable TERM before any session can spawn
        // (`tty::setup_env` in `AlacritreeApp::new`), but this test spawns
        // without the app, so a bare runner's unset or `dumb` TERM would reach
        // the pager and `less` would never leave the primary screen. Inject
        // the fallback `setup_env` guarantees.
        let mut config = Config::default();
        config.env.insert("TERM".to_string(), "xterm-256color".to_string());

        let session = Session::spawn_command(
            egui::Context::default(),
            &config,
            std::env::current_dir().ok(),
            TermSize::new(80, 24),
            (8.0, 16.0),
            "less".to_string(),
            vec![page.to_string_lossy().into_owned()],
            "probe".to_string(),
            SessionKind::Diff { key: "probe".to_string() },
        )
        .unwrap();

        let start = Instant::now();
        while !session.term.lock().mode().contains(TermMode::ALT_SCREEN) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the pager never entered the alternate screen, so the wheel has nothing to \
                 scroll: check that a diff pane's LESS does not carry `X` (--no-init)"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Paired with ALT_SCREEN this is what `apply_scroll` keys off; it is on by
        // default, so a pager resetting it would silently break the wheel too.
        assert!(session.term.lock().mode().contains(TermMode::ALTERNATE_SCROLL));
    }

    /// A pane must not wait on the console host's startup handshake.
    ///
    /// `harden_dll_search_path` keeps `LoadLibraryW("conpty.dll")` off PATH.  Without
    /// it, any `conpty.dll` sitting in another terminal's install directory (WezTerm
    /// ships one) hosts our pseudoconsoles, and WezTerm's blocks the child process
    /// for three seconds waiting on a device-attributes reply that never satisfies
    /// it.  The child here prints and exits immediately, so a runtime anywhere near
    /// that timeout means a foreign console server is back in the loop.
    #[cfg(windows)]
    #[test]
    fn a_pane_runs_its_child_without_a_console_host_handshake() {
        crate::harden_dll_search_path();

        let start = Instant::now();
        let session = Session::spawn_command(
            egui::Context::default(),
            &Config::default(),
            std::env::current_dir().ok(),
            TermSize::new(80, 24),
            (8.0, 16.0),
            "cmd".to_string(),
            vec!["/c".to_string(), "echo".to_string(), "ready".to_string()],
            "probe".to_string(),
            SessionKind::Shell,
        )
        .unwrap();

        let exited = loop {
            assert!(start.elapsed() < Duration::from_secs(10), "child never exited");
            match session.events.try_recv() {
                Ok(TermEvent::ChildExit(_)) => break start.elapsed(),
                Ok(_) => {},
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        };

        assert!(
            exited < Duration::from_secs(2),
            "`cmd /c echo ready` took {exited:?}; the console host is stalling on a \
             handshake (the foreign conpty.dll stall is ~3s)"
        );
    }

    #[derive(Default)]
    struct Collector(Mutex<Vec<TermEvent>>);

    impl EventListener for &Collector {
        fn send_event(&self, event: TermEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// Drive `bytes` through a real VT parser and return the reply the session
    /// would put back on the PTY for the colour query they contain.
    fn reply_to(bytes: &[u8], palette: &Palette) -> Option<Vec<u8>> {
        let collector = Collector::default();
        let size = TermSize::new(80, 24);
        let mut term = Term::new(TermConfig::default(), &size, &collector);
        Processor::<StdSyncHandler>::new().advance(&mut term, bytes);

        let events = collector.0.lock().unwrap();
        events.iter().find_map(|event| match event {
            TermEvent::ColorRequest(index, format) => {
                Some(color_query_reply(*index, format.as_ref(), term.colors(), palette))
            },
            _ => None,
        })?
    }

    fn expected(prefix: &str, rgb: Rgb) -> Vec<u8> {
        format!(
            "\x1b]{prefix};rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}\x07",
            rgb.r, rgb.g, rgb.b
        )
        .into_bytes()
    }

    /// Drive `bytes` through a real VT parser and return the reply the session
    /// would put back on the PTY for the text-area-size query they contain.
    fn size_reply_to(bytes: &[u8], size: TermSize, cell_size: (f32, f32)) -> Option<Vec<u8>> {
        let collector = Collector::default();
        let mut term = Term::new(TermConfig::default(), &size, &collector);
        Processor::<StdSyncHandler>::new().advance(&mut term, bytes);

        let events = collector.0.lock().unwrap();
        events.iter().find_map(|event| match event {
            TermEvent::TextAreaSizeRequest(format) => {
                Some(text_area_size_reply(format.as_ref(), size, cell_size))
            },
            _ => None,
        })
    }

    #[test]
    fn csi14t_size_query_is_answered_in_pixels() {
        let reply = size_reply_to(b"\x1b[14t", TermSize::new(80, 24), (7.0, 15.0));
        assert_eq!(reply, Some(b"\x1b[4;360;560t".to_vec()));
    }

    #[test]
    fn osc11_background_query_is_answered_from_the_palette() {
        let palette = Palette::default();
        assert_eq!(reply_to(b"\x1b]11;?\x07", &palette), Some(expected("11", palette.bg)));
    }

    #[test]
    fn osc10_foreground_query_is_answered_from_the_palette() {
        let palette = Palette::default();
        assert_eq!(reply_to(b"\x1b]10;?\x07", &palette), Some(expected("10", palette.fg)));
    }

    #[test]
    fn osc4_indexed_query_is_answered_from_the_palette() {
        let palette = Palette::default();
        assert_eq!(reply_to(b"\x1b]4;1;?\x07", &palette), Some(expected("4;1", palette.normal[1])));
    }

    #[test]
    fn a_color_the_app_set_at_runtime_wins_over_the_palette() {
        let palette = Palette::default();
        let red = Rgb { r: 0xff, g: 0x00, b: 0x00 };
        let reply = reply_to(b"\x1b]11;rgb:ff/00/00\x07\x1b]11;?\x07", &palette);
        assert_eq!(reply, Some(expected("11", red)));
    }

    #[test]
    fn an_unset_cursor_color_is_left_unanswered() {
        assert_eq!(reply_to(b"\x1b]12;?\x07", &Palette::default()), None);
    }

    #[test]
    fn busy_when_agent_glyph_present() {
        assert!(looks_busy(Some('✳'), "plain title"));
    }

    #[test]
    fn busy_when_title_is_spinner() {
        assert!(looks_busy(None, "⠋ Thinking…"));
    }

    #[test]
    fn idle_when_no_glyph_and_plain_title() {
        assert!(!looks_busy(None, "~/projects/alacritree"));
        assert!(!looks_busy(None, ""));
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
    fn missing_dir_is_a_readable_error() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone");
        let err = ensure_working_directory(Some(&gone)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("no longer exists"));
    }

    #[test]
    fn none_and_existing_dirs_pass() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ensure_working_directory(None).is_ok());
        assert!(ensure_working_directory(Some(tmp.path())).is_ok());
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
        assert_eq!(agent_glyph_by_name(["pwsh.exe", "Claude.exe"]), Some('✳'));
        assert_eq!(agent_glyph_by_name(["cursor-agent.exe"]), Some('❖'));
        assert_eq!(agent_glyph_by_name(["pwsh.exe", "git.exe"]), None);
        assert_eq!(agent_glyph_by_name(["not-claude.exe"]), None);
        assert_eq!(agent_glyph_by_name(std::iter::empty::<&str>()), None);
    }

    #[test]
    fn nav_tui_name_match_covers_both_platforms_naming() {
        // Windows image names.
        assert!(is_nav_tui_name("nvim.exe"));
        assert!(is_nav_tui_name("NVIM.EXE"));
        assert!(is_nav_tui_name("vim.exe"));
        // Linux comm values.
        assert!(is_nav_tui_name("nvim"));
        assert!(is_nav_tui_name("tmux: client"));
        // gvim owns its own window — it never runs inside the terminal.
        assert!(!is_nav_tui_name("gvim.exe"));
        assert!(!is_nav_tui_name("chezmoi.exe"));
        assert!(!is_nav_tui_name("pwsh.exe"));
    }

    #[test]
    fn wsl_boundary_match_covers_the_helper_processes() {
        assert!(is_wsl_boundary_name("wsl.exe"));
        assert!(is_wsl_boundary_name("wslhost.exe"));
        assert!(is_wsl_boundary_name("WSLRELAY.EXE"));
        assert!(!is_wsl_boundary_name("pwsh.exe"));
    }

    #[test]
    fn cmdline_match_catches_runtime_wrappers() {
        let cmd =
            r"node C:\Users\lev\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\cli.js";
        assert_eq!(agent_glyph_by_cmdline([cmd]), Some('✳'));
        assert_eq!(agent_glyph_by_cmdline([r"pwsh.exe -NoLogo"]), None);
    }
}
