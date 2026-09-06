use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::tty::Shell;
use eframe::CreationContext;
use egui::{Color32, Context, Frame, Margin, RichText, ScrollArea, SidePanel, Stroke};

use serde_json::{Value, json};

use crate::bindings::{self, BindingAction, KeyBinding, NamedAction};
use crate::clipboard::{self, Target};
use crate::colors::rgb_to_color32;
use crate::command_palette::{self, CommandPalette, PaletteAction, PaletteItem};
use crate::config::{
    BakedGlyph, Config, DEFAULT_ADD_ICON, DEFAULT_AGENT_ICON, DEFAULT_BLOCKED_ICON,
    DEFAULT_CLOSE_ICON, DEFAULT_HERDR_ICON, DEFAULT_HOME_ICON, DEFAULT_PR_CLOSED_ICON,
    DEFAULT_PR_DRAFT_ICON, DEFAULT_PR_MERGED_ICON, DEFAULT_PR_OPEN_ICON,
    DEFAULT_PROJECT_COLLAPSED_ICON, DEFAULT_PROJECT_EXPANDED_ICON, DEFAULT_REFRESH_ICON,
    DEFAULT_REORDER_ICON, DEFAULT_SEARCH_ICON, DEFAULT_SESSION_ICON,
    DEFAULT_UPSTREAM_DIVERGED_ICON, DEFAULT_UPSTREAM_GONE_ICON, DEFAULT_UPSTREAM_LEVEL_ICON,
    DEFAULT_UPSTREAM_UNTRACKED_ICON, DEFAULT_WORKTREE_ICON, DEFAULT_WORKTREE_MAIN_ICON, FontConfig,
    IconStyle, Icons, LastSessionClose, PathStyleConfig, ScrollbarStyle, SearchScope, SidebarFocus,
    SidebarTooltips, TextEmphasis, UiFont, UiTheme, profile_command,
};
use crate::crash_log::{self, ExitReason};
use crate::git_nav::{self, GitSection, SectionCount};
use crate::git_status::{self, ChangeKind, DirtyCounts, FileChange, GitStatus, StatusCache};
use crate::panel_filter::{self, PanelFilter};
use crate::path_style::PathStyle;
use crate::pending_spawn::{Finished, PendingSpawns};
use crate::pr_status::{self, PrCache, PrInfo, PrState};
use crate::projects::{Project, Worktree, project_json};
use crate::session::{
    self, AttentionVerdict, LiveState, Session, SessionActivity, SessionId, SessionKind, TermSize,
    poll_attention_debounce,
};
use crate::sidebar_nav::{self, SidebarRow, StepTarget};
use crate::state::{self, PersistedProject};
use crate::upstream::UpstreamState;
use crate::worktree::{self as wt, CreateRequest, Progress};
use crate::wsl::{self, ShellChoice};
use crate::wsl_helper::{self, WslProbe};
use crate::{
    clipboard_image, doppler, file_drop, herdr, ipc, jobs, paste, path_style, scratchpad,
    sidebar_focus, terminal_view, worktree_liveness,
};

/// `None` is the home workspace (sessions inherit `$PWD`); `Some` is a worktree path.
pub type WorkspaceKey = Option<PathBuf>;

/// Channel from notification-worker threads back to the app.  Set once by
/// `AlacritreeApp::new`; each worker reads it to deliver the session the
/// user clicked on.  Static because the worker has no other handle to the
/// app and there's only ever one app instance per process.
static NOTIFY_TX: OnceLock<Mutex<Sender<SessionId>>> = OnceLock::new();

#[derive(Clone, Copy)]
struct FocusOutlineTheme {
    sidebar: bool,
    terminal: bool,
    color: Color32,
    thickness: f32,
}

#[derive(Clone, Copy)]
struct Theme {
    terminal_bg: Color32,
    sidebar_bg: Color32,
    sidebar_border: Color32,
    row_hover_bg: Color32,
    row_active_bg: Color32,
    text: Color32,
    text_dim: Color32,
    text_muted: Color32,
    accent: Color32,
    /// "Needs attention" highlight.  Distinct from `accent` ("active
    /// workspace") so the two signals don't read as the same thing.
    attention: Color32,
    /// PR badge colors, mapped to GitHub's conventions from the ANSI palette.
    pr_open: Color32,
    pr_draft: Color32,
    pr_merged: Color32,
    pr_closed: Color32,
    /// Branch upstream badge colors, mapped from the ANSI palette.
    upstream_level: Color32,
    upstream_diverged: Color32,
    upstream_gone: Color32,
    upstream_untracked: Color32,
    /// Colors for a harness's own state vocabulary, mapped from the ANSI
    /// palette the way the PR and upstream badges are.
    harness_state: StateColors,
    /// Logical-pixel size for headings (titles like "Projects", "Git").
    /// `FontConfig::UI_HEADING_RATIO` of the terminal font size.
    font_heading: f32,
    /// Logical-pixel size for normal UI text (rows, captions, button labels).
    /// `FontConfig::UI_NORMAL_RATIO` of the terminal font size — keeps the
    /// chrome secondary to the grid.
    font_normal: f32,
    /// Multiplier applied to hard-coded UI sizes (icons, paddings, modal
    /// widths) so the chrome scales with `font.size`.  Anchored to the
    /// historical 11.25-logical-pixel baseline so unmodified config keeps the
    /// existing layout proportions.
    ui_scale: f32,
    focus_outline: FocusOutlineTheme,
    /// Per-site path abbreviation, so free-standing row painters can spell a
    /// path without taking a `&Config`.
    path_style: PathStyleConfig,
    /// When a row spells its full name out on hover.
    sidebar_tooltips: SidebarTooltips,
    /// Whether a sidebar button says what it does on hover.
    icon_tooltips: bool,
    /// Where a row a sidebar scrolled to is parked; `None` is egui's own
    /// minimal scroll.
    scroll_align: Option<egui::Align>,
}

/// One color per [`StateTone`], since a harness names its states rather than
/// its colors and alacritree owns the palette they land in.
#[derive(Debug, Clone, Copy)]
struct StateColors {
    blocked: Color32,
    working: Color32,
    done: Color32,
    idle: Color32,
    unclear: Color32,
}

impl StateColors {
    fn of(&self, tone: StateTone) -> Color32 {
        match tone {
            StateTone::Blocked => self.blocked,
            StateTone::Working => self.working,
            StateTone::Done => self.done,
            StateTone::Idle => self.idle,
            StateTone::Unclear => self.unclear,
        }
    }
}

/// Logical-pixel (normal, heading) sizes for UI text.  `[ui.font] size`
/// overrides the normal size directly (same pt→px conversion as
/// `FontConfig::egui_size`); the heading keeps its existing ratio to normal
/// text.  Unset, both fall back to the `[font]`-derived values unchanged.
fn ui_text_px(font: &FontConfig, ui_font: &UiFont) -> (f32, f32) {
    match ui_font.size {
        Some(pt) => {
            let normal = pt * 96.0 / 72.0;
            let heading = normal * (FontConfig::UI_HEADING_RATIO / FontConfig::UI_NORMAL_RATIO);
            (normal, heading)
        },
        None => (font.ui_normal_px(), font.ui_heading_px()),
    }
}

impl Theme {
    fn from_config(config: &Config) -> Self {
        let terminal_bg = rgb_to_color32(config.palette.bg);
        let sidebar_bg = config.ui.sidebar_background.unwrap_or(terminal_bg);
        let text =
            config.ui.sidebar_foreground.unwrap_or_else(|| rgb_to_color32(config.palette.fg));
        let accent =
            config.ui.sidebar_accent.unwrap_or_else(|| rgb_to_color32(config.palette.normal[4])); // ANSI blue
        let attention =
            config.ui.sidebar_attention.unwrap_or_else(|| rgb_to_color32(config.palette.normal[3])); // ANSI yellow
        let border = config.ui.sidebar_border.unwrap_or_else(|| lighten(sidebar_bg, 0.10));
        let text_muted = blend_toward(text, sidebar_bg, 0.55);
        let (font_normal, font_heading) = ui_text_px(&config.font, &config.ui_font);
        Self {
            terminal_bg,
            sidebar_bg,
            sidebar_border: border,
            row_hover_bg: lighten(sidebar_bg, 0.05),
            row_active_bg: lighten(sidebar_bg, 0.10),
            text,
            text_dim: blend_toward(text, sidebar_bg, 0.35),
            text_muted,
            accent,
            attention,
            pr_open: rgb_to_color32(config.palette.normal[2]), // green
            pr_draft: text_muted,
            pr_merged: rgb_to_color32(config.palette.normal[5]), // magenta
            pr_closed: rgb_to_color32(config.palette.normal[1]), // red
            upstream_level: rgb_to_color32(config.palette.normal[2]), // green
            upstream_diverged: rgb_to_color32(config.palette.normal[3]), // yellow
            upstream_gone: rgb_to_color32(config.palette.normal[1]), // red
            upstream_untracked: rgb_to_color32(config.palette.normal[4]), // blue
            harness_state: StateColors {
                blocked: rgb_to_color32(config.palette.normal[1]), // red
                working: rgb_to_color32(config.palette.normal[3]), // yellow
                done: rgb_to_color32(config.palette.normal[6]),    // cyan, herdr's teal
                idle: rgb_to_color32(config.palette.normal[2]),    // green
                unclear: text_muted,
            },
            font_heading,
            font_normal,
            ui_scale: font_normal / 11.25,
            focus_outline: FocusOutlineTheme {
                sidebar: config.ui.focus_outline.sidebar,
                terminal: config.ui.focus_outline.terminal,
                color: config.ui.focus_outline.color.unwrap_or(accent),
                thickness: config.ui.focus_outline.thickness,
            },
            path_style: config.ui.path_style,
            sidebar_tooltips: config.ui.sidebar_tooltips,
            icon_tooltips: config.ui.icon_tooltips,
            scroll_align: config.ui.sidebar_scroll_align.align(),
        }
    }
}

fn lighten(c: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |x: u8| -> u8 {
        let v = x as f32;
        (v + (255.0 - v) * amount).round().clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

fn paint_panel_border(ctx: &Context, x: f32, y_range: egui::Rangef, color: Color32) {
    // `Middle` keeps the line above the panel content (`Background`) but below
    // modals, popups, and tooltips (`Foreground`/`Tooltip`) — otherwise the
    // border bleeds through whatever modal is open.
    let layer =
        egui::LayerId::new(egui::Order::Middle, egui::Id::new(("sidebar_border", x.to_bits())));
    ctx.layer_painter(layer).vline(x, y_range, Stroke::new(1.0_f32, color));
}

fn paint_focus_outline(ctx: &Context, rect: egui::Rect, theme: &Theme) {
    let fo = theme.focus_outline;
    let layer = egui::LayerId::new(
        egui::Order::Middle,
        egui::Id::new(("focus_outline", rect.min.x.to_bits())),
    );
    ctx.layer_painter(layer).rect_stroke(
        rect,
        0.0,
        Stroke::new(fo.thickness, fo.color),
        egui::StrokeKind::Inside,
    );
}

/// A primary press landed on the panel itself: inside its rect with no
/// floating layer (modal, window, context menu) above the press position.
/// `layer_id_at` resolves only floating `Area` layers — `None` means the
/// press reached the background panels — and while a modal is open egui
/// resolves *every* position to the modal's layer, so presses never register
/// here until the modal closes.
fn pressed_on_panel(ctx: &Context, resp: &egui::Response) -> bool {
    let (pressed, origin) = ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.press_origin()));
    pressed
        && origin.is_some_and(|pos| {
            resp.rect.contains(pos) && ctx.layer_id_at(pos).is_none_or(|l| l == resp.layer_id)
        })
}

fn blend_toward(c: Color32, target: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| -> u8 {
        let av = a as f32;
        let bv = b as f32;
        (av + (bv - av) * amount).round().clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(mix(c.r(), target.r()), mix(c.g(), target.g()), mix(c.b(), target.b()))
}

/// Which pane owns keyboard input.  The terminal re-requests egui focus
/// every frame while it owns this; anything else holding focus (modals
/// aside) must win here first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneFocus {
    Terminal,
    ProjectsSidebar,
    GitSidebar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusDir {
    Left,
    Right,
}

/// Where a dispatched binding action came from.  A keyboard action consumed
/// a real key press, so FocusLeft/FocusRight may re-synthesize it into the
/// PTY when the inner TUI should handle it.  An IPC action has no key press
/// to forward — the caller is typically that inner program declaring it has
/// no window in the requested direction, and passthrough would bounce the
/// key straight back to it.  A palette action consumed a key press too, but
/// arrives with the panel still searching over a row the query may have
/// hidden — so actions that need a browsing cursor are refused at this origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOrigin {
    Keyboard,
    Palette,
    Ipc,
}

/// What a FocusLeft/FocusRight press does, decided by [`focus_move`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusMove {
    /// The TUI inside the terminal can still move that way — forward the
    /// Ctrl+Arrow to the PTY instead of switching panels.
    Passthrough,
    Focus(PaneFocus),
    Nothing,
}

/// Panel-focus decision for FocusLeft/FocusRight.  Panels sit in a fixed
/// `ProjectsSidebar ↔ Terminal ↔ GitSidebar` row; movement toward a hidden
/// panel is dropped (focus never opens a panel).  From the terminal, a
/// keyboard-originated move is forwarded to a running split-managing TUI
/// (`tui_running`, see [`Session::nav_tui_running`]): the TUI walks its own
/// splits and hands focus back with `alacritree action Focus…` once it has
/// no window left in that direction — which is why IPC moves never pass
/// through (see [`ActionOrigin`]).
fn focus_move(
    focus: PaneFocus,
    dir: FocusDir,
    left_open: bool,
    right_open: bool,
    origin: ActionOrigin,
    tui_running: bool,
) -> FocusMove {
    if origin != ActionOrigin::Ipc && focus == PaneFocus::Terminal && tui_running {
        return FocusMove::Passthrough;
    }
    let target = match (focus, dir) {
        (PaneFocus::Terminal, FocusDir::Left) => left_open.then_some(PaneFocus::ProjectsSidebar),
        (PaneFocus::Terminal, FocusDir::Right) => right_open.then_some(PaneFocus::GitSidebar),
        (PaneFocus::ProjectsSidebar, FocusDir::Right) => Some(PaneFocus::Terminal),
        (PaneFocus::GitSidebar, FocusDir::Left) => Some(PaneFocus::Terminal),
        _ => None,
    };
    match target {
        Some(t) => FocusMove::Focus(t),
        None => FocusMove::Nothing,
    }
}

/// Whether a matched binding's key press should reach `action`, given which
/// pane currently owns keyboard focus. Filter actions are scoped to the
/// sidebar that owns them so a bare letter like `d` doesn't fire a git-panel
/// filter while the projects sidebar (or the terminal) has focus, and vice
/// versa. `terminal_only` actions additionally step aside for the scratchpad
/// editor, which wants those same keys for native text editing.
fn valid_for_focus(
    action: &BindingAction,
    sidebar_focused: bool,
    git_focused: bool,
    scratchpad_focused: bool,
) -> bool {
    let focus_ok = match action {
        BindingAction::Named(n) if n.is_projects_filter_scoped() => sidebar_focused,
        BindingAction::Named(n) if n.is_git_filter_scoped() => git_focused,
        BindingAction::Named(n) if n.is_sidebar_scoped() => sidebar_focused,
        _ => true,
    };
    let terminal_only = match action {
        BindingAction::Chars(_) => true,
        BindingAction::Named(n) => n.is_terminal_only(),
        BindingAction::Unsupported(_) => false,
    };
    focus_ok && !(scratchpad_focused && terminal_only)
}

/// Whether a workspace survives the projects panel's toggle dimension.
fn project_toggles_pass(
    apply: bool,
    toggle_sessions: bool,
    has_sessions: bool,
    toggle_attention: bool,
    needs_attention: bool,
) -> bool {
    if !apply {
        return true;
    }
    (!toggle_sessions || has_sessions) && (!toggle_attention || needs_attention)
}

/// The toggle identities the projects panel accepts.  The PR identities exist
/// only when polling does, or every PR state would read as unknown and the
/// filters could only ever empty the panel.
fn project_filter_toggles(pr_status: bool) -> &'static [char] {
    if pr_status { &['s', 'a', 'o', 'd', 'm', 'c'] } else { &['s', 'a'] }
}

/// The toggle identities the git panel accepts: modified, deleted, untracked.
const GIT_FILTER_TOGGLES: &[char] = &['m', 'd', 'u'];

/// How long after the user's last event the worktree liveness tick keeps
/// asking for frames.  Long enough that a `git worktree remove` typed at a
/// prompt finishes and greys its row; short enough that a window left open
/// goes back to producing no frames at all.
const PROBE_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// The projects-panel toggle a named action flips, or `None` for an action that
/// is not one of its filters.  `PanelFilter::toggle` ignores an identity it does
/// not allow and dispatch falls through on an unmatched action, so nothing at
/// the call site can catch a wrong pairing — assert it here instead.
fn project_filter_identity(action: NamedAction) -> Option<char> {
    match action {
        NamedAction::ToggleSessionsFilter => Some('s'),
        NamedAction::ToggleAttentionFilter => Some('a'),
        NamedAction::TogglePrOpenFilter => Some('o'),
        NamedAction::TogglePrDraftFilter => Some('d'),
        NamedAction::TogglePrMergedFilter => Some('m'),
        NamedAction::TogglePrClosedFilter => Some('c'),
        _ => None,
    }
}

/// The git-panel toggle a named action flips, or `None` for an action that is
/// not one of its filters.
fn git_filter_identity(action: NamedAction) -> Option<char> {
    match action {
        NamedAction::ToggleModifiedFilter => Some('m'),
        NamedAction::ToggleDeletedFilter => Some('d'),
        NamedAction::ToggleUntrackedFilter => Some('u'),
        _ => None,
    }
}

/// Whether any toggle dimension narrows the projects panel this frame —
/// session presence, attention, or PR state. `project_self` falls back to
/// plain fuzzy matching only when this is false.
fn any_project_toggle_active(toggle_sessions: bool, toggle_attention: bool, any_pr: bool) -> bool {
    toggle_sessions || toggle_attention || any_pr
}

/// Whether a worktree survives the projects panel's PR dimension. Inert
/// when no PR toggle is active, so a worktree passes regardless of what
/// `pr_matches` holds for it. Once a PR toggle is active, a worktree
/// missing from `pr_matches` is excluded — its PR lookup hasn't landed.
fn worktree_pr_passes(any_pr: bool, pr_matches: &HashMap<PathBuf, bool>, path: &Path) -> bool {
    !any_pr || pr_matches.get(path).copied().unwrap_or(false)
}

/// Whether the projects panel is filtering on PR state this frame.  A toggle
/// the scope has stood down narrows nothing, so it must not pull the cache
/// generation into the reconciler or reach `gh` for a collapsed project.
fn any_pr_toggle_active(filter: &PanelFilter, scope: SearchScope) -> bool {
    filter.toggles_apply(scope)
        && ['o', 'd', 'm', 'c'].into_iter().any(|key| filter.is_toggled(key))
}

/// The cache generation the reconciler observes.  Held at `0` unless a PR
/// filter is active, so a banked result only invalidates a row set that
/// actually depends on PR state.
fn pr_generation_for(generation: u64, any_pr_toggle_active: bool) -> u64 {
    if any_pr_toggle_active { generation } else { 0 }
}

/// Whether this worktree's PR state is polled this frame.  Collapsed projects
/// normally cost no `gh` processes, but a PR filter has to see every row or it
/// would hide worktrees for want of a lookup it declined to start.
fn should_poll_pr(pr_enabled: bool, expanded: bool, any_pr_toggle: bool) -> bool {
    pr_enabled && (expanded || any_pr_toggle)
}

/// Resolves one worktree's PR info against this frame's memo. `lookup` runs
/// at most once per distinct `path`: a repeated path (the same worktree under
/// two projects) reuses the banked answer instead of polling `PrCache` twice.
fn resolve_pr_info<F>(
    memo: &mut HashMap<PathBuf, Option<PrInfo>>,
    path: &Path,
    eligible: bool,
    lookup: F,
) -> Option<PrInfo>
where
    F: FnOnce() -> Option<PrInfo>,
{
    if !eligible {
        return None;
    }
    if let Some(cached) = memo.get(path) {
        return cached.clone();
    }
    let info = lookup();
    memo.insert(path.to_path_buf(), info.clone());
    info
}

/// Whether a git-status row survives the git panel's toggle dimension. Unlike
/// `project_toggles_pass`, standing this down needs no separate `apply` flag:
/// forcing all three toggles to `false` already makes `!any` admit every row.
fn git_toggles_pass(m: bool, d: bool, u: bool, kind: ChangeKind) -> bool {
    let any = m || d || u;
    !any || (m && matches!(kind, ChangeKind::Modified | ChangeKind::Renamed))
        || (d && kind == ChangeKind::Deleted)
        || (u && matches!(kind, ChangeKind::Untracked | ChangeKind::Added))
}

pub struct AlacritreeApp {
    show_left_sidebar: bool,
    show_right_sidebar: bool,
    focus: PaneFocus,
    /// Runtime copies of `[ui.session_display]`.  The config is only the
    /// startup default; toggles flip these and are never persisted.
    session_rows_always: bool,
    session_tabs_always: bool,
    /// Runtime copy of `[ui.session_reorder] drag`.  Like the display toggles
    /// above, the config is only the startup default and nothing is persisted.
    session_drag: bool,
    sidebar_cursor: Option<SidebarRow>,
    /// Reveals the project rows' drag grips.  A transient mode, not persisted:
    /// reordering is a rare, deliberate act, and a grip on every row the rest
    /// of the time is noise.
    reorder_mode: bool,
    /// The focus toggle opened a hidden sidebar; returning focus closes it
    /// again so a keyboard round trip leaves the layout untouched.
    sidebar_auto_shown: bool,
    /// One-shot: scroll the cursor row into view on the next sidebar paint.
    sidebar_cursor_moved: bool,
    /// The workspace and session the projects panel last scrolled to, so a
    /// change is detected by comparison rather than by every writer of those
    /// two fields remembering to raise a flag.  Written only once a scroll
    /// actually fires, so a change whose row renders nowhere is retried.
    last_followed: (WorkspaceKey, Option<SessionId>),
    /// Fuzzy-search query and `s`/`a` toggle state for the projects panel.
    /// Transient: never persisted, never touches the `expanded` flag.
    project_filter: PanelFilter,
    /// Fuzzy-search query and `m`/`d`/`u` change-kind toggle state for the git
    /// panel.  Transient: never persisted.
    git_filter: PanelFilter,
    /// `[ui] search_scope`: whether a live query stands down both panels'
    /// toggle filters.  Toggled at runtime, never persisted.
    search_scope: SearchScope,
    /// Git-panel cursor, identified by `(section, path)`.  Rebuilt every render
    /// pass from `git_rows`, so it survives the 1.5 s status refresh.
    git_cursor: Option<git_nav::GitRow>,
    /// One-shot: scroll the git cursor row into view on the next paint.
    git_cursor_moved: bool,
    /// Render-order git rows the cursor steps over, refreshed by the render pass.
    git_rows: Vec<git_nav::GitRow>,
    /// Resolved default-branch ref backing the git panel's branch-diff rows,
    /// refreshed by the render pass so Enter opens the same diff a click would.
    git_branch_base: Option<String>,
    /// The focus toggle opened a hidden git sidebar; returning focus closes it
    /// again so a keyboard round trip leaves the layout untouched.
    git_sidebar_auto_shown: bool,
    /// The Ctrl+K command palette (query, selection, matcher). Transient:
    /// never persisted.
    palette: CommandPalette,
    sessions: Vec<Session>,
    current_workspace: WorkspaceKey,
    active_session: HashMap<WorkspaceKey, SessionId>,
    projects: Vec<Project>,
    git_status: HashMap<PathBuf, StatusCache>,
    /// Per-worktree override of the git panel's diff base, keyed by worktree
    /// path.  Mirrors `state.toml`; written through `state::set_base_branch`.
    base_branch_overrides: HashMap<PathBuf, String>,
    pr_cache: PrCache,
    /// Renders `[ui] worktree_name` / `project_name` templates at paint time.
    row_labels: crate::row_label::LabelTemplates,
    config: Config,
    theme: Theme,
    /// A modal popup carrying a failure message the user must dismiss.  Every
    /// failure that has no inline home lands here — a background action (e.g. a
    /// worktree delete) that failed after its dialog closed, or a shell that
    /// would not spawn.  Dismissing it leaves the app usable, which an error
    /// painted over the terminal would not.
    error_dialog: Option<String>,
    quit_dialog_open: bool,
    pending_delete: Option<DeleteRequest>,
    /// Confirmed deletes whose git removal is running off-thread; polled and
    /// adopted in `poll_pending_deletes`.
    pending_deletes: Vec<DeleteTask>,
    pending_create: Option<CreateState>,
    /// Creations the user minimized off the running modal; they keep streaming
    /// off-thread and are adopted in `poll_pending_creates`.
    pending_creates: Vec<BackgroundCreate>,
    pending_rename: Option<RenameState>,
    /// The base-branch picker modal.  Transient: never persisted.
    pending_base_branch: Option<BaseBranchPicker>,
    pending_project_remove: Option<ProjectRemoveState>,
    /// Worktrees already given a Doppler scope pass this app run, so opening
    /// more shells there doesn't re-invoke the doppler CLI.
    doppler_synced: HashSet<PathBuf>,
    /// Fire-and-forget jobs whose result nothing reads — Doppler scope syncs,
    /// image-cache sweeps, link opens.  Held anyway: dropping a `Job` cancels
    /// work that has not started yet, and a submission followed immediately
    /// by drop would race the pool for nothing.  Drained once a frame.
    detached_jobs: Vec<jobs::Job<()>>,
    pending_session_close: Option<SessionId>,
    notify_rx: Receiver<SessionId>,
    /// Requests from IPC connection threads, drained once per frame.
    ipc_rx: Option<Receiver<ipc::AppCall>>,
    /// Held for its Drop: unlinks the socket file on shutdown.
    _ipc_socket: Option<ipc::SocketHandle>,
    /// Shared across sessions; auto-invalidated when cell size changes.
    builtin_glyphs: crate::builtin_font::BuiltinGlyphCache,
    ime: crate::ime::Ime,
    color_glyphs: crate::color_glyph::ColorGlyphCache,
    glyph_cache: crate::glyph_cache::GlyphCache,
    /// The `[font.normal]` face's own decoration metrics, parsed once when the
    /// fonts were installed.  Nothing re-reads the file per frame.
    face_metrics: crate::fonts::FaceMetrics,
    /// Scratch buffers the painter copies the visible grid into, so the
    /// terminal lock is released before any shape is built.
    grid_snapshot: crate::terminal_view::GridSnapshot,
    /// Buffers and GL objects for `[ui] gpu_grid`.  Held whether or not the
    /// option is on: it allocates nothing until a frame writes to it, and
    /// the GL side is built on the first paint that needs it.
    gpu_grid: crate::grid_gl::GpuGrid,
    /// Present only when frame timing was asked for; `None` is the normal run.
    frame_log: Option<crate::frame_log::FrameLog>,
    phases: crate::frame_log::Phases,
    /// How much of the frame in progress went to painting the terminal grid,
    /// as opposed to the sidebars and everything else sharing it.
    grid_paint: std::time::Duration,
    /// Geometry of the terminal pane as `terminal_view` last painted it.  A
    /// session spawned into an empty workspace is born at this size rather
    /// than at a constant, so a shell fast enough to print before the first
    /// paint prints into the grid it will keep.
    last_pane_geometry: Option<(TermSize, (f32, f32))>,
    /// In-flight background re-discoveries, keyed by project root.  Neither
    /// backend may block paint: wsl.exe takes seconds while the distro VM
    /// boots, and git2 takes tens of milliseconds on a project with many
    /// worktrees.  Results are adopted in `poll_project_refreshes`.
    project_refreshes: crate::project_refresh::ProjectRefreshes,
    /// Keeps each `project_refreshes` job alive on the pool: `jobs::Job`
    /// cancels its work on drop, so this is what stands between a refresh and
    /// having it cancelled the instant `refresh_project` returns.  Cleared
    /// alongside `project_refreshes` as each result is adopted.
    project_refresh_jobs: HashMap<PathBuf, jobs::Job<()>>,
    /// PTYs opened on a worker, adopted in `poll_pending_spawns`.
    pending_spawns: PendingSpawns,
    /// Shared-view attaches whose herdr calls are running on the pool,
    /// adopted in `poll_herdr_attach`.
    pending_herdr_attach: Vec<PendingHerdrAttach>,
    /// The shared view herdr was last focused for, and the call still on its
    /// way, both owned by `sync_herdr_view_focus`.
    herdr_focused_view: Option<SessionId>,
    herdr_view_focus: Option<HerdrViewFocus>,
    /// Resolved absolute path of `delta` inside each WSL distro, so diff panes
    /// stop re-sourcing a login profile on every open.  Successes only: a miss
    /// is never stored, so installing delta mid-session is picked up later.
    wsl_delta_paths: HashMap<String, String>,
    /// In-flight delta discoveries, keyed by distro, mirroring
    /// `pending_project_refresh` — resolved off the UI thread, adopted in
    /// `wsl_delta_path`.
    pending_delta: HashMap<String, jobs::Job<Option<String>>>,
    /// Row styling only — never `Worktree::prunable`, which the delete flow
    /// reads to choose between removing a worktree and pruning it.
    liveness: worktree_liveness::LivenessCache,
    /// The probe job in flight, if any.  One at a time: a path slower than
    /// the interval stretches freshness rather than queueing more work.
    liveness_probe: Option<jobs::Job<Vec<(PathBuf, worktree_liveness::Liveness)>>>,
    /// When the user last gave the app an event.  Timed wake-ups are armed
    /// only just after one, so an app left open overnight goes fully quiet.
    last_input: Instant,
    /// Rows behind the last-built focus snapshot. Paint reuses this until the
    /// next rebuild instead of recomputing the projection every frame.
    sidebar_rows_cache: Option<Vec<SidebarRow>>,
    /// Last reconciled snapshot, the baseline for the next cursor repair.
    sidebar_focus_prev: Option<sidebar_focus::TreeSnapshot>,
    /// The deepest row a filter hid, restored when it becomes visible again.
    sidebar_anchor: Option<SidebarRow>,
    /// What the reconciler itself last wrote.  Different values on the next
    /// pass mean the user navigated — a click, session cycling, the palette, a
    /// notification, IPC — and the anchor has been overtaken.
    sidebar_focus_written: Option<SidebarFocusWrite>,
    /// A close verdict the reconciler still owes the terminal.
    sidebar_deferred_close: Option<DeferredClose>,
    /// The herdr servers this app talks to: the native side plus one per
    /// running WSL distro, kept in step with which distros are up.
    herdr_endpoints: herdr::Endpoints,
}

struct DeleteRequest {
    project_idx: usize,
    worktree_path: PathBuf,
    worktree_name: String,
    branch: Option<String>,
    /// `None` until a count lands. The cache answers for a worktree the git
    /// panel has shown; one never selected has to wait for the job.
    dirty: Option<DirtyCounts>,
    /// Fills `dirty` when the cache was cold.
    dirty_job: Option<jobs::Job<DirtyCounts>>,
    /// The checkout dir is already gone; confirm prunes metadata instead of
    /// removing a directory.
    prunable: bool,
    /// Checkbox state for the prune dialog's "also delete branch".
    delete_branch: bool,
    /// Whether this confirm's removal passes `--force`: preset `true` when
    /// the dirty count is already known dirty (a warm cache, or a cold
    /// probe that landed before the confirm), left `false` while the count
    /// is unknown, and set `true` when reopening as the retry after an
    /// unforced removal was refused by git.
    force: bool,
}

/// An in-flight background delete/prune awaiting its git result.
struct DeleteTask {
    project_idx: usize,
    /// Marks the matching sidebar row with a spinner while the removal runs.
    worktree_path: PathBuf,
    worktree_name: String,
    branch: Option<String>,
    dirty: Option<DirtyCounts>,
    delete_branch: bool,
    /// Distinguishes the "prune" vs "delete" wording in a failure message.
    prunable: bool,
    job: jobs::Job<Result<(), String>>,
}

/// What a create reports when its worker unwound instead of returning: the
/// pool records only that a panic happened, and the step list stops wherever
/// it got to.
const CREATE_WORKER_PANICKED: &str = "the background worker panicked";

enum CreateState {
    Prompt {
        project_idx: usize,
        branch: String,
        error: Option<String>,
    },
    Running {
        project_idx: usize,
        branch: String,
        steps: Vec<String>,
        rx: Receiver<Progress>,
        /// Kept alive so dropping it doesn't cancel the still-running create
        /// on the pool.  `rx` carries the result, so the handle is polled
        /// only for the failure latch a panicked create reports through.
        job: jobs::Job<()>,
    },
    Done {
        project_idx: usize,
        steps: Vec<String>,
        result: Result<PathBuf, String>,
    },
}

/// A worktree creation the user minimized from the running modal: it keeps
/// running off-thread while they work, and its result is adopted in
/// `poll_pending_creates`.
struct BackgroundCreate {
    project_idx: usize,
    /// Shown on the sidebar placeholder row until the finished worktree
    /// replaces it on refresh.
    branch: String,
    rx: Receiver<Progress>,
    /// See `CreateState::Running::job`.
    job: jobs::Job<()>,
}

/// The rename dialog, keyed by root rather than index: an IPC `remove_project`
/// can reorder `projects` while the modal is open.
struct RenameState {
    root: PathBuf,
    /// Text being edited; seeded with the current display name.
    label: String,
}

/// The "remove project" confirmation modal.  Keyed by root, like the rename
/// dialog, so a reorder or IPC removal under the modal can't retarget it.
struct ProjectRemoveState {
    root: PathBuf,
    /// Display name, kept for the prompt after `projects` may have shifted.
    name: String,
}

/// Modal state for choosing a worktree's diff base.
struct BaseBranchPicker {
    worktree: PathBuf,
    query: String,
    /// `None` until the listing lands; the picker opens before git answers.
    /// `Err` is what git said when listing failed (not a repo, WSL down…).
    branches: Option<Result<Vec<String>, String>>,
    branches_job: Option<jobs::Job<Result<Vec<String>, String>>>,
    /// Auto-detected base shown on the "Auto" row.
    detected: Option<String>,
    cursor: usize,
}

/// Drag-and-drop payload for reordering the project list.  Carries the dragged
/// project's root rather than its index so a background refresh that shifts the
/// list mid-drag can't drop onto the wrong project.
#[derive(Clone)]
struct DraggedProject(PathBuf);

/// Drag-and-drop payload for reordering sessions.  Carries the id rather than
/// a position so a spawn, close or reorder mid-drag can't retarget the drop.
#[derive(Clone)]
struct DraggedSession(SessionId);

/// Which `git diff` flavor a sidebar click should open in delta.
enum DiffSource {
    Staged,
    Worktree,
    Untracked,
    /// Triple-dot diff against this base ref (merge-base, matching the
    /// `Changes vs <branch>` sidebar section).
    Branch {
        base: String,
    },
}

struct DiffRequest {
    file: String,
    source: DiffSource,
}

/// Stable identifier for "the diff this click would open" — matched against
/// the active diff session's `SessionKind::Diff { key }` to highlight the
/// originating row and toggle the pane off when clicked again.
fn diff_key(req: &DiffRequest) -> String {
    let tag = match &req.source {
        DiffSource::Staged => "staged",
        DiffSource::Worktree => "worktree",
        DiffSource::Untracked => "untracked",
        DiffSource::Branch { .. } => "branch",
    };
    format!("{tag}:{}", req.file)
}

/// The diff a git-panel cursor row would open, mirroring the render pass's
/// per-section click mapping.  `None` for a branch-diff row with no resolved
/// base, matching the render pass's unclickable base-less rows.
fn git_row_diff_request(row: &git_nav::GitRow, base: Option<&str>) -> Option<DiffRequest> {
    let source = match row.section {
        GitSection::Staged => DiffSource::Staged,
        GitSection::Unstaged => {
            if row.kind == Some(ChangeKind::Untracked) {
                DiffSource::Untracked
            } else {
                DiffSource::Worktree
            }
        },
        GitSection::Branch => DiffSource::Branch { base: base?.to_string() },
    };
    Some(DiffRequest { file: row.path.clone(), source })
}

impl AlacritreeApp {
    pub fn new(cc: &CreationContext<'_>, config: Config) -> Self {
        // A job's own closure cannot wake the loop when it unwinds, and the
        // failure it reports is only ever read from a frame.
        let waker_ctx = cc.egui_ctx.clone();
        jobs::pool().set_waker(move || waker_ctx.request_repaint());

        let theme = Theme::from_config(&config);

        let (font_chain, face_metrics) =
            crate::fonts::install_terminal_fonts(&cc.egui_ctx, &config.font, &config.ui_font);
        let color_glyph_budget_mb = config.font.color_glyph_cache_mb;

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = theme.terminal_bg;
        visuals.window_fill = theme.terminal_bg;
        visuals.extreme_bg_color = theme.terminal_bg;
        cc.egui_ctx.set_visuals(visuals);

        // Anchor every text style to the terminal font: titles (unmodified
        // labels) use `Body`/`Heading` at 100% of the grid's text size, and
        // every other UI label (`.small()`, buttons) drops to 80% via
        // `font_normal`.  Spacing knobs scale with the normal-text size so
        // paddings/widths track changes to `font.size`.
        let mut style = (*cc.egui_ctx.style()).clone();
        let scale = theme.ui_scale;
        let heading_px = theme.font_heading;
        let normal_px = theme.font_normal;
        style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(heading_px));
        style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(heading_px));
        style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(normal_px));
        style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(normal_px));
        style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(normal_px));
        let s = &mut style.spacing;
        s.item_spacing *= scale;
        s.button_padding *= scale;
        s.indent *= scale;
        s.interact_size *= scale;
        s.icon_width *= scale;
        s.icon_width_inner *= scale;
        s.icon_spacing *= scale;
        s.text_edit_width *= scale;
        // egui's debug build paints "Unaligned" labels next to widgets whose
        // edges land on fractional physical pixels.  Our chrome scaling
        // produces non-integer sizes by design (matching `font.size`), so the
        // warning is noise rather than signal — silence it everywhere.
        // `Style::debug` itself is `#[cfg(debug_assertions)]` in egui, so the
        // assignment has to be cfg-gated to keep `--release` compiling.
        #[cfg(debug_assertions)]
        {
            style.debug.show_unaligned = false;
        }
        cc.egui_ctx.set_style(style);

        // Terminal IME hint — matches alacritty's set_ime_purpose.
        cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::IMEPurpose(
            egui::viewport::IMEPurpose::Terminal,
        ));

        alacritty_terminal::tty::setup_env();

        // Before the first PTY spawn so children inherit ALACRITREE_SOCKET.
        let (ipc_socket, ipc_rx) = if config.ipc_socket {
            match ipc::spawn_listener(cc.egui_ctx.clone()) {
                Ok((handle, rx)) => {
                    log::info!("IPC socket: {}", handle.path().display());
                    (Some(handle), Some(rx))
                },
                Err(e) => {
                    log::warn!("failed to create IPC socket: {e}");
                    (None, None)
                },
            }
        } else {
            (None, None)
        };

        let persisted = state::load();
        let projects: Vec<Project> = persisted
            .projects
            .iter()
            .map(|p| {
                // WSL roots discover in the background after construction —
                // a cold distro takes seconds to boot and would block first
                // paint. Normalize the root first so a persisted `\\wsl$\`
                // spelling converges with the `\\wsl.localhost\` paths that
                // background discovery later swaps in via `poll_project_refreshes`.
                let root = wsl::normalize_root(p.root.clone());
                let mut project = match wsl::classify(&root) {
                    wsl::Location::Windows(_) => jobs::on_this_thread(|blocking| {
                        Project::discover(root, config.ui.upstream_status, blocking).project
                    }),
                    wsl::Location::Wsl { .. } => Project::placeholder(root),
                };
                project.expanded = p.expanded;
                project.shell_override = p.shell.as_deref().and_then(wsl::ShellChoice::parse);
                project.label = p.label.clone();
                project
            })
            .collect();

        // Delegate installation and the permission prompt belong to startup:
        // deferring them to the first toast would drop that toast (macOS
        // won't deliver while the authorization sheet is pending).
        #[cfg(target_os = "macos")]
        if config.ui.notifications {
            crate::notify_macos::init(cc.egui_ctx.clone());
        }

        let (notify_tx, notify_rx) = mpsc::channel();
        // `set` may fail only if a previous instance already initialized the
        // static (e.g. tests).  In that case the old sender points at a dead
        // app, so overwriting via `Mutex` would be ideal — but since we only
        // ever spawn one app per process, ignoring the error is fine.
        let _ = NOTIFY_TX.set(Mutex::new(notify_tx));

        let row_labels = crate::row_label::LabelTemplates::new(
            config.ui.worktree_name.clone(),
            config.ui.project_name.clone(),
        );

        let pr_status_concurrency = config.ui.pr_status_concurrency;
        let mut app = Self {
            show_left_sidebar: persisted.show_left_sidebar,
            show_right_sidebar: persisted.show_right_sidebar,
            focus: PaneFocus::Terminal,
            session_rows_always: config.ui.session_display.sidebar_always,
            session_tabs_always: config.ui.session_display.tabs_always,
            session_drag: config.ui.session_reorder.drag,
            sidebar_cursor: None,
            reorder_mode: false,
            sidebar_auto_shown: false,
            sidebar_cursor_moved: false,
            last_followed: (None, None),
            project_filter: PanelFilter::new(project_filter_toggles(config.ui.pr_status)),
            git_filter: PanelFilter::new(GIT_FILTER_TOGGLES),
            search_scope: config.ui.search_scope,
            git_cursor: None,
            git_cursor_moved: false,
            git_rows: Vec::new(),
            git_branch_base: None,
            git_sidebar_auto_shown: false,
            palette: CommandPalette::new(),
            sessions: Vec::new(),
            current_workspace: None,
            active_session: HashMap::new(),
            projects,
            git_status: HashMap::new(),
            base_branch_overrides: persisted
                .base_branches
                .iter()
                .map(|b| (b.worktree.clone(), b.branch.clone()))
                .collect(),
            pr_cache: PrCache::new(),
            row_labels,
            config,
            theme,
            error_dialog: None,
            quit_dialog_open: false,
            pending_delete: None,
            pending_deletes: Vec::new(),
            pending_create: None,
            pending_creates: Vec::new(),
            pending_rename: None,
            pending_base_branch: None,
            pending_project_remove: None,
            doppler_synced: HashSet::new(),
            detached_jobs: Vec::new(),
            pending_session_close: None,
            notify_rx,
            ipc_rx,
            _ipc_socket: ipc_socket,
            builtin_glyphs: crate::builtin_font::BuiltinGlyphCache::new(),
            ime: crate::ime::Ime::default(),
            color_glyphs: crate::color_glyph::ColorGlyphCache::new(
                font_chain,
                color_glyph_budget_mb,
            ),
            face_metrics,
            glyph_cache: crate::glyph_cache::GlyphCache::new(),
            grid_snapshot: crate::terminal_view::GridSnapshot::new(),
            gpu_grid: crate::grid_gl::GpuGrid::new(),
            frame_log: crate::frame_log::FrameLog::start(),
            phases: crate::frame_log::Phases::new(),
            grid_paint: std::time::Duration::ZERO,
            last_pane_geometry: None,
            project_refreshes: Default::default(),
            project_refresh_jobs: HashMap::new(),
            pending_spawns: Default::default(),
            pending_herdr_attach: Vec::new(),
            herdr_focused_view: None,
            herdr_view_focus: None,
            wsl_delta_paths: HashMap::new(),
            pending_delta: HashMap::new(),
            liveness: Default::default(),
            liveness_probe: None,
            last_input: Instant::now(),
            sidebar_rows_cache: None,
            sidebar_focus_prev: None,
            sidebar_anchor: None,
            sidebar_focus_written: None,
            sidebar_deferred_close: None,
            herdr_endpoints: herdr::Endpoints::default(),
        };

        app.pr_cache.set_concurrency(pr_status_concurrency);

        // The sidebar reads the distro list every frame and the registry
        // answers most machines outright; only the `wsl.exe` fallback for a
        // machine whose registry key is unreadable needs a thread of its own.
        app.detached_jobs.push(jobs::pool().spawn(jobs::Priority::Background, |blocking| {
            wsl::prime_distros_from_cli(blocking);
        }));

        let wsl_indices: Vec<usize> = app
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(wsl::classify(&p.root), wsl::Location::Wsl { .. }))
            .map(|(i, _)| i)
            .collect();
        for idx in wsl_indices {
            app.refresh_project(&cc.egui_ctx, idx);
        }

        if let Err(e) = app.spawn_session(&cc.egui_ctx, None) {
            app.error_dialog = Some(format!("failed to spawn shell: {e}"));
        }

        app
    }

    fn persist_sidebars(&self) {
        // Don't persist a sidebar the user never opened — an auto-shown
        // sidebar (e.g. from Ctrl+Shift+B while it was hidden) should not
        // reappear on next launch.
        let left = self.show_left_sidebar && !self.sidebar_auto_shown;
        let right = self.show_right_sidebar && !self.git_sidebar_auto_shown;
        state::mutate(|s| {
            s.show_left_sidebar = left;
            s.show_right_sidebar = right;
        });
    }

    /// Persist one project's `expanded` / `shell` fields without touching the
    /// rest of the file, so a second window's project list survives.
    fn persist_project(&self, root: &Path) {
        let Some(p) = self.projects.iter().find(|p| &p.root == root) else {
            return;
        };
        let (expanded, shell, label) =
            (p.expanded, p.shell_override.as_ref().map(|c| c.to_state_string()), p.label.clone());
        let root = root.to_path_buf();
        state::mutate(move |s| {
            if let Some(ps) = s.projects.iter_mut().find(|ps| ps.root == root) {
                ps.expanded = expanded;
                ps.shell = shell;
            } else {
                s.projects.push(PersistedProject { root, expanded, shell, label });
            }
        });
    }

    fn persist_project_label(&self, root: &Path) {
        let label = self.projects.iter().find(|p| p.root == *root).and_then(|p| p.label.clone());
        let root = root.to_path_buf();
        state::mutate(move |s| {
            if let Some(p) = s.projects.iter_mut().find(|p| p.root == root) {
                p.label = label;
            }
        });
    }

    /// Set or clear a project's display label and persist it.  Returns the
    /// project's index so IPC can reply with its JSON.
    fn rename_project(&mut self, root: &Path, label: Option<String>) -> Result<usize, String> {
        let idx = self
            .projects
            .iter()
            .position(|p| p.root == *root)
            .ok_or_else(|| format!("{} is not a project in the sidebar", root.display()))?;
        self.projects[idx].label = crate::projects::normalize_label(label);
        self.persist_project_label(root);
        Ok(idx)
    }

    /// Re-discovery always runs on a worker thread: wsl.exe takes ~400 ms warm
    /// and seconds while the distro VM boots, and git2 discovery costs tens of
    /// milliseconds on a project with many worktrees.
    fn refresh_project(&mut self, ctx: &Context, idx: usize) {
        let root = self.projects[idx].root.clone();
        if self.project_refreshes.is_running(&root) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let worker_root = root.clone();
        let upstream = self.config.ui.upstream_status;
        let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            let _ = tx.send(Project::discover(worker_root, upstream, blocking));
            ctx.request_repaint();
        });
        self.project_refresh_jobs.insert(root.clone(), job);
        self.project_refreshes.start(root, rx);
    }

    /// Keep the worktree rows the sidebar just drew honest about whether their
    /// checkout is still there.  Discovery only re-runs when something asks it
    /// to, so a `git worktree remove` typed into one of our own sessions would
    /// otherwise leave the row looking live until the user pressed refresh.
    ///
    /// `request_repaint_after` is what carries the tick across a terminal that
    /// has gone quiet — egui paints on demand, so without it the probe would
    /// never run a second time.  It is armed only for a short window after the
    /// user last touched the app, because an unconditional 1.5 s wake-up is
    /// not just a repaint: every frame runs `StatusCache::poll` from the git
    /// sidebar's paint on the same staleness interval, so a permanent
    /// heartbeat would spawn a git status walk forever on an app nobody is
    /// using.
    ///
    /// `probing` is the sidebar's decision, not a re-derivation: only it knows
    /// whether the walk that produced `drawn` was collecting at all, and an
    /// empty `drawn` on a probe frame ("nothing eligible painted") has to
    /// restart the interval where an empty `drawn` on any other frame must
    /// leave it alone.
    fn poll_worktree_liveness(&mut self, ctx: &Context, probing: bool, drawn: &[PathBuf]) {
        let now = Instant::now();
        match self.liveness_probe.as_ref().map(|job| (job.poll(), job.failed())) {
            Some((Some(results), _)) => {
                self.liveness.adopt(results, now);
                self.liveness_probe = None;
                // This runs after the rows painted, so the answers that just
                // landed are one frame late. Without asking for that frame the
                // new styling waits out a whole interval, or never arrives at
                // all once the grace window has closed.
                ctx.request_repaint();
            },
            // A job still running is the backpressure: a path slower than
            // the interval stretches freshness instead of stacking up probes,
            // and its own `request_repaint` brings us back here.
            Some((None, false)) => return,
            // A panicked probe adopts nothing, and an interval that never
            // restarts leaves `wants_probe` true: the next frame starts
            // another batch, and the pool wakes a frame at every job end, so
            // a probe that fails every time would run at frame rate.  An
            // empty round restarts the interval the same way.
            Some((None, true)) => {
                self.liveness_probe = None;
                self.liveness.adopt(Vec::new(), now);
            },
            None => {},
        }

        if probing {
            let batch = self.liveness.batch(drawn);
            if batch.is_empty() {
                // No job will land to close the interval, so close it here.
                self.liveness.adopt(Vec::new(), now);
            } else {
                let ctx = ctx.clone();
                let job = jobs::pool().spawn(jobs::Priority::Background, move |_blocking| {
                    let results: Vec<_> =
                        batch.iter().map(|p| (p.clone(), worktree_liveness::probe(p))).collect();
                    ctx.request_repaint();
                    results
                });
                self.liveness_probe = Some(job);
                return;
            }
        }

        if self.config.ui.worktree_liveness
            && self.last_input.elapsed() < PROBE_GRACE
            && let Some(wait) = self.liveness.wait(now)
        {
            ctx.request_repaint_after(wait);
        }
    }

    /// Re-run worktree discovery for every project — the keyboard/IPC
    /// equivalent of pressing each row's refresh button in turn.
    fn refresh_all_projects(&mut self, ctx: &Context) {
        for idx in 0..self.projects.len() {
            self.refresh_project(ctx, idx);
        }
    }

    /// Adopt completed background discoveries through `Project::apply`, which
    /// drops a result the backend could not vouch for and keeps `expanded`,
    /// the shell override, and the label either way.
    ///
    /// Runs every frame, so the occupied-directory set is built inside the
    /// callback: hoisting it would clone every session's path on every repaint
    /// terminal output happened to trigger, for the discoveries that are not
    /// running.
    fn poll_project_refreshes(&mut self) {
        let sessions = &self.sessions;
        let projects = &mut self.projects;
        let refresh_jobs = &mut self.project_refresh_jobs;
        self.project_refreshes.poll(|root, found| {
            refresh_jobs.remove(root);
            match projects.iter_mut().find(|p| p.root == *root) {
                Some(project) => {
                    let occupied: HashSet<PathBuf> =
                        sessions.iter().filter_map(|s| s.working_directory.clone()).collect();
                    project.apply(found, &occupied);
                    Ok(project_json(project))
                },
                None => Err(format!("{} is not a project in the sidebar", root.display())),
            }
        });
    }

    /// Push a session record and get its PTY opened: inline when the gate is
    /// off, on the job pool when it is on.  The record exists before this
    /// returns either way, so a caller can activate the tab without waiting
    /// for a shell.  Callers own `active_session`; this owns `self.sessions`.
    fn open_session(
        &mut self,
        session: Session,
        request: session::OpenRequest,
    ) -> std::io::Result<SessionId> {
        let id = session.id;
        self.sessions.push(session);

        if !self.config.ui.async_session_spawn {
            match session::open(request) {
                Ok(attachment) => {
                    let idx = self.sessions.iter().position(|s| s.id == id).expect("just pushed");
                    self.sessions[idx].attach(attachment);
                    return Ok(id);
                },
                Err(e) => {
                    // The record went in before the open, so it comes back out
                    // before the error does: with the gate off, a caller that
                    // gets `Err` must see no trace of the session.
                    self.sessions.retain(|s| s.id != id);
                    return Err(e);
                },
            }
        }

        // Interactive: an empty pane is on screen until this lands.  The pool
        // repaints once the job returns, so nothing here has to — without that
        // the result would wait for whatever wakes the loop next, which under
        // load is the shell's own first output seconds later.
        let job = jobs::pool()
            .spawn(jobs::Priority::Interactive, move |_blocking| session::open(request));
        self.pending_spawns.start(id, job);
        Ok(id)
    }

    /// Adopt every PTY that finished opening.  A session whose record is gone
    /// was closed while it was opening: dropping the attachment shuts its
    /// shell down rather than resurrecting the tab.
    fn poll_pending_spawns(&mut self, ctx: &Context) {
        for finished in self.pending_spawns.take_finished() {
            match finished {
                Finished::Opened(id, attachment, waiters) => {
                    match self.sessions.iter().position(|s| s.id == id) {
                        Some(idx) => {
                            let started = Instant::now();
                            self.sessions[idx].attach(attachment);
                            crate::frame_log::spawn_phase(Some(id), "attach", started.elapsed());
                            PendingSpawns::answer(waiters, Ok(json!({ "session_id": id })));
                        },
                        None => {
                            drop(attachment);
                            PendingSpawns::answer(
                                waiters,
                                Err("the session was closed while its shell was starting".into()),
                            );
                        },
                    }
                },
                Finished::Failed(id, e, waiters) => {
                    // The workspace comes off the record rather than off the
                    // pending entry: `move_session_to_key` can re-key a
                    // session while its PTY is opening.
                    let ws = self
                        .sessions
                        .iter()
                        .find(|s| s.id == id)
                        .map(|s| s.working_directory.clone());
                    if let Some(ws) = ws {
                        self.close_session_with(ctx, id, CloseReason::SpawnFailed);
                        self.report_spawn_failure(ctx, &ws, &e);
                    }
                    PendingSpawns::answer(waiters, Err(format!("failed to spawn shell: {e}")));
                },
            }
        }
    }

    fn spawn_session(
        &mut self,
        ctx: &Context,
        working_directory: WorkspaceKey,
    ) -> std::io::Result<SessionId> {
        let (shell, wsl_probe) = self.resolve_shell(&working_directory);
        self.spawn_session_with_shell(ctx, working_directory, shell, wsl_probe)
    }

    /// The geometry to open a PTY at, so it is born at the size it will keep.
    /// Under the gate this matters: a session that opened at 80x24 and was
    /// resized on attach makes a fast child print its first output into a grid
    /// that is about to be reflowed under it.  Three tiers, most exact first:
    /// the active session's own numbers when one exists; the terminal pane's
    /// last painted size when it doesn't, which covers a respawn after
    /// `close_session` removes the active entry before the replacement spawns;
    /// 80x24 when neither is available, which only the constructor reaches,
    /// since no frame has painted yet to leave a better number behind.  Never
    /// `self.sessions.last()`, an arbitrary session possibly in another
    /// workspace at a different pane size.
    fn next_spawn_geometry(&self) -> (TermSize, (f32, f32)) {
        let active = self.active_session_index().map(|idx| {
            let session = &self.sessions[idx];
            ActiveGeometry {
                size: session.size,
                cell_size: session.cell_size,
                is_scratchpad: session.scratchpad.is_some(),
            }
        });
        spawn_geometry(active, self.last_pane_geometry)
    }

    /// The one path every shell reaches, which is why the checkout guard and
    /// the Doppler sync live here rather than in `spawn_session`: a named
    /// profile arrives with its shell already chosen and would otherwise open
    /// in a checkout Ctrl+T refuses.
    fn spawn_session_with_shell(
        &mut self,
        ctx: &Context,
        working_directory: WorkspaceKey,
        shell: Option<Shell>,
        wsl_probe: Option<WslProbe>,
    ) -> std::io::Result<SessionId> {
        if let Some(dir) = &working_directory {
            // A checkout git has forgotten is refused here rather than in
            // `session::open`, which can only see whether the directory
            // exists — a half-finished `git worktree remove` leaves one that
            // does.  Refusing here is what keeps the greyed row's promise.
            if self.worktree_gone(dir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("worktree is no longer checked out: {}", dir.display()),
                ));
            }
            // Called synchronously, so the once-per-worktree guard is set
            // before a second rapid spawn for the same worktree can see it
            // unset. The scope mirror itself runs off-thread, so a shell in
            // a worktree git already knows about can still start before the
            // mirrored scopes land, racing `doppler run` against the write.
            // That costs one retryable "You must specify a project" failure,
            // not lost work.
            self.sync_doppler_scopes(dir.clone());
        }
        let (size, cell_size) = self.next_spawn_geometry();
        let (session, request) = Session::pending_shell(
            ctx.clone(),
            &self.config,
            working_directory.clone(),
            size,
            cell_size,
            shell,
            wsl_probe,
        );
        let id = self.open_session(session, request)?;
        self.active_session.insert(working_directory, id);
        Ok(id)
    }

    /// Opens a herdr agent in a session running herdr's attach client.  The
    /// session is an ordinary shell, so nothing in the grid or input path
    /// treats it specially; only the key marks it as this agent's row.
    /// Returns whether the attach succeeded so the caller can switch
    /// `current_workspace` to `workspace` first and restore it on failure —
    /// the same replace-and-restore shape `spawn_shell_request` uses, needed
    /// here for the same reason: a refusal is only readable in the
    /// workspace it happened in.
    fn attach_herdr_agent(
        &mut self,
        ctx: &Context,
        key: herdr::HerdrKey,
        pane_id: &str,
        workspace: WorkspaceKey,
    ) -> bool {
        if let Some(id) = self.herdr_session_for(&key) {
            self.activate_session_by_id(id);
            return true;
        }
        if herdr::can_attach(&key.side) {
            // Nothing to ask herdr first: the pane id is the whole target,
            // and the client attaches to it directly.
            let args = herdr::attach_args(pane_id);
            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            let (program, argv) = key.side.command(&borrowed);
            return self.open_herdr_session(ctx, key, workspace, program, argv);
        }

        // Every one of herdr's app clients draws the same focused pane, so a
        // shared view shows a row's pane only while herdr is focused there.
        // The attach focuses it so the first frame is already right, and
        // `sync_herdr_view_focus` focuses it again whenever the session comes
        // back up, which is what lets a side hold one session per row.
        if self.pending_herdr_attach.iter().any(|pending| pending.key == key) {
            return true;
        }
        // The gesture is two herdr processes whatever `async_session_spawn`
        // says, and running them from the click would hold the frame for as
        // long as herdr takes to answer.
        let name = self.herdr_session_name(&key.side);
        let side = key.side.clone();
        let pane = pane_id.to_string();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
            herdr_attach_gesture(&side, &pane, name)
        });
        self.pending_herdr_attach.push(PendingHerdrAttach { job, key, workspace });
        true
    }

    /// Adopt the shared-view attaches whose herdr calls have landed.  Each
    /// session opens in the workspace its own click came from, which that
    /// click switched to before handing the gesture over.
    fn poll_herdr_attach(&mut self, ctx: &Context) {
        let mut running = Vec::new();
        for pending in std::mem::take(&mut self.pending_herdr_attach) {
            match pending.job.poll() {
                Some(Ok((program, argv))) => {
                    self.open_herdr_session(ctx, pending.key, pending.workspace, program, argv);
                },
                Some(Err(e)) => self.error_dialog = Some(e),
                None if pending.job.failed() => {
                    self.error_dialog = Some("the herdr attach did not finish".to_string());
                },
                None => running.push(pending),
            }
        }
        self.pending_herdr_attach = running;
    }

    /// Open the session that runs an attach client.  A shared view starts on
    /// the pane the gesture just focused, so herdr is already where the new
    /// session's row says it is and no second focus is owed.
    fn open_herdr_session(
        &mut self,
        ctx: &Context,
        key: herdr::HerdrKey,
        workspace: WorkspaceKey,
        program: String,
        argv: Vec<String>,
    ) -> bool {
        let shared_view = !herdr::can_attach(&key.side);
        // `alacritty_terminal::tty::Shell`'s fields are crate-private, so
        // this goes through the constructor rather than a struct literal.
        let shell = Shell::new(program, argv);
        match self.spawn_session_with_shell(ctx, workspace, Some(shell), None) {
            Ok(id) => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                    session.herdr_key = Some(key);
                }
                if shared_view {
                    self.herdr_focused_view = Some(id);
                }
                true
            },
            Err(e) => {
                self.error_dialog = Some(format!("failed to attach herdr agent: {e}"));
                false
            },
        }
    }

    /// Keep herdr focused on the pane the visible shared view stands for.
    ///
    /// herdr's app clients all render one shared state, so several shared
    /// views on a side draw the same pane at any moment.  Asking herdr to
    /// focus the pane of whichever one the user is looking at is what makes
    /// them behave as one session per agent: the visible one is right, and
    /// the rest are off screen.
    ///
    /// One call at a time, and the tracker moves only once herdr has answered
    /// — two focuses in flight would land in whatever order herdr finished
    /// them in.  A refusal still moves it, so a herdr that keeps saying no
    /// costs one call rather than one per frame.
    fn sync_herdr_view_focus(&mut self) {
        if let Some(pending) = self.herdr_view_focus.take() {
            match pending.job.poll() {
                Some(result) => {
                    if let Err(e) = result {
                        log::warn!("{e}");
                    }
                    self.herdr_focused_view = Some(pending.session);
                },
                None if pending.job.failed() => self.herdr_focused_view = Some(pending.session),
                None => self.herdr_view_focus = Some(pending),
            }
            return;
        }
        let Some(id) = self.active_session.get(&self.current_workspace).copied() else {
            return;
        };
        let key = self.sessions.iter().find(|s| s.id == id).and_then(|s| s.herdr_key.clone());
        if !needs_view_focus(key.as_ref(), id, self.herdr_focused_view) {
            return;
        }
        let Some(key) = key else { return };
        let Some(pane_id) =
            self.find_herdr_agent(&key.side, &key.terminal_id).map(|agent| agent.pane_id.clone())
        else {
            return;
        };
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
            herdr::focus_agent(&key.side, &pane_id)
        });
        self.herdr_view_focus = Some(HerdrViewFocus { session: id, job });
    }

    fn toggle_scratchpad_tab(&mut self, ctx: &Context) {
        let workspace = self.current_workspace.clone();
        if let Some(index) = self.scratchpad_session_index(&workspace) {
            let id = self.sessions[index].id;
            if self.active_session.get(&workspace).copied() == Some(id) {
                // Scratchpad edits are persisted as they happen, so toggling
                // the active tab closed never needs the session-close prompt.
                self.close_session(ctx, id);
                return;
            }
            self.active_session.insert(workspace, id);
        } else if let Err(e) = self.spawn_scratchpad(ctx, workspace) {
            self.error_dialog = Some(format!("failed to open scratchpad: {e}"));
            return;
        }
        self.focus_terminal();
    }

    fn spawn_scratchpad(
        &mut self,
        ctx: &Context,
        workspace: WorkspaceKey,
    ) -> std::io::Result<SessionId> {
        let file = scratchpad::ensure_file(&workspace)?;
        let session = Session::spawn_scratchpad(
            ctx.clone(),
            &self.config,
            workspace.clone(),
            TermSize::new(80, 24),
            (8.0, 16.0),
            file,
        )?;
        let id = session.id;
        self.sessions.push(session);
        self.active_session.insert(workspace, id);
        Ok(id)
    }

    /// Mirror Doppler scopes into a worktree the first time a shell opens
    /// there.  The create-time hook in `worktree.rs` covers worktrees we
    /// make; this lazy pass covers ones created outside alacritree, which
    /// otherwise hit "Doppler Error: You must specify a project".
    fn sync_doppler_scopes(&mut self, worktree: PathBuf) {
        if !self.doppler_synced.insert(worktree.clone()) {
            return;
        }
        let main_checkout = self.projects.iter().find_map(|p| {
            let owns = p.worktrees.iter().any(|wt| !wt.is_main && wt.path == worktree);
            if !owns {
                return None;
            }
            p.worktrees.iter().find(|wt| wt.is_main).map(|wt| wt.path.clone())
        });
        let Some(main_checkout) = main_checkout else {
            return;
        };
        self.detached_jobs.push(jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            let linked = doppler::mirror_scopes(&main_checkout, &worktree, blocking);
            if linked > 0 {
                log::info!("linked {linked} doppler scope(s) into {}", worktree.display());
            }
        }));
    }

    /// Spawn a named profile into the current workspace, bypassing the
    /// override/auto resolution chain — the user asked for this profile
    /// explicitly.  Raises `error_dialog` directly: this wrapper's three
    /// callers (the `SpawnProfileN` keybinding, the tab strip `+`, and the
    /// palette) have no stale-row state to reconcile, unlike the sidebar's
    /// `spawn_profile_session_in` caller.
    fn spawn_profile_session(&mut self, ctx: &Context, name: &str) {
        let ws = self.current_workspace.clone();
        if let Err(e) = self.spawn_profile_session_in(ctx, name, ws) {
            self.error_dialog = Some(format!("failed to spawn profile `{name}`: {e}"));
        }
    }

    /// Spawn a named profile into an arbitrary workspace — the worktree
    /// sidebar's profile menu targets the row it was opened on, which is
    /// often not the workspace currently on screen.  Returns the error
    /// instead of raising `error_dialog` itself so the sidebar caller can
    /// run it through `report_spawn_failure`, matching `spawn_shell_request`.
    fn spawn_profile_session_in(
        &mut self,
        ctx: &Context,
        name: &str,
        ws: WorkspaceKey,
    ) -> std::io::Result<SessionId> {
        let Some(profile) = self.config.profile(name) else {
            let msg = format!("no shell profile named `{name}`");
            log::warn!("{msg}");
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, msg));
        };
        let (shell, wsl_probe) = profile_session_shell(profile);
        self.spawn_session_with_shell(ctx, ws, shell, wsl_probe)
    }

    /// Shell for a workspace; `None` means "no override" —
    /// `Session::pending_shell` falls through to alacritty's config-driven
    /// shell with its OS-guaranteed fallback.  The home tab (`None`
    /// workspace) has no project or location, so only the default profile can
    /// apply there.
    fn resolve_shell(&self, workspace: &WorkspaceKey) -> (Option<Shell>, Option<WslProbe>) {
        let path = workspace.as_deref();
        let choice = path.and_then(|p| {
            self.projects
                .iter()
                .find(|proj| proj.worktrees.iter().any(|wt| wt.path.as_path() == p))
                .and_then(|proj| proj.shell_override.clone())
        });
        let location_distro = path.and_then(|p| match wsl::classify(p) {
            wsl::Location::Wsl { distro, .. } => Some(distro),
            wsl::Location::Windows(_) => None,
        });
        let known: Vec<String> = wsl::distros().into_iter().map(|d| d.name).collect();
        match shell_decision(
            choice.as_ref(),
            location_distro.as_deref(),
            &known,
            &self.config.profiles,
            self.config.default_profile.as_deref(),
        ) {
            ShellDecision::ConfigShell => config_session_shell(&self.config),
            // A WSL decision only arises from a workspace path (override or
            // location), never from the home tab.
            ShellDecision::WslDistro(distro) => match path {
                Some(p) => wsl_session_shell(&distro, p),
                None => (None, None),
            },
            ShellDecision::Profile(name) => match self.config.profile(&name) {
                Some(profile) => profile_session_shell(profile),
                None => (None, None),
            },
        }
    }

    fn activate_worktree(&mut self, ctx: &Context, path: &Path) {
        // The dir can vanish between discovery marking the row live and the
        // click. Switching first would strand the user on a dead workspace
        // with a failed spawn — stay put and let the sidebar re-mark the row.
        // Shells already running there are the exception: they outlive the
        // directory, and this row is the only way back to them.
        if self.worktree_gone(path) && !self.workspace_has_sessions(&Some(path.to_path_buf())) {
            self.error_dialog =
                Some("worktree directory is missing — prune it from the sidebar".to_string());
            if let Some(idx) =
                self.projects.iter().position(|p| p.worktrees.iter().any(|w| w.path == path))
            {
                self.refresh_project(ctx, idx);
            }
            return;
        }
        self.current_workspace = Some(path.to_path_buf());
        self.ensure_active_session(ctx);
    }

    fn activate_home(&mut self, ctx: &Context) {
        self.current_workspace = None;
        self.ensure_active_session(ctx);
    }

    fn ensure_active_session(&mut self, ctx: &Context) {
        if self.active_session_index().is_some() {
            return;
        }
        self.adopt_active_session();
        if self.active_session_index().is_some() {
            return;
        }
        let ws = self.current_workspace.clone();
        if let Err(e) = self.spawn_session(ctx, ws.clone()) {
            self.report_spawn_failure(ctx, &ws, &e);
            return;
        }
        // Filling in a missing active entry is self-healing, not navigation.
        self.mark_sidebar_focus_write();
    }

    /// Re-attach to an existing session when the active id went stale
    /// (closed or reaped this frame). Never spawns: an emptied on-screen
    /// workspace either navigated away in `close_session` or shows the
    /// "no session" placeholder.
    fn adopt_active_session(&mut self) {
        let ws_idx = self.workspace_display_indices(&self.current_workspace);
        if let Some(&idx) = ws_idx.first() {
            let id = self.sessions[idx].id;
            self.active_session.insert(self.current_workspace.clone(), id);
            // Filling in a missing active entry is self-healing, not navigation.
            self.mark_sidebar_focus_write();
        }
    }

    fn close_session(&mut self, ctx: &Context, id: SessionId) {
        self.close_session_with(ctx, id, CloseReason::User);
    }

    fn close_session_with(&mut self, ctx: &Context, id: SessionId, reason: CloseReason) {
        let Some(idx) = self.sessions.iter().position(|s| s.id == id) else {
            return;
        };
        let workspace = self.sessions[idx].working_directory.clone();
        let policy = self.config.ui.last_session_close;
        let ring = policy.rings().then(|| self.session_ring()).unwrap_or_default();
        self.sessions.remove(idx);

        let remaining: Vec<(WorkspaceKey, SessionId)> =
            self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect();

        if self.active_session.get(&workspace).copied() == Some(id) {
            match close_landing(&remaining, &workspace, idx, self.config.ui.sidebar_focus) {
                Some(new_id) => {
                    self.active_session.insert(workspace.clone(), new_id);
                },
                None => {
                    self.active_session.remove(&workspace);
                },
            }
        }

        // Closing the on-screen workspace's last session must not strand the
        // view on an empty pane. What happens instead is policy: `respawn`
        // recycles a shell in place (the last session is by design
        // unclosable), `navigate` falls back to the project main, then home,
        // and the ring policies land on the nearest surviving session in the
        // flat session ring instead.
        let main = workspace.as_deref().and_then(|p| project_main_for(&self.projects, p));
        let mut verdict = close_navigation(
            reason,
            close_fallback(&workspace, &self.current_workspace, &remaining, main),
        );
        if verdict != CloseFallback::Stay && policy.rings() {
            let prefer = policy
                .prefers_project()
                .then(|| sidebar_nav::project_of(&self.projects, &workspace))
                .flatten();
            if let Some((_, landing)) = ring_landing(&ring, &[id], prefer) {
                verdict = CloseFallback::ActivateSession(landing);
            }
        }
        if verdict != CloseFallback::Stay && policy == LastSessionClose::Respawn {
            if let Err(e) = self.spawn_session(ctx, workspace.clone()) {
                self.report_spawn_failure(ctx, &workspace, &e);
            }
            return;
        }
        if defers_close_navigation(self.config.ui.sidebar_focus) && verdict != CloseFallback::Stay {
            self.sidebar_deferred_close = Some(DeferredClose { verdict, removed_worktree: None });
            // `reap_exited_sessions` runs after paint, so a shell that exited
            // on its own has no reconciler pass left this frame; without this
            // the deferral would wait for unrelated input.
            ctx.request_repaint();
            return;
        }
        self.apply_close_fallback(ctx, verdict);
    }

    /// Act on a removal verdict: stay put, move to the project's main
    /// checkout, move to a session the ring chose, or go home.
    fn apply_close_fallback(&mut self, ctx: &Context, verdict: CloseFallback) {
        match verdict {
            CloseFallback::Stay => {},
            CloseFallback::Activate(main) => {
                self.activate_worktree(ctx, &main);
                // Adopting an existing idle session produces no PTY event, so
                // nothing else would wake the paint that shows it.
                ctx.request_repaint();
            },
            CloseFallback::ActivateSession(id) => {
                self.activate_session_by_id(id);
                ctx.request_repaint();
            },
            CloseFallback::Home => {
                self.activate_home(ctx);
                ctx.request_repaint();
            },
        }
    }

    fn request_close_session(&mut self, ctx: &Context, id: SessionId) {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        if close_needs_prompt(&self.config.ui, session.herdr_key.is_some(), session.is_busy()) {
            self.pending_session_close = Some(id);
        } else {
            self.close_session(ctx, id);
        }
    }

    /// Open the delete/prune confirm dialog for the worktree at `path`.
    /// Main checkouts have no delete affordance, and a worktree whose
    /// removal is already running is inert.
    fn request_worktree_delete(&mut self, path: &Path) {
        if self.pending_deletes.iter().any(|t| t.worktree_path == *path) {
            return;
        }
        let Some((project_idx, wt)) =
            self.projects.iter().enumerate().find_map(|(idx, p)| {
                p.worktrees.iter().find(|w| w.path == *path).map(|w| (idx, w))
            })
        else {
            return;
        };
        if wt.is_main {
            return;
        }
        // Discovery marking can be stale; a dir deleted since the last
        // refresh should still get the prune flow, not a doomed
        // `git worktree remove`.
        let prunable = wt.prunable || worktree_liveness::is_gone(&wt.path);
        // A missing dir has nothing to be dirty; skip the status probe. A
        // worktree the git panel has already completed a compute for answers
        // from that cache instead of walking the tree again — a cache entry
        // with no compute yet (the panel's first frame for this workspace)
        // is `GitStatus::default()`, indistinguishable from "known clean",
        // so it is not read as an answer. A cold one waits on a job so the
        // dialog opens at once and fills in.
        //
        // A resolved dirty count preloads `force` so a known-dirty tree goes
        // straight to a forced removal, as it always has — the unforced
        // first attempt is reserved for a genuinely unknown count, where
        // git's own refusal decides instead.
        let (dirty, dirty_job, force) = if prunable {
            (Some(DirtyCounts::default()), None, false)
        } else if let Some(counts) = self
            .git_status
            .get(&wt.path)
            .filter(|cache| cache.has_status())
            .map(|cache| DirtyCounts::from_status(cache.last()))
        {
            let force = counts.is_dirty();
            (Some(counts), None, force)
        } else {
            let path = wt.path.clone();
            let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
                git_status::dirty_counts(&path, blocking)
            });
            (None, Some(job), false)
        };
        self.pending_delete = Some(DeleteRequest {
            project_idx,
            worktree_path: wt.path.clone(),
            worktree_name: wt.name.clone(),
            branch: wt.branch.clone(),
            dirty,
            dirty_job,
            prunable,
            delete_branch: true,
            force,
        });
    }

    /// Re-key `id` to `target`, repairing both workspaces' active-session
    /// entries and following the move with the view when the session was the
    /// one on screen.
    fn move_session_to_key(
        &mut self,
        id: SessionId,
        target: WorkspaceKey,
    ) -> Result<WorkspaceKey, String> {
        let idx = self
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| format!("no session with id {id} — see list_sessions"))?;
        if matches!(&self.sessions[idx].kind, SessionKind::Scratchpad { .. }) {
            return Err("scratchpads belong to their backing workspace and cannot be moved".into());
        }
        // A workspace's diff pane is found by workspace plus kind, so a pane
        // carried elsewhere becomes the one the next git click closes while
        // the workspace it left opens a second.
        if matches!(&self.sessions[idx].kind, SessionKind::Diff { .. }) {
            return Err("diff panes belong to the workspace they were opened from".into());
        }
        let source = self.sessions[idx].working_directory.clone();
        if source == target {
            return Ok(target);
        }

        let was_source_active = self.active_session.get(&source).copied() == Some(id);
        let on_screen = was_source_active && self.current_workspace == source;
        self.sessions[idx].working_directory = target.clone();
        let next_in_source =
            self.sessions.iter().find(|s| s.working_directory == source).map(|s| s.id);

        let outcome = plan_move(
            was_source_active,
            on_screen,
            next_in_source,
            self.active_session.contains_key(&target),
        );
        match outcome.source {
            SourceRepair::Keep => {},
            SourceRepair::Set(next) => {
                self.active_session.insert(source, next);
            },
            SourceRepair::Remove => {
                self.active_session.remove(&source);
            },
        }
        if outcome.claim_target {
            self.active_session.insert(target.clone(), id);
        }
        if outcome.follow {
            self.current_workspace = target.clone();
        }
        Ok(target)
    }

    /// Whether the sidebar worktree at `path` is one git no longer recognises,
    /// which is the single question the row's styling, this guard and the
    /// delete flow all ask — a greyed row that still spawns a shell would be
    /// the inconsistency this exists to remove.
    ///
    /// Main checkouts and non-git project roots have no `.git` link to lose,
    /// so they fall back to the directory itself; so does a path no project
    /// lists, which is the safe default for a caller we cannot place.
    ///
    /// The same path can be listed by two projects, so any row that calls it a
    /// linked worktree decides.  Taking the first match instead would let
    /// sidebar order pick the weaker test, and the husk of a linked checkout
    /// would read as alive.
    fn worktree_gone(&self, path: &Path) -> bool {
        let linked = self
            .projects
            .iter()
            .flat_map(|p| &p.worktrees)
            .filter(|wt| wt.path == path)
            .any(|wt| !wt.is_main);
        if linked { worktree_liveness::is_gone(path) } else { !path.is_dir() }
    }

    /// Report a failed spawn, and re-run discovery when the cause was a
    /// vanished checkout: git may have forgotten the worktree entirely, in
    /// which case the row should go rather than keep offering a shell that
    /// cannot start.
    fn report_spawn_failure(&mut self, ctx: &Context, ws: &WorkspaceKey, e: &std::io::Error) {
        self.error_dialog = Some(format!("failed to spawn shell: {e}"));
        let Some(path) = ws.as_deref().filter(|p| self.worktree_gone(p)) else {
            return;
        };
        if let Some(idx) =
            self.projects.iter().position(|p| p.worktrees.iter().any(|w| w.path == path))
        {
            self.refresh_project(ctx, idx);
        }
    }

    /// The sessions of `ws` a reorder may move, in the order they are drawn.
    ///
    /// A session attached to a harness pane is not among them: its place in
    /// the sidebar is the pane's place in the harness, which alacritree does
    /// not own and cannot write back.  Moving one inside `self.sessions`
    /// would change nothing on screen and quietly change the tab order, so
    /// every reorder path reads its subject and its landing slots from here.
    fn workspace_reorder_indices(&self, ws: &WorkspaceKey) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.working_directory == *ws && s.herdr_key.is_none())
            .map(|(i, _)| i)
            .collect()
    }

    /// `ws`'s sessions in the order the sidebar and the tab strip draw them:
    /// alacritree's own first, then the harness-backed ones in the harness's
    /// order.  Every ring the user steps through is built from here, so a
    /// press walks the list on screen rather than the order the sessions
    /// happened to open in — the two part company as soon as a harness pane
    /// is attached out of the harness's own order.
    fn workspace_display_indices(&self, ws: &WorkspaceKey) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.working_directory == *ws)
            .map(|(i, _)| i)
            .collect();
        indices.sort_by_key(|i| match self.sessions[*i].herdr_key.clone() {
            Some(key) => (1, self.herdr_pane_index(&key).unwrap_or(usize::MAX)),
            None => (0, 0),
        });
        indices
    }

    /// The workspaces a reorder may use: those the app is willing to switch
    /// to, minus any whose delete is already running.  A session landing on a
    /// spinner row is a session that delete is about to reap.
    fn reorderable_workspaces(&self) -> Vec<WorkspaceKey> {
        self.workspace_order()
            .into_iter()
            .filter(|ws| match ws {
                None => true,
                Some(path) => !self.pending_deletes.iter().any(|t| t.worktree_path == *path),
            })
            .collect()
    }

    /// The workspace a session sits in, and the workspaces a reorder may carry
    /// it through.  A scratchpad or diff pane belongs to its workspace, so its
    /// range is that workspace alone whatever the scope says — the keyboard and
    /// the mouse both read the rule from here so neither can offer a landing
    /// the move would refuse.
    fn reorder_range(&self, id: SessionId) -> Option<(WorkspaceKey, Vec<WorkspaceKey>)> {
        let idx = self.sessions.iter().position(|s| s.id == id)?;
        let origin = self.sessions[idx].working_directory.clone();
        if matches!(
            &self.sessions[idx].kind,
            SessionKind::Scratchpad { .. } | SessionKind::Diff { .. }
        ) {
            return Some((origin.clone(), vec![origin]));
        }
        let range = sidebar_nav::move_range(
            &self.projects,
            &self.reorderable_workspaces(),
            &origin,
            self.config.ui.session_reorder.scope,
        );
        Some((origin, range))
    }

    /// Walk `id` to `position` among its own workspace's sessions.
    fn reorder_session_within_workspace(&mut self, id: SessionId, position: usize) {
        let Some(abs) = self.sessions.iter().position(|s| s.id == id) else { return };
        let ws = self.sessions[abs].working_directory.clone();
        let indices = self.workspace_reorder_indices(&ws);
        let Some(j) = indices.iter().position(|i| *i == abs) else { return };
        for (a, b) in walk_swaps(&indices, j, position) {
            self.sessions.swap(a, b);
        }
    }

    /// Apply a decided move: change the workspace first when the target is a
    /// different one, then walk the session to its position there.  Reports the
    /// workspace the session actually ended up in, or `None` when the move was
    /// refused and the session stayed where it was.
    fn apply_session_move(&mut self, id: SessionId, target: StepTarget) -> Option<WorkspaceKey> {
        let abs = self.sessions.iter().position(|s| s.id == id)?;
        let landed_in = if self.sessions[abs].working_directory == target.workspace {
            target.workspace
        } else {
            self.move_session_to_key(id, target.workspace).ok()?
        };
        self.reorder_session_within_workspace(id, target.position);
        Some(landed_in)
    }

    /// Apply a mouse drop, whose slot arithmetic `drop_position` decides.
    fn apply_session_drop(&mut self, id: SessionId, workspace: WorkspaceKey, insert_before: usize) {
        let Some(abs) = self.sessions.iter().position(|s| s.id == id) else { return };
        // A harness owns this pane's place, so there is no slot to drop it
        // into and no arithmetic to do.
        if self.sessions[abs].herdr_key.is_some() {
            return;
        }
        let same_workspace = self.sessions[abs].working_directory == workspace;
        let indices = self.workspace_reorder_indices(&workspace);
        let from = indices.iter().position(|i| *i == abs).unwrap_or(indices.len());
        let Some(position) = drop_position(same_workspace, indices.len(), from, insert_before)
        else {
            return;
        };
        if same_workspace {
            self.reorder_session_within_workspace(id, position);
        } else {
            let _ = self.apply_session_move(id, StepTarget { workspace, position });
        }
    }

    /// One `MoveSessionUp` / `MoveSessionDown` press.  Every refusal is a
    /// silent no-op: a clamped end, a boundary the scope forbids, a scratchpad
    /// asked to leave its workspace.  None of those is a failure — each is a
    /// move with nowhere to go.
    fn step_session(&mut self, delta: i32) {
        let sidebar_focused = self.focus == PaneFocus::ProjectsSidebar;
        let Some(id) = reorder_subject(
            sidebar_focused,
            self.sidebar_cursor.as_ref(),
            || self.active_session.get(&None).copied(),
            |path| self.active_session.get(&Some(path.to_path_buf())).copied(),
            || self.active_session_index().map(|idx| self.sessions[idx].id),
        ) else {
            return;
        };
        let Some(abs) = self.sessions.iter().position(|s| s.id == id) else { return };
        let Some((origin, range)) = self.reorder_range(id) else { return };
        let lens: Vec<usize> =
            range.iter().map(|ws| self.workspace_reorder_indices(ws).len()).collect();
        let indices = self.workspace_reorder_indices(&origin);
        // A harness-backed session is in no reorderable list, so the step has
        // nowhere to start — the same silent no-op as a clamped end.
        let Some(index) = indices.iter().position(|i| *i == abs) else { return };
        let Some(target) = sidebar_nav::step_target(&range, &lens, &origin, index, delta) else {
            return;
        };
        // Follow the landing the move reports, not the one it was asked for:
        // expanding a project is persisted, so a refusal that still ran this
        // would leave a trace of a move that never happened.
        let Some(landed_in) = self.apply_session_move(id, target) else { return };
        if sidebar_focused {
            self.follow_moved_session(id, &landed_in);
        }
    }

    /// Keep the sidebar pointed at the session a key just moved.
    ///
    /// The cursor key is unchanged across a move inside one workspace, so
    /// neither `set_sidebar_cursor` nor the focus reconciler would notice the
    /// row moved and scroll after it — this sets the one-shot itself.  A
    /// landing inside a collapsed project expands it, because a cursor with no
    /// painted row is the state the reconciler treats as a row that went away.
    fn follow_moved_session(&mut self, id: SessionId, landed_in: &WorkspaceKey) {
        self.sidebar_cursor = Some(SidebarRow::Session(id));
        self.sidebar_cursor_moved = true;
        let Some(path) = landed_in.as_deref() else { return };
        let root = self
            .projects
            .iter()
            .find(|p| p.worktrees.iter().any(|w| w.path == path))
            .map(|p| p.root.clone());
        if let Some(root) = root {
            self.set_project_expanded(&root, true);
        }
    }

    fn scratchpad_session_index(&self, ws: &WorkspaceKey) -> Option<usize> {
        self.sessions.iter().position(|session| {
            session.working_directory == *ws
                && matches!(&session.kind, SessionKind::Scratchpad { .. })
        })
    }

    fn current_session_indices(&self) -> Vec<usize> {
        self.workspace_display_indices(&self.current_workspace)
    }

    fn active_session_index(&self) -> Option<usize> {
        let id = self.active_session.get(&self.current_workspace).copied()?;
        self.sessions.iter().position(|s| s.id == id)
    }

    fn set_active_in_current_workspace(&mut self, id: SessionId) {
        self.active_session.insert(self.current_workspace.clone(), id);
    }

    fn cycle_tabs(&mut self, delta: i32) {
        let indices = self.current_session_indices();
        if indices.len() < 2 {
            return;
        }
        let current = self.active_session_index().unwrap_or(indices[0]);
        let pos = indices.iter().position(|&i| i == current).unwrap_or(0);
        let len = indices.len() as i32;
        let new_pos = ((pos as i32 + delta).rem_euclid(len)) as usize;
        let id = self.sessions[indices[new_pos]].id;
        self.set_active_in_current_workspace(id);
    }

    fn cycle_workspaces(&mut self, ctx: &Context, delta: i32) {
        let order = self.workspace_order();
        if order.len() < 2 {
            return;
        }
        let cur_pos = order.iter().position(|w| *w == self.current_workspace).unwrap_or(0);
        let len = order.len() as i32;
        let new_pos = ((cur_pos as i32 + delta).rem_euclid(len)) as usize;
        match &order[new_pos] {
            None => self.activate_home(ctx),
            Some(p) => {
                let path = p.clone();
                self.activate_worktree(ctx, &path);
            },
        }
    }

    fn cycle_sessions(&mut self, ctx: &Context, delta: i32) {
        let ring: Vec<(WorkspaceKey, SessionId)> = self
            .workspace_order()
            .into_iter()
            .flat_map(|ws| {
                let entries: Vec<_> = self
                    .workspace_display_indices(&ws)
                    .into_iter()
                    .map(|i| (ws.clone(), self.sessions[i].id))
                    .collect();
                entries
            })
            .collect();
        let current = self.active_session_index().map(|i| self.sessions[i].id);
        let Some((target_ws, id)) = session_ring_target(&ring, current, delta) else {
            return;
        };
        // Record the target before switching: ensure_active_session would
        // otherwise re-adopt the workspace's previously active session.
        self.active_session.insert(target_ws.clone(), id);
        match target_ws {
            None => self.activate_home(ctx),
            Some(path) => self.activate_worktree(ctx, &path),
        }
    }

    /// Every workspace the app is willing to switch to, in sidebar order.
    /// Duplicates are kept: git lets two projects list one path, and
    /// dropping the second would change what `cycle_workspaces` visits for
    /// a user who configured nothing.
    fn workspace_order(&self) -> Vec<WorkspaceKey> {
        let mut order: Vec<WorkspaceKey> = vec![None];
        for project in &self.projects {
            for wt in &project.worktrees {
                let has_sessions = self.workspace_has_sessions(&Some(wt.path.clone()));
                if worktree_is_switchable(wt, self.liveness.missing(&wt.path), has_sessions) {
                    order.push(Some(wt.path.clone()));
                }
            }
        }
        order
    }

    /// The flat session ring, tagged with each workspace's owning project.
    /// Callers build it only under a ring policy: it allocates per removal.
    fn session_ring(&self) -> Vec<RingEntry> {
        self.workspace_order()
            .into_iter()
            .flat_map(|workspace| {
                let project =
                    sidebar_nav::project_of(&self.projects, &workspace).map(Path::to_path_buf);
                self.workspace_display_indices(&workspace)
                    .into_iter()
                    .map(|i| RingEntry {
                        project: project.clone(),
                        workspace: workspace.clone(),
                        id: self.sessions[i].id,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn add_project_via_dialog(&mut self, ctx: &Context) {
        let Some(path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        self.add_project_off_thread(ctx, wsl::normalize_root(path));
    }

    /// Put a project in the sidebar without stalling the frame: discovery
    /// opens the repository, lists worktrees, opens each one, and detects the
    /// default branch, none of which is free on a loaded machine (WSL roots
    /// also pay `wsl.exe`'s startup cost on top).  Every root goes in as a
    /// placeholder and discovers on a worker.
    fn add_project_off_thread(&mut self, ctx: &Context, path: PathBuf) {
        if self.projects.iter().any(|p| p.root == path) {
            return;
        }
        self.projects.push(Project::placeholder(path.clone()));
        let idx = self.projects.len() - 1;
        self.refresh_project(ctx, idx);
        self.persist_project(&path);
    }

    /// Tint whichever region a drop would land on while files are hovering, so
    /// three targets do not become a guessing game.  Silent off Windows: no
    /// cursor position is available there, so the tint would be a lie.
    fn paint_drop_hover(&self, ctx: &Context, regions: &file_drop::Regions) {
        let cfg = &self.config.ui.drop;
        if !cfg.enabled || !cfg.highlight || ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let Some(pointer) = file_drop::screen_pointer(ctx) else {
            return;
        };
        // winit's `DragOver` handler emits no event, so moving the cursor
        // mid-drag wakes nothing and the polled position would stay frozen at
        // wherever the drag entered.  This is the only place the feature drives
        // the loop, and it stops when the drag leaves or drops.
        ctx.request_repaint();
        let active_is_scratchpad =
            self.active_session_index().is_some_and(|idx| self.sessions[idx].scratchpad.is_some());
        let Some(target) = file_drop::route(Some(pointer), regions, active_is_scratchpad, cfg)
        else {
            return;
        };
        let rect = match target {
            file_drop::Target::ProjectsSidebar => match regions.sidebar {
                Some(rect) => rect,
                None => return,
            },
            file_drop::Target::Terminal | file_drop::Target::Scratchpad => regions.central,
        };
        // `Theme::accent` is already resolved (config accent, else ANSI blue);
        // `UiTheme::sidebar_accent` is the raw `Option` and would paint nothing
        // on an unconfigured palette.
        let accent = self.theme.accent;
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop_hover")))
            .rect_filled(rect, 0.0, accent.linear_multiply(0.15));
    }

    /// Send this frame's dropped files wherever they landed.  All of the
    /// deciding happens in `file_drop`; this only reaches the sinks.
    fn handle_dropped_files(&mut self, ctx: &Context, regions: &file_drop::Regions) {
        let paths: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if paths.is_empty() {
            return;
        }
        let active_is_scratchpad =
            self.active_session_index().is_some_and(|idx| self.sessions[idx].scratchpad.is_some());
        let pointer = file_drop::screen_pointer(ctx);
        let Some(target) =
            file_drop::route(pointer, regions, active_is_scratchpad, &self.config.ui.drop)
        else {
            log::debug!("drop at {pointer:?} lands on no enabled target, discarding {paths:?}");
            return;
        };
        match target {
            file_drop::Target::Terminal => {
                let Some(idx) = self.active_session_index() else {
                    log::debug!(
                        "drop on the terminal with no active session, discarding {paths:?}"
                    );
                    return;
                };
                let text = file_drop::shell_payload(
                    &paths,
                    self.sessions[idx].wsl_distro(),
                    &self.config.ui.drop.spelling,
                );
                if !text.is_empty() {
                    paste::paste(&mut self.sessions[idx], &text, true);
                }
            },
            file_drop::Target::Scratchpad => {
                let Some(idx) = self.active_session_index() else {
                    log::debug!(
                        "drop on the scratchpad with no active session, discarding {paths:?}"
                    );
                    return;
                };
                let id = self.sessions[idx].id;
                let Some(editor) = self.sessions[idx].scratchpad.as_mut() else {
                    return;
                };
                let (preceding, following) = editor.cursor_boundary(ctx, id);
                let text = file_drop::document_payload(&paths, preceding, following);
                editor.insert_at_cursor(ctx, id, &text);
            },
            file_drop::Target::ProjectsSidebar => {
                for root in file_drop::project_roots(&paths) {
                    self.add_project_off_thread(ctx, wsl::normalize_root(root));
                }
            },
        }

        // This runs after the sidebar and central panel have painted for the
        // frame, and eframe here is reactive, so the mutation above would
        // otherwise sit invisible until some unrelated event wakes the loop.
        ctx.request_repaint();
    }

    /// Drop a project from the sidebar.  Nothing on disk is touched, and
    /// sessions already open in its worktrees keep running — they outlive the
    /// sidebar entry the same way they outlive a workspace switch.
    fn remove_project(&mut self, idx: usize) -> PathBuf {
        let root = self.projects.remove(idx).root;
        let key = root.clone();
        state::mutate(move |s| s.projects.retain(|p| p.root != key));
        root
    }

    /// Move a project so it sits before display index `insert_before`, keyed by
    /// root so a drag that started before a background refresh still targets the
    /// right project.  `insert_before` counts positions in the pre-move list.
    fn move_project(&mut self, from_root: &Path, insert_before: usize) {
        let Some(from) = self.projects.iter().position(|p| p.root == *from_root) else {
            return;
        };
        let Some(to) = move_target(self.projects.len(), from, insert_before) else {
            return;
        };
        let project = self.projects.remove(from);
        self.projects.insert(to, project);
        self.persist_project_order();
    }

    /// Rewrite the persisted project order to match the in-memory list.  Roots
    /// only on disk (added by another window) keep their relative order at the
    /// end, so reordering here never drops a project this window can't see.
    fn persist_project_order(&self) {
        let order: Vec<PathBuf> = self.projects.iter().map(|p| p.root.clone()).collect();
        state::mutate(move |s| state::reorder_projects(s, &order));
    }

    fn is_modal_open(&self) -> bool {
        self.quit_dialog_open
            || self.pending_delete.is_some()
            || self.pending_create.is_some()
            || self.pending_session_close.is_some()
            || self.pending_rename.is_some()
            || self.pending_base_branch.is_some()
            || self.pending_project_remove.is_some()
            || self.error_dialog.is_some()
    }

    fn focus_sidebar(&mut self) {
        if !self.show_left_sidebar {
            self.show_left_sidebar = true;
            self.sidebar_auto_shown = true;
            self.persist_sidebars();
        }
        self.focus = PaneFocus::ProjectsSidebar;
        self.sidebar_cursor = Some(sidebar_nav::seed(
            &self.projects,
            self.current_workspace.as_deref(),
            &self.listed_workspace_rows(),
            self.active_session.get(&self.current_workspace).copied(),
        ));
        // Seeding reads the unfiltered tree, so a lingering filter from a prior
        // focus round-trip can leave the seeded row outside the current rows;
        // repair it immediately rather than waiting for the first key press.
        let rows = self.current_project_rows();
        self.sidebar_cursor = sidebar_nav::ensure_cursor(&rows, self.sidebar_cursor.as_ref());
        self.sidebar_cursor_moved = true;
        // Seeding rewrites the cursor from terminal state, which the overtaken
        // check would otherwise read as the user navigating.  The anchor
        // outlives a trip through the terminal by design.
        self.mark_sidebar_focus_write();
    }

    fn focus_git_sidebar(&mut self) {
        if !self.show_right_sidebar {
            self.show_right_sidebar = true;
            self.git_sidebar_auto_shown = true;
            self.persist_sidebars();
        }
        self.focus = PaneFocus::GitSidebar;
        // Rows come from the render pass, so seeding waits for it — leave the
        // cursor as-is and let the render pass repair it.
        self.git_cursor_moved = true;
    }

    fn focus_terminal(&mut self) {
        self.focus = PaneFocus::Terminal;
        if self.sidebar_auto_shown {
            self.show_left_sidebar = false;
            self.sidebar_auto_shown = false;
            self.persist_sidebars();
        }
        if self.git_sidebar_auto_shown {
            self.show_right_sidebar = false;
            self.git_sidebar_auto_shown = false;
            self.persist_sidebars();
        }
    }

    fn move_focus(&mut self, dir: FocusDir, origin: ActionOrigin) {
        let idx = self.active_session_index();
        let tui_running = idx.is_some_and(|i| self.sessions[i].nav_tui_running());
        let decision = focus_move(
            self.focus,
            dir,
            self.show_left_sidebar,
            self.show_right_sidebar,
            origin,
            tui_running,
        );
        match decision {
            FocusMove::Passthrough => {
                let Some(i) = idx else { return };
                let key = match dir {
                    FocusDir::Left => egui::Key::ArrowLeft,
                    FocusDir::Right => egui::Key::ArrowRight,
                };
                let mode = *self.sessions[i].term.lock().mode();
                // The binding consumed the key press before the terminal view
                // saw it, so the Ctrl+Arrow the inner TUI listens for is
                // re-synthesized with the terminal's own encoding.
                if let Some(bytes) =
                    crate::input::key_to_bytes(key, egui::Modifiers::CTRL, None, mode)
                {
                    self.sessions[i].write(bytes);
                }
            },
            FocusMove::Focus(PaneFocus::ProjectsSidebar) => self.focus_sidebar(),
            FocusMove::Focus(PaneFocus::Terminal) => self.focus_terminal(),
            FocusMove::Focus(PaneFocus::GitSidebar) => self.focus = PaneFocus::GitSidebar,
            FocusMove::Nothing => {},
        }
    }

    /// Match key events against the binding table (user bindings + defaults)
    /// before the terminal sees raw events, so a binding wins over plain
    /// text input.  Matched events are consumed unless every matched action
    /// is `ReceiveChar` (alacritty's pass-through marker).
    fn handle_shortcuts(&mut self, ctx: &Context) {
        let sidebar_focused = self.focus == PaneFocus::ProjectsSidebar && !self.palette.is_open();
        let git_focused = self.focus == PaneFocus::GitSidebar && !self.palette.is_open();
        let scratchpad_focused = self.focus == PaneFocus::Terminal
            && self
                .active_session_index()
                .is_some_and(|idx| self.sessions[idx].scratchpad.is_some());
        let actions: Vec<BindingAction> = ctx.input_mut(|i| {
            let mut actions = Vec::new();
            i.events.retain(|ev| {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                    let matched =
                        crate::bindings::all_matches(&self.config.bindings, *key, *modifiers);
                    // Sidebar-cursor actions only exist while the sidebar owns focus;
                    // anywhere else their keys (unmodified Home/End/PageUp/PageDown) are
                    // terminal input.  Stacked user bindings can mix a sidebar action with
                    // a global one on a single trigger, so filter per action — and if
                    // nothing else matched, let the event through untouched.
                    let matched: Vec<_> = matched
                        .into_iter()
                        .filter(|a| {
                            valid_for_focus(a, sidebar_focused, git_focused, scratchpad_focused)
                        })
                        // Search actions are owned by the sidebar nav pass; here
                        // their default Enter/Esc/Shift+Esc must fall through to
                        // the PTY when the terminal (or a non-searching panel)
                        // has focus.
                        .filter(|a| !matches!(a, BindingAction::Named(n) if n.is_search_scoped()))
                        // Palette cursor moves are owned by the palette modal,
                        // which suppresses this pass entirely while it is up.
                        // Reaching here means it is closed, so their keys belong
                        // to the sidebar or the PTY.
                        .filter(|a| !matches!(a, BindingAction::Named(n) if n.is_palette_scoped()))
                        .collect();
                    if !matched.is_empty() {
                        let suppress_chars = matched
                            .iter()
                            .all(|a| !matches!(a, BindingAction::Named(NamedAction::ReceiveChar)));
                        for a in matched {
                            actions.push(a.clone());
                        }
                        return !suppress_chars;
                    }
                }
                true
            });
            actions
        });
        for action in actions {
            let name = action.label();
            let started = std::time::Instant::now();
            self.dispatch_action(ctx, action, ActionOrigin::Keyboard);
            crate::frame_log::note_if_slow("action", name, started.elapsed());
        }
    }

    /// Arrow/Enter/Escape navigation while the projects sidebar owns
    /// keyboard focus.  Consumes only unmodified keys, so modifier-bound
    /// app shortcuts still match in `handle_shortcuts` afterwards.
    fn handle_sidebar_nav(&mut self, ctx: &Context) {
        let filter = &mut self.project_filter;
        let bindings = &self.config.bindings;
        let steps: Vec<SidebarNavStep> = ctx.input_mut(|i| {
            let mut steps = Vec::new();
            let text_keys = keys_paired_with_text(&i.events);
            let mut idx = 0;
            i.events.retain(|ev| {
                let produced_text = text_keys[idx];
                idx += 1;
                match ev {
                    egui::Event::Text(text) => match filter.on_text(text) {
                        Some(outcome) => {
                            steps.push(SidebarNavStep::Filter(outcome));
                            false
                        },
                        None => true,
                    },
                    egui::Event::Key { key, pressed: true, modifiers, .. } => drain_search_or_nav(
                        &mut steps,
                        filter,
                        bindings,
                        *key,
                        *modifiers,
                        produced_text,
                    ),
                    _ => true,
                }
            });
            steps
        });
        for step in steps {
            match step {
                SidebarNavStep::Filter(outcome) => self.apply_filter_outcome(outcome),
                SidebarNavStep::Nav(key) => self.apply_sidebar_nav(ctx, key),
                SidebarNavStep::SearchAction(action) => {
                    self.dispatch_action(ctx, BindingAction::Named(action), ActionOrigin::Keyboard);
                },
            }
        }
    }

    fn apply_filter_outcome(&mut self, outcome: panel_filter::Outcome) {
        use panel_filter::Outcome;
        match outcome {
            // The reconciler repairs the cursor later in this same update, from
            // a snapshot that still knows which row the filter hid.  Repairing
            // here would reset it before anything could observe that.
            Outcome::FilterChanged => {},
            Outcome::Consumed => {},
            Outcome::MoveCursor(delta) => self.move_sidebar_cursor(delta),
            Outcome::LeavePanel => self.focus_terminal(),
        }
    }

    fn move_sidebar_cursor(&mut self, delta: i32) {
        let rows = self.current_project_rows();
        let cursor = match self.sidebar_cursor.clone() {
            Some(c) if rows.contains(&c) => c,
            _ => {
                if let Some(first) = rows.first() {
                    self.set_sidebar_cursor(first.clone());
                }
                return;
            },
        };
        self.set_sidebar_cursor(sidebar_nav::step(&rows, &cursor, delta));
    }

    /// Home/End for the sidebar cursor: first or last of the rows the arrow
    /// keys step over (the filtered set while a filter is active).
    fn sidebar_cursor_to_edge(&mut self, top: bool) {
        let rows = self.current_project_rows();
        let target = if top { rows.first() } else { rows.last() };
        if let Some(row) = target.cloned() {
            self.set_sidebar_cursor(row);
        }
    }

    /// PageUp/PageDown for the sidebar cursor: the nearest project header
    /// above/below, clamped at the extremes.  A stale cursor reseats on the
    /// first row, same as `apply_sidebar_nav`.
    fn sidebar_cursor_project_jump(&mut self, delta: i32) {
        let rows = self.current_project_rows();
        let Some(cursor) = self.sidebar_cursor.clone().filter(|c| rows.contains(c)) else {
            if let Some(first) = rows.first() {
                self.set_sidebar_cursor(first.clone());
            }
            return;
        };
        let target = if delta > 0 {
            sidebar_nav::next_project(&rows, &cursor)
        } else {
            sidebar_nav::previous_project(&rows, &cursor)
        };
        if let Some(row) = target {
            self.set_sidebar_cursor(row);
        }
    }

    /// Rows the sidebar cursor steps over this frame: the fuzzy/toggle-filtered
    /// set while a filter is active, the full visible set otherwise.
    fn current_project_rows(&mut self) -> Vec<SidebarRow> {
        let listed = self.listed_workspace_rows();
        if !self.project_filter.is_filtering() {
            return sidebar_nav::visible_rows(&self.projects, &listed);
        }

        let apply = self.project_filter.toggles_apply(self.search_scope);
        let toggle_sessions = apply && self.project_filter.is_toggled('s');
        let toggle_attention = apply && self.project_filter.is_toggled('a');
        let pr_open = apply && self.project_filter.is_toggled('o');
        let pr_draft = apply && self.project_filter.is_toggled('d');
        let pr_merged = apply && self.project_filter.is_toggled('m');
        let pr_closed = apply && self.project_filter.is_toggled('c');
        let any_pr = pr_open || pr_draft || pr_merged || pr_closed;
        let any_toggle = any_project_toggle_active(toggle_sessions, toggle_attention, any_pr);

        // Precompute every fuzzy result before building the closures: the
        // matcher needs `&mut self.project_filter`, and releasing that borrow
        // up-front lets the predicates read the rest of `&self` freely.
        let home_matches = self.project_filter.matches("Home");
        let project_matches: HashMap<PathBuf, bool> = {
            let filter = &mut self.project_filter;
            self.projects
                .iter()
                .map(|p| (p.root.clone(), filter.matches(p.display_name())))
                .collect()
        };
        let worktree_matches: HashMap<PathBuf, bool> = {
            let filter = &mut self.project_filter;
            self.projects
                .iter()
                .flat_map(|p| p.worktrees.iter())
                .map(|wt| (wt.path.clone(), filter.matches(&wt.name)))
                .collect()
        };
        let live_branch = self
            .current_workspace
            .as_deref()
            .and_then(|p| self.git_status.get(p))
            .and_then(|c| c.current_branch());
        let current_workspace = self.current_workspace.as_deref();
        // Skipped outright while the PR dimension is inert: `worktree_pr_passes`
        // would not read the map, and building it costs a path clone per
        // worktree on a call that runs whenever the panel is filtering at all.
        let pr_matches: HashMap<PathBuf, bool> = if any_pr {
            self.projects
                .iter()
                .flat_map(|p| p.worktrees.iter())
                .map(|wt| {
                    let branch = pr_status::effective_branch(wt, current_workspace, live_branch);
                    let state = self.pr_cache.state(&wt.path, branch);
                    (
                        wt.path.clone(),
                        pr_status::pr_pass(state, pr_open, pr_draft, pr_merged, pr_closed),
                    )
                })
                .collect()
        } else {
            HashMap::new()
        };

        let toggles_pass = |key: &WorkspaceKey| {
            project_toggles_pass(
                apply,
                toggle_sessions,
                self.workspace_has_sessions(key),
                toggle_attention,
                self.workspace_needs_attention(key),
            )
        };
        let home = home_matches && toggles_pass(&None);
        let project_self =
            |p: &Project| !any_toggle && project_matches.get(&p.root).copied().unwrap_or(false);
        let mut worktree = |_p: &Project, wt: &Worktree| {
            worktree_matches.get(&wt.path).copied().unwrap_or(false)
                && toggles_pass(&Some(wt.path.clone()))
                && worktree_pr_passes(any_pr, &pr_matches, &wt.path)
        };
        sidebar_nav::filtered_rows(&self.projects, &listed, sidebar_nav::RowPredicates {
            home,
            project_self: &project_self,
            worktree: &mut worktree,
        })
    }

    fn workspace_has_sessions(&self, key: &WorkspaceKey) -> bool {
        self.sessions.iter().any(|s| s.working_directory == *key)
    }

    /// Every live session as a `(workspace, id)` pair — the same shape
    /// `close_fallback` takes, and the model the
    /// focus reconciler observes.
    fn session_pairs(&self) -> Vec<(WorkspaceKey, SessionId)> {
        self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect()
    }

    /// Live sessions borrowed for the unchanged-inputs check, which runs on
    /// every frame and must not allocate.
    fn session_inputs(&self) -> impl Iterator<Item = sidebar_focus::SessionInput<'_>> {
        self.sessions.iter().map(|s| sidebar_focus::SessionInput {
            workspace: &s.working_directory,
            id: s.id,
            attention: s.needs_attention,
        })
    }

    fn sidebar_snapshot(&mut self, skip_worktree: Option<&Path>) -> sidebar_focus::TreeSnapshot {
        let active_workspace = self.current_workspace.as_deref();
        let active_branch =
            active_workspace.and_then(|p| self.git_status.get(p)).and_then(|c| c.current_branch());
        let inputs = sidebar_focus::ObservedInputs::capture(
            &self.projects,
            self.session_inputs(),
            sidebar_focus::UiInputs {
                session_rows_always: self.session_rows_always,
                query: self.project_filter.query(),
                toggles: self.project_filter.toggle_bits(),
                toggles_apply: self.project_filter.toggles_apply(self.search_scope),
                pr_generation: pr_generation_for(
                    self.pr_cache.generation(),
                    any_pr_toggle_active(&self.project_filter, self.search_scope),
                ),
                active_workspace,
                active_branch,
                herdr_generation: self.herdr_generation(),
            },
        );
        let rows = self.current_project_rows();
        let live = self.session_pairs();
        let listed = self.listed_workspace_rows();
        let snapshot =
            build_sidebar_snapshot(&self.projects, &live, &listed, &rows, skip_worktree, inputs);
        // Paint reuses these until the next rebuild, so an unchanged filtering
        // frame runs no fuzzy matching at all.
        self.sidebar_rows_cache = Some(rows);
        snapshot
    }

    /// Repair the sidebar cursor against what changed since the last pass.
    /// Called twice per `update` — before paint for everything the input and
    /// background drains produced, and again at the end for what only
    /// `reap_exited_sessions` and paint-time clicks can produce.  A pass with
    /// nothing to do costs one `ObservedInputs` compare, which is the whole
    /// steady-state budget: there is no setting that skips this.
    fn reconcile_sidebar_focus(&mut self, ctx: &Context) {
        if sidebar_focus_overtaken(
            &self.sidebar_focus_written,
            self.sidebar_cursor.as_ref(),
            &self.current_workspace,
            self.active_session.get(&self.current_workspace).copied(),
        ) {
            self.sidebar_anchor = None;
        }

        let deferred = self.sidebar_deferred_close.take();
        let skip = deferred.as_ref().and_then(|d| d.removed_worktree.clone());

        if deferred.is_none() {
            let active_workspace = self.current_workspace.as_deref();
            let active_branch = active_workspace
                .and_then(|p| self.git_status.get(p))
                .and_then(|c| c.current_branch());
            if let Some(prev) = &self.sidebar_focus_prev {
                let unchanged = prev.inputs.matches(
                    &self.projects,
                    self.session_inputs(),
                    sidebar_focus::UiInputs {
                        session_rows_always: self.session_rows_always,
                        query: self.project_filter.query(),
                        toggles: self.project_filter.toggle_bits(),
                        toggles_apply: self.project_filter.toggles_apply(self.search_scope),
                        pr_generation: pr_generation_for(
                            self.pr_cache.generation(),
                            any_pr_toggle_active(&self.project_filter, self.search_scope),
                        ),
                        active_workspace,
                        active_branch,
                        herdr_generation: self.herdr_generation(),
                    },
                );
                if unchanged {
                    return;
                }
            }
        }

        let next = self.sidebar_snapshot(skip.as_deref());
        let prev = self.sidebar_focus_prev.take().unwrap_or_else(|| next.clone());
        let outcome = sidebar_focus::repair(
            &prev,
            &next,
            self.sidebar_cursor.as_ref(),
            self.sidebar_anchor.as_ref(),
        );

        if outcome.cursor != self.sidebar_cursor {
            self.sidebar_cursor = outcome.cursor;
            self.sidebar_cursor_moved = true;
        }
        self.sidebar_anchor = outcome.anchor;
        self.sidebar_focus_prev = Some(next);

        if self.config.ui.sidebar_focus.follows() {
            match (outcome.follow, deferred) {
                (Some(target), _) => self.apply_follow_target(ctx, target),
                // Nothing live to land on, so the verdict this pass took over
                // from still decides where the terminal goes.
                (None, Some(deferred)) => self.apply_close_fallback(ctx, deferred.verdict),
                (None, None) => {},
            }
        }

        self.mark_sidebar_focus_write();
    }

    /// Record the current focus triple as the reconciler's own, so the next
    /// pass does not mistake it for the user navigating.
    fn mark_sidebar_focus_write(&mut self) {
        self.sidebar_focus_written = Some(SidebarFocusWrite {
            cursor: self.sidebar_cursor.clone(),
            workspace: self.current_workspace.clone(),
            active: self.active_session.get(&self.current_workspace).copied(),
        });
    }

    /// Move the terminal to a removal landing.  A workspace target adopts its
    /// active session, or its first live one when that entry went stale.
    fn apply_follow_target(&mut self, ctx: &Context, target: sidebar_focus::FollowTarget) {
        match target {
            sidebar_focus::FollowTarget::Session(id) => self.activate_session_by_id(id),
            sidebar_focus::FollowTarget::Workspace(ws) => {
                let id = self
                    .active_session
                    .get(&ws)
                    .copied()
                    .filter(|id| self.sessions.iter().any(|s| s.id == *id))
                    .or_else(|| {
                        self.sessions.iter().find(|s| s.working_directory == ws).map(|s| s.id)
                    });
                if let Some(id) = id {
                    self.activate_session_by_id(id);
                }
            },
        }
        ctx.request_repaint();
    }

    fn apply_sidebar_nav(&mut self, ctx: &Context, key: egui::Key) {
        use egui::Key;
        let rows = self.current_project_rows();
        let cursor = match self.sidebar_cursor.clone() {
            Some(c) if rows.contains(&c) => c,
            // Stale or unseeded cursor (worktree removed, project collapsed
            // by mouse, or a filter toggle narrowing the rows out from under
            // it): land on the first row and let the next press act from
            // there. Unfiltered `rows` always leads with Home.
            _ => {
                if let Some(first) = rows.first() {
                    self.set_sidebar_cursor(first.clone());
                }
                return;
            },
        };
        match key {
            Key::ArrowUp => self.set_sidebar_cursor(sidebar_nav::step(&rows, &cursor, -1)),
            Key::ArrowDown => self.set_sidebar_cursor(sidebar_nav::step(&rows, &cursor, 1)),
            Key::ArrowRight => match &cursor {
                SidebarRow::Project(root) => {
                    let root = root.clone();
                    self.set_project_expanded(&root, true);
                },
                SidebarRow::Session(id) => {
                    let id = *id;
                    self.activate_session_by_id(id);
                    self.focus_terminal();
                },
                _ => {},
            },
            Key::ArrowLeft => match &cursor {
                SidebarRow::Project(root) => self.set_project_expanded(root, false),
                SidebarRow::Worktree(_) | SidebarRow::Session(_) | SidebarRow::HerdrAgent(..) => {
                    if let Some(target) = sidebar_nav::left_target(&rows, &cursor) {
                        self.set_sidebar_cursor(target);
                    }
                },
                SidebarRow::Home => {},
            },
            Key::Enter => self.activate_sidebar_row(ctx, &cursor),
            Key::Escape => self.focus_terminal(),
            _ => {},
        }
    }

    /// Enter on a cursor row: open Home/worktree sessions and return focus to
    /// the terminal, or toggle a project header's expansion in place.
    fn activate_sidebar_row(&mut self, ctx: &Context, cursor: &SidebarRow) {
        match cursor {
            SidebarRow::Home => {
                self.activate_home(ctx);
                self.focus_terminal();
            },
            SidebarRow::Worktree(path) => {
                let path = path.clone();
                self.activate_worktree(ctx, &path);
                self.focus_terminal();
            },
            SidebarRow::Session(id) => {
                let id = *id;
                self.activate_session_by_id(id);
                self.focus_terminal();
            },
            SidebarRow::Project(root) => {
                let root = root.clone();
                let expanded =
                    self.projects.iter().find(|p| p.root == root).is_some_and(|p| p.expanded);
                self.set_project_expanded(&root, !expanded);
            },
            SidebarRow::HerdrAgent(side, terminal_id) => {
                let side = side.clone();
                let terminal_id = terminal_id.clone();
                let pane_id = self.find_herdr_agent(&side, &terminal_id).map(|a| a.pane_id.clone());
                let workspace = self.herdr_row_workspace(&side, &terminal_id);
                if let (Some(pane_id), Some(workspace)) = (pane_id, workspace) {
                    let key = herdr::HerdrKey { side, terminal_id };
                    // Switches first, same as the click path: a refusal is
                    // only visible if the workspace it happened in is on
                    // screen.
                    let previous =
                        std::mem::replace(&mut self.current_workspace, workspace.clone());
                    if self.attach_herdr_agent(ctx, key, &pane_id, workspace) {
                        self.focus_terminal();
                    } else {
                        self.current_workspace = previous;
                    }
                }
            },
        }
    }

    /// Switch to the session's workspace and mark it active — the keyboard
    /// equivalent of clicking its sidebar row.  A stale id (session reaped
    /// this frame) self-heals next frame via `ensure_active_session`.
    fn activate_session_by_id(&mut self, id: SessionId) {
        let Some(ws) =
            self.sessions.iter().find(|s| s.id == id).map(|s| s.working_directory.clone())
        else {
            return;
        };
        self.current_workspace = ws.clone();
        self.active_session.insert(ws, id);
    }

    fn set_sidebar_cursor(&mut self, row: SidebarRow) {
        if self.sidebar_cursor.as_ref() != Some(&row) {
            self.sidebar_cursor = Some(row);
            self.sidebar_cursor_moved = true;
        }
    }

    /// Arrow/Enter/Escape navigation while the git sidebar owns keyboard
    /// focus.  Same event-drain shape as `handle_sidebar_nav`: consumes only
    /// unmodified nav keys, leaving modifier-bound shortcuts for
    /// `handle_shortcuts`.
    fn handle_git_sidebar_nav(&mut self, ctx: &Context) {
        let filter = &mut self.git_filter;
        let bindings = &self.config.bindings;
        let steps: Vec<SidebarNavStep> = ctx.input_mut(|i| {
            let mut steps = Vec::new();
            let text_keys = keys_paired_with_text(&i.events);
            let mut idx = 0;
            i.events.retain(|ev| {
                let produced_text = text_keys[idx];
                idx += 1;
                match ev {
                    egui::Event::Text(text) => match filter.on_text(text) {
                        Some(outcome) => {
                            steps.push(SidebarNavStep::Filter(outcome));
                            false
                        },
                        None => true,
                    },
                    egui::Event::Key { key, pressed: true, modifiers, .. } => drain_search_or_nav(
                        &mut steps,
                        filter,
                        bindings,
                        *key,
                        *modifiers,
                        produced_text,
                    ),
                    _ => true,
                }
            });
            steps
        });
        for step in steps {
            match step {
                SidebarNavStep::Filter(outcome) => self.apply_git_filter_outcome(ctx, outcome),
                SidebarNavStep::Nav(key) => self.apply_git_sidebar_nav(ctx, key),
                SidebarNavStep::SearchAction(action) => {
                    self.dispatch_action(ctx, BindingAction::Named(action), ActionOrigin::Keyboard);
                },
            }
        }
    }

    fn apply_git_filter_outcome(&mut self, _ctx: &Context, outcome: panel_filter::Outcome) {
        use panel_filter::Outcome;
        match outcome {
            Outcome::FilterChanged => self.after_git_filter_changed(),
            Outcome::Consumed => {},
            Outcome::MoveCursor(delta) => self.move_git_cursor(delta),
            Outcome::LeavePanel => self.focus_terminal(),
        }
    }

    /// Repair the git cursor after the row set narrows or widens: recompute the
    /// filtered rows from the cached status so the next key event acts on them,
    /// then keep the cursor where it is when still visible, else fall to the
    /// first surviving row.
    fn after_git_filter_changed(&mut self) {
        self.recompute_git_rows();
        let next = git_nav::ensure_cursor(&self.git_rows, self.git_cursor.as_ref());
        if next.as_ref() != self.git_cursor.as_ref() {
            self.git_cursor = next;
            self.git_cursor_moved = true;
        }
    }

    fn move_git_cursor(&mut self, delta: i32) {
        let cursor = match self.git_cursor.clone() {
            Some(c) if self.git_rows.contains(&c) => c,
            _ => {
                if let Some(first) = self.git_rows.first().cloned() {
                    self.set_git_cursor(first);
                }
                return;
            },
        };
        if let Some(row) = git_nav::step(&self.git_rows, &cursor, delta) {
            self.set_git_cursor(row);
        }
    }

    /// Rebuild `git_rows` from the cached status under the active filter,
    /// without polling.  The render pass recomputes the same way from a fresh
    /// poll; this keeps the row set current between frames so a filter change
    /// and a following key event in the same batch agree on the rows.
    fn recompute_git_rows(&mut self) {
        let Some(path) = self.active_session_path() else {
            self.git_rows.clear();
            return;
        };
        let Some(status) = self.git_status.get(&path).map(|c| c.last().clone()) else {
            self.git_rows.clear();
            return;
        };
        self.git_rows = self.filtered_git_rows(&status).rows;
    }

    /// Apply the git panel's kind toggles and fuzzy query to a status snapshot.
    /// With no kind toggle active every kind passes; otherwise the active
    /// toggles union (`m`: Modified/Renamed, `d`: Deleted, `u`: Untracked/Added).
    /// Conflicted rows and the branch-diff section are handled by `visible_rows`.
    fn filtered_git_rows(&mut self, status: &GitStatus) -> git_nav::GitRows {
        let apply = self.git_filter.toggles_apply(self.search_scope);
        let m = apply && self.git_filter.is_toggled('m');
        let d = apply && self.git_filter.is_toggled('d');
        let u = apply && self.git_filter.is_toggled('u');
        let kind_pass = move |k: ChangeKind| git_toggles_pass(m, d, u, k);
        let filter = &mut self.git_filter;
        let mut query_pass = |path: &str| filter.matches(path);
        git_nav::visible_rows(
            &status.staged,
            &status.unstaged,
            &status.branch_diff,
            &kind_pass,
            &mut query_pass,
        )
    }

    fn apply_git_sidebar_nav(&mut self, ctx: &Context, key: egui::Key) {
        use egui::Key;
        let cursor = match self.git_cursor.clone() {
            Some(c) if self.git_rows.contains(&c) => c,
            // Stale or unseeded cursor (status refreshed the row out from under
            // it): land on the first row and let the next press act from there.
            _ => {
                if let Some(first) = self.git_rows.first().cloned() {
                    self.set_git_cursor(first);
                }
                return;
            },
        };
        match key {
            Key::ArrowUp => {
                if let Some(row) = git_nav::step(&self.git_rows, &cursor, -1) {
                    self.set_git_cursor(row);
                }
            },
            Key::ArrowDown => {
                if let Some(row) = git_nav::step(&self.git_rows, &cursor, 1) {
                    self.set_git_cursor(row);
                }
            },
            Key::Enter => {
                if let Some(req) = git_row_diff_request(&cursor, self.git_branch_base.as_deref()) {
                    self.open_diff(ctx, req);
                }
            },
            Key::Escape => self.focus_terminal(),
            _ => {},
        }
    }

    fn set_git_cursor(&mut self, row: git_nav::GitRow) {
        if self.git_cursor.as_ref() != Some(&row) {
            self.git_cursor = Some(row);
            self.git_cursor_moved = true;
        }
    }

    fn set_project_expanded(&mut self, root: &Path, expanded: bool) {
        if let Some(p) = self.projects.iter_mut().find(|p| p.root == *root) {
            if p.expanded != expanded {
                p.expanded = expanded;
                self.persist_project(root);
            }
        }
    }

    fn dispatch_action(&mut self, ctx: &Context, action: BindingAction, origin: ActionOrigin) {
        // A palette row is dispatched with the panel still searching, and the
        // cursor operations below act on a row the query may have hidden.  The
        // keyboard path cannot reach here mid-query at all: a letter's text is
        // swallowed by the query before the binding table sees the key.
        if origin == ActionOrigin::Palette
            && matches!(&action, BindingAction::Named(n) if n.requires_project_browsing())
            && self.project_filter.mode() != panel_filter::Mode::Browsing
        {
            return;
        }
        match action {
            BindingAction::Chars(bytes) => {
                if let Some(idx) = self.active_session_index() {
                    let id = self.sessions[idx].id;
                    if let Some(editor) = self.sessions[idx].scratchpad.as_mut() {
                        // Custom `Chars` bindings can carry terminal control
                        // sequences (Shift+Tab is ESC [ Z, for example).  A
                        // document should only accept actual text here; native
                        // editing keys are handled by egui's TextEdit itself.
                        if let Ok(text) = std::str::from_utf8(&bytes)
                            && !text.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
                        {
                            editor.insert_at_cursor(ctx, id, text);
                        }
                    } else {
                        paste::on_terminal_input_start(&self.sessions[idx]);
                        self.sessions[idx].write(bytes);
                    }
                }
            },
            BindingAction::Named(NamedAction::Paste) => {
                self.paste_from_clipboard(ctx, Target::Clipboard);
            },
            BindingAction::Named(NamedAction::PasteSelection) => {
                self.paste_from_clipboard(ctx, Target::Primary);
            },
            BindingAction::Named(NamedAction::Copy) => {
                if let Some(idx) = self.active_session_index() {
                    if let Some(editor) = self.sessions[idx].scratchpad.as_ref() {
                        if let Some(text) = editor.selected_text(ctx, self.sessions[idx].id) {
                            clipboard::write(Target::Clipboard, &text);
                        }
                    } else {
                        paste::copy_selection(&self.sessions[idx], &self.config, Target::Clipboard);
                    }
                }
            },
            BindingAction::Named(NamedAction::CopySelection) => {
                if let Some(idx) = self.active_session_index() {
                    if let Some(editor) = self.sessions[idx].scratchpad.as_ref() {
                        if let Some(text) = editor.selected_text(ctx, self.sessions[idx].id) {
                            clipboard::write(Target::Primary, &text);
                        }
                    } else {
                        paste::copy_selection(&self.sessions[idx], &self.config, Target::Primary);
                    }
                }
            },
            BindingAction::Named(NamedAction::SpawnNewInstance) => {
                let ws = self.current_workspace.clone();
                if let Err(e) = self.spawn_session(ctx, ws.clone()) {
                    self.report_spawn_failure(ctx, &ws, &e);
                }
            },
            BindingAction::Named(NamedAction::Quit) => {
                self.quit_dialog_open = true;
            },
            BindingAction::Named(NamedAction::ClearHistory) => {
                use alacritty_terminal::vte::ansi::{ClearMode, Handler};
                if let Some(idx) = self.active_session_index() {
                    if self.sessions[idx].scratchpad.is_none() {
                        self.sessions[idx].term.lock().clear_screen(ClearMode::Saved);
                    }
                }
            },
            BindingAction::Named(NamedAction::ToggleFullscreen) => {
                let on = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!on));
            },
            BindingAction::Named(NamedAction::ToggleMaximized) => {
                let on = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!on));
            },
            BindingAction::Named(NamedAction::Minimize) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            },
            BindingAction::Named(NamedAction::SelectNextTab) => self.cycle_tabs(1),
            BindingAction::Named(NamedAction::SelectPreviousTab) => self.cycle_tabs(-1),
            BindingAction::Named(NamedAction::SelectNextSession) => self.cycle_sessions(ctx, 1),
            BindingAction::Named(NamedAction::SelectPreviousSession) => {
                self.cycle_sessions(ctx, -1);
            },
            BindingAction::Named(NamedAction::SelectTab(n)) => self.select_tab(n),
            BindingAction::Named(NamedAction::SelectLastTab) => self.select_last_tab(),
            BindingAction::Named(NamedAction::SpawnProfile(n)) => {
                match self.config.profiles.get((n - 1) as usize).map(|p| p.name.clone()) {
                    Some(name) => self.spawn_profile_session(ctx, &name),
                    None => {
                        log::warn!(
                            "SpawnProfile{n}: only {} profiles configured",
                            self.config.profiles.len()
                        );
                        self.error_dialog = Some(format!("SpawnProfile{n}: no such profile"));
                    },
                }
            },
            BindingAction::Named(NamedAction::NoOp) => {},
            BindingAction::Named(NamedAction::ReceiveChar) => {},
            BindingAction::Named(NamedAction::ToggleLeftSidebar) => {
                self.show_left_sidebar = !self.show_left_sidebar;
                // A deliberate visibility change opts out of the auto-shown
                // round trip, and a hidden sidebar cannot keep keyboard focus.
                self.sidebar_auto_shown = false;
                if !self.show_left_sidebar && self.focus == PaneFocus::ProjectsSidebar {
                    self.focus = PaneFocus::Terminal;
                }
                self.persist_sidebars();
            },
            BindingAction::Named(NamedAction::ToggleRightSidebar) => {
                self.show_right_sidebar = !self.show_right_sidebar;
                // A deliberate visibility change opts out of the auto-shown
                // round trip, and a hidden sidebar cannot keep keyboard focus.
                self.git_sidebar_auto_shown = false;
                if !self.show_right_sidebar && self.focus == PaneFocus::GitSidebar {
                    self.focus = PaneFocus::Terminal;
                }
                self.persist_sidebars();
            },
            BindingAction::Named(NamedAction::ToggleSessionRows) => {
                self.session_rows_always = !self.session_rows_always;
            },
            BindingAction::Named(NamedAction::ToggleSessionTabs) => {
                self.session_tabs_always = !self.session_tabs_always;
            },
            BindingAction::Named(NamedAction::MoveSessionUp) => self.step_session(-1),
            BindingAction::Named(NamedAction::MoveSessionDown) => self.step_session(1),
            BindingAction::Named(NamedAction::ToggleSessionDrag) => {
                self.session_drag = !self.session_drag;
            },
            BindingAction::Named(NamedAction::SelectNextWorkspace) => {
                self.cycle_workspaces(ctx, 1);
            },
            BindingAction::Named(NamedAction::SelectPreviousWorkspace) => {
                self.cycle_workspaces(ctx, -1);
            },
            BindingAction::Named(NamedAction::OpenScratchpad) => self.toggle_scratchpad_tab(ctx),
            BindingAction::Named(NamedAction::AddProject) => self.add_project_via_dialog(ctx),
            BindingAction::Named(NamedAction::ToggleSidebarFocus) => match self.focus {
                PaneFocus::Terminal => self.focus_sidebar(),
                PaneFocus::ProjectsSidebar => self.focus_terminal(),
                // Toggle stays "left ↔ terminal"; from the right panel it hops
                // to the left one rather than doing nothing.
                PaneFocus::GitSidebar => self.focus_sidebar(),
            },
            BindingAction::Named(NamedAction::CloseSession) => {
                let cursored = if self.focus == PaneFocus::ProjectsSidebar {
                    match &self.sidebar_cursor {
                        Some(SidebarRow::Session(id)) => Some(*id),
                        _ => None,
                    }
                } else {
                    None
                };
                let target = cursored
                    .or_else(|| self.active_session_index().map(|idx| self.sessions[idx].id));
                if let Some(id) = target {
                    self.request_close_session(ctx, id);
                }
            },
            BindingAction::Named(NamedAction::SidebarTop) => self.sidebar_cursor_to_edge(true),
            BindingAction::Named(NamedAction::SidebarBottom) => self.sidebar_cursor_to_edge(false),
            BindingAction::Named(NamedAction::SidebarNextProject) => {
                self.sidebar_cursor_project_jump(1)
            },
            BindingAction::Named(NamedAction::SidebarPreviousProject) => {
                self.sidebar_cursor_project_jump(-1)
            },
            BindingAction::Named(NamedAction::RefreshProjects) => {
                self.refresh_all_projects(ctx);
            },
            BindingAction::Named(NamedAction::DeleteSelected) => {
                match self.sidebar_cursor.clone() {
                    Some(SidebarRow::Session(id)) => self.request_close_session(ctx, id),
                    Some(SidebarRow::Worktree(path)) => self.request_worktree_delete(&path),
                    Some(SidebarRow::Project(root)) => {
                        if let Some(p) = self.projects.iter().find(|p| p.root == root) {
                            self.pending_project_remove = Some(ProjectRemoveState {
                                name: p.display_name().to_string(),
                                root,
                            });
                        }
                    },
                    Some(SidebarRow::Home) | Some(SidebarRow::HerdrAgent(..)) | None => {},
                }
            },
            BindingAction::Named(NamedAction::RenameSelected) => {
                // Only project rows carry an editable label; sessions and
                // worktrees take their names from the terminal title and the
                // `[ui] worktree_name` template.
                if let Some(SidebarRow::Project(root)) = self.sidebar_cursor.clone() {
                    if let Some(p) = self.projects.iter().find(|p| p.root == root) {
                        self.pending_rename =
                            Some(RenameState { root, label: p.display_name().to_string() });
                    }
                }
            },
            BindingAction::Named(NamedAction::ToggleProjectExpanded) => {
                let Some(cursor) = self.sidebar_cursor.clone() else {
                    return;
                };
                let root = {
                    let session_workspace = |id: SessionId| {
                        self.sessions
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.working_directory.clone())
                    };
                    row_project_root(&self.projects, session_workspace, &cursor)
                };
                if let Some(root) = root {
                    let expanded =
                        self.projects.iter().find(|p| p.root == root).is_some_and(|p| p.expanded);
                    self.set_project_expanded(&root, !expanded);
                    // Collapsing hides the cursored child; move the cursor to
                    // the header so it doesn't point at a now-invisible row.
                    if expanded && !matches!(cursor, SidebarRow::Project(_)) {
                        self.set_sidebar_cursor(SidebarRow::Project(root));
                    }
                }
            },
            BindingAction::Named(NamedAction::TogglePalette) => {
                self.palette.toggle();
            },
            BindingAction::Named(NamedAction::FocusProjectsSidebar) => {
                if self.focus != PaneFocus::ProjectsSidebar {
                    self.focus_sidebar();
                }
            },
            BindingAction::Named(NamedAction::FocusGitSidebar) => {
                if self.focus != PaneFocus::GitSidebar {
                    self.focus_git_sidebar()
                } else {
                    self.focus_terminal()
                }
            },
            BindingAction::Named(NamedAction::FocusTerminal) => self.focus_terminal(),
            BindingAction::Named(NamedAction::FocusLeft) => {
                self.move_focus(FocusDir::Left, origin);
            },
            BindingAction::Named(NamedAction::FocusRight) => {
                self.move_focus(FocusDir::Right, origin);
            },
            BindingAction::Named(NamedAction::SetBaseBranch) => {
                let target = base_branch_target(
                    self.focus == PaneFocus::ProjectsSidebar,
                    self.sidebar_cursor.as_ref(),
                    |id| {
                        self.sessions
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.working_directory.clone())
                    },
                    &self.current_workspace,
                );
                if let Some(path) = target {
                    self.open_base_branch_picker(path);
                }
            },
            BindingAction::Named(NamedAction::SidebarSearchConfirm) => {
                self.sidebar_search_confirm();
            },
            BindingAction::Named(NamedAction::SidebarSearchCancel) => {
                self.sidebar_search_cancel();
            },
            BindingAction::Named(NamedAction::SidebarSearchCancelToTerminal) => {
                self.sidebar_search_cancel_to_terminal();
            },
            BindingAction::Named(NamedAction::ClearProjectFilters) => {
                self.project_filter.clear_toggles();
            },
            BindingAction::Named(NamedAction::ClearGitFilters) => {
                self.git_filter.clear_toggles();
                self.after_git_filter_changed();
            },
            BindingAction::Named(NamedAction::ToggleSearchScope) => {
                self.search_scope = match self.search_scope {
                    SearchScope::Filtered => SearchScope::All,
                    SearchScope::All => SearchScope::Filtered,
                };
            },
            BindingAction::Named(NamedAction::RefreshPrStatus) => {
                self.pr_cache.invalidate_all();
                // The poll sites run while the sidebars paint, and the palette
                // dispatches after both have; without a wake the re-query would
                // wait for whatever repaint happened to come next.
                ctx.request_repaint();
            },
            BindingAction::Named(other) => {
                if let Some(key) = project_filter_identity(other) {
                    self.project_filter.toggle(key);
                } else if let Some(key) = git_filter_identity(other) {
                    self.git_filter.toggle(key);
                    self.after_git_filter_changed();
                } else {
                    self.dispatch_scroll_or_other(other);
                }
            },
            BindingAction::Unsupported(name) => {
                log::debug!("unsupported keyboard binding action: {name}");
            },
        }
    }

    /// The target is resolved before the clipboard so a paste with nowhere to
    /// go opens nothing.  Only the regular clipboard carries files and images:
    /// PRIMARY is a text selection, so its probes are skipped outright.
    fn paste_from_clipboard(&mut self, ctx: &Context, target: Target) {
        let Some(idx) = self.active_session_index() else {
            return;
        };
        let extras = target == Target::Clipboard;
        let payload = clipboard::resolve(
            &self.config.ui.paste,
            || clipboard::read_text(target),
            || if extras { clipboard::read_files() } else { clipboard::Probe::Absent },
            || if extras { clipboard::read_image() } else { clipboard::Probe::Absent },
        );

        let paths = match payload {
            clipboard::Payload::Text(text) => {
                self.insert_paste(ctx, idx, &text);
                return;
            },
            clipboard::Payload::Paths(paths) => paths,
            clipboard::Payload::Image(image) => match self.store_clipboard_image(&image) {
                Some(path) => vec![path],
                None => return,
            },
            clipboard::Payload::Nothing => return,
        };

        let session = &self.sessions[idx];
        let scratchpad = session.scratchpad.is_some();
        let text = file_drop::paste_payload(
            &paths,
            scratchpad,
            session.wsl_distro(),
            &self.config.ui.drop.spelling,
        );
        // Every path was filtered out.  A paste of nothing still clears the
        // selection and snaps the view to the bottom, or drops the scratchpad's
        // selection — side effects with nothing to show for them.
        if !text.is_empty() {
            self.insert_paste(ctx, idx, &text);
        }
    }

    fn insert_paste(&mut self, ctx: &Context, idx: usize, text: &str) {
        let id = self.sessions[idx].id;
        if let Some(editor) = self.sessions[idx].scratchpad.as_mut() {
            editor.insert_at_cursor(ctx, id, text);
        } else {
            paste::paste(&mut self.sessions[idx], text, true);
        }
    }

    /// The clipboard bitmap as a file something else can open, or `None` with
    /// the reason logged.
    ///
    /// The returned path is pasted into the terminal immediately, so `store`
    /// runs inline; only the cap sweep that follows a managed directory is
    /// backgrounded, since nothing reads its result.
    fn store_clipboard_image(&mut self, image: &arboard::ImageData<'_>) -> Option<PathBuf> {
        let png = match clipboard_image::encode_png(image) {
            Ok(png) => png,
            Err(e) => {
                log::warn!("cannot encode the clipboard image: {e}");
                return None;
            },
        };
        let cfg = &self.config.ui.paste;
        let (dir, owned) = cfg.image_target();
        let keep = cfg.image_keep;
        match clipboard_image::store(&dir, &png, owned) {
            Ok(path) => {
                if owned {
                    let in_use = path.clone();
                    self.detached_jobs.push(
                        jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
                            clipboard_image::sweep(&dir, keep, &in_use, blocking)
                        }),
                    );
                }
                Some(path)
            },
            Err(e) => {
                log::warn!("cannot write the clipboard image to {}: {e}", dir.display());
                None
            },
        }
    }

    /// Confirm the focused sidebar's fuzzy search: leave search and land the
    /// cursor on the highlighted row, scrolled into view, keeping focus in the
    /// sidebar. Selecting a row never activates it — a following browsing
    /// `Enter` does that. No-op unless the focused panel is in search mode.
    fn sidebar_search_confirm(&mut self) {
        match self.focus {
            PaneFocus::ProjectsSidebar
                if self.project_filter.mode() == panel_filter::Mode::Search =>
            {
                let acted = self.sidebar_cursor.clone();
                if let Some(row) = acted.as_ref() {
                    self.reveal_search_row(row);
                }
                self.finish_project_search_at(acted);
            },
            PaneFocus::GitSidebar if self.git_filter.mode() == panel_filter::Mode::Search => {
                let cursor = self.git_cursor.clone();
                self.finish_git_search_at(cursor);
            },
            _ => {},
        }
    }

    /// Expand the project owning a worktree/session row so the row outlives the
    /// search exit: search lists matched children whatever their project's
    /// `expanded` flag says, so a child under a collapsed project would vanish
    /// the moment the query clears. Expand-only, and headers are left alone, so
    /// confirming a project row never toggles it.
    fn reveal_search_row(&mut self, row: &SidebarRow) {
        let root = {
            let session_workspace = |id: SessionId| {
                self.sessions.iter().find(|s| s.id == id).map(|s| s.working_directory.clone())
            };
            search_reveal_root(&self.projects, session_workspace, row)
        };
        if let Some(root) = root {
            self.set_project_expanded(&root, true);
        }
    }

    /// Leave search and land the projects cursor on `requested`, falling back
    /// through `ensure_cursor` when it no longer renders. The scroll is forced
    /// rather than keyed on the cursor changing: restoring the unfiltered list
    /// can move the very same row far off-screen.
    fn finish_project_search_at(&mut self, requested: Option<SidebarRow>) {
        self.project_filter.exit_search();
        let rows = self.current_project_rows();
        self.sidebar_cursor = sidebar_nav::ensure_cursor(&rows, requested.as_ref());
        self.sidebar_cursor_moved = true;
    }

    /// Git-panel counterpart of `finish_project_search_at`.
    fn finish_git_search_at(&mut self, requested: Option<git_nav::GitRow>) {
        self.git_filter.exit_search();
        self.recompute_git_rows();
        self.git_cursor = git_nav::ensure_cursor(&self.git_rows, requested.as_ref());
        self.git_cursor_moved = true;
    }

    /// Cancel the focused sidebar's fuzzy search, staying in the sidebar with the
    /// cursor on the seed row (active session / workspace, else Home). No-op
    /// unless the focused panel is in search mode.
    fn sidebar_search_cancel(&mut self) {
        match self.focus {
            PaneFocus::ProjectsSidebar
                if self.project_filter.mode() == panel_filter::Mode::Search =>
            {
                let seed = sidebar_nav::seed(
                    &self.projects,
                    self.current_workspace.as_deref(),
                    &self.listed_workspace_rows(),
                    self.active_session.get(&self.current_workspace).copied(),
                );
                self.finish_project_search_at(Some(seed));
            },
            PaneFocus::GitSidebar if self.git_filter.mode() == panel_filter::Mode::Search => {
                let cursor = self.git_cursor.clone();
                self.finish_git_search_at(cursor);
            },
            _ => {},
        }
    }

    /// Cancel the focused sidebar's fuzzy search and return focus to the
    /// terminal. No-op unless the focused panel is in search mode.
    fn sidebar_search_cancel_to_terminal(&mut self) {
        match self.focus {
            PaneFocus::ProjectsSidebar
                if self.project_filter.mode() == panel_filter::Mode::Search =>
            {
                self.project_filter.exit_search();
                self.focus_terminal();
            },
            PaneFocus::GitSidebar if self.git_filter.mode() == panel_filter::Mode::Search => {
                self.git_filter.exit_search();
                self.recompute_git_rows();
                self.focus_terminal();
            },
            _ => {},
        }
    }

    fn dispatch_scroll_or_other(&mut self, action: NamedAction) {
        use alacritty_terminal::grid::{Dimensions, Scroll};
        let Some(idx) = self.active_session_index() else {
            return;
        };
        let session = &mut self.sessions[idx];
        if session.scratchpad.is_some() {
            return;
        }
        let mut term = session.term.lock();
        let lines_per_page = term.grid().screen_lines() as i32;
        let scroll = match action {
            NamedAction::ScrollLineUp => Some(Scroll::Delta(1)),
            NamedAction::ScrollLineDown => Some(Scroll::Delta(-1)),
            NamedAction::ScrollHalfPageUp => Some(Scroll::Delta(lines_per_page / 2)),
            NamedAction::ScrollHalfPageDown => Some(Scroll::Delta(-(lines_per_page / 2))),
            NamedAction::ScrollPageUp => Some(Scroll::PageUp),
            NamedAction::ScrollPageDown => Some(Scroll::PageDown),
            NamedAction::ScrollToTop => Some(Scroll::Top),
            NamedAction::ScrollToBottom => Some(Scroll::Bottom),
            _ => None,
        };
        if let Some(s) = scroll {
            term.scroll_display(s);
        }
    }

    fn select_tab(&mut self, n: u8) {
        if n == 0 {
            return;
        }
        let indices = self.current_session_indices();
        let Some(&session_idx) = indices.get((n - 1) as usize) else {
            return;
        };
        let id = self.sessions[session_idx].id;
        self.set_active_in_current_workspace(id);
    }

    fn select_last_tab(&mut self) {
        let indices = self.current_session_indices();
        let Some(&session_idx) = indices.last() else {
            return;
        };
        let id = self.sessions[session_idx].id;
        self.set_active_in_current_workspace(id);
    }

    fn show_tab_strip(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme;
        let indices = self.current_session_indices();
        // The strip exists to switch between sessions, so it only earns its
        // space once there's a choice to make (or the user forces it on).  With
        // a single session this hides the trailing "+" new-session tab too,
        // rather than leaving a lone hint above the terminal.
        if indices.len() < 2 && !self.session_tabs_always {
            ui.add_space(2.0);
            return;
        }
        let active_idx = self.active_session_index();

        // Reserve a 2px-tall strip across the full width of the terminal pane.
        let strip_height = 2.0;
        let gap = 4.0;
        let plus_width = 12.0;
        let avail = ui.available_width();
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(avail, strip_height + 2.0), egui::Sense::hover());

        let mut activate: Option<SessionId> = None;
        let seg_avail = avail - plus_width - gap;
        let segment_width =
            ((seg_avail - gap * (indices.len() as f32 - 1.0)) / indices.len() as f32).max(1.0);
        for (i, &session_idx) in indices.iter().enumerate() {
            let x0 = rect.min.x + i as f32 * (segment_width + gap);
            let seg_rect = egui::Rect::from_min_size(
                egui::pos2(x0, rect.min.y + 1.0),
                egui::vec2(segment_width, strip_height),
            );
            let is_active = active_idx == Some(session_idx);
            // 2px is too small to reliably click — expand the hit zone vertically.
            let click_rect = seg_rect.expand2(egui::vec2(0.0, 4.0));
            let id = ui.id().with(("tab_strip", self.sessions[session_idx].id));
            let resp = ui.interact(click_rect, id, egui::Sense::click());
            // Attention wins over the active/inactive shading so a bell from a
            // non-active tab pulls the eye even when another tab is selected.
            let color = if self.sessions[session_idx].needs_attention {
                theme.attention
            } else if is_active {
                theme.text
            } else if resp.hovered() {
                theme.text_dim
            } else {
                theme.text_muted
            };
            ui.painter().rect_filled(seg_rect, 0.0, color);
            if resp.clicked() {
                activate = Some(self.sessions[session_idx].id);
            }
            if resp.hovered() {
                resp.on_hover_text(&self.sessions[session_idx].title);
            }
        }

        let profile_names: Vec<String> =
            self.config.profiles.iter().map(|p| p.name.clone()).collect();
        let mut spawn_default = false;
        let mut spawn_profile: Option<String> = None;

        let plus_rect = egui::Rect::from_min_size(
            egui::pos2(rect.max.x - plus_width, rect.min.y + 1.0),
            egui::vec2(plus_width, strip_height),
        );
        let click_rect = plus_rect.expand2(egui::vec2(0.0, 4.0));
        let resp = ui.interact(click_rect, ui.id().with("tab_strip_plus"), egui::Sense::click());
        let color = if resp.hovered() { theme.text_dim } else { theme.text_muted };
        ui.painter().rect_filled(plus_rect, 0.0, color);
        if resp.clicked() {
            spawn_default = true;
        }
        if !profile_names.is_empty() {
            resp.context_menu(|ui| {
                ui.label(RichText::new("New session with…").color(theme.text_muted).small());
                for name in &profile_names {
                    if ui.button(name).clicked() {
                        spawn_profile = Some(name.clone());
                        ui.close_menu();
                    }
                }
            });
        }
        let hover_text = if profile_names.is_empty() {
            "New session"
        } else {
            "New session (right-click: profiles)"
        };
        resp.on_hover_text(hover_text);

        if let Some(id) = activate {
            self.set_active_in_current_workspace(id);
        }
        if spawn_default {
            let ctx = ui.ctx().clone();
            let ws = self.current_workspace.clone();
            if let Err(e) = self.spawn_session(&ctx, ws.clone()) {
                self.report_spawn_failure(&ctx, &ws, &e);
            }
        }
        if let Some(name) = spawn_profile {
            let ctx = ui.ctx().clone();
            self.spawn_profile_session(&ctx, &name);
        }
    }

    fn show_project_sidebar(&mut self, ctx: &Context, panel_frame: Frame) -> egui::Rect {
        // Only rows that actually paint are worth a liveness probe, and which
        // ones those are is not known until the tree, its filters and its
        // collapsed projects have all had their say.  Deciding *before* the
        // walk that this frame is not a probe frame is what keeps the other
        // ~89 frames of every 90 from collecting anything at all.
        let probing = self.config.ui.worktree_liveness
            && self.liveness_probe.is_none()
            && self.liveness.wants_probe(Instant::now());
        let drawn_worktrees: std::cell::RefCell<Vec<PathBuf>> = Default::default();
        let activate_request: std::cell::Cell<Option<PathBuf>> = std::cell::Cell::new(None);
        let delete_request: std::cell::Cell<Option<PathBuf>> = std::cell::Cell::new(None);
        let create_request: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
        let spawn_shell_request: std::cell::Cell<Option<WorkspaceKey>> = std::cell::Cell::new(None);
        let spawn_profile_request: std::cell::Cell<Option<(PathBuf, String)>> =
            std::cell::Cell::new(None);
        let activate_session_request: std::cell::Cell<Option<(WorkspaceKey, SessionId)>> =
            std::cell::Cell::new(None);
        let close_session_request: std::cell::Cell<Option<SessionId>> = std::cell::Cell::new(None);
        let attach_herdr_request: std::cell::Cell<Option<(WorkspaceKey, herdr::HerdrKey, String)>> =
            std::cell::Cell::new(None);
        let base_picker_request: std::cell::Cell<Option<PathBuf>> = std::cell::Cell::new(None);
        // Drag-to-reorder: (dragged root, insert-before display index).
        let reorder_request: std::cell::Cell<Option<(PathBuf, usize)>> = std::cell::Cell::new(None);
        let mut add_project_clicked = false;
        let mut reorder_toggled = false;
        let mut refresh_idx: Option<usize> = None;
        let mut remove_request: Option<ProjectRemoveState> = None;
        let mut expand_toggled: Option<(PathBuf, bool)> = None;
        let mut home_clicked = false;
        let theme = self.theme;
        let scrollbar = self.config.ui.scrollbar;
        let reorder_mode = self.reorder_mode;
        let session_drag = self.session_drag;
        // The render pass cannot borrow `self.sessions`, so the dragged
        // session's own scope is resolved here: a row outside this range draws
        // no indicator and never becomes a drop.
        let drag_range: Option<(SessionId, Vec<WorkspaceKey>)> =
            egui::DragAndDrop::payload::<DraggedSession>(ctx).and_then(|dragged| {
                let (_, range) = self.reorder_range(dragged.0)?;
                Some((dragged.0, range))
            });
        let session_drop_request: std::cell::Cell<Option<(SessionId, WorkspaceKey, usize)>> =
            std::cell::Cell::new(None);
        let cursor_row = if self.focus == PaneFocus::ProjectsSidebar {
            self.sidebar_cursor.clone()
        } else {
            None
        };
        let cursor_moved = std::mem::take(&mut self.sidebar_cursor_moved);
        let scrolls = |is_cursor: bool| is_cursor && cursor_moved;

        let filtering = self.project_filter.is_filtering();
        let active_now = self.active_session.get(&self.current_workspace).copied();
        // egui keeps one scroll target per frame and the last writer wins, so the
        // two reasons to scroll are resolved here rather than by paint order.  An
        // explicit cursor move outranks following the terminal.
        let wants_follow = sidebar_nav::wants_follow(
            self.config.ui.sidebar_follow_active,
            cursor_moved,
            &self.last_followed,
            &self.current_workspace,
            active_now,
        );
        let rows: Vec<SidebarRow> = if filtering || wants_follow {
            match &self.sidebar_rows_cache {
                Some(rows) => rows.clone(),
                None => self.current_project_rows(),
            }
        } else {
            Vec::new()
        };
        let follow_row = wants_follow
            .then(|| {
                let project_root = sidebar_nav::project_of(&self.projects, &self.current_workspace)
                    .map(Path::to_path_buf);
                sidebar_nav::follow_scroll_row(
                    &rows,
                    &self.current_workspace,
                    active_now,
                    project_root.as_deref(),
                )
            })
            .flatten();
        if follow_row.is_some() {
            self.last_followed = (self.current_workspace.clone(), active_now);
        }
        // `Project`/`Worktree` rows carry a `PathBuf`; matching by reference
        // here (mirroring the `cursor_row` matches below) keeps every scroll
        // check on the paint path allocation-free, follow target or not.
        let follows_home = follow_row == Some(SidebarRow::Home);
        let follows_session = |id: SessionId| follow_row == Some(SidebarRow::Session(id));
        let follows_project = |root: &Path| matches!(&follow_row, Some(SidebarRow::Project(r)) if r.as_path() == root);
        let follows_worktree = |path: &Path| matches!(&follow_row, Some(SidebarRow::Worktree(p)) if p.as_path() == path);

        // Membership for the active filter, resolved once so paint can skip
        // non-surviving rows.  While filtering, matched projects render their
        // matched worktrees regardless of `expanded` (display-only — the flag
        // is never written).
        let mut home_visible = true;
        let mut visible_projects: HashSet<PathBuf> = HashSet::new();
        let mut visible_worktrees: HashSet<PathBuf> = HashSet::new();
        if filtering {
            home_visible = false;
            for row in rows {
                match row {
                    SidebarRow::Home => home_visible = true,
                    SidebarRow::Project(root) => {
                        visible_projects.insert(root);
                    },
                    SidebarRow::Worktree(path) => {
                        visible_worktrees.insert(path);
                    },
                    // Session rows follow their workspace row's visibility.
                    // `filtered_rows` never emits `HerdrAgent`, so this arm
                    // never actually sees one while filtering.
                    SidebarRow::Session(_) | SidebarRow::HerdrAgent(..) => {},
                }
            }
        }
        let filtered_empty = filtering
            && !home_visible
            && visible_projects.is_empty()
            && visible_worktrees.is_empty();

        // Snapshot attention + agent-glyph state up-front so the `iter_mut`
        // over projects below isn't blocked from calling back into `&self`
        // helpers.
        // `sidebar_nav::filtered_rows` never emits `SidebarRow::HerdrAgent`
        // (it would need the agent's display name, which the row's `(Side,
        // String)` payload doesn't carry), so a herdr row painted while
        // filtering would have no cursor path to reach it.  Dropping them
        // from the listing rather than rebuilding it keeps the sessions the
        // filter does render in the positions the nav model gave them.
        let mut listed = self.listed_workspace_rows();
        if filtering {
            for entries in listed.values_mut() {
                entries.retain(|entry| entry.session().is_some());
            }
        }
        let home_rows = self.workspace_rows(&None, &listed);
        let worktree_rows: Vec<Vec<Vec<WorkspaceRowData>>> = self
            .projects
            .iter()
            .map(|p| {
                p.worktrees
                    .iter()
                    .map(|wt| self.workspace_rows(&Some(wt.path.clone()), &listed))
                    .collect()
            })
            .collect();

        let worktree_listed: Vec<Vec<bool>> = worktree_rows
            .iter()
            .map(|v| v.iter().map(|rows| WorkspaceRowData::any_session(rows)).collect())
            .collect();

        // A rendered session list carries its own per-session status; repeating
        // it on the parent row reads as noise — the same
        // rule the project row applies when expanded.  Aggregates therefore
        // apply only while the list is hidden (fewer than two sessions).
        let home_lists_sessions = WorkspaceRowData::any_session(&home_rows);
        let home_attention = !home_lists_sessions && self.workspace_needs_attention(&None);
        let home_activity = if home_lists_sessions {
            SessionActivity::Shell
        } else {
            self.workspace_activity(&None)
        };
        let project_attention: Vec<bool> =
            self.projects.iter().map(|p| self.project_needs_attention(p)).collect();
        let worktree_attention: Vec<Vec<bool>> = self
            .projects
            .iter()
            .enumerate()
            .map(|(p_idx, p)| {
                p.worktrees
                    .iter()
                    .enumerate()
                    .map(|(w_idx, wt)| {
                        let listed = worktree_listed
                            .get(p_idx)
                            .and_then(|v| v.get(w_idx))
                            .copied()
                            .unwrap_or(false);
                        !listed && self.workspace_needs_attention(&Some(wt.path.clone()))
                    })
                    .collect()
            })
            .collect();
        let worktree_activity: Vec<Vec<SessionActivity>> = self
            .projects
            .iter()
            .enumerate()
            .map(|(p_idx, p)| {
                p.worktrees
                    .iter()
                    .enumerate()
                    .map(|(w_idx, wt)| {
                        let listed = worktree_listed
                            .get(p_idx)
                            .and_then(|v| v.get(w_idx))
                            .copied()
                            .unwrap_or(false);
                        if listed {
                            SessionActivity::Shell
                        } else {
                            self.workspace_activity(&Some(wt.path.clone()))
                        }
                    })
                    .collect()
            })
            .collect();
        // Worktrees whose background removal is still running: their rows show
        // a spinner instead of the delete/new-shell controls.
        let deleting_paths: HashSet<PathBuf> =
            self.pending_deletes.iter().map(|t| t.worktree_path.clone()).collect();
        // Minimized creations, keyed by project index, rendered as spinner
        // placeholder rows until the finished worktree shows up on refresh.
        let creating: Vec<(usize, String)> =
            self.pending_creates.iter().map(|c| (c.project_idx, c.branch.clone())).collect();
        let distros = wsl::distros();
        let icons = self.config.ui.icons.clone();
        let profile_names: Vec<String> =
            self.config.profiles.iter().map(|p| p.name.clone()).collect();
        // Name + command pairs for the worktree row's "Open session" menu —
        // the command is only ever shown as hover text, never painted.
        let worktree_profiles: Vec<(String, String)> =
            self.config.profiles.iter().map(|p| (p.name.clone(), profile_command(p))).collect();
        let mut shell_override_changed: Option<PathBuf> = None;
        let mut label_cleared: Option<PathBuf> = None;
        let mut rename_request: Option<RenameState> = None;
        // Polled up front: the panel closure borrows `projects` mutably, so the
        // cache cannot be polled from inside it.
        let pr_enabled = self.config.ui.pr_status;
        let any_pr_toggle = any_pr_toggle_active(&self.project_filter, self.search_scope);
        let current_workspace = self.current_workspace.as_deref();
        let live_branch = current_workspace
            .and_then(|p| self.git_status.get(p))
            .and_then(|cache| cache.current_branch());
        // The same path can be a worktree of two projects, and `PrCache` is
        // keyed by path alone, so a second poller would only invalidate the
        // first's lookup and burn a `gh` process every frame.
        let mut polled: HashMap<PathBuf, Option<PrInfo>> = HashMap::new();
        let mut pr_infos: Vec<Vec<Option<PrInfo>>> = Vec::with_capacity(self.projects.len());
        for project in &self.projects {
            let mut rows = Vec::with_capacity(project.worktrees.len());
            for wt in &project.worktrees {
                let info = resolve_pr_info(
                    &mut polled,
                    &wt.path,
                    should_poll_pr(pr_enabled, project.expanded, any_pr_toggle),
                    || {
                        let branch =
                            pr_status::effective_branch(wt, current_workspace, live_branch);
                        self.pr_cache.poll(&wt.path, branch, ctx)
                    },
                );
                rows.push(info);
            }
            pr_infos.push(rows);
        }
        // Rendered up front: the panel closure borrows `projects` mutably, and
        // substitution over short strings is microseconds, so no cache is kept.
        // After `pr_infos` so `$pr` sees this frame's PR numbers.
        let mut project_labels: Vec<String> = Vec::with_capacity(self.projects.len());
        let mut worktree_labels: Vec<Vec<String>> = Vec::with_capacity(self.projects.len());
        for (project, prs) in self.projects.iter().zip(&pr_infos) {
            project_labels.push(self.row_labels.project_label(project));
            let mut rows = Vec::with_capacity(project.worktrees.len());
            for (wt, pr) in project.worktrees.iter().zip(prs) {
                rows.push(self.row_labels.worktree_label(wt, pr.as_ref()));
            }
            worktree_labels.push(rows);
        }

        let panel_resp = SidePanel::left("left_sidebar")
            .resizable(true)
            .default_width(240.0 * theme.ui_scale)
            .min_width(180.0 * theme.ui_scale)
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Sidebar rows are click targets, not selectable prose; the
                // default I-beam-and-select on labels is the wrong affordance.
                ui.style_mut().interaction.selectable_labels = false;
                apply_scrollbar_style(ui, scrollbar);
                ui.horizontal(|ui| {
                    panel_header_filter_ui(
                        ui,
                        "Projects",
                        &self.project_filter,
                        &self.config.ui.icons.search,
                        &theme,
                        self.project_filter.toggles_apply(self.search_scope),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_tooltip(
                            styled_icon_button(
                                ui,
                                &icons.add_project,
                                DEFAULT_ADD_ICON,
                                theme.text_dim,
                                &theme,
                            ),
                            "add project",
                            theme.icon_tooltips,
                        )
                        .clicked()
                        {
                            add_project_clicked = true;
                        }
                        // Lit while active: the mode is only visible as grips
                        // on the rows, so the button has to say it's on.
                        let (color, hint) = if reorder_mode {
                            (theme.accent, "done reordering")
                        } else {
                            (theme.text_dim, "reorder projects")
                        };
                        if icon_tooltip(
                            styled_icon_button(ui, &icons.reorder, DEFAULT_REORDER_ICON, color, &theme),
                            hint,
                            theme.icon_tooltips,
                        )
                        .clicked()
                        {
                            reorder_toggled = true;
                        }
                    });
                });
                ui.separator();

                // `slot` carries a session row's display index and id; `None`
                // is a workspace row.
                let session_drop = |ui: &egui::Ui,
                                    row_rect: egui::Rect,
                                    ws: &WorkspaceKey,
                                    slot: Option<(usize, SessionId)>| {
                    let Some((dragged, range)) = drag_range.as_ref() else { return };
                    // The dragged row's own edges are no-ops, so offering them
                    // as targets would paint a drop that does nothing.
                    if slot.is_some_and(|(_, id)| id == *dragged) {
                        return;
                    }
                    if !range.contains(ws) {
                        return;
                    }
                    let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else { return };
                    if !row_rect.contains(pointer) {
                        return;
                    }
                    let position = match slot {
                        // A session row: the half the pointer is in decides.
                        Some((idx, _)) => {
                            if draw_drop_indicator(ui, row_rect, pointer, &theme) {
                                idx
                            } else {
                                idx + 1
                            }
                        },
                        // A workspace row: its sessions start under it, so a
                        // drop here means the front of that workspace.  This is
                        // the only way to reach a workspace listing no session
                        // rows — an empty one, or a single-session one below the
                        // display threshold.
                        None => {
                            ui.painter().hline(
                                row_rect.x_range(),
                                row_rect.bottom(),
                                drop_indicator_stroke(&theme),
                            );
                            0
                        },
                    };
                    if ui.input(|i| i.pointer.any_released()) {
                        session_drop_request.set(Some((*dragged, ws.clone(), position)));
                        egui::DragAndDrop::clear_payload(ui.ctx());
                    }
                };

                ScrollArea::vertical().show(ui, |ui| {
                    // Inter-group spacing is emitted above the group that
                    // follows, never after the last one: trailing padding
                    // makes the content measure taller than the rows on
                    // screen, which shows a scrollbar with nothing to scroll
                    // whenever the list otherwise fits the panel.
                    let mut group_gap = 0.0_f32;
                    if !filtering || home_visible {
                        let home_is_cursor = matches!(&cursor_row, Some(SidebarRow::Home));
                        let home_action = home_row(
                            ui,
                            self.current_workspace.is_none(),
                            home_is_cursor,
                            scrolls(home_is_cursor) || follows_home,
                            home_attention,
                            home_activity,
                            &icons,
                            &theme,
                        );
                        if home_action.activate {
                            home_clicked = true;
                        }
                        if home_action.spawn {
                            spawn_shell_request.set(Some(None));
                        }
                        session_drop(ui, home_action.rect, &None, None);
                        // Only rows a reorder can move take a drop slot, and
                        // the slot index counts those alone: a herdr pane's
                        // place is herdr's to decide, so it is neither a drag
                        // subject nor a landing.
                        let mut slot = 0usize;
                        for row in &home_rows {
                            match row {
                                WorkspaceRowData::Session(row) => {
                                    let is_cursor = matches!(
                                        &cursor_row,
                                        Some(SidebarRow::Session(id)) if *id == row.id
                                    );
                                    let scroll = scrolls(is_cursor) || follows_session(row.id);
                                    let movable = row.managed.is_none();
                                    let act = session_row(
                                        ui,
                                        row,
                                        is_cursor,
                                        scroll,
                                        session_drag && movable,
                                        &icons,
                                        &theme,
                                    );
                                    if act.activate {
                                        activate_session_request.set(Some((None, row.id)));
                                    }
                                    if act.close {
                                        close_session_request.set(Some(row.id));
                                    }
                                    if movable {
                                        session_drop(ui, act.rect, &None, Some((slot, row.id)));
                                        slot += 1;
                                    }
                                },
                                WorkspaceRowData::Herdr(row) => {
                                    let is_cursor = matches!(
                                        &cursor_row,
                                        Some(SidebarRow::HerdrAgent(side, id))
                                            if *side == row.side && *id == row.terminal_id
                                    );
                                    let scroll = scrolls(is_cursor);
                                    let act =
                                        herdr_row(ui, row, is_cursor, scroll, &icons, &theme);
                                    if act.attach {
                                        attach_herdr_request.set(Some((
                                            None,
                                            herdr::HerdrKey {
                                                side: row.side.clone(),
                                                terminal_id: row.terminal_id.clone(),
                                            },
                                            row.pane_id.clone(),
                                        )));
                                    }
                                },
                            }
                        }
                        group_gap = 2.0;
                    }

                    if self.projects.is_empty() {
                        ui.add_space(std::mem::take(&mut group_gap));
                        ui.label(
                            RichText::new("Click + to add a project.")
                                .color(theme.text_dim)
                                .small(),
                        );
                        ui.add_space(4.0);
                        ui.label(RichText::new("Ctrl+B to toggle").small().color(theme.text_muted));
                    } else if filtered_empty {
                        ui.add_space(std::mem::take(&mut group_gap));
                        ui.label(RichText::new("no matches").color(theme.text_dim).small());
                    }

                    for (idx, project) in self.projects.iter_mut().enumerate() {
                        if filtering && !visible_projects.contains(&project.root) {
                            continue;
                        }
                        ui.add_space(std::mem::take(&mut group_gap));
                        let proj_attention = project_attention.get(idx).copied().unwrap_or(false);
                        // Bubble attention up to the project row only when the
                        // project is collapsed — once expanded, the actual
                        // worktree rows already show the dot, and doubling it
                        // on the parent reads as noise.
                        let show_proj_dot = proj_attention && !project.expanded;
                        // Cloned out before the row closures borrow `project`
                        // mutably: the trailing closure needs them for the
                        // remove-confirmation prompt.
                        let project_root = project.root.clone();
                        let project_name = project.display_name().to_string();
                        let mut name_resp: Option<egui::Response> = None;
                        let row_rect = row_with_trailing(
                            ui,
                            |ui| {
                                ui.spacing_mut().item_spacing.x = ICON_CLUSTER_SPACING;
                                if reorder_mode {
                                    drag_handle(ui, &theme)
                                        .dnd_set_drag_payload(DraggedProject(project.root.clone()));
                                }
                                let (arrow_style, arrow_default, arrow_hint) = if project.expanded {
                                    (
                                        &icons.project_expanded,
                                        DEFAULT_PROJECT_EXPANDED_ICON,
                                        "collapse project",
                                    )
                                } else {
                                    (
                                        &icons.project_collapsed,
                                        DEFAULT_PROJECT_COLLAPSED_ICON,
                                        "expand project",
                                    )
                                };
                                if icon_tooltip(
                                    styled_icon_button(
                                        ui,
                                        arrow_style,
                                        arrow_default,
                                        theme.text_dim,
                                        &theme,
                                    ),
                                    arrow_hint,
                                    theme.icon_tooltips,
                                )
                                .clicked()
                                {
                                    project.expanded = !project.expanded;
                                    expand_toggled = Some((project.root.clone(), project.expanded));
                                }
                                let name = project_labels
                                    .get(idx)
                                    .map(String::as_str)
                                    .unwrap_or(project.display_name());
                                let (resp, galley) = truncating_label(
                                    ui,
                                    RichText::new(name).strong().small().color(theme.text),
                                    theme.text,
                                    egui::Sense::click(),
                                );
                                name_resp = Some(name_tooltip(
                                    resp,
                                    name,
                                    galley.elided,
                                    theme.sidebar_tooltips,
                                ));
                            },
                            |ui| {
                                if icon_tooltip(
                                    styled_icon_button(
                                        ui,
                                        &icons.remove_project,
                                        DEFAULT_CLOSE_ICON,
                                        theme.text_muted,
                                        &theme,
                                    ),
                                    "remove from sidebar",
                                    theme.icon_tooltips,
                                )
                                .clicked()
                                {
                                    remove_request = Some(ProjectRemoveState {
                                        root: project_root.clone(),
                                        name: project_name.clone(),
                                    });
                                }
                                if icon_tooltip(
                                    styled_icon_button(
                                        ui,
                                        &icons.refresh,
                                        DEFAULT_REFRESH_ICON,
                                        theme.text_muted,
                                        &theme,
                                    ),
                                    "refresh worktrees",
                                    theme.icon_tooltips,
                                )
                                .clicked()
                                {
                                    refresh_idx = Some(idx);
                                }
                                if icon_tooltip(
                                    styled_icon_button(
                                        ui,
                                        &icons.new_worktree,
                                        DEFAULT_ADD_ICON,
                                        theme.text_muted,
                                        &theme,
                                    ),
                                    "create new worktree",
                                    theme.icon_tooltips,
                                )
                                .clicked()
                                {
                                    create_request.set(Some(idx));
                                }
                                if show_proj_dot {
                                    icon_tooltip(
                                        attention_dot(ui, &theme),
                                        ATTENTION_HINT,
                                        theme.icon_tooltips,
                                    );
                                }
                            },
                        );
                        let header_is_cursor =
                            matches!(&cursor_row, Some(SidebarRow::Project(r)) if *r == project.root);
                        let header_rect = egui::Rect::from_x_y_ranges(
                            ui.max_rect().x_range(),
                            row_rect.y_range(),
                        );
                        if header_is_cursor {
                            paint_cursor_outline(ui, header_rect, &theme);
                        }
                        if scrolls(header_is_cursor) || follows_project(&project.root) {
                            ui.scroll_to_rect(header_rect, theme.scroll_align);
                        }

                        // Drop target for a reorder drag.  Detected against the
                        // raw payload rather than a `dnd_drop_zone` widget so no
                        // extra hover-sensing rect steals the row buttons' own
                        // hover highlight.
                        if let Some(dragged) =
                            egui::DragAndDrop::payload::<DraggedProject>(ui.ctx())
                        {
                            let pointer = ui.input(|i| i.pointer.interact_pos());
                            if let Some(pointer) = pointer
                                .filter(|p| row_rect.contains(*p) && dragged.0 != project.root)
                            {
                                let before = draw_drop_indicator(ui, row_rect, pointer, &theme);
                                if ui.input(|i| i.pointer.any_released()) {
                                    let insert_before = if before { idx } else { idx + 1 };
                                    reorder_request.set(Some((dragged.0.clone(), insert_before)));
                                    egui::DragAndDrop::clear_payload(ui.ctx());
                                }
                            }
                        }

                        // Right-click: rename the project, and choose which
                        // shell its sessions use.
                        if let Some(resp) = name_resp {
                            resp.context_menu(|ui| {
                                if ui.button("Rename…").clicked() {
                                    rename_request = Some(RenameState {
                                        root: project.root.clone(),
                                        label: project.display_name().to_string(),
                                    });
                                    ui.close_menu();
                                }
                                if project.label.is_some() && ui.button("Reset name").clicked() {
                                    project.label = None;
                                    label_cleared = Some(project.root.clone());
                                    ui.close_menu();
                                }
                                // The shell picker is hidden when there is
                                // nothing to choose (no distros, no profiles)
                                // so minimal setups see only the rename.
                                if !distros.is_empty() || !profile_names.is_empty() {
                                    ui.separator();
                                    ui.label(
                                        RichText::new("Open in…").color(theme.text_muted).small(),
                                    );
                                    let mark =
                                        |selected: bool| if selected { "• " } else { "   " };
                                    let auto = project.shell_override.is_none();
                                    if ui
                                        .button(format!("{}Auto (by location)", mark(auto)))
                                        .clicked()
                                    {
                                        project.shell_override = None;
                                        shell_override_changed = Some(project.root.clone());
                                        ui.close_menu();
                                    }
                                    let win = matches!(
                                        project.shell_override,
                                        Some(ShellChoice::Windows)
                                    );
                                    if ui.button(format!("{}Windows shell", mark(win))).clicked() {
                                        project.shell_override = Some(ShellChoice::Windows);
                                        shell_override_changed = Some(project.root.clone());
                                        ui.close_menu();
                                    }
                                    for distro in &distros {
                                        let selected = matches!(
                                            &project.shell_override,
                                            Some(ShellChoice::Wsl(name)) if name == &distro.name
                                        );
                                        if ui
                                            .button(format!(
                                                "{}WSL ({})",
                                                mark(selected),
                                                distro.name
                                            ))
                                            .clicked()
                                        {
                                            project.shell_override =
                                                Some(ShellChoice::Wsl(distro.name.clone()));
                                            shell_override_changed = Some(project.root.clone());
                                            ui.close_menu();
                                        }
                                    }
                                    for name in &profile_names {
                                        let selected = matches!(
                                            &project.shell_override,
                                            Some(ShellChoice::Profile(n)) if n == name
                                        );
                                        if ui
                                            .button(format!("{}Profile: {}", mark(selected), name))
                                            .clicked()
                                        {
                                            project.shell_override =
                                                Some(ShellChoice::Profile(name.clone()));
                                            shell_override_changed = Some(project.root.clone());
                                            ui.close_menu();
                                        }
                                    }
                                }
                            });
                        }

                        if project.expanded || filtering {
                            for (wt_idx, wt) in project.worktrees.iter().enumerate() {
                                if filtering && !visible_worktrees.contains(&wt.path) {
                                    continue;
                                }
                                let is_active = self.current_workspace.as_deref() == Some(&wt.path);
                                let wt_attention = worktree_attention
                                    .get(idx)
                                    .and_then(|v| v.get(wt_idx))
                                    .copied()
                                    .unwrap_or(false);
                                let wt_activity = worktree_activity
                                    .get(idx)
                                    .and_then(|v| v.get(wt_idx))
                                    .copied()
                                    .unwrap_or_default();
                                let is_cursor = matches!(
                                    &cursor_row,
                                    Some(SidebarRow::Worktree(p)) if *p == wt.path
                                );
                                let wt_scroll = scrolls(is_cursor) || follows_worktree(&wt.path);
                                let is_deleting = deleting_paths.contains(&wt.path);
                                // A `\\wsl.localhost\` stat boots the distro's
                                // 9P server, so probing one would restart a VM
                                // the user had shut down and hold it resident
                                // for as long as its worktrees are listed.
                                // WSL rows keep discovery's word.
                                if probing
                                    && matches!(
                                        wsl::classify(&wt.path),
                                        wsl::Location::Windows(_)
                                    )
                                {
                                    drawn_worktrees.borrow_mut().push(wt.path.clone());
                                }
                                let action = worktree_row(
                                    ui,
                                    wt,
                                    self.liveness.missing(&wt.path),
                                    worktree_labels
                                        .get(idx)
                                        .and_then(|v| v.get(wt_idx))
                                        .map(String::as_str)
                                        .unwrap_or(&wt.name),
                                    pr_infos
                                        .get(idx)
                                        .and_then(|v| v.get(wt_idx))
                                        .and_then(Option::as_ref),
                                    is_active,
                                    is_cursor,
                                    wt_scroll,
                                    wt_attention,
                                    wt_activity,
                                    is_deleting,
                                    &worktree_profiles,
                                    &icons,
                                    &theme,
                                );
                                if action.activate {
                                    activate_request.set(Some(wt.path.clone()));
                                }
                                if action.delete {
                                    delete_request.set(Some(wt.path.clone()));
                                }
                                if action.spawn {
                                    spawn_shell_request.set(Some(Some(wt.path.clone())));
                                }
                                if action.set_base {
                                    base_picker_request.set(Some(wt.path.clone()));
                                }
                                if let Some(name) = action.spawn_profile {
                                    spawn_profile_request.set(Some((wt.path.clone(), name)));
                                }
                                session_drop(ui, action.rect, &Some(wt.path.clone()), None);
                                let listed_rows = worktree_rows
                                    .get(idx)
                                    .and_then(|v| v.get(wt_idx))
                                    .map(Vec::as_slice)
                                    .unwrap_or(&[]);
                                let mut slot = 0usize;
                                for row in listed_rows {
                                    match row {
                                        WorkspaceRowData::Session(row) => {
                                            let is_cursor = matches!(
                                                &cursor_row,
                                                Some(SidebarRow::Session(id)) if *id == row.id
                                            );
                                            let scroll =
                                                scrolls(is_cursor) || follows_session(row.id);
                                            let movable = row.managed.is_none();
                                            let act = session_row(
                                                ui,
                                                row,
                                                is_cursor,
                                                scroll,
                                                session_drag && movable,
                                                &icons,
                                                &theme,
                                            );
                                            if act.activate {
                                                activate_session_request
                                                    .set(Some((Some(wt.path.clone()), row.id)));
                                            }
                                            if act.close {
                                                close_session_request.set(Some(row.id));
                                            }
                                            if movable {
                                                session_drop(
                                                    ui,
                                                    act.rect,
                                                    &Some(wt.path.clone()),
                                                    Some((slot, row.id)),
                                                );
                                                slot += 1;
                                            }
                                        },
                                        WorkspaceRowData::Herdr(row) => {
                                            let is_cursor = matches!(
                                                &cursor_row,
                                                Some(SidebarRow::HerdrAgent(side, id))
                                                    if *side == row.side
                                                        && *id == row.terminal_id
                                            );
                                            let scroll = scrolls(is_cursor);
                                            let act = herdr_row(
                                                ui, row, is_cursor, scroll, &icons, &theme,
                                            );
                                            if act.attach {
                                                attach_herdr_request.set(Some((
                                                    Some(wt.path.clone()),
                                                    herdr::HerdrKey {
                                                        side: row.side.clone(),
                                                        terminal_id: row.terminal_id.clone(),
                                                    },
                                                    row.pane_id.clone(),
                                                )));
                                            }
                                        },
                                    }
                                }
                            }
                            for (_, branch) in creating.iter().filter(|(pi, _)| *pi == idx) {
                                creating_row(ui, branch, &icons, &theme);
                            }
                            group_gap = 4.0;
                        }
                    }
                });
            });

        if add_project_clicked {
            self.add_project_via_dialog(ctx);
        }
        if reorder_toggled {
            self.reorder_mode = !self.reorder_mode;
        }
        if let Some(idx) = refresh_idx {
            self.refresh_project(ctx, idx);
        }
        if let Some(req) = remove_request {
            self.pending_project_remove = Some(req);
        }
        if let Some((root, insert_before)) = reorder_request.take() {
            self.move_project(&root, insert_before);
        }
        if let Some((id, workspace, position)) = session_drop_request.take() {
            self.apply_session_drop(id, workspace, position);
        }
        if let Some((root, expanded)) = expand_toggled {
            state::mutate(|s| {
                if let Some(p) = s.projects.iter_mut().find(|p| p.root == root) {
                    p.expanded = expanded;
                }
            });
        }
        if let Some(root) = shell_override_changed {
            self.persist_project(&root);
        }
        if let Some(root) = label_cleared {
            self.persist_project_label(&root);
        }
        if rename_request.is_some() {
            self.pending_rename = rename_request;
        }
        let mut workspace_activated = false;
        if home_clicked {
            self.activate_home(ctx);
            workspace_activated = true;
        }
        if let Some(path) = activate_request.take() {
            self.activate_worktree(ctx, &path);
            workspace_activated = true;
        }
        if let Some(path) = base_picker_request.take() {
            self.open_base_branch_picker(path);
        }
        if let Some(path) = delete_request.take() {
            self.request_worktree_delete(&path);
        }
        if let Some(idx) = create_request.take() {
            self.pending_create =
                Some(CreateState::Prompt { project_idx: idx, branch: String::new(), error: None });
        }
        if let Some((ws, id)) = activate_session_request.take() {
            // A stale id (session reaped this frame) self-heals next frame:
            // active_session_index() misses and adopt_active_session picks
            // an existing shell, or the empty-workspace placeholder shows.
            self.current_workspace = ws.clone();
            self.active_session.insert(ws, id);
            workspace_activated = true;
        }
        if let Some(id) = close_session_request.take() {
            self.request_close_session(ctx, id);
        }
        if let Some((ws, key, pane_id)) = attach_herdr_request.take() {
            // Switches first, same as `spawn_shell_request` below: a refusal
            // is only visible if the workspace it happened in is on screen.
            let previous = std::mem::replace(&mut self.current_workspace, ws.clone());
            if self.attach_herdr_agent(ctx, key, &pane_id, ws) {
                workspace_activated = true;
            } else {
                self.current_workspace = previous;
            }
        }
        if let Some(ws) = spawn_shell_request.take() {
            // Spawning activates the workspace and the new session, matching
            // Ctrl+T and worktree-creation's open-on-done.  An `Err` here
            // arrived before the session record did — a checkout git has
            // forgotten, or a PTY opened inline — and hands the workspace
            // back rather than stranding the user on one with no shell, the
            // same reasoning as `activate_worktree`.  A PTY opened on a
            // worker fails after the record exists, so the switch stands and
            // `poll_pending_spawns` leaves the pane on the "no session"
            // placeholder: every workspace it could hand back to is one
            // `ensure_active_session` would spawn into and fail identically.
            let previous = std::mem::replace(&mut self.current_workspace, ws.clone());
            match self.spawn_session(ctx, ws.clone()) {
                Ok(_) => workspace_activated = true,
                Err(e) => {
                    self.current_workspace = previous;
                    self.report_spawn_failure(ctx, &ws, &e);
                },
            }
        }
        if let Some((path, name)) = spawn_profile_request.take() {
            // Same activate-on-success and stale-row-recovery shape as
            // `spawn_shell_request`: a stale worktree row's `+` reaches
            // `report_spawn_failure` today, and a profile picked from the
            // same row's menu must un-grey it the same way.
            let ws = Some(path);
            let previous = std::mem::replace(&mut self.current_workspace, ws.clone());
            match self.spawn_profile_session_in(ctx, &name, ws.clone()) {
                Ok(_) => workspace_activated = true,
                Err(e) => {
                    self.current_workspace = previous;
                    self.report_spawn_failure(ctx, &ws, &e);
                },
            }
        }
        self.poll_worktree_liveness(ctx, probing, &drawn_worktrees.into_inner());
        if self.config.ui.sidebar_click_focus {
            // A click that picks a workspace or session means "go work
            // there", so it focuses the terminal; other panel clicks focus
            // the sidebar for filter typing.  Row activations fire on the
            // release frame, after the press already focused the sidebar,
            // which is why this can't fold into the press test below.
            if workspace_activated {
                self.focus_terminal();
            } else if self.focus != PaneFocus::ProjectsSidebar
                && pressed_on_panel(ctx, &panel_resp.response)
            {
                self.focus_sidebar();
            }
        }
        panel_resp.response.rect
    }

    fn active_session_path(&self) -> Option<PathBuf> {
        self.current_workspace.clone()
    }

    /// The home directory a workspace path should collapse to.  A WSL path's
    /// home lives inside the distro and is only known through discovery, so a
    /// project that has not finished discovering yet simply gets no `~`.
    fn workspace_home(&self, path: &Path) -> Option<String> {
        match wsl::classify(path) {
            wsl::Location::Wsl { .. } => self
                .projects
                .iter()
                .find(|p| p.worktrees.iter().any(|w| w.path == path))
                .and_then(|p| p.home.clone()),
            wsl::Location::Windows(_) => home::home_dir().map(|h| h.display().to_string()),
        }
    }

    fn project_default_branch_for(&self, path: &Path) -> Option<String> {
        for project in &self.projects {
            for wt in &project.worktrees {
                if wt.path == path {
                    return project.default_branch.clone();
                }
            }
        }
        None
    }

    fn open_base_branch_picker(&mut self, worktree: PathBuf) {
        let detected = self.project_default_branch_for(&worktree);
        let job_worktree = worktree.clone();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
            crate::worktree::list_branches(&job_worktree, blocking)
        });
        self.pending_base_branch = Some(BaseBranchPicker {
            worktree,
            query: String::new(),
            branches: None,
            branches_job: Some(job),
            detected,
            cursor: 0,
        });
    }

    fn apply_base_branch(&mut self, worktree: PathBuf, branch: Option<String>) {
        match &branch {
            Some(b) => {
                self.base_branch_overrides.insert(worktree.clone(), b.clone());
            },
            None => {
                self.base_branch_overrides.remove(&worktree);
            },
        }
        // The next `StatusCache::poll` sees the changed hint and recomputes;
        // nothing to invalidate by hand.
        state::mutate(|s| state::set_base_branch(s, &worktree, branch));
    }

    fn show_git_sidebar(&mut self, ctx: &Context, panel_frame: Frame) -> egui::Rect {
        let theme = self.theme;
        let scrollbar = self.config.ui.scrollbar;
        let palette = self.config.palette.clone();
        let active_diff_key = self.active_diff_key();
        let diff_request: std::cell::Cell<Option<DiffRequest>> = std::cell::Cell::new(None);
        let open_picker: std::cell::Cell<Option<PathBuf>> = std::cell::Cell::new(None);
        let panel_resp = SidePanel::right("right_sidebar")
            .resizable(true)
            .default_width(300.0 * theme.ui_scale)
            .min_width(220.0 * theme.ui_scale)
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Sidebar rows are click targets, not selectable prose; the
                // default I-beam-and-select on labels is the wrong affordance.
                ui.style_mut().interaction.selectable_labels = false;
                apply_scrollbar_style(ui, scrollbar);
                ui.horizontal(|ui| {
                    panel_header_filter_ui(
                        ui,
                        "Git",
                        &self.git_filter,
                        &self.config.ui.icons.search,
                        &theme,
                        self.git_filter.toggles_apply(self.search_scope),
                    );
                });
                ui.separator();

                let path = match self.active_session_path() {
                    Some(p) => p,
                    None => {
                        // No workspace, no rows: keep the cursor model from
                        // acting on stale rows left by a previous workspace.
                        self.git_rows.clear();
                        self.git_branch_base = None;
                        ScrollArea::vertical().show(ui, |ui| {
                            ui.label(
                                RichText::new("Open a worktree from the left sidebar.")
                                    .color(theme.text_dim)
                                    .small(),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Ctrl+G to toggle").small().color(theme.text_muted),
                            );
                        });
                        return;
                    },
                };
                let workspace_home = self.workspace_home(&path);

                let project_default = self.project_default_branch_for(&path);
                let cache = self
                    .git_status
                    .entry(path.clone())
                    .or_insert_with(|| StatusCache::new(path.clone()));

                // Use whatever branch the cache already knows to query the PR
                // cache without waiting for a fresh compute — first frame may
                // be `None`, which `pr_cache.poll` handles by returning early.
                let cached_branch = cache.current_branch().map(str::to_string);
                let pr_info = self.pr_cache.poll(&path, cached_branch.as_deref(), ctx);
                let effective_default = effective_base_branch(
                    self.base_branch_overrides.get(&path).map(String::as_str),
                    pr_info.as_ref().map(|p| p.base_branch.as_str()),
                    project_default.as_deref(),
                );
                // Single non-blocking poll: returns the last known status and
                // kicks off a background refresh if stale or if the hint
                // changed since the last completed compute.  Cloned so the
                // `self.git_status` borrow ends before the cursor repair below
                // mutates other `self` fields.
                let status = cache.poll(effective_default.as_deref(), ctx).clone();

                // Prefer the resolved ref (e.g. `refs/remotes/origin/main`) so
                // the cursor's Enter-to-diff matches the branch section's rows.
                let git_branch_base = status
                    .default_branch_resolved
                    .clone()
                    .or_else(|| status.default_branch.clone());
                let filtering = self.git_filter.is_filtering();
                let filtered = self.filtered_git_rows(&status);
                let staged_count = filtered.staged;
                let unstaged_count = filtered.unstaged;
                let branch_count = filtered.branch;
                self.git_rows = filtered.rows;
                let mut staged_visible: HashSet<String> = HashSet::new();
                let mut unstaged_visible: HashSet<String> = HashSet::new();
                let mut branch_visible: HashSet<String> = HashSet::new();
                for row in &self.git_rows {
                    match row.section {
                        GitSection::Staged => &mut staged_visible,
                        GitSection::Unstaged => &mut unstaged_visible,
                        GitSection::Branch => &mut branch_visible,
                    }
                    .insert(row.path.clone());
                }
                self.git_branch_base = git_branch_base.clone();
                if self.focus == PaneFocus::GitSidebar {
                    let mut repaired =
                        git_nav::ensure_cursor(&self.git_rows, self.git_cursor.as_ref());
                    // An unseeded cursor lands on the row backing the open diff
                    // when there is one, so focusing the panel points at what
                    // the user is already looking at.
                    if self.git_cursor.is_none() {
                        if let Some(active) = active_diff_key.as_deref() {
                            if let Some(row) = self.git_rows.iter().find(|r| {
                                git_row_diff_request(r, git_branch_base.as_deref())
                                    .is_some_and(|req| diff_key(&req) == active)
                            }) {
                                repaired = Some(row.clone());
                            }
                        }
                    }
                    self.git_cursor = repaired;
                }
                let cursor_row = if self.focus == PaneFocus::GitSidebar {
                    self.git_cursor.clone()
                } else {
                    None
                };
                let cursor_moved = std::mem::take(&mut self.git_cursor_moved);

                ScrollArea::vertical().show(ui, |ui| {
                    if let Some(err) = &status.error {
                        ui.label(
                            RichText::new(err).color(rgb_to_color32(palette.normal[1])).small(),
                        );
                        return;
                    }

                    path_header_label(
                        ui,
                        &wsl::display_path(&path),
                        theme.text_muted,
                        &theme,
                        theme.path_style.git_header,
                        workspace_home.as_deref(),
                    );
                    if let Some(branch) = &status.branch {
                        // A greedy `truncate()` label in a plain `horizontal` row
                        // consumes all the width, shoving any trailing widgets past
                        // the panel edge. Since the right sidebar's `ScrollArea`
                        // grows to fit its content, that overflow ratchets the whole
                        // panel wider every frame until the full branch name fits.
                        // Pin `vs <default>` to the right and let the current branch
                        // truncate in the space that's left, so the row can't overflow.
                        let default = status
                            .default_branch
                            .as_deref()
                            .filter(|default| *default != branch.as_str());
                        row_with_trailing(
                            ui,
                            |ui| {
                                ui.label(RichText::new("on").color(theme.text_muted).small());
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(branch).color(theme.accent).small().strong(),
                                    )
                                    .truncate(),
                                );
                            },
                            |ui| {
                                if let Some(default) = default {
                                    // right_to_left: default sits rightmost, `vs` to its left.
                                    let resp = icon_tooltip(
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(default)
                                                    .color(theme.text_dim)
                                                    .small(),
                                            )
                                            .truncate()
                                            .sense(egui::Sense::click()),
                                        )
                                        .on_hover_cursor(egui::CursorIcon::PointingHand),
                                        "Set the branch this panel diffs against",
                                        theme.icon_tooltips,
                                    );
                                    if resp.clicked() {
                                        open_picker.set(Some(path.clone()));
                                    }
                                    ui.label(RichText::new("vs").color(theme.text_muted).small());
                                }
                            },
                        );
                    }
                    let mut section_gap = 10.0_f32;

                    section(
                        ui,
                        &theme,
                        "Staged",
                        staged_count,
                        filtering,
                        &mut section_gap,
                        |ui| {
                            for f in &status.staged {
                                if !staged_visible.contains(&f.path) {
                                    continue;
                                }
                                let req = DiffRequest {
                                    file: f.path.clone(),
                                    source: DiffSource::Staged,
                                };
                                let is_active = active_diff_key.as_deref() == Some(&diff_key(&req));
                                let resp = file_row(ui, f, &theme, &palette, is_active);
                                if resp.clicked() {
                                    diff_request.set(Some(req));
                                }
                                paint_git_row_cursor(
                                    ui,
                                    &resp,
                                    &cursor_row,
                                    GitSection::Staged,
                                    &f.path,
                                    cursor_moved,
                                    &theme,
                                );
                            }
                        },
                    );

                    section(
                        ui,
                        &theme,
                        "Unstaged",
                        unstaged_count,
                        filtering,
                        &mut section_gap,
                        |ui| {
                            for f in &status.unstaged {
                                if !unstaged_visible.contains(&f.path) {
                                    continue;
                                }
                                let source = if f.kind == ChangeKind::Untracked {
                                    DiffSource::Untracked
                                } else {
                                    DiffSource::Worktree
                                };
                                let req = DiffRequest { file: f.path.clone(), source };
                                let is_active = active_diff_key.as_deref() == Some(&diff_key(&req));
                                let resp = file_row(ui, f, &theme, &palette, is_active);
                                if resp.clicked() {
                                    diff_request.set(Some(req));
                                }
                                paint_git_row_cursor(
                                    ui,
                                    &resp,
                                    &cursor_row,
                                    GitSection::Unstaged,
                                    &f.path,
                                    cursor_moved,
                                    &theme,
                                );
                            }
                        },
                    );

                    if !status.branch_diff.is_empty() {
                        let base_label = match &status.default_branch {
                            Some(b) => format!("Changes vs {b}"),
                            None => "Changes vs default".to_string(),
                        };
                        let base = git_branch_base.clone();
                        let count_label = section_count_label(&branch_count, filtering);

                        ui.add_space(std::mem::take(&mut section_gap));
                        // Open-coded section header so the PR number can be a
                        // hyperlink while the rest stays plain text.
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&base_label).color(theme.text).strong().small());
                            if let Some(pr) = &pr_info {
                                ui.label(RichText::new("·").color(theme.text_muted).small());
                                ui.hyperlink_to(
                                    RichText::new(format!("PR #{}", pr.number))
                                        .color(theme.accent)
                                        .small()
                                        .strong(),
                                    &pr.url,
                                );
                            }
                            ui.label(RichText::new(count_label).color(theme.text_muted).small());
                        });
                        ui.add_space(2.0);
                        for stat in &status.branch_diff {
                            if !branch_visible.contains(&stat.path) {
                                continue;
                            }
                            let Some(base) = base.clone() else {
                                let resp = branch_diff_row(ui, stat, &theme, &palette, false);
                                paint_git_row_cursor(
                                    ui,
                                    &resp,
                                    &cursor_row,
                                    GitSection::Branch,
                                    &stat.path,
                                    cursor_moved,
                                    &theme,
                                );
                                continue;
                            };
                            let req = DiffRequest {
                                file: stat.path.clone(),
                                source: DiffSource::Branch { base },
                            };
                            let is_active = active_diff_key.as_deref() == Some(&diff_key(&req));
                            let resp = branch_diff_row(ui, stat, &theme, &palette, is_active);
                            if resp.clicked() {
                                diff_request.set(Some(req));
                            }
                            paint_git_row_cursor(
                                ui,
                                &resp,
                                &cursor_row,
                                GitSection::Branch,
                                &stat.path,
                                cursor_moved,
                                &theme,
                            );
                        }
                    }
                });
            });
        if let Some(req) = diff_request.take() {
            self.open_diff(ctx, req);
        }
        if let Some(path) = open_picker.take() {
            self.open_base_branch_picker(path);
        }
        if self.config.ui.sidebar_click_focus
            && self.focus != PaneFocus::GitSidebar
            && pressed_on_panel(ctx, &panel_resp.response)
        {
            self.focus_git_sidebar();
        }
        panel_resp.response.rect
    }

    /// Clicking a sidebar row either opens, replaces, or closes the workspace's
    /// single diff pane:
    /// - row matches the active diff → toggle off (close)
    /// - row matches a different diff → drop the old pane, open this one
    /// - no active diff → open a new pane
    /// Dropping the old `Session` runs `Drop`, which sends `Msg::Shutdown` to
    /// the event loop and exits delta cleanly.
    fn open_diff(&mut self, ctx: &Context, req: DiffRequest) {
        let Some(workspace) = self.current_workspace.clone() else {
            return;
        };
        let new_key = diff_key(&req);
        let existing = self.sessions.iter().find(|s| {
            s.working_directory.as_deref() == Some(&workspace)
                && matches!(&s.kind, SessionKind::Diff { .. })
        });
        if let Some(session) = existing {
            let id = session.id;
            if matches!(&session.kind, SessionKind::Diff { key } if key == &new_key) {
                // Routing through close_session applies the same
                // sibling-promotion and fallback navigation as any other
                // close, so toggling off the diff pane never strands the
                // workspace on an empty view.
                self.close_session(ctx, id);
                return;
            }
            self.sessions.retain(|s| s.id != id);
        }

        let delta_override = self.config.delta_path.clone();
        let (program, args) = match wsl::classify(&workspace) {
            wsl::Location::Wsl { distro, .. } => match delta_override {
                Some(delta) => build_wsl_diff_command_direct(&distro, &workspace, &req, &delta),
                None => match self.wsl_delta_path(&distro, ctx) {
                    Some(delta) => build_wsl_diff_command_direct(&distro, &workspace, &req, &delta),
                    None => build_wsl_diff_command_login(&distro, &workspace, &req),
                },
            },
            wsl::Location::Windows(_) => {
                build_diff_command(delta_override.as_deref().unwrap_or("delta"), &req)
            },
        };
        let title = format!(
            "diff: {}",
            path_style::render(&req.file, self.config.ui.path_style.diff_title, None)
        );
        let (size, cell_size) = self.next_spawn_geometry();
        let (session, request) = Session::pending_command(
            ctx.clone(),
            &self.config,
            Some(workspace.clone()),
            size,
            cell_size,
            program,
            args,
            title,
            SessionKind::Diff { key: new_key },
        );
        match self.open_session(session, request) {
            Ok(id) => {
                self.active_session.insert(Some(workspace), id);
            },
            Err(e) => {
                self.error_dialog = Some(format!("failed to open diff: {e}"));
            },
        }
    }

    /// Cached absolute path of `delta` inside `distro`, if known.  Adopts a
    /// finished background discovery, then spawns one when the path is neither
    /// cached nor already in flight.  Returns `None` until the first discovery
    /// lands — callers fall back to the login-shell command meanwhile.  A miss
    /// is never cached, so the discovery re-runs and a mid-session install is
    /// picked up on a later open.
    fn wsl_delta_path(&mut self, distro: &str, ctx: &Context) -> Option<String> {
        match self.pending_delta.get(distro).map(|job| (job.poll(), job.failed())) {
            Some((Some(Some(path)), _)) => {
                self.pending_delta.remove(distro);
                self.wsl_delta_paths.insert(distro.to_string(), path);
            },
            // A found-nothing landing and a panicked lookup both clear the
            // pending entry: the former banked its answer, the latter has
            // none to bank, and either way it must not wedge this distro out
            // of ever being retried.
            Some((Some(None), _)) | Some((None, true)) => {
                self.pending_delta.remove(distro);
            },
            _ => {},
        }

        if let Some(path) = self.wsl_delta_paths.get(distro) {
            return Some(path.clone());
        }

        if !self.pending_delta.contains_key(distro) {
            let distro_owned = distro.to_string();
            let ctx = ctx.clone();
            let job = jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
                let found = wsl::discover_delta(&distro_owned, blocking);
                ctx.request_repaint();
                found
            });
            self.pending_delta.insert(distro.to_string(), job);
        }
        None
    }

    /// Key of the diff currently displayed in this workspace, if any.  Used by
    /// the sidebar to highlight the originating row so the toggle-on-reclick
    /// behavior is discoverable.
    fn active_diff_key(&self) -> Option<String> {
        self.sessions.iter().find_map(|s| {
            if s.working_directory != self.current_workspace {
                return None;
            }
            if let SessionKind::Diff { key } = &s.kind { Some(key.clone()) } else { None }
        })
    }
}

/// What the session on screen contributes to a spawn's geometry.
struct ActiveGeometry {
    size: TermSize,
    cell_size: (f32, f32),
    /// A scratchpad's size is fixed at construction: it takes the editor
    /// branch, so the pane never resizes it.
    is_scratchpad: bool,
}

/// Geometry a new PTY is born at, most exact source first: the active
/// session's own numbers, then the terminal pane's last painted size, then
/// the constant neither has anything to improve on.
///
/// A scratchpad drops out of the first tier, since its pinned size would
/// otherwise shadow the pane geometry with a constant worse than the tier
/// below it.
fn spawn_geometry(
    active: Option<ActiveGeometry>,
    last_pane: Option<(TermSize, (f32, f32))>,
) -> (TermSize, (f32, f32)) {
    active
        .filter(|active| !active.is_scratchpad)
        .map(|active| (active.size, active.cell_size))
        .or(last_pane)
        .unwrap_or((TermSize::new(80, 24), (8.0, 16.0)))
}

/// What one session contributes to the GUI's own priority boost for a frame.
struct SessionBoost {
    /// The session's job holds a boost of its own.
    raised: bool,
    /// The session is the one on screen, with the window focused.
    visible: bool,
    /// The session's PTY is still opening.
    pending: bool,
}

/// Whether this session is a reason for the GUI to stay boosted.  A session
/// still opening its PTY has no job to raise yet but will have one within a
/// frame or two, and counting it is what stops a spawn dropping the GUI to
/// normal priority for the whole open and raising it again on attach.
fn holds_self_boost(session: SessionBoost) -> bool {
    session.raised || (session.visible && session.pending)
}

/// Whether a frame's sessions, taken together, are a reason for the GUI to
/// stay boosted.  Folded rather than `any`, because the caller computes each
/// `SessionBoost` by asking a session to raise or drop its own boost: every
/// session has to be reached, whatever the sessions before it answered.
fn frame_holds_self_boost(boosts: impl Iterator<Item = SessionBoost>) -> bool {
    boosts.fold(false, |held, session| held | holds_self_boost(session))
}

/// git arguments (everything after `git`) for the requested diff — shared
/// by the Windows and WSL pane commands.
fn diff_args(req: &DiffRequest) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    match &req.source {
        DiffSource::Staged => args.push("--cached".to_string()),
        DiffSource::Worktree => {},
        // `--no-index` against /dev/null shows the untracked file as a pure
        // addition; git special-cases "/dev/null" on every platform. Exits
        // non-zero by design.
        DiffSource::Untracked => args.push("--no-index".to_string()),
        // Triple-dot diff = "from merge-base to HEAD" — matches the sidebar's
        // `Changes vs <branch>` stat semantics in git_status.rs.
        DiffSource::Branch { base } => args.push(format!("{base}...")),
    }
    args.push("--".to_string());
    if matches!(req.source, DiffSource::Untracked) {
        args.push("/dev/null".to_string());
    }
    args.push(req.file.clone());
    args
}

/// Show the clicked file's `git diff` in `delta`, wired in as git's pager so
/// git drives the pipe itself.  This drops the POSIX-`sh` dependency the old
/// `sh -c '… | delta'` had — which had no equivalent on Windows, so diffs never
/// opened there.  Paths/branches stay in argv, so no file name is shell-parsed.
/// `delta` is the resolved program (bare `delta` from PATH, or a user override).
fn build_diff_command(delta: &str, req: &DiffRequest) -> (String, Vec<String>) {
    let mut args = vec!["-c".to_string(), format!("core.pager={delta} --paging=always")];
    args.extend(diff_args(req));
    ("git".to_string(), args)
}

/// The distro-side diff when `delta`'s absolute path is known (autodiscovered
/// or a user override): a plain `sh` finds it without sourcing a login profile,
/// so this avoids the per-open profile cost of the login fallback.
///
/// The `LESS=R` the diff pane puts in the child's environment stays on the
/// Windows side of the wsl.exe boundary (only `WSLENV`-listed variables
/// cross), so git in the distro would hand its pager `LESS=FRX` and `F`
/// (quit-if-one-screen) would reap short diffs on open.  The script exports
/// `LESS` itself where git runs.  Diff arguments travel as positional
/// parameters, so no file name is shell-parsed.
fn build_wsl_diff_command_direct(
    distro: &str,
    workspace: &Path,
    req: &DiffRequest,
    delta: &str,
) -> (String, Vec<String>) {
    let script = format!(
        r#"export LESS="${{LESS-R}}"; exec git -c "core.pager={delta} --paging=always" "$@""#
    );
    let mut args = vec![
        "-d".to_string(),
        distro.to_string(),
        "--cd".to_string(),
        workspace.to_string_lossy().into_owned(),
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script,
        "sh".to_string(),
    ];
    args.extend(diff_args(req));
    ("wsl.exe".to_string(), args)
}

/// The distro-side diff before `delta`'s path is known: resolve the user's
/// login shell (`getent passwd`) and re-exec through it so `delta` resolves
/// from their real PATH — `--exec sh` alone only sees the default system PATH,
/// which omits per-user install dirs like `~/.cargo/bin`.  The `LESS` export
/// happens inside the login shell's script, after the profile is sourced, so
/// a profile-set `LESS` wins — mirroring the `[env]` precedence on the
/// Windows side.  Diff arguments travel as positional parameters through both
/// shells, so no file name is shell-parsed.
fn build_wsl_diff_command_login(
    distro: &str,
    workspace: &Path,
    req: &DiffRequest,
) -> (String, Vec<String>) {
    let script = r#"s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$s" ] || s=${SHELL:-/bin/sh}; exec "$s" -lc 'export LESS="${LESS-R}"; exec git -c "core.pager=delta --paging=always" "$@"' "$s" "$@""#;
    let mut args = vec![
        "-d".to_string(),
        distro.to_string(),
        "--cd".to_string(),
        workspace.to_string_lossy().into_owned(),
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
    ];
    args.extend(diff_args(req));
    ("wsl.exe".to_string(), args)
}

fn wsl_shell(distro: &str, workdir: &Path) -> Shell {
    let (program, args) = wsl::shell_invocation(distro, workdir);
    Shell::new(program, args)
}

/// Shimmed when the resident helper is on; the plain wsl.exe login-shell
/// launch (and an unknown probe) otherwise.
fn wsl_session_shell(distro: &str, workdir: &Path) -> (Option<Shell>, Option<WslProbe>) {
    if !wsl_helper::enabled() {
        return (Some(wsl_shell(distro, workdir)), None);
    }
    let key = wsl_helper::new_probe_key();
    let (program, args) = wsl_helper::shim_invocation(distro, workdir, &key);
    (Some(Shell::new(program, args)), Some(WslProbe { distro: distro.to_string(), key }))
}

/// The probe shim for any user-supplied wsl.exe argv (profile or
/// `[terminal.shell]`): `Some` only when the argv is fully understood and
/// a distro name is known — the probe registry needs one, so a wrapped
/// default-distro launch resolves it via enumeration.  Anything exotic
/// runs unmodified and probes as unknown.
fn shimmed_wsl_argv(program: &str, args: &[String]) -> Option<(Shell, WslProbe)> {
    if !wsl_helper::enabled() {
        return None;
    }
    let key = wsl_helper::new_probe_key();
    let (args, distro) = wsl_helper::wrap_profile_argv(program, args, &key)?;
    let distro =
        distro.or_else(|| wsl::distros().into_iter().find(|d| d.is_default).map(|d| d.name))?;
    Some((Shell::new(program.to_string(), args), WslProbe { distro, key }))
}

fn profile_session_shell(profile: &crate::config::Profile) -> (Option<Shell>, Option<WslProbe>) {
    match shimmed_wsl_argv(&profile.program, &profile.args) {
        Some((shell, probe)) => (Some(shell), Some(probe)),
        None => (Some(profile_shell(profile)), None),
    }
}

/// `[terminal.shell] program = "wsl.exe"` gets the same shim as a wsl.exe
/// profile; any other config shell (or none) spawns unchanged through
/// `Session::pending_shell`'s own config-shell default.
fn config_session_shell(config: &crate::config::Config) -> (Option<Shell>, Option<WslProbe>) {
    match &config.shell {
        Some(s) => match shimmed_wsl_argv(&s.program, &s.args) {
            Some((shell, probe)) => (Some(shell), Some(probe)),
            None => (None, None),
        },
        None => (None, None),
    }
}

/// What shell a new session should run, decided from plain data so the
/// precedence chain stays testable off the GUI.
#[derive(Debug, PartialEq, Eq)]
pub enum ShellDecision {
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
pub fn shell_decision(
    override_choice: Option<&ShellChoice>,
    location_distro: Option<&str>,
    known_distros: &[String],
    profiles: &[crate::config::Profile],
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

fn profile_shell(profile: &crate::config::Profile) -> Shell {
    Shell::new(profile.program.clone(), profile.args.clone())
}

/// `git worktree remove` refuses a tree with work in it, and that refusal is
/// the authority on whether removing would lose anything. Both fragments are
/// real git wording: `contains modified or untracked files` is the current
/// message, `is dirty` is what git 2.17 (the version that introduced
/// `worktree remove`) said before the message was reworded.
///
/// `worktree.rs`'s failure string is `git <args>: fatal: '<path>' <reason>`,
/// and `<path>` (attacker- or at least user-controlled) is echoed twice —
/// once in the command args, once quoted right after `fatal:`. Matching
/// against the raw message would let a worktree path that happens to spell
/// out one of these fragments turn an unrelated failure (locked tree, main
/// worktree, filesystem error) into a false "needs --force" prompt. Since
/// git's own wording always lands after the closing quote of the path — never
/// inside it — cutting the tail at the last `'` drops both copies of the path
/// and leaves only text git itself authored.
fn refused_for_unsaved_work(message: &str) -> bool {
    let tail = message.rsplit_once("fatal:").map_or(message, |(_, tail)| tail);
    let reason = tail.rsplit_once('\'').map_or(tail, |(_, after)| after).to_ascii_lowercase();
    reason.contains("contains modified or untracked files, use --force")
        || reason.contains("is dirty, use --force")
}

fn dirty_parts(counts: &DirtyCounts) -> String {
    let mut parts = Vec::new();
    if counts.staged > 0 {
        parts.push(format!("{} staged", counts.staged));
    }
    if counts.modified > 0 {
        parts.push(format!("{} modified", counts.modified));
    }
    if counts.untracked > 0 {
        parts.push(format!("{} untracked", counts.untracked));
    }
    parts.join(", ")
}

/// The delete confirm's warning line.
///
/// `counts` is `None` until a count lands (`checking`) or after a probe
/// failed and left nothing to show (`!checking`). `force` is whether this
/// confirm would pass `--force` — a first attempt whose resolved count is
/// already known dirty, or the retry after git refused an unforced removal.
///
/// `force` is checked first: a forced retry followed git's own refusal, so
/// it is never safe to render "nothing to warn about" for it, regardless of
/// what `counts` holds — a stale-clean read, or none at all (the request was
/// confirmed before its probe landed, which cancelled the probe).
fn dirty_warning(counts: Option<&DirtyCounts>, force: bool, checking: bool) -> Option<String> {
    if force {
        return Some(match counts.filter(|c| c.is_dirty()) {
            Some(counts) => {
                format!(
                    "Working tree has {} file(s) — they will be discarded with --force.",
                    dirty_parts(counts)
                )
            },
            None => "git reported local changes; they will be discarded with --force.".to_string(),
        });
    }
    match counts {
        Some(counts) if counts.is_dirty() => {
            Some(format!("Working tree has {} file(s) with local changes.", dirty_parts(counts)))
        },
        Some(_) => None,
        None if checking => Some("Checking working tree for uncommitted changes…".to_string()),
        None => Some("Couldn't check the working tree for local changes.".to_string()),
    }
}

/// The modal frame's horizontal inner margin.  Any width budgeted against the
/// window has to leave room for it, so it lives apart from the frame itself.
fn modal_pad_x(scale: f32) -> f32 {
    (16.0 * scale).round()
}

fn modal_frame(theme: &Theme) -> Frame {
    let s = theme.ui_scale;
    let pad_x = modal_pad_x(s) as i8;
    let pad_y = (12.0 * s).round() as i8;
    Frame::default()
        .fill(theme.sidebar_bg)
        .stroke(Stroke::new(1.0_f32, theme.sidebar_border))
        .inner_margin(Margin { left: pad_x, right: pad_x, top: pad_y, bottom: pad_y })
}

/// Take this frame's palette cursor jumps off the event queue, honoring
/// rebinds.  The palette owns these keys only while it is up, which is why they
/// are read here rather than dispatched like an ordinary action — and why they
/// can share the sidebar's unmodified Home/End/PageUp/PageDown.
fn consume_palette_keys(ctx: &Context, bindings: &[KeyBinding]) -> Vec<NamedAction> {
    ctx.input_mut(|i| {
        let mut jumps = Vec::new();
        i.events.retain(|ev| {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = ev else {
                return true;
            };
            let matched: Vec<NamedAction> = bindings::all_matches(bindings, *key, *modifiers)
                .into_iter()
                .filter_map(|a| match a {
                    BindingAction::Named(n) if n.is_palette_scoped() => Some(*n),
                    _ => None,
                })
                .collect();
            if matched.is_empty() {
                return true;
            }
            jumps.extend(matched);
            false
        });
        jumps
    })
}

/// The palette's footer, naming the keys actually bound to its cursor moves so
/// a rebind shows up here instead of the hint quietly going stale.
fn palette_hint(bindings: &[KeyBinding]) -> String {
    let mut parts = vec!["↑↓ move".to_string()];
    for (action, label) in [
        (NamedAction::PaletteTop, "top"),
        (NamedAction::PaletteBottom, "bottom"),
        (NamedAction::PalettePageUp, "page up"),
        (NamedAction::PalettePageDown, "page down"),
    ] {
        if let Some(key) = command_palette::first_key(bindings, action) {
            parts.push(format!("{key} {label}"));
        }
    }
    parts.push("Enter run".into());
    parts.push("Esc close".into());
    parts.join(" · ")
}

fn consume_modal_keys(ctx: &Context) -> (bool, bool) {
    ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
        )
    })
}

/// Move focus to `id` if no widget currently has it — gives the modal's
/// primary control focus on open without stealing it from the user later.
fn focus_default(ctx: &Context, id: egui::Id) {
    let has_focus = ctx.memory(|m| m.focused().is_some());
    if !has_focus {
        ctx.memory_mut(|m| m.request_focus(id));
    }
}

/// What a palette column does with text too wide for it.  epaint overruns the
/// column rather than splitting a word unless told it may break anywhere, so
/// the choice follows the content: prose can rely on its spaces, a lone
/// identifier or key chord cannot.
#[derive(Clone, Copy)]
enum ColumnWrap {
    /// One line, ellipsized at the column edge — the scannable default.
    Clip,
    /// Wrap at word boundaries, over as many lines as it takes.
    Words,
    /// Wrap mid-token if that is the only way to stay inside the column.
    Anywhere,
}

impl ColumnWrap {
    fn limits(self) -> (usize, bool) {
        match self {
            Self::Clip => (1, true),
            Self::Words => (usize::MAX, false),
            Self::Anywhere => (usize::MAX, true),
        }
    }
}

/// A palette column's text, laid out to `max_w`.  Whatever still does not fit
/// is ellipsized, which the caller reads back off the galley's `elided` flag to
/// offer the full text on hover.
fn column_galley(
    ctx: &Context,
    text: &str,
    family: egui::FontFamily,
    size: f32,
    color: Color32,
    max_w: f32,
    wrap: ColumnWrap,
) -> std::sync::Arc<egui::Galley> {
    use egui::text::{LayoutJob, TextFormat};
    let (max_rows, break_anywhere) = wrap.limits();
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat {
        font_id: egui::FontId::new(size, family),
        color,
        ..Default::default()
    });
    job.wrap.max_width = max_w.max(0.0);
    job.wrap.max_rows = max_rows;
    job.wrap.break_anywhere = break_anywhere;
    job.wrap.overflow_character = Some('…');
    ctx.fonts(|f| f.layout_job(job))
}

/// Prose laid out to `max_w`.  Wrapping at spaces reads best, but a word wider
/// than the column overruns it instead of breaking, so a galley that came back
/// too wide is laid out again mid-word.
fn prose_galley(
    ctx: &Context,
    text: &str,
    family: egui::FontFamily,
    size: f32,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let wrapped = column_galley(ctx, text, family.clone(), size, color, max_w, ColumnWrap::Words);
    if wrapped.size().x <= max_w {
        return wrapped;
    }
    column_galley(ctx, text, family, size, color, max_w, ColumnWrap::Anywhere)
}

/// The hover text for a row: the full text of whatever its columns had to cut,
/// and nothing at all when everything already reads in place.
fn elided_hover(columns: &[(bool, &str)]) -> Option<String> {
    let full: Vec<&str> =
        columns.iter().filter(|(elided, _)| *elided).map(|(_, text)| *text).collect();
    (!full.is_empty()).then(|| full.join("\n"))
}

/// The palette's comfortable content width, and the share of a window it may
/// take instead when the window cannot hold that.
const PALETTE_WIDTH: f32 = 760.0;
const PALETTE_SCREEN_FRACTION: f32 = 0.8;

/// How wide the palette's content may be.  A window too narrow for the
/// comfortable width sizes the palette against the window instead, so the modal
/// keeps a margin either side rather than running past both edges.
fn palette_content_width(scale: f32, screen_w: f32) -> f32 {
    let budget = screen_w * PALETTE_SCREEN_FRACTION - 2.0 * modal_pad_x(scale);
    budget.min(PALETTE_WIDTH * scale).max(0.0)
}

/// The action and keys columns' fixed widths, and the narrowest the description
/// still reads at beside them.
const PALETTE_ACTION_W: f32 = 200.0;
const PALETTE_KEYS_W: f32 = 180.0;
const PALETTE_DESC_MIN: f32 = 160.0;

/// Geometry for the palette's `description | action | keys` grid.  Every row and
/// the header lay out against the same widths, so the columns line up down the
/// list instead of each row packing its own way.  A grid with room for the fixed
/// widths gets them; a tighter one shrinks all three by the same factor and
/// wraps their text, rather than letting the last column run off the edge.
struct PaletteColumns {
    width: f32,
    pad: f32,
    desc: f32,
    action: f32,
    keys: f32,
    gap: f32,
    /// Set once the grid is tighter than its fixed widths, at which point every
    /// column wraps instead of ellipsizing.
    narrow: bool,
}

impl PaletteColumns {
    fn new(scale: f32, width: f32) -> Self {
        let pad = 10.0 * scale;
        let gap = 14.0 * scale;
        let content = (width - 2.0 * pad - 2.0 * gap).max(0.0);
        let action = PALETTE_ACTION_W * scale;
        let keys = PALETTE_KEYS_W * scale;
        let comfortable = PALETTE_DESC_MIN * scale + action + keys;
        if content >= comfortable {
            let desc = content - action - keys;
            return Self { width, pad, desc, action, keys, gap, narrow: false };
        }
        let shrink = content / comfortable;
        Self {
            width,
            pad,
            desc: PALETTE_DESC_MIN * scale * shrink,
            action: action * shrink,
            keys: keys * shrink,
            gap,
            narrow: true,
        }
    }

    /// How the action and keys columns lay out.  Their text is one unbroken
    /// token, so a narrow grid has to split it mid-word; a comfortable one
    /// keeps every row one line tall and ellipsizes the overflow.
    fn token_wrap(&self) -> ColumnWrap {
        if self.narrow { ColumnWrap::Anywhere } else { ColumnWrap::Clip }
    }

    fn desc_x(&self, left: f32) -> f32 {
        left + self.pad
    }

    fn action_x(&self, left: f32) -> f32 {
        self.desc_x(left) + self.desc + self.gap
    }

    fn keys_x(&self, left: f32) -> f32 {
        self.action_x(left) + self.action + self.gap
    }
}

/// The palette's column captions, on the same grid as its rows and outside the
/// scrolling list so they stay put while it moves.
fn paint_palette_header(ui: &mut egui::Ui, theme: &Theme, cols: &PaletteColumns) {
    let s = theme.ui_scale;
    let size = (theme.font_normal - 2.0).max(8.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(cols.width, size + 10.0 * s), egui::Sense::hover());
    let painter = ui.painter().clone();
    let left = rect.left();
    for (text, x, w) in [
        ("DESCRIPTION", cols.desc_x(left), cols.desc),
        ("ACTION", cols.action_x(left), cols.action),
        ("KEYS", cols.keys_x(left), cols.keys),
    ] {
        let g = column_galley(
            ui.ctx(),
            text,
            egui::FontFamily::Proportional,
            size,
            theme.text_muted,
            w,
            ColumnWrap::Clip,
        );
        painter.galley(egui::pos2(x, rect.top() + 2.0 * s), g, theme.text_muted);
    }
    painter.hline(rect.x_range(), rect.bottom(), Stroke::new(1.0_f32, theme.sidebar_border));
}

/// A section heading in the palette list.  Not selectable — the cursor steps
/// over rows only.
fn paint_palette_section(ui: &mut egui::Ui, theme: &Theme, cols: &PaletteColumns, title: &str) {
    let s = theme.ui_scale;
    let size = (theme.font_normal - 2.0).max(8.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(cols.width, size + 14.0 * s), egui::Sense::hover());
    // A heading spans the row rather than the description column, so a narrow
    // grid does not cut it down to the width of the text beside it.
    let g = column_galley(
        ui.ctx(),
        &title.to_uppercase(),
        egui::FontFamily::Proportional,
        size,
        theme.accent,
        cols.width - 2.0 * cols.pad,
        ColumnWrap::Clip,
    );
    ui.painter().galley(
        egui::pos2(cols.desc_x(rect.left()), rect.bottom() - size - 3.0 * s),
        g,
        theme.accent,
    );
}

/// Paint one command-palette row across the three columns: the description
/// (bright, wrapped over as many lines as it needs), the action's config name
/// (dim), and every key bound to it (accent).  On a narrow grid the other two
/// columns wrap as well; whatever is still cut short offers its full text on
/// hover.  A selected row gets a soft accent wash and a crisp accent bar; a
/// hovered one a faint fill.
fn paint_palette_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    cols: &PaletteColumns,
    item: &PaletteItem,
    selected: bool,
) -> egui::Response {
    let s = theme.ui_scale;
    let v_pad = 6.0 * s;
    let ctx = ui.ctx();

    let token = cols.token_wrap();
    let desc = prose_galley(
        ctx,
        &item.primary,
        egui::FontFamily::Proportional,
        theme.font_normal,
        theme.text,
        cols.desc,
    );
    let action = column_galley(
        ctx,
        &item.secondary,
        egui::FontFamily::Proportional,
        theme.font_normal,
        theme.text_dim,
        cols.action,
        token,
    );
    let keys = column_galley(
        ctx,
        &item.keys,
        egui::FontFamily::Monospace,
        theme.font_normal,
        theme.accent,
        cols.keys,
        token,
    );
    let hover = elided_hover(&[
        (desc.elided, item.primary.as_str()),
        (action.elided, item.secondary.as_str()),
        (keys.elided, item.keys.as_str()),
    ]);

    let row_h = (desc.size().y.max(action.size().y).max(keys.size().y) + 2.0 * v_pad).round();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(cols.width, row_h), egui::Sense::click());
    let painter = ui.painter().clone();

    if selected {
        let wash = Color32::from_rgba_unmultiplied(
            theme.accent.r(),
            theme.accent.g(),
            theme.accent.b(),
            46,
        );
        painter.rect_filled(rect, 5.0 * s, wash);
        let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.5 * s, rect.height()));
        painter.rect_filled(bar, 0.0, theme.accent);
    } else if resp.hovered() {
        painter.rect_filled(rect, 5.0 * s, theme.row_hover_bg);
    }

    // Top-aligned, so a wrapped description's first line shares a baseline with
    // the single-line columns beside it.
    let (left, top) = (rect.left(), rect.top() + v_pad);
    painter.galley(egui::pos2(cols.desc_x(left), top), desc, theme.text);
    painter.galley(egui::pos2(cols.action_x(left), top), action, theme.text_dim);
    painter.galley(egui::pos2(cols.keys_x(left), top), keys, theme.accent);

    match hover {
        Some(text) => resp.on_hover_text(text),
        None => resp,
    }
}

/// A modal action button.  Framed and filled so it reads as clickable —
/// frameless text buttons looked like captions and users reached for the
/// keyboard hint instead of the mouse.
fn modal_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    text_color: Color32,
) -> egui::Response {
    let s = theme.ui_scale;
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(10.0 * s, 3.0 * s);
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = theme.row_hover_bg;
        widgets.inactive.bg_stroke = Stroke::new(1.0_f32, theme.sidebar_border);
        widgets.hovered.weak_bg_fill = theme.row_active_bg;
        widgets.hovered.bg_stroke = Stroke::new(1.0_f32, theme.sidebar_border);
        widgets.active.weak_bg_fill = theme.row_active_bg;
        ui.add(egui::Button::new(RichText::new(label).color(text_color)))
    })
    .inner
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Section header count: `visible of total` while a filter narrows the panel,
/// the plain total otherwise.
fn section_count_label(count: &SectionCount, filtering: bool) -> String {
    if filtering {
        format!("{} of {}", count.visible, count.total)
    } else {
        format!("{}", count.total)
    }
}

/// Render a collapsed-when-empty git section.
///
/// Empty sections are skipped entirely — a placeholder glyph for "no files
/// here" added visual noise without communicating anything the count badge
/// didn't already say.
///
/// `gap` carries the inter-section spacing: consumed above a section that
/// renders and re-armed below it, so spacing lands between sections but never
/// after the last one — trailing padding would make the content overflow the
/// panel and show a scrollbar with nothing to scroll.
fn section<R>(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: &str,
    count: SectionCount,
    filtering: bool,
    gap: &mut f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) {
    if count.total == 0 {
        return;
    }
    ui.add_space(std::mem::take(gap));
    let label = section_count_label(&count, filtering);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).color(theme.text).strong().small());
        ui.label(RichText::new(label).color(theme.text_muted).small());
    });
    ui.add_space(2.0);
    add_contents(ui);
    *gap = 10.0;
}

fn file_row(
    ui: &mut egui::Ui,
    change: &FileChange,
    theme: &Theme,
    palette: &crate::config::Palette,
    is_active: bool,
) -> egui::Response {
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();
    let row_h = ui.spacing().interact_size.y;
    let color = match change.kind {
        ChangeKind::Added | ChangeKind::Untracked => rgb_to_color32(palette.normal[2]),
        ChangeKind::Modified => rgb_to_color32(palette.normal[3]),
        ChangeKind::Deleted => rgb_to_color32(palette.normal[1]),
        ChangeKind::Renamed => rgb_to_color32(palette.normal[4]),
        ChangeKind::Conflicted => rgb_to_color32(palette.bright[1]),
    };
    let path_color = if is_active { theme.text } else { theme.text_dim };
    let mut path_galley = None;
    let mut hints = IconHints::default();
    // `ui.horizontal` sizes its response rect to the (often short) path text,
    // leaving most of the row's width as a dead zone — and short labels make
    // the row barely taller than the text, so vertical misses are easy too.
    // Allocate an explicit interact-sized row and pad it out so the click hit
    // box spans the full panel width and the row's full height.
    let resp = ui
        .allocate_ui_with_layout(
            egui::vec2(ui.available_width(), row_h),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_height(row_h);
                // Labels default to `Sense::click_and_drag` for text selection;
                // hit testing picks the smallest covering widget, so a clickable
                // label inside our row would eat clicks before the row sees
                // them.  Opt out of selection on every label that lives inside
                // a clickable row so the click falls through.
                let badge = ui.add(
                    egui::Label::new(
                        RichText::new(change.kind.glyph()).color(color).monospace().small(),
                    )
                    .selectable(false),
                );
                hints.add(badge.rect, change.kind.label());
                let (_, galley) = git_path_label(ui, &change.path, path_color, theme);
                path_galley = Some(galley);
                fill_row(ui);
            },
        )
        .response
        .interact(egui::Sense::click());
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| {
        git_path_tooltip(resp, path_galley.as_deref(), theme)
    });
    paint_row_bg(ui, &resp, bg_idx, panel_x, theme, is_active);
    resp
}

fn branch_diff_row(
    ui: &mut egui::Ui,
    stat: &crate::git_status::DiffStat,
    theme: &Theme,
    palette: &crate::config::Palette,
    is_active: bool,
) -> egui::Response {
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();
    let row_h = ui.spacing().interact_size.y;
    let added = rgb_to_color32(palette.normal[2]);
    let removed = rgb_to_color32(palette.normal[1]);
    let path_color = if is_active { theme.text } else { theme.text_dim };
    let mut path_galley = None;

    // Same shape as row_with_trailing (right_to_left wrapping a left_to_right)
    // so +/- counts pin to the right edge while the path truncates cleanly;
    // `set_min_height` + `fill_row` push the hit box to the full row size.
    let resp = ui
        .allocate_ui_with_layout(
            egui::vec2(ui.available_width(), row_h),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.set_min_height(row_h);
                if stat.deletions > 0 {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("-{}", stat.deletions))
                                .color(removed)
                                .small()
                                .monospace(),
                        )
                        .selectable(false),
                    );
                }
                if stat.additions > 0 {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("+{}", stat.additions))
                                .color(added)
                                .small()
                                .monospace(),
                        )
                        .selectable(false),
                    );
                }
                let remaining = ui.available_width();
                if remaining > 0.0 {
                    ui.allocate_ui_with_layout(
                        egui::vec2(remaining, row_h),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_min_height(row_h);
                            let (_, galley) = git_path_label(ui, &stat.path, path_color, theme);
                            path_galley = Some(galley);
                            fill_row(ui);
                        },
                    );
                }
            },
        )
        .response
        .interact(egui::Sense::click());
    let resp = git_path_tooltip(resp, path_galley.as_deref(), theme);
    paint_row_bg(ui, &resp, bg_idx, panel_x, theme, is_active);
    resp
}

/// Bold and italic are real faces rather than a colour swap, but only the
/// terminal font registers them — an emphasized span at a proportional site
/// keeps the weight and shifts family rather than losing the weight.
fn emphasis_family(e: &TextEmphasis, base: &egui::FontFamily) -> egui::FontFamily {
    match (e.bold, e.italic) {
        (true, true) => egui::FontFamily::Name(crate::fonts::BOLD_ITALIC_FAMILY.into()),
        (true, false) => egui::FontFamily::Name(crate::fonts::BOLD_FAMILY.into()),
        (false, true) => egui::FontFamily::Name(crate::fonts::ITALIC_FAMILY.into()),
        (false, false) => base.clone(),
    }
}

/// Add a truncating label, reporting its response and the galley it painted —
/// `elided` says whether the row had to ellipsize, `text()` spells the name out
/// in full however the label abbreviated it.
///
/// `egui::Label` offers an elided name as a tooltip by itself, but only to a
/// widget the hit test marks hovered — and a row that senses its click
/// retroactively, once its labels are already laid out, takes that mark away
/// from them.  Laying the galley out here keeps both decisions with the row:
/// which response carries the tooltip, and whether `[ui] sidebar_tooltips`
/// wants one at all.
///
/// `fallback_color` paints whatever spans the text left uncolored.  Selection
/// stays off whatever the surrounding style says: a selectable label unions
/// drag into `sense` and takes the click its row is waiting for.
fn truncating_label(
    ui: &mut egui::Ui,
    text: impl Into<egui::WidgetText>,
    fallback_color: Color32,
    sense: egui::Sense,
) -> (egui::Response, Arc<egui::Galley>) {
    let (pos, galley, response) =
        egui::Label::new(text).truncate().selectable(false).sense(sense).layout_in_ui(ui);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), galley.text())
    });
    ui.painter().galley(pos, galley.clone(), fallback_color);
    (response, galley)
}

/// The hints for icons a row paints inside itself, with the rect each covers.
///
/// A row allocates its frame at end-of-show, so its retroactive `interact`
/// registers after the icons in egui's z-order and takes the hover mark away
/// from them — an icon's own `on_hover_text` never opens. The row answers for
/// whichever icon the pointer is over instead, the same way it already routes
/// a click that landed on a button.
#[derive(Default)]
struct IconHints(Vec<(egui::Rect, String)>);

impl IconHints {
    fn add(&mut self, rect: egui::Rect, hint: impl Into<String>) {
        self.0.push((rect, hint.into()));
    }

    fn at(&self, pos: egui::Pos2) -> Option<&str> {
        self.0.iter().find(|(rect, _)| rect.contains(pos)).map(|(_, hint)| hint.as_str())
    }

    /// The tooltip `resp` should carry: an icon's hint where the pointer is on
    /// one, otherwise `fallback` for the rest of the row.
    fn apply(
        &self,
        resp: egui::Response,
        enabled: bool,
        fallback: impl FnOnce(egui::Response) -> egui::Response,
    ) -> egui::Response {
        match resp.hover_pos().and_then(|pos| self.at(pos)) {
            Some(hint) if enabled => resp.on_hover_text(hint.to_owned()),
            _ => fallback(resp),
        }
    }
}

/// Offer `hint` — what the icon under `resp` does or reports — as its tooltip,
/// unless `[ui] icon_tooltips` turns the hints off.
fn icon_tooltip(resp: egui::Response, hint: &str, enabled: bool) -> egui::Response {
    if enabled { resp.on_hover_text(hint) } else { resp }
}

/// Offer `name` as `resp`'s tooltip, as far as the configured mode allows.
fn name_tooltip(
    resp: egui::Response,
    name: &str,
    elided: bool,
    mode: SidebarTooltips,
) -> egui::Response {
    match mode {
        SidebarTooltips::Off => resp,
        SidebarTooltips::Elided if !elided => resp,
        _ => resp.on_hover_text(name),
    }
}

/// Lay a path out as the text of one truncating label.
///
/// `Zed` needs two differently-formatted spans, and one `LayoutJob` is the
/// only way to get them without an `item_spacing` gap between two labels, a
/// second response competing for the row's click, and a filename that can
/// overflow the width `row_with_trailing` is managing.  Putting the filename
/// first only *prioritizes* it: epaint truncates the tail of one linear glyph
/// stream, so a row narrower than the filename still elides it.
fn path_text(
    ui: &egui::Ui,
    path: &str,
    base: Color32,
    theme: &Theme,
    style: PathStyle,
    family: egui::FontFamily,
    home: Option<&str>,
) -> egui::WidgetText {
    if style != PathStyle::Zed {
        return RichText::new(path_style::render(path, style, home))
            .color(base)
            .family(family)
            .small()
            .into();
    }

    let size = egui::TextStyle::Small.resolve(ui.style()).size;
    // A hand-built job does not inherit the ui's text valign the way RichText
    // does, so it must be carried across or the path sits off-centre against
    // the change glyph beside it.
    let valign = ui.text_valign();
    let parts = path_style::split(path, style, home);
    let mut job = egui::text::LayoutJob::default();
    let mut push = |text: String, e: &TextEmphasis| {
        if text.is_empty() {
            return;
        }
        job.append(&text, 0.0, egui::TextFormat {
            font_id: egui::FontId::new(size, emphasis_family(e, &family)),
            color: e.color.unwrap_or(base),
            valign,
            ..Default::default()
        });
    };
    let emphases = [&theme.path_style.filename, &theme.path_style.parent];
    for (text, e) in zed_spans(&parts).into_iter().zip(emphases) {
        push(text, e);
    }
    job.into()
}

/// The git panel's header path.  It stays selectable although the panel turns
/// label selection off, and — being a header rather than a row — keeps
/// `egui::Label`'s own elided-text tooltip instead of answering to
/// `[ui] sidebar_tooltips`.
fn path_header_label(
    ui: &mut egui::Ui,
    path: &str,
    base: Color32,
    theme: &Theme,
    style: PathStyle,
    home: Option<&str>,
) -> egui::Response {
    let text = path_text(ui, path, base, theme, style, egui::FontFamily::Proportional, home);
    ui.add(egui::Label::new(text).truncate().selectable(true))
}

/// A git panel row's path, laid out rather than added as an `egui::Label` so
/// its tooltip is the row's to give: the label covers only the text, and a
/// pointer sweeping down the panel spends most of its time past the end of
/// short paths, where a label-borne tooltip would go quiet.  The row passes the
/// galley back through `git_path_tooltip` once it has its full-width response.
fn git_path_label(
    ui: &mut egui::Ui,
    path: &str,
    base: Color32,
    theme: &Theme,
) -> (egui::Response, Arc<egui::Galley>) {
    let text = path_text(
        ui,
        path,
        base,
        theme,
        theme.path_style.git_rows,
        egui::FontFamily::Proportional,
        None,
    );
    truncating_label(ui, text, base, egui::Sense::hover())
}

/// Offer the row's own response the path its label painted, once the row has
/// one to hang it off.
fn git_path_tooltip(
    resp: egui::Response,
    galley: Option<&egui::Galley>,
    theme: &Theme,
) -> egui::Response {
    match galley {
        Some(galley) => name_tooltip(resp, galley.text(), galley.elided, theme.sidebar_tooltips),
        None => resp,
    }
}

/// The Zed style's span decomposition: the filename text, then — when there
/// is a parent to show — the parent text with its separating space already
/// folded in. Position carries the emphasis: span 0 always paints with the
/// filename emphasis, span 1 (if present) with the parent emphasis. Shared by
/// `path_label`'s job builder and its fidelity test so a regression in the
/// split logic fails the test that exercises the real render path.
fn zed_spans(parts: &path_style::Parts) -> Vec<String> {
    if parts.parent.is_empty() {
        vec![format!("{}{}", parts.root, parts.name)]
    } else {
        vec![parts.name.clone(), format!(" {}{}", parts.root, parts.parent)]
    }
}

/// Extend a row's bounding rect to its parent's full width so the response
/// covers the empty space past short labels, instead of just the content.
fn fill_row(ui: &mut egui::Ui) {
    let remaining = ui.available_width();
    if remaining > 0.0 {
        ui.allocate_space(egui::vec2(remaining, 0.0));
    }
}

fn paint_row_bg(
    ui: &mut egui::Ui,
    resp: &egui::Response,
    bg_idx: egui::layers::ShapeIdx,
    panel_x: egui::Rangef,
    theme: &Theme,
    is_active: bool,
) {
    let bg = if is_active {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        return;
    };
    let rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    ui.painter().set(bg_idx, egui::Shape::rect_filled(rect, 0.0, bg));
}

/// Footprint every leading row marker claims, whichever glyph it ends up
/// drawing. Markers vary wildly in intrinsic width (`·` vs `✳`), so sizing the
/// slot to the glyph would start each row's label at a different x.
fn row_status_icon_size(theme: &Theme) -> egui::Vec2 {
    egui::vec2(10.0, 14.0) * theme.ui_scale
}

const ATTENTION_HINT: &str = "needs attention";
const LOADER_FRAME: Duration = Duration::from_millis(120);

/// Painted (rather than `RichText("●")`) so its size is independent of font
/// metrics — `RichText("●")` renders inconsistently across fallback fonts.
fn attention_dot(ui: &mut egui::Ui, theme: &Theme) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
    let radius = 3.0 * theme.ui_scale;
    ui.painter().circle_filled(rect.center(), radius, theme.attention);
    resp
}

/// The loader is geometry rather than text so its three square dots stay the
/// same shape in status and action slots on every font stack.
fn three_square_loader_dots(rect: egui::Rect, missing: usize) -> [egui::Rect; 3] {
    let side = rect.width().min(rect.height());
    let canvas = egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(side));
    let gap = side / 6.0;
    let dot = (side - gap) / 2.0;
    let corners = [
        canvas.left_top(),
        egui::pos2(canvas.right() - dot, canvas.top()),
        egui::pos2(canvas.right() - dot, canvas.bottom() - dot),
        egui::pos2(canvas.left(), canvas.bottom() - dot),
    ];
    const VISIBLE: [[usize; 3]; 4] = [[1, 2, 3], [0, 2, 3], [0, 1, 3], [0, 1, 2]];
    VISIBLE[missing % VISIBLE.len()]
        .map(|index| egui::Rect::from_min_size(corners[index], egui::Vec2::splat(dot)))
}

fn paint_three_square_loader(ui: &mut egui::Ui, rect: egui::Rect, color: Color32) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.ctx().request_repaint_after(LOADER_FRAME);

    let missing = ui.input(|i| (i.time / LOADER_FRAME.as_secs_f64()) as usize % 4);
    for dot in three_square_loader_dots(rect, missing) {
        ui.painter().rect_filled(dot, 0.0, color);
    }
}

fn three_square_loader(ui: &mut egui::Ui, size: f32, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::ProgressIndicator));
    paint_three_square_loader(ui, rect, color);
    response
}

/// What the status slot draws for an agent's live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentMark {
    /// Live work animates.  A static glyph would have to blink to say as much.
    Loader,
    Glyph(BakedGlyph, Color32),
}

/// `quiet` is the row's own weight for an agent with nothing to report — dim
/// on a listed herdr row, ordinary text on a live session.  Blocked takes
/// `attention` wherever it appears, an active row's accent included: a state
/// that wants a human cannot also be quiet.
fn agent_mark(live: LiveState, quiet: Color32, attention: Color32) -> AgentMark {
    match live {
        LiveState::Idle => AgentMark::Glyph(DEFAULT_AGENT_ICON, quiet),
        LiveState::Working => AgentMark::Loader,
        LiveState::Blocked => AgentMark::Glyph(DEFAULT_BLOCKED_ICON, attention),
    }
}

/// What the status slot says on hover.  A named agent is named, so a
/// workspace running several can be told apart without opening any of them.
fn agent_hint(live: LiveState, name: Option<&str>) -> String {
    let doing = match live {
        LiveState::Idle => "is running",
        LiveState::Working => "is working",
        LiveState::Blocked => "is waiting for you",
    };
    format!("{} {doing}", name.unwrap_or("agent"))
}

/// Draw a harness's own state mark into an already-allocated slot.  A harness
/// that has stopped reporting leaves the slot empty rather than inventing a
/// state for it.
fn paint_harness_mark(
    ui: &mut egui::Ui,
    mark: Option<HarnessMark>,
    rect: egui::Rect,
    theme: &Theme,
) {
    let Some(mark) = mark else { return };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        mark.glyph,
        egui::FontId::new(10.0 * theme.ui_scale, crate::fonts::ui_variant_family(false, false)),
        theme.harness_state.of(mark.tone),
    );
}

/// Draw one agent mark into an already-allocated slot.
fn paint_agent_mark(ui: &mut egui::Ui, mark: AgentMark, rect: egui::Rect, theme: &Theme) {
    let s = theme.ui_scale;
    match mark {
        AgentMark::Loader => {
            let loader = egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(10.0 * s));
            paint_three_square_loader(ui, loader, theme.accent);
        },
        AgentMark::Glyph(glyph, color) => {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                glyph.as_str(),
                egui::FontId::proportional(10.0 * s),
                color,
            );
        },
    }
}

/// What a row knows about its own state, in the order the status slot ranks
/// it.  Grouped rather than passed loose because the three answer one
/// question between them, and the slot draws whichever ranks highest.
struct RowStatus<'a> {
    attention: bool,
    activity: SessionActivity,
    managed: Option<&'a Managed>,
}

/// Priority: attention dot > the harness's own state mark > the agent's live
/// state > active highlight > the configured color > the built-in default.
///
/// A harness outranks the live axis because it watches the pane from outside
/// and alacritree only reads its title, so where both have a reading the
/// harness's is the better one — and drawing it in the harness's vocabulary is
/// what keeps a pane looking the same listed and attached.
///
/// Returns what the slot has to say on hover, for the row to register with the
/// rest of its icons. The row icon proper reports nothing the row does not
/// already spell out, so it stays silent.
fn paint_row_status_icon(
    ui: &mut egui::Ui,
    theme: &Theme,
    status: RowStatus<'_>,
    style: &IconStyle,
    default_glyph: BakedGlyph,
    is_active: bool,
) -> Option<(egui::Rect, String)> {
    if status.attention {
        return Some((attention_dot(ui, theme).rect, ATTENTION_HINT.to_owned()));
    }
    // Centered into the fixed slot: laying a glyph out as text would size the
    // slot to its advance width and shift the label with it.
    let (rect, _) = ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
    if let Some(managed) = status.managed
        && let Some(mark) = managed.mark
    {
        paint_harness_mark(ui, Some(mark), rect, theme);
        return Some((rect, format!("{} says {}", managed.harness, mark.label)));
    }
    match status.activity {
        SessionActivity::Agent { name, live } => {
            let quiet = if is_active { theme.accent } else { theme.text };
            paint_agent_mark(ui, agent_mark(live, quiet, theme.attention), rect, theme);
            Some((rect, agent_hint(live, name)))
        },
        SessionActivity::Shell => {
            let (glyph, font, resolved) =
                resolve_icon(style, default_glyph, theme.text_muted, 10.0, 10.0, theme);
            let color = if is_active { theme.accent } else { resolved };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, font, color);
            None
        },
    }
}

/// Gap between adjacent action buttons. They already pad their own glyph, so
/// the default item spacing on top of that reads as a hole in the cluster.
/// Deliberately unscaled: the padding it supplements grows with `ui_scale`.
const ICON_CLUSTER_SPACING: f32 = 2.0;

/// Resolve an icon's paint-time glyph, font, and color from its config and
/// the site's built-in defaults.  `default_glyph` covers the case where a
/// table styles a key without setting `glyph`.  `default_px` and `slot_px`
/// are deliberately separate: an action button paints its glyph at `12.0 *
/// ui_scale` inside a `16.0 * ui_scale` slot, and conflating the two would
/// resize every unconfigured icon.
///
/// One resolver, not one paint helper: `RichText` participates in layout
/// while painter text draws into preallocated geometry, so each site keeps
/// its own drawing call.
fn resolve_icon<'a>(
    style: &'a IconStyle,
    default_glyph: BakedGlyph,
    default_color: Color32,
    default_px: f32,
    slot_px: f32,
    theme: &Theme,
) -> (&'a str, egui::FontId, Color32) {
    let size = style.size.unwrap_or(default_px).min(slot_px) * theme.ui_scale;
    let family = crate::fonts::ui_variant_family(style.bold, style.italic);
    (
        style.or_glyph(default_glyph.as_str()),
        egui::FontId::new(size, family),
        style.color.unwrap_or(default_color),
    )
}

/// A configurable icon in a 16×16 slot: the glyph, weight, slant, size and
/// colour come from config, with the built-in glyph as the fallback.
fn styled_icon_button(
    ui: &mut egui::Ui,
    style: &IconStyle,
    default_glyph: BakedGlyph,
    color: Color32,
    theme: &Theme,
) -> egui::Response {
    let s = theme.ui_scale;
    let (glyph, font, color) = resolve_icon(style, default_glyph, color, 12.0, 16.0, theme);
    let size = egui::vec2(16.0 * s, 16.0 * s);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let painted = if resp.hovered() {
        Color32::from_rgb(
            color.r().saturating_add(40),
            color.g().saturating_add(40),
            color.b().saturating_add(40),
        )
    } else {
        color
    };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, font, painted);
    resp
}

/// Destination index for moving the item at `from` so it lands before display
/// slot `insert_before` (counted in the pre-move list), or `None` for a no-op.
/// Removing `from` before inserting shifts every later slot down by one — the
/// off-by-one this isolates so it can be tested without an app.
fn move_target(len: usize, from: usize, insert_before: usize) -> Option<usize> {
    if from >= len {
        return None;
    }
    let mut to = insert_before.min(len);
    if to > from {
        to -= 1;
    }
    (to != from).then_some(to)
}

/// Position a session dropped before display slot `insert_before` should walk
/// to.  Inside its own workspace the session is removed before it is inserted,
/// so `move_target` compensates for the slots that shift down; coming from
/// another workspace it is inserted into a list it is not in yet, where the
/// display slot already is the position.
fn drop_position(
    same_workspace: bool,
    len: usize,
    from: usize,
    insert_before: usize,
) -> Option<usize> {
    if same_workspace { move_target(len, from, insert_before) } else { Some(insert_before) }
}

/// The weight and colour every reorder drop line is drawn with, shared so the
/// project and session drags cannot drift apart.
fn drop_indicator_stroke(theme: &Theme) -> Stroke {
    Stroke::new(2.0 * theme.ui_scale, theme.accent)
}

/// Paint the line a reorder drop would land on, at the row edge nearest the
/// pointer, and report whether that edge is the top — which is what "insert
/// before this row" means for both the project and the session drag.
fn draw_drop_indicator(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    pointer: egui::Pos2,
    theme: &Theme,
) -> bool {
    let before = pointer.y < row_rect.center().y;
    let y = if before { row_rect.top() } else { row_rect.bottom() };
    ui.painter().hline(row_rect.x_range(), y, drop_indicator_stroke(theme));
    before
}

/// The neighbour swaps that walk the element at `indices[j]` to slot
/// `position` of `indices`.
///
/// `indices` are the absolute positions one workspace occupies inside the
/// session vector, which are not contiguous: swapping only across them keeps
/// every other workspace's sessions at the index they were at.  Swapping is
/// also what avoids a `Clone` bound on `Session`, which owns a PTY.
fn walk_swaps(indices: &[usize], j: usize, position: usize) -> Vec<(usize, usize)> {
    let mut swaps = Vec::new();
    if indices.is_empty() || j >= indices.len() {
        return swaps;
    }
    let position = position.min(indices.len() - 1);
    let mut j = j;
    while j > position {
        swaps.push((indices[j - 1], indices[j]));
        j -= 1;
    }
    while j < position {
        swaps.push((indices[j], indices[j + 1]));
        j += 1;
    }
    swaps
}

/// The session a reorder key acts on.
///
/// A cursored session wins, then the workspace the cursor is resting on lends
/// its active session, and otherwise the session on screen moves.  The middle
/// case is what makes a held key work across a workspace boundary: a session
/// arriving alone in a workspace paints no row of its own, so the cursor
/// climbs to that workspace's row, and the next press must still find it.
///
/// `CloseSession` has the same first-and-last shape; `DeleteSelected` reads
/// the cursor whatever has focus, which is the wrong convention here — a key
/// pressed at the terminal should move the terminal you are looking at.
fn reorder_subject(
    sidebar_focused: bool,
    cursor: Option<&SidebarRow>,
    home_active: impl Fn() -> Option<SessionId>,
    worktree_active: impl Fn(&Path) -> Option<SessionId>,
    on_screen: impl Fn() -> Option<SessionId>,
) -> Option<SessionId> {
    if sidebar_focused {
        match cursor {
            Some(SidebarRow::Session(id)) => return Some(*id),
            Some(SidebarRow::Home) => {
                if let Some(id) = home_active() {
                    return Some(id);
                }
            },
            Some(SidebarRow::Worktree(path)) => {
                if let Some(id) = worktree_active(path) {
                    return Some(id);
                }
            },
            _ => {},
        }
    }
    on_screen()
}

/// A grip that a project row can be dragged by to reorder it.  Drag-sensing
/// only, so a plain click still falls through to the row's other controls.
fn drag_handle(ui: &mut egui::Ui, theme: &Theme) -> egui::Response {
    let s = theme.ui_scale;
    let size = egui::vec2(12.0 * s, 16.0 * s);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::drag());
    let color = if resp.hovered() || resp.dragged() { theme.text_dim } else { theme.text_muted };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "⠿",
        egui::FontId::proportional(12.0 * s),
        color,
    );
    resp.on_hover_cursor(egui::CursorIcon::Grab)
}

/// Lay out a row whose `trailing` widgets pin to the right edge while `leading`
/// fills the remaining width — so a `Label::truncate()` inside `leading` knows
/// exactly how much space it has and ellipsizes cleanly when the panel is narrow.
///
/// The row is pre-sized to `interact_size.y` (mirroring `Ui::horizontal`'s own
/// internals) so it doesn't claim the parent's full remaining height when nested
/// in a vertical layout — without this, `Align::Center` would push the row's
/// content to the middle of the column and leave a giant gap before the next row.
fn row_with_trailing<L, T>(ui: &mut egui::Ui, leading: L, trailing: T) -> egui::Rect
where
    L: FnOnce(&mut egui::Ui),
    T: FnOnce(&mut egui::Ui),
{
    let row_size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(row_size, egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let outer_spacing = ui.spacing().item_spacing.x;
        ui.spacing_mut().item_spacing.x = ICON_CLUSTER_SPACING;
        trailing(ui);
        // Restore before the leading group so only the icons cluster; the
        // labels next to them keep the panel's normal spacing.
        ui.spacing_mut().item_spacing.x = outer_spacing;
        let remaining = ui.available_width();
        if remaining <= 0.0 {
            return;
        }
        let row_h = ui.available_height();
        ui.allocate_ui_with_layout(
            egui::vec2(remaining, row_h),
            egui::Layout::left_to_right(egui::Align::Center),
            leading,
        );
    })
    .response
    .rect
}

/// Apply the configured sidebar scrollbar style to a panel's `Ui`.
///
/// `Solid` reserves a gutter right of the content instead of egui's floating
/// overlay, whose hover expansion covers the icons at the right end of the
/// rows.  Scoped to the panel so terminal-side scroll areas keep the default.
fn apply_scrollbar_style(ui: &mut egui::Ui, scrollbar: ScrollbarStyle) {
    if scrollbar == ScrollbarStyle::Solid {
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
    }
}

/// Keyboard-cursor indicator: an outline rather than a fill so it stays
/// legible on top of the active row's lightened background.
fn paint_cursor_outline(ui: &egui::Ui, rect: egui::Rect, theme: &Theme) {
    ui.painter().rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.0_f32, theme.accent),
        egui::StrokeKind::Inside,
    );
}

/// Outline the git row the keyboard cursor rests on, matched by section+path so
/// it survives the status refresh.  Full-width rect from the panel plus the
/// row's `y_range`, mirroring the project rows.
fn paint_git_row_cursor(
    ui: &egui::Ui,
    resp: &egui::Response,
    cursor: &Option<git_nav::GitRow>,
    section: GitSection,
    path: &str,
    scroll_into_view: bool,
    theme: &Theme,
) {
    if !matches!(cursor, Some(c) if c.section == section && c.path == path) {
        return;
    }
    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), resp.rect.y_range());
    paint_cursor_outline(ui, rect, theme);
    if scroll_into_view {
        ui.scroll_to_rect(rect, theme.scroll_align);
    }
}

/// One drained event's effect on a sidebar panel: either a filter outcome
/// (search/toggle) or a plain browsing nav key.
enum SidebarNavStep {
    Filter(panel_filter::Outcome),
    Nav(egui::Key),
    SearchAction(NamedAction),
}

/// Panel title plus its filter chrome, shared by both sidebars: the heading,
/// then `[s]`-style chips for each active toggle, then a bordered
/// `<icon> query▌` input box while searching (`search_icon` comes from
/// `[ui] search_icon`).  Renders only the title when the filter is idle.
fn panel_header_filter_ui(
    ui: &mut egui::Ui,
    title: &str,
    filter: &PanelFilter,
    search_icon: &IconStyle,
    theme: &Theme,
    toggles_apply: bool,
) {
    ui.label(RichText::new(title).color(theme.text).strong());
    let chip = if toggles_apply { theme.accent } else { theme.text_muted };
    for key in filter.active_toggles() {
        ui.label(RichText::new(format!("[{key}]")).color(chip).monospace().small());
    }
    if filter.mode() == panel_filter::Mode::Search || !filter.query().is_empty() {
        let s = theme.ui_scale;
        Frame::default()
            .stroke(Stroke::new(1.0_f32, theme.text_muted))
            .corner_radius((3.0 * s).round() as u8)
            .inner_margin(Margin::symmetric((4.0 * s).round() as i8, (1.0 * s).round() as i8))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 3.0 * s;
                // `TextStyle::Small`'s logical size is `font_normal`, unscaled —
                // `resolve_icon` multiplies by `ui_scale` internally, so dividing
                // it out here keeps the resolved size at `font_normal`. The slot
                // is generous (double that) since the icon sits in a frame that
                // grows with its content rather than a fixed-pixel button.
                let default_px = theme.font_normal / s;
                let (glyph, font, color) = resolve_icon(
                    search_icon,
                    DEFAULT_SEARCH_ICON,
                    theme.text_dim,
                    default_px,
                    default_px * 2.0,
                    theme,
                );
                ui.label(RichText::new(glyph).color(color).font(font));
                ui.label(
                    RichText::new(format!("{}▌", filter.query()))
                        .color(theme.text)
                        .monospace()
                        .small(),
                );
            });
    }
}

/// Which events are key presses whose text the search box will swallow.
///
/// egui-winit pushes `Event::Key` and then `Event::Text` adjacently for one
/// printable press, so adjacency identifies the pair.  The result is positional
/// rather than a set of triggers: key repeat and the `logical_key.or(physical_key)`
/// fallback both let two presses in one frame share a `(key, modifiers)`, and
/// only the occurrence carrying text may be treated as query input.
fn keys_paired_with_text(events: &[egui::Event]) -> Vec<bool> {
    events
        .iter()
        .enumerate()
        .map(|(n, ev)| {
            matches!(ev, egui::Event::Key { pressed: true, .. })
                && matches!(events.get(n + 1), Some(egui::Event::Text(_)))
        })
        .collect()
}

/// Decide one key event for a focused sidebar panel and record its step.
///
/// In search mode a key whose text the query already swallowed is consumed
/// outright — text input is unconditional, so it outranks even a search-scoped
/// binding on that letter.  Otherwise a search-scoped binding match (any
/// modifiers, so `Shift+Esc` counts) is dispatched through the binding table,
/// keeping `Enter`/`Esc` rebindable; an unmodified key drives the filter or
/// browsing nav; and a modified non-search key is retained for
/// `handle_shortcuts`.  Returns whether the event stays in the queue (`true`)
/// or is consumed here (`false`).
fn drain_search_or_nav(
    steps: &mut Vec<SidebarNavStep>,
    filter: &mut PanelFilter,
    bindings: &[crate::bindings::KeyBinding],
    key: egui::Key,
    modifiers: egui::Modifiers,
    produced_text: bool,
) -> bool {
    let searching = filter.mode() == panel_filter::Mode::Search;
    if searching && produced_text {
        return false;
    }
    if searching {
        let mut matched = false;
        for a in crate::bindings::all_matches(bindings, key, modifiers) {
            if let BindingAction::Named(n) = a {
                if n.is_search_scoped() {
                    steps.push(SidebarNavStep::SearchAction(*n));
                    matched = true;
                }
            }
        }
        if matched {
            return false;
        }
    }
    if !modifiers.is_none() {
        return true;
    }
    if let Some(outcome) = filter.on_key(key) {
        steps.push(SidebarNavStep::Filter(outcome));
        return false;
    }
    // Browsing consumes the whole nav-key set.  In search only Space and Delete
    // stay consumed as no-ops: Space preserves the fake-click guard on the
    // terminal view, and Delete is a text-editing key the append-only query has
    // nothing to do with, so it must not fall through to the cursored row.
    let consume = if filter.mode() == panel_filter::Mode::Browsing {
        is_sidebar_nav_key(key)
    } else {
        key == egui::Key::Space || key == egui::Key::Delete
    };
    if consume {
        steps.push(SidebarNavStep::Nav(key));
        return false;
    }
    true
}

fn is_sidebar_nav_key(key: egui::Key) -> bool {
    use egui::Key;
    matches!(
        key,
        Key::ArrowUp
            | Key::ArrowDown
            | Key::ArrowLeft
            | Key::ArrowRight
            | Key::Enter
            // egui synthesizes a click on the natively focused widget from
            // Space (like Enter); consuming it here stops keyboard clicks on
            // widgets the cursor model doesn't govern while the sidebar owns
            // focus.
            | Key::Space
            | Key::Escape
    )
}

struct HomeAction {
    activate: bool,
    spawn: bool,
    /// Full-width row rect, for a drop target to test the pointer against.
    rect: egui::Rect,
}

fn home_row(
    ui: &mut egui::Ui,
    is_active: bool,
    is_cursor: bool,
    scroll_into_view: bool,
    attention: bool,
    activity: SessionActivity,
    icons: &Icons,
    theme: &Theme,
) -> HomeAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut spawn_clicked = false;
    let mut spawn_rect: Option<egui::Rect> = None;
    let mut hints = IconHints::default();
    // The leading and trailing groups run as sibling closures, so the status
    // slot's hint travels out separately and joins the rest afterwards.
    let mut status_hint = None;
    let frame = Frame::default().inner_margin(Margin { left: 6, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            row_with_trailing(
                ui,
                |ui| {
                    status_hint = paint_row_status_icon(
                        ui,
                        theme,
                        RowStatus { attention, activity, managed: None },
                        &icons.home,
                        DEFAULT_HOME_ICON,
                        is_active,
                    );
                    ui.label(
                        RichText::new("Home")
                            .color(if is_active { theme.text } else { theme.text_dim })
                            .strong()
                            .small(),
                    );
                },
                |ui| {
                    let btn = styled_icon_button(
                        ui,
                        &icons.new_session,
                        DEFAULT_ADD_ICON,
                        theme.text_muted,
                        theme,
                    );
                    hints.add(btn.rect, "new shell");
                    spawn_rect = Some(btn.rect);
                    if btn.clicked() {
                        spawn_clicked = true;
                    }
                },
            );
        })
        .response
        .interact(egui::Sense::click());
    if let Some((rect, hint)) = status_hint {
        hints.add(rect, hint);
    }
    // The row carries no name tooltip of its own, so the icons' hints are the
    // only thing a hover here has to say.
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| resp);

    // Same z-order recovery as worktree_row: the retroactive frame interact
    // shadows the inner button, so route clicks inside its rect to spawn.
    if resp.clicked() && !spawn_clicked {
        if let (Some(rect), Some(pos)) = (spawn_rect, resp.interact_pointer_pos()) {
            if rect.contains(pos) {
                spawn_clicked = true;
            }
        }
    }

    let bg = if is_active {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        Color32::TRANSPARENT
    };
    if bg != Color32::TRANSPARENT {
        let rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
        ui.painter().set(bg_idx, egui::Shape::rect_filled(rect, 0.0, bg));
    }
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if scroll_into_view {
        ui.scroll_to_rect(full_rect, theme.scroll_align);
    }
    HomeAction { activate: resp.clicked() && !spawn_clicked, spawn: spawn_clicked, rect: full_rect }
}

struct WorktreeAction {
    activate: bool,
    delete: bool,
    spawn: bool,
    set_base: bool,
    /// Name of the profile picked from the row's "Open session" menu, if any.
    spawn_profile: Option<String>,
    /// Full-width row rect, for a drop target to test the pointer against.
    rect: egui::Rect,
}

/// Everything a sidebar session row needs, snapshotted before the panel
/// closure so rendering doesn't borrow `self.sessions`.
struct SessionRowData {
    id: SessionId,
    name: RowName,
    needs_attention: bool,
    activity: SessionActivity,
    /// This workspace's remembered active session (accent icon).
    is_active: bool,
    /// Active *and* the workspace is current — the session on screen
    /// (row background highlight).
    is_displayed: bool,
    /// Set while this session is attached to a harness-managed pane, so an
    /// attached agent's row still says where it lives and how to leave.
    managed: Option<Managed>,
}

/// One painted row under a workspace, in the order the sidebar draws them.
/// Attaching turns a herdr row into a session row in place, so the two travel
/// as one list rather than as two blocks that would reorder on attach.
enum WorkspaceRowData {
    Session(SessionRowData),
    Herdr(HerdrRowData),
}

impl WorkspaceRowData {
    /// Whether any of `rows` is a session of alacritree's own.  The workspace
    /// row shows aggregate attention and activity only while none is: with a
    /// list on screen, repeating its summary above it reads as noise.
    fn any_session(rows: &[Self]) -> bool {
        rows.iter().any(|row| matches!(row, Self::Session(_)))
    }
}

/// Everything a sidebar herdr-agent row needs, snapshotted before the panel
/// closure so rendering doesn't borrow `self.herdr_endpoints`.
struct HerdrRowData {
    side: herdr::Side,
    terminal_id: String,
    pane_id: String,
    name: RowName,
    managed: Managed,
}

/// The external supervisor a pane belongs to.  Named rather than flagged
/// because a second harness would otherwise add a parallel boolean to every
/// row, and because what a row must say — whose mark to paint, how to get
/// out, whether the attach is exclusive — varies by harness rather than by
/// row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Managed {
    /// What the tooltip calls it.
    harness: &'static str,
    /// The harness's own detach chord, already rendered.  `None` when its
    /// config could not be read or binds detach to nothing, both of which are
    /// reasons to stay quiet rather than name a chord the user may not have.
    detach: Option<String>,
    /// The attach shares the harness's whole view rather than one pane, so
    /// the row says so before a resize reveals it.
    shared_view: bool,
    /// The agent kind the harness detected, spelled the way it invokes it.
    kind: Option<String>,
    /// The pane's own title, when it says something the kind does not.
    title: Option<String>,
    /// How the harness draws the state it reports.  `None` when it is no
    /// longer reporting one — a pane alacritree still holds open after its
    /// harness stopped listing it.
    mark: Option<HarnessMark>,
}

impl HerdrRowData {
    fn from_agent(agent: &herdr::Agent, side: &herdr::Side, settings: &herdr::Settings) -> Self {
        // A listed row has nothing better than the terminal id's tail behind
        // the kind, so the kind takes the name rather than standing in front
        // of six characters nobody reads.
        let name = herdr_row_name(agent).unwrap_or_else(|| {
            RowName::plain(agent.kind.clone().unwrap_or_else(|| {
                let id = &agent.terminal_id;
                let skip = id.chars().count().saturating_sub(6);
                id.chars().skip(skip).collect()
            }))
        });
        Self {
            side: side.clone(),
            terminal_id: agent.terminal_id.clone(),
            pane_id: agent.pane_id.clone(),
            name,
            managed: Managed::herdr(side, settings, Some(agent)),
        }
    }
}

/// A row's name in two parts, ranked by weight rather than punctuation: the
/// identity, and the category standing in front of it as context.  `context`
/// is absent when the identity is already the category, so a row never spells
/// one thing twice.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowName {
    text: String,
    context: Option<String>,
}

impl RowName {
    fn plain(text: String) -> Self {
        Self { text, context: None }
    }
}

/// The name herdr reports for a pane, and `None` when it reports none.  The
/// kind rides along as context unless it says the same thing as the title.
/// What a titleless agent falls back to differs by row, so each caller says
/// so itself rather than passing its answer through here.
fn herdr_row_name(agent: &herdr::Agent) -> Option<RowName> {
    let title = agent.title.clone()?;
    let context = agent.kind.clone().filter(|kind| *kind != title);
    Some(RowName { text: title, context })
}

impl Managed {
    /// `agent` is herdr's current word on the pane, and `None` once it stops
    /// reporting one — a pane alacritree still holds open after its harness
    /// let go of it, which has a harness and a way out but no state or name.
    fn herdr(side: &herdr::Side, settings: &herdr::Settings, agent: Option<&herdr::Agent>) -> Self {
        let kind = agent.and_then(|a| a.kind.clone());
        let title =
            agent.and_then(|a| a.title.clone()).filter(|t| Some(t.as_str()) != kind.as_deref());
        Self {
            harness: "herdr",
            detach: settings.detach.clone(),
            shared_view: !herdr::can_attach(side),
            mark: agent.map(|a| herdr_mark(a.status, settings.indicators)),
            kind,
            title,
        }
    }

    /// What the harness calls this pane: the agent kind backquoted as the
    /// command it is, and the title quoted as the words it is.
    fn pane_name(&self) -> Option<String> {
        match (&self.kind, &self.title) {
            (Some(kind), Some(title)) => Some(format!("`{kind}` \"{title}\"")),
            (Some(kind), None) => Some(format!("`{kind}`")),
            (None, Some(title)) => Some(format!("\"{title}\"")),
            (None, None) => None,
        }
    }
}

/// The mark a harness paints for the state it reports, in that harness's own
/// vocabulary.  Resolved once per row, so a pane reads the same whether it is
/// listed or attached — the two are drawn by different painters, and attaching
/// must not repaint a pane in a language it does not speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HarnessMark {
    glyph: &'static str,
    tone: StateTone,
    /// The harness's own word for this state, for the hover text.
    label: &'static str,
}

/// What a harness means by a state's color.  Named rather than carried as a
/// `Color32` so the palette stays alacritree's and a row snapshot stays free
/// of the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateTone {
    Blocked,
    Working,
    Done,
    Idle,
    /// The harness reported something alacritree does not recognise.
    Unclear,
}

/// herdr's state vocabulary, taken from its own `state_icon_symbol` and
/// `state_label_color`.  Which of the two sets applies is herdr's `[ui]
/// status_indicators`, so a user who picked one in herdr gets it here too.
///
/// `done` is `idle` on herdr's internal axis and a status of its own over its
/// API, which is the axis alacritree reads — so the two arrive already
/// distinguished, without the "has a human looked at it yet" bit herdr tracks
/// to tell them apart.
fn herdr_mark(status: herdr::Status, indicators: herdr::Indicators) -> HarnessMark {
    use herdr::Indicators::{Dots, Symbols};
    use herdr::Status::{Blocked, Done, Idle, Unknown, Working};
    let (glyph, tone) = match (indicators, status) {
        (_, Idle) => ("○", StateTone::Idle),
        (_, Unknown) => ("·", StateTone::Unclear),
        (Dots, Blocked) => ("●", StateTone::Blocked),
        (Dots, Working) => ("●", StateTone::Working),
        (Dots, Done) => ("●", StateTone::Done),
        (Symbols, Blocked) => ("×", StateTone::Blocked),
        (Symbols, Working) => ("◐", StateTone::Working),
        (Symbols, Done) => ("✓", StateTone::Done),
    };
    HarnessMark { glyph, tone, label: status.label() }
}

/// Spawn-ordered ids of the sessions in `ws`, or empty below the list
/// threshold.  The threshold is normally two — a single-session workspace row
/// keeps its compact form, mirroring the tab strip — and `always` lowers it
/// to one.
///
/// herdr rows are never held back by it: a pane alacritree does not own has
/// no other surface to appear on, and hiding it would hide the workspace's
/// only row.  They do count toward the threshold, so a lone shell session
/// beside one is listed rather than folded into the workspace row, which
/// would leave a hole in a list its neighbours are already in.
///
/// `managed` carries herdr's own position for each of its rows, and sorts by
/// it here so an attached session and a listed agent interleave the way herdr
/// has them rather than by which kind of row they are.  The sort is stable,
/// so panes herdr no longer lists keep the order they were spawned in.
fn workspace_entries(
    shells: &[SessionId],
    managed: Vec<(usize, sidebar_nav::WorkspaceEntry)>,
    always: bool,
) -> Vec<sidebar_nav::WorkspaceEntry> {
    let threshold = if always { 1 } else { 2 };
    let mut managed = managed;
    managed.sort_by_key(|(at, _)| *at);
    let mut entries = Vec::with_capacity(shells.len() + managed.len());
    if shells.len() + managed.len() >= threshold {
        entries.extend(shells.iter().copied().map(sidebar_nav::WorkspaceEntry::Session));
    }
    entries.extend(managed.into_iter().map(|(_, entry)| entry));
    entries
}

/// Step the lockstep index over the rows a skipped worktree owns.
///
/// The projection is built before the deletion is known, so it still lists
/// the worktree with everything under it.  Leaving the index parked on a row
/// no node will match again would mark every later node unprojected, and the
/// cursor repair reads an unprojected row as one that has gone away.
fn skip_projected_rows(
    rows: &[SidebarRow],
    next_row: &mut usize,
    listed: &sidebar_nav::ListedRows,
    path: &Path,
) {
    if rows.get(*next_row) != Some(&SidebarRow::Worktree(path.to_path_buf())) {
        return;
    }
    *next_row += 1;
    for entry in listed.get(&Some(path.to_path_buf())).map_or(&[][..], Vec::as_slice) {
        if rows.get(*next_row) != Some(&entry.row()) {
            break;
        }
        *next_row += 1;
    }
}

/// Assemble the model arena and the projection.  `rows` is the projection —
/// exactly what the cursor steps over — and `live` is the model: every running
/// session, whatever the listing threshold or the filter says.  Building
/// membership from `listed` instead would make the last session in a workspace
/// read as deleted the moment its sibling closed.
///
/// `listed` is the listing the projection was built from.  A herdr row exists
/// only while its agent is listed, so there is no wider model to take it from,
/// and reading a second listing here could disagree with `rows`.
///
/// `skip_worktree` drops a worktree whose deletion is already committed but
/// whose git operation has not finished, so nothing lands the cursor — or a
/// new shell — inside a directory on its way out.
///
/// Nodes are pushed in exactly the order `sidebar_nav::visible_rows` emits,
/// with unprojected nodes interleaved, so one forward index into `rows`
/// classifies every node.  Asking `rows.contains` per node instead would be
/// quadratic in path comparisons on a path that runs whenever the user types.
fn build_sidebar_snapshot(
    projects: &[Project],
    live: &[(WorkspaceKey, SessionId)],
    listed: &sidebar_nav::ListedRows,
    rows: &[SidebarRow],
    skip_worktree: Option<&Path>,
    inputs: sidebar_focus::ObservedInputs,
) -> sidebar_focus::TreeSnapshot {
    use sidebar_focus::Parent;
    use sidebar_nav::WorkspaceEntry;

    let mut b = sidebar_focus::SnapshotBuilder::default();
    let mut next_row = 0usize;
    let mut placed = vec![false; live.len()];

    // Consume `rows` in lockstep: a node is projected exactly when it is the
    // row the projection expects next.
    let push = |b: &mut sidebar_focus::SnapshotBuilder,
                next_row: &mut usize,
                row: SidebarRow,
                parent: Parent| {
        let projected = rows.get(*next_row) == Some(&row);
        if projected {
            *next_row += 1;
        }
        b.push(row, parent, projected)
    };
    let push_workspace = |b: &mut sidebar_focus::SnapshotBuilder,
                          next_row: &mut usize,
                          placed: &mut [bool],
                          ws: &WorkspaceKey,
                          parent: Parent| {
        let entries = listed.get(ws).map_or(&[][..], Vec::as_slice);
        for entry in entries {
            push(b, next_row, entry.row(), parent);
        }
        // A workspace lists every shell session it has or none of them, and a
        // session attached to a herdr pane is always listed, so a session
        // reaching the second arm here belongs to a workspace that listed
        // nothing at all.  It is running, so the model keeps it; it is drawn
        // nowhere, so the projection does not.
        for (i, (w, id)) in live.iter().enumerate() {
            if w != ws {
                continue;
            }
            placed[i] = true;
            if !entries.contains(&WorkspaceEntry::Session(*id)) {
                b.push(SidebarRow::Session(*id), parent, false);
            }
        }
    };

    let home_id = push(&mut b, &mut next_row, SidebarRow::Home, Parent::Root);
    push_workspace(&mut b, &mut next_row, &mut placed, &None, Parent::Node(home_id));

    for p in projects {
        let project_id =
            push(&mut b, &mut next_row, SidebarRow::Project(p.root.clone()), Parent::Root);
        for wt in &p.worktrees {
            if skip_worktree == Some(wt.path.as_path()) {
                skip_projected_rows(rows, &mut next_row, listed, &wt.path);
                continue;
            }
            let wt_id = push(
                &mut b,
                &mut next_row,
                SidebarRow::Worktree(wt.path.clone()),
                Parent::Node(project_id),
            );
            let ws = Some(wt.path.clone());
            push_workspace(&mut b, &mut next_row, &mut placed, &ws, Parent::Node(wt_id));
        }
    }

    // Sessions whose workspace has no row left — a removed project, or a
    // worktree already treated as gone.  They are running, so they belong in
    // the model; they have no place in the tree, so they are nobody's sibling.
    for (i, (_, id)) in live.iter().enumerate() {
        if !placed[i] {
            b.push(SidebarRow::Session(*id), Parent::Detached, false);
        }
    }

    debug_assert_eq!(next_row, rows.len(), "every projected row must be in the arena");
    b.finish(inputs)
}

/// The cursor, workspace, and active session the reconciler last wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarFocusWrite {
    cursor: Option<SidebarRow>,
    workspace: WorkspaceKey,
    active: Option<SessionId>,
}

/// Whether focus moved behind the reconciler's back.  The active session is
/// part of the comparison because the tab and session cycling actions can
/// switch sessions without leaving the workspace, changing nothing else.
/// Comparing the resulting state rather than matching on action names covers
/// every route to them — rebound keys, the command palette, MCP — at the price
/// of `ensure_active_session` and `adopt_active_session` marking their own
/// writes so their self-healing does not read as navigation.
fn sidebar_focus_overtaken(
    written: &Option<SidebarFocusWrite>,
    cursor: Option<&SidebarRow>,
    workspace: &WorkspaceKey,
    active: Option<SessionId>,
) -> bool {
    match written {
        None => false,
        Some(w) => w.cursor.as_ref() != cursor || w.workspace != *workspace || w.active != active,
    }
}

/// Where the view goes after a session's removal.
#[derive(Debug, PartialEq)]
enum CloseFallback {
    /// Removal didn't empty the on-screen workspace — no navigation.
    Stay,
    /// Switch to the project's main checkout, which still has a session.
    Activate(PathBuf),
    /// A session in another workspace, chosen by `ring_landing`.
    ActivateSession(SessionId),
    /// Switch to home; `activate_home` spawns a shell there if none exists.
    Home,
}

/// Why a session record is going away.  The distinction exists because
/// neither half of a close, the respawn policy or the navigation, may apply
/// to a session that never got a PTY.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CloseReason {
    User,
    SpawnFailed,
}

/// The verdict a close acts on.  A failed open stays put whatever the
/// workspace's state says: every destination `close_fallback` can name is one
/// `ensure_active_session` will spawn into, and that open fails the same way.
/// Staying leaves the pane on the "no session" placeholder, which is what the
/// workspace honestly holds.
fn close_navigation(reason: CloseReason, verdict: CloseFallback) -> CloseFallback {
    match reason {
        CloseReason::User => verdict,
        CloseReason::SpawnFailed => CloseFallback::Stay,
    }
}

/// Which session a workspace switches to when the one at `removed_idx` is
/// closed.  `sessions` is the list *after* removal; `removed_idx` indexes the
/// list *before* it, so the first surviving sibling at or past it is the
/// closed session's successor.  Pure over (workspace, id) pairs for the same
/// reason as `close_fallback`.
///
/// `Preserve` hands the workspace its first session whichever one closed.
/// `Follow` takes the successor, or the predecessor when the last session
/// closed — the ordinal rule `sidebar_focus::slide` lands the cursor by, so a
/// close that moves both cannot point them at different siblings.
fn close_landing(
    sessions: &[(WorkspaceKey, SessionId)],
    workspace: &WorkspaceKey,
    removed_idx: usize,
    mode: SidebarFocus,
) -> Option<SessionId> {
    let mut siblings = sessions
        .iter()
        .enumerate()
        .filter(|(_, (w, _))| w == workspace)
        .map(|(i, (_, id))| (i, *id));
    if !mode.follows() {
        return siblings.next().map(|(_, id)| id);
    }
    let mut predecessor = None;
    for (i, id) in siblings {
        if i >= removed_idx {
            return Some(id);
        }
        predecessor = Some(id);
    }
    predecessor
}

/// Post-close navigation for the workspace that just lost a session.
/// `remaining` is the session list after removal; `main_checkout` is the
/// removed workspace's project main (None when the workspace *is* the main,
/// is home, or belongs to no known project). Pure over (workspace, id)
/// pairs for the same reason the sidebar listing does: the rule stays
/// testable without spawning PTYs.
fn close_fallback(
    removed_ws: &WorkspaceKey,
    current_ws: &WorkspaceKey,
    remaining: &[(WorkspaceKey, SessionId)],
    main_checkout: Option<PathBuf>,
) -> CloseFallback {
    if removed_ws != current_ws || remaining.iter().any(|(w, _)| w == removed_ws) {
        return CloseFallback::Stay;
    }
    match main_checkout {
        Some(main) if remaining.iter().any(|(w, _)| w.as_deref() == Some(main.as_path())) => {
            CloseFallback::Activate(main)
        },
        _ => CloseFallback::Home,
    }
}

/// One session's place in the flat ring: workspaces in sidebar order, each
/// workspace's sessions in spawn order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RingEntry {
    /// The owning project's root, from `project_of`.  None for home.
    project: Option<PathBuf>,
    workspace: WorkspaceKey,
    id: SessionId,
}

/// The session a removal lands on under the `ring_*` policies.  `ring` is the
/// flat session ring captured before the removal and `removed` is what left
/// it: one session for a close, a worktree's whole list for a delete.
/// Successor first, the earliest survivor past the last removed entry, else
/// the latest survivor before the first.
///
/// `prefer` is the removed workspace's owning project under `ring_project`,
/// and None under `ring_global` and for home.  When set, the search runs over
/// that project's entries before running over the whole ring.
///
/// A path two projects both list appears in the ring twice.  Both entries
/// carry the same `project_of` tag and name the same session, so a duplicate
/// changes no answer; indices are taken by first occurrence, the way
/// `session_ring_target` takes them.
fn ring_landing(
    ring: &[RingEntry],
    removed: &[SessionId],
    prefer: Option<&Path>,
) -> Option<(WorkspaceKey, SessionId)> {
    let positions: Vec<usize> =
        removed.iter().filter_map(|id| ring.iter().position(|e| e.id == *id)).collect();
    let first = *positions.iter().min()?;
    let last = *positions.iter().max()?;

    let search = |group: Option<&Path>| {
        let in_group = |e: &RingEntry| match group {
            Some(root) => e.project.as_deref() == Some(root),
            None => true,
        };
        let survives = |e: &RingEntry| !removed.contains(&e.id);
        ring[last + 1..]
            .iter()
            .find(|e| in_group(e) && survives(e))
            .or_else(|| ring[..first].iter().rev().find(|e| in_group(e) && survives(e)))
            .map(|e| (e.workspace.clone(), e.id))
    };

    prefer.and_then(|root| search(Some(root))).or_else(|| search(None))
}

/// A close-fallback verdict the reconciler owes the terminal, and the worktree
/// whose rows must already read as gone.  The verdict is carried rather than
/// recomputed because only `close_fallback` knows the difference between
/// staying put, hopping to the project's main checkout, and going home.
#[derive(Debug)]
struct DeferredClose {
    verdict: CloseFallback,
    /// Set when an asynchronous worktree deletion is in flight: `projects`
    /// still lists it, so without this the reconciler would see an intact row
    /// and could spawn a shell inside the directory being removed.  It pairs
    /// with any verdict, including a ring landing in another project.
    removed_worktree: Option<PathBuf>,
}

/// Whether the reconciler owns post-removal navigation.  Under `"follow"` the
/// landing row decides where the terminal goes, so acting here first would
/// show one workspace for a frame and another the next.
fn defers_close_navigation(mode: SidebarFocus) -> bool {
    mode.follows()
}

/// What re-homing a session does to the active-session maps and the view.
/// Pure over the same kind of snapshot `close_fallback` takes, so the policy
/// is testable without spawning PTYs.
#[derive(Debug, PartialEq, Eq)]
enum SourceRepair {
    Keep,
    Set(SessionId),
    Remove,
}

#[derive(Debug, PartialEq, Eq)]
struct MoveOutcome {
    source: SourceRepair,
    /// The moved session becomes the target workspace's active session.
    claim_target: bool,
    /// Switch the view to the target — the user was watching this session.
    follow: bool,
}

fn plan_move(
    was_source_active: bool,
    on_screen: bool,
    next_in_source: Option<SessionId>,
    target_has_active: bool,
) -> MoveOutcome {
    let source = match (was_source_active, next_in_source) {
        (false, _) => SourceRepair::Keep,
        (true, Some(id)) => SourceRepair::Set(id),
        (true, None) => SourceRepair::Remove,
    };
    MoveOutcome { source, claim_target: on_screen || !target_has_active, follow: on_screen }
}

/// The owning project's main checkout for `ws`, or None when `ws` already
/// is the main (including non-git roots, whose single pseudo-worktree is
/// its own main) or belongs to no known project.
fn project_main_for(projects: &[Project], ws: &Path) -> Option<PathBuf> {
    let root = sidebar_nav::project_of(projects, &Some(ws.to_path_buf()))?;
    let project = projects.iter().find(|p| p.root == root)?;
    let main = project.worktrees.iter().find(|w| w.is_main)?;
    if main.path == ws { None } else { Some(main.path.clone()) }
}

/// The project to expand so `row` still renders once search exits, if any.
/// Only child rows qualify: search lists matched worktrees and sessions whatever
/// their project's `expanded` flag says, so they vanish when the query clears.
/// A header is already its own row, and expanding it would turn selecting a
/// project into a toggle.
fn search_reveal_root(
    projects: &[Project],
    session_workspace: impl Fn(SessionId) -> Option<WorkspaceKey>,
    row: &SidebarRow,
) -> Option<PathBuf> {
    if !matches!(row, SidebarRow::Worktree(_) | SidebarRow::Session(_)) {
        return None;
    }
    row_project_root(projects, session_workspace, row)
}

/// The root of the project owning `row`: a worktree resolves by its path, a
/// session through its workspace.  `None` for Home or a row outside every
/// known project.  Lets `ToggleProjectExpanded` act on the whole subtree, not
/// just the header.
fn row_project_root(
    projects: &[Project],
    session_workspace: impl Fn(SessionId) -> Option<WorkspaceKey>,
    row: &SidebarRow,
) -> Option<PathBuf> {
    let workspace = match row {
        SidebarRow::Project(root) => return Some(root.clone()),
        SidebarRow::Worktree(path) => path.clone(),
        SidebarRow::Session(id) => session_workspace(*id).flatten()?,
        SidebarRow::Home => return None,
        // Carries a (Side, terminal id) pair, not a workspace or a SessionId,
        // so unlike a session row there is nothing here to resolve against.
        SidebarRow::HerdrAgent(..) => return None,
    };
    projects
        .iter()
        .find(|p| p.worktrees.iter().any(|w| w.path == workspace))
        .map(|p| p.root.clone())
}

/// The branch the git panel diffs against: the user's explicit override,
/// else the open PR's base (what GitHub will review), else the project's
/// detected default branch.
fn effective_base_branch(
    override_branch: Option<&str>,
    pr_base: Option<&str>,
    project_default: Option<&str>,
) -> Option<String> {
    override_branch.or(pr_base).or(project_default).map(str::to_string)
}

/// The worktree a SetBaseBranch press targets: the sidebar cursor's worktree
/// while the projects sidebar owns focus (a session row resolves to its
/// workspace), otherwise the current workspace.  Home and project-header
/// cursors, and the home workspace, have no base branch to override.
fn base_branch_target(
    sidebar_focused: bool,
    cursor: Option<&SidebarRow>,
    session_workspace: impl Fn(SessionId) -> Option<WorkspaceKey>,
    current: &WorkspaceKey,
) -> Option<PathBuf> {
    if sidebar_focused {
        return match cursor {
            Some(SidebarRow::Worktree(p)) => Some(p.clone()),
            Some(SidebarRow::Session(id)) => session_workspace(*id).flatten(),
            _ => None,
        };
    }
    current.clone()
}

/// The session a SelectNextSession/SelectPreviousSession press lands on:
/// one flat ring over every open session, workspaces in sidebar order and
/// each workspace's sessions in the order its rows are drawn.  `None` means stay put — a
/// ring too small to cycle, or an active session missing from the ring
/// (its worktree turned prunable).  With no active session (an emptied
/// workspace on screen) the first entry re-anchors the cycle.
fn session_ring_target(
    ring: &[(WorkspaceKey, SessionId)],
    current: Option<SessionId>,
    delta: i32,
) -> Option<(WorkspaceKey, SessionId)> {
    if ring.len() < 2 {
        return None;
    }
    let Some(current) = current else {
        return Some(ring[0].clone());
    };
    let pos = ring.iter().position(|(_, id)| *id == current)?;
    let next = (pos as i32 + delta).rem_euclid(ring.len() as i32) as usize;
    Some(ring[next].clone())
}

/// Branches whose name contains `query`, case-insensitively.
fn filter_branches(branches: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    branches.iter().filter(|b| b.to_lowercase().contains(&query)).cloned().collect()
}

/// Where the picker cursor lands after this frame's filter changes.  Row 0 is
/// always Auto, so reseeding a query edit to 0 would apply Auto on the primary
/// "type a branch name, press Enter" flow.  A non-empty query instead seeds
/// the first branch row (1), clamped to 0 when nothing matches; an empty
/// query seeds Auto.  With no query change, the previous cursor is kept,
/// clamped to the (possibly shrunk) filtered length.
fn picker_cursor(
    query_changed: bool,
    query_empty: bool,
    prev: usize,
    filtered_len: usize,
) -> usize {
    if query_changed {
        if query_empty { 0 } else { 1.min(filtered_len) }
    } else {
        prev.min(filtered_len)
    }
}

/// The activity a session's row paints.  A session attached to a herdr agent
/// takes herdr's word, because herdr watches the pane from outside and sees an
/// approval dialog no title heuristic can reach.
///
/// `unknown` is herdr declining to say, so the session's own reading stands.
/// The gate closes either way: an attached pane holds an agent whether or not
/// the process probe recognized one.
fn herdr_backed_activity(own: SessionActivity, status: Option<herdr::Status>) -> SessionActivity {
    let Some(status) = status else { return own };
    let live = LiveState::from_herdr(status).unwrap_or(own.live().unwrap_or_default());
    own.with_live(live)
}

/// Agent titles commonly lead with their own decorative mark. Once the row
/// paints a semantic agent/loader status, retaining that mark beside it would
/// reintroduce the vendor-specific icon set this status model replaces.
/// What an attached session's row is called.  On Linux and WSL an attach is
/// full passthrough, so the pane on screen is herdr's and the row names it
/// the way herdr's own listed row would — attaching must not rename the row
/// under the user.  A pane herdr reports no title for keeps the title its own
/// PTY set, with the kind in front of it.
fn session_row_name(
    pty_title: &str,
    activity: SessionActivity,
    agent: Option<&herdr::Agent>,
) -> RowName {
    let Some(agent) = agent else {
        return RowName::plain(session_row_title(pty_title, activity));
    };
    herdr_row_name(agent).unwrap_or_else(|| RowName {
        text: session_row_title(pty_title, activity),
        context: agent.kind.clone(),
    })
}

fn session_row_title(title: &str, activity: SessionActivity) -> String {
    if activity.is_agent() {
        let trimmed = title.trim_start();
        if let Some(first) = trimmed.chars().next() {
            let rest = &trimmed[first.len_utf8()..];
            if !first.is_ascii() && rest.chars().next().is_some_and(char::is_whitespace) {
                let rest = rest.trim_start();
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
    }
    title.to_string()
}

/// Sidebar placeholder for a worktree whose creation the user minimized: a
/// spinner stands in until `poll_pending_creates` refreshes the project and the
/// real worktree row takes its place.  Indentation and the leading glyph match
/// `worktree_row` so it lines up with its future sibling.
fn creating_row(ui: &mut egui::Ui, branch: &str, icons: &Icons, theme: &Theme) {
    let s = theme.ui_scale;
    let frame = Frame::default().inner_margin(Margin { left: 16, right: 0, top: 3, bottom: 3 });
    frame.show(ui, |ui| {
        row_with_trailing(
            ui,
            |ui| {
                let (glyph, font, color) = resolve_icon(
                    &icons.worktree,
                    DEFAULT_WORKTREE_ICON,
                    theme.text_muted,
                    10.0,
                    10.0,
                    theme,
                );
                ui.label(RichText::new(glyph).color(color).font(font));
                let (resp, galley) = truncating_label(
                    ui,
                    RichText::new(branch).color(theme.text_muted).small(),
                    theme.text_muted,
                    egui::Sense::hover(),
                );
                let _ = name_tooltip(resp, branch, galley.elided, theme.sidebar_tooltips);
            },
            |ui| {
                three_square_loader(ui, 12.0 * s, theme.accent);
            },
        );
    });
}

/// Badge glyph, color, and tooltip word for a PR state.
fn pr_badge<'a>(
    icons: &'a Icons,
    theme: &Theme,
    state: PrState,
) -> (&'a IconStyle, BakedGlyph, Color32, &'static str) {
    match state {
        PrState::Open => (&icons.pr_open, DEFAULT_PR_OPEN_ICON, theme.pr_open, "open"),
        PrState::Draft => (&icons.pr_draft, DEFAULT_PR_DRAFT_ICON, theme.pr_draft, "draft"),
        PrState::Merged => (&icons.pr_merged, DEFAULT_PR_MERGED_ICON, theme.pr_merged, "merged"),
        PrState::Closed => (&icons.pr_closed, DEFAULT_PR_CLOSED_ICON, theme.pr_closed, "closed"),
    }
}

/// Badge style, color, and tooltip for an upstream state.  The tooltip names
/// the upstream ref because the glyph cannot.
fn upstream_badge<'a>(
    icons: &'a Icons,
    theme: &Theme,
    state: &UpstreamState,
) -> (&'a IconStyle, BakedGlyph, Color32, String) {
    match state {
        UpstreamState::Level { upstream } => (
            &icons.upstream_level,
            DEFAULT_UPSTREAM_LEVEL_ICON,
            theme.upstream_level,
            format!("tracks {upstream}"),
        ),
        UpstreamState::Diverged { upstream, ahead, behind } => (
            &icons.upstream_diverged,
            DEFAULT_UPSTREAM_DIVERGED_ICON,
            theme.upstream_diverged,
            format!("tracks {upstream} — {ahead} ahead, {behind} behind"),
        ),
        UpstreamState::Gone { upstream } => (
            &icons.upstream_gone,
            DEFAULT_UPSTREAM_GONE_ICON,
            theme.upstream_gone,
            format!("{upstream} is missing locally"),
        ),
        UpstreamState::Untracked => (
            &icons.upstream_untracked,
            DEFAULT_UPSTREAM_UNTRACKED_ICON,
            theme.upstream_untracked,
            "no upstream configured".to_string(),
        ),
    }
}

/// Width the worktree row's context menu is held to, so a long profile name
/// wraps onto a second line instead of stretching the popup to fit it.
const WORKTREE_MENU_MAX_WIDTH: f32 = 220.0;

/// The "index. name" label for a profile entry in the worktree row's "Open
/// session" menu, 1-based to match `SpawnProfile1`..`SpawnProfile9` in the
/// palette.
fn profile_menu_label(index: usize, name: &str) -> String {
    format!("{index}. {name}")
}

fn worktree_row(
    ui: &mut egui::Ui,
    wt: &Worktree,
    // What the liveness probe has seen since discovery ran, if anything.
    // `Some` overrides `wt.prunable` in both directions; `None` leaves it
    // standing.  Kept out of the flag itself because that also picks between
    // `git worktree remove` and a prune, and a probe must never decide that.
    missing: Option<bool>,
    display_name: &str,
    pr: Option<&PrInfo>,
    is_active: bool,
    is_cursor: bool,
    scroll_into_view: bool,
    attention: bool,
    activity: SessionActivity,
    deleting: bool,
    // Shell profiles offered in the row's "Open session" menu: `.0` is the
    // profile name (spawned and shown as the button label), `.1` is the
    // command shown on hover.
    profiles: &[(String, String)],
    icons: &Icons,
    theme: &Theme,
) -> WorktreeAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut delete_clicked = false;
    let mut hints = IconHints::default();
    let mut delete_rect: Option<egui::Rect> = None;
    let mut spawn_clicked = false;
    let mut spawn_rect: Option<egui::Rect> = None;
    let mut name_elided = false;
    // The leading and trailing groups run as sibling closures, so the status
    // slot's hint travels out separately and joins the rest afterwards.
    let mut status_hint = None;
    // Discovery's word, corrected by whatever the probe has seen since.  The
    // main worktree is never offered for pruning, so it never greys either.
    let prunable = worktree_looks_gone(wt, missing);
    // right: 0 keeps the worktree `×` at the same x as the project row's `×`,
    // which has no frame margin and sits flush against the panel's outer padding.
    let frame = Frame::default().inner_margin(Margin { left: 16, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            let (default_icon, default_glyph) = if wt.is_main {
                (&icons.worktree_main, DEFAULT_WORKTREE_MAIN_ICON)
            } else {
                (&icons.worktree, DEFAULT_WORKTREE_ICON)
            };
            let name_color = if prunable || deleting {
                theme.text_muted
            } else if is_active {
                theme.text
            } else {
                theme.text_dim
            };
            row_with_trailing(
                ui,
                |ui| {
                    status_hint = paint_row_status_icon(
                        ui,
                        theme,
                        RowStatus { attention, activity, managed: None },
                        default_icon,
                        default_glyph,
                        is_active,
                    );
                    let (_, galley) = truncating_label(
                        ui,
                        RichText::new(display_name).small().color(name_color),
                        name_color,
                        egui::Sense::hover(),
                    );
                    name_elided = galley.elided;
                },
                |ui| {
                    // Mid-removal the row is inert: swap its controls for a
                    // spinner so the user sees the delete is in flight.
                    if deleting {
                        three_square_loader(ui, 12.0 * theme.ui_scale, theme.accent);
                        return;
                    }
                    if !wt.is_main {
                        let hover =
                            if prunable { "prune worktree" } else { "delete worktree and branch" };
                        let btn = styled_icon_button(
                            ui,
                            &icons.delete_worktree,
                            DEFAULT_CLOSE_ICON,
                            theme.text_muted,
                            theme,
                        );
                        hints.add(btn.rect, hover);
                        delete_rect = Some(btn.rect);
                        if btn.clicked() {
                            delete_clicked = true;
                        }
                    }
                    let btn = styled_icon_button(
                        ui,
                        &icons.new_session,
                        DEFAULT_ADD_ICON,
                        theme.text_muted,
                        theme,
                    );
                    hints.add(btn.rect, "new shell");
                    spawn_rect = Some(btn.rect);
                    if btn.clicked() {
                        spawn_clicked = true;
                    }
                    if let Some(info) = pr {
                        let (style, default_glyph, color, word) =
                            pr_badge(icons, theme, info.state);
                        let (glyph, font, color) =
                            resolve_icon(style, default_glyph, color, 10.0, 10.0, theme);
                        let (rect, _) = ui
                            .allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            glyph,
                            font,
                            color,
                        );
                        hints.add(rect, format!("PR #{} — {word}", info.number));
                    }
                    if let Some(state) = wt.upstream.as_ref() {
                        let (style, default_glyph, color, tip) =
                            upstream_badge(icons, theme, state);
                        let (glyph, font, color) =
                            resolve_icon(style, default_glyph, color, 10.0, 10.0, theme);
                        let (rect, _) = ui
                            .allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            glyph,
                            font,
                            color,
                        );
                        hints.add(rect, tip);
                    }
                },
            );
        })
        .response
        .interact(egui::Sense::click());
    if let Some((rect, hint)) = status_hint {
        hints.add(rect, hint);
    }
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| {
        if prunable {
            resp.on_hover_text("worktree directory is missing — × prunes it")
        } else {
            name_tooltip(resp, display_name, name_elided, theme.sidebar_tooltips)
        }
    });

    // Frame allocates its space at end-of-show, so its retroactive `interact`
    // registers *after* the inner button in egui's z-order — meaning clicks on
    // the × land on this row response, not the button.  Recover by routing
    // clicks whose position falls inside the button rect to delete.
    if resp.clicked() && !delete_clicked && !spawn_clicked {
        if let Some(pos) = resp.interact_pointer_pos() {
            if delete_rect.is_some_and(|r| r.contains(pos)) {
                delete_clicked = true;
            } else if spawn_rect.is_some_and(|r| r.contains(pos)) {
                spawn_clicked = true;
            }
        }
    }

    let mut set_base_clicked = false;
    let mut spawn_profile_clicked: Option<String> = None;
    resp.context_menu(|ui| {
        if ui.button("Set base branch…").clicked() {
            set_base_clicked = true;
            ui.close_menu();
        }
        if !profiles.is_empty() {
            ui.separator();
            ui.label(RichText::new("Open session").color(theme.text_muted).small());
            ui.set_max_width(WORKTREE_MENU_MAX_WIDTH);
            for (i, (name, command)) in profiles.iter().enumerate() {
                let btn = ui.button(profile_menu_label(i + 1, name));
                if btn.on_hover_text(command.as_str()).clicked() {
                    spawn_profile_clicked = Some(name.clone());
                    ui.close_menu();
                }
            }
        }
    });

    let bg = if is_active {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        Color32::TRANSPARENT
    };
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if bg != Color32::TRANSPARENT {
        ui.painter().set(bg_idx, egui::Shape::rect_filled(full_rect, 0.0, bg));
    }
    if is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if scroll_into_view {
        ui.scroll_to_rect(full_rect, theme.scroll_align);
    }
    WorktreeAction {
        // A prunable row is still worth clicking when shells are homed there;
        // `activate_worktree` turns the ones that aren't into the prune hint.
        activate: !deleting && resp.clicked() && !delete_clicked && !spawn_clicked,
        delete: delete_clicked,
        spawn: spawn_clicked,
        set_base: set_base_clicked,
        spawn_profile: spawn_profile_clicked,
        rect: full_rect,
    }
}

/// The liveness cache corrects discovery for paint and navigation only. Keep
/// this shared so a row that has just gone grey cannot remain a dead stop in
/// the workspace ring. Main checkouts are never prune candidates, even when
/// their project is a non-git directory with no .git entry.
fn worktree_looks_gone(wt: &Worktree, missing: Option<bool>) -> bool {
    missing.map_or(wt.prunable, |gone| gone && !wt.is_main)
}

fn worktree_is_switchable(wt: &Worktree, missing: Option<bool>, has_sessions: bool) -> bool {
    !worktree_looks_gone(wt, missing) || has_sessions
}

/// The workspaces a herdr agent may be matched against.  A checkout that
/// looks gone offers none, which is what makes an agent working there
/// unmatched rather than parked under a row that can only refuse it.
/// `missing` is the liveness cache's word for a path, `None` where it has
/// none, so the row's grey and this list agree about the same directory.
fn herdr_workspaces(projects: &[Project], missing: impl Fn(&Path) -> Option<bool>) -> Vec<PathBuf> {
    projects
        .iter()
        .flat_map(|p| p.worktrees.iter())
        .filter(|wt| !worktree_looks_gone(wt, missing(&wt.path)))
        .map(|wt| wt.path.clone())
        .collect()
}

struct SessionRowAction {
    activate: bool,
    close: bool,
    /// Full-width row rect, for a drop target to test the pointer against.
    rect: egui::Rect,
}

/// `draggable` makes the whole row the drag handle rather than adding a grip:
/// a session row is a tab, where a project row's own controls are what a click
/// there is usually for.
fn session_row(
    ui: &mut egui::Ui,
    row: &SessionRowData,
    is_cursor: bool,
    scroll_into_view: bool,
    draggable: bool,
    icons: &Icons,
    theme: &Theme,
) -> SessionRowAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut close_clicked = false;
    let mut hints = IconHints::default();
    let mut close_rect: Option<egui::Rect> = None;
    let mut title_elided = false;
    // The leading and trailing groups run as sibling closures, so the leading
    // slots' hints travel out separately and join the rest afterwards.
    let mut status_hint = None;
    let mut managed_slot = None;
    // One indent level deeper than worktree rows (16); right: 0 keeps the ×
    // at the same x as the other rows' trailing icons.
    let frame = Frame::default().inner_margin(Margin { left: 28, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            let title_color = if row.is_active { theme.text } else { theme.text_dim };
            row_with_trailing(
                ui,
                |ui| {
                    status_hint = paint_row_status_icon(
                        ui,
                        theme,
                        RowStatus {
                            attention: row.needs_attention,
                            activity: row.activity,
                            managed: row.managed.as_ref(),
                        },
                        &icons.session,
                        DEFAULT_SESSION_ICON,
                        row.is_active,
                    );
                    if let Some(managed) = &row.managed {
                        let rect = paint_managed_mark(ui, icons, theme, theme.text_muted);
                        managed_slot = Some((rect, managed_tooltip(managed)));
                    }
                    let (_, galley) = truncating_label(
                        ui,
                        row_name_text(ui, &row.name, title_color, theme.text_muted),
                        title_color,
                        egui::Sense::hover(),
                    );
                    title_elided = galley.elided;
                },
                |ui| {
                    let btn = styled_icon_button(
                        ui,
                        &icons.close_session,
                        DEFAULT_CLOSE_ICON,
                        theme.text_muted,
                        theme,
                    );
                    hints.add(btn.rect, close_button_hint(row.managed.is_some()));
                    close_rect = Some(btn.rect);
                    if btn.clicked() {
                        close_clicked = true;
                    }
                },
            );
        })
        .response
        .interact(if draggable {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::click()
        });
    if let Some((rect, hint)) = status_hint {
        hints.add(rect, hint);
    }
    if let Some((rect, hint)) = managed_slot {
        hints.add(rect, hint);
    }
    // A managed row answers with the harness's own sentence wherever the
    // pointer is not on an icon: the row the user attached is the one they
    // ask how to leave, and the name tooltip cannot say it.
    let resp = hints.apply(resp, theme.icon_tooltips, |resp| match &row.managed {
        Some(managed) if theme.icon_tooltips => resp.on_hover_text(managed_tooltip(managed)),
        _ => name_tooltip(resp, &row.name.text, title_elided, theme.sidebar_tooltips),
    });

    // Frame allocates its space at end-of-show, so its retroactive `interact`
    // registers *after* the inner button in egui's z-order — meaning clicks on
    // the × land on this row response, not the button.  Recover by routing
    // clicks whose position falls inside the button rect to close.
    if resp.clicked() && !close_clicked {
        if let (Some(rect), Some(pos)) = (close_rect, resp.interact_pointer_pos()) {
            if rect.contains(pos) {
                close_clicked = true;
            }
        }
    }

    let bg = if row.is_displayed {
        theme.row_active_bg
    } else if resp.hovered() {
        theme.row_hover_bg
    } else {
        Color32::TRANSPARENT
    };
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if bg != Color32::TRANSPARENT {
        ui.painter().set(bg_idx, egui::Shape::rect_filled(full_rect, 0.0, bg));
    }
    if is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if scroll_into_view {
        ui.scroll_to_rect(full_rect, theme.scroll_align);
    }
    if draggable {
        resp.dnd_set_drag_payload(DraggedSession(row.id));
    }
    SessionRowAction {
        activate: resp.clicked() && !close_clicked,
        close: close_clicked,
        rect: full_rect,
    }
}

struct HerdrRowAction {
    attach: bool,
}

/// A row's name as two spans: the context, then the identity.  Nothing
/// separates them but weight — a punctuation mark here would spell out a
/// relationship the colours already show, and the status word one used to
/// join only repeated the mark two slots to its left.
///
/// One `LayoutJob` rather than two labels, for the reasons `path_text` gives:
/// no `item_spacing` gap, no second response competing for the row's click,
/// and elision that measures the whole stream.
fn row_name_text(
    ui: &egui::Ui,
    name: &RowName,
    text_color: Color32,
    context_color: Color32,
) -> egui::WidgetText {
    let Some(context) = &name.context else {
        return RichText::new(&name.text).small().color(text_color).into();
    };
    let size = egui::TextStyle::Small.resolve(ui.style()).size;
    // A hand-built job does not inherit the ui's text valign the way RichText
    // does, so it must be carried across or the text sits off-centre against
    // the marks beside it.
    let valign = ui.text_valign();
    let mut job = egui::text::LayoutJob::default();
    for (text, color) in [(format!("{context} "), context_color), (name.text.clone(), text_color)] {
        job.append(&text, 0.0, egui::TextFormat {
            font_id: egui::FontId::new(size, egui::FontFamily::Proportional),
            color,
            valign,
            ..Default::default()
        });
    }
    job.into()
}

/// What a harness-managed row explains on hover, one fact per comma: the
/// state, since that is what changes; who reports it; whether the attach is
/// the harness's whole view; and what the harness calls the pane.  The way
/// out follows in parentheses, since it is an instruction rather than
/// another fact about the pane.
///
/// The same sentence serves a listed agent and an attached one.  Attaching
/// changes how alacritree draws a pane, not what there is to say about it,
/// and the chord has no other surface in alacritree — it is the harness's
/// key, not one of ours — so it has to reach the row the user is sitting in.
fn managed_tooltip(managed: &Managed) -> String {
    let mut parts = Vec::new();
    if let Some(mark) = managed.mark {
        parts.push(mark.label.to_owned());
    }
    parts.push(managed.harness.to_owned());
    if managed.shared_view {
        parts.push("shared view".to_owned());
    }
    parts.extend(managed.pane_name());
    let mut hint = parts.join(", ");
    hint.push('.');
    if let Some(chord) = &managed.detach {
        // Backquoted because the chord is a sequence, not one combination:
        // unquoted, "detach with Ctrl+B q" reads as a sentence whose last
        // word happens to be `q`.
        hint.push_str(&format!(" (detach with `{chord}`)"));
    }
    hint
}

/// A shared-view attach waiting on herdr.  The gesture answers with the argv
/// its client runs, so everything the session needs is in hand by the time it
/// opens.
struct PendingHerdrAttach {
    job: jobs::Job<Result<(String, Vec<String>), String>>,
    key: herdr::HerdrKey,
    workspace: WorkspaceKey,
}

/// The shared view herdr is being pointed at, and the call doing the
/// pointing.  The handle is held rather than dropped because dropping a job
/// cancels it.
struct HerdrViewFocus {
    session: SessionId,
    job: jobs::Job<Result<(), String>>,
}

/// Whether the session on screen still owes herdr a focus call.  A direct
/// attach draws its own pane whatever herdr focuses, and an ordinary shell
/// has no pane at all, so neither ever asks.
fn needs_view_focus(
    key: Option<&herdr::HerdrKey>,
    active: SessionId,
    focused: Option<SessionId>,
) -> bool {
    key.is_some_and(|key| !herdr::can_attach(&key.side)) && focused != Some(active)
}

/// What a shared-view attach asks herdr before its client can start: focus
/// the pane, since every app client draws whatever herdr has focused, then
/// name the session, since that is what the client attaches to.  Both are
/// process spawns, and on native Windows both wait on herdr starting up,
/// which is why this only ever runs on the pool.
///
/// `cached_name` is what the endpoint learned in the background.  A gesture
/// that beats the first read asks herdr itself: a wait is better than a
/// refusal.
fn herdr_attach_gesture(
    side: &herdr::Side,
    pane_id: &str,
    cached_name: Option<String>,
) -> Result<(String, Vec<String>), String> {
    // Two argv spawns, no shell: the only shell a `Native` command could
    // reach on this side is cmd.exe, which does not understand `sh_quote`'s
    // single-quoting.
    herdr::focus_agent(side, pane_id)?;
    let session = match cached_name {
        Some(session) => session,
        None => herdr::running_session_name(side)?,
    };
    Ok(side.command(&["session", "attach", &session]))
}

/// Whether ending a session asks first.  A harness-managed one is a detach
/// rather than a kill, so it answers to its own switch: the attach client is
/// always running, which would make the busy question a close asks fire every
/// time and warn about nothing.
fn close_needs_prompt(ui: &UiTheme, managed: bool, busy: bool) -> bool {
    if managed { ui.confirm_session_detach } else { ui.confirm_session_close.requires_prompt(busy) }
}

/// What the × on a session row does.  Ending a harness-managed session ends
/// the attach client and nothing else — the pane keeps running under the
/// harness, and the row it came from comes back — so calling that a close
/// promises a destruction that does not happen.
fn close_button_hint(managed: bool) -> &'static str {
    if managed { "detach session" } else { "close session" }
}

/// Paint the harness mark and return the rect it claimed, so the caller can
/// hang the hint on it.
fn paint_managed_mark(
    ui: &mut egui::Ui,
    icons: &Icons,
    theme: &Theme,
    color: Color32,
) -> egui::Rect {
    // 10.0 is what the status marks beside it use, and `◫` shares its em
    // height with `◇` and `●`, so the same size puts them on one optical line.
    let (glyph, font, glyph_color) =
        resolve_icon(&icons.herdr, DEFAULT_HERDR_ICON, color, 10.0, 10.0, theme);
    ui.label(RichText::new(glyph).color(glyph_color).font(font)).rect
}

/// A herdr agent nothing is attached to.  Drawn in `theme.text_dim` because
/// it is listed but not live — the same weight `worktree_gone` gives a row
/// whose checkout has been removed.  An attached agent has an ordinary
/// session row instead, so no agent is ever drawn twice.
///
/// Not draggable and carries no drop-target rect: a herdr agent has no
/// position in the session order to reorder into.
fn herdr_row(
    ui: &mut egui::Ui,
    row: &HerdrRowData,
    is_cursor: bool,
    scroll_into_view: bool,
    icons: &Icons,
    theme: &Theme,
) -> HerdrRowAction {
    // Reserve a slot *before* the label so the hover bg paints beneath it.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let frame = Frame::default().inner_margin(Margin { left: 28, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            row_with_trailing(
                ui,
                |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
                    paint_harness_mark(ui, row.managed.mark, rect, theme);
                    paint_managed_mark(ui, icons, theme, theme.text_dim);
                    let text = row_name_text(ui, &row.name, theme.text_dim, theme.text_muted);
                    let _ = truncating_label(ui, text, theme.text_dim, egui::Sense::hover());
                },
                |_ui| {},
            );
        })
        .response
        .interact(egui::Sense::click());
    let resp =
        if theme.icon_tooltips { resp.on_hover_text(managed_tooltip(&row.managed)) } else { resp };

    let bg = if resp.hovered() { theme.row_hover_bg } else { Color32::TRANSPARENT };
    let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
    if bg != Color32::TRANSPARENT {
        ui.painter().set(bg_idx, egui::Shape::rect_filled(full_rect, 0.0, bg));
    }
    if is_cursor {
        paint_cursor_outline(ui, full_rect, theme);
    }
    if scroll_into_view {
        ui.scroll_to_rect(full_rect, None);
    }
    HerdrRowAction { attach: resp.clicked() }
}

impl AlacritreeApp {
    fn reap_exited_sessions(&mut self, ctx: &Context) {
        let exited_ids: Vec<SessionId> =
            self.sessions.iter().filter(|s| s.should_reap()).map(|s| s.id).collect();
        for id in exited_ids {
            self.close_session(ctx, id);
        }
    }

    /// Handle session-switch requests from clicked notifications.  A stale
    /// id (session closed before the click) makes the activate a no-op, but
    /// the window still comes forward — the user asked for the app.
    fn process_notification_actions(&mut self, ctx: &Context) {
        let Some(id) = latest_notification_click(&self.notify_rx) else { return };
        self.activate_session_by_id(id);
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Drain every session's PTY events and surface "needs attention" for
    /// any session the user isn't currently looking at.
    fn process_session_events(&mut self, ctx: &Context) {
        let visible_idx = self.active_session_index();
        // `viewport().focused` is `None` on platforms that don't report focus;
        // treat unknown as "focused" so we don't pile up stale attention dots.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);

        // Only the session on screen, and only while the window has focus:
        // typing somewhere else is the one moment a terminal has no claim on
        // the machine.  Both calls are no-ops unless they change something, so
        // a frame where focus has not moved costs nothing, and a session with
        // no boost to give — the feature off, or a platform that has none —
        // answers false without a call of any kind.
        let target = visible_idx.filter(|_| focused);
        let anything_raised =
            frame_holds_self_boost(self.sessions.iter().enumerate().map(|(idx, session)| {
                let wanted = Some(idx) == target;
                SessionBoost {
                    raised: session.set_priority_boost(wanted),
                    visible: wanted,
                    pending: session.is_pending(),
                }
            }));
        // A boost covers every depth, so a focused tab running
        // `cargo build -j16` raises all sixteen compilers.  The GUI left at
        // normal would then lose to the tree it is drawing.
        crate::focus_priority::set_self_boosted(anything_raised);

        let grace = self.config.ui.attention_grace;
        for idx in 0..self.sessions.len() {
            // Window focus is deliberately not part of this: an unfocused
            // window still shows its grid, so its output still has to repaint.
            self.sessions[idx].set_visible(Some(idx) == visible_idx);
            let outcome = self.sessions[idx].drain_events(&self.config.palette);
            // Ahead of the attention early-out: a background session copying
            // with OSC 52 still owns the clipboard.
            for (target, text) in &outcome.clipboard {
                clipboard::write(*target, text);
            }
            let is_visible_to_user = Some(idx) == visible_idx && focused;
            if is_visible_to_user {
                // Nothing pending survives the user already looking at it.
                self.sessions[idx].pending_attention = None;
                continue;
            }
            if outcome.attention && self.sessions[idx].pending_attention.is_none() {
                self.sessions[idx].pending_attention = Some(Instant::now());
            }
            let Some(since) = self.sessions[idx].pending_attention else {
                continue;
            };
            match poll_attention_debounce(since, Instant::now(), &self.sessions[idx].title, grace) {
                AttentionVerdict::Cancel => self.sessions[idx].pending_attention = None,
                // A quiet PTY repaints nothing on its own, so the wake-up
                // that decides the ping has to be scheduled here.
                AttentionVerdict::Wait(remaining) => ctx.request_repaint_after(remaining),
                AttentionVerdict::Fire => {
                    self.sessions[idx].pending_attention = None;
                    // Only toast on the *transition* into needs_attention — otherwise
                    // BEL + title-transition firing in the same idle cycle would
                    // produce two toasts for the same "Claude is done" event.
                    let was_attending = self.sessions[idx].needs_attention;
                    self.sessions[idx].needs_attention = true;
                    if !was_attending && self.config.ui.notifications {
                        notify_attention(&self.sessions[idx], ctx);
                    }
                },
            }
        }

        // Visible session shouldn't keep an attention marker once the user is
        // actually looking at it — covers tab switches, workspace switches,
        // and refocusing the window after stepping away.
        if focused {
            if let Some(idx) = visible_idx {
                self.sessions[idx].needs_attention = false;
            }
        }
    }

    fn workspace_needs_attention(&self, ws: &WorkspaceKey) -> bool {
        self.sessions.iter().any(|s| s.working_directory == *ws && s.needs_attention)
    }

    fn project_needs_attention(&self, project: &Project) -> bool {
        project.worktrees.iter().any(|wt| self.workspace_needs_attention(&Some(wt.path.clone())))
    }

    /// Prefer the active session's status so parallel agents do not fight over
    /// the parent row. If that session has nothing to report, a background
    /// session that is working or blocked wins over a merely present agent,
    /// because a collapsed row is the only place either state can surface.
    fn workspace_activity(&self, ws: &WorkspaceKey) -> SessionActivity {
        let active_id = self.active_session.get(ws).copied();
        let mut other = SessionActivity::Shell;
        for s in &self.sessions {
            if s.working_directory != *ws {
                continue;
            }
            let activity = herdr_backed_activity(s.activity(), self.session_herdr_status(s));
            let Some(live) = activity.live() else {
                continue;
            };
            if Some(s.id) == active_id {
                return activity;
            }
            if live > LiveState::Idle || !other.is_agent() {
                other = activity;
            }
        }
        other
    }

    /// Every row each workspace lists, in the order it draws them: its own
    /// shell sessions first, then every herdr pane the workspace holds — the
    /// sessions attached to one and the agents nothing is attached to alike —
    /// in herdr's own order.
    ///
    /// Position comes from herdr rather than from alacritree because herdr is
    /// the only party that has an opinion surviving all three moments: attach,
    /// detach, and a restart, which leaves every pane unattached again.  An
    /// order built from when a session was attached agrees with itself until
    /// the first restart and then contradicts everything the user saw.
    ///
    /// Agents are bucketed by the directory they work in, and an agent whose
    /// directory matches no worktree — including one whose checkout has been
    /// removed — lands under Home, which is the common case: an agent in a
    /// repository alacritree does not track still belongs somewhere, and a
    /// checkout that has gone cannot start a shell.
    fn listed_workspace_rows(&self) -> sidebar_nav::ListedRows {
        use sidebar_nav::WorkspaceEntry;

        // `usize::MAX` parks a pane herdr has stopped listing at the tail of
        // its own block rather than letting it fall in among the shells: the
        // session is still herdr's, and it goes back to its slot when the
        // listing carries it again.
        let mut shells: HashMap<WorkspaceKey, Vec<SessionId>> = HashMap::new();
        let mut managed: HashMap<WorkspaceKey, Vec<(usize, WorkspaceEntry)>> = HashMap::new();
        for session in &self.sessions {
            let ws = session.working_directory.clone();
            match session.herdr_key.clone() {
                Some(key) => {
                    let at = self.herdr_pane_index(&key).unwrap_or(usize::MAX);
                    managed.entry(ws).or_default().push((at, WorkspaceEntry::Session(session.id)));
                },
                None => shells.entry(ws).or_default().push(session.id),
            }
        }

        if self.config.ui.herdr.enabled {
            let claimed: Vec<herdr::HerdrKey> =
                self.sessions.iter().filter_map(|s| s.herdr_key.clone()).collect();
            let workspaces = herdr_workspaces(&self.projects, |path| self.liveness.missing(path));
            for cache in self.herdr_endpoints.caches() {
                let side = cache.side();
                for agent in herdr::unattached(cache.agents(), side, &claimed) {
                    let ws = herdr::match_workspace(agent, side, &workspaces);
                    if ws.is_none() && !self.config.ui.herdr.show_unmatched {
                        continue;
                    }
                    let key = herdr::HerdrKey {
                        side: side.clone(),
                        terminal_id: agent.terminal_id.clone(),
                    };
                    let at = self.herdr_pane_index(&key).unwrap_or(usize::MAX);
                    let entry = WorkspaceEntry::Agent(side.clone(), agent.terminal_id.clone());
                    managed.entry(ws).or_default().push((at, entry));
                }
            }
        }

        let mut listed = sidebar_nav::ListedRows::new();
        for ws in shells.keys().chain(managed.keys()).cloned().collect::<Vec<_>>() {
            if listed.contains_key(&ws) {
                continue;
            }
            let entries = workspace_entries(
                shells.get(&ws).map_or(&[][..], Vec::as_slice),
                managed.remove(&ws).unwrap_or_default(),
                self.session_rows_always,
            );
            if !entries.is_empty() {
                listed.insert(ws, entries);
            }
        }
        listed
    }

    /// Where a pane sits in herdr's own listing, counted across endpoints in
    /// the order they are polled.  `None` for a pane no endpoint lists.
    fn herdr_pane_index(&self, key: &herdr::HerdrKey) -> Option<usize> {
        let mut before = 0;
        for cache in self.herdr_endpoints.caches() {
            if cache.side() == &key.side {
                let at = cache.agents().iter().position(|a| a.terminal_id == key.terminal_id)?;
                return Some(before + at);
            }
            before += cache.agents().len();
        }
        None
    }

    /// The workspace a herdr row is currently listed under, for the keyboard
    /// path: `SidebarRow::HerdrAgent` itself carries no workspace, unlike a
    /// click, which already knows which panel section it landed in.
    fn herdr_row_workspace(&self, side: &herdr::Side, terminal_id: &str) -> Option<WorkspaceKey> {
        let wanted = sidebar_nav::WorkspaceEntry::Agent(side.clone(), terminal_id.to_string());
        self.listed_workspace_rows()
            .into_iter()
            .find(|(_, entries)| entries.contains(&wanted))
            .map(|(ws, _)| ws)
    }

    /// The agent behind `(side, terminal_id)`, if its endpoint still has it
    /// cached.  A stale key (the agent exited between poll and paint, or an
    /// Enter that outraced this frame's own listing) yields no row rather
    /// than a panic; the next poll drops it from the listing for good.
    fn find_herdr_agent(&self, side: &herdr::Side, terminal_id: &str) -> Option<&herdr::Agent> {
        self.herdr_endpoints
            .caches()
            .iter()
            .find(|cache| cache.side() == side)
            .and_then(|cache| cache.agents().iter().find(|a| a.terminal_id == terminal_id))
    }

    /// herdr's word on a session's agent: `Some` only while this session is
    /// attached to one the endpoint listing still carries.
    fn session_herdr_status(&self, session: &Session) -> Option<herdr::Status> {
        self.session_herdr_agent(session).map(|agent| agent.status)
    }

    /// The agent this session is attached to, while the endpoint listing
    /// still carries it.  herdr watches the pane from outside, so it is the
    /// authority on both what the pane is called and what it is doing.
    fn session_herdr_agent(&self, session: &Session) -> Option<&herdr::Agent> {
        let key = session.herdr_key.as_ref()?;
        self.find_herdr_agent(&key.side, &key.terminal_id)
    }

    /// What the endpoint learned this side's herdr session is called.
    fn herdr_session_name(&self, side: &herdr::Side) -> Option<String> {
        self.herdr_endpoints
            .caches()
            .iter()
            .find(|cache| cache.side() == side)
            .and_then(herdr::EndpointCache::session_name)
    }

    /// The session already attached to this agent, if one is open.
    fn herdr_session_for(&self, key: &herdr::HerdrKey) -> Option<SessionId> {
        self.sessions.iter().find(|s| s.herdr_key.as_ref() == Some(key)).map(|s| s.id)
    }

    /// herdr's detach chord on `side`, once that endpoint's config read has
    /// landed.
    fn herdr_settings(&self, side: &herdr::Side) -> herdr::Settings {
        self.herdr_endpoints
            .caches()
            .iter()
            .find(|cache| cache.side() == side)
            .map(herdr::EndpointCache::settings)
            .unwrap_or_default()
    }

    /// What supervises `session`, when anything does.  Derived per frame
    /// rather than stored, so a config read that lands later, or a herdr that
    /// stops listing the agent, reaches the row without a second source of
    /// truth to keep in step.
    fn session_managed(&self, session: &Session) -> Option<Managed> {
        let key = session.herdr_key.as_ref()?;
        let agent = self.session_herdr_agent(session);
        Some(Managed::herdr(&key.side, &self.herdr_settings(&key.side), agent))
    }

    /// One number standing for every endpoint's rendered state, so the
    /// sidebar's per-frame comparison stays a `u64` compare.
    fn herdr_generation(&self) -> u64 {
        self.herdr_endpoints.generation()
    }

    /// Refreshes the herdr endpoints on their own clock; a no-op per endpoint
    /// until its poll interval elapses.  Disabled stops the polling, not just
    /// the rows: the subprocesses are the whole cost of the feature, so an
    /// opt-out that kept running them would opt out of nothing.
    fn poll_herdr_endpoints(&mut self) {
        if !self.config.ui.herdr.enabled {
            return;
        }
        self.herdr_endpoints.poll(self.config.ui.herdr.poll_interval);
    }

    /// The rows `ws` paints, in `listed`'s order.  An entry whose session or
    /// agent has gone since the listing was built yields no row rather than a
    /// panic; the next rebuild drops it for good.
    fn workspace_rows(
        &self,
        ws: &WorkspaceKey,
        listed: &sidebar_nav::ListedRows,
    ) -> Vec<WorkspaceRowData> {
        let Some(entries) = listed.get(ws) else { return Vec::new() };
        let active = self.active_session.get(ws).copied();
        let is_current = self.current_workspace == *ws;
        entries
            .iter()
            .filter_map(|entry| match entry {
                sidebar_nav::WorkspaceEntry::Session(id) => {
                    let s = self.sessions.iter().find(|s| s.id == *id)?;
                    let activity =
                        herdr_backed_activity(s.activity(), self.session_herdr_status(s));
                    Some(WorkspaceRowData::Session(SessionRowData {
                        id: s.id,
                        name: session_row_name(&s.title, activity, self.session_herdr_agent(s)),
                        needs_attention: s.needs_attention,
                        activity,
                        is_active: active == Some(s.id),
                        is_displayed: is_current && active == Some(s.id),
                        managed: self.session_managed(s),
                    }))
                },
                sidebar_nav::WorkspaceEntry::Agent(side, terminal_id) => {
                    let agent = self.find_herdr_agent(side, terminal_id)?;
                    let settings = self.herdr_settings(side);
                    Some(WorkspaceRowData::Herdr(HerdrRowData::from_agent(agent, side, &settings)))
                },
            })
            .collect()
    }

    fn show_delete_dialog(&mut self, ctx: &Context) {
        if self.pending_delete.is_none() {
            return;
        }

        // Consume Enter/Escape, and act on a confirm, before adopting a
        // dirty count below: adoption can flip `force` from `false` to
        // `true` this same frame, but the keypress was the user's reaction
        // to what was already painted (a previous frame's "checking…", read
        // as `force: false`). Executing the confirm here, against the
        // request as it stands before this frame's adoption runs, is what
        // keeps "the `force` a confirm executes" equal to "the `force` the
        // user was shown" — held Enter (key repeat) would otherwise hit the
        // race on the exact frame the probe lands.
        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        if confirm_via_key {
            self.run_pending_delete(ctx);
            return;
        }
        if cancel_via_key {
            self.pending_delete = None;
            return;
        }

        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let Some(req) = self.pending_delete.as_mut() else {
            return;
        };
        if let Some(job) = req.dirty_job.as_ref() {
            match job.poll() {
                Some(counts) => {
                    // A known-dirty count preloads `force` so confirming goes
                    // straight to a forced removal, with the discard warning
                    // already on screen — the same outcome a warm cache gets
                    // at request time, just landing a frame later.
                    req.force = counts.is_dirty();
                    req.dirty = Some(counts);
                    req.dirty_job = None;
                },
                // A panicked probe never lands a count; drop the handle so
                // the dialog stops claiming to be checking and reads
                // "couldn't check" instead (see `dirty_warning`).
                None if job.failed() => req.dirty_job = None,
                None => {},
            }
        }
        let (title, detail, verb) = if req.prunable {
            (
                format!("Prune worktree `{}`?", req.worktree_name),
                "The worktree directory is already gone; this removes git's leftover metadata."
                    .to_string(),
                "Prune",
            )
        } else {
            (
                format!("Delete worktree `{}`?", req.worktree_name),
                match &req.branch {
                    Some(b) => format!("Removes the worktree directory and deletes branch `{b}`."),
                    None => "Removes the worktree directory.".to_string(),
                },
                "Delete",
            )
        };
        let warning = dirty_warning(req.dirty.as_ref(), req.force, req.dirty_job.is_some());

        let frame = modal_frame(&theme);
        let mut confirmed = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_delete_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(360.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(title).color(theme.text).strong());
                ui.label(RichText::new(detail).color(theme.text_muted).small());
                if let Some(w) = &warning {
                    ui.label(RichText::new(w).color(danger).small());
                }
                if req.prunable {
                    if let Some(b) = req.branch.clone() {
                        ui.checkbox(
                            &mut req.delete_branch,
                            RichText::new(format!("Also delete branch `{b}`"))
                                .color(theme.text_muted)
                                .small(),
                        );
                    }
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Enter to {} · Esc to cancel", verb.to_lowercase()))
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let delete = modal_button(ui, &theme, verb, danger);
                        if delete.clicked() {
                            confirmed = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), delete.id);
                    });
                });
            },
        );

        if confirmed {
            self.run_pending_delete(ctx);
            return;
        }
        if cancelled || modal.should_close() {
            self.pending_delete = None;
        }
    }

    fn show_close_session_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let Some(id) = self.pending_session_close else {
            return;
        };
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            // Exited between the click and this frame — nothing left to close.
            self.pending_session_close = None;
            return;
        };
        // A managed session's attach client is always running, so the busy
        // warning would fire every time and warn about nothing: what it
        // guards against is losing work, and detaching loses none.
        let managed = session.herdr_key.is_some();
        let title = if managed {
            format!("Detach from `{}`?", session.title)
        } else {
            format!("Close session `{}`?", session.title)
        };
        let busy = session.is_busy() && !managed;

        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        let frame = modal_frame(&theme);
        let mut confirmed = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_close_session_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(320.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(title).color(theme.text).strong());
                if busy {
                    ui.label(
                        RichText::new("A process appears to be running.").color(danger).small(),
                    );
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    let keys = if managed {
                        "Enter to detach · Esc to cancel"
                    } else {
                        "Enter to close · Esc to cancel"
                    };
                    ui.label(RichText::new(keys).color(theme.text_muted).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (verb, tint) =
                            if managed { ("Detach", theme.text) } else { ("Close", danger) };
                        let close_btn = modal_button(ui, &theme, verb, tint);
                        if close_btn.clicked() {
                            confirmed = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), close_btn.id);
                    });
                });
            });

        if confirm_via_key || confirmed {
            self.pending_session_close = None;
            self.close_session(ctx, id);
            return;
        }
        if cancel_via_key || cancelled || modal.should_close() {
            self.pending_session_close = None;
        }
    }

    fn show_remove_project_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let Some(state) = self.pending_project_remove.as_ref() else {
            return;
        };
        let title = format!("Remove `{}` from the sidebar?", state.name);

        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        let frame = modal_frame(&theme);
        let mut confirmed = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_remove_project_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(340.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(title).color(theme.text).strong());
                ui.label(
                    RichText::new("Nothing on disk is touched; open sessions keep running.")
                        .color(theme.text_muted)
                        .small(),
                );
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to remove · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let remove = ui.add(
                            egui::Button::new(RichText::new("Remove").color(danger)).frame(false),
                        );
                        if remove.clicked() {
                            confirmed = true;
                        }
                        let cancel = ui.add(
                            egui::Button::new(RichText::new("Cancel").color(theme.text_dim))
                                .frame(false),
                        );
                        if cancel.clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), remove.id);
                    });
                });
            });

        if confirm_via_key || confirmed {
            // Re-resolve by root: the list may have shifted (reorder, IPC) while
            // the modal was up.
            if let Some(state) = self.pending_project_remove.take() {
                if let Some(idx) = self.projects.iter().position(|p| p.root == state.root) {
                    self.remove_project(idx);
                }
            }
            return;
        }
        if cancel_via_key || cancelled || modal.should_close() {
            self.pending_project_remove = None;
        }
    }

    fn show_error_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let Some(message) = self.error_dialog.clone() else {
            return;
        };

        // Enter and Esc both just dismiss — there's nothing to confirm.
        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        let frame = modal_frame(&theme);
        let mut dismissed = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_error_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(360.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new("Something went wrong").color(danger).strong());
                ui.label(RichText::new(&message).color(theme.text_muted).small());
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter or Esc to dismiss").color(theme.text_muted).small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ok = ui.add(
                            egui::Button::new(RichText::new("OK").color(theme.text)).frame(false),
                        );
                        if ok.clicked() {
                            dismissed = true;
                        }
                        focus_default(ui.ctx(), ok.id);
                    });
                });
            },
        );

        if confirm_via_key || cancel_via_key || dismissed || modal.should_close() {
            self.error_dialog = None;
        }
    }

    /// The Ctrl+K command palette: one fuzzy-searchable, executable list of
    /// every keyboard action, open session, and switchable workspace.  A real
    /// modal — while it is up, terminal input and bindings are suppressed (see
    /// `update`) and the palette owns its own keys.
    fn show_command_palette(&mut self, ctx: &Context) {
        let theme = self.theme;
        let s = theme.ui_scale;

        // Drain the nav/confirm/cancel keys before the TextEdit runs so it
        // never steals Enter (run), Esc (clear then close), the arrows, or the
        // bound cursor jumps.  Ctrl+K shuts the palette with the same key that
        // opened it.
        let (cancel, confirm) = consume_modal_keys(ctx);
        let (up, down) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            )
        });
        let jumps = consume_palette_keys(ctx, &self.config.bindings);
        let toggle = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::K));

        let items = self.palette_items();
        let hint = palette_hint(&self.config.bindings);
        let content_w = palette_content_width(s, ctx.screen_rect().width());
        let mut chosen: Option<PaletteAction> = None;

        let modal = {
            let palette = &mut self.palette;
            egui::Modal::new(egui::Id::new("alacritree_command_palette"))
                .frame(modal_frame(&theme))
                .show(ctx, |ui| {
                    ui.set_width(content_w);
                    ui.spacing_mut().item_spacing.y = 6.0 * s;
                    let cols = PaletteColumns::new(s, ui.available_width());

                    let input_id = egui::Id::new("alacritree_command_palette_query");
                    let query_changed = ui
                        .add(
                            egui::TextEdit::singleline(palette.query_mut())
                                .id(input_id)
                                .hint_text("search actions, sessions, workspaces")
                                .desired_width(f32::INFINITY),
                        )
                        .changed();
                    focus_default(ui.ctx(), input_id);

                    let ranked = palette.rank(&items);
                    // A query edit reseeds to the top match; the cursor keys
                    // then move within this frame's results.
                    palette.reseed(query_changed, ranked.len());
                    let groups = command_palette::group(&items, &ranked);
                    // Sections reorder the ranked rows, so the cursor steps over
                    // this flattened view rather than the ranking itself.
                    let flat: Vec<usize> =
                        groups.iter().flat_map(|(_, rows)| rows.iter().copied()).collect();
                    if up {
                        palette.select_prev();
                    }
                    if down {
                        palette.select_next(flat.len());
                    }
                    for jump in &jumps {
                        match jump {
                            NamedAction::PaletteTop => palette.select_top(),
                            NamedAction::PaletteBottom => palette.select_bottom(flat.len()),
                            NamedAction::PalettePageUp => palette.page_up(),
                            NamedAction::PalettePageDown => palette.page_down(flat.len()),
                            _ => {},
                        }
                    }
                    let moved = query_changed || up || down || !jumps.is_empty();
                    if confirm {
                        chosen = flat.get(palette.selected()).map(|&i| items[i].action.clone());
                    }
                    let selected = palette.selected();

                    ui.add_space(2.0 * s);
                    paint_palette_header(ui, &theme, &cols);
                    egui::ScrollArea::vertical()
                        .max_height(400.0 * s)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0 * s;
                            if flat.is_empty() {
                                ui.add_space(4.0 * s);
                                ui.label(RichText::new("  no matches").color(theme.text_dim));
                                return;
                            }
                            let mut row = 0usize;
                            for (section, rows) in &groups {
                                paint_palette_section(ui, &theme, &cols, section.title());
                                for &i in rows {
                                    let is_sel = row == selected;
                                    let resp =
                                        paint_palette_row(ui, &theme, &cols, &items[i], is_sel)
                                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                                    if resp.clicked() {
                                        chosen = Some(items[i].action.clone());
                                    }
                                    // Keep the keyboard-selected row in view as
                                    // it moves past the fold.
                                    if is_sel && moved {
                                        resp.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    row += 1;
                                }
                            }
                        });

                    ui.add_space(6.0 * s);
                    ui.label(RichText::new(hint).color(theme.text_muted).small());
                })
        };

        if chosen.is_some() || toggle {
            self.palette.close();
        } else if cancel {
            // Esc narrows before it closes, mirroring the sidebar filters.
            if self.palette.query().is_empty() {
                self.palette.close();
            } else {
                self.palette.clear_query();
            }
        } else if modal.should_close() {
            self.palette.close();
        }

        if let Some(action) = chosen {
            self.run_palette_action(ctx, action);
        }
    }

    /// Everything the palette can act on this frame: every runnable keyboard
    /// action, then each configured shell profile, then each open session,
    /// then each switchable workspace.  Rebuilt each frame — cheap beside
    /// ranking, and always current as sessions and worktrees come and go.
    fn palette_items(&self) -> Vec<PaletteItem> {
        let mut items = command_palette::action_items(&self.config.bindings);
        for (i, profile) in self.config.profiles.iter().enumerate() {
            let index = i + 1;
            // SpawnProfile only binds indices 1..=9; past that there is no
            // config name to search by.
            let config_name =
                if index <= 9 { format!("SpawnProfile{index}") } else { String::new() };
            let command = profile_command(profile);
            let keys = command_palette::profile_keys(&self.config.bindings, index as u8);
            items.push(PaletteItem::profile(profile.name.clone(), command, keys, &config_name));
        }
        for session in &self.sessions {
            let ws = self.workspace_label(&session.working_directory);
            items.push(PaletteItem::session(
                session.id,
                session.title.clone(),
                format!("session · {ws}"),
            ));
        }
        for ws in self.workspace_order() {
            let (primary, secondary) = self.workspace_entry_label(&ws);
            items.push(PaletteItem::workspace(ws, primary, secondary));
        }
        for project in &self.projects {
            items.push(PaletteItem::create_worktree(
                project.root.clone(),
                format!("{}: new worktree", project.display_name()),
                format!("project · {}", wsl::display_path(&project.root)),
            ));
        }
        items
    }

    /// Human label for a workspace: `project / worktree` for a known worktree,
    /// "Home" for the home tab, else the path's final component.
    fn workspace_label(&self, ws: &WorkspaceKey) -> String {
        let Some(path) = ws else {
            return "Home".to_string();
        };
        for project in &self.projects {
            for wt in &project.worktrees {
                if &wt.path == path {
                    return format!("{} / {}", project.display_name(), wt.name);
                }
            }
        }
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| wsl::display_path(path))
    }

    /// The (primary, secondary) a workspace palette row shows.
    fn workspace_entry_label(&self, ws: &WorkspaceKey) -> (String, String) {
        let secondary = match ws {
            None => "workspace · home".to_string(),
            Some(path) => format!("workspace · {}", wsl::display_path(path)),
        };
        (self.workspace_label(ws), secondary)
    }

    /// Carry out a chosen palette row.  Actions dispatch exactly as their
    /// binding would; session/workspace rows switch to the target and hand
    /// focus back to the terminal so the user can type straight away; a project
    /// row opens the same new-worktree prompt the sidebar's `+` button does.
    fn run_palette_action(&mut self, ctx: &Context, action: PaletteAction) {
        match action {
            PaletteAction::Run(a) => {
                self.dispatch_action(ctx, BindingAction::Named(a), ActionOrigin::Palette);
            },
            PaletteAction::ActivateSession(id) => {
                self.activate_session_by_id(id);
                self.focus_terminal();
            },
            PaletteAction::SwitchWorkspace(ws) => {
                match ws {
                    None => self.activate_home(ctx),
                    Some(path) => self.activate_worktree(ctx, &path),
                }
                self.focus_terminal();
            },
            PaletteAction::CreateWorktree(root) => {
                if let Some(project_idx) = self.projects.iter().position(|p| p.root == root) {
                    self.pending_create = Some(CreateState::Prompt {
                        project_idx,
                        branch: String::new(),
                        error: None,
                    });
                }
            },
            PaletteAction::SpawnProfile(name) => {
                self.spawn_profile_session(ctx, &name);
                self.focus_terminal();
            },
        }
    }

    fn run_pending_delete(&mut self, ctx: &Context) {
        let Some(req) = self.pending_delete.take() else {
            return;
        };
        let project_root = self.projects[req.project_idx].root.clone();
        let policy = self.config.ui.last_session_close;
        let ring = policy.rings().then(|| self.session_ring()).unwrap_or_default();
        let removed: Vec<SessionId> = policy
            .rings()
            .then(|| {
                self.sessions
                    .iter()
                    .filter(|s| s.working_directory.as_deref() == Some(&req.worktree_path))
                    .map(|s| s.id)
                    .collect()
            })
            .unwrap_or_default();

        // Drop sessions whose cwd is the worktree before deleting it; the PTY
        // would otherwise block the directory removal on some filesystems.
        self.sessions.retain(|s| s.working_directory.as_deref() != Some(&req.worktree_path));
        self.active_session.remove(&Some(req.worktree_path.clone()));
        if self.current_workspace.as_deref() == Some(&req.worktree_path) {
            let landing = policy
                .rings()
                .then(|| {
                    let prefer = policy
                        .prefers_project()
                        .then(|| {
                            sidebar_nav::project_of(
                                &self.projects,
                                &Some(req.worktree_path.clone()),
                            )
                        })
                        .flatten();
                    ring_landing(&ring, &removed, prefer)
                })
                .flatten();
            let verdict = match landing {
                Some((_, id)) => CloseFallback::ActivateSession(id),
                None => CloseFallback::Home,
            };
            if defers_close_navigation(self.config.ui.sidebar_focus) {
                self.sidebar_deferred_close = Some(DeferredClose {
                    verdict,
                    removed_worktree: Some(req.worktree_path.clone()),
                });
                ctx.request_repaint();
            } else {
                // Deleting the on-screen worktree is an explicit user action,
                // so the view should greet with a live shell rather than the
                // "no session" placeholder.
                self.apply_close_fallback(ctx, verdict);
            }
        }

        // The git removal (shellouts, branch delete, doppler cleanup) is slow
        // enough to stutter paint, so run it off-thread and adopt the result in
        // `poll_pending_deletes`; the dialog closes immediately either way and
        // the sidebar row shows a spinner meanwhile.
        let worktree_path = req.worktree_path.clone();
        let worktree_name = req.worktree_name.clone();
        let branch = req.branch.clone();
        let delete_job = if req.prunable {
            wt::DeleteJob::Prune {
                worktree_name: req.worktree_name,
                branch: req.branch,
                delete_branch: req.delete_branch,
            }
        } else {
            // `req.force` already reflects a resolved dirty count (set in
            // `request_worktree_delete` or when its probe landed); a count
            // that never resolved before the confirm leaves it `false`, and
            // `poll_pending_deletes` retries with `force: true` once git
            // itself refuses the tree as dirty.
            wt::DeleteJob::Remove {
                worktree_path: req.worktree_path,
                branch: req.branch,
                force: req.force,
            }
        };
        let job = wt::spawn_delete(project_root, delete_job, ctx.clone());
        self.pending_deletes.push(DeleteTask {
            project_idx: req.project_idx,
            worktree_path,
            worktree_name,
            branch,
            dirty: req.dirty,
            delete_branch: req.delete_branch,
            prunable: req.prunable,
            job,
        });
    }

    /// Adopt finished background deletes: pop up any failure and refresh the
    /// affected project so the removed worktree (or its spinner) drops out of
    /// the sidebar. A refusal that names a dirty or untracked tree reopens the
    /// confirm as a forced retry instead of surfacing a plain error — git is
    /// the authority on whether the removal would lose work, not a count read
    /// while the user was staring at the first dialog.
    fn poll_pending_deletes(&mut self, ctx: &Context) {
        struct Finished {
            project_idx: usize,
            worktree_path: PathBuf,
            worktree_name: String,
            branch: Option<String>,
            dirty: Option<DirtyCounts>,
            delete_branch: bool,
            prunable: bool,
            result: Result<(), String>,
        }
        let mut finished: Vec<Finished> = Vec::new();
        self.pending_deletes.retain(|task| match task.job.poll() {
            Some(result) => {
                finished.push(Finished {
                    project_idx: task.project_idx,
                    worktree_path: task.worktree_path.clone(),
                    worktree_name: task.worktree_name.clone(),
                    branch: task.branch.clone(),
                    dirty: task.dirty,
                    delete_branch: task.delete_branch,
                    prunable: task.prunable,
                    result,
                });
                false
            },
            // A panicked delete job never lands a result; without this the
            // sidebar row's spinner would spin forever instead of surfacing
            // a failure.
            None if task.job.failed() => {
                finished.push(Finished {
                    project_idx: task.project_idx,
                    worktree_path: task.worktree_path.clone(),
                    worktree_name: task.worktree_name.clone(),
                    branch: task.branch.clone(),
                    dirty: task.dirty,
                    delete_branch: task.delete_branch,
                    prunable: task.prunable,
                    result: Err("the background worker panicked".to_string()),
                });
                false
            },
            None => true,
        });
        for f in finished {
            match f.result {
                Ok(()) => {},
                // Only opens the retry when no confirm is currently on
                // screen — reopening unconditionally would swap the dialog
                // contents under a user looking at an unrelated confirm
                // (same modal id, so an in-flight Enter would force-delete
                // the wrong worktree), and a second refusal landing in this
                // same batch would silently overwrite the first retry
                // instead of surfacing it.
                Err(e) if !f.prunable && refused_for_unsaved_work(&e) => {
                    if self.pending_delete.is_none() {
                        self.pending_delete = Some(DeleteRequest {
                            project_idx: f.project_idx,
                            worktree_path: f.worktree_path,
                            worktree_name: f.worktree_name,
                            branch: f.branch,
                            dirty: f.dirty,
                            dirty_job: None,
                            prunable: false,
                            delete_branch: f.delete_branch,
                            force: true,
                        });
                    } else {
                        self.error_dialog = Some(format!("Delete failed.\n\n{e}"));
                    }
                },
                Err(e) => {
                    let action = if f.prunable { "Prune" } else { "Delete" };
                    self.error_dialog = Some(format!("{action} failed.\n\n{e}"));
                },
            }
            self.refresh_project(ctx, f.project_idx);
        }
    }

    /// Adopt minimized creates once their worker finishes: pop up any failure
    /// (its modal is long gone) and refresh the project so the new worktree
    /// replaces its sidebar placeholder.  A successful create is deliberately
    /// not activated: the user minimized to work elsewhere, so don't yank them
    /// into the new worktree.
    fn poll_pending_creates(&mut self, ctx: &Context) {
        let mut finished: Vec<(usize, Result<PathBuf, String>)> = Vec::new();
        self.pending_creates.retain_mut(|task| {
            let mut done = None;
            loop {
                match task.rx.try_recv() {
                    Ok(Progress::Step(_)) => {},
                    Ok(Progress::Done(result)) => {
                        done = Some(result);
                        break;
                    },
                    // Nothing more this frame either way — a worker that
                    // unwound instead of reporting comes back through the
                    // failure latch below, so its placeholder row is
                    // replaced rather than left standing forever.
                    Err(_) => break,
                }
            }
            let _ = task.job.poll();
            let done =
                done.or_else(|| task.job.failed().then(|| Err(CREATE_WORKER_PANICKED.to_string())));
            match done {
                Some(result) => {
                    finished.push((task.project_idx, result));
                    false
                },
                None => true,
            }
        });
        for (project_idx, result) in finished {
            if let Err(e) = result {
                self.error_dialog = Some(format!("Worktree creation failed.\n\n{e}"));
            }
            self.refresh_project(ctx, project_idx);
        }
    }

    fn show_rename_dialog(&mut self, ctx: &Context) {
        let Some(RenameState { root, mut label }) = self.pending_rename.take() else {
            return;
        };
        // The project can vanish under the modal (IPC remove_project);
        // nothing is left to rename then.
        let Some(dir_name) = self.projects.iter().find(|p| p.root == root).map(|p| p.name.clone())
        else {
            return;
        };
        let theme = self.theme;
        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        let frame = modal_frame(&theme);
        let mut rename_clicked = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_rename_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(format!("Rename `{dir_name}`")).color(theme.text).strong());
                ui.label(
                    RichText::new("Sidebar name only — the directory is untouched.")
                        .color(theme.text_muted)
                        .small(),
                );
                let input_id = egui::Id::new("alacritree_rename_input");
                let edit = egui::TextEdit::singleline(&mut label)
                    .id(input_id)
                    .hint_text(dir_name.as_str())
                    .desired_width(f32::INFINITY);
                let resp = ui.add(edit);
                focus_default(ui.ctx(), input_id);
                if resp.lost_focus() && resp.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    rename_clicked = true;
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to rename · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if modal_button(ui, &theme, "Rename", theme.accent).clicked() {
                            rename_clicked = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                    });
                });
            },
        );

        if cancel_via_key || cancelled || modal.should_close() {
            return;
        }
        if confirm_via_key || rename_clicked {
            let _ = self.rename_project(&root, Some(label));
            return;
        }
        self.pending_rename = Some(RenameState { root, label });
    }

    fn show_base_branch_picker(&mut self, ctx: &Context) {
        let Some(mut picker) = self.pending_base_branch.take() else {
            return;
        };
        if let Some(job) = picker.branches_job.as_ref() {
            match job.poll() {
                Some(branches) => {
                    picker.branches = Some(branches);
                    picker.branches_job = None;
                },
                // A panicked listing never lands a result; drop the handle
                // so the picker shows the failure row instead of "loading
                // branches…" forever.
                None if job.failed() => {
                    picker.branches = Some(Err("branch listing did not complete".to_string()));
                    picker.branches_job = None;
                },
                None => {},
            }
        }
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        let (up, down) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            )
        });
        let frame = modal_frame(&theme);
        let current = self.base_branch_overrides.get(&picker.worktree).cloned();
        let s = theme.ui_scale;

        // Row 0 is always "Auto"; branch rows follow, narrowed by the query.
        // Populated inside the modal closure, after the TextEdit runs, so the
        // rows reflect this frame's query rather than the previous one.
        let mut filtered: Vec<String> = Vec::new();
        let mut chosen: Option<Option<String>> = None; // Some(None) = Auto
        let modal = egui::Modal::new(egui::Id::new("alacritree_base_branch_picker"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 4.0 * s;
                let name = picker
                    .worktree
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| wsl::display_path(&picker.worktree));
                ui.label(
                    RichText::new(format!("Base branch for `{name}`")).color(theme.text).strong(),
                );
                ui.label(
                    RichText::new("The git panel diffs this worktree against it.")
                        .color(theme.text_muted)
                        .small(),
                );
                let input_id = egui::Id::new("alacritree_base_branch_query");
                let edit = egui::TextEdit::singleline(&mut picker.query)
                    .id(input_id)
                    .hint_text("filter branches")
                    .desired_width(f32::INFINITY);
                let query_changed = ui.add(edit).changed();
                focus_default(ui.ctx(), input_id);

                if let Some(Err(e)) = &picker.branches {
                    ui.label(RichText::new(e).color(danger).small());
                }

                filtered = match &picker.branches {
                    Some(Ok(branches)) => filter_branches(branches, &picker.query),
                    Some(Err(_)) | None => Vec::new(),
                };
                picker.cursor = picker_cursor(
                    query_changed,
                    picker.query.is_empty(),
                    picker.cursor,
                    filtered.len(),
                );

                let mark = |selected: bool| if selected { "• " } else { "   " };
                egui::ScrollArea::vertical().max_height(240.0 * s).show(ui, |ui| {
                    let auto_label = match &picker.detected {
                        Some(d) => format!("{}Auto ({d})", mark(current.is_none())),
                        None => format!("{}Auto", mark(current.is_none())),
                    };
                    let auto = ui.selectable_label(picker.cursor == 0, auto_label);
                    if auto.clicked() {
                        chosen = Some(None);
                    }
                    if picker.branches.is_none() {
                        ui.add_enabled(
                            false,
                            egui::Label::new(
                                RichText::new("loading branches…").color(theme.text_muted),
                            ),
                        );
                    }
                    for (i, branch) in filtered.iter().enumerate() {
                        let selected = current.as_deref() == Some(branch.as_str());
                        let resp = ui.selectable_label(
                            picker.cursor == i + 1,
                            format!("{}{branch}", mark(selected)),
                        );
                        if resp.clicked() {
                            chosen = Some(Some(branch.clone()));
                        }
                    }
                });
                ui.label(
                    RichText::new("↑↓ move · Enter apply · Esc cancel")
                        .color(theme.text_muted)
                        .small(),
                );
            });

        if up {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
        if down {
            picker.cursor = (picker.cursor + 1).min(filtered.len());
        }
        // While the listing is pending or failed, `filtered` is empty, so
        // cursor 0 would resolve to Auto — applying it on Enter would clear
        // an existing override on a reflexive keypress rather than the no-op
        // that state should produce. Clicking Auto still works either way
        // (see `auto.clicked()` above); only the keyboard shortcut is gated.
        if confirm_via_key && matches!(picker.branches, Some(Ok(_))) {
            chosen = Some(if picker.cursor == 0 {
                None
            } else {
                filtered.get(picker.cursor - 1).cloned()
            });
        }
        if cancel_via_key || modal.should_close() {
            return;
        }
        if let Some(branch) = chosen {
            self.apply_base_branch(picker.worktree, branch);
            return;
        }
        self.pending_base_branch = Some(picker);
    }

    fn show_create_dialog(&mut self, ctx: &Context) {
        let Some(state) = self.pending_create.take() else {
            return;
        };
        let next = match state {
            CreateState::Prompt { project_idx, branch, error } => {
                self.show_create_prompt(ctx, project_idx, branch, error)
            },
            CreateState::Running { project_idx, branch, mut steps, rx, job } => {
                let mut done: Option<Result<PathBuf, String>> = None;
                while let Ok(p) = rx.try_recv() {
                    match p {
                        Progress::Step(s) => steps.push(s),
                        Progress::Done(r) => done = Some(r),
                    }
                }
                // A panicked worker sends no `Progress::Done`, so without the
                // latch the modal would sit on its last step forever.
                let _ = job.poll();
                if done.is_none() && job.failed() {
                    done = Some(Err(CREATE_WORKER_PANICKED.to_string()));
                }
                let minimized = self.show_create_running(ctx, project_idx, &branch, &steps);
                match done {
                    // A finished job goes to its result even if a minimize press
                    // lands on the same frame, so the outcome is never lost.
                    Some(result) => Some(CreateState::Done { project_idx, steps, result }),
                    // Minimized: hand the still-running create off to
                    // `poll_pending_creates` and dismiss the modal.
                    None if minimized => {
                        self.pending_creates.push(BackgroundCreate {
                            project_idx,
                            branch,
                            rx,
                            job,
                        });
                        None
                    },
                    None => Some(CreateState::Running { project_idx, branch, steps, rx, job }),
                }
            },
            CreateState::Done { project_idx, steps, result } => {
                if self.show_create_done(ctx, project_idx, &steps, &result) {
                    if let Ok(path) = &result {
                        self.refresh_project(ctx, project_idx);
                        let path = path.clone();
                        self.activate_worktree(ctx, &path);
                    }
                    None
                } else {
                    Some(CreateState::Done { project_idx, steps, result })
                }
            },
        };
        self.pending_create = next;
    }

    fn show_create_prompt(
        &mut self,
        ctx: &Context,
        project_idx: usize,
        mut branch: String,
        mut error: Option<String>,
    ) -> Option<CreateState> {
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let project_name = self.projects[project_idx].display_name().to_string();
        let default_branch = self.projects[project_idx].default_branch.clone();
        let project_root = self.projects[project_idx].root.clone();

        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        let frame = modal_frame(&theme);
        let mut create_clicked = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_create_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(
                    RichText::new(format!("New worktree in `{project_name}`"))
                        .color(theme.text)
                        .strong(),
                );
                ui.label(
                    RichText::new(match default_branch.as_deref() {
                        Some(b) => format!("Branched from origin/{b}"),
                        None => "Base branch will be resolved from origin/HEAD.".to_string(),
                    })
                    .color(theme.text_muted)
                    .small(),
                );
                let input_id = egui::Id::new("alacritree_create_input");
                let edit = egui::TextEdit::singleline(&mut branch)
                    .id(input_id)
                    .hint_text("branch name")
                    .desired_width(f32::INFINITY);
                let resp = ui.add(edit);
                focus_default(ui.ctx(), input_id);
                if resp.lost_focus() && resp.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create_clicked = true;
                }
                if let Some(e) = &error {
                    ui.label(RichText::new(e).color(danger).small());
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to create · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if modal_button(ui, &theme, "Create", theme.accent).clicked() {
                            create_clicked = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                    });
                });
            },
        );

        if cancel_via_key || cancelled || modal.should_close() {
            return None;
        }
        if confirm_via_key || create_clicked {
            // Whitespace runs become single hyphens — `some text like this` →
            // `some-text-like-this`.
            let canonical: String = branch.split_whitespace().collect::<Vec<_>>().join("-");
            if let Err(msg) = wt::validate_branch_name(&canonical) {
                error = Some(msg);
                return Some(CreateState::Prompt { project_idx, branch, error });
            }
            let base_dir = self.config.workspace.base_dir_for(&project_root);
            let req =
                CreateRequest { project_root, default_branch, branch: canonical.clone(), base_dir };
            let (rx, job) = wt::spawn_create(req, ctx.clone());
            return Some(CreateState::Running {
                project_idx,
                branch: canonical,
                steps: Vec::new(),
                rx,
                job,
            });
        }
        Some(CreateState::Prompt { project_idx, branch, error })
    }

    /// Renders the live progress view and returns `true` when the user asks to
    /// minimize (Enter, Escape, or a click outside), sending the create to the
    /// background so they can keep working.  The git operation can't be
    /// cancelled mid-flight, so every dismiss path minimizes rather than aborts.
    fn show_create_running(
        &self,
        ctx: &Context,
        project_idx: usize,
        branch: &str,
        steps: &[String],
    ) -> bool {
        let theme = self.theme;
        let project_name = self.projects[project_idx].display_name().to_string();
        let frame = modal_frame(&theme);
        let s = theme.ui_scale;
        let (minimize_via_esc, minimize_via_enter) = consume_modal_keys(ctx);
        let modal = egui::Modal::new(egui::Id::new("alacritree_create_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(
                    RichText::new(format!("Creating `{branch}` in `{project_name}`"))
                        .color(theme.text)
                        .strong(),
                );
                ui.add_space(4.0 * s);
                for (i, step) in steps.iter().enumerate() {
                    let is_last = i + 1 == steps.len();
                    let bullet_color = if is_last { theme.accent } else { theme.text_dim };
                    let text_color = if is_last { theme.text } else { theme.text_dim };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("•").color(bullet_color));
                        ui.label(RichText::new(step).color(text_color).small());
                    });
                }
                if steps.is_empty() {
                    ui.label(RichText::new("Starting…").color(theme.text_muted).small());
                }
                ui.add_space(4.0 * s);
                ui.label(
                    RichText::new("Enter to keep working while it finishes in the background")
                        .color(theme.text_muted)
                        .small(),
                );
            },
        );
        minimize_via_esc || minimize_via_enter || modal.should_close()
    }

    fn show_create_done(
        &self,
        ctx: &Context,
        project_idx: usize,
        steps: &[String],
        result: &Result<PathBuf, String>,
    ) -> bool {
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let ok = rgb_to_color32(self.config.palette.normal[2]);
        let project_name = self.projects[project_idx].display_name().to_string();
        let frame = modal_frame(&theme);
        let mut close = false;
        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_create_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                let (title, color) = match result {
                    Ok(_) => (format!("Created worktree in `{project_name}`"), ok),
                    Err(_) => ("Worktree creation failed".to_string(), danger),
                };
                ui.label(RichText::new(title).color(color).strong());
                let last = steps.len().saturating_sub(1);
                for (i, step) in steps.iter().enumerate() {
                    let failed_step = result.is_err() && i == last;
                    let bullet_color = if failed_step { danger } else { ok };
                    let text_color = if failed_step { danger } else { theme.text_dim };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("•").color(bullet_color));
                        ui.label(RichText::new(step).color(text_color).small());
                    });
                }
                if let Err(e) = result {
                    ui.add_space(4.0 * s);
                    ui.label(RichText::new(e).color(danger).small());
                }
                ui.add_space(4.0 * s);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if result.is_ok() { "Open" } else { "Close" };
                    let btn = modal_button(ui, &theme, label, theme.accent);
                    if btn.clicked() {
                        close = true;
                    }
                    focus_default(ui.ctx(), btn.id);
                });
            },
        );

        if confirm_via_key || cancel_via_key || close || modal.should_close() {
            return true;
        }
        false
    }

    fn show_quit_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let n = self.sessions.len();

        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);
        let frame = modal_frame(&theme);
        let mut quit_clicked = false;
        let mut cancel_clicked = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_quit_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(320.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new("Quit alacritree?").color(theme.text).strong());
                let msg = match n {
                    0 => "No sessions running.".to_string(),
                    1 => "1 session will be terminated.".to_string(),
                    n => format!("{n} sessions will be terminated."),
                };
                ui.label(RichText::new(msg).color(theme.text_muted).small());
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to quit · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let quit = modal_button(ui, &theme, "Quit", danger);
                        if quit.clicked() {
                            quit_clicked = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancel_clicked = true;
                        }
                        focus_default(ui.ctx(), quit.id);
                    });
                });
            },
        );

        if confirm_via_key || quit_clicked {
            self.quit_dialog_open = false;
            crash_log::record_reason(ExitReason::UserQuit);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cancel_via_key || cancel_clicked || modal.should_close() {
            self.quit_dialog_open = false;
        }
    }
}

/// IPC request handling.  Runs on the UI thread inside `update` so every
/// request sees (and mutates) app state the same way user input does; the
/// connection thread blocks on `reply_tx` meanwhile.
impl AlacritreeApp {
    fn process_ipc_calls(&mut self, ctx: &Context) {
        let Some(rx) = &self.ipc_rx else { return };
        let calls: Vec<ipc::AppCall> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        for call in calls {
            let ipc::AppCall { request, reply_tx } = call;
            // Discovery is far too slow to run here, and the caller still has
            // to be answered from the discovered list rather than the stale
            // one (or the placeholder), so these requests own their reply
            // channel until it lands.
            let request = match request {
                ipc::IpcRequest::RefreshProject { root } => {
                    self.defer_project_refresh(ctx, root, reply_tx);
                    continue;
                },
                ipc::IpcRequest::AddProject { path } => {
                    self.defer_project_add(ctx, path, reply_tx);
                    continue;
                },
                // The reply has to wait for the PTY: a client that creates a
                // session in order to write to it would otherwise be told the
                // id before anything can receive what it writes.
                ipc::IpcRequest::CreateSession { workspace } => {
                    self.defer_create_session(ctx, workspace, reply_tx);
                    continue;
                },
                other => other,
            };
            let name = request.name();
            let started = std::time::Instant::now();
            let result = self.handle_ipc_request(ctx, request);
            crate::frame_log::note_if_slow("ipc request", name, started.elapsed());
            // A send error means the client gave up waiting — nothing to do.
            let _ = reply_tx.send(result);
        }
    }

    fn defer_project_refresh(
        &mut self,
        ctx: &Context,
        root: PathBuf,
        reply_tx: mpsc::Sender<ipc::IpcResult>,
    ) {
        let Some(idx) = self.projects.iter().position(|p| p.root == root) else {
            let _ =
                reply_tx.send(Err(format!("{} is not a project in the sidebar", root.display())));
            return;
        };
        self.refresh_project(ctx, idx);
        if let Some(reply_tx) = self.project_refreshes.watch(&root, reply_tx) {
            let _ = reply_tx.send(Ok(project_json(&self.projects[idx])));
        }
    }

    /// A project that is already in the sidebar is answered from the list as
    /// it stands; a new one goes in as a placeholder and its discovery runs on
    /// a worker, so the reply waits for that rather than describing worktrees
    /// nothing has looked for yet.
    fn defer_project_add(
        &mut self,
        ctx: &Context,
        path: PathBuf,
        reply_tx: mpsc::Sender<ipc::IpcResult>,
    ) {
        self.add_project_off_thread(ctx, path.clone());
        let Some(idx) = self.projects.iter().position(|p| p.root == path) else {
            let _ = reply_tx.send(Err(format!("{} could not be added", path.display())));
            return;
        };
        if let Some(reply_tx) = self.project_refreshes.watch(&path, reply_tx) {
            let _ = reply_tx.send(Ok(project_json(&self.projects[idx])));
        }
    }

    fn defer_create_session(
        &mut self,
        ctx: &Context,
        workspace: Option<PathBuf>,
        reply_tx: mpsc::Sender<ipc::IpcResult>,
    ) {
        let workspace = match workspace {
            None => None,
            Some(p) => match self.known_worktree_path(&p) {
                Some(known) => Some(known),
                None => {
                    let _ = reply_tx.send(Err(unknown_worktree(&p)));
                    return;
                },
            },
        };
        let id = match self.spawn_session(ctx, workspace) {
            Ok(id) => id,
            // `defer_create_session` answers the client itself, so a failure
            // the frame can still see has to be sent rather than returned.
            Err(e) => {
                let _ = reply_tx.send(Err(format!("failed to spawn shell: {e}")));
                return;
            },
        };
        // Nothing is opening for this id when the gate is off, since
        // `spawn_session` attaches inline before returning: `watch` hands
        // the channel straight back and it is answered the same way the
        // gate-off path answers it.
        if let Some(reply_tx) = self.pending_spawns.watch(id, reply_tx) {
            let _ = reply_tx.send(Ok(json!({ "session_id": id })));
        }
    }

    fn handle_ipc_request(&mut self, ctx: &Context, request: ipc::IpcRequest) -> ipc::IpcResult {
        use ipc::IpcRequest as Req;
        match request {
            Req::ListProjects => Ok(json!({
                "current_workspace": self.current_workspace,
                "projects": self.projects.iter().map(project_json).collect::<Vec<_>>(),
            })),
            Req::ListSessions => {
                let sessions: Vec<Value> = self
                    .sessions
                    .iter()
                    .map(|s| {
                        let active =
                            self.active_session.get(&s.working_directory).copied() == Some(s.id);
                        session_json(s, active)
                    })
                    .collect();
                Ok(json!({ "current_workspace": self.current_workspace, "sessions": sessions }))
            },
            Req::SelectWorkspace { path } => match path {
                None => {
                    self.activate_home(ctx);
                    Ok(json!({ "workspace": Value::Null }))
                },
                Some(p) => {
                    let known = self.known_worktree_path(&p).ok_or_else(|| unknown_worktree(&p))?;
                    self.activate_worktree(ctx, &known);
                    Ok(json!({ "workspace": known }))
                },
            },
            // Claimed by `process_ipc_calls` before dispatch: the reply is
            // held until the session's PTY is live, which needs the reply
            // channel this method does not have.
            Req::CreateSession { .. } => Err("create_session was not deferred".to_string()),
            Req::CloseSession { session_id } => {
                if !self.sessions.iter().any(|s| s.id == session_id) {
                    return Err(format!("no session with id {session_id}"));
                }
                self.close_session(ctx, session_id);
                Ok(json!({ "closed": session_id }))
            },
            Req::MoveSession { session_id, path } => {
                let target =
                    self.workspace_for_path(&path).ok_or_else(|| unknown_worktree(&path))?;
                let workspace = self.move_session_to_key(session_id, Some(target))?;
                // A silent re-grouping produces no PTY events, so nothing
                // else would wake the next paint.
                ctx.request_repaint();
                Ok(json!({ "session_id": session_id, "workspace": workspace }))
            },
            Req::SendText { session_id, text } => {
                let idx = self
                    .sessions
                    .iter()
                    .position(|s| s.id == session_id)
                    .ok_or_else(|| format!("no session with id {session_id}"))?;
                let written = text.len();
                if let Some(editor) = self.sessions[idx].scratchpad.as_mut() {
                    editor.insert_at_cursor(ctx, session_id, &text);
                } else {
                    let session = &mut self.sessions[idx];
                    paste::on_terminal_input_start(session);
                    session.write(text.into_bytes());
                }
                Ok(json!({ "bytes_written": written }))
            },
            Req::ReadScreen { session_id, scrollback_lines } => {
                let session = self
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .ok_or_else(|| format!("no session with id {session_id}"))?;
                let snapshot = session.screen_snapshot(scrollback_lines);
                Ok(json!({
                    "title": session.title,
                    "lines": snapshot.lines,
                    "cursor": { "line": snapshot.cursor_line, "column": snapshot.cursor_column },
                    "scrollback_available": snapshot.history_size,
                }))
            },
            Req::ReadScratchpad { workspace } => {
                let workspace = match workspace.as_deref() {
                    None | Some("current") => self.current_workspace.clone(),
                    Some("home") => None,
                    Some(path) => Some(
                        self.known_worktree_path(Path::new(path))
                            .ok_or_else(|| unknown_worktree(Path::new(path)))?,
                    ),
                };
                scratchpad::read_json(&workspace)
            },
            // Claimed by `process_ipc_calls` before dispatch: the reply is
            // held until the background discovery lands, which needs the
            // reply channel this method does not have.
            Req::RefreshProject { .. } | Req::AddProject { .. } => {
                Err("discovery was not deferred".to_string())
            },
            Req::RemoveProject { root } => {
                let idx =
                    self.projects.iter().position(|p| p.root == root).ok_or_else(|| {
                        format!("{} is not a project in the sidebar", root.display())
                    })?;
                Ok(json!({ "removed": self.remove_project(idx) }))
            },
            Req::RenameProject { root, label } => {
                let idx = self.rename_project(&root, label)?;
                Ok(project_json(&self.projects[idx]))
            },
            Req::RunAction { action } => match crate::bindings::parse_action(&action) {
                BindingAction::Unsupported(name) => Err(format!("unknown action `{name}`")),
                parsed => {
                    self.dispatch_action(ctx, parsed, ActionOrigin::Ipc);
                    Ok(json!({ "action": action }))
                },
            },
            // Dispatched on the IPC connection thread; never forwarded here.
            Req::GitStatus { .. } | Req::CreateWorktree { .. } => {
                Err("request is handled off the UI thread".to_string())
            },
        }
    }

    /// Resolve `path` to a sidebar worktree, tolerating symlinks and trailing
    /// slashes via canonicalization.
    fn known_worktree_path(&self, path: &Path) -> Option<PathBuf> {
        let canonical = path.canonicalize().ok();
        self.projects.iter().flat_map(|p| &p.worktrees).find_map(|wt| {
            (wt.path == path || canonical.as_deref() == Some(wt.path.as_path()))
                .then(|| wt.path.clone())
        })
    }

    /// Like [`Self::known_worktree_path`], but a path anywhere *inside* a
    /// worktree's subtree counts — a mover reports its cwd, which is usually
    /// a subdirectory, not the worktree root itself.
    fn workspace_for_path(&self, path: &Path) -> Option<PathBuf> {
        let worktrees: Vec<PathBuf> =
            self.projects.iter().flat_map(|p| &p.worktrees).map(|wt| wt.path.clone()).collect();
        owning_worktree(&worktrees, path)
            .or_else(|| path.canonicalize().ok().and_then(|c| owning_worktree(&worktrees, &c)))
    }
}

/// The known worktree that owns `path`: the longest worktree path that
/// `path` equals or descends from.  Longest wins so a worktree nested under
/// another checkout resolves to the inner one.
fn owning_worktree(worktrees: &[PathBuf], path: &Path) -> Option<PathBuf> {
    worktrees
        .iter()
        .filter(|wt| path.starts_with(wt))
        .max_by_key(|wt| wt.components().count())
        .cloned()
}

fn unknown_worktree(path: &Path) -> String {
    format!("{} is not a worktree in the sidebar — see list_projects", path.display())
}

fn session_json(session: &Session, is_active_tab: bool) -> Value {
    json!({
        "id": session.id,
        "title": session.title,
        "workspace": session.working_directory,
        "kind": match &session.kind {
            SessionKind::Shell => "shell",
            SessionKind::Diff { .. } => "diff",
            SessionKind::Scratchpad { .. } => "scratchpad",
        },
        "columns": session.size.columns,
        "lines": session.size.screen_lines,
        "is_active_tab": is_active_tab,
        "needs_attention": session.needs_attention,
    })
}

impl eframe::App for AlacritreeApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // This clear is the only thing painting a cell the grid leaves alone,
        // so it has to carry the terminal's own background rather than the
        // configured one.  eframe reads it before `update`, so a colour OSC 11
        // moved this frame lands next frame; terminal output requests a repaint
        // of its own, so the stale frame is replaced rather than left up.
        let bg = self.grid_snapshot.default_bg(&self.config.palette);
        // Deliberately not premultiplied, where alacritty's `renderer::clear`
        // writes `(rgb * alpha, alpha)`.  `egui_glow::clear` hands these to
        // `glClearColor` untouched and the compositor reads the framebuffer as
        // premultiplied, so a translucent window carries its background at full
        // strength; scaling it here would darken every `[window] opacity`
        // already tuned against this.
        let n = |c: u8| c as f32 / 255.0;
        [n(bg.r()), n(bg.g()), n(bg.b()), self.config.window.opacity]
    }

    fn update(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        let frame_started = self
            .frame_log
            .as_ref()
            .map(|_| (std::time::Instant::now(), crate::frame_log::output_wait()));
        self.grid_paint = std::time::Duration::ZERO;
        if ctx.input(|i| !i.events.is_empty()) {
            self.last_input = Instant::now();
        }
        self.phases.restart();
        self.glyph_cache.begin_frame(ctx);
        // The latch is what makes this safe to run on every close path: a quit
        // through the dialog has already recorded `user-quit`, and on Windows a
        // session end has already recorded its own reason, so this only ever
        // fires for a close nothing else explained.
        if ctx.input(|i| i.viewport().close_requested()) {
            crash_log::record_reason(ExitReason::WindowClosed);
        }
        self.poll_project_refreshes();
        self.poll_pending_spawns(ctx);
        self.poll_herdr_attach(ctx);
        self.sync_herdr_view_focus();
        // Unconditional: either sidebar can be hidden, and a drain hung off one
        // of them would strand every entry the other polled.
        self.pr_cache.drain_completed(ctx);
        self.poll_pending_deletes(ctx);
        self.poll_pending_creates(ctx);
        self.poll_herdr_endpoints();
        // Poll first, then check `failed`: a panicked job's `poll` returns
        // `None` forever, so `failed` is what stops its handle from sitting
        // here for the rest of the process.
        self.detached_jobs.retain(|job| match job.poll() {
            Some(()) => false,
            None => !job.failed(),
        });
        self.phases.mark("polls");
        let modal_open = self.is_modal_open();
        // Keys pressed mid-composition drive the IME's candidate window,
        // not the app — alacritty's key_input returns early the same way,
        // above binding dispatch.
        if !modal_open && self.ime.preedit().is_none() {
            // While the command palette is open it owns every key: neither the
            // sidebar filters nor the app bindings run, so typing into it never
            // leaks an action.  The palette consumes its own keys (Ctrl+K to
            // close included) when it paints below.
            if !self.palette.is_open() {
                match self.focus {
                    PaneFocus::ProjectsSidebar => self.handle_sidebar_nav(ctx),
                    PaneFocus::GitSidebar => self.handle_git_sidebar_nav(ctx),
                    PaneFocus::Terminal => {},
                }
                self.phases.mark("sidebar-nav");
                self.handle_shortcuts(ctx);
            }
        }
        self.phases.mark("shortcuts");
        self.process_notification_actions(ctx);
        self.process_ipc_calls(ctx);
        self.phases.mark("ipc");
        self.process_session_events(ctx);
        self.phases.mark("session-events");
        self.reconcile_sidebar_focus(ctx);
        self.phases.mark("focus");
        let theme = self.theme;
        // GL clear is the sole source of the bg when opacity < 1; painting any
        // panel fill on top would compound the alpha through egui's blend.
        let translucent = self.config.window.opacity < 1.0;
        let sidebar_fill = if translucent { Color32::TRANSPARENT } else { theme.sidebar_bg };
        // Opaque, this fill is what a collapsed cell shows, so it tracks the
        // terminal's background for the same reason the clear does.
        let terminal_bg = self.grid_snapshot.default_bg(&self.config.palette);
        let central_fill = if translucent { Color32::TRANSPARENT } else { terminal_bg };

        let panel_frame = Frame::default().fill(sidebar_fill).inner_margin(Margin::same(8));

        let mut sidebar_rect = None;
        if self.show_left_sidebar {
            let r = self.show_project_sidebar(ctx, panel_frame.clone());
            paint_panel_border(ctx, r.right(), r.y_range(), theme.sidebar_border);
            if theme.focus_outline.sidebar
                && !modal_open
                && self.focus == PaneFocus::ProjectsSidebar
            {
                paint_focus_outline(ctx, r, &theme);
            }
            sidebar_rect = Some(r);
        }

        self.phases.mark("projects-sidebar");

        if self.show_right_sidebar {
            let r = self.show_git_sidebar(ctx, panel_frame);
            paint_panel_border(ctx, r.left(), r.y_range(), theme.sidebar_border);
            if theme.focus_outline.sidebar && !modal_open && self.focus == PaneFocus::GitSidebar {
                paint_focus_outline(ctx, r, &theme);
            }
        }
        self.phases.mark("git-sidebar");

        let central = egui::CentralPanel::default()
            .frame(Frame::default().fill(central_fill).inner_margin(Margin::same(0)))
            .show(ctx, |ui| {
                self.show_tab_strip(ui);

                if self.active_session_index().is_none() {
                    self.adopt_active_session();
                }

                let Some(idx) = self.active_session_index() else {
                    // A preedit can only be finalized or cancelled by the terminal
                    // view's event drain, so without a session view to run it the
                    // preedit would go stale and keep shortcuts suppressed forever.
                    self.ime.clear();
                    ui.label(
                        RichText::new("no session — Ctrl+T to open one").color(theme.text_dim),
                    );
                    return;
                };
                let editor_text = rgb_to_color32(self.config.palette.fg);
                let editor_hint = blend_toward(editor_text, theme.terminal_bg, 0.55);
                let editor_error = rgb_to_color32(self.config.palette.normal[1]);
                let session = &mut self.sessions[idx];
                let allow_focus =
                    !modal_open && !self.palette.is_open() && self.focus == PaneFocus::Terminal;
                let response = if let Some(editor) = session.scratchpad.as_mut() {
                    self.ime.clear();
                    scratchpad::show_editor(
                        ui,
                        session.id,
                        editor,
                        allow_focus,
                        theme.ui_scale,
                        editor_text,
                        editor_hint,
                        editor_error,
                    )
                } else {
                    let started = std::time::Instant::now();
                    let response = terminal_view::show(
                        ui,
                        session,
                        &self.config,
                        &self.face_metrics,
                        allow_focus,
                        &mut self.builtin_glyphs,
                        &mut self.ime,
                        &mut self.color_glyphs,
                        &mut self.glyph_cache,
                        &mut self.grid_snapshot,
                        Some(&self.gpu_grid),
                        &mut self.detached_jobs,
                    );
                    self.grid_paint += started.elapsed();
                    self.last_pane_geometry = Some((session.size, session.cell_size));
                    response
                };
                // egui fake-clicks the natively focused widget on Space/Enter,
                // and the terminal keeps native focus while the sidebar owns
                // app focus — so keyboard "clicks" must not steal it back.
                if response.clicked_by(egui::PointerButton::Primary)
                    && self.focus != PaneFocus::Terminal
                {
                    self.focus_terminal();
                }
            });
        if theme.focus_outline.terminal && !modal_open && self.focus == PaneFocus::Terminal {
            paint_focus_outline(ctx, central.response.rect, &theme);
        }

        // A modal or the palette owns input while it is up; a drop landing
        // behind one would act on a surface the user cannot see.
        if !modal_open && !self.palette.is_open() {
            let regions =
                file_drop::Regions::new(sidebar_rect, central.response.rect, &self.config.ui.drop);
            self.paint_drop_hover(ctx, &regions);
            self.handle_dropped_files(ctx, &regions);
        }
        self.phases.mark("central");

        if self.pending_create.is_some() {
            self.show_create_dialog(ctx);
        }
        if self.pending_delete.is_some() {
            self.show_delete_dialog(ctx);
        }
        if self.pending_session_close.is_some() {
            self.show_close_session_dialog(ctx);
        }
        if self.pending_rename.is_some() {
            self.show_rename_dialog(ctx);
        }
        if self.pending_base_branch.is_some() {
            self.show_base_branch_picker(ctx);
        }
        if self.pending_project_remove.is_some() {
            self.show_remove_project_dialog(ctx);
        }
        if self.error_dialog.is_some() {
            self.show_error_dialog(ctx);
        }
        if self.quit_dialog_open {
            self.show_quit_dialog(ctx);
        }
        if self.palette.is_open() && !modal_open {
            self.show_command_palette(ctx);
        }
        self.phases.mark("dialogs");

        self.reap_exited_sessions(ctx);
        // A shell that exited on its own is only removed here, after paint.
        // Without this pass its deferred verdict would wait for unrelated
        // input; with it, the repair is queued for the frame the repaint
        // request has already scheduled.
        self.reconcile_sidebar_focus(ctx);
        self.phases.mark("reap");
        self.phases.report_if_slow();

        if let (Some(log), Some((started, waited))) = (self.frame_log.as_mut(), frame_started) {
            log.record(crate::frame_log::Timings {
                started,
                grid: self.grid_paint,
                cpu: frame.info().cpu_usage.map(std::time::Duration::from_secs_f32),
                waited,
                echo: crate::frame_log::echo(),
            });
        }
    }
}

/// Drain every queued notification click, keeping only the newest.  Clicks
/// can pile up while the window is unfocused; the user most likely meant
/// the latest one.
fn latest_notification_click(rx: &Receiver<SessionId>) -> Option<SessionId> {
    let mut latest = None;
    while let Ok(id) = rx.try_recv() {
        latest = Some(id);
    }
    latest
}

/// Spawn a throwaway thread so the platform notifier's synchronous calls
/// don't stall the egui paint loop.  The thread posts the session's id back
/// through `NOTIFY_TX` when the user clicks the notification.
fn notify_attention(session: &Session, ctx: &egui::Context) {
    let where_label = session
        .working_directory
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| session.title.clone());
    let body = if where_label.is_empty() {
        "Session is waiting for input".to_string()
    } else {
        format!("{where_label} is waiting for input")
    };
    let id = session.id;
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("alacritree-notify".into())
        .spawn(move || notify_worker(body, id, ctx))
        .ok();
}

/// Deliver a clicked notification's session id to the UI thread.
pub(crate) fn notify_click(id: SessionId, ctx: &egui::Context) {
    if let Some(lock) = NOTIFY_TX.get() {
        if let Ok(tx) = lock.lock() {
            let _ = tx.send(id);
            ctx.request_repaint();
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn notify_worker(body: String, id: SessionId, ctx: egui::Context) {
    // `default` is the action id freedesktop notifiers fire on body-click.
    let result = notify_rust::Notification::new()
        .summary("alacritree")
        .body(&body)
        .action("default", "Open")
        .show();
    let handle = match result {
        Ok(h) => h,
        Err(e) => {
            log::debug!("desktop notification failed: {e}");
            return;
        },
    };
    handle.wait_for_action(|action| {
        if action == "__closed" {
            return;
        }
        notify_click(id, &ctx);
    });
}

#[cfg(windows)]
fn notify_worker(body: String, id: SessionId, ctx: egui::Context) {
    use tauri_winrt_notification::Toast;
    // notify-rust doesn't surface WinRT activation, so drive its own backend
    // crate directly.  `show` returns immediately; the WinRT runtime holds
    // the activation handler, so this worker thread can exit right away.
    let result = Toast::new(Toast::POWERSHELL_APP_ID)
        .title("alacritree")
        .text1(&body)
        .on_activated(move |_action| {
            notify_click(id, &ctx);
            Ok(())
        })
        .show();
    if let Err(e) = result {
        log::debug!("desktop notification failed: {e}");
    }
}

#[cfg(target_os = "macos")]
fn notify_worker(body: String, id: SessionId, _ctx: egui::Context) {
    // Clicks come back through the UNUserNotificationCenter delegate that
    // `notify_macos::init` installed, not through this worker.
    crate::notify_macos::notify(&body, id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(p: &str) -> WorkspaceKey {
        Some(PathBuf::from(p))
    }

    #[test]
    fn dirty_warning_under_force_never_goes_silent() {
        // The exact regression this fixes: a forced retry with no counts at
        // all (the request was confirmed before its probe landed, which
        // cancelled the probe) must still tell the user `--force` discards
        // work, not silently drop the warning.
        let message = dirty_warning(None, true, false).expect("a forced confirm always warns");
        assert!(message.contains("--force"));

        // A stale-clean read carried into the retry must not read as "safe"
        // either -- the retry only exists because git already refused this
        // exact tree as dirty.
        let clean = DirtyCounts::default();
        let message =
            dirty_warning(Some(&clean), true, false).expect("a forced confirm always warns");
        assert!(message.contains("--force"));
    }

    #[test]
    fn dirty_warning_stays_quiet_for_a_known_clean_unforced_tree() {
        let clean = DirtyCounts::default();
        assert_eq!(dirty_warning(Some(&clean), false, false), None);
    }

    #[test]
    fn dirty_warning_distinguishes_checking_from_unavailable() {
        let checking = dirty_warning(None, false, true).expect("still checking");
        assert!(checking.to_lowercase().contains("checking"));
        let unavailable = dirty_warning(None, false, false).expect("probe failed or was skipped");
        assert!(!unavailable.to_lowercase().contains("checking"));
    }

    #[test]
    fn refused_for_unsaved_work_matches_a_real_git_refusal() {
        let message = "git worktree remove ../wt1: fatal: '../wt1' contains modified or untracked \
                       files, use --force to delete it";
        assert!(refused_for_unsaved_work(message));
    }

    #[test]
    fn refused_for_unsaved_work_ignores_unrelated_failures() {
        assert!(!refused_for_unsaved_work(
            "git worktree remove ../wt1: fatal: '../wt1' is a main working tree"
        ));
    }

    /// A worktree path that happens to contain the matched phrase must not
    /// turn an unrelated failure into a false "needs --force" prompt --
    /// `refused_for_unsaved_work` only reads the text after the closing
    /// quote of the path, never the quoted path itself.
    #[test]
    fn refused_for_unsaved_work_is_not_fooled_by_a_path_spelling_out_the_phrase() {
        let path = "../is dirty, use --force to delete it";
        let message = format!(
            "git worktree remove {path}: fatal: '{path}' cannot be locked: filesystem error"
        );
        assert!(!refused_for_unsaved_work(&message));
    }

    #[test]
    fn spawn_geometry_prefers_the_active_session_over_the_last_painted_pane() {
        let active = Some(ActiveGeometry {
            size: TermSize::new(120, 40),
            cell_size: (9.0, 18.0),
            is_scratchpad: false,
        });
        let last_pane = Some((TermSize::new(80, 24), (8.0, 16.0)));

        let (size, cell_size) = spawn_geometry(active, last_pane);

        assert_eq!((size.columns, size.screen_lines), (120, 40));
        assert_eq!(cell_size, (9.0, 18.0));
    }

    #[test]
    fn an_active_scratchpad_does_not_shadow_the_last_painted_pane() {
        // The size every scratchpad keeps for its whole life.
        let active = Some(ActiveGeometry {
            size: TermSize::new(80, 24),
            cell_size: (8.0, 16.0),
            is_scratchpad: true,
        });
        let last_pane = Some((TermSize::new(120, 40), (9.0, 18.0)));

        let (size, cell_size) = spawn_geometry(active, last_pane);

        assert_eq!((size.columns, size.screen_lines), (120, 40));
        assert_eq!(cell_size, (9.0, 18.0));
    }

    #[test]
    fn spawn_geometry_falls_back_to_the_last_painted_pane_without_an_active_session() {
        let last_pane = Some((TermSize::new(120, 40), (9.0, 18.0)));

        let (size, cell_size) = spawn_geometry(None, last_pane);

        assert_eq!((size.columns, size.screen_lines), (120, 40));
        assert_eq!(cell_size, (9.0, 18.0));
    }

    #[test]
    fn spawn_geometry_falls_back_to_80x24_before_anything_has_painted() {
        let (size, cell_size) = spawn_geometry(None, None);

        assert_eq!((size.columns, size.screen_lines), (80, 24));
        assert_eq!(cell_size, (8.0, 16.0));
    }

    #[test]
    fn the_visible_session_holds_the_self_boost_while_its_pty_is_still_opening() {
        // Nothing to raise yet, so `set_priority_boost` answered false.
        let visible = SessionBoost { raised: false, visible: true, pending: true };

        assert!(holds_self_boost(visible));
    }

    #[test]
    fn a_background_session_still_opening_its_pty_holds_no_self_boost() {
        let background = SessionBoost { raised: false, visible: false, pending: true };

        assert!(!holds_self_boost(background));
    }

    #[test]
    fn a_session_whose_job_took_the_boost_holds_it_wherever_it_sits() {
        let background = SessionBoost { raised: true, visible: false, pending: false };

        assert!(holds_self_boost(background));
    }

    #[test]
    fn a_frame_whose_visible_session_is_still_pending_leaves_the_self_boost_where_it_was() {
        let frame = [
            SessionBoost { raised: false, visible: false, pending: false },
            // On screen, its PTY still opening: no job exists to answer for
            // it, and the boost must survive the gap until one does.
            SessionBoost { raised: false, visible: true, pending: true },
            SessionBoost { raised: false, visible: false, pending: true },
        ];

        assert!(frame_holds_self_boost(frame.into_iter()));
    }

    #[test]
    fn a_frame_of_idle_background_sessions_drops_the_self_boost() {
        let frame = [
            SessionBoost { raised: false, visible: false, pending: false },
            SessionBoost { raised: false, visible: false, pending: true },
        ];

        assert!(!frame_holds_self_boost(frame.into_iter()));
    }

    #[test]
    fn a_grey_worktree_only_stays_in_the_workspace_ring_while_it_holds_sessions() {
        let wt = Worktree {
            name: "gone".into(),
            path: PathBuf::from("/repo-worktrees/gone"),
            branch: Some("feature".into()),
            is_main: false,
            prunable: false,
            upstream: None,
        };

        assert!(!worktree_is_switchable(&wt, Some(true), false));
        assert!(worktree_is_switchable(&wt, Some(true), true));
    }

    #[test]
    fn a_main_checkout_never_looks_prunable_from_the_row_probe() {
        let wt = Worktree {
            name: "main".into(),
            path: PathBuf::from("/plain-project"),
            branch: None,
            is_main: true,
            prunable: false,
            upstream: None,
        };

        assert!(!worktree_looks_gone(&wt, Some(true)));
    }

    /// Apply `move_target` to a concrete list so the drag semantics (drop
    /// above/below a row, no-op on self and neighbors) are legible.
    fn moved(items: &[&str], from: usize, insert_before: usize) -> Vec<String> {
        let mut v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        if let Some(to) = move_target(v.len(), from, insert_before) {
            let it = v.remove(from);
            v.insert(to, it);
        }
        v
    }

    /// Apply `walk_swaps` to a concrete list, with `indices` standing in for
    /// the absolute slots one workspace occupies inside the session vector.
    fn walked(items: &[&str], indices: &[usize], j: usize, position: usize) -> Vec<String> {
        let mut v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        for (a, b) in walk_swaps(indices, j, position) {
            v.swap(a, b);
        }
        v
    }

    fn key_ev(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn text_pairing_marks_only_keys_followed_by_text() {
        let events = vec![
            key_ev(egui::Key::A, true),
            egui::Event::Text("a".into()),
            key_ev(egui::Key::Enter, true),
            key_ev(egui::Key::B, true),
            egui::Event::Text("b".into()),
        ];
        assert_eq!(keys_paired_with_text(&events), vec![true, false, false, true, false]);
    }

    #[test]
    fn text_pairing_ignores_released_keys_and_orphan_text() {
        let events = vec![
            key_ev(egui::Key::A, false),
            egui::Event::Text("a".into()),
            egui::Event::Text("pasted".into()),
        ];
        assert_eq!(keys_paired_with_text(&events), vec![false, false, false]);
    }

    /// Two presses sharing one `(key, modifiers)` in a frame: only the occurrence
    /// actually followed by text is marked. A set keyed by value would mark both.
    #[test]
    fn text_pairing_is_per_occurrence_not_per_trigger() {
        let events = vec![
            key_ev(egui::Key::A, true),
            egui::Event::Text("a".into()),
            key_ev(egui::Key::A, true),
        ];
        assert_eq!(keys_paired_with_text(&events), vec![true, false, false]);
    }

    fn searching_filter() -> PanelFilter {
        let mut f = PanelFilter::new(&['s', 'a']);
        f.on_text("/");
        f
    }

    #[test]
    fn the_sentinel_sees_a_same_workspace_session_switch() {
        let written =
            SidebarFocusWrite { cursor: Some(SidebarRow::Home), workspace: None, active: Some(1) };
        let written = Some(written);

        // The reconciler's own values still stand.
        assert!(!sidebar_focus_overtaken(&written, Some(&SidebarRow::Home), &None, Some(1)));

        // Any action that switches sessions without leaving the workspace —
        // SelectNextTab, SelectNextSession, SelectTab(n) — changes neither the
        // cursor nor the workspace, only the active session.
        assert!(sidebar_focus_overtaken(&written, Some(&SidebarRow::Home), &None, Some(2)));

        // A different workspace, and a different cursor, each count too.
        assert!(sidebar_focus_overtaken(
            &written,
            Some(&SidebarRow::Home),
            &Some(PathBuf::from("/a/wt1")),
            Some(1),
        ));
        assert!(sidebar_focus_overtaken(
            &written,
            Some(&SidebarRow::Project(PathBuf::from("/a"))),
            &None,
            Some(1),
        ));

        // Nothing written yet cannot have been overtaken.
        assert!(!sidebar_focus_overtaken(&None, Some(&SidebarRow::Home), &None, Some(1)));
    }

    #[test]
    fn search_enter_escape_and_shift_escape_dispatch_distinct_actions() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = searching_filter();
        f.on_text("foo");

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Enter,
            egui::Modifiers::NONE,
            false,
        );
        assert!(!retain, "a matched search action consumes the key");
        assert!(matches!(steps.as_slice(), [SidebarNavStep::SearchAction(
            NamedAction::SidebarSearchConfirm
        )]));
        // The filter is untouched by the drain — the action does the exit.
        assert_eq!(f.mode(), panel_filter::Mode::Search);
        assert_eq!(f.query(), "foo");

        let mut steps = Vec::new();
        drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Escape,
            egui::Modifiers::NONE,
            false,
        );
        assert!(matches!(steps.as_slice(), [SidebarNavStep::SearchAction(
            NamedAction::SidebarSearchCancel
        )]));

        let mut steps = Vec::new();
        drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Escape,
            egui::Modifiers::SHIFT,
            false,
        );
        assert!(
            matches!(steps.as_slice(), [SidebarNavStep::SearchAction(
                NamedAction::SidebarSearchCancelToTerminal
            )]),
            "Shift+Esc is a distinct search action from plain Esc"
        );
    }

    #[test]
    fn search_arrows_move_cursor_and_space_is_swallowed() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = searching_filter();

        let mut steps = Vec::new();
        drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::ArrowDown,
            egui::Modifiers::NONE,
            false,
        );
        assert!(matches!(steps.as_slice(), [SidebarNavStep::Filter(
            panel_filter::Outcome::MoveCursor(1)
        )]));

        // Space stays consumed as a no-op nav even in search (fake-click guard).
        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Space,
            egui::Modifiers::NONE,
            false,
        );
        assert!(!retain);
        assert!(matches!(steps.as_slice(), [SidebarNavStep::Nav(egui::Key::Space)]));
    }

    #[test]
    fn browsing_enter_navigates_and_modified_keys_fall_through() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = PanelFilter::new(&['s', 'a']); // browsing

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Enter,
            egui::Modifiers::NONE,
            false,
        );
        assert!(!retain, "Enter in browsing is a nav activate, consumed here");
        assert!(matches!(steps.as_slice(), [SidebarNavStep::Nav(egui::Key::Enter)]));

        // A modifier-bound key is left for handle_shortcuts.
        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Enter,
            egui::Modifiers::CTRL,
            false,
        );
        assert!(retain);
        assert!(steps.is_empty());
    }

    #[test]
    fn search_enter_with_no_binding_falls_through_without_activating() {
        // User freed Enter (no search binding): it must not hard-fire browsing
        // activate — it falls through for the terminal/shortcuts instead.
        let binds: Vec<crate::bindings::KeyBinding> = Vec::new();
        let mut f = searching_filter();

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Enter,
            egui::Modifiers::NONE,
            false,
        );
        assert!(retain, "an unbound Enter in search is retained, not consumed as nav");
        assert!(steps.is_empty());
    }

    /// A text-producing key in search mode is query input and must not also run a
    /// binding — including one bound to a search action, since text input is
    /// unconditional.
    #[test]
    fn a_text_key_in_search_is_consumed_before_any_binding() {
        let binds = crate::bindings::parse_bindings(vec![crate::bindings::RawBinding {
            key: "G".into(),
            mods: None,
            mode: None,
            chars: None,
            action: Some("SidebarSearchConfirm".into()),
            command: None,
        }]);
        let mut f = searching_filter();

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::G,
            egui::Modifiers::NONE,
            true,
        );

        assert!(!retain, "a key carrying query text is consumed");
        assert!(steps.is_empty(), "and dispatches nothing, not even a search action");
    }

    /// Shift+letter still produces text, so it must be consumed too. The modifier
    /// early-return would otherwise let the built-in Shift+R reach RenameSelected.
    #[test]
    fn shift_letter_in_search_is_consumed() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = searching_filter();

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::R,
            egui::Modifiers::SHIFT,
            true,
        );

        assert!(!retain);
        assert!(steps.is_empty());
    }

    /// Bare Delete carries no text, so the pairing rule cannot claim it. It is a
    /// search-box editing key, so it is consumed as a no-op instead of reaching the
    /// cursored row.
    #[test]
    fn bare_delete_in_search_is_consumed_as_a_no_op() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = searching_filter();
        f.on_text("typed");

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::Delete,
            egui::Modifiers::NONE,
            false,
        );

        assert!(!retain, "Delete must not reach the cursored row");
        assert!(
            matches!(steps.as_slice(), [SidebarNavStep::Nav(egui::Key::Delete)]),
            "Delete is consumed as a plain nav key, not routed into the filter"
        );
        assert_eq!(f.query(), "typed", "an append-only query has no delete");
    }

    /// Keys that produce no text keep falling through to the binding table, which
    /// is what lets Home/End/PageUp/PageDown navigate filtered results.
    #[test]
    fn non_text_keys_in_search_still_fall_through() {
        let binds = crate::bindings::parse_bindings(vec![]);
        for key in [egui::Key::ArrowLeft, egui::Key::ArrowRight, egui::Key::Tab, egui::Key::Home] {
            let mut f = searching_filter();
            let mut steps = Vec::new();
            let retain =
                drain_search_or_nav(&mut steps, &mut f, &binds, key, egui::Modifiers::NONE, false);
            assert!(retain, "{key:?} produces no query text and must reach the binding table");
        }
    }

    /// Ctrl-modified keys suppress text generation, so they have no query input
    /// and must reach the binding table to fire user bindings.
    #[test]
    fn ctrl_keys_in_search_still_fall_through() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = searching_filter();

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::C,
            egui::Modifiers::CTRL,
            false,
        );

        assert!(
            retain,
            "ctrl-modified key produces no query text and must reach the binding table"
        );
    }

    /// Browsing mode is untouched: a letter must reach the binding table, which is
    /// how the filter toggle actions fire.
    #[test]
    fn a_text_key_in_browsing_is_not_consumed_by_the_pairing_rule() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = PanelFilter::new(&['s', 'a']);

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &binds,
            egui::Key::S,
            egui::Modifiers::NONE,
            true,
        );

        assert!(retain);
    }

    #[test]
    fn a_pile_of_notification_clicks_resolves_to_the_newest() {
        let (tx, rx) = mpsc::channel();
        assert_eq!(latest_notification_click(&rx), None);
        tx.send(3).unwrap();
        tx.send(7).unwrap();
        tx.send(5).unwrap();
        assert_eq!(latest_notification_click(&rx), Some(5));
        // The drain consumed everything, not just the returned click.
        assert_eq!(latest_notification_click(&rx), None);
    }

    #[test]
    fn session_ring_crosses_workspace_boundaries_and_wraps() {
        let ring = [(None, 1), (None, 2), (ws("a"), 3), (ws("b"), 4)];
        // Within a workspace it moves like tab cycling…
        assert_eq!(session_ring_target(&ring, Some(1), 1), Some((None, 2)));
        // …and crossing a boundary switches workspaces.
        assert_eq!(session_ring_target(&ring, Some(2), 1), Some((ws("a"), 3)));
        assert_eq!(session_ring_target(&ring, Some(3), -1), Some((None, 2)));
        // The ring wraps at both ends.
        assert_eq!(session_ring_target(&ring, Some(4), 1), Some((None, 1)));
        assert_eq!(session_ring_target(&ring, Some(1), -1), Some((ws("b"), 4)));
    }

    #[test]
    fn session_ring_stays_put_on_degenerate_input() {
        // Fewer than two sessions: nowhere to go.
        assert_eq!(session_ring_target(&[], Some(1), 1), None);
        assert_eq!(session_ring_target(&[(None, 1)], Some(1), 1), None);
        let ring = [(None, 1), (ws("a"), 2)];
        // No active session (emptied workspace on screen) re-anchors on the
        // first entry.
        assert_eq!(session_ring_target(&ring, None, 1), Some((None, 1)));
        // An active session missing from the ring does nothing.
        assert_eq!(session_ring_target(&ring, Some(9), 1), None);
    }

    fn entry(project: Option<&str>, workspace: &str, id: SessionId) -> RingEntry {
        RingEntry { project: project.map(PathBuf::from), workspace: ws(workspace), id }
    }

    /// The tree from the spec: home holds nothing, p1 owns w1 and w2, p2 owns
    /// w3.  Ring order is sidebar order, so p1's sessions precede p2's.
    fn spec_ring() -> Vec<RingEntry> {
        vec![
            entry(Some("/p1"), "/p1/w1", 1),
            entry(Some("/p1"), "/p1/w2", 2),
            entry(Some("/p2"), "/p2/w3", 3),
        ]
    }

    #[test]
    fn a_close_lands_on_the_successor() {
        assert_eq!(ring_landing(&spec_ring(), &[1], None), Some((ws("/p1/w2"), 2)));
    }

    #[test]
    fn a_close_at_the_tail_lands_on_the_predecessor() {
        assert_eq!(ring_landing(&spec_ring(), &[3], None), Some((ws("/p1/w2"), 2)));
    }

    /// A worktree deletion takes every session in the workspace at once, so
    /// the successor is measured past the last of them and the predecessor
    /// before the first.
    #[test]
    fn a_deletion_steps_over_every_session_it_removed() {
        let ring = vec![
            entry(Some("/p1"), "/p1/w1", 1),
            entry(Some("/p1"), "/p1/w2", 2),
            entry(Some("/p1"), "/p1/w2", 3),
            entry(Some("/p2"), "/p2/w3", 4),
        ];
        assert_eq!(ring_landing(&ring, &[2, 3], None), Some((ws("/p2/w3"), 4)));
    }

    #[test]
    fn an_empty_ring_and_an_unknown_removal_have_no_landing() {
        assert_eq!(ring_landing(&[], &[1], None), None);
        assert_eq!(ring_landing(&spec_ring(), &[99], None), None);
    }

    #[test]
    fn removing_everything_leaves_no_landing() {
        assert_eq!(ring_landing(&spec_ring(), &[1, 2, 3], None), None);
    }

    #[test]
    fn prefer_project_takes_its_own_project_over_a_nearer_neighbour() {
        // p2's session sits between the two p1 sessions, so a global search
        // from id 1 finds id 9 and a project-preferring one finds id 2.
        let ring = vec![
            entry(Some("/p1"), "/p1/w1", 1),
            entry(Some("/p2"), "/p2/w3", 9),
            entry(Some("/p1"), "/p1/w2", 2),
        ];
        assert_eq!(ring_landing(&ring, &[1], None), Some((ws("/p2/w3"), 9)));
        assert_eq!(ring_landing(&ring, &[1], Some(Path::new("/p1"))), Some((ws("/p1/w2"), 2)));
    }

    /// `ring_project` is `ring_global` plus a first pass, so the two must
    /// agree whenever that pass finds nothing.
    #[test]
    fn prefer_project_falls_through_to_the_whole_ring() {
        let ring = spec_ring();
        assert_eq!(
            ring_landing(&ring, &[3], Some(Path::new("/p2"))),
            ring_landing(&ring, &[3], None),
        );
    }

    #[test]
    fn home_has_no_project_to_prefer() {
        let ring = vec![entry(None, "/home-placeholder", 1), entry(Some("/p1"), "/p1/w1", 2)];
        assert_eq!(ring_landing(&ring, &[1], None), Some((ws("/p1/w1"), 2)));
    }

    /// A path two projects list is in the ring twice with one owner, so
    /// either occurrence resolves to the same landing.
    #[test]
    fn a_duplicated_workspace_changes_no_landing() {
        let ring = vec![
            entry(Some("/p1"), "/shared", 1),
            entry(Some("/p1"), "/shared", 1),
            entry(Some("/p2"), "/p2/w", 2),
        ];
        assert_eq!(ring_landing(&ring, &[1], None), Some((ws("/p2/w"), 2)));
    }

    #[test]
    fn move_target_reorders_forward_and_back() {
        // Drag "a" to the end (drop below the last row, index len).
        assert_eq!(moved(&["a", "b", "c"], 0, 3), vec!["b", "c", "a"]);
        // Drag "c" to the front (drop above row 0).
        assert_eq!(moved(&["a", "b", "c"], 2, 0), vec!["c", "a", "b"]);
        // Drag "a" to sit before "c" (drop above row 2).
        assert_eq!(moved(&["a", "b", "c"], 0, 2), vec!["b", "a", "c"]);
    }

    #[test]
    fn move_target_is_a_no_op_when_position_is_unchanged() {
        // Dropping above your own row, or just below it, changes nothing.
        assert_eq!(move_target(3, 1, 1), None);
        assert_eq!(move_target(3, 1, 2), None);
        // Dropping onto yourself.
        assert_eq!(move_target(3, 0, 0), None);
        // A stale source index (list shrank mid-drag) is ignored.
        assert_eq!(move_target(2, 5, 0), None);
    }

    #[test]
    fn walk_swaps_moves_within_a_contiguous_workspace() {
        assert_eq!(walked(&["a", "b", "c"], &[0, 1, 2], 0, 2), vec!["b", "c", "a"]);
        assert_eq!(walked(&["a", "b", "c"], &[0, 1, 2], 2, 0), vec!["c", "a", "b"]);
    }

    #[test]
    fn walk_swaps_leaves_interleaved_workspaces_in_place() {
        // Slots 0 and 2 belong to one workspace, slot 1 to another; moving the
        // first workspace's second session to the front must not disturb it.
        assert_eq!(walked(&["a", "x", "b"], &[0, 2], 1, 0), vec!["b", "x", "a"]);
    }

    #[test]
    fn walk_swaps_is_empty_when_nothing_moves() {
        assert!(walk_swaps(&[0, 1, 2], 1, 1).is_empty());
        // A position past the end clamps to the last slot, which is a no-op
        // for the element already there.
        assert!(walk_swaps(&[0, 1, 2], 2, 9).is_empty());
    }

    #[test]
    fn a_same_workspace_drop_uses_the_move_target_arithmetic() {
        // Dropping below your own row is a no-op, the same as for projects.
        assert_eq!(drop_position(true, 3, 1, 2), None);
        // Dropping onto the row below moves you past it.
        assert_eq!(drop_position(true, 3, 1, 3), Some(2));
        assert_eq!(moved(&["a", "b", "c"], 1, 3), vec!["a", "c", "b"]);
    }

    #[test]
    fn a_cross_workspace_drop_takes_the_display_slot_as_the_position() {
        // The session is not in that workspace's list yet, so nothing shifts
        // down and every slot passes through — including the two the same
        // workspace answers differently, which is what tells the branches
        // apart.
        assert_eq!(drop_position(false, 3, 1, 2), Some(2));
        assert_eq!(drop_position(false, 3, 1, 3), Some(3));
        // A drop onto the front of a workspace whose rows are all below it.
        assert_eq!(drop_position(false, 0, 0, 0), Some(0));
    }

    #[test]
    fn walk_swaps_places_an_arrival_at_the_stated_position() {
        // Arriving from another workspace, the display slot is the position:
        // nothing was removed from this list first, so there is no off-by-one.
        assert_eq!(walk_swaps(&[0, 1, 2], 2, 0), vec![(1, 2), (0, 1)]);
    }

    #[test]
    fn reorder_subject_prefers_the_cursored_session() {
        assert_eq!(
            reorder_subject(true, Some(&SidebarRow::Session(7)), || None, |_| None, || Some(3)),
            Some(7)
        );
    }

    #[test]
    fn reorder_subject_takes_a_workspace_rows_active_session() {
        // The landing after a cross-workspace step: the session paints no row
        // yet, so the cursor sits on the worktree it arrived in.
        let row = SidebarRow::Worktree(PathBuf::from("/b"));
        assert_eq!(
            reorder_subject(true, Some(&row), || None, |p| (p == Path::new("/b")).then_some(9), || Some(3)),
            Some(9)
        );
        assert_eq!(
            reorder_subject(true, Some(&SidebarRow::Home), || Some(4), |_| None, || Some(3)),
            Some(4)
        );
    }

    #[test]
    fn reorder_subject_falls_back_to_the_session_on_screen() {
        // Terminal focused: the cursor is ignored entirely.
        assert_eq!(
            reorder_subject(false, Some(&SidebarRow::Session(7)), || None, |_| None, || Some(3)),
            Some(3)
        );
        // Sidebar focused on a project header, which owns no session.
        let row = SidebarRow::Project(PathBuf::from("/a"));
        assert_eq!(reorder_subject(true, Some(&row), || None, |_| None, || Some(3)), Some(3));
        // And an empty workspace row falls through rather than refusing.
        let row = SidebarRow::Worktree(PathBuf::from("/b"));
        assert_eq!(reorder_subject(true, Some(&row), || None, |_| None, || Some(3)), Some(3));
    }

    fn entries(ids: &[SessionId]) -> Vec<sidebar_nav::WorkspaceEntry> {
        ids.iter().copied().map(sidebar_nav::WorkspaceEntry::Session).collect()
    }

    /// herdr's index, paired with the row it belongs to, is what
    /// `workspace_entries` sorts on.
    fn at(
        index: usize,
        entry: sidebar_nav::WorkspaceEntry,
    ) -> (usize, sidebar_nav::WorkspaceEntry) {
        (index, entry)
    }

    fn agent_entry(terminal_id: &str) -> sidebar_nav::WorkspaceEntry {
        sidebar_nav::WorkspaceEntry::Agent(herdr::Side::Native, terminal_id.to_string())
    }

    #[test]
    fn workspace_entries_keep_shell_sessions_in_spawn_order() {
        assert_eq!(workspace_entries(&[1, 3], Vec::new(), false), entries(&[1, 3]));
    }

    #[test]
    fn profile_menu_label_numbers_from_one() {
        assert_eq!(profile_menu_label(1, "WSL"), "1. WSL");
        assert_eq!(profile_menu_label(2, "cmd"), "2. cmd");
    }

    #[test]
    fn base_branch_precedence_is_override_then_pr_then_default() {
        let f = effective_base_branch;
        assert_eq!(f(Some("develop"), Some("main"), Some("master")), Some("develop".into()));
        assert_eq!(f(None, Some("main"), Some("master")), Some("main".into()));
        assert_eq!(f(None, None, Some("master")), Some("master".into()));
        assert_eq!(f(None, None, None), None);
    }

    #[test]
    fn picker_filter_is_a_case_insensitive_contains() {
        let branches =
            vec!["main".to_string(), "develop".to_string(), "origin/develop".to_string()];
        assert_eq!(filter_branches(&branches, ""), branches);
        assert_eq!(filter_branches(&branches, "DEV"), vec!["develop", "origin/develop"]);
        assert!(filter_branches(&branches, "zz").is_empty());
    }

    #[test]
    fn picker_cursor_seeds_the_first_match_on_a_non_empty_query_change() {
        // Typing a query that matches something jumps past Auto to the first
        // match, so Enter applies that match instead of Auto.
        assert_eq!(picker_cursor(true, false, 0, 3), 1);
        // A query with no matches has nothing to land on but Auto.
        assert_eq!(picker_cursor(true, false, 0, 0), 0);
        // Clearing the query back to empty returns the cursor to Auto.
        assert_eq!(picker_cursor(true, true, 5, 3), 0);
        // No query change this frame: clamp the previous cursor to the
        // (possibly shrunk) filtered length instead of reseeding it.
        assert_eq!(picker_cursor(false, false, 5, 3), 3);
        assert_eq!(picker_cursor(false, false, 2, 3), 2);
    }

    #[test]
    fn session_row_title_drops_an_agents_decorative_prefix() {
        let agent = SessionActivity::agent(Some("claude"), LiveState::Idle);
        assert_eq!(session_row_title("✳ claude", agent), "claude");
        assert_eq!(
            session_row_title("⠋ Thinking…", SessionActivity::agent(None, LiveState::Working)),
            "Thinking…"
        );
        // An ordinary session title owns its decoration because the status
        // slot is not replacing it with an agent mark.
        assert_eq!(session_row_title("✳ favorite", SessionActivity::Shell), "✳ favorite");
        // A recognized agent with a plain title strips nothing.
        assert_eq!(session_row_title("node build", agent), "node build");
        // Never strip down to an empty label.
        assert_eq!(session_row_title("✳ ", agent), "✳ ");
    }

    /// The status slot is the only place a blocked agent announces itself, so
    /// the three live states must not collapse into the same mark.
    #[test]
    fn each_live_state_draws_its_own_mark() {
        let quiet = Color32::from_rgb(1, 1, 1);
        let attention = Color32::from_rgb(2, 2, 2);
        assert_eq!(
            agent_mark(LiveState::Idle, quiet, attention),
            AgentMark::Glyph(DEFAULT_AGENT_ICON, quiet)
        );
        assert_eq!(agent_mark(LiveState::Working, quiet, attention), AgentMark::Loader);
        assert_eq!(
            agent_mark(LiveState::Blocked, quiet, attention),
            AgentMark::Glyph(DEFAULT_BLOCKED_ICON, attention)
        );
    }

    /// A blocked agent stays amber on the row the user is looking at, where
    /// the accent would otherwise claim the slot.
    #[test]
    fn blocked_keeps_its_color_over_the_active_rows_accent() {
        let accent = Color32::from_rgb(3, 3, 3);
        let attention = Color32::from_rgb(4, 4, 4);
        assert_eq!(
            agent_mark(LiveState::Blocked, accent, attention),
            AgentMark::Glyph(DEFAULT_BLOCKED_ICON, attention)
        );
    }

    #[test]
    fn the_status_hint_names_the_agent_and_what_it_is_doing() {
        assert_eq!(agent_hint(LiveState::Idle, Some("claude")), "claude is running");
        assert_eq!(agent_hint(LiveState::Working, Some("codex")), "codex is working");
        assert_eq!(agent_hint(LiveState::Blocked, Some("claude")), "claude is waiting for you");
        assert_eq!(agent_hint(LiveState::Blocked, None), "agent is waiting for you");
    }

    #[test]
    fn an_attached_herdr_session_takes_herdrs_live_state() {
        let claude = SessionActivity::agent(Some("claude"), LiveState::Idle);

        // Not attached to herdr: nothing overrides the session's own reading.
        assert_eq!(herdr_backed_activity(claude, None), claude);

        // herdr sees the approval dialog no title heuristic can.
        assert_eq!(
            herdr_backed_activity(claude, Some(herdr::Status::Blocked)),
            SessionActivity::agent(Some("claude"), LiveState::Blocked)
        );

        // An attached pane holds an agent even when the process probe missed
        // one, so the gate closes on herdr's word alone.
        assert_eq!(
            herdr_backed_activity(SessionActivity::Shell, Some(herdr::Status::Working)),
            SessionActivity::agent(None, LiveState::Working)
        );

        // `unknown` is herdr declining to say, not a claim of idleness: the
        // session keeps whatever it already knew.
        let working = SessionActivity::agent(Some("claude"), LiveState::Working);
        assert_eq!(herdr_backed_activity(working, Some(herdr::Status::Unknown)), working);
    }

    fn herdr_agent(kind: Option<&str>) -> herdr::Agent {
        herdr::Agent {
            terminal_id: "term_65abfc8e300361".into(),
            pane_id: "w5:p1".into(),
            kind: kind.map(String::from),
            title: None,
            status: herdr::Status::Idle,
            focused: false,
            cwd: None,
            foreground_cwd: None,
        }
    }

    /// herdr distinguishes four live states and says so on its own panes.
    /// Collapsing any pair onto one mark would make the sidebar say less
    /// about a pane than the window it came from.
    #[test]
    fn herdr_marks_keep_its_four_states_apart() {
        for set in [herdr::Indicators::Dots, herdr::Indicators::Symbols] {
            let marks: Vec<HarnessMark> = [
                herdr::Status::Blocked,
                herdr::Status::Working,
                herdr::Status::Done,
                herdr::Status::Idle,
            ]
            .into_iter()
            .map(|status| herdr_mark(status, set))
            .collect();
            for (i, a) in marks.iter().enumerate() {
                for b in &marks[i + 1..] {
                    assert_ne!(a, b, "{set:?} draws two states the same");
                }
            }
        }
    }

    /// Taken from herdr's own `state_icon_symbol`, so a pane carries one mark
    /// whether it is read in herdr or in the sidebar.
    #[test]
    fn herdr_marks_are_the_ones_herdr_paints() {
        let dots = |status| herdr_mark(status, herdr::Indicators::Dots).glyph;
        assert_eq!(dots(herdr::Status::Blocked), "●");
        assert_eq!(dots(herdr::Status::Working), "●");
        assert_eq!(dots(herdr::Status::Done), "●");
        assert_eq!(dots(herdr::Status::Idle), "○");

        let symbols = |status| herdr_mark(status, herdr::Indicators::Symbols).glyph;
        assert_eq!(symbols(herdr::Status::Blocked), "×");
        assert_eq!(symbols(herdr::Status::Working), "◐");
        assert_eq!(symbols(herdr::Status::Done), "✓");
        assert_eq!(symbols(herdr::Status::Idle), "○");
    }

    /// A status alacritree does not recognise is herdr declining to say, and
    /// the row says that rather than claiming the agent is idle.
    #[test]
    fn an_unknown_herdr_status_is_drawn_as_no_reading() {
        for set in [herdr::Indicators::Dots, herdr::Indicators::Symbols] {
            let mark = herdr_mark(herdr::Status::Unknown, set);
            assert_eq!(mark.glyph, "·");
            assert_eq!(mark.tone, StateTone::Unclear);
        }
    }

    #[test]
    fn herdr_row_name_prefers_the_reported_kind() {
        let row = HerdrRowData::from_agent(
            &herdr_agent(Some("claude")),
            &herdr::Side::Native,
            &herdr::Settings::default(),
        );
        assert_eq!(row.name, RowName::plain("claude".into()));
    }

    #[test]
    fn herdr_row_name_falls_back_to_the_terminal_id_tail() {
        let row = HerdrRowData::from_agent(
            &herdr_agent(None),
            &herdr::Side::Native,
            &herdr::Settings::default(),
        );
        assert_eq!(row.name, RowName::plain("300361".into()));
    }

    /// A shared view shows whatever pane herdr has focused, so the one on
    /// screen has to keep asking for its own.
    #[test]
    fn a_shared_view_asks_herdr_for_its_pane() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        let asks = needs_view_focus(Some(&key), 1, None);
        assert_eq!(asks, cfg!(windows));
    }

    /// A direct attach is wired to one pane, so herdr's focus decides nothing
    /// about what it draws.
    #[test]
    fn a_direct_attach_never_asks_herdr_for_its_pane() {
        let key = herdr::HerdrKey { side: herdr::Side::Wsl("d".into()), terminal_id: "t1".into() };
        assert!(!needs_view_focus(Some(&key), 1, None));
        assert!(!needs_view_focus(None, 1, None));
    }

    /// The pane herdr was last pointed at is where it still is, and asking
    /// again every frame would spawn a herdr per frame.
    #[test]
    fn a_shared_view_asks_once_per_switch() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        assert!(!needs_view_focus(Some(&key), 1, Some(1)));
        let asks = needs_view_focus(Some(&key), 2, Some(1));
        assert_eq!(asks, cfg!(windows));
    }

    #[test]
    fn herdr_row_shared_view_follows_can_attach() {
        let native = HerdrRowData::from_agent(
            &herdr_agent(None),
            &herdr::Side::Native,
            &herdr::Settings::default(),
        );
        assert_eq!(native.managed.shared_view, cfg!(windows));

        let wsl = HerdrRowData::from_agent(
            &herdr_agent(None),
            &herdr::Side::Wsl("d".into()),
            &herdr::Settings::default(),
        );
        assert!(!wsl.managed.shared_view);
    }

    #[test]
    fn herdr_row_name_keeps_a_short_terminal_id_whole() {
        // `saturating_sub(6)` exists precisely for ids shorter than the tail
        // it takes; a plain `- 6` would panic on this one.
        let agent = herdr::Agent {
            terminal_id: "t1".into(),
            pane_id: "w1:p1".into(),
            kind: None,
            title: None,
            status: herdr::Status::Idle,
            focused: false,
            cwd: None,
            foreground_cwd: None,
        };
        let row =
            HerdrRowData::from_agent(&agent, &herdr::Side::Native, &herdr::Settings::default());
        assert_eq!(row.name, RowName::plain("t1".into()));
    }

    /// On Linux and WSL an attach is full passthrough, so the pane the user
    /// ends up looking at is herdr's.  The row says so by naming it the way
    /// herdr's own listed row does, rather than by whatever the attach
    /// process titled its PTY.
    #[test]
    fn an_attached_session_takes_herdrs_name() {
        let agent = titled(Some("claude"), Some("primary"));
        let name = session_row_name("bash", SessionActivity::Shell, Some(&agent));
        assert_eq!((name.context.as_deref(), name.text.as_str()), (Some("claude"), "primary"));
    }

    /// herdr reporting no title is not a reason to lose the name the session
    /// already had, so the PTY's title stands in.
    #[test]
    fn an_attached_session_without_a_herdr_title_keeps_its_own() {
        let agent = titled(Some("claude"), None);
        let name = session_row_name("✳ building", SessionActivity::Shell, Some(&agent));
        assert_eq!((name.context.as_deref(), name.text.as_str()), (Some("claude"), "✳ building"));
    }

    /// A session no harness owns is untouched by any of this.
    #[test]
    fn an_ordinary_session_keeps_its_pty_title() {
        let agent = SessionActivity::agent(Some("claude"), LiveState::Idle);
        let name = session_row_name("✳ claude", agent, None);
        assert_eq!((name.context, name.text.as_str()), (None, "claude"));
    }

    fn titled(kind: Option<&str>, title: Option<&str>) -> herdr::Agent {
        herdr::Agent { title: title.map(String::from), ..herdr_agent(kind) }
    }

    fn row_of(kind: Option<&str>, title: Option<&str>) -> HerdrRowData {
        HerdrRowData::from_agent(
            &titled(kind, title),
            &herdr::Side::Wsl("d".into()),
            &herdr::Settings::default(),
        )
    }

    /// The kind is a category and the title is an identity, so the title takes
    /// the bright slot and the kind stands in front of it as context.
    #[test]
    fn a_titled_agent_reads_kind_then_title() {
        let row = row_of(Some("claude"), Some("primary"));
        assert_eq!(
            (row.name.context.as_deref(), row.name.text.as_str()),
            (Some("claude"), "primary")
        );
    }

    /// Nothing is repeated: a title that only echoes the kind leaves the row
    /// with one word, not the same word twice.
    #[test]
    fn a_title_equal_to_the_kind_is_not_repeated() {
        let row = row_of(Some("codex"), Some("codex"));
        assert_eq!((row.name.context.as_deref(), row.name.text.as_str()), (None, "codex"));
    }

    #[test]
    fn an_untitled_agent_keeps_the_kind_as_its_name() {
        let row = row_of(Some("claude"), None);
        assert_eq!((row.name.context.as_deref(), row.name.text.as_str()), (None, "claude"));
    }

    /// An agent herdr has not identified still has a pane title, and that is a
    /// better name than six characters of a terminal id.
    #[test]
    fn a_kindless_agent_is_named_by_its_title() {
        let row = row_of(None, Some("scratch"));
        assert_eq!((row.name.context.as_deref(), row.name.text.as_str()), (None, "scratch"));
    }

    #[test]
    fn an_agent_with_neither_falls_back_to_the_terminal_id_tail() {
        let row = row_of(None, None);
        assert_eq!((row.name.context.as_deref(), row.name.text.as_str()), (None, "300361"));
    }

    /// Everything the row's marks cannot say goes here, one fact per comma,
    /// with the way out in parentheses after them: a mark can carry a state
    /// but not the word for it, and nothing on the row can carry a chord.
    #[test]
    fn the_tooltip_spells_out_what_the_marks_cannot() {
        let agent = herdr::Agent {
            status: herdr::Status::Working,
            ..titled(Some("claude"), Some("Claude Code"))
        };
        let mut row = HerdrRowData::from_agent(
            &agent,
            &herdr::Side::Wsl("d".into()),
            &herdr::Settings::default(),
        );
        assert_eq!(managed_tooltip(&row.managed), r#"working, herdr, `claude` "Claude Code"."#);

        row.managed.detach = Some("Ctrl+B q".to_string());
        assert_eq!(
            managed_tooltip(&row.managed),
            r#"working, herdr, `claude` "Claude Code". (detach with `Ctrl+B q`)"#
        );

        row.managed.shared_view = true;
        assert_eq!(
            managed_tooltip(&row.managed),
            r#"working, herdr, shared view, `claude` "Claude Code". (detach with `Ctrl+B q`)"#
        );
    }

    /// A pane whose title says no more than its kind is named once.
    #[test]
    fn the_tooltip_does_not_say_the_kind_twice() {
        let row = row_of(Some("codex"), Some("codex"));
        assert_eq!(managed_tooltip(&row.managed), "idle, herdr, `codex`.");
    }

    /// A session alacritree still holds open after herdr stopped listing its
    /// pane has no state and no name left to report, but it is still herdr's
    /// and the user still has to know how to leave it.
    #[test]
    fn an_unlisted_pane_still_says_how_to_leave() {
        let settings =
            herdr::Settings { detach: Some("Ctrl+B q".into()), ..herdr::Settings::default() };
        let managed = Managed::herdr(&herdr::Side::Wsl("d".into()), &settings, None);
        assert_eq!(managed_tooltip(&managed), "herdr. (detach with `Ctrl+B q`)");
    }

    /// Losing a view costs a click to get back and losing a shell costs the
    /// shell, so the two prompts answer to separate switches — and the busy
    /// question a close asks never reaches a detach, whose attach client is
    /// running by definition.
    #[test]
    fn a_detach_asks_on_its_own_switch() {
        use crate::config::ConfirmSessionClose;

        let mut ui = UiTheme::default();
        assert_eq!(ui.confirm_session_close, ConfirmSessionClose::Never);
        assert!(close_needs_prompt(&ui, true, false));
        assert!(!close_needs_prompt(&ui, false, true));

        ui.confirm_session_close = ConfirmSessionClose::Always;
        ui.confirm_session_detach = false;
        assert!(!close_needs_prompt(&ui, true, true));
        assert!(close_needs_prompt(&ui, false, false));
    }

    /// Ending a herdr-managed session ends the attach and leaves the pane
    /// running, so the control cannot call itself a close.
    #[test]
    fn the_close_control_is_a_detach_on_a_managed_row() {
        assert_eq!(close_button_hint(true), "detach session");
        assert_eq!(close_button_hint(false), "close session");
    }

    #[test]
    fn loader_is_three_equal_squares_rotating_around_a_two_by_two_grid() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(12.0));
        let corners = [
            egui::pos2(0.0, 0.0),
            egui::pos2(7.0, 0.0),
            egui::pos2(7.0, 7.0),
            egui::pos2(0.0, 7.0),
        ];
        for missing in 0..4 {
            let dots = three_square_loader_dots(rect, missing);
            assert!(dots.iter().all(|dot| dot.size() == egui::Vec2::splat(5.0)));
            for (index, corner) in corners.iter().enumerate() {
                assert_eq!(
                    dots.iter().any(|dot| dot.min == *corner),
                    index != missing,
                    "frame {missing}, corner {index}"
                );
            }
        }
    }

    #[test]
    fn workspace_entries_apply_the_two_row_threshold() {
        assert!(workspace_entries(&[], Vec::new(), false).is_empty());
        assert!(workspace_entries(&[1], Vec::new(), false).is_empty());
        assert_eq!(workspace_entries(&[1, 3], Vec::new(), false), entries(&[1, 3]));
    }

    #[test]
    fn workspace_entries_always_flag_lists_single_sessions() {
        assert_eq!(workspace_entries(&[1], Vec::new(), true), entries(&[1]));
        assert!(workspace_entries(&[], Vec::new(), true).is_empty());
    }

    /// A herdr pane has no other surface to appear on, so the threshold can
    /// never hide one — and a lone shell session beside one is listed rather
    /// than folded into the workspace row, which would leave a hole in a list
    /// its neighbour is already in.
    #[test]
    fn a_herdr_row_is_listed_whatever_the_threshold_says() {
        let lone = workspace_entries(&[], vec![at(0, agent_entry("t1"))], false);
        assert_eq!(lone, vec![agent_entry("t1")]);

        let beside = workspace_entries(&[1], vec![at(0, agent_entry("t1"))], false);
        assert_eq!(beside, vec![sidebar_nav::WorkspaceEntry::Session(1), agent_entry("t1")]);
    }

    /// The whole point of the merged list: a pane keeps herdr's position
    /// whether alacritree is attached to it or not, so attaching changes how
    /// a row is drawn and never where it sits.
    #[test]
    fn herdr_rows_take_their_order_from_herdr_not_from_the_attach() {
        // herdr lists t1 then t2; alacritree attached to t2 first, so the
        // session vec has them the other way round.
        let managed =
            vec![at(1, sidebar_nav::WorkspaceEntry::Session(9)), at(0, agent_entry("t1"))];
        assert_eq!(workspace_entries(&[], managed, false), vec![
            agent_entry("t1"),
            sidebar_nav::WorkspaceEntry::Session(9)
        ]);
    }

    /// A pane herdr has stopped listing keeps its block rather than falling
    /// in among the shells: the session is still herdr's, and it goes back to
    /// its slot when the listing carries it again.
    #[test]
    fn an_unlisted_pane_waits_at_the_tail_of_its_own_block() {
        let managed =
            vec![at(usize::MAX, sidebar_nav::WorkspaceEntry::Session(9)), at(0, agent_entry("t1"))];
        assert_eq!(workspace_entries(&[1], managed, false), vec![
            sidebar_nav::WorkspaceEntry::Session(1),
            agent_entry("t1"),
            sidebar_nav::WorkspaceEntry::Session(9),
        ]);
    }

    use crate::projects::{Project, Worktree};

    /// A project whose main checkout is `root`, plus secondary worktrees.
    fn project_with(root: &str, extra: &[&str]) -> Project {
        let wt = |path: &str, is_main: bool| Worktree {
            name: path.to_string(),
            path: PathBuf::from(path),
            branch: None,
            is_main,
            prunable: false,
            upstream: None,
        };
        Project {
            root: PathBuf::from(root),
            name: "p".to_string(),
            label: None,
            default_branch: None,
            worktrees: std::iter::once(wt(root, true))
                .chain(extra.iter().map(|p| wt(p, false)))
                .collect(),
            expanded: true,
            shell_override: None,
            home: None,
        }
    }

    /// One worktree holding a `main` shell and three more, with an unrelated
    /// workspace ahead of them so a landing that ignored `workspace` would
    /// show up as picking id 9.
    fn close_row() -> Vec<(WorkspaceKey, SessionId)> {
        vec![
            (ws("/other"), 9),
            (ws("/repo/wt"), 1),
            (ws("/repo/wt"), 2),
            (ws("/repo/wt"), 3),
            (ws("/repo/wt"), 4),
        ]
    }

    fn closing(idx: usize) -> Vec<(WorkspaceKey, SessionId)> {
        let mut sessions = close_row();
        sessions.remove(idx);
        sessions
    }

    #[test]
    fn preserve_hands_the_workspace_its_first_session() {
        for (removed_idx, expected) in [(1, 2), (2, 1), (3, 1), (4, 1)] {
            assert_eq!(
                close_landing(
                    &closing(removed_idx),
                    &ws("/repo/wt"),
                    removed_idx,
                    SidebarFocus::Preserve
                ),
                Some(expected),
                "closing index {removed_idx}"
            );
        }
    }

    #[test]
    fn follow_lands_on_the_successor() {
        for (removed_idx, expected) in [(1, 2), (2, 3), (3, 4)] {
            assert_eq!(
                close_landing(
                    &closing(removed_idx),
                    &ws("/repo/wt"),
                    removed_idx,
                    SidebarFocus::Follow
                ),
                Some(expected),
                "closing index {removed_idx}"
            );
        }
    }

    /// Matches `sidebar_focus::slide`, which falls back to the last survivor
    /// when the removed row had no successor; cursor and terminal would
    /// otherwise pick different siblings under `"follow"`.
    #[test]
    fn follow_lands_on_the_predecessor_when_the_last_session_closes() {
        assert_eq!(close_landing(&closing(4), &ws("/repo/wt"), 4, SidebarFocus::Follow), Some(3));
    }

    #[test]
    fn an_emptied_workspace_has_no_landing_under_either_mode() {
        let remaining = vec![(ws("/other"), 9)];
        for mode in [SidebarFocus::Preserve, SidebarFocus::Follow] {
            assert_eq!(close_landing(&remaining, &ws("/repo/wt"), 1, mode), None, "{mode:?}");
        }
    }

    #[test]
    fn fallback_prefers_project_main_with_live_session() {
        let remaining = vec![(ws("/repo"), 1)];
        assert_eq!(
            close_fallback(
                &ws("/repo/wt"),
                &ws("/repo/wt"),
                &remaining,
                Some(PathBuf::from("/repo"))
            ),
            CloseFallback::Activate(PathBuf::from("/repo"))
        );
    }

    #[test]
    fn fallback_goes_home_when_project_main_has_no_session() {
        let remaining = vec![(ws("/other"), 1)];
        assert_eq!(
            close_fallback(
                &ws("/repo/wt"),
                &ws("/repo/wt"),
                &remaining,
                Some(PathBuf::from("/repo"))
            ),
            CloseFallback::Home
        );
    }

    #[test]
    fn fallback_goes_home_from_the_project_main_itself() {
        // project_main_for returns None when ws is the main checkout, so the
        // decision sees no main to activate.
        assert_eq!(close_fallback(&ws("/repo"), &ws("/repo"), &[], None), CloseFallback::Home);
    }

    #[test]
    fn fallback_goes_home_from_home() {
        assert_eq!(close_fallback(&None, &None, &[], None), CloseFallback::Home);
    }

    #[test]
    fn fallback_stays_on_background_workspace_close() {
        assert_eq!(
            close_fallback(&ws("/repo/wt"), &None, &[], Some(PathBuf::from("/repo"))),
            CloseFallback::Stay
        );
    }

    #[test]
    fn fallback_stays_when_siblings_survive() {
        let remaining = vec![(ws("/repo/wt"), 2)];
        assert_eq!(
            close_fallback(
                &ws("/repo/wt"),
                &ws("/repo/wt"),
                &remaining,
                Some(PathBuf::from("/repo"))
            ),
            CloseFallback::Stay
        );
    }

    #[test]
    fn a_deferred_verdict_survives_instead_of_being_re_derived() {
        // `close_fallback` is the only thing that knows to hop to the project's
        // main checkout; a generic "spawn something" fallback would strand
        // last_session_close = "navigate" in the workspace that just emptied.
        let main = PathBuf::from("/p/main");
        let removed = Some(PathBuf::from("/p/feature"));
        let remaining = vec![(Some(main.clone()), 1)];

        let verdict = close_fallback(&removed, &removed, &remaining, Some(main.clone()));
        assert_eq!(verdict, CloseFallback::Activate(main.clone()));

        let deferred = DeferredClose { verdict, removed_worktree: None };
        assert_eq!(
            deferred.verdict,
            CloseFallback::Activate(main),
            "the verdict is carried, not recomputed from whatever state remains"
        );
    }

    /// A user's close navigates: away from an emptied workspace, or into a
    /// replacement shell.  A failed open must do neither.  Wherever it
    /// navigates to, `ensure_active_session` spawns into it, and that open
    /// fails the same way.
    #[test]
    fn a_failed_spawn_neither_navigates_nor_respawns() {
        assert_eq!(close_navigation(CloseReason::User, CloseFallback::Home), CloseFallback::Home);
        assert_eq!(
            close_navigation(CloseReason::SpawnFailed, CloseFallback::Home),
            CloseFallback::Stay
        );
    }

    #[test]
    fn only_follow_defers_close_navigation() {
        use crate::config::SidebarFocus;

        assert!(defers_close_navigation(SidebarFocus::Follow));
        assert!(!defers_close_navigation(SidebarFocus::Preserve));
    }

    #[test]
    fn project_main_resolves_for_secondary_worktrees_only() {
        let projects = vec![project_with("/repo", &["/repo-wt/feat"])];
        assert_eq!(
            project_main_for(&projects, Path::new("/repo-wt/feat")),
            Some(PathBuf::from("/repo"))
        );
        // The main itself and unknown paths have no fallback target.
        assert_eq!(project_main_for(&projects, Path::new("/repo")), None);
        assert_eq!(project_main_for(&projects, Path::new("/elsewhere")), None);
    }

    /// Keyboard-originated `focus_move` with both panels open.
    fn mv(focus: PaneFocus, dir: FocusDir, tui_running: bool) -> FocusMove {
        focus_move(focus, dir, true, true, ActionOrigin::Keyboard, tui_running)
    }

    #[test]
    fn focus_moves_between_open_panels() {
        assert_eq!(
            mv(PaneFocus::Terminal, FocusDir::Left, false),
            FocusMove::Focus(PaneFocus::ProjectsSidebar)
        );
        assert_eq!(
            mv(PaneFocus::Terminal, FocusDir::Right, false),
            FocusMove::Focus(PaneFocus::GitSidebar)
        );
        assert_eq!(
            mv(PaneFocus::ProjectsSidebar, FocusDir::Right, false),
            FocusMove::Focus(PaneFocus::Terminal)
        );
        assert_eq!(
            mv(PaneFocus::GitSidebar, FocusDir::Left, false),
            FocusMove::Focus(PaneFocus::Terminal)
        );
    }

    #[test]
    fn focus_stops_at_the_outer_edges() {
        assert_eq!(mv(PaneFocus::ProjectsSidebar, FocusDir::Left, false), FocusMove::Nothing);
        assert_eq!(mv(PaneFocus::GitSidebar, FocusDir::Right, false), FocusMove::Nothing);
    }

    #[test]
    fn focus_never_moves_toward_a_closed_panel() {
        assert_eq!(
            focus_move(
                PaneFocus::Terminal,
                FocusDir::Left,
                false,
                true,
                ActionOrigin::Keyboard,
                false
            ),
            FocusMove::Nothing
        );
        assert_eq!(
            focus_move(
                PaneFocus::Terminal,
                FocusDir::Right,
                true,
                false,
                ActionOrigin::Keyboard,
                false
            ),
            FocusMove::Nothing
        );
    }

    #[test]
    fn running_tui_keeps_the_key() {
        assert_eq!(mv(PaneFocus::Terminal, FocusDir::Left, true), FocusMove::Passthrough);
        assert_eq!(mv(PaneFocus::Terminal, FocusDir::Right, true), FocusMove::Passthrough);
    }

    /// A palette-dispatched Focus Left/Right is a binding stand-in, so a
    /// running TUI must see the same passthrough a real keypress would.
    #[test]
    fn palette_origin_keeps_the_key_for_a_running_tui() {
        assert_eq!(
            focus_move(
                PaneFocus::Terminal,
                FocusDir::Left,
                true,
                true,
                ActionOrigin::Palette,
                true
            ),
            FocusMove::Passthrough
        );
    }

    #[test]
    fn sidebars_never_pass_through() {
        assert_eq!(
            mv(PaneFocus::ProjectsSidebar, FocusDir::Right, true),
            FocusMove::Focus(PaneFocus::Terminal)
        );
    }

    /// An IPC move is the inner program saying it is out of windows —
    /// passthrough would bounce the key straight back to it.
    #[test]
    fn ipc_moves_never_pass_through() {
        assert_eq!(
            focus_move(PaneFocus::Terminal, FocusDir::Left, true, true, ActionOrigin::Ipc, true),
            FocusMove::Focus(PaneFocus::ProjectsSidebar)
        );
        assert_eq!(
            focus_move(PaneFocus::Terminal, FocusDir::Left, false, true, ActionOrigin::Ipc, true),
            FocusMove::Nothing
        );
    }

    #[test]
    fn projects_filter_action_valid_when_projects_sidebar_focused() {
        let action = BindingAction::Named(NamedAction::ToggleSessionsFilter);
        assert!(valid_for_focus(&action, true, false, false));
    }

    #[test]
    fn projects_filter_action_rejected_when_git_sidebar_focused() {
        let action = BindingAction::Named(NamedAction::ToggleSessionsFilter);
        assert!(!valid_for_focus(&action, false, true, false));
    }

    #[test]
    fn git_filter_action_valid_when_git_sidebar_focused() {
        let action = BindingAction::Named(NamedAction::ToggleModifiedFilter);
        assert!(valid_for_focus(&action, false, true, false));
    }

    #[test]
    fn git_filter_action_rejected_when_projects_sidebar_focused() {
        let action = BindingAction::Named(NamedAction::ToggleModifiedFilter);
        assert!(!valid_for_focus(&action, true, false, false));
    }

    #[test]
    fn both_sidebar_filters_rejected_when_terminal_focused() {
        let projects_action = BindingAction::Named(NamedAction::ToggleSessionsFilter);
        let git_action = BindingAction::Named(NamedAction::ToggleModifiedFilter);
        assert!(!valid_for_focus(&projects_action, false, false, false));
        assert!(!valid_for_focus(&git_action, false, false, false));
    }

    /// `ScrollPageUp` is unscoped by pane focus, so only the scratchpad
    /// editor stealing it back (via `terminal_only`) should block it.
    #[test]
    fn terminal_only_action_yields_to_the_scratchpad_editor() {
        let action = BindingAction::Named(NamedAction::ScrollPageUp);
        assert!(!valid_for_focus(&action, false, false, true));
        assert!(valid_for_focus(&action, false, false, false));
    }

    #[test]
    fn a_wide_search_stands_down_the_project_toggles() {
        // Toggled on, workspace fails both: excluded while the toggles apply,
        // included once a wide search stands them down.
        assert!(!project_toggles_pass(true, true, false, true, false));
        assert!(project_toggles_pass(false, true, false, true, false));
    }

    #[test]
    fn a_wide_search_stands_down_the_git_toggles() {
        // Toggled on for "modified" only, an untracked row fails while the
        // toggle applies and passes once a wide search stands it down.
        assert!(!git_toggles_pass(true, false, false, ChangeKind::Untracked));
        assert!(git_toggles_pass(false, false, false, ChangeKind::Untracked));
    }

    #[test]
    fn a_pr_toggle_alone_makes_any_toggle_active() {
        assert!(!any_project_toggle_active(false, false, false));
        assert!(any_project_toggle_active(false, false, true));
    }

    #[test]
    fn worktree_pr_passes_is_inert_without_a_pr_toggle() {
        let path = PathBuf::from("/worktree");
        let mut pr_matches = HashMap::new();
        pr_matches.insert(path.clone(), false);
        assert!(worktree_pr_passes(false, &pr_matches, &path));
    }

    #[test]
    fn worktree_pr_passes_follows_the_map_once_a_pr_toggle_is_active() {
        let path = PathBuf::from("/worktree");
        let mut pr_matches = HashMap::new();
        pr_matches.insert(path.clone(), true);
        assert!(worktree_pr_passes(true, &pr_matches, &path));
        pr_matches.insert(path.clone(), false);
        assert!(!worktree_pr_passes(true, &pr_matches, &path));
    }

    #[test]
    fn worktree_pr_passes_excludes_a_worktree_missing_from_the_map() {
        let path = PathBuf::from("/worktree");
        let pr_matches: HashMap<PathBuf, bool> = HashMap::new();
        assert!(!worktree_pr_passes(true, &pr_matches, &path));
    }

    fn req(file: &str, source: DiffSource) -> DiffRequest {
        DiffRequest { file: file.to_string(), source }
    }

    #[test]
    fn diff_args_staged() {
        let args = diff_args(&req("a.rs", DiffSource::Staged));
        assert_eq!(args, vec!["diff", "--cached", "--", "a.rs"]);
    }

    #[test]
    fn diff_args_worktree() {
        let args = diff_args(&req("a.rs", DiffSource::Worktree));
        assert_eq!(args, vec!["diff", "--", "a.rs"]);
    }

    #[test]
    fn diff_args_untracked() {
        let args = diff_args(&req("a.rs", DiffSource::Untracked));
        assert_eq!(args, vec!["diff", "--no-index", "--", "/dev/null", "a.rs"]);
    }

    #[test]
    fn diff_args_branch() {
        let args = diff_args(&req("a.rs", DiffSource::Branch { base: "main".to_string() }));
        assert_eq!(args, vec!["diff", "main...", "--", "a.rs"]);
    }

    #[test]
    fn diff_command_uses_given_delta_program() {
        let (program, args) = build_diff_command("delta", &req("a.rs", DiffSource::Staged));
        assert_eq!(program, "git");
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "core.pager=delta --paging=always");
        assert_eq!(&args[2..], diff_args(&req("a.rs", DiffSource::Staged)).as_slice());
    }

    #[test]
    fn diff_command_honors_delta_override_path() {
        let (_, args) =
            build_diff_command(r"C:\tools\delta.exe", &req("a.rs", DiffSource::Worktree));
        assert_eq!(args[1], r"core.pager=C:\tools\delta.exe --paging=always");
    }

    #[test]
    fn wsl_diff_direct_uses_resolved_delta_and_keeps_pager_open() {
        let (program, args) = build_wsl_diff_command_direct(
            "kali-linux",
            Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj"),
            &req("a.rs", DiffSource::Staged),
            "/home/lev/.cargo/bin/delta",
        );
        assert_eq!(program, "wsl.exe");
        assert_eq!(args[..8], [
            "-d",
            "kali-linux",
            "--cd",
            r"\\wsl.localhost\kali-linux\home\lev\proj",
            "--exec",
            "sh",
            "-c",
            r#"export LESS="${LESS-R}"; exec git -c "core.pager=/home/lev/.cargo/bin/delta --paging=always" "$@""#,
        ]);
        assert_eq!(args[8], "sh");
        assert_eq!(&args[9..], diff_args(&req("a.rs", DiffSource::Staged)).as_slice());
    }

    #[test]
    fn wsl_diff_login_resolves_shell_and_keeps_pager_open() {
        let (program, args) = build_wsl_diff_command_login(
            "kali-linux",
            Path::new(r"\\wsl.localhost\kali-linux\home\lev\proj"),
            &req("a.rs", DiffSource::Staged),
        );
        assert_eq!(program, "wsl.exe");
        assert_eq!(args[..7], [
            "-d",
            "kali-linux",
            "--cd",
            r"\\wsl.localhost\kali-linux\home\lev\proj",
            "--exec",
            "sh",
            "-c"
        ]);
        let script = &args[7];
        assert!(script.contains("getent passwd"), "resolves login shell: {script}");
        // The LESS export lives inside the login shell's script so a LESS
        // sourced from the profile still wins.
        assert!(
            script.contains(
                r#"-lc 'export LESS="${LESS-R}"; exec git -c "core.pager=delta --paging=always" "$@"'"#
            ),
            "keeps pager open after profile sourcing: {script}"
        );
        assert_eq!(args[8], "sh");
        assert_eq!(&args[9..], diff_args(&req("a.rs", DiffSource::Staged)).as_slice());
    }

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

    #[test]
    fn ui_text_px_defaults_to_terminal_derivation() {
        let font = crate::config::FontConfig::default();
        let (normal, heading) = ui_text_px(&font, &crate::config::UiFont::default());
        assert_eq!(normal, font.ui_normal_px());
        assert_eq!(heading, font.ui_heading_px());
    }

    #[test]
    fn ui_text_px_overrides_from_ui_font_size() {
        let font = crate::config::FontConfig::default();
        let ui = crate::config::UiFont { size: Some(12.0), ..Default::default() };
        let (normal, heading) = ui_text_px(&font, &ui);
        assert_eq!(normal, 16.0); // 12 pt × 96/72
        assert_eq!(
            heading,
            16.0 * (crate::config::FontConfig::UI_HEADING_RATIO
                / crate::config::FontConfig::UI_NORMAL_RATIO)
        );
    }

    #[test]
    fn owning_worktree_matches_exact_and_descendant_paths() {
        let wts = vec![PathBuf::from("C:/w/feat-a"), PathBuf::from("C:/w/feat-b")];
        assert_eq!(
            owning_worktree(&wts, Path::new("C:/w/feat-a")),
            Some(PathBuf::from("C:/w/feat-a"))
        );
        assert_eq!(
            owning_worktree(&wts, Path::new("C:/w/feat-b/src/deep")),
            Some(PathBuf::from("C:/w/feat-b"))
        );
        assert_eq!(owning_worktree(&wts, Path::new("C:/elsewhere")), None);
    }

    /// A worktree checked out inside another checkout's subtree (e.g. under the
    /// main repo) must resolve to the inner worktree, not the enclosing one.
    #[test]
    fn owning_worktree_prefers_the_longest_prefix() {
        let wts = vec![PathBuf::from("C:/repo"), PathBuf::from("C:/repo/wt/inner")];
        assert_eq!(
            owning_worktree(&wts, Path::new("C:/repo/wt/inner/src")),
            Some(PathBuf::from("C:/repo/wt/inner"))
        );
    }

    /// The on-screen session keeps being watched: the view follows it to the
    /// target workspace.
    #[test]
    fn moving_the_on_screen_session_follows_it() {
        let out = plan_move(true, true, None, false);
        assert!(out.follow);
        assert!(out.claim_target);
        assert!(matches!(out.source, SourceRepair::Remove));
    }

    /// A background move is silent — no focus stealing — and only claims the
    /// target's active slot when the target had none.
    #[test]
    fn a_background_move_never_steals_focus() {
        let out = plan_move(false, false, None, true);
        assert!(!out.follow);
        assert!(!out.claim_target, "the target's own active session stays");
        assert!(matches!(out.source, SourceRepair::Keep));

        let out = plan_move(false, false, None, false);
        assert!(!out.follow);
        assert!(out.claim_target, "an empty target adopts the arrival");
    }

    /// Moving the source workspace's active-but-not-on-screen session promotes
    /// the next remaining session there, the way closing it would.
    #[test]
    fn the_source_workspace_repairs_its_active_session() {
        let out = plan_move(true, false, Some(9), false);
        assert!(matches!(out.source, SourceRepair::Set(9)));
        assert!(!out.follow);

        let out = plan_move(true, false, None, false);
        assert!(matches!(out.source, SourceRepair::Remove), "no session left to promote");
    }

    #[test]
    fn set_base_branch_targets_the_cursored_worktree_when_sidebar_focused() {
        let wt = PathBuf::from("C:/repo/wt");
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };
        let cursor = SidebarRow::Worktree(wt.clone());
        assert_eq!(
            base_branch_target(true, Some(&cursor), none, &Some(PathBuf::from("C:/other"))),
            Some(wt)
        );
    }

    #[test]
    fn set_base_branch_resolves_a_session_row_to_its_workspace() {
        let wt = PathBuf::from("C:/repo/wt");
        let ws = wt.clone();
        let lookup = move |id: SessionId| (id == 7).then(|| Some(ws.clone()));
        let cursor = SidebarRow::Session(7);
        assert_eq!(base_branch_target(true, Some(&cursor), lookup, &None), Some(wt));
    }

    #[test]
    fn set_base_branch_ignores_home_and_project_rows() {
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(true, Some(&SidebarRow::Home), none, &None), None);
        let cursor = SidebarRow::Project(PathBuf::from("C:/repo"));
        let none2 = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(true, Some(&cursor), none2, &None), None);
    }

    #[test]
    fn toggle_expanded_resolves_child_rows_to_their_project_root() {
        let projects = vec![project_with("/repo", &["/repo/wt"])];
        let root = PathBuf::from("/repo");
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };

        // The project header resolves to itself.
        assert_eq!(
            row_project_root(&projects, none, &SidebarRow::Project(root.clone())),
            Some(root.clone())
        );
        // A worktree child resolves to the owning project root — the case the
        // old dispatch missed, leaving `o` inert inside an expanded project.
        assert_eq!(
            row_project_root(&projects, none, &SidebarRow::Worktree(PathBuf::from("/repo/wt"))),
            Some(root.clone())
        );
        // A session resolves through its workspace to the project root.
        let lookup = |id: SessionId| (id == 7).then(|| Some(PathBuf::from("/repo/wt")));
        assert_eq!(
            row_project_root(&projects, lookup, &SidebarRow::Session(7)),
            Some(root.clone())
        );
        // Home belongs to no project.
        assert_eq!(row_project_root(&projects, none, &SidebarRow::Home), None);
    }

    #[test]
    fn search_confirm_reveals_only_child_rows() {
        let projects = vec![project_with("/repo", &["/repo/wt"])];
        let root = PathBuf::from("/repo");
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };

        // A worktree matched under a collapsed project would vanish once the
        // query clears, so its project is expanded to keep it selectable.
        assert_eq!(
            search_reveal_root(&projects, none, &SidebarRow::Worktree(PathBuf::from("/repo/wt"))),
            Some(root.clone())
        );
        // A session resolves through its workspace to the same project.
        let lookup = |id: SessionId| (id == 7).then(|| Some(PathBuf::from("/repo/wt")));
        assert_eq!(search_reveal_root(&projects, lookup, &SidebarRow::Session(7)), Some(root));
        // Confirming a header selects it without expanding or collapsing it.
        assert_eq!(
            search_reveal_root(&projects, none, &SidebarRow::Project(PathBuf::from("/repo"))),
            None
        );
        // Home owns no project to reveal.
        assert_eq!(search_reveal_root(&projects, none, &SidebarRow::Home), None);
    }

    #[test]
    fn set_base_branch_falls_back_to_the_current_worktree() {
        let wt = PathBuf::from("C:/repo/wt");
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(false, None, none, &Some(wt.clone())), Some(wt));
        let none2 = |_id: SessionId| -> Option<WorkspaceKey> { None };
        assert_eq!(base_branch_target(false, None, none2, &None), None, "home has no base branch");
    }

    /// The row painters are free functions that only ever see a `Theme`, so the
    /// configured style has to survive the trip through it.
    #[test]
    fn the_theme_carries_the_configured_path_style() {
        let mut config = Config::default();
        config.ui.path_style.git_rows = PathStyle::Fish;
        config.ui.path_style.filename.bold = true;

        let theme = Theme::from_config(&config);
        assert_eq!(theme.path_style.git_rows, PathStyle::Fish);
        assert_eq!(theme.path_style.git_header, PathStyle::Full);
        assert!(theme.path_style.filename.bold);
    }

    /// With no override, `attention` reads the palette's yellow slot; a
    /// configured color must reach `Theme::attention`, not just the raw
    /// config field.
    #[test]
    fn sidebar_attention_overrides_the_palette_default() {
        let default_theme = Theme::from_config(&Config::default());
        assert_eq!(default_theme.attention, rgb_to_color32(Config::default().palette.normal[3]));

        let mut config = Config::default();
        config.ui.sidebar_attention = Some(Color32::from_rgb(0xff, 0xb8, 0x6c));
        let theme = Theme::from_config(&config);
        assert_eq!(theme.attention, Color32::from_rgb(0xff, 0xb8, 0x6c));
    }

    /// The header is the one site whose path is absolute, so it is the one that
    /// must convert before it abbreviates: fish-abbreviating the UNC spelling
    /// would produce `\\w\k\h\l\monorepo` instead of `~/G/monorepo`.
    #[cfg(windows)]
    #[test]
    fn the_git_header_converts_before_it_abbreviates() {
        let unc = std::path::Path::new(r"\\wsl.localhost\kali-linux\home\lev\Git\monorepo");
        let shown = crate::path_style::render(
            &crate::wsl::display_path(unc),
            crate::path_style::PathStyle::Fish,
            Some("/home/lev"),
        );
        assert_eq!(shown, "~/G/monorepo");
    }

    /// Every emphasis combination must resolve to a registered face; falling back
    /// to the base family for, say, bold-italic would silently drop the weight.
    /// An unemphasized span keeps whatever family the site already paints in.
    #[test]
    fn emphasis_resolves_to_the_registered_faces() {
        let plain = TextEmphasis::default();
        let bold = TextEmphasis { bold: true, ..Default::default() };
        let italic = TextEmphasis { italic: true, ..Default::default() };
        let both = TextEmphasis { bold: true, italic: true, ..Default::default() };

        for base in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
            assert_eq!(emphasis_family(&plain, &base), base);
            assert_eq!(
                emphasis_family(&bold, &base),
                egui::FontFamily::Name(crate::fonts::BOLD_FAMILY.into())
            );
            assert_eq!(
                emphasis_family(&italic, &base),
                egui::FontFamily::Name(crate::fonts::ITALIC_FAMILY.into())
            );
            assert_eq!(
                emphasis_family(&both, &base),
                egui::FontFamily::Name(crate::fonts::BOLD_ITALIC_FAMILY.into())
            );
        }
    }

    /// The job's spans, as `path_label` itself builds them via `zed_spans`,
    /// must reassemble into exactly what `render` produces, so the emphasis
    /// only changes how the text looks, never what it says.
    #[test]
    fn the_zed_job_spells_the_same_text_as_render() {
        for (path, home) in [
            ("path/to/file.txt", None),
            ("/a/b/c.txt", None),
            ("f.txt", None),
            ("/f.txt", None),
            ("/home/lev/Git/x/y.rs", Some("/home/lev")),
        ] {
            let parts = crate::path_style::split(path, PathStyle::Zed, home);
            let spans = zed_spans(&parts).concat();
            assert_eq!(spans, crate::path_style::render(path, PathStyle::Zed, home), "{path:?}");
        }
    }

    /// The header must stay text-selectable exactly as it was before
    /// `path_text` existed; a row must stay non-selectable so its own click
    /// wins the hit test instead of a text drag-select. Both the plain and
    /// the `Zed` `LayoutJob` branch build their own label, so both are
    /// checked here.
    #[test]
    fn only_the_header_path_is_selectable() {
        let mut config = Config::default();
        let ctx = egui::Context::default();

        for style in [PathStyle::Full, PathStyle::Zed] {
            config.ui.path_style.git_header = style;
            config.ui.path_style.git_rows = style;
            let theme = Theme::from_config(&config);

            for header in [true, false] {
                let mut sense = None;
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::Vec2::new(400.0, 100.0),
                    )),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        if header {
                            let resp = path_header_label(
                                ui,
                                "path/to/file.txt",
                                theme.text,
                                &theme,
                                style,
                                None,
                            );
                            sense = Some(resp.sense);
                        } else {
                            let (resp, _) =
                                git_path_label(ui, "path/to/file.txt", theme.text, &theme);
                            sense = Some(resp.sense);
                        }
                    });
                });

                let sense = sense.expect("the label must run inside the panel closure");
                assert_eq!(
                    sense.senses_drag(),
                    header,
                    "style {style:?} header {header}: {sense:?}"
                );
            }
        }
    }

    /// Every text a frame painted and whether it had to ellipsize, tooltips
    /// included — tooltips live in their own layer, so the only way to see one
    /// from a headless run is to read the shapes back out. A galley keeps the
    /// whole text even when it paints an ellipsis, so `elided` is what
    /// separates a clipped row from the tooltip spelling it out in full.
    fn painted_texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, bool)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, bool)>) {
            match shape {
                egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.galley.elided)),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {},
            }
        }
        let mut out = Vec::new();
        for clipped in shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// The x-coordinate of every painted glyph, keyed by its text — for
    /// asserting left-to-right screen order rather than the (reversed)
    /// right-to-left call order `row_with_trailing` lays widgets out in.
    fn painted_glyph_centers(shapes: &[egui::epaint::ClippedShape]) -> HashMap<String, f32> {
        fn walk(shape: &egui::Shape, out: &mut HashMap<String, f32>) {
            match shape {
                egui::Shape::Text(t) => {
                    out.insert(t.galley.text().to_owned(), t.pos.x);
                },
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {},
            }
        }
        let mut out = HashMap::new();
        for clipped in shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// Rest the pointer over a row and collect every text painted while it
    /// lingers there, tooltip included. The frames advance the clock past
    /// `tooltip_delay` and keep the pointer still, which is what egui waits
    /// for before opening one.
    ///
    /// The row is squeezed to `row_width` inside a roomy window, the way a
    /// narrow sidebar sits beside a wide terminal: the row must ellipsize
    /// while the tooltip still has space to spell the name out.
    fn texts_while_hovering(
        row_width: f32,
        row: impl FnMut(&mut egui::Ui),
    ) -> Vec<Vec<(String, bool)>> {
        texts_while_hovering_at(egui::Pos2::new(row_width / 2.0, 20.0), row_width, row)
    }

    /// `texts_while_hovering` over a chosen point rather than the row's middle.
    /// A button occupies a slot too small to hit by guessing at the layout, so
    /// its tests render once to learn where it landed and hover that.
    fn texts_while_hovering_at(
        hover: egui::Pos2,
        row_width: f32,
        row: impl FnMut(&mut egui::Ui),
    ) -> Vec<Vec<(String, bool)>> {
        frames_while_hovering_at(hover, row_width, row)
            .iter()
            .map(|shapes| painted_texts(shapes))
            .collect()
    }

    /// The shapes behind `texts_while_hovering_at`. A status badge exposes no
    /// rect to aim at, so its tests paint one pass with the pointer away to
    /// find where the glyph landed, then hover exactly that — which only holds
    /// because both passes lay out through this same function.
    fn frames_while_hovering_at(
        hover: egui::Pos2,
        row_width: f32,
        mut row: impl FnMut(&mut egui::Ui),
    ) -> Vec<Vec<egui::epaint::ClippedShape>> {
        let ctx = egui::Context::default();
        let mut seen = Vec::new();
        for frame in 0..8 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(600.0, 200.0),
                )),
                time: Some(frame as f64 * 0.25),
                events: if frame == 0 {
                    vec![egui::Event::PointerMoved(hover)]
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    // The sidebars turn label selection off, which drops the
                    // labels out of the interactive set — the harness has to
                    // match that or it tests a widget the app never builds.
                    ui.style_mut().interaction.selectable_labels = false;
                    ui.allocate_ui_with_layout(
                        egui::vec2(row_width, 60.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| row(ui),
                    );
                });
            });
            seen.push(output.shapes);
        }
        seen
    }

    /// Where each glyph painted, keyed by its text.
    fn painted_glyph_positions(
        shapes: &[egui::epaint::ClippedShape],
    ) -> HashMap<String, egui::Pos2> {
        fn walk(shape: &egui::Shape, out: &mut HashMap<String, egui::Pos2>) {
            match shape {
                egui::Shape::Text(t) => {
                    out.insert(t.galley.text().to_owned(), t.pos + t.galley.size() / 2.0);
                },
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {},
            }
        }
        let mut out = HashMap::new();
        for clipped in shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// Rest the pointer on a lone sidebar button and report whether its hint
    /// was painted. The button is rendered twice: once off-pointer to learn
    /// its slot, then again with the pointer resting in the middle of it.
    fn button_hint_painted(theme: &Theme, hint: &str) -> bool {
        let slot = std::cell::Cell::new(None);
        let mut button = |ui: &mut egui::Ui| {
            let resp = icon_tooltip(
                styled_icon_button(
                    ui,
                    &IconStyle::default(),
                    DEFAULT_CLOSE_ICON,
                    theme.text_muted,
                    theme,
                ),
                hint,
                theme.icon_tooltips,
            );
            slot.set(Some(resp.rect));
        };

        let off_pointer = egui::Pos2::new(-100.0, -100.0);
        let _ = texts_while_hovering_at(off_pointer, 140.0, &mut button);
        let centre = slot.get().expect("the button painted a slot").center();

        let frames = texts_while_hovering_at(centre, 140.0, &mut button);
        frames.iter().flatten().any(|(text, _)| text == hint)
    }

    /// Whether a tooltip spelled `name` out over the row that already paints
    /// it. The row paints the name once a frame, so a second paint in the same
    /// frame is the tooltip — true whether or not the row had room for it,
    /// which a plain "the full text appeared" check cannot tell apart.
    fn tooltip_shown(frames: &[Vec<(String, bool)>], name: &str) -> bool {
        frames.iter().any(|f| f.iter().filter(|(t, _)| t == name).count() >= 2)
    }

    /// Whether the row had to ellipsize `name` — the precondition every
    /// tooltip assertion below rests on.
    fn row_elided(frames: &[Vec<(String, bool)>], name: &str) -> bool {
        frames.iter().flatten().any(|(t, elided)| t == name && *elided)
    }

    /// A sidebar row too narrow for its name elides it, and egui offers the
    /// full text as a tooltip — but only to a widget the hit test marks
    /// hovered. The worktree row senses its click on the frame *around* the
    /// name, which takes that mark away from the label. Resting the pointer
    /// on such a row must still surface the whole name.
    #[test]
    fn hovering_an_elided_worktree_row_reveals_the_full_name() {
        let theme = Theme::from_config(&Config::default());
        let icons = crate::config::Icons::default();
        let wt = crate::projects::Worktree {
            name: "feature/a-branch-name-far-too-long-for-the-sidebar".to_owned(),
            path: PathBuf::from("/repo/wt"),
            branch: None,
            is_main: false,
            prunable: false,
            upstream: None,
        };

        let texts = texts_while_hovering(140.0, |ui| {
            worktree_row(
                ui,
                &wt,
                None,
                &wt.name,
                None,
                true,
                false,
                false,
                false,
                SessionActivity::Shell,
                false,
                &[],
                &icons,
                &theme,
            );
        });

        assert!(
            row_elided(&texts, &wt.name),
            "the row must be too narrow for the name, or the test proves nothing: {texts:?}"
        );
        assert!(
            tooltip_shown(&texts, &wt.name),
            "hovering the elided row painted no tooltip with the full name: {texts:?}"
        );
    }

    #[test]
    fn a_configured_size_is_clamped_to_the_slot() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { size: Some(400.0), ..Default::default() };
        let (_, font, _) =
            resolve_icon(&style, DEFAULT_WORKTREE_ICON, Color32::WHITE, 10.0, 10.0, &theme);
        assert!(
            font.size <= 10.0 * theme.ui_scale,
            "an oversized glyph must not overlap its neighbours"
        );

        let (_, font, _) =
            resolve_icon(&style, DEFAULT_CLOSE_ICON, Color32::WHITE, 12.0, 16.0, &theme);
        assert!(font.size <= 16.0 * theme.ui_scale, "a button glyph clamps to its own 16px slot");
    }

    /// With no config, every icon paints at its built-in size: buttons at 12
    /// inside a 16 slot, status markers at 10.
    #[test]
    fn an_unconfigured_icon_keeps_its_current_size() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle::default();
        let (_, font, _) =
            resolve_icon(&style, DEFAULT_CLOSE_ICON, Color32::WHITE, 12.0, 16.0, &theme);
        assert_eq!(font.size, 12.0 * theme.ui_scale);
    }

    #[test]
    fn a_configured_color_wins_over_the_site_default() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { color: Some(Color32::RED), ..Default::default() };
        let (_, _, color) =
            resolve_icon(&style, DEFAULT_WORKTREE_ICON, Color32::WHITE, 10.0, 10.0, &theme);
        assert_eq!(color, Color32::RED);
    }

    /// An unstyled icon must render `FontFamily::Proportional`, matching
    /// every icon call site with no `bold`/`italic` configured.
    #[test]
    fn an_unconfigured_icon_resolves_to_the_proportional_family() {
        let theme = Theme::from_config(&Config::default());
        let (_, font, _) = resolve_icon(
            &IconStyle::default(),
            DEFAULT_WORKTREE_ICON,
            Color32::WHITE,
            10.0,
            10.0,
            &theme,
        );
        assert_eq!(font.family, egui::FontFamily::Proportional);
    }

    /// `italic` alone (no `bold`) must resolve to the italic face, not the
    /// bold-italic one — the two flags are independent inputs to
    /// `ui_variant_family`.
    #[test]
    fn an_italic_icon_resolves_to_the_italic_family() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { italic: true, ..Default::default() };
        let (_, font, _) =
            resolve_icon(&style, DEFAULT_WORKTREE_ICON, Color32::WHITE, 10.0, 10.0, &theme);
        assert_eq!(font.family, egui::FontFamily::Name(crate::fonts::UI_ITALIC_FAMILY.into()));
    }

    /// A context with the three chrome variant families bound, as
    /// `fonts::install_terminal_fonts` leaves it in the app.  egui panics on a
    /// family it was never given, so any test that paints a bold/italic icon
    /// needs them registered first.
    fn ctx_with_ui_variant_faces() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let mono = fonts.families[&egui::FontFamily::Monospace].clone();
        for name in [
            crate::fonts::UI_BOLD_FAMILY,
            crate::fonts::UI_ITALIC_FAMILY,
            crate::fonts::UI_BOLD_ITALIC_FAMILY,
        ] {
            fonts.families.insert(egui::FontFamily::Name(name.into()), mono.clone());
        }
        ctx.set_fonts(fonts);
        ctx
    }

    /// The font family, size, and paint color of the first shape whose text
    /// matches `text`, or `None` if nothing painted it.
    fn painted_glyph_style(
        shapes: &[egui::epaint::ClippedShape],
        text: &str,
    ) -> Option<(egui::FontFamily, f32, Color32)> {
        fn walk(
            shape: &egui::Shape,
            text: &str,
            out: &mut Option<(egui::FontFamily, f32, Color32)>,
        ) {
            match shape {
                egui::Shape::Text(t) => {
                    if out.is_none() && t.galley.text() == text {
                        let font_id = t.galley.job.sections[0].format.font_id.clone();
                        let color = t.override_text_color.unwrap_or(t.fallback_color);
                        *out = Some((font_id.family, font_id.size, color));
                    }
                },
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, text, out)),
                _ => {},
            }
        }
        let mut out = None;
        for clipped in shapes {
            walk(&clipped.shape, text, &mut out);
        }
        out
    }

    /// End-to-end: a worktree row painting an upstream badge styled with a
    /// custom glyph, color, and weight through `upstream_badge` and
    /// `resolve_icon` — proving the wiring, not just the resolver in isolation.
    #[test]
    fn a_styled_upstream_badge_paints_its_configured_glyph_color_and_weight() {
        let theme = Theme::from_config(&Config::default());
        let ctx = ctx_with_ui_variant_faces();
        let mut icons = crate::config::Icons::default();
        icons.upstream_gone = IconStyle {
            glyph: Some("✕".to_string()),
            color: Some(Color32::RED),
            bold: true,
            italic: false,
            size: None,
        };
        let wt = crate::projects::Worktree {
            name: "feature/x".to_owned(),
            path: PathBuf::from("/repo/wt"),
            branch: None,
            is_main: false,
            prunable: false,
            upstream: Some(UpstreamState::Gone { upstream: "origin/x".into() }),
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(600.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                worktree_row(
                    ui,
                    &wt,
                    None,
                    &wt.name,
                    None,
                    true,
                    false,
                    false,
                    false,
                    SessionActivity::Shell,
                    false,
                    &[],
                    &icons,
                    &theme,
                );
            });
        });
        let (family, _, color) =
            painted_glyph_style(&output.shapes, "✕").expect("the configured glyph painted");
        assert_eq!(family, egui::FontFamily::Name(crate::fonts::UI_BOLD_FAMILY.into()));
        assert_eq!(color, Color32::RED);
    }

    /// The same badge unconfigured: the built-in glyph, the theme's built-in
    /// color, and the plain proportional family.
    #[test]
    fn an_unconfigured_upstream_badge_keeps_its_built_in_color_and_family() {
        let theme = Theme::from_config(&Config::default());
        let (family, _, color) = painted_glyph_style(&render_worktree_row_with_badges(), "✓")
            .expect("the default glyph painted");
        assert_eq!(family, egui::FontFamily::Proportional);
        assert_eq!(color, theme.upstream_level);
    }

    /// End-to-end: an unconfigured search icon painted through
    /// `panel_header_filter_ui` must land at exactly `theme.font_normal`,
    /// what `TextStyle::Small` painted at that call site. Pins the
    /// production `default_px`/`slot_px` expression, not a copy of it.
    #[test]
    fn an_unconfigured_search_icon_paints_at_the_small_text_style_size() {
        let theme = Theme::from_config(&Config::default());
        let mut filter = PanelFilter::new(&[]);
        filter.on_text("/");
        let icons = crate::config::Icons::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(400.0, 100.0),
            )),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                panel_header_filter_ui(ui, "Projects", &filter, &icons.search, &theme, true);
            });
        });
        let (_, size, _) = painted_glyph_style(&output.shapes, DEFAULT_SEARCH_ICON.as_str())
            .expect("the search icon painted");
        assert_eq!(size, theme.font_normal);
    }

    /// Every action button shares the same 16x16 slot while painting a
    /// glyph, color, and weight from config; the expand/collapse arrow
    /// exercises that path here.
    #[test]
    fn styled_icon_button_paints_a_configured_glyph_in_its_16px_slot() {
        let theme = Theme::from_config(&Config::default());
        let ctx = ctx_with_ui_variant_faces();
        let s = theme.ui_scale;
        let style = IconStyle {
            glyph: Some("▶".to_string()),
            color: Some(Color32::RED),
            bold: true,
            italic: false,
            size: None,
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(100.0, 100.0),
            )),
            ..Default::default()
        };
        let mut rect = None;
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                rect = Some(
                    styled_icon_button(
                        ui,
                        &style,
                        DEFAULT_PROJECT_COLLAPSED_ICON,
                        theme.text_dim,
                        &theme,
                    )
                    .rect,
                );
            });
        });

        let painted_size = rect.expect("the button painted").size();
        let expected_size = egui::vec2(16.0 * s, 16.0 * s);
        assert!(
            (painted_size - expected_size).length() < 0.01,
            "styled_icon_button must paint into a 16x16 slot: got {painted_size:?}, expected \
             {expected_size:?}"
        );
        let (family, size, color) =
            painted_glyph_style(&output.shapes, "▶").expect("the configured glyph painted");
        assert_eq!(family, egui::FontFamily::Name(crate::fonts::UI_BOLD_FAMILY.into()));
        assert_eq!(size, 12.0 * s, "a button glyph paints at 12px inside its 16px slot");
        assert_eq!(color, Color32::RED);
    }

    /// Rest the pointer on the worktree-row badge that painted `glyph` and
    /// report every text drawn while it lingers there.
    fn texts_while_hovering_badge(theme: &Theme, glyph: &str) -> Vec<Vec<(String, bool)>> {
        let icons = crate::config::Icons::default();
        let wt = crate::projects::Worktree {
            name: "wt".to_owned(),
            path: PathBuf::from("/repo/wt"),
            branch: None,
            is_main: false,
            prunable: false,
            upstream: Some(UpstreamState::Level { upstream: "origin/x".into() }),
        };
        let pr = PrInfo {
            number: 7,
            base_branch: "main".into(),
            url: String::new(),
            state: PrState::Open,
        };
        let mut render = |ui: &mut egui::Ui| {
            worktree_row(
                ui,
                &wt,
                None,
                &wt.name,
                Some(&pr),
                true,
                false,
                false,
                false,
                SessionActivity::Shell,
                false,
                &[],
                &icons,
                theme,
            );
        };

        texts_while_hovering_icon(&mut render, glyph)
    }

    /// Whether resting the pointer on the icon that painted `glyph` surfaces
    /// `hint`.
    fn hint_painted_over(row: impl FnMut(&mut egui::Ui), glyph: &str, hint: &str) -> bool {
        texts_while_hovering_icon(row, glyph).iter().flatten().any(|(text, _)| text == hint)
    }

    /// Find the icon that painted `glyph`, then hover it. Two passes: an icon
    /// exposes no rect to aim at, so the first pass reads the position back out
    /// of the shapes it drew.
    fn texts_while_hovering_icon(
        mut row: impl FnMut(&mut egui::Ui),
        glyph: &str,
    ) -> Vec<Vec<(String, bool)>> {
        const WIDTH: f32 = 220.0;
        let off_pointer = egui::Pos2::new(-100.0, -100.0);
        let probe = frames_while_hovering_at(off_pointer, WIDTH, &mut row);
        let at = painted_glyph_positions(probe.last().expect("the row painted"));
        let centre = *at.get(glyph).unwrap_or_else(|| panic!("no {glyph} icon painted: {at:?}"));

        texts_while_hovering_at(centre, WIDTH, &mut row)
    }

    /// Every icon a worktree row paints — buttons and status badges alike —
    /// explains itself on hover, and answers to one key. A row that senses its
    /// own frame outranks the icons inside it, so each of these would go quiet
    /// on its own `on_hover_text`.
    #[test]
    fn icon_tooltips_gate_every_worktree_row_icon() {
        for (icon_tooltips, want) in [(true, true), (false, false)] {
            let mut config = Config::default();
            config.ui.icon_tooltips = icon_tooltips;
            config.ui.sidebar_tooltips = SidebarTooltips::Off;
            let theme = Theme::from_config(&config);

            for (glyph, hint) in [
                (DEFAULT_UPSTREAM_LEVEL_ICON, "tracks origin/x"),
                (DEFAULT_PR_OPEN_ICON, "PR #7 — open"),
                (DEFAULT_CLOSE_ICON, "delete worktree and branch"),
                (DEFAULT_ADD_ICON, "new shell"),
            ] {
                let glyph = glyph.as_str();
                let texts = texts_while_hovering_badge(&theme, glyph);
                let shown = texts.iter().flatten().any(|(text, _)| text == hint);
                assert_eq!(shown, want, "icon_tooltips = {icon_tooltips}, icon {glyph}");
            }
        }
    }

    /// The session and home rows sense their own frames the same way, so their
    /// buttons need the same recovery as the worktree row's.
    #[test]
    fn icon_tooltips_reach_the_session_and_home_row_buttons() {
        let icons = crate::config::Icons::default();
        for (icon_tooltips, want) in [(true, true), (false, false)] {
            let mut config = Config::default();
            config.ui.icon_tooltips = icon_tooltips;
            config.ui.sidebar_tooltips = SidebarTooltips::Off;
            let theme = Theme::from_config(&config);

            let row = SessionRowData {
                id: 1,
                name: RowName::plain("zsh".to_owned()),
                needs_attention: false,
                activity: SessionActivity::Shell,
                is_active: true,
                is_displayed: true,
                managed: None,
            };
            let mut session = |ui: &mut egui::Ui| {
                session_row(ui, &row, false, false, false, &icons, &theme);
            };
            assert_eq!(
                hint_painted_over(&mut session, "×", "close session"),
                want,
                "session row, icon_tooltips = {icon_tooltips}"
            );

            let mut home = |ui: &mut egui::Ui| {
                home_row(ui, true, false, false, false, SessionActivity::Shell, &icons, &theme);
            };
            assert_eq!(
                hint_painted_over(&mut home, "+", "new shell"),
                want,
                "home row, icon_tooltips = {icon_tooltips}"
            );
        }
    }

    /// A sidebar button says what it does on hover, and `[ui] icon_tooltips`
    /// is what decides whether it may. The two settings are independent axes:
    /// silencing the row names must leave the button hints alone, or turning
    /// off one kind of tooltip would quietly cost the other.
    #[test]
    fn icon_tooltips_gate_the_button_hint() {
        for (icon_tooltips, want) in [(true, true), (false, false)] {
            let mut config = Config::default();
            config.ui.icon_tooltips = icon_tooltips;
            config.ui.sidebar_tooltips = SidebarTooltips::Off;
            let theme = Theme::from_config(&config);

            assert_eq!(
                button_hint_painted(&theme, "close session"),
                want,
                "icon_tooltips = {icon_tooltips}"
            );
        }
    }

    /// The letter a git row leads with is the whole report — `M`, `?`, `!` say
    /// nothing to a reader who does not already know porcelain. The row senses
    /// its own frame, so the badge needs the same recovery the sidebar icons do.
    #[test]
    fn icon_tooltips_gate_the_git_status_badge_hint() {
        for (icon_tooltips, want) in [(true, true), (false, false)] {
            let mut config = Config::default();
            config.ui.icon_tooltips = icon_tooltips;
            config.ui.sidebar_tooltips = SidebarTooltips::Off;
            let theme = Theme::from_config(&config);
            let palette = config.palette.clone();

            for (kind, glyph, hint) in [
                (ChangeKind::Modified, "M", "modified"),
                (ChangeKind::Untracked, "?", "untracked"),
                (ChangeKind::Conflicted, "!", "conflicted"),
            ] {
                let change = FileChange { path: "README.md".to_owned(), kind };
                let mut row = |ui: &mut egui::Ui| {
                    let _ = file_row(ui, &change, &theme, &palette, false);
                };
                assert_eq!(
                    hint_painted_over(&mut row, glyph, hint),
                    want,
                    "icon_tooltips = {icon_tooltips}, badge {glyph}"
                );
            }
        }
    }

    /// The slot a row leads with is a report rather than a button: it stands
    /// for the agent running in the session, or for the session asking to be
    /// looked at. Both say so on hover. The dot paints no text to aim at, so
    /// it is found through the slot the glyph occupies — one replaces the
    /// other in place.
    #[test]
    fn icon_tooltips_gate_the_status_slot_hint() {
        const WIDTH: f32 = 220.0;
        let icons = crate::config::Icons::default();
        let session = |attention, activity| SessionRowData {
            id: 1,
            name: RowName::plain("zsh".to_owned()),
            needs_attention: attention,
            activity,
            is_active: true,
            is_displayed: true,
            managed: None,
        };

        for (icon_tooltips, want) in [(true, true), (false, false)] {
            let mut config = Config::default();
            config.ui.icon_tooltips = icon_tooltips;
            config.ui.sidebar_tooltips = SidebarTooltips::Off;
            let theme = Theme::from_config(&config);

            let agent = session(false, SessionActivity::agent(Some("claude"), LiveState::Idle));
            let mut agent_row = |ui: &mut egui::Ui| {
                session_row(ui, &agent, false, false, false, &icons, &theme);
            };
            assert_eq!(
                hint_painted_over(&mut agent_row, DEFAULT_AGENT_ICON.as_str(), "claude is running",),
                want,
                "agent status, icon_tooltips = {icon_tooltips}"
            );

            let probe =
                frames_while_hovering_at(egui::Pos2::new(-100.0, -100.0), WIDTH, &mut agent_row);
            let slot = painted_glyph_positions(probe.last().expect("the row painted"))
                [DEFAULT_AGENT_ICON.as_str()];

            let loading =
                session(false, SessionActivity::agent(Some("claude"), LiveState::Working));
            let texts = texts_while_hovering_at(slot, WIDTH, |ui| {
                session_row(ui, &loading, false, false, false, &icons, &theme);
            });
            assert_eq!(
                texts.iter().flatten().any(|(text, _)| text == "claude is working"),
                want,
                "loading status, icon_tooltips = {icon_tooltips}"
            );

            let waiting = session(true, SessionActivity::Shell);
            let texts = texts_while_hovering_at(slot, WIDTH, |ui| {
                session_row(ui, &waiting, false, false, false, &icons, &theme);
            });
            assert_eq!(
                texts.iter().flatten().any(|(text, _)| text == "needs attention"),
                want,
                "attention dot, icon_tooltips = {icon_tooltips}"
            );
        }
    }

    /// An unconfigured action button paints its built-in glyph in the
    /// proportional family, at 12px inside its 16px slot, in the site's
    /// default colour.
    #[test]
    fn an_unconfigured_action_button_is_unchanged() {
        let theme = Theme::from_config(&Config::default());
        let icons = Icons::default();
        let (glyph, font, color) = resolve_icon(
            &icons.delete_worktree,
            DEFAULT_CLOSE_ICON,
            theme.text_muted,
            12.0,
            16.0,
            &theme,
        );
        assert_eq!(glyph, "×");
        assert_eq!(font.family, egui::FontFamily::Proportional);
        assert_eq!(font.size, 12.0 * theme.ui_scale);
        assert_eq!(color, theme.text_muted);
    }

    /// Styling one action button must not reach a sibling that shares its glyph.
    #[test]
    fn styling_the_destructive_button_leaves_its_siblings_alone() {
        let theme = Theme::from_config(&Config::default());
        let mut icons = Icons::default();
        icons.delete_worktree = IconStyle {
            glyph: Some("✖".into()),
            color: Some(Color32::RED),
            bold: true,
            ..Default::default()
        };

        let (glyph, font, color) = resolve_icon(
            &icons.delete_worktree,
            DEFAULT_CLOSE_ICON,
            theme.text_muted,
            12.0,
            16.0,
            &theme,
        );
        assert_eq!(glyph, "✖");
        assert_eq!(color, Color32::RED);
        assert_eq!(font.family, egui::FontFamily::Name(crate::fonts::UI_BOLD_FAMILY.into()));

        let (glyph, _, color) = resolve_icon(
            &icons.close_session,
            DEFAULT_CLOSE_ICON,
            theme.text_muted,
            12.0,
            16.0,
            &theme,
        );
        assert_eq!(glyph, "×");
        assert_eq!(color, theme.text_muted);
    }

    /// A table that styles a key without setting `glyph` (color/weight only)
    /// must still fall back to the site's `default_glyph` argument —
    /// `Icons::default()` never exercises this path, since its glyph is
    /// always set.
    #[test]
    fn a_glyphless_style_falls_back_to_the_site_default_glyph() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { color: Some(Color32::RED), bold: true, ..Default::default() };
        let (glyph, font, color) =
            resolve_icon(&style, DEFAULT_CLOSE_ICON, theme.text_muted, 12.0, 16.0, &theme);
        assert_eq!(glyph, DEFAULT_CLOSE_ICON.as_str());
        assert_eq!(color, Color32::RED);
        assert_eq!(font.family, egui::FontFamily::Name(crate::fonts::UI_BOLD_FAMILY.into()));
    }

    /// End-to-end: the delete-worktree button in a real row paints its own
    /// configured styling, and that styling does not leak onto the sibling
    /// new-shell button — pinning the `icons.delete_worktree` binding at its
    /// call site, not just `resolve_icon` in isolation. Wiring the wrong key
    /// at that call site (e.g. `icons.close_session` where
    /// `icons.delete_worktree` belongs) would still compile and still paint
    /// a glyph, but this would fail.
    #[test]
    fn the_delete_worktree_button_paints_its_own_key_not_its_siblings() {
        let theme = Theme::from_config(&Config::default());
        let ctx = ctx_with_ui_variant_faces();
        let mut icons = crate::config::Icons::default();
        let distinctive = Color32::from_rgb(200, 30, 220);
        icons.delete_worktree =
            IconStyle { color: Some(distinctive), bold: true, ..Default::default() };
        let wt = crate::projects::Worktree {
            name: "feature/x".to_owned(),
            path: PathBuf::from("/repo/wt"),
            branch: None,
            is_main: false,
            prunable: false,
            upstream: None,
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(600.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                worktree_row(
                    ui,
                    &wt,
                    None,
                    &wt.name,
                    None,
                    true,
                    false,
                    false,
                    false,
                    SessionActivity::Shell,
                    false,
                    &[],
                    &icons,
                    &theme,
                );
            });
        });

        let (delete_family, _, delete_color) =
            painted_glyph_style(&output.shapes, "×").expect("the delete button painted");
        assert_eq!(
            delete_color, distinctive,
            "the delete button must paint icons.delete_worktree's configured colour"
        );
        assert_eq!(delete_family, egui::FontFamily::Name(crate::fonts::UI_BOLD_FAMILY.into()));

        let (spawn_family, _, spawn_color) =
            painted_glyph_style(&output.shapes, "+").expect("the new-shell button painted");
        assert_eq!(
            spawn_color, theme.text_muted,
            "styling delete_worktree must not leak onto the sibling new-shell button"
        );
        assert_eq!(spawn_family, egui::FontFamily::Proportional);
    }

    /// `[ui] sidebar_tooltips` bounds the row tooltip on both sides: `off`
    /// withholds a name the panel cut off, and `always` offers one even for a
    /// name that fits — which is what keeps a sweep down the list from losing
    /// egui's instant-reopen grace every time a short name goes by.
    #[test]
    fn sidebar_tooltips_modes_bound_the_row_tooltip() {
        let icons = crate::config::Icons::default();
        let long = "feature/a-branch-name-far-too-long-for-the-sidebar";
        let short = "main";

        for (mode, name, want) in [
            (SidebarTooltips::Off, long, false),
            (SidebarTooltips::Elided, long, true),
            (SidebarTooltips::Elided, short, false),
            (SidebarTooltips::Always, long, true),
            (SidebarTooltips::Always, short, true),
        ] {
            let mut config = Config::default();
            config.ui.sidebar_tooltips = mode;
            let theme = Theme::from_config(&config);
            let wt = crate::projects::Worktree {
                name: name.to_owned(),
                path: PathBuf::from("/repo/wt"),
                branch: None,
                is_main: false,
                prunable: false,
                upstream: None,
            };

            let texts = texts_while_hovering(140.0, |ui| {
                worktree_row(
                    ui,
                    &wt,
                    None,
                    name,
                    None,
                    true,
                    false,
                    false,
                    false,
                    SessionActivity::Shell,
                    false,
                    &[],
                    &icons,
                    &theme,
                );
            });

            assert_eq!(
                row_elided(&texts, name),
                name == long,
                "{mode:?} on {name:?}: the harness must elide exactly the long name: {texts:?}"
            );
            assert_eq!(tooltip_shown(&texts, name), want, "{mode:?} on {name:?}: {texts:?}");
        }
    }

    #[test]
    fn the_upstream_tooltip_names_the_upstream_ref() {
        let icons = crate::config::Icons::default();
        let theme = Theme::from_config(&Config::default());
        let (_, _, _, tip) = upstream_badge(&icons, &theme, &UpstreamState::Diverged {
            upstream: "origin/x".into(),
            ahead: 2,
            behind: 1,
        });
        assert_eq!(tip, "tracks origin/x — 2 ahead, 1 behind");

        let (_, _, _, tip) = upstream_badge(&icons, &theme, &UpstreamState::Untracked);
        assert_eq!(tip, "no upstream configured");
    }

    /// A single headless frame of a worktree row carrying both a PR and an
    /// upstream state, so both trailing badges paint alongside the × and +
    /// buttons.
    fn render_worktree_row_with_badges() -> Vec<egui::epaint::ClippedShape> {
        let theme = Theme::from_config(&Config::default());
        let icons = crate::config::Icons::default();
        let wt = crate::projects::Worktree {
            name: "feature/x".to_owned(),
            path: PathBuf::from("/repo/wt"),
            branch: None,
            is_main: false,
            prunable: false,
            upstream: Some(UpstreamState::Level { upstream: "origin/x".into() }),
        };
        let pr = PrInfo {
            number: 1,
            base_branch: "main".into(),
            url: String::new(),
            state: PrState::Open,
        };

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(600.0, 200.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                worktree_row(
                    ui,
                    &wt,
                    None,
                    &wt.name,
                    Some(&pr),
                    true,
                    false,
                    false,
                    false,
                    SessionActivity::Shell,
                    false,
                    &[],
                    &icons,
                    &theme,
                );
            });
        });
        output.shapes
    }

    /// `row_with_trailing` lays the trailing group out right-to-left, so call
    /// order is the reverse of what the user sees.  Assert the rendering, not
    /// the call order — the two read as opposites.
    #[test]
    fn the_upstream_badge_paints_left_of_the_pr_badge_and_the_buttons() {
        let centers = painted_glyph_centers(&render_worktree_row_with_badges());
        let x = |g: &str| centers.get(g).copied().expect(g);
        assert!(x("✓") < x("⬤"), "upstream badge sits left of the PR badge");
        assert!(x("⬤") < x("+"), "badges sit left of the action buttons");
        assert!(x("+") < x("×"), "the existing button order is unchanged");
    }

    /// Session rows sense their click the same retroactive way, so a long
    /// shell title has to reach the pointer through the row too.
    #[test]
    fn hovering_an_elided_session_row_reveals_the_full_title() {
        let theme = Theme::from_config(&Config::default());
        let icons = crate::config::Icons::default();
        let row = SessionRowData {
            id: 1,
            name: RowName::plain("cargo test --workspace --all-features -- --nocapture".to_owned()),
            needs_attention: false,
            activity: SessionActivity::Shell,
            is_active: true,
            is_displayed: true,
            managed: None,
        };

        let texts = texts_while_hovering(140.0, |ui| {
            session_row(ui, &row, false, false, false, &icons, &theme);
        });

        assert!(
            row_elided(&texts, &row.name.text),
            "the row must be too narrow for the title, or the test proves nothing: {texts:?}"
        );
        assert!(
            tooltip_shown(&texts, &row.name.text),
            "hovering the elided row painted no tooltip with the full title: {texts:?}"
        );
    }

    /// The git panel's rows answer to the same mode as the left sidebar's, so
    /// a path the panel cut off is withheld under `off` and a path that fits
    /// is still offered under `always`. Both row kinds are checked: the diff
    /// row nests its path in a second layout to pin the +/- counts right, and
    /// that is exactly the kind of nesting that can cost a row its hover.
    #[test]
    fn sidebar_tooltips_modes_bound_the_git_row_tooltip() {
        let long = "alacritree/src/some/deeply/nested/module/file_name.rs";
        let short = "README.md";

        for (mode, path, want) in [
            (SidebarTooltips::Off, long, false),
            (SidebarTooltips::Elided, long, true),
            (SidebarTooltips::Elided, short, false),
            (SidebarTooltips::Always, long, true),
            (SidebarTooltips::Always, short, true),
        ] {
            let mut config = Config::default();
            config.ui.sidebar_tooltips = mode;
            let theme = Theme::from_config(&config);
            let palette = config.palette.clone();
            let change = crate::git_status::FileChange {
                path: path.to_owned(),
                kind: crate::git_status::ChangeKind::Modified,
            };
            let stat =
                crate::git_status::DiffStat { path: path.to_owned(), additions: 3, deletions: 1 };

            for (kind, is_diff) in [("file", false), ("diff", true)] {
                let texts = texts_while_hovering(140.0, |ui| {
                    if is_diff {
                        let _ = branch_diff_row(ui, &stat, &theme, &palette, false);
                    } else {
                        let _ = file_row(ui, &change, &theme, &palette, false);
                    }
                });

                assert_eq!(
                    row_elided(&texts, path),
                    path == long,
                    "{mode:?} on {kind} {path:?}: the harness must elide exactly the long path: \
                     {texts:?}"
                );
                assert_eq!(
                    tooltip_shown(&texts, path),
                    want,
                    "{mode:?} on {kind} {path:?}: {texts:?}"
                );
            }
        }
    }

    #[test]
    fn snapshot_parents_agree_with_the_row_model() {
        use crate::sidebar_focus::Parent;
        use crate::sidebar_nav::{self, SidebarRow};

        // Two projects, one collapsed, with sessions under the expanded one.
        let projects = vec![
            sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"]),
            sidebar_nav::tests::project("/b", false, &["/b/wt1"]),
        ];
        let live =
            vec![(None, 1), (Some(PathBuf::from("/a/wt1")), 2), (Some(PathBuf::from("/a/wt1")), 3)];
        let listed = sidebar_nav::tests::sessions_only(HashMap::from([
            (None, vec![1]),
            (Some(PathBuf::from("/a/wt1")), vec![2, 3]),
        ]));
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        for row in &rows {
            let id = snapshot.find(row).expect("every projected row is in the model");
            let arena_parent = match snapshot.parent(id) {
                Parent::Root => None,
                Parent::Node(p) => Some(snapshot.row(p).clone()),
                Parent::Detached => panic!("a projected row is never detached: {row:?}"),
            };
            assert_eq!(
                arena_parent,
                sidebar_nav::left_target(&rows, row),
                "arena parent must agree with the row model for {row:?}"
            );
        }

        // The collapsed project's worktree is in the model but not projected.
        let hidden = snapshot
            .find(&SidebarRow::Worktree(PathBuf::from("/b/wt1")))
            .expect("collapsed worktrees stay in the model");
        assert!(!snapshot.is_projected(hidden));
    }

    /// Herdr rows are navigable rows, so the arena has to carry them in the
    /// order the projection lists them.  Missing them parks the lockstep index
    /// on the first agent row, which marks every row below it unprojected and
    /// leaves the cursor with no node to sit on.
    #[test]
    fn herdr_rows_are_projected_under_the_workspace_they_are_listed_in() {
        use crate::sidebar_focus::Parent;
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"])];
        let live: Vec<(WorkspaceKey, SessionId)> = vec![(Some(PathBuf::from("/a/wt1")), 1)];
        let home_agent = SidebarRow::HerdrAgent(herdr::Side::Native, "term_home".into());
        let worktree_agent = SidebarRow::HerdrAgent(herdr::Side::Wsl("d".into()), "term_wt".into());
        let listed = sidebar_nav::ListedRows::from([
            (None, vec![sidebar_nav::WorkspaceEntry::Agent(
                herdr::Side::Native,
                "term_home".to_string(),
            )]),
            (Some(PathBuf::from("/a/wt1")), vec![
                sidebar_nav::WorkspaceEntry::Session(1),
                sidebar_nav::WorkspaceEntry::Agent(
                    herdr::Side::Wsl("d".into()),
                    "term_wt".to_string(),
                ),
            ]),
        ]);
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        for row in &rows {
            let id = snapshot.find(row).expect("every projected row is in the model");
            assert!(snapshot.is_projected(id), "{row:?} must stay navigable");
            let arena_parent = match snapshot.parent(id) {
                Parent::Root => None,
                Parent::Node(p) => Some(snapshot.row(p).clone()),
                Parent::Detached => panic!("a projected row is never detached: {row:?}"),
            };
            assert_eq!(
                arena_parent,
                sidebar_nav::left_target(&rows, row),
                "arena parent must agree with the row model for {row:?}"
            );
        }

        assert_eq!(
            snapshot.parent(snapshot.find(&home_agent).unwrap()),
            Parent::Node(snapshot.find(&SidebarRow::Home).unwrap()),
        );
        assert_eq!(
            snapshot.parent(snapshot.find(&worktree_agent).unwrap()),
            Parent::Node(snapshot.find(&SidebarRow::Worktree(PathBuf::from("/a/wt1"))).unwrap()),
        );
    }

    #[test]
    fn a_session_below_the_listing_threshold_is_still_in_the_model() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1"])];
        // One live session in the worktree.  The real rule needs two before it
        // lists any, so this one is live but unprojected.
        let live = vec![(Some(PathBuf::from("/a/wt1")), 7)];
        let listed = {
            let mut l = sidebar_nav::ListedRows::new();
            let entries = workspace_entries(&[7], Vec::new(), false);
            assert!(entries.is_empty(), "the threshold rule must actually drop this session");
            if !entries.is_empty() {
                l.insert(Some(PathBuf::from("/a/wt1")), entries);
            }
            l
        };
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        let id = snapshot
            .find(&SidebarRow::Session(7))
            .expect("a live session is in the model whatever the listing threshold says");
        assert!(!snapshot.is_projected(id), "but it is not a navigable row");
    }

    #[test]
    fn a_session_whose_project_is_gone_is_detached_not_deleted() {
        use crate::sidebar_focus::Parent;
        use crate::sidebar_nav::{self, SidebarRow};

        // `remove_project` drops the project but keeps its sessions running.
        let projects: Vec<crate::projects::Project> = vec![];
        let live = vec![(Some(PathBuf::from("/orphan/wt1")), 5)];
        let listed = sidebar_nav::ListedRows::new();
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        let id = snapshot.find(&SidebarRow::Session(5)).expect("the session is still running");
        assert_eq!(
            snapshot.parent(id),
            Parent::Detached,
            "an orphan must not become a sibling of Home"
        );
    }

    #[test]
    fn a_worktree_being_deleted_reads_as_gone_immediately() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"])];
        let listed = sidebar_nav::ListedRows::new();
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let doomed = PathBuf::from("/a/wt2");
        let snapshot = build_sidebar_snapshot(
            &projects,
            &[],
            &listed,
            &rows,
            Some(doomed.as_path()),
            Default::default(),
        );

        assert_eq!(
            snapshot.find(&SidebarRow::Worktree(doomed)),
            None,
            "the async git delete has not finished, but the row must not read as present"
        );
        assert!(snapshot.find(&SidebarRow::Worktree(PathBuf::from("/a/wt1"))).is_some());
    }

    /// The rows below a worktree being deleted must stay navigable.
    ///
    /// The projection is built before the deletion is known, so it still
    /// lists the doomed worktree.  The builder consumes that projection in
    /// lockstep, so skipping the worktree without stepping the index leaves
    /// it parked on a row nothing will ever match again — every later node
    /// reads as unprojected, and the cursor repair treats an unprojected row
    /// as one that has gone away.
    #[test]
    fn rows_below_a_deleted_worktree_stay_navigable() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects =
            vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2", "/a/wt3"])];
        let listed = sidebar_nav::ListedRows::new();
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let doomed = PathBuf::from("/a/wt2");
        let snapshot = build_sidebar_snapshot(
            &projects,
            &[],
            &listed,
            &rows,
            Some(doomed.as_path()),
            Default::default(),
        );

        let below = snapshot
            .find(&SidebarRow::Worktree(PathBuf::from("/a/wt3")))
            .expect("the worktree below the deleted one is still in the tree");
        assert!(
            snapshot.is_projected(below),
            "a row below the one being deleted must still be navigable"
        );
    }

    /// A checkout the liveness cache calls gone offers no workspace, so the
    /// agent working in it matches nothing and lists under Home.  Matched to
    /// the removed worktree instead, its row's Enter could only refuse.
    #[test]
    fn a_gone_worktree_offers_no_workspace_to_an_agent() {
        use crate::sidebar_nav;

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"])];
        let gone = PathBuf::from("/a/wt2");
        let workspaces = herdr_workspaces(&projects, |path| Some(path == gone));
        assert_eq!(workspaces, vec![PathBuf::from("/a/wt1")]);

        let agent = herdr::Agent {
            terminal_id: "t1".into(),
            pane_id: "w1:p1".into(),
            kind: None,
            title: None,
            status: herdr::Status::Idle,
            focused: false,
            cwd: Some(gone.to_string_lossy().into_owned()),
            foreground_cwd: None,
        };
        assert_eq!(
            herdr::match_workspace(&agent, &herdr::Side::Native, &workspaces),
            None,
            "an agent under a removed checkout falls back to Home"
        );
    }

    /// The lockstep walk follows the listing, not the session vector.
    ///
    /// Attaching to the second pane first leaves the two sessions in the
    /// opposite order to herdr's, and a walk that trusted the vector would
    /// push them the wrong way round, match neither against the projection
    /// and trip its own assert.
    #[test]
    fn the_snapshot_walk_follows_the_listing_not_the_session_vector() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1"])];
        let wt = Some(PathBuf::from("/a/wt1"));
        // Attached in the order 9 then 4; herdr lists the panes 4 then 9.
        let live = vec![(wt.clone(), 9), (wt.clone(), 4)];
        let listed = sidebar_nav::ListedRows::from([(wt.clone(), vec![
            sidebar_nav::WorkspaceEntry::Session(4),
            sidebar_nav::WorkspaceEntry::Session(9),
        ])]);
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot =
            build_sidebar_snapshot(&projects, &live, &listed, &rows, None, Default::default());

        assert_eq!(rows, vec![
            SidebarRow::Home,
            SidebarRow::Project(PathBuf::from("/a")),
            SidebarRow::Worktree(PathBuf::from("/a/wt1")),
            SidebarRow::Session(4),
            SidebarRow::Session(9),
        ]);
        for row in &rows {
            let id = snapshot.find(row).expect("every projected row is in the model");
            assert!(snapshot.is_projected(id), "{row:?} must stay navigable");
        }
    }

    /// Same lockstep hazard, with the doomed worktree carrying a herdr row:
    /// the agent rows it owns have to be stepped over as well.
    #[test]
    fn rows_below_a_deleted_worktree_with_a_herdr_row_stay_navigable() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects =
            vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2", "/a/wt3"])];
        let doomed = PathBuf::from("/a/wt2");
        let listed = sidebar_nav::ListedRows::from([(Some(doomed.clone()), vec![
            sidebar_nav::WorkspaceEntry::Agent(herdr::Side::Native, "term_doomed".to_string()),
        ])]);
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot = build_sidebar_snapshot(
            &projects,
            &[],
            &listed,
            &rows,
            Some(doomed.as_path()),
            Default::default(),
        );

        let below = snapshot
            .find(&SidebarRow::Worktree(PathBuf::from("/a/wt3")))
            .expect("the worktree below the deleted one is still in the tree");
        assert!(snapshot.is_projected(below));
    }

    /// Dispatch cannot catch a wrong pairing: `toggle` drops an identity the
    /// panel does not allow, and an action with no arm falls through to the
    /// scroll handler.  Swapping two identities here is otherwise invisible.
    #[test]
    fn the_projects_filter_actions_map_to_their_identities() {
        for (action, identity) in [
            (NamedAction::ToggleSessionsFilter, Some('s')),
            (NamedAction::ToggleAttentionFilter, Some('a')),
            (NamedAction::TogglePrOpenFilter, Some('o')),
            (NamedAction::TogglePrDraftFilter, Some('d')),
            (NamedAction::TogglePrMergedFilter, Some('m')),
            (NamedAction::TogglePrClosedFilter, Some('c')),
            (NamedAction::ClearProjectFilters, None),
            (NamedAction::ToggleModifiedFilter, None),
            (NamedAction::ToggleDeletedFilter, None),
            (NamedAction::ToggleUntrackedFilter, None),
            (NamedAction::ToggleSearchScope, None),
            (NamedAction::RefreshPrStatus, None),
            (NamedAction::Paste, None),
        ] {
            assert_eq!(project_filter_identity(action), identity, "{action:?}");
            if let Some(key) = identity {
                assert!(
                    project_filter_toggles(true).contains(&key),
                    "{action:?} maps to {key}, which the panel would drop"
                );
            }
        }
    }

    #[test]
    fn the_git_filter_actions_map_to_their_identities() {
        for (action, identity) in [
            (NamedAction::ToggleModifiedFilter, Some('m')),
            (NamedAction::ToggleDeletedFilter, Some('d')),
            (NamedAction::ToggleUntrackedFilter, Some('u')),
            (NamedAction::ClearGitFilters, None),
            (NamedAction::ToggleSessionsFilter, None),
            (NamedAction::ToggleAttentionFilter, None),
            (NamedAction::TogglePrOpenFilter, None),
            (NamedAction::TogglePrDraftFilter, None),
            (NamedAction::TogglePrMergedFilter, None),
            (NamedAction::TogglePrClosedFilter, None),
            (NamedAction::Paste, None),
        ] {
            assert_eq!(git_filter_identity(action), identity, "{action:?}");
            if let Some(key) = identity {
                assert!(
                    GIT_FILTER_TOGGLES.contains(&key),
                    "{action:?} maps to {key}, which the panel would drop"
                );
            }
        }
    }

    #[test]
    fn the_pr_identities_exist_only_when_polling_does() {
        assert_eq!(project_filter_toggles(false), &['s', 'a']);
        assert_eq!(project_filter_toggles(true), &['s', 'a', 'o', 'd', 'm', 'c']);
    }

    /// Guards the staging dependency: the four PR actions already dispatch to
    /// `project_filter.toggle`, and `toggle` silently ignores an identity the
    /// filter does not allow — so a narrow slice here makes them dead keys.
    #[test]
    fn the_pr_actions_reach_a_configured_projects_filter() {
        let mut f = PanelFilter::new(project_filter_toggles(true));
        for key in ['o', 'd', 'm', 'c'] {
            f.toggle(key);
            assert!(f.is_toggled(key), "{key} must be a live identity");
        }
    }

    #[test]
    fn any_pr_toggle_active_ignores_the_non_pr_identities() {
        let mut f = PanelFilter::new(project_filter_toggles(true));
        assert!(!any_pr_toggle_active(&f, SearchScope::Filtered));
        f.toggle('s');
        assert!(
            !any_pr_toggle_active(&f, SearchScope::Filtered),
            "a session toggle is not a PR toggle"
        );
        f.toggle('o');
        assert!(any_pr_toggle_active(&f, SearchScope::Filtered));
    }

    /// A search under `All` stands the toggles down for row selection, so the
    /// PR dimension narrows nothing — polling collapsed projects for it and
    /// rebuilding on every banked result would both be pure cost.
    #[test]
    fn a_stood_down_pr_toggle_does_not_read_as_active() {
        let mut f = PanelFilter::new(project_filter_toggles(true));
        f.toggle('o');
        f.on_text("/");
        f.on_text("a");

        assert!(any_pr_toggle_active(&f, SearchScope::Filtered));
        assert!(!any_pr_toggle_active(&f, SearchScope::All));
    }

    /// The reconciler must not churn for users who never touch a PR filter:
    /// every banked result would otherwise rebuild the row set.
    #[test]
    fn the_generation_reaches_the_reconciler_only_while_filtering() {
        assert_eq!(pr_generation_for(7, false), 0);
        assert_eq!(pr_generation_for(7, true), 7);
    }

    #[test]
    fn a_pr_filter_reaches_into_collapsed_projects() {
        assert!(!should_poll_pr(true, false, false), "collapsed and unfiltered: no lookup");
        assert!(should_poll_pr(true, false, true), "a PR filter must see collapsed rows");
        assert!(should_poll_pr(true, true, false));
        assert!(!should_poll_pr(false, true, true), "disabled means never");
    }

    /// The same path can be a worktree of two projects, and `PrCache` is keyed
    /// by path alone — two pollers would burn a `gh` process per frame.
    #[test]
    fn a_repeated_path_is_polled_once_but_rendered_everywhere() {
        let mut memo: HashMap<PathBuf, Option<PrInfo>> = HashMap::new();
        let lookups = std::cell::Cell::new(0);
        let path = PathBuf::from("/repo/wt");

        let poll = || {
            lookups.set(lookups.get() + 1);
            Some(PrInfo {
                number: 1,
                base_branch: "master".into(),
                url: String::new(),
                state: PrState::Open,
            })
        };

        let first = resolve_pr_info(&mut memo, &path, true, &poll);
        let second = resolve_pr_info(&mut memo, &path, true, &poll);

        assert_eq!(lookups.get(), 1, "one lookup per path per frame");
        assert!(second.is_some(), "the duplicate row still renders its badge");
        assert_eq!(first.map(|i| i.number), second.map(|i| i.number));

        let ineligible = resolve_pr_info(&mut memo, &PathBuf::from("/repo/other"), false, &poll);
        assert_eq!(lookups.get(), 1, "an ineligible path never runs the lookup");
        assert!(ineligible.is_none());
    }

    /// The palette asks for a comfortable fixed width, but a window too narrow
    /// to hold it must still show the whole modal, margins included.
    #[test]
    fn the_palette_shrinks_to_fit_a_narrow_window() {
        assert_eq!(
            palette_content_width(1.0, 1920.0),
            PALETTE_WIDTH,
            "a wide window gets the comfortable width unchanged"
        );

        for screen in [1000.0_f32, 820.0, 700.0, 520.0, 400.0] {
            let outer = palette_content_width(1.0, screen) + 2.0 * modal_pad_x(1.0);
            assert!(
                outer <= screen * PALETTE_SCREEN_FRACTION + 0.5,
                "at {screen}px the modal is {outer}px, past its share of the window"
            );
        }
    }

    #[test]
    fn the_palette_never_asks_for_a_negative_width() {
        assert!(palette_content_width(1.0, 10.0) >= 0.0);
    }

    /// A window wide enough keeps the fixed grid, so the columns line up exactly
    /// where they always have.
    #[test]
    fn wide_columns_keep_the_fixed_grid() {
        let cols = PaletteColumns::new(1.0, 760.0);
        assert_eq!(cols.action, 200.0);
        assert_eq!(cols.keys, 180.0);
        assert_eq!(cols.desc, 760.0 - 2.0 * 10.0 - 2.0 * 14.0 - 380.0);
        assert!(!cols.narrow, "a wide palette ellipsizes its columns rather than wrapping them");
    }

    /// Too narrow for the fixed grid, every column shrinks by the same factor
    /// rather than the last one running off the edge.
    #[test]
    fn narrow_columns_shrink_together_and_stay_inside_the_row() {
        for width in [520.0_f32, 440.0, 360.0, 240.0, 120.0] {
            let cols = PaletteColumns::new(1.0, width);
            assert!(cols.narrow, "at {width}px the fixed grid cannot fit");
            let right = cols.keys_x(0.0) + cols.keys;
            assert!(
                right <= width - cols.pad + 0.5,
                "at {width}px the keys column ends at {right}, past the row"
            );
            assert!(cols.desc > 0.0 && cols.action > 0.0 && cols.keys > 0.0);
            assert!((cols.action / cols.desc - 200.0 / 160.0).abs() < 1e-3);
            assert!((cols.keys / cols.desc - 180.0 / 160.0).abs() < 1e-3);
        }
    }

    /// The grid does not jump as it crosses from fixed to proportional.
    #[test]
    fn the_columns_are_continuous_across_the_narrow_threshold() {
        let fixed = PaletteColumns::new(1.0, 588.0);
        let narrow = PaletteColumns::new(1.0, 587.0);
        assert!(!fixed.narrow && narrow.narrow);
        assert!((fixed.desc - narrow.desc).abs() < 1.0);
        assert!((fixed.action - narrow.action).abs() < 1.0);
        assert!((fixed.keys - narrow.keys).abs() < 1.0);
    }

    #[test]
    fn only_a_cut_column_offers_its_full_text_on_hover() {
        assert_eq!(elided_hover(&[(false, "Copy"), (false, "Copy"), (false, "Ctrl+C")]), None);
        assert_eq!(
            elided_hover(&[
                (false, "Increase the font size"),
                (true, "IncreaseFontSize"),
                (true, "Ctrl+Plus, Ctrl+="),
            ]),
            Some("IncreaseFontSize\nCtrl+Plus, Ctrl+=".to_string())
        );
    }
}
