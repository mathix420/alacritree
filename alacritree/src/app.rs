use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use alacritree_checkout_hooks::{CheckoutEvent, CheckoutHook, CheckoutHooks};
use alacritree_forge::{PrInfo, PrState};
use eframe::CreationContext;
use egui::{Color32, Context, Frame, Margin, RichText, ScrollArea, SidePanel, Stroke};

use serde_json::{Value, json};

use crate::bindings::{BindingAction, NamedAction, action};
use crate::clipboard::{self, Target};
use crate::colors::rgb_to_color32;
use crate::command_palette::{self, CommandPalette, PaletteAction, PaletteItem};
use crate::config::{
    BakedGlyph, Config, DEFAULT_ADD_ICON, DEFAULT_BLOCKED_SYMBOL, DEFAULT_CLOSE_ICON,
    DEFAULT_DONE_SYMBOL, DEFAULT_FILLED_MARK, DEFAULT_HOLLOW_MARK, DEFAULT_HOME_ICON,
    DEFAULT_PR_CLOSED_ICON, DEFAULT_PR_DRAFT_ICON, DEFAULT_PR_MERGED_ICON, DEFAULT_PR_OPEN_ICON,
    DEFAULT_PROJECT_COLLAPSED_ICON, DEFAULT_PROJECT_EXPANDED_ICON, DEFAULT_REFRESH_ICON,
    DEFAULT_REORDER_ICON, DEFAULT_SEARCH_ICON, DEFAULT_SESSION_ICON,
    DEFAULT_UPSTREAM_DIVERGED_ICON, DEFAULT_UPSTREAM_GONE_ICON, DEFAULT_UPSTREAM_LEVEL_ICON,
    DEFAULT_UPSTREAM_UNTRACKED_ICON, DEFAULT_WORKTREE_ICON, DEFAULT_WORKTREE_MAIN_ICON, FontConfig,
    IconStyle, Icons, LastSessionClose, PathStyleConfig, ScrollAlign, ScrollbarStyle, SearchDepth,
    SearchScope, SidebarFocus, SidebarTooltips, StatusIndicators, TextEmphasis, UiFont, UiTheme,
    profile_command,
};
use crate::crash_log::{self, ExitReason};
use crate::forge::Forge;
use crate::git_nav::{self, GitSection, SectionCount};
use crate::in_flight::{Finished, InFlight};
use crate::modal_gate::{ModalGate, ModalKind};
use crate::multiplexer::{
    AttachFocus, Managed, MultiplexerSession, Multiplexers, PaneKey, PaneStatus, PaneTarget, Side,
};
use crate::panel_filter::{self, PanelFilter};
use crate::path_style::PathStyle;
use crate::pr_status::{self, PrCache};
use crate::projects::{Discovered, NotAProject, Project, project_json};
use crate::session::{
    self, Attachment, AttentionVerdict, LiveState, PendingAttention, Session, SessionActivity,
    SessionId, SessionKind, ShellCommand, ShownState, TermSize, poll_attention_debounce,
};
use crate::shell_decision::{ShellDecision, shell_decision};
use crate::sidebar_model::{SidebarInputs, SidebarModel, Step};
use crate::sidebar_nav::{self, SidebarRow, StepTarget};
use crate::state::{self, PersistedProject};
use crate::status_cache::StatusCache;
use crate::workspace::WorkspaceKey;
use crate::worktree::{self as wt, CreateRequest, Progress};
use crate::wsl::{self, ShellChoice};
use crate::wsl_helper::{self, WslProbe};
use crate::{
    clipboard_image, file_drop, ipc, jobs, mouse_hide, notify, paste, path_style, scratchpad,
    sidebar_focus, terminal_view, worktree_liveness,
};
use alacritree_vcs::{Checkout, Dirty, Liveness, UpstreamState, VersionControl};

mod actions;
mod focus;
mod git_panel;
mod ipc_handler;
mod modals;
mod palette;
mod panes;
mod session_list;
mod sidebar;
mod widgets;

pub(crate) use actions::{Action, ActionOrigin};
use focus::DeferredClose;
use modals::{BaseBranchPicker, CreateState, DeleteRequest, ProjectRemoveState, RenameState};
use panes::managed_tooltip;
use session_list::SessionList;
use sidebar::{
    PaintedIcons, PaneRowData, SessionRowData, WorkspaceRowData, any_pr_toggle_active,
    project_filter_toggles, session_row_name,
};
use widgets::{
    ATTENTION_HINT, ICON_CLUSTER_SPACING, IconHints, ROW_STATUS_ICON_W, RowStatus,
    apply_scrollbar_style, attention_mark, braille_loader, icon_tooltip, name_tooltip,
    paint_cursor_outline, paint_row_status_icon, paint_status_mark, path_text, resolve_icon,
    row_status_icon_size, row_with_trailing, session_status_mark, styled_icon_button,
    truncating_label,
};

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
    /// Colors for agent status marks, mapped from the ANSI palette the way
    /// the PR and upstream badges are.
    state_colors: StateColors,
    /// Which glyph set agent status marks draw from.
    status_indicators: StatusIndicators,
    /// Logical-pixel size for headings (titles like "Projects", "Git").
    /// `FontConfig::UI_HEADING_RATIO` of the terminal font size.
    font_heading: f32,
    /// Logical-pixel size for normal UI text (rows, captions, button labels).
    /// `FontConfig::UI_NORMAL_RATIO` of the terminal font size, which keeps the
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
    path_style: PathStyleConfig<Color32>,
    /// When a row spells its full name out on hover.
    sidebar_tooltips: SidebarTooltips,
    /// Whether a sidebar button says what it does on hover.
    icon_tooltips: bool,
    /// Where a row a sidebar scrolled to is parked; `None` is egui's own
    /// minimal scroll.
    scroll_align: Option<egui::Align>,
    /// Error and success text, the palette's red and green.
    error: Color32,
    ok: Color32,
    /// The scratchpad editor's text and its placeholder hint.
    editor_text: Color32,
    editor_hint: Color32,
    git: GitColors,
}

/// One color per git change kind, taken from the terminal palette.
#[derive(Debug, Clone, Copy)]
struct GitColors {
    added: Color32,
    modified: Color32,
    deleted: Color32,
    renamed: Color32,
    conflicted: Color32,
}

/// One colour per agent state that has one of its own.  Working paints in the
/// accent and a ping in the attention colour, both of which the rest of the
/// sidebar already uses.
#[derive(Debug, Clone, Copy)]
struct StateColors {
    blocked: Color32,
    done: Color32,
    idle: Color32,
    unknown: Color32,
}

impl Theme {
    fn from_config(config: &Config) -> Self {
        let terminal_bg = rgb_to_color32(config.palette.bg);
        let editor_text = rgb_to_color32(config.palette.fg);
        let sidebar_bg = config.ui.sidebar_background.map_or(terminal_bg, rgb_to_color32);
        let text = rgb_to_color32(config.ui.sidebar_foreground.unwrap_or(config.palette.fg));
        let accent = rgb_to_color32(config.ui.sidebar_accent.unwrap_or(config.palette.normal[4])); // ANSI blue
        let attention =
            rgb_to_color32(config.ui.sidebar_attention.unwrap_or(config.palette.normal[3])); // ANSI yellow
        let border =
            config.ui.sidebar_border.map_or_else(|| lighten(sidebar_bg, 0.10), rgb_to_color32);
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
            state_colors: StateColors {
                blocked: rgb_to_color32(config.palette.normal[1]), // red
                done: rgb_to_color32(config.palette.normal[6]),    // cyan, herdr's teal
                idle: rgb_to_color32(config.palette.normal[2]),    // green
                unknown: text_muted,
            },
            status_indicators: config.ui.status_indicators,
            font_heading,
            font_normal,
            ui_scale: font_normal / 11.25,
            focus_outline: FocusOutlineTheme {
                sidebar: config.ui.focus_outline.sidebar,
                terminal: config.ui.focus_outline.terminal,
                color: config.ui.focus_outline.color.map_or(accent, rgb_to_color32),
                thickness: config.ui.focus_outline.thickness,
            },
            path_style: config.ui.path_style.map_colors(rgb_to_color32),
            sidebar_tooltips: config.ui.sidebar_tooltips,
            icon_tooltips: config.ui.icon_tooltips,
            scroll_align: egui_scroll_align(config.ui.sidebar_scroll_align),
            error: rgb_to_color32(config.palette.normal[1]),
            ok: rgb_to_color32(config.palette.normal[2]),
            editor_text,
            editor_hint: blend_toward(editor_text, terminal_bg, 0.55),
            git: GitColors {
                added: rgb_to_color32(config.palette.normal[2]),
                modified: rgb_to_color32(config.palette.normal[3]),
                deleted: rgb_to_color32(config.palette.normal[1]),
                renamed: rgb_to_color32(config.palette.normal[4]),
                conflicted: rgb_to_color32(config.palette.bright[1]),
            },
        }
    }
}

fn egui_scroll_align(align: ScrollAlign) -> Option<egui::Align> {
    match align {
        ScrollAlign::Minimal => None,
        ScrollAlign::Center => Some(egui::Align::Center),
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
    // modals, popups, and tooltips (`Foreground`/`Tooltip`). Otherwise the
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
/// `layer_id_at` resolves only floating `Area` layers. `None` means the
/// press reached the background panels. While a modal is open egui resolves
/// *every* position to the modal's layer, so presses never register here
/// until the modal closes.
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

/// How long after the user's last event the worktree liveness tick keeps
/// asking for frames.  Long enough that a `git worktree remove` typed at a
/// prompt finishes and greys its row; short enough that a window left open
/// goes back to producing no frames at all.
const PROBE_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

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

/// A session whose PTY output wakes the egui loop that paints it.
type AppSession = Session<Context>;

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
    /// Runtime copy of `[ui] sessions_filter_counts_detached`.  Like the
    /// display toggles above, the config is only the startup default and
    /// nothing is persisted.
    sessions_filter_counts_detached: bool,
    sidebar: sidebar::Sidebar,
    /// The focus toggle opened a hidden sidebar; returning focus closes it
    /// again so a keyboard round trip leaves the layout untouched.
    sidebar_auto_shown: bool,
    /// The workspace and session the projects panel last scrolled to, so a
    /// change is detected by comparison rather than by every writer of those
    /// two fields remembering to raise a flag.  Written only once a scroll
    /// actually fires, so a change whose row renders nowhere is retried.
    last_followed: (WorkspaceKey, Option<SessionId>),
    git_panel: git_panel::GitPanel,
    sidebar_focus_state: focus::SidebarFocusState,
    /// `[ui] search_depth`: whether a projects-panel query also matches
    /// session titles and multiplexer pane names.  Not runtime-toggled.
    search_depth: SearchDepth,
    /// The Ctrl+K command palette (query, selection, matcher). Transient:
    /// never persisted.
    palette: CommandPalette,
    sessions: SessionList,
    current_workspace: WorkspaceKey,
    projects: Vec<Project>,
    pr_cache: PrCache<Forge>,
    /// The enabled version control backends, in the order they claim a root.
    vcs_backends: Vec<crate::vcs::Vcs>,
    /// Renders `[ui] worktree_name` / `project_name` templates at paint time.
    row_labels: crate::row_label::LabelTemplates,
    config: Config,
    theme: Theme,
    icons: PaintedIcons,
    /// `config.bindings` with its keys converted for matching.
    shortcuts: crate::shortcut::Shortcuts,
    modals: modals::Modals,
    /// Worktrees whose checkout hooks already ran `on_opened` this app run, so
    /// opening more shells there doesn't re-run every hooked tool.
    hooks_opened: HashSet<PathBuf>,
    /// Fire-and-forget jobs whose result nothing reads, such as checkout hook
    /// runs, image-cache sweeps and link opens. Held because dropping a `Job`
    /// cancels work not yet started, and dropping right after submitting
    /// would race the pool for nothing. Drained once a frame.
    detached_jobs: Vec<jobs::Job<()>>,
    notify_rx: Receiver<SessionId>,
    /// Requests from IPC connection threads, drained once per frame.
    ipc_rx: Option<Receiver<ipc::server::AppCall>>,
    /// Held for its Drop: unlinks the socket file on shutdown.
    _ipc_socket: Option<ipc::server::SocketHandle>,
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
    /// boots, and native discovery takes tens of milliseconds on a project
    /// with many worktrees.  Results are adopted in `poll_project_refreshes`.
    ///
    /// IPC callers are answered only once the result is live, since a client
    /// that refreshes a project to act on the new worktree list would
    /// otherwise race its own request.
    project_refreshes: InFlight<PathBuf, Discovered>,
    /// PTYs opened on a worker, adopted in `poll_pending_spawns`.  A client
    /// that creates a session to write to it is answered once the PTY is
    /// live, or it would race its own shell.
    pending_spawns: InFlight<SessionId, std::io::Result<Attachment>>,
    /// Every multiplexer alacritree hosts panes from, each with its own
    /// listing and calls in flight.
    multiplexers: Multiplexers,
    /// Row styling only, never `Checkout::gone`, which the delete flow
    /// reads to choose between removing a worktree and pruning it.
    liveness: worktree_liveness::LivenessCache,
    /// The probe job in flight, if any.  One at a time: a path slower than
    /// the interval stretches freshness rather than queueing more work.
    liveness_probe: Option<jobs::Job<Vec<(PathBuf, alacritree_vcs::Probe)>>>,
    /// When the user last gave the app an event.  Timed wake-ups are armed
    /// only just after one, so an app left open overnight goes fully quiet.
    last_input: Instant,
    /// When input the user aimed at this window last arrived.  Distinct from
    /// `last_input`, which also advances on bare pointer motion, and which
    /// the liveness probe reads as its grace period.
    last_direct_input: Option<Instant>,
    /// Whether typing has hidden the mouse pointer, under `[mouse]
    /// hide_when_typing`.
    mouse_hide: mouse_hide::MouseHide,
}

/// The backend that answers for a checkout: its project's, else the first
/// enabled one, so a folder that is no repository still gets that backend's
/// own error text. Free of `self` so a caller can hold it while it borrows
/// another field mutably.
fn owning_vcs<'a>(
    projects: &'a [Project],
    backends: &'a [crate::vcs::Vcs],
    path: &Path,
) -> Option<&'a crate::vcs::Vcs> {
    projects
        .iter()
        .find(|p| p.checkouts.iter().any(|c| c.path == path))
        .and_then(|p| p.vcs.as_ref())
        .or_else(|| backends.first())
}

impl AlacritreeApp {
    fn from_parts(
        config: Config,
        theme: Theme,
        persisted: state::PersistedState,
        projects: Vec<Project>,
        fonts: (Vec<crate::fonts::ChainFace>, crate::fonts::FaceMetrics),
        notify_rx: Receiver<SessionId>,
        ipc: (Option<ipc::server::SocketHandle>, Option<Receiver<ipc::server::AppCall>>),
    ) -> Self {
        let (font_chain, face_metrics) = fonts;
        let color_glyph_budget_mb = config.font.color_glyph_cache_mb;
        let grid_snapshot = crate::terminal_view::GridSnapshot::new(&config.palette);
        let (ipc_socket, ipc_rx) = ipc;
        let multiplexers = Multiplexers::new(&config.integrations);
        let row_labels = crate::row_label::LabelTemplates::new(
            config.ui.worktree_name.clone(),
            config.ui.project_name.clone(),
        );

        Self {
            show_left_sidebar: persisted.show_left_sidebar,
            show_right_sidebar: persisted.show_right_sidebar,
            focus: PaneFocus::Terminal,
            session_rows_always: config.ui.session_display.sidebar_always,
            session_tabs_always: config.ui.session_display.tabs_always,
            session_drag: config.ui.session_reorder.drag,
            sessions_filter_counts_detached: config.ui.sessions_filter_counts_detached,
            sidebar: sidebar::Sidebar::new(PanelFilter::new(project_filter_toggles(
                config.integrations.gh.pr_status,
            ))),
            sidebar_auto_shown: false,
            last_followed: (None, None),
            git_panel: git_panel::GitPanel::new(
                persisted
                    .base_branches
                    .iter()
                    .map(|b| (b.worktree.clone(), b.branch.clone()))
                    .collect(),
            ),
            sidebar_focus_state: focus::SidebarFocusState::new(config.ui.search_scope),
            search_depth: config.ui.search_depth,
            palette: CommandPalette::new(),
            sessions: SessionList::default(),
            current_workspace: None,
            projects,
            pr_cache: PrCache::new(Forge::default()),
            vcs_backends: crate::vcs::backends(&config.integrations),
            row_labels,
            icons: PaintedIcons::new(&config, &multiplexers),
            shortcuts: crate::shortcut::Shortcuts::new(&config.bindings),
            config,
            theme,
            modals: modals::Modals::default(),
            hooks_opened: HashSet::new(),
            detached_jobs: Vec::new(),
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
            grid_snapshot,
            gpu_grid: crate::grid_gl::GpuGrid::new(),
            frame_log: crate::frame_log::FrameLog::start(),
            phases: crate::frame_log::Phases::new(),
            grid_paint: std::time::Duration::ZERO,
            last_pane_geometry: None,
            project_refreshes: Default::default(),
            pending_spawns: Default::default(),
            multiplexers,
            liveness: Default::default(),
            liveness_probe: None,
            last_input: Instant::now(),
            last_direct_input: None,
            mouse_hide: Default::default(),
        }
    }

    fn configure_context(
        ctx: &Context,
        config: &Config,
        theme: &Theme,
    ) -> (Vec<crate::fonts::ChainFace>, crate::fonts::FaceMetrics) {
        // A job's own closure cannot wake the loop when it unwinds, and the
        // failure it reports is only ever read from a frame.
        let waker_ctx = ctx.clone();
        jobs::pool().set_waker(move || waker_ctx.request_repaint());

        let (font_chain, face_metrics) =
            crate::fonts::install_terminal_fonts(ctx, &config.font, &config.ui_font);

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = theme.terminal_bg;
        visuals.window_fill = theme.terminal_bg;
        visuals.extreme_bg_color = theme.terminal_bg;
        ctx.set_visuals(visuals);

        // Anchor every text style to the terminal font: titles (unmodified
        // labels) use `Body`/`Heading` at 100% of the grid's text size, and
        // every other UI label (`.small()`, buttons) drops to 80% via
        // `font_normal`.  Spacing knobs scale with the normal-text size so
        // paddings/widths track changes to `font.size`.
        let mut style = (*ctx.style()).clone();
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
        // warning is noise rather than signal, so silence it everywhere.
        // `Style::debug` itself is `#[cfg(debug_assertions)]` in egui, so the
        // assignment has to be cfg-gated to keep `--release` compiling.
        #[cfg(debug_assertions)]
        {
            style.debug.show_unaligned = false;
        }
        ctx.set_style(style);

        // Terminal IME hint, matching alacritty's set_ime_purpose.
        ctx.send_viewport_cmd(egui::ViewportCommand::IMEPurpose(
            egui::viewport::IMEPurpose::Terminal,
        ));

        alacritty_terminal::tty::setup_env();

        (font_chain, face_metrics)
    }

    fn load_projects(config: &Config) -> (state::PersistedState, Vec<Project>) {
        let persisted = state::load();
        let projects: Vec<Project> = persisted
            .projects
            .iter()
            .map(|p| {
                // WSL roots discover in the background after construction,
                // since a cold distro takes seconds to boot and would block first
                // paint. Normalize the root first so a persisted `\\wsl$\`
                // spelling converges with the `\\wsl.localhost\` paths that
                // background discovery later swaps in via `poll_project_refreshes`.
                let root = wsl::normalize_root(p.root.clone());
                let mut project = match wsl::classify(&root) {
                    wsl::Location::Windows(_) => jobs::on_this_thread(|blocking| {
                        let backends = crate::vcs::backends(&config.integrations);
                        Project::discover(root, &backends, config.ui.upstream_status, blocking)
                            .project
                    }),
                    wsl::Location::Wsl { .. } => Project::placeholder(root),
                };
                project.expanded = p.expanded;
                project.shell_override = p.shell.as_deref().and_then(wsl::ShellChoice::parse);
                project.label = p.label.clone();
                project
            })
            .collect();

        (persisted, projects)
    }

    pub fn new(cc: &CreationContext<'_>, config: Config) -> Self {
        let theme = Theme::from_config(&config);
        let fonts = Self::configure_context(&cc.egui_ctx, &config, &theme);
        let (ipc_socket, ipc_rx) = Self::start_ipc(&cc.egui_ctx, &config);
        let (persisted, projects) = Self::load_projects(&config);

        // Delegate installation and the permission prompt belong to startup:
        // deferring them to the first toast would drop that toast (macOS
        // won't deliver while the authorization sheet is pending).
        #[cfg(target_os = "macos")]
        if config.ui.notifications {
            notify::macos::init(cc.egui_ctx.clone());
        }

        let notify_rx = notify::channel();

        let pr_status_concurrency = config.integrations.gh.pr_status_concurrency;
        let mut app = Self::from_parts(
            config,
            theme,
            persisted,
            projects,
            fonts,
            notify_rx,
            (ipc_socket, ipc_rx),
        );

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
            app.modals.error_dialog = Some(format!("failed to spawn shell: {e}"));
        }

        app
    }

    fn persist_sidebars(&self) {
        // Don't persist a sidebar the user never opened. An auto-shown
        // sidebar (e.g. from Ctrl+Shift+B while it was hidden) should not
        // reappear on next launch.
        let left = self.show_left_sidebar && !self.sidebar_auto_shown;
        let right = self.show_right_sidebar && !self.git_panel.auto_shown;
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
    fn rename_project(&mut self, root: &Path, label: Option<String>) -> Result<usize, NotAProject> {
        let idx = self
            .projects
            .iter()
            .position(|p| p.root == *root)
            .ok_or_else(|| NotAProject(root.to_path_buf()))?;
        self.projects[idx].label = crate::projects::normalize_label(label);
        self.persist_project_label(root);
        Ok(idx)
    }

    /// Re-discovery always runs on a worker thread: wsl.exe takes ~400 ms warm
    /// and seconds while the distro VM boots, and native discovery costs tens
    /// of milliseconds on a project with many worktrees.
    fn refresh_project(&mut self, ctx: &Context, idx: usize) {
        let root = self.projects[idx].root.clone();
        let ctx = ctx.clone();
        let worker_root = root.clone();
        let upstream = self.config.ui.upstream_status;
        let backends = self.vcs_backends.clone();
        self.project_refreshes.start(root, || {
            jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
                let found = Project::discover(worker_root, &backends, upstream, blocking);
                ctx.request_repaint();
                found
            })
        });
    }

    /// Keep the worktree rows the sidebar just drew honest about whether their
    /// checkout is still there. Discovery only re-runs when something asks it
    /// to, so a `git worktree remove` typed into one of our own sessions would
    /// otherwise leave the row looking live until the user pressed refresh.
    ///
    /// `request_repaint_after` is what carries the tick across a terminal that
    /// has gone quiet. Since egui paints on demand, the probe would never run
    /// a second time without it. It is armed only for a short window after the
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
                self.refresh_moved_branches(ctx, &results);
                self.liveness
                    .adopt(results.into_iter().map(|(path, probe)| (path, probe.liveness)), now);
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
            let batch: Vec<(PathBuf, crate::vcs::Vcs)> = self
                .liveness
                .batch(drawn)
                .into_iter()
                .filter_map(|p| self.vcs_for(&p).map(|vcs| (p, vcs)))
                .collect();
            if batch.is_empty() {
                // No job will land to close the interval, so close it here.
                self.liveness.adopt(Vec::new(), now);
            } else {
                let ctx = ctx.clone();
                let job = jobs::pool().spawn(jobs::Priority::Background, move |_blocking| {
                    let results: Vec<_> = batch
                        .into_iter()
                        .map(|(p, vcs)| {
                            let probe = vcs.probe(&p);
                            (p, probe)
                        })
                        .collect();
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

    /// Re-run worktree discovery for every project, the keyboard/IPC
    /// equivalent of pressing each row's refresh button in turn.
    fn refresh_all_projects(&mut self, ctx: &Context) {
        for idx in 0..self.projects.len() {
            self.refresh_project(ctx, idx);
        }
    }

    /// Re-discover every project holding a checkout whose `HEAD` has left the
    /// branch discovery recorded.  That branch keys the PR badge and the row
    /// label, and discovery otherwise runs only when something asks for it.
    fn refresh_moved_branches(
        &mut self,
        ctx: &Context,
        results: &[(PathBuf, alacritree_vcs::Probe)],
    ) {
        let mut moved: Vec<&Path> = Vec::new();
        for (path, probe) in results {
            let Some(head) = probe.head.as_deref() else { continue };
            let known = self
                .projects
                .iter()
                .flat_map(|p| &p.checkouts)
                .find(|wt| wt.path == *path)
                .map(|wt| wt.head.label());
            if let Some(known) = known
                && self.liveness.branch_moved(path, head, known)
            {
                moved.push(path);
            }
        }
        if moved.is_empty() {
            return;
        }
        for idx in 0..self.projects.len() {
            if self.projects[idx].checkouts.iter().any(|wt| moved.contains(&wt.path.as_path())) {
                self.refresh_project(ctx, idx);
            }
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
        for Finished { key: root, outcome, waiters } in self.project_refreshes.take_finished() {
            let Some(found) = outcome else {
                waiters.answer(Err("the project refresh worker panicked".to_string()));
                continue;
            };
            let reply = match self.projects.iter_mut().find(|p| p.root == root) {
                Some(project) => {
                    let occupied: HashSet<PathBuf> =
                        self.sessions.iter().filter_map(|s| s.working_directory.clone()).collect();
                    project.apply(found, &occupied);
                    Ok(project_json(project))
                },
                None => Err(NotAProject(root).to_string()),
            };
            waiters.answer(reply);
        }
    }

    /// Push a session record and get its PTY opened: inline when the gate is
    /// off, on the job pool when it is on.  The record exists before this
    /// returns either way, so a caller can activate the tab without waiting
    /// for a shell.  Callers decide whether it becomes the active session.
    fn open_session(
        &mut self,
        session: AppSession,
        request: session::OpenRequest<Context>,
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
                    self.sessions.remove(&[id], self.config.ui.sidebar_focus);
                    return Err(e);
                },
            }
        }

        // Interactive: an empty pane is on screen until this lands. The pool
        // repaints once the job returns, so nothing here has to. Without that
        // the result would wait for whatever wakes the loop next, which under
        // load is the shell's own first output seconds later.
        let job = jobs::pool()
            .spawn(jobs::Priority::Interactive, move |_blocking| session::open(request));
        self.pending_spawns.start(id, || job);
        Ok(id)
    }

    /// Adopt every PTY that finished opening.  A session whose record is gone
    /// was closed while it was opening: dropping the attachment shuts its
    /// shell down rather than resurrecting the tab.
    fn poll_pending_spawns(&mut self, ctx: &Context) {
        for Finished { key: id, outcome, waiters } in self.pending_spawns.take_finished() {
            let opened = outcome
                .unwrap_or_else(|| Err(std::io::Error::other("the session's PTY worker panicked")));
            match opened {
                Ok(attachment) => match self.sessions.iter().position(|s| s.id == id) {
                    Some(idx) => {
                        let started = Instant::now();
                        self.sessions[idx].attach(attachment);
                        crate::frame_log::spawn_phase(Some(id), "attach", started.elapsed());
                        waiters.answer(Ok(json!({ "session_id": id })));
                    },
                    None => {
                        drop(attachment);
                        waiters.answer(Err(
                            "the session was closed while its shell was starting".into()
                        ));
                    },
                },
                Err(e) => {
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
                    waiters.answer(Err(format!("failed to spawn shell: {e}")));
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
    /// the checkout hooks live here rather than in `spawn_session`: a named
    /// profile arrives with its shell already chosen and would otherwise open
    /// in a checkout Ctrl+T refuses.
    fn spawn_session_with_shell(
        &mut self,
        ctx: &Context,
        working_directory: WorkspaceKey,
        shell: Option<ShellCommand>,
        wsl_probe: Option<WslProbe>,
    ) -> std::io::Result<SessionId> {
        if let Some(dir) = &working_directory {
            // A checkout git has forgotten is refused here rather than in
            // `session::open`, which can only see whether the directory
            // exists. A half-finished `git worktree remove` leaves one that
            // does. Refusing here is what keeps the greyed row's promise.
            if self.worktree_gone(dir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("worktree is no longer checked out: {}", dir.display()),
                ));
            }
            // Synchronous, so a second rapid spawn sees the once-per-worktree
            // guard set. The hooks run off-thread, so the first shell may
            // start before they land.
            self.sync_checkout_hooks(dir.clone());
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
        self.sessions.set_active(working_directory, id);
        Ok(id)
    }

    fn toggle_scratchpad_tab(&mut self, ctx: &Context) {
        let workspace = self.current_workspace.clone();
        if let Some(index) = self.scratchpad_session_index(&workspace) {
            let id = self.sessions[index].id;
            if self.sessions.active(&workspace) == Some(id) {
                // Scratchpad edits are persisted as they happen, so toggling
                // the active tab closed never needs the session-close prompt.
                self.close_session(ctx, id);
                return;
            }
            self.sessions.set_active(workspace, id);
        } else if let Err(e) = self.spawn_scratchpad(ctx, workspace) {
            self.modals.error_dialog = Some(format!("failed to open scratchpad: {e}"));
            return;
        }
        self.focus_terminal();
    }

    fn toggle_tasks_tab(&mut self, ctx: &Context) {
        let workspace = self.current_workspace.clone();
        if let Some(index) = self.tasks_session_index(&workspace) {
            let id = self.sessions[index].id;
            if self.sessions.active(&workspace) == Some(id) {
                // The backend holds every task, so there is nothing to lose.
                self.close_session(ctx, id);
                return;
            }
            self.sessions.set_active(workspace, id);
        } else {
            let (project, worktree) = self.project_and_worktree(&workspace);
            let scope = crate::tasks::view::Scope::for_workspace(project, worktree);
            let session = Session::spawn_tasks(
                ctx.clone(),
                &self.config,
                workspace.clone(),
                TermSize::new(80, 24),
                (8.0, 16.0),
                crate::tasks::view::TasksView::new(
                    crate::tasks::backend::Backend::from_config(&self.config.integrations),
                    scope,
                    worktree.map(|w| w.path.clone()),
                    self.vcs_backends.clone(),
                ),
            );
            let id = session.id;
            self.sessions.push(session);
            self.sessions.set_active(workspace, id);
        }
        self.focus_terminal();
    }

    /// Home has neither. A folder with no version control has a project but
    /// no worktree, since its placeholder has no branch to key a workspace on.
    fn project_and_worktree(&self, ws: &WorkspaceKey) -> (Option<&Project>, Option<&Checkout>) {
        let Some(path) = ws else { return (None, None) };
        self.projects
            .iter()
            .find_map(|p| {
                let wt = p.checkouts.iter().find(|wt| wt.path == *path)?;
                Some((Some(p), p.vcs.is_some().then_some(wt)))
            })
            .unwrap_or((None, None))
    }

    /// [`owning_vcs`], cloned.
    fn vcs_for(&self, path: &Path) -> Option<crate::vcs::Vcs> {
        owning_vcs(&self.projects, &self.vcs_backends, path).cloned()
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
        self.sessions.set_active(workspace, id);
        Ok(id)
    }

    /// Run every checkout hook's `on_opened` the first time this process
    /// opens a shell in a linked worktree. The create path covers worktrees
    /// alacritree makes, and this covers ones created outside it.
    fn sync_checkout_hooks(&mut self, worktree: PathBuf) {
        self.open_checkout_hooks(worktree, crate::checkout_hooks::from_config);
    }

    fn open_checkout_hooks<H: CheckoutHook + Send + 'static>(
        &mut self,
        worktree: PathBuf,
        hooks: impl FnOnce(&crate::config::IntegrationsConfig) -> Vec<H>,
    ) {
        if !self.hooks_opened.insert(worktree.clone()) {
            return;
        }
        let main_checkout = self.projects.iter().find_map(|p| {
            let owns = p.checkouts.iter().any(|wt| !wt.is_main && wt.path == worktree);
            if !owns {
                return None;
            }
            p.checkouts.iter().find(|wt| wt.is_main).map(|wt| wt.path.clone())
        });
        let Some(main_checkout) = main_checkout else {
            return;
        };
        let hooks = hooks(&self.config.integrations);
        self.detached_jobs.push(jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            let event = CheckoutEvent { main: &main_checkout, checkout: &worktree };
            crate::checkout_hooks::report(hooks.opened(&event, blocking), |level, line| {
                log::log!(level, "{line} ({})", worktree.display())
            });
        }));
    }

    /// Spawn a named profile into the current workspace, bypassing the
    /// override/auto resolution chain, because the user asked for this profile
    /// explicitly. Raises `error_dialog` directly: this wrapper's three
    /// callers (the `SpawnProfileN` keybinding, the tab strip `+`, and the
    /// palette) have no stale-row state to reconcile, unlike the sidebar's
    /// `spawn_profile_session_in` caller.
    fn spawn_profile_session(&mut self, ctx: &Context, name: &str) {
        let ws = self.current_workspace.clone();
        if let Err(e) = self.spawn_profile_session_in(ctx, name, ws) {
            self.modals.error_dialog = Some(format!("failed to spawn profile `{name}`: {e}"));
        }
    }

    /// Spawn a named profile into an arbitrary workspace. The worktree
    /// sidebar's profile menu targets the row it was opened on, which is
    /// often not the workspace currently on screen. Returns the error
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

    /// Shell for a workspace; `None` means "no override", and
    /// `Session::pending_shell` falls through to alacritty's config-driven
    /// shell with its OS-guaranteed fallback. The home tab (`None`
    /// workspace) has no project or location, so only the default profile can
    /// apply there.
    fn resolve_shell(&self, workspace: &WorkspaceKey) -> (Option<ShellCommand>, Option<WslProbe>) {
        let path = workspace.as_deref();
        let choice = path.and_then(|p| {
            self.projects
                .iter()
                .find(|proj| proj.checkouts.iter().any(|wt| wt.path.as_path() == p))
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
        // with a failed spawn. Stay put and let the sidebar re-mark the row.
        // Shells already running there are the exception: they outlive the
        // directory, and this row is the only way back to them.
        if self.worktree_gone(path) && !self.workspace_has_sessions_only(&Some(path.to_path_buf()))
        {
            self.modals.error_dialog =
                Some("worktree directory is missing. Prune it from the sidebar.".to_string());
            if let Some(idx) =
                self.projects.iter().position(|p| p.checkouts.iter().any(|w| w.path == path))
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
            self.sessions.set_active(self.current_workspace.clone(), id);
            // Filling in a missing active entry is self-healing, not navigation.
            self.mark_sidebar_focus_write();
        }
    }

    fn close_session(&mut self, ctx: &Context, id: SessionId) {
        self.close_session_with(ctx, id, CloseReason::User);
    }

    fn close_session_with(&mut self, ctx: &Context, id: SessionId, reason: CloseReason) {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        let workspace = session.working_directory.clone();
        self.close_sessions(ctx, &[id], workspace, reason);
    }

    /// Remove `ids`, all of them from `workspace`, and settle where the view
    /// goes.  Every close of a session that got a record comes through here,
    /// so each gets the same cleanup and lands by the same rules.
    fn close_sessions(
        &mut self,
        ctx: &Context,
        ids: &[SessionId],
        workspace: WorkspaceKey,
        reason: CloseReason,
    ) {
        let policy = self.config.ui.last_session_close;
        let ring = policy.rings().then(|| self.session_ring()).unwrap_or_default();
        for session in self.sessions.remove(ids, self.config.ui.sidebar_focus) {
            self.multiplexers.session_closed(session.id, session.pane_key.as_ref());
            if self.modals.pending_session_close == Some(session.id) {
                self.modals.pending_session_close = None;
            }
        }

        let remaining: Vec<(WorkspaceKey, SessionId)> =
            self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect();

        // Closing the on-screen workspace's last session must not strand the
        // view on an empty pane. What happens instead is policy: `respawn`
        // recycles a shell in place (the last session is by design
        // unclosable), `navigate` falls back to the project main, then home,
        // and the ring policies land on the nearest surviving session in the
        // flat session ring instead.
        let deleted = reason == CloseReason::WorktreeDeleted;
        let main = workspace
            .as_deref()
            .filter(|_| !deleted)
            .and_then(|p| project_main_for(&self.projects, p));
        let mut verdict = close_navigation(
            reason,
            close_fallback(&workspace, &self.current_workspace, &remaining, main),
        );
        if verdict != CloseFallback::Stay && policy.rings() {
            let prefer = policy
                .prefers_project()
                .then(|| sidebar_nav::project_of(&self.projects, &workspace))
                .flatten();
            if let Some((_, landing)) = ring_landing(&ring, ids, prefer) {
                verdict = CloseFallback::ActivateSession(landing);
            }
        }
        if verdict != CloseFallback::Stay && policy == LastSessionClose::Respawn && !deleted {
            if let Err(e) = self.spawn_session(ctx, workspace.clone()) {
                self.report_spawn_failure(ctx, &workspace, &e);
            }
            return;
        }
        if defers_close_navigation(self.config.ui.sidebar_focus) && verdict != CloseFallback::Stay {
            let removed_worktree = if deleted { workspace } else { None };
            self.sidebar_focus_state.deferred_close =
                Some(DeferredClose { verdict, removed_worktree });
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
        if close_needs_prompt(&self.config.ui, session.pane_key.is_some(), session.is_busy()) {
            self.modals.pending_session_close = Some(id);
        } else {
            self.close_session(ctx, id);
        }
    }

    /// Open the delete/prune confirm dialog for the worktree at `path`.
    /// Main checkouts have no delete affordance, and a worktree whose
    /// removal is already running is inert.
    fn request_worktree_delete(&mut self, path: &Path) {
        if self.modals.pending_deletes.iter().any(|t| t.worktree_path == *path) {
            return;
        }
        let Some((project_idx, wt)) =
            self.projects.iter().enumerate().find_map(|(idx, p)| {
                p.checkouts.iter().find(|w| w.path == *path).map(|w| (idx, w))
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
        let prunable = wt.gone || self.is_gone(&wt.path);
        // A missing dir has nothing to be dirty; skip the status probe. A
        // worktree the git panel has already completed a compute for answers
        // from that cache instead of walking the tree again. A cache entry
        // with no compute yet (the panel's first frame for this workspace)
        // is `Status::default()`, indistinguishable from "known clean",
        // so it is not read as an answer. A cold one waits on a job so the
        // dialog opens at once and fills in.
        //
        // A resolved dirty count preloads `force` so a known-dirty tree goes
        // straight to a forced removal. The dialog does not confirm until a
        // count is known, so no removal runs against an unknown tree.
        let (dirty, dirty_job, force) = if prunable {
            (Some(Dirty::default()), None, false)
        } else if let Some(counts) = self
            .git_panel
            .status
            .get(&wt.path)
            .filter(|cache| cache.has_status())
            .map(|cache| crate::status_cache::dirty_of(cache.last()))
        {
            let force = counts.is_dirty();
            (Some(counts), None, force)
        } else {
            let path = wt.path.clone();
            let job = jobs::pool().spawn(jobs::Priority::Interactive, {
                let vcs = self.vcs_for(&path);
                move |blocking| {
                    vcs.map_or_else(Dirty::default, |vcs| {
                        vcs.dirty(&path, blocking).unwrap_or_default()
                    })
                }
            });
            (None, Some(job), false)
        };
        self.modals.pending_delete = Some(DeleteRequest {
            project_idx,
            worktree_path: wt.path.clone(),
            worktree_name: wt.name.clone(),
            branch: wt.head.name.clone(),
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
    ) -> Result<WorkspaceKey, MoveError> {
        let idx = self.sessions.iter().position(|s| s.id == id).ok_or(MoveError::NoSession(id))?;
        match &self.sessions[idx].kind {
            SessionKind::Scratchpad { .. } => return Err(MoveError::Scratchpad),
            SessionKind::Tasks => return Err(MoveError::Tasks),
            // A workspace's diff pane is found by workspace plus kind, so a
            // pane carried elsewhere becomes the one the next git click closes
            // while the workspace it left opens a second.
            SessionKind::Diff { .. } => return Err(MoveError::Diff),
            _ => {},
        }
        if self.sessions.move_to(idx, &target, &self.current_workspace) {
            self.current_workspace = target.clone();
        }
        Ok(target)
    }

    /// Whether the sidebar worktree at `path` is one git no longer recognises,
    /// which is the single question the row's styling, this guard and the
    /// delete flow all ask. A greyed row that still spawns a shell would be
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
            .flat_map(|p| &p.checkouts)
            .filter(|wt| wt.path == path)
            .any(|wt| !wt.is_main);
        if linked { self.is_gone(path) } else { !path.is_dir() }
    }

    /// Whether the owning backend calls this checkout gone.  The row, the
    /// activate guard and the spawn guard all ask this, so a greyed row and a
    /// refused shell never disagree about the same directory.  A probe that
    /// could not tell answers `false`: an unreachable filesystem must not
    /// turn into a refusal.
    fn is_gone(&self, path: &Path) -> bool {
        self.vcs_for(path).is_some_and(|vcs| vcs.probe(path).liveness == Liveness::Missing)
    }

    /// Report a failed spawn, and re-run discovery when the cause was a
    /// vanished checkout: git may have forgotten the worktree entirely, in
    /// which case the row should go rather than keep offering a shell that
    /// cannot start.
    fn report_spawn_failure(&mut self, ctx: &Context, ws: &WorkspaceKey, e: &std::io::Error) {
        self.modals.error_dialog = Some(format!("failed to spawn shell: {e}"));
        let Some(path) = ws.as_deref().filter(|p| self.worktree_gone(p)) else {
            return;
        };
        if let Some(idx) =
            self.projects.iter().position(|p| p.checkouts.iter().any(|w| w.path == path))
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
            .filter(|(_, s)| s.working_directory == *ws && s.pane_key.is_none())
            .map(|(i, _)| i)
            .collect()
    }

    /// `ws`'s sessions in the order the sidebar and the tab strip draw them:
    /// alacritree's own first, then the harness-backed ones in the harness's
    /// order.  Every ring the user steps through is built from here, so a
    /// press walks the list on screen rather than the order the sessions
    /// happened to open in. The two part company as soon as a harness pane
    /// is attached out of the harness's own order.
    fn workspace_display_indices(&self, ws: &WorkspaceKey) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.working_directory == *ws)
            .map(|(i, _)| i)
            .collect();
        indices.sort_by_key(|i| match self.sessions[*i].pane_key.clone() {
            Some(key) => (1, self.multiplexers.pane_index(&key).unwrap_or(usize::MAX)),
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
                Some(path) => !self.modals.pending_deletes.iter().any(|t| t.worktree_path == *path),
            })
            .collect()
    }

    /// The workspace a session sits in, and the workspaces a reorder may carry
    /// it through.  A scratchpad or diff pane belongs to its workspace, so its
    /// range is that workspace alone whatever the scope says. The keyboard and
    /// the mouse both read the rule from here so neither can offer a landing
    /// the move would refuse.
    fn reorder_range(&self, id: SessionId) -> Option<(WorkspaceKey, Vec<WorkspaceKey>)> {
        let idx = self.sessions.iter().position(|s| s.id == id)?;
        let origin = self.sessions[idx].working_directory.clone();
        if matches!(
            &self.sessions[idx].kind,
            SessionKind::Scratchpad { .. } | SessionKind::Diff { .. } | SessionKind::Tasks
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
        if self.sessions[abs].pane_key.is_some() {
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

    /// One `MoveSessionUp` / `MoveSessionDown` press. Every refusal is a
    /// silent no-op: a clamped end, a boundary the scope forbids, a scratchpad
    /// asked to leave its workspace. None of those is a failure. Each is a
    /// move with nowhere to go.
    fn step_session(&mut self, delta: i32) {
        let sidebar_focused = self.focus == PaneFocus::ProjectsSidebar;
        let Some(id) = reorder_subject(
            sidebar_focused,
            self.sidebar.model.cursor(),
            || self.sessions.active(&None),
            |path| self.sessions.active(&Some(path.to_path_buf())),
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
        // nowhere to start. That is the same silent no-op as a clamped end.
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
    /// neither `SidebarModel::set_cursor` nor the focus reconciler would notice
    /// the row moved and scroll after it, so this pins the cursor instead. A
    /// landing inside a collapsed project expands it, because a cursor with no
    /// painted row is the state the reconciler treats as a row that went away.
    fn follow_moved_session(&mut self, id: SessionId, landed_in: &WorkspaceKey) {
        self.sidebar.model.pin_cursor(SidebarRow::Session(id));
        let Some(path) = landed_in.as_deref() else { return };
        let root = self
            .projects
            .iter()
            .find(|p| p.checkouts.iter().any(|w| w.path == path))
            .map(|p| p.root.clone());
        if let Some(root) = root {
            self.set_project_expanded(&root, true);
        }
    }

    fn tasks_session_index(&self, ws: &WorkspaceKey) -> Option<usize> {
        self.sessions.iter().position(|session| {
            session.working_directory == *ws && matches!(&session.kind, SessionKind::Tasks)
        })
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
        let id = self.sessions.active(&self.current_workspace)?;
        self.sessions.iter().position(|s| s.id == id)
    }

    fn set_active_in_current_workspace(&mut self, id: SessionId) {
        self.sessions.set_active(self.current_workspace.clone(), id);
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
        self.sessions.set_active(target_ws.clone(), id);
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
            for wt in &project.checkouts {
                let has_sessions = self.workspace_has_sessions_only(&Some(wt.path.clone()));
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

    /// Drop a project from the sidebar. Nothing on disk is touched, and
    /// sessions already open in its worktrees keep running. They outlive the
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
        self.modals.quit_dialog_open
            || self.modals.pending_delete.is_some()
            || self.modals.pending_create.is_some()
            || self.modals.pending_session_close.is_some()
            || self.modals.pending_detach_all.is_some()
            || self.modals.pending_rename.is_some()
            || self.modals.pending_base_branch.is_some()
            || self.modals.pending_project_remove.is_some()
            || self.modals.error_dialog.is_some()
    }

    fn focus_sidebar(&mut self) {
        if !self.show_left_sidebar {
            self.show_left_sidebar = true;
            self.sidebar_auto_shown = true;
            self.persist_sidebars();
        }
        self.focus = PaneFocus::ProjectsSidebar;
        let seed = sidebar_nav::seed(
            &self.projects,
            self.current_workspace.as_deref(),
            &self.listed_workspace_rows(),
            self.sessions.active(&self.current_workspace),
        );
        // Seeding reads the unfiltered tree, so a lingering filter from a prior
        // focus round-trip can leave the seeded row outside the current rows;
        // repair it immediately rather than waiting for the first key press.
        self.fresh_sidebar_model().seat_cursor(Some(seed));
        // Seeding rewrites the cursor from terminal state, which the overtaken
        // check would otherwise read as the user navigating.  The anchor
        // outlives a trip through the terminal by design.
        self.mark_sidebar_focus_write();
    }

    fn focus_git_sidebar(&mut self) {
        if !self.show_right_sidebar {
            self.show_right_sidebar = true;
            self.git_panel.auto_shown = true;
            self.persist_sidebars();
        }
        self.focus = PaneFocus::GitSidebar;
        // Rows come from the render pass, so seeding waits for it. Leave the
        // cursor as-is and let the render pass repair it.
        self.git_panel.cursor_moved = true;
    }

    fn focus_terminal(&mut self) {
        self.focus = PaneFocus::Terminal;
        if self.sidebar_auto_shown {
            self.show_left_sidebar = false;
            self.sidebar_auto_shown = false;
            self.persist_sidebars();
        }
        if self.git_panel.auto_shown {
            self.show_right_sidebar = false;
            self.git_panel.auto_shown = false;
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
        let active = self.active_session_index().map(|idx| SessionFocus {
            scratchpad: self.sessions[idx].scratchpad.is_some(),
            exited: self.sessions[idx].is_exited(),
        });
        let scope = binding_scope(self.focus, self.palette.is_open(), active);
        let actions: Vec<BindingAction> = ctx.input_mut(|i| {
            let mut actions = Vec::new();
            i.events.retain(|ev| {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                    let matched =
                        dispatched_actions(self.shortcuts.matches(*key, *modifiers), scope);
                    if !matched.is_empty() {
                        let suppress_chars = matched.iter().all(|a| {
                            !matches!(a, BindingAction::Named(NamedAction::ReceiveChar(_)))
                        });
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
        let filter = &mut self.sidebar.filter;
        let shortcuts = &self.shortcuts;
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
                        shortcuts,
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
            Outcome::MoveCursor(delta) => self.fresh_sidebar_model().move_cursor(Step::Line(delta)),
            Outcome::LeavePanel => self.focus_terminal(),
        }
    }

    fn workspace_has_sessions_only(&self, key: &WorkspaceKey) -> bool {
        self.sessions.iter().any(|s| s.working_directory == *key)
    }

    /// Every live session as a `(workspace, id)` pair. That is the same shape
    /// `close_fallback` takes, and the model the
    /// focus reconciler observes.
    fn session_pairs(&self) -> Vec<(WorkspaceKey, SessionId)> {
        self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect()
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
            SidebarRow::Pane(key) => {
                let key = key.clone();
                let pane_id = self.find_pane(&key).map(|pane| pane.pane_id.clone());
                let workspace = self.pane_row_workspace(&key);
                if let (Some(pane_id), Some(workspace)) = (pane_id, workspace) {
                    let unlisted = PaneTarget::unlisted(&key, &pane_id);
                    // Switches first, same as the click path: a refusal is
                    // only visible if the workspace it happened in is on
                    // screen.
                    let switch = self.switch_for_attach(&workspace, AttachFocus::Take);
                    if self.attach_pane(ctx, key, unlisted, &switch, None, AttachFocus::Take) {
                        self.focus_terminal();
                    } else {
                        self.current_workspace = switch.from;
                    }
                }
            },
        }
    }

    /// Switch to the session's workspace and mark it active, the keyboard
    /// equivalent of clicking its sidebar row. A stale id (session reaped
    /// this frame) self-heals next frame via `ensure_active_session`.
    fn activate_session_by_id(&mut self, id: SessionId) {
        let Some(ws) =
            self.sessions.iter().find(|s| s.id == id).map(|s| s.working_directory.clone())
        else {
            return;
        };
        self.current_workspace = ws.clone();
        self.sessions.set_active(ws, id);
    }

    fn set_project_expanded(&mut self, root: &Path, expanded: bool) {
        if let Some(p) = self.projects.iter_mut().find(|p| p.root == *root) {
            if p.expanded != expanded {
                p.expanded = expanded;
                self.persist_project(root);
            }
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
        // Every path was filtered out. A paste of nothing still clears the
        // selection and snaps the view to the bottom, or drops the scratchpad's
        // selection. Those are side effects with nothing to show for them.
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

    /// Scroll the terminal on screen. `scroll` receives the page height in
    /// lines, for the half-page steps; a scratchpad has no grid to scroll.
    fn scroll_display(&mut self, scroll: impl FnOnce(i32) -> alacritty_terminal::grid::Scroll) {
        use alacritty_terminal::grid::Dimensions;
        let Some(idx) = self.active_session_index() else {
            return;
        };
        let session = &mut self.sessions[idx];
        if session.scratchpad.is_some() {
            return;
        }
        let mut term = session.term.lock();
        let lines_per_page = term.grid().screen_lines() as i32;
        term.scroll_display(scroll(lines_per_page));
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
            // 2px is too small to reliably click, so expand the hit zone vertically.
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
                .find(|p| p.checkouts.iter().any(|w| w.path == path))
                .and_then(|p| p.home.clone()),
            wsl::Location::Windows(_) => home::home_dir().map(|h| h.display().to_string()),
        }
    }
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

/// Take Escape and Enter for the modal now painting.
///
/// The keys leave the queue whether or not the modal may act on them: they
/// were aimed at a screen that is no longer in front of the user, and letting
/// one fall through would type it into a shell they can no longer see.  The
/// gate decides only whether the modal answers them.
fn consume_modal_keys(ctx: &Context, gate: &ModalGate, modal: ModalKind) -> (bool, bool) {
    let accepts = gate.accepts(modal);
    ctx.input_mut(|i| {
        let escape = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        let enter = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
        (accepts && escape, accepts && enter)
    })
}

/// Move focus to `id` if no widget currently has it. This gives the modal's
/// primary control focus on open without stealing it from the user later.
fn focus_default(ctx: &Context, id: egui::Id) {
    let has_focus = ctx.memory(|m| m.focused().is_some());
    if !has_focus {
        ctx.memory_mut(|m| m.request_focus(id));
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
    search_icon: &IconStyle<Color32>,
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
                // `TextStyle::Small`'s logical size is `font_normal`, unscaled.
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
/// outright. Text input is unconditional, so it outranks even a search-scoped
/// binding on that letter. Otherwise a search-scoped binding match (any
/// modifiers, so `Shift+Esc` counts) is dispatched through the binding table,
/// keeping `Enter`/`Esc` rebindable; an unmodified key drives the filter or
/// browsing nav; and a modified non-search key is retained for
/// `handle_shortcuts`. Returns whether the event stays in the queue (`true`)
/// or is consumed here (`false`).
fn drain_search_or_nav(
    steps: &mut Vec<SidebarNavStep>,
    filter: &mut PanelFilter,
    shortcuts: &crate::shortcut::Shortcuts,
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
        for a in shortcuts.matches(key, modifiers) {
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

impl AlacritreeApp {
    fn reap_exited_sessions(&mut self, ctx: &Context) {
        let hold = self.config.ui.hold_exited_sessions;
        let exited_ids: Vec<SessionId> =
            self.sessions.iter().filter(|s| s.should_reap(hold)).map(|s| s.id).collect();
        for id in exited_ids {
            self.close_session(ctx, id);
        }
    }

    /// Handle session-switch requests from clicked notifications. A stale
    /// id (session closed before the click) makes the activate a no-op, but
    /// the window still comes forward, because the user asked for the app.
    fn process_notification_actions(&mut self, ctx: &Context) {
        let Some(id) = notify::latest_click(&self.notify_rx) else { return };
        self.activate_session_by_id(id);
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Drain every session's PTY events and surface "needs attention" for
    /// any session the user isn't currently looking at.
    fn process_session_events(&mut self, ctx: &Context) {
        let visible_idx = self.active_session_index();
        // `viewport().focused` is `None` on platforms that don't report focus;
        // treat unknown as "focused" so we don't pile up stale attention marks.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);

        // Only the session on screen, and only while the window has focus:
        // typing somewhere else is the one moment a terminal has no claim on
        // the machine. Both calls are no-ops unless they change something, so
        // a frame where focus has not moved costs nothing, and a session with
        // no boost to give, whether the feature is off or the platform has none,
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
        let hold = self.config.ui.hold_exited_sessions;
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
            // The exit is the last thing the PTY will ever deliver, so a
            // session that survives it says here how to dismiss it. Nothing
            // else on screen would.
            if outcome.exited && !self.sessions[idx].should_reap(hold) {
                let chord = command_palette::first_key(
                    &self.shortcuts,
                    NamedAction::CloseExitedSession(action::CloseExitedSession),
                );
                self.sessions[idx].write_hold_notice(chord.as_deref());
            }
            let live = self.session_activity(&self.sessions[idx]).live();
            if live == Some(LiveState::Working) {
                // A finished turn stops describing anything once the next
                // one starts.
                self.sessions[idx].done = false;
            }
            let is_visible_to_user = Some(idx) == visible_idx && focused;
            if is_visible_to_user {
                // Nothing pending survives the user already looking at it.
                self.sessions[idx].pending_attention = None;
                continue;
            }
            let now = Instant::now();
            let pending = PendingAttention::merge(
                self.sessions[idx].pending_attention,
                outcome.finished,
                outcome.rang,
                now,
            );
            self.sessions[idx].pending_attention = pending;
            let Some(pending) = pending else {
                continue;
            };
            match poll_attention_debounce(pending.since, now, live, grace) {
                AttentionVerdict::Cancel => self.sessions[idx].pending_attention = None,
                // A quiet PTY repaints nothing on its own, so the wake-up
                // that decides the ping has to be scheduled here.
                AttentionVerdict::Wait(remaining) => ctx.request_repaint_after(remaining),
                AttentionVerdict::Fire => {
                    self.sessions[idx].pending_attention = None;
                    // A spinner stopping on a plain shell is a finished
                    // command rather than an agent's turn, so it pings.
                    let is_agent = live.is_some();
                    let session = &mut self.sessions[idx];
                    let was_latched = session.done || session.needs_attention;
                    session.done |= pending.finished && is_agent;
                    session.needs_attention |= pending.rang || (pending.finished && !is_agent);
                    // Only toast on the transition into a latch: BEL and the
                    // title settling in the same idle cycle are one "Claude is
                    // done" event, not two.
                    if !was_latched && self.config.ui.notifications {
                        notify::attention(&self.sessions[idx], ctx);
                    }
                },
            }
        }

        // Visible session shouldn't keep an attention marker once the user is
        // actually looking at it. That covers tab switches, workspace switches,
        // and refocusing the window after stepping away.
        if focused {
            if let Some(idx) = visible_idx {
                self.sessions[idx].needs_attention = false;
                self.sessions[idx].done = false;
            }
        }
    }

    /// A session's live reading with its multiplexer's status folded in.
    fn session_activity(&self, s: &AppSession) -> SessionActivity {
        pane_backed_activity(s.activity(), self.session_pane_status(s))
    }

    /// The mark a session's row draws, from its live reading and its latches.
    fn session_shown_state(&self, s: &AppSession) -> Option<ShownState> {
        let pane_done = self.session_pane_status(s) == Some(PaneStatus::Done);
        ShownState::of(self.session_activity(s).live(), s.done || pane_done, s.needs_attention)
    }

    /// Whether any session in `ws` is blocked, done or pinged: what the
    /// attention filter keeps.
    fn workspace_needs_attention(&self, ws: &WorkspaceKey) -> bool {
        self.sessions.iter().any(|s| {
            s.working_directory == *ws
                && self.session_shown_state(s).is_some_and(ShownState::wants_attention)
        })
    }

    fn project_needs_attention(&self, project: &Project) -> bool {
        project.checkouts.iter().any(|wt| self.workspace_needs_attention(&Some(wt.path.clone())))
    }

    /// What a collapsed workspace row draws.  A session that is blocked, done
    /// or pinged wins wherever it sits, loudest first, since the collapsed row
    /// is the only place it can surface.  Otherwise the live reading follows
    /// [`Self::workspace_activity`].
    fn workspace_status(&self, ws: &WorkspaceKey) -> RowStatus<'static> {
        let loudest = self
            .sessions
            .iter()
            .filter(|s| s.working_directory == *ws)
            .filter_map(|s| Some((self.session_shown_state(s)?, s)))
            .filter(|(state, _)| state.wants_attention())
            .max_by_key(|(state, _)| *state);
        match loudest {
            Some((_, s)) => RowStatus {
                pinged: s.needs_attention,
                done: s.done || self.session_pane_status(s) == Some(PaneStatus::Done),
                activity: self.session_activity(s),
                managed: None,
            },
            None => RowStatus::live(self.workspace_activity(ws)),
        }
    }

    /// Prefer the active session's status so parallel agents do not fight over
    /// the parent row. If that session has nothing to report, a background
    /// session that is working or blocked wins over a merely present agent,
    /// because a collapsed row is the only place either state can surface.
    fn workspace_activity(&self, ws: &WorkspaceKey) -> SessionActivity {
        let active_id = self.sessions.active(ws);
        let mut other = SessionActivity::Shell;
        for s in &self.sessions {
            if s.working_directory != *ws {
                continue;
            }
            let activity = pane_backed_activity(s.activity(), self.session_pane_status(s));
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
    /// shell sessions first, then every multiplexer pane the workspace holds,
    /// attached or not, in the multiplexer's own order.
    ///
    /// Position comes from the multiplexer rather than from alacritree because
    /// it is the only party that has an opinion surviving all three moments: attach,
    /// detach, and a restart, which leaves every pane unattached again.  An
    /// order built from when a session was attached agrees with itself until
    /// the first restart and then contradicts everything the user saw.
    ///
    /// Agents are bucketed by the directory they work in, and an agent whose
    /// directory matches no worktree, including one whose checkout has been
    /// removed, lands under Home, which is the common case: an agent in a
    /// repository alacritree does not track still belongs somewhere, and a
    /// checkout that has gone cannot start a shell.
    fn listed_workspace_rows(&self) -> sidebar_nav::ListedRows {
        use sidebar_nav::WorkspaceEntry;

        // `usize::MAX` parks a pane its multiplexer has stopped listing at
        // the tail of its own block rather than letting it fall in among the
        // shells: the session is still the multiplexer's, and it goes back to
        // its slot when the listing carries it again.
        let mut shells: HashMap<WorkspaceKey, Vec<SessionId>> = HashMap::new();
        let mut managed: HashMap<WorkspaceKey, Vec<(usize, WorkspaceEntry)>> = HashMap::new();
        for session in &self.sessions {
            let ws = session.working_directory.clone();
            match session.pane_key.clone() {
                Some(key) => {
                    let at = self.multiplexers.pane_index(&key).unwrap_or(usize::MAX);
                    managed.entry(ws).or_default().push((at, WorkspaceEntry::Session(session.id)));
                },
                None => shells.entry(ws).or_default().push(session.id),
            }
        }

        for listed in self.pane_listing() {
            let at = self.multiplexers.pane_index(&listed.key).unwrap_or(usize::MAX);
            managed
                .entry(listed.workspace)
                .or_default()
                .push((at, WorkspaceEntry::Pane(listed.key)));
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

    /// The rows `ws` paints, in `listed`'s order.  An entry whose session or
    /// agent has gone since the listing was built yields no row rather than a
    /// panic; the next rebuild drops it for good.
    fn workspace_rows(
        &self,
        ws: &WorkspaceKey,
        listed: &sidebar_nav::ListedRows,
    ) -> Vec<WorkspaceRowData> {
        let Some(entries) = listed.get(ws) else { return Vec::new() };
        let active = self.sessions.active(ws);
        let is_current = self.current_workspace == *ws;
        entries
            .iter()
            .filter_map(|entry| match entry {
                sidebar_nav::WorkspaceEntry::Session(id) => {
                    let s = self.sessions.iter().find(|s| s.id == *id)?;
                    let activity = pane_backed_activity(s.activity(), self.session_pane_status(s));
                    Some(WorkspaceRowData::Session(SessionRowData {
                        id: s.id,
                        name: session_row_name(&s.title, activity, self.session_pane(s)),
                        needs_attention: s.needs_attention,
                        done: s.done,
                        activity,
                        is_active: active == Some(s.id),
                        is_displayed: is_current && active == Some(s.id),
                        managed: self.session_managed(s),
                    }))
                },
                sidebar_nav::WorkspaceEntry::Pane(key) => {
                    let pane = self.find_pane(key)?;
                    let managed = self.pane_managed(key, pane);
                    Some(WorkspaceRowData::Pane(PaneRowData::new(key.clone(), pane, managed)))
                },
            })
            .collect()
    }
}

impl AlacritreeApp {
    /// Resolve `path` to a sidebar worktree, tolerating symlinks and trailing
    /// slashes via canonicalization.
    fn known_worktree_path(&self, path: &Path) -> Option<PathBuf> {
        let canonical = path.canonicalize().ok();
        self.projects.iter().flat_map(|p| &p.checkouts).find_map(|wt| {
            (wt.path == path || canonical.as_deref() == Some(wt.path.as_path()))
                .then(|| wt.path.clone())
        })
    }
}

/// Input the user aimed at this window: keystrokes, text, IME composition,
/// clipboard actions, scrolling, zooming, pointer clicks, touches and window
/// focus.  Bare pointer motion is the one deliberate exclusion, or a mouse
/// resting over the window would hold a follow off forever; a finger cannot
/// rest without touching, so a touch is always an action.
fn is_direct_input(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key { .. }
            | egui::Event::Text(_)
            | egui::Event::Ime(_)
            | egui::Event::Paste(_)
            | egui::Event::Copy
            | egui::Event::Cut
            | egui::Event::MouseWheel { .. }
            | egui::Event::Zoom(_)
            | egui::Event::PointerButton { .. }
            | egui::Event::Touch { .. }
            | egui::Event::WindowFocused(_)
    )
}

type FrameStarted = Option<(Instant, Option<Duration>)>;

#[derive(Clone, Copy)]
struct FramePaintView {
    theme: Theme,
    modal_open: bool,
    sidebar_fill: Color32,
    central_fill: Color32,
}

impl AlacritreeApp {
    fn begin_update(&mut self, ctx: &Context) -> FrameStarted {
        let frame_started = self
            .frame_log
            .as_ref()
            .map(|_| (std::time::Instant::now(), crate::frame_log::output_wait()));
        self.grid_paint = std::time::Duration::ZERO;
        let (any_event, direct_input) = ctx.input(|i| {
            self.mouse_hide.observe(self.config.mouse.hide_when_typing, &i.events);
            (!i.events.is_empty(), i.events.iter().any(is_direct_input))
        });
        if any_event {
            self.last_input = Instant::now();
        }
        if direct_input {
            self.last_direct_input = Some(Instant::now());
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
        frame_started
    }

    fn poll_update_jobs(&mut self, ctx: &Context) {
        self.poll_project_refreshes();
        self.poll_pending_spawns(ctx);
        self.poll_pane_creates(ctx);
        self.poll_pane_attaches(ctx);
        // Unconditional: either sidebar can be hidden, and a drain hung off one
        // of them would strand every entry the other polled.
        self.pr_cache.drain_completed(ctx);
        self.poll_pending_deletes(ctx);
        self.poll_pending_creates(ctx);
        self.poll_multiplexers();
        self.reconcile_pane_sessions(ctx);
        self.sync_pane_views(ctx);
        // Poll first, then check `failed`: a panicked job's `poll` returns
        // `None` forever, so `failed` is what stops its handle from sitting
        // here for the rest of the process.
        self.detached_jobs.retain(|job| match job.poll() {
            Some(()) => false,
            None => !job.failed(),
        });
        self.phases.mark("polls");
    }

    fn handle_update_input(&mut self, ctx: &Context) -> bool {
        let modal_open = self.is_modal_open();
        // Keys pressed mid-composition drive the IME's candidate window,
        // not the app. Alacritty's key_input returns early the same way,
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
        modal_open
    }

    fn frame_paint_view(&self, modal_open: bool) -> FramePaintView {
        let theme = self.theme;
        // GL clear is the sole source of the bg when opacity < 1; painting any
        // panel fill on top would compound the alpha through egui's blend.
        let translucent = self.config.window.opacity < 1.0;
        let sidebar_fill = if translucent { Color32::TRANSPARENT } else { theme.sidebar_bg };
        // Opaque, this fill is what a collapsed cell shows, so it tracks the
        // terminal's background for the same reason the clear does.
        let terminal_bg = self.grid_snapshot.default_bg();
        let central_fill = if translucent { Color32::TRANSPARENT } else { terminal_bg };

        FramePaintView { theme, modal_open, sidebar_fill, central_fill }
    }

    fn paint_sidebars(&mut self, ctx: &Context, view: FramePaintView) -> Option<egui::Rect> {
        let FramePaintView { theme, modal_open, sidebar_fill, .. } = view;
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

        sidebar_rect
    }

    fn paint_central(
        &mut self,
        ctx: &Context,
        view: FramePaintView,
        sidebar_rect: Option<egui::Rect>,
    ) {
        let FramePaintView { theme, modal_open, central_fill, .. } = view;
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
                        RichText::new("no session. Press Ctrl+T to open one").color(theme.text_dim),
                    );
                    return;
                };
                let editor_text = theme.editor_text;
                let editor_hint = theme.editor_hint;
                let editor_error = theme.error;
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
                } else if let Some(view) = session.tasks.as_mut() {
                    self.ime.clear();
                    crate::tasks::view::show(
                        ui,
                        view,
                        allow_focus,
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
                // app focus, so keyboard "clicks" must not steal it back.
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
    }

    fn paint_dialogs(&mut self, ctx: &Context, modal_open: bool) {
        if self.modals.pending_create.is_some() {
            self.show_create_dialog(ctx);
        }
        if self.modals.pending_delete.is_some() {
            self.show_delete_dialog(ctx);
        }
        if self.modals.pending_session_close.is_some() {
            self.show_close_session_dialog(ctx);
        }
        if self.modals.pending_detach_all.is_some() {
            self.show_detach_all_dialog(ctx);
        }
        if self.modals.pending_rename.is_some() {
            self.show_rename_dialog(ctx);
        }
        if self.modals.pending_base_branch.is_some() {
            self.show_base_branch_picker(ctx);
        }
        if self.modals.pending_project_remove.is_some() {
            self.show_remove_project_dialog(ctx);
        }
        if self.modals.error_dialog.is_some() {
            self.show_error_dialog(ctx);
        }
        if self.modals.quit_dialog_open {
            self.show_quit_dialog(ctx);
        }
        if self.palette.is_open() && !modal_open {
            self.show_command_palette(ctx);
        }
        self.modals.gate.end_frame();
        self.phases.mark("dialogs");
    }

    fn finish_update(&mut self, ctx: &Context, frame: &eframe::Frame, frame_started: FrameStarted) {
        self.reap_exited_sessions(ctx);
        // A shell that exited on its own is only removed here, after paint.
        // Without this pass its deferred verdict would wait for unrelated
        // input; with it, the repair is queued for the frame the repaint
        // request has already scheduled.
        self.reconcile_sidebar_focus(ctx);
        self.phases.mark("reap");

        // After paint, so it outlasts the hover cursors the widgets set while
        // they drew.  egui clears the icon every frame, so this reasserts it
        // for as long as the pointer stays hidden.
        if self.mouse_hide.hidden() {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
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

impl eframe::App for AlacritreeApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // This clear is the only thing painting a cell the grid leaves alone,
        // so it has to carry the terminal's own background rather than the
        // configured one.  eframe reads it before `update`, so a colour OSC 11
        // moved this frame lands next frame; terminal output requests a repaint
        // of its own, so the stale frame is replaced rather than left up.
        let bg = self.grid_snapshot.default_bg();
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
        let frame_started = self.begin_update(ctx);
        self.poll_update_jobs(ctx);
        let modal_open = self.handle_update_input(ctx);
        let view = self.frame_paint_view(modal_open);
        let sidebar_rect = self.paint_sidebars(ctx, view);
        self.paint_central(ctx, view, sidebar_rect);
        self.paint_dialogs(ctx, modal_open);
        self.finish_update(ctx, frame, frame_started);
    }
}

/// Logical-pixel (normal, heading) sizes for UI text.  `[ui.font] size`
/// overrides the normal size directly (same pt→px conversion as
/// `FontConfig::logical_size`); the heading keeps its existing ratio to normal
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

/// What a FocusLeft/FocusRight press does, decided by [`focus_move`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusMove {
    /// The TUI inside the terminal can still move that way, so forward the
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
/// no window left in that direction, which is why IPC moves never pass
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

/// What the binding pass knows about the frame when it decides whether a
/// matched action may consume a key press.  Each field names a scope some
/// action is gated on; outside it the action stands aside.
#[derive(Clone, Copy, Default)]
struct BindingScope {
    sidebar_focused: bool,
    git_focused: bool,
    scratchpad_focused: bool,
    /// The terminal owns focus and the session on screen has exited, so no
    /// child is left to read the keys its bindings would otherwise consume.
    exited_session_focused: bool,
}

/// What the binding pass needs to know about the session on screen.
#[derive(Clone, Copy)]
struct SessionFocus {
    scratchpad: bool,
    exited: bool,
}

/// The scope a key press is judged in.  Kept apart from the frame it is read
/// from so the mapping can be pinned on its own: `exited_session_focused` is
/// what lets a bare `Enter` be a chord at all, and the filter chain below
/// cannot tell a correct mapping from an inverted one.
fn binding_scope(
    focus: PaneFocus,
    palette_open: bool,
    active: Option<SessionFocus>,
) -> BindingScope {
    // The palette is a modal that owns every key while it is up.
    let active = active.filter(|_| focus == PaneFocus::Terminal && !palette_open);
    BindingScope {
        sidebar_focused: focus == PaneFocus::ProjectsSidebar && !palette_open,
        git_focused: focus == PaneFocus::GitSidebar && !palette_open,
        scratchpad_focused: active.is_some_and(|s| s.scratchpad),
        exited_session_focused: active.is_some_and(|s| s.exited),
    }
}

/// Whether a matched binding's key press should reach `action`, given what
/// currently owns keyboard focus. Filter actions are scoped to the
/// sidebar that owns them so a bare letter like `d` doesn't fire a git-panel
/// filter while the projects sidebar (or the terminal) has focus, and vice
/// versa. `terminal_only` actions additionally step aside for the scratchpad
/// editor, which wants those same keys for native text editing.
fn valid_for_focus(action: &BindingAction, scope: BindingScope) -> bool {
    let focus_ok = match action {
        BindingAction::Named(n) if n.is_exited_session_scoped() => scope.exited_session_focused,
        BindingAction::Named(n) if n.is_projects_filter_scoped() => scope.sidebar_focused,
        BindingAction::Named(n) if n.is_git_filter_scoped() => scope.git_focused,
        BindingAction::Named(n) if n.is_sidebar_scoped() => scope.sidebar_focused,
        _ => true,
    };
    let terminal_only = match action {
        BindingAction::Chars(_) => true,
        BindingAction::Named(n) => n.is_terminal_only(),
        BindingAction::Unsupported(_) => false,
    };
    focus_ok && !(scope.scratchpad_focused && terminal_only)
}

/// The actions one key press dispatches. Stacked user bindings can mix a
/// scoped action with a global one on a single trigger, so each is judged on
/// its own; an empty result leaves the press in the event queue, which is what
/// keeps a bare-key binding such as `Enter`, `Delete` or a plain letter out
/// of the PTY's way while its scope is inactive.
fn dispatched_actions(matched: Vec<&BindingAction>, scope: BindingScope) -> Vec<&BindingAction> {
    matched
        .into_iter()
        .filter(|a| valid_for_focus(a, scope))
        // Search actions are owned by the sidebar nav pass; here their default
        // Enter/Esc/Shift+Esc must fall through to the PTY when the terminal
        // (or a non-searching panel) has focus.
        .filter(|a| !matches!(a, BindingAction::Named(n) if n.is_search_scoped()))
        // Palette cursor moves are owned by the palette modal, which suppresses
        // this pass entirely while it is up. Reaching here means it is closed,
        // so their keys belong to the sidebar or the PTY.
        .filter(|a| !matches!(a, BindingAction::Named(n) if n.is_palette_scoped()))
        .collect()
}

fn workspace_label_for(projects: &[Project], ws: &WorkspaceKey) -> String {
    let Some(path) = ws else {
        return "Home".to_string();
    };
    for project in projects {
        for wt in &project.checkouts {
            if &wt.path == path {
                return format!("{} / {}", project.display_name(), wt.name);
            }
        }
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| wsl::display_path(path))
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

fn wsl_shell(distro: &str, workdir: &Path) -> ShellCommand {
    let (program, args) = wsl::shell_invocation(distro, workdir);
    ShellCommand::new(program, args)
}

/// Shimmed when the resident helper is on; the plain wsl.exe login-shell
/// launch (and an unknown probe) otherwise.
fn wsl_session_shell(distro: &str, workdir: &Path) -> (Option<ShellCommand>, Option<WslProbe>) {
    if !wsl_helper::enabled() {
        return (Some(wsl_shell(distro, workdir)), None);
    }
    let key = wsl_helper::new_probe_key();
    let (program, args) = wsl_helper::shim_invocation(distro, workdir, &key);
    (Some(ShellCommand::new(program, args)), Some(WslProbe { distro: distro.to_string(), key }))
}

/// The probe shim for any user-supplied wsl.exe argv (profile or
/// `[terminal.shell]`): `Some` only when the argv is fully understood and
/// a distro name is known. The probe registry needs one, so a wrapped
/// default-distro launch resolves it via enumeration. Anything exotic
/// runs unmodified and probes as unknown.
fn shimmed_wsl_argv(program: &str, args: &[String]) -> Option<(ShellCommand, WslProbe)> {
    if !wsl_helper::enabled() {
        return None;
    }
    let key = wsl_helper::new_probe_key();
    let (args, distro) = wsl_helper::wrap_profile_argv(program, args, &key)?;
    let distro =
        distro.or_else(|| wsl::distros().into_iter().find(|d| d.is_default).map(|d| d.name))?;
    Some((ShellCommand::new(program.to_string(), args), WslProbe { distro, key }))
}

/// The probe shim for a multiplexer attach, which launches a command rather
/// than a login shell.  A WSL attach runs the multiplexer inside the distro,
/// where the Windows descendant walk cannot see it, so it needs the helper's
/// foreground probe to answer for it.  A native attach already stands in that
/// walk, and an argv this module cannot wrap probes as unknown: both get
/// `None` and spawn the argv unchanged.
pub(crate) fn multiplexer_attach_probe(
    side: &Side,
    program: &str,
    argv: &[String],
) -> Option<(Vec<String>, WslProbe)> {
    let Side::Wsl(distro) = side else {
        return None;
    };
    if !wsl_helper::enabled() {
        return None;
    }
    let key = wsl_helper::new_probe_key();
    let wrapped = wsl_helper::wrap_exec_argv(program, argv, &key)?;
    Some((wrapped, WslProbe { distro: distro.clone(), key }))
}

fn profile_session_shell(
    profile: &crate::config::Profile,
) -> (Option<ShellCommand>, Option<WslProbe>) {
    match shimmed_wsl_argv(&profile.program, &profile.args) {
        Some((shell, probe)) => (Some(shell), Some(probe)),
        None => (Some(profile_shell(profile)), None),
    }
}

/// `[terminal.shell] program = "wsl.exe"` gets the same shim as a wsl.exe
/// profile; any other config shell (or none) spawns unchanged through
/// `Session::pending_shell`'s own config-shell default.
fn config_session_shell(
    config: &crate::config::Config,
) -> (Option<ShellCommand>, Option<WslProbe>) {
    match &config.shell {
        Some(s) => match shimmed_wsl_argv(&s.program, &s.args) {
            Some((shell, probe)) => (Some(shell), Some(probe)),
            None => (None, None),
        },
        None => (None, None),
    }
}

fn profile_shell(profile: &crate::config::Profile) -> ShellCommand {
    ShellCommand::new(profile.program.clone(), profile.args.clone())
}

/// The modal frame's horizontal inner margin.  Any width budgeted against the
/// window has to leave room for it, so it lives apart from the frame itself.
fn modal_pad_x(scale: f32) -> f32 {
    (16.0 * scale).round()
}

/// Destination index for moving the item at `from` so it lands before display
/// slot `insert_before` (counted in the pre-move list), or `None` for a no-op.
/// Removing `from` before inserting shifts every later slot down by one, the
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
/// the cursor whatever has focus, which is the wrong convention here. A key
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

/// Spawn-ordered ids of the sessions in `ws`, or empty below the list
/// threshold. The threshold is normally two, so a single-session workspace
/// row keeps its compact form, mirroring the tab strip. `always` lowers it
/// to one.
///
/// Pane rows are never held back by it: a pane alacritree does not own has
/// no other surface to appear on, and hiding it would hide the workspace's
/// only row.  They do count toward the threshold, so a lone shell session
/// beside one is listed rather than folded into the workspace row, which
/// would leave a hole in a list its neighbours are already in.
///
/// `managed` carries the multiplexer's own position for each of its rows, and
/// sorts by it here so an attached session and a listed pane interleave the
/// way the multiplexer has them rather than by which kind of row they are.
/// The sort is stable, so panes it no longer lists keep the order they were
/// spawned in.
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

/// Where the view goes after a session's removal.
#[derive(Debug, PartialEq)]
enum CloseFallback {
    /// Removal didn't empty the on-screen workspace, so there is no navigation.
    Stay,
    /// Switch to the project's main checkout, which still has a session.
    Activate(PathBuf),
    /// A session in another workspace, chosen by `ring_landing`.
    ActivateSession(SessionId),
    /// Switch to home; `activate_home` spawns a shell there if none exists.
    Home,
}

/// Why a session cannot move to another workspace.
#[derive(Debug, thiserror::Error)]
enum MoveError {
    #[error("no session with id {0}. See list_sessions")]
    NoSession(SessionId),
    #[error("scratchpads belong to their backing workspace and cannot be moved")]
    Scratchpad,
    #[error("a tasks tab shows the lists of the workspace it was opened in")]
    Tasks,
    #[error("diff panes belong to the workspace they were opened from")]
    Diff,
}

/// Why a session record is going away.  The distinction exists because
/// neither half of a close, the respawn policy or the navigation, may apply
/// to a session that never got a PTY.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CloseReason {
    User,
    SpawnFailed,
    /// The workspace's directory is being removed, so nothing may respawn
    /// into it and the view goes home rather than to its main checkout.
    WorktreeDeleted,
}

/// The verdict a close acts on.  A failed open stays put whatever the
/// workspace's state says: every destination `close_fallback` can name is one
/// `ensure_active_session` will spawn into, and that open fails the same way.
/// Staying leaves the pane on the "no session" placeholder, which is what the
/// workspace honestly holds.
fn close_navigation(reason: CloseReason, verdict: CloseFallback) -> CloseFallback {
    match reason {
        CloseReason::User | CloseReason::WorktreeDeleted => verdict,
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
/// closed. This is the ordinal rule `sidebar_focus::slide` lands the cursor
/// by, so a close that moves both cannot point them at different siblings.
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
    /// Switch the view to the target, because the user was watching this session.
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
    let main = project.checkouts.iter().find(|w| w.is_main)?;
    if main.path == ws { None } else { Some(main.path.clone()) }
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
        SidebarRow::Pane(_) => return None,
    };
    projects
        .iter()
        .find(|p| p.checkouts.iter().any(|w| w.path == workspace))
        .map(|p| p.root.clone())
}

/// The session a SelectNextSession/SelectPreviousSession press lands on:
/// one flat ring over every open session, workspaces in sidebar order and
/// each workspace's sessions in the order its rows are drawn. `None` means
/// stay put, for a ring too small to cycle or an active session missing from
/// the ring (its worktree turned prunable). With no active session (an
/// emptied workspace on screen) the first entry re-anchors the cycle.
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

/// The activity a session's row paints.  A session attached to a
/// multiplexer's pane takes the multiplexer's word, because it watches the
/// pane from outside and sees an approval dialog no title heuristic can reach.
///
/// `unknown` is the multiplexer declining to say, so the session's own
/// reading stands.  The gate closes either way: an attached pane holds an
/// agent whether or not the process probe recognized one.
fn pane_backed_activity(own: SessionActivity, status: Option<PaneStatus>) -> SessionActivity {
    let Some(status) = status else { return own };
    let live = LiveState::from_pane(status).unwrap_or(own.live().unwrap_or_default());
    own.with_live(live)
}

/// The liveness cache corrects discovery for paint and navigation only. Keep
/// this shared so a row that has just gone grey cannot remain a dead stop in
/// the workspace ring. Main checkouts are never prune candidates, even when
/// their project is a non-git directory with no .git entry.
fn worktree_looks_gone(wt: &Checkout, missing: Option<bool>) -> bool {
    missing.map_or(wt.gone, |gone| gone && !wt.is_main)
}

fn worktree_is_switchable(wt: &Checkout, missing: Option<bool>, has_sessions: bool) -> bool {
    !worktree_looks_gone(wt, missing) || has_sessions
}

/// Whether ending a session asks first.  A harness-managed one is a detach
/// rather than a kill, so it answers to its own switch: the attach client is
/// always running, which would make the busy question a close asks fire every
/// time and warn about nothing.
fn close_needs_prompt(ui: &UiTheme, managed: bool, busy: bool) -> bool {
    if managed { ui.confirm_session_detach } else { ui.confirm_session_close.requires_prompt(busy) }
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
    format!("{} is not a worktree in the sidebar. See list_projects", path.display())
}

#[cfg(test)]
mod tests {
    use alacritree_herdr::{self as herdr, AttachMode, PendingAttach, PendingCreate};

    use super::*;
    use crate::config::{SidebarFocus, UiTheme};
    use crate::multiplexer::{CreatedPane, Launch};

    use super::focus::search_reveal_root;
    use super::git_panel::{
        GIT_FILTER_TOGGLES, base_branch_target, branch_diff_row, file_row, git_filter_identity,
        git_path_label, path_header_label,
    };
    use super::modals::dirty_warning;
    use super::palette::{PaletteColumns, paint_palette_row, pane_palette_content};
    use super::panes::WorkspaceSwitch;
    use super::sidebar::{
        RowName, WorktreeRowView, home_row, pane_display_name, session_row, session_row_title,
        sessions_filter_passes, upstream_badge, worktree_row,
    };
    use super::widgets::agent_hint;
    use crate::multiplexer::{
        AttachRequest, MultiplexerKind, Pane, PaneError, PaneStatus, Scripted,
    };
    use crate::sidebar_model::build_snapshot;
    use crate::test_util::herdr_pane_key;

    fn plain_worktree_row<'a>(
        wt: &'a alacritree_vcs::Checkout,
        icons: &'a crate::config::Icons<Color32>,
        theme: &'a Theme,
    ) -> WorktreeRowView<'a> {
        WorktreeRowView {
            wt,
            missing: None,
            display_name: &wt.name,
            pr: None,
            is_active: true,
            is_cursor: false,
            scroll_into_view: false,
            status: RowStatus::live(SessionActivity::Shell),
            deleting: false,
            profiles: &[],
            icons,
            theme,
        }
    }

    fn herdr_lifecycle_app() -> AlacritreeApp {
        let mut config = Config::default();
        config.integrations.herdr.enabled = true;
        config.integrations.herdr.show_unmatched = true;
        let (_, notify_rx) = mpsc::channel();
        let theme = Theme::from_config(&config);
        let mut app = AlacritreeApp::from_parts(
            config,
            theme,
            state::PersistedState::default(),
            Vec::new(),
            (Vec::new(), crate::fonts::FaceMetrics::default()),
            notify_rx,
            (None, None),
        );
        app.current_workspace = Some(PathBuf::from("unopened-workspace"));
        app
    }

    #[test]
    fn checkout_hooks_open_once_per_worktree_per_process() {
        use alacritree_checkout_hooks::fake::{Event, FakeHook};

        let mut app = test_app();
        app.projects.push(project_with("/repo", &["/repo/wt"]));
        let hook = FakeHook::silent();
        let wt = PathBuf::from("/repo/wt");
        let jobs_before = app.detached_jobs.len();
        app.open_checkout_hooks(wt.clone(), |_| vec![hook.clone()]);
        app.open_checkout_hooks(wt.clone(), |_| vec![hook.clone()]);
        assert_eq!(app.detached_jobs.len(), jobs_before + 1, "the second open ran the hooks again");

        let deadline = Instant::now() + Duration::from_secs(10);
        while hook.events().is_empty() {
            assert!(Instant::now() < deadline, "the hook job never ran");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(hook.events(), [Event::Opened { main: "/repo".into(), checkout: wt }]);
    }

    #[test]
    fn checkout_hooks_skip_a_directory_no_project_owns() {
        use alacritree_checkout_hooks::fake::FakeHook;

        let mut app = test_app();
        let hook = FakeHook::silent();
        app.open_checkout_hooks(PathBuf::from("/not/a/project/worktree"), |_| vec![hook.clone()]);
        assert!(app.detached_jobs.is_empty());
        assert!(hook.events().is_empty());
    }

    /// An app with one plain shell session, for tests that need nothing
    /// herdr-specific from the app itself.
    fn test_app() -> AlacritreeApp {
        let (_, notify_rx) = mpsc::channel();
        let mut app = AlacritreeApp::from_parts(
            Config::default(),
            Theme::from_config(&Config::default()),
            state::PersistedState::default(),
            Vec::new(),
            (Vec::new(), crate::fonts::FaceMetrics::default()),
            notify_rx,
            (None, None),
        );
        let (session, _) = Session::pending_shell(
            Context::default(),
            &app.config,
            None,
            TermSize { columns: 80, screen_lines: 24 },
            (8.0, 16.0),
            None,
            None,
        );
        app.sessions.push(session);
        app
    }

    fn checkout_at(path: &std::path::Path) -> alacritree_vcs::Checkout {
        alacritree_vcs::Checkout {
            name: "main".into(),
            path: path.to_path_buf(),
            head: alacritree_vcs::Head::default(),
            is_main: true,
            gone: false,
            upstream: None,
        }
    }

    #[test]
    fn a_project_with_a_backend_and_no_trunk_has_a_worktree_for_the_tasks_scope() {
        let mut app = test_app();
        let root = PathBuf::from("/r");
        app.projects.push(Project {
            vcs: Some(crate::vcs::Vcs::Fake(alacritree_vcs::fake::FakeVcs::new("/r"))),
            trunk: None,
            checkouts: vec![checkout_at(&root)],
            ..Project::placeholder(root.clone())
        });
        let (project, worktree) = app.project_and_worktree(&Some(root));
        assert!(project.is_some());
        assert!(worktree.is_some());
    }

    /// `test_app` with its one session in a workspace nobody is looking at,
    /// the only place an attention trigger can latch.  Desktop notifications
    /// are off so a latch does not reach the OS.
    fn app_with_a_background_session(title: &str) -> AlacritreeApp {
        let mut app = test_app();
        app.config.ui.notifications = false;
        app.current_workspace = Some(PathBuf::from("elsewhere"));
        app.sessions[0].title = title.to_owned();
        app
    }

    fn drain(app: &mut AlacritreeApp, event: alacritty_terminal::event::Event) {
        app.sessions[0].inject_for_test(event);
        app.process_session_events(&Context::default());
    }

    /// An agent's spinner stopping while nobody is looking is a finished
    /// turn: the row shows done, not a ping, and the attention filter keeps
    /// its workspace.  A bell on top pings underneath, and looking at the
    /// session clears both.
    #[test]
    fn a_background_agent_that_finishes_shows_done_until_viewed() {
        use alacritty_terminal::event::Event;
        let mut app = app_with_a_background_session("\u{280b} claude");
        drain(&mut app, Event::Title("\u{2733} claude".into()));
        assert!(app.sessions[0].done);
        assert!(!app.sessions[0].needs_attention);
        assert_eq!(app.session_shown_state(&app.sessions[0]), Some(ShownState::Done));
        assert!(app.workspace_needs_attention(&None));

        drain(&mut app, Event::Bell);
        assert!(app.sessions[0].needs_attention);
        assert_eq!(app.session_shown_state(&app.sessions[0]), Some(ShownState::Done));

        app.current_workspace = None;
        app.set_active_in_current_workspace(app.sessions[0].id);
        app.process_session_events(&Context::default());
        assert!(!app.sessions[0].done);
        assert!(!app.sessions[0].needs_attention);
    }

    /// A spinner stopping on a plain shell is a finished command, not an
    /// agent's turn, so it pings the way it always has.
    #[test]
    fn a_background_shell_whose_spinner_stops_is_pinged() {
        use alacritty_terminal::event::Event;
        let mut app = app_with_a_background_session("\u{280b} cargo build");
        drain(&mut app, Event::Title("cargo build".into()));
        assert!(!app.sessions[0].done);
        assert!(app.sessions[0].needs_attention);
        assert_eq!(app.session_shown_state(&app.sessions[0]), Some(ShownState::Pinged));
    }

    /// Going back to work retires a finished turn: the next one has started,
    /// so "done" no longer describes anything.
    #[test]
    fn going_back_to_work_clears_done() {
        use alacritty_terminal::event::Event;
        let mut app = app_with_a_background_session("\u{280b} claude");
        drain(&mut app, Event::Title("\u{2733} claude".into()));
        assert!(app.sessions[0].done);
        drain(&mut app, Event::Title("\u{2819} claude".into()));
        assert!(!app.sessions[0].done);
        assert_eq!(app.session_shown_state(&app.sessions[0]), Some(ShownState::Working));
    }

    /// An app with the scripted multiplexer on and nothing else, for the pane
    /// lifecycle.  Its workspace is one nothing has opened, so a pane that
    /// should land somewhere has to say where.
    fn lifecycle_app() -> AlacritreeApp {
        let config = Config::default();
        let (_, notify_rx) = mpsc::channel();
        let theme = Theme::from_config(&config);
        let mut app = AlacritreeApp::from_parts(
            config,
            theme,
            state::PersistedState::default(),
            Vec::new(),
            (Vec::new(), crate::fonts::FaceMetrics::default()),
            notify_rx,
            (None, None),
        );
        app.current_workspace = Some(PathBuf::from("unopened-workspace"));
        app.multiplexers.only_scripted().enable().show_unmatched(true);
        app
    }

    /// An icon styled by its glyph alone.
    fn glyph_icon(glyph: &str) -> crate::config::IconStyle {
        crate::config::IconStyle { glyph: Some(glyph.to_string()), ..Default::default() }
    }

    /// What the scripted multiplexer lists on `side`, replacing whatever it
    /// listed before.
    fn adopt_panes(app: &mut AlacritreeApp, side: &Side, panes: Vec<Pane>) {
        app.multiplexers.scripted_mut().set_panes(side, panes);
    }

    /// A session holding the scripted pane `terminal` on `side`.
    fn bind_pane_fixture(app: &mut AlacritreeApp, side: &Side, terminal: &str) -> SessionId {
        let (mut session, _) = Session::pending_shell(
            Context::default(),
            &app.config,
            None,
            TermSize { columns: 80, screen_lines: 24 },
            (8.0, 16.0),
            None,
            None,
        );
        session.bind_pane(Scripted::key(side, terminal), true);
        let id = session.id;
        app.sessions.push(session);
        id
    }

    fn bind_herdr_fixture(app: &mut AlacritreeApp, side: Side, terminal: &str) -> SessionId {
        let (mut session, _) = Session::pending_shell(
            Context::default(),
            &app.config,
            None,
            TermSize { columns: 80, screen_lines: 24 },
            (8.0, 16.0),
            None,
            None,
        );
        session.bind_pane(herdr_pane_key(side, terminal), true);
        let id = session.id;
        app.sessions.push(session);
        id
    }

    fn adopt_herdr_fixture(app: &mut AlacritreeApp, side: Side, json: &str, at: Instant) {
        app.multiplexers.herdr_mut_for_test().adopt_listing_for_test(&side, json, at);
    }

    /// A pane's row as the sidebar builds it.  `shared_view` is what a
    /// multiplexer answers when opening the row shows someone else's pane
    /// rather than handing over the pane itself.
    fn pane_row(pane: &Pane, side: Side, shared_view: bool) -> PaneRowData {
        let mut multiplexers = Multiplexers::new(&crate::config::IntegrationsConfig::default());
        multiplexers.only_scripted().enable().attach_directly(!shared_view);
        let key = Scripted::key(&side, &pane.terminal_id);
        let managed = multiplexers.get(key.multiplexer).managed(&key.side, Some(pane));
        PaneRowData::new(key, pane, managed)
    }

    /// Every variant of `egui::Event` as compiled in this build, so an egui
    /// bump that adds, removes or renames one fails this test instead of
    /// silently narrowing or widening the direct-input clock.
    #[test]
    fn is_direct_input_classifies_every_compiled_event_variant() {
        let modifiers = egui::Modifiers::default();
        let cases: &[(egui::Event, bool)] = &[
            (egui::Event::Copy, true),
            (egui::Event::Cut, true),
            (egui::Event::Paste(String::new()), true),
            (egui::Event::Text(String::new()), true),
            (
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                true,
            ),
            (egui::Event::PointerMoved(egui::Pos2::ZERO), false),
            (egui::Event::MouseMoved(egui::Vec2::ZERO), false),
            (
                egui::Event::PointerButton {
                    pos: egui::Pos2::ZERO,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers,
                },
                true,
            ),
            (egui::Event::PointerGone, false),
            (egui::Event::Zoom(1.0), true),
            (egui::Event::Ime(egui::ImeEvent::Commit(String::new())), true),
            (
                egui::Event::Touch {
                    device_id: egui::TouchDeviceId(0),
                    id: egui::TouchId(0),
                    phase: egui::TouchPhase::Start,
                    pos: egui::Pos2::ZERO,
                    force: None,
                },
                true,
            ),
            (
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::Vec2::ZERO,
                    modifiers,
                },
                true,
            ),
            (egui::Event::WindowFocused(true), true),
            (
                egui::Event::Screenshot {
                    viewport_id: egui::ViewportId::default(),
                    user_data: egui::UserData::default(),
                    image: Arc::new(egui::ColorImage::new([1, 1], egui::Color32::BLACK)),
                },
                false,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(is_direct_input(event), *expected, "{event:?}");
        }
    }

    #[test]
    fn a_plain_shell_session_reports_no_agent_and_no_multiplexer() {
        let app = test_app();
        let session = app.sessions.first().expect("the app starts with a session");
        let json = app.session_json(session, true);
        assert_eq!(json["agent"], Value::Null);
        assert_eq!(json["multiplexer"], Value::Null);
        assert!(json["busy"].is_boolean());
    }

    /// A checkout switched outside the app, from a terminal or another tool,
    /// reaches the row's branch without a manual refresh.  The PR badge is
    /// keyed to that branch, so a stale one keeps painting the old branch's PR.
    #[test]
    fn a_branch_switched_outside_the_app_reaches_the_sidebar() {
        let dir = tempfile::tempdir().unwrap();
        let root = alacritree_git::test_support::init_repo(&dir.path().join("main"));
        let linked = alacritree_git::test_support::add_worktree(&root, "topic");
        let mut app = test_app();
        app.projects.push(
            jobs::on_this_thread(|b| {
                Project::discover(
                    root.clone(),
                    &crate::vcs::backends(&app.config.integrations),
                    false,
                    b,
                )
            })
            .project,
        );

        for (checkout, branch) in [(&root, "switched-main"), (&linked, "switched-linked")] {
            alacritree_git::test_support::switch_head(checkout, branch);
        }

        let branches = |app: &AlacritreeApp| {
            let mut branches: Vec<String> =
                app.projects[0].checkouts.iter().filter_map(|wt| wt.head.name.clone()).collect();
            branches.sort();
            branches
        };
        let drawn: Vec<PathBuf> =
            app.projects[0].checkouts.iter().map(|wt| wt.path.clone()).collect();
        let ctx = Context::default();
        app.poll_worktree_liveness(&ctx, true, &drawn);
        let deadline = Instant::now() + Duration::from_secs(10);
        while branches(&app) != ["switched-linked", "switched-main"] && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            app.poll_worktree_liveness(&ctx, false, &[]);
            app.poll_project_refreshes();
        }

        assert_eq!(branches(&app), ["switched-linked", "switched-main"]);
    }

    #[test]
    fn a_pane_backed_session_names_its_side_and_terminal() {
        let mut app = test_app();
        let key = herdr_pane_key(Side::Native, "t7");
        let id = app.sessions.first().expect("a session").id;
        if let Some(session) = app.sessions.iter_mut().find(|s| s.id == id) {
            session.bind_pane(key, true);
        }
        let session = app.sessions.iter().find(|s| s.id == id).expect("a session");
        let json = app.session_json(session, true);
        assert_eq!(json["multiplexer"]["name"], "herdr");
        assert_eq!(json["multiplexer"]["side"], "native");
        assert_eq!(json["multiplexer"]["terminal_id"], "t7");
        // The attach client is itself the foreground job, so probing it would
        // answer true forever.
        assert_eq!(json["busy"], Value::Null);
    }

    #[test]
    fn a_wsl_side_spells_its_distro() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let key = Scripted::key(&Side::Wsl("ubuntu".into()), "t1");
        let id = app.sessions.first().expect("a session").id;
        if let Some(session) = app.sessions.iter_mut().find(|s| s.id == id) {
            session.bind_pane(key, true);
        }
        let session = app.sessions.iter().find(|s| s.id == id).expect("a session");
        assert_eq!(app.session_json(session, true)["multiplexer"]["side"], "wsl:ubuntu");
    }

    /// The listing is the only way a client learns a pane exists before
    /// anything is attached to it, so a pane no session holds must appear
    /// with a null session id rather than be filtered out the way the
    /// sidebar filters one.
    #[test]
    fn the_pane_listing_carries_an_unattached_pane_with_a_null_session_id() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        adopt_panes(&mut app, &Side::Native, vec![
            Scripted::pane("term-loose")
                .with_agent("claude", PaneStatus::Working)
                .with_title("loose pane")
                .in_dir("/repo"),
        ]);
        let json = app.multiplexer_panes_json();
        let panes = json["panes"].as_array().expect("panes array");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["multiplexer"]["terminal_id"], "term-loose");
        assert_eq!(panes[0]["session_id"], Value::Null);
    }

    /// A pane a session already holds still appears, naming that session, so
    /// a client can tell "already open" from "not there".
    #[test]
    fn the_pane_listing_names_the_session_holding_a_pane() {
        let mut app = test_app();
        let side = Side::Native;
        app.multiplexers.scripted_mut().enable();
        adopt_panes(&mut app, &side, vec![
            Scripted::pane("term-held")
                .with_agent("claude", PaneStatus::Working)
                .with_title("held pane")
                .in_dir("/repo"),
        ]);
        let id = bind_pane_fixture(&mut app, &side, "term-held");
        let json = app.multiplexer_panes_json();
        let panes = json["panes"].as_array().expect("panes array");
        let pane = panes
            .iter()
            .find(|p| p["multiplexer"]["terminal_id"] == "term-held")
            .expect("the fixture pane");
        assert_eq!(pane["session_id"].as_u64(), Some(id));
    }

    /// A cache nothing polls has nothing to report, so the reply is empty
    /// rather than an error: there is no pane to fail to find.
    #[test]
    fn the_pane_listing_is_empty_while_the_integration_is_disabled() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().disable();
        adopt_panes(&mut app, &Side::Native, vec![Scripted::pane("term-hidden").in_dir("/repo")]);
        let json = app.multiplexer_panes_json();
        assert_eq!(json["panes"].as_array().expect("panes array").len(), 0);
    }

    /// Three refusals a client acts on differently: only the disabled one is
    /// worth retrying after a config change, and a caller that cannot tell
    /// them apart retries all three or none.
    #[test]
    fn attaching_to_a_side_that_names_no_server_is_refused_by_name() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (None, "bogus", "t1"),
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("`bogus` is not a side, expected `native` or `wsl:<distro>`".to_string())
        );
    }

    #[test]
    fn attaching_to_a_pane_no_endpoint_reports_is_refused_by_name() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (None, "native", "missing"),
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("no pane `missing` on native, see list_multiplexer_panes".to_string())
        );
    }

    #[test]
    fn attaching_while_the_integration_is_disabled_says_so() {
        let mut app = test_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = false;
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (Some("herdr"), "native", "t1"),
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("the herdr integration is disabled ([integrations.herdr] enabled)".to_string())
        );
    }

    /// With every multiplexer off, a request naming none cannot be pointed at
    /// one table to fix, so the refusal names them all.
    #[test]
    fn attaching_while_every_integration_is_disabled_names_each_table() {
        let mut app = test_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = false;
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (None, "native", "t1"),
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("every multiplexer integration is disabled ([integrations.herdr] or \
                 [integrations.zellij] enabled)"
                .to_string())
        );
    }

    #[test]
    fn attaching_through_a_multiplexer_that_does_not_exist_is_refused_by_name() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (Some("tmux"), "native", "t1"),
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("`tmux` is not a multiplexer, expected `herdr` or `zellij`".to_string())
        );
    }

    /// A zellij pane reaches every client the way a herdr one does, named by
    /// its multiplexer and session, through nothing but the trait.
    #[test]
    fn a_zellij_pane_is_listed_under_its_own_name() {
        let mut app = test_app();
        let pane = Pane {
            terminal_id: "work/terminal_3".into(),
            pane_id: "terminal_3".into(),
            tab_id: Some("0".into()),
            kind: None,
            title: Some("shell".into()),
            status: None,
            focused: true,
            cwd: Some("/repo".into()),
            foreground_cwd: None,
        };
        app.multiplexers.zellij_mut_for_test().adopt_for_test(vec![
            alacritree_zellij::SideListing {
                side: Side::Native,
                sessions: vec!["work".into()],
                read: vec!["work".into()],
                panes: vec![pane],
                sampled_at: Instant::now(),
            },
        ]);

        let json = app.multiplexer_panes_json();

        let listed = &json["panes"][0]["multiplexer"];
        assert_eq!(listed["name"], "zellij");
        assert_eq!(listed["session"], "work");
        assert_eq!(listed["terminal_id"], "work/terminal_3");
    }

    /// A pane a session already holds answers with that session rather than
    /// opening a second attach client against the same pane.
    #[test]
    fn attaching_to_a_pane_a_session_already_holds_returns_that_session() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let side = Side::Native;
        adopt_panes(&mut app, &side, vec![
            Scripted::pane("term-held").with_agent("claude", PaneStatus::Working).in_dir("/repo"),
        ]);
        let id = bind_pane_fixture(&mut app, &side, "term-held");
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (None, "native", "term-held"),
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(reply_rx.try_recv().unwrap(), Ok(json!({ "session_id": id })));
    }

    /// A background attach to a pane a session already holds answers with
    /// that session without bringing it on screen.
    #[test]
    fn attaching_without_focus_to_a_held_pane_leaves_the_screen_alone() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let side = Side::Native;
        adopt_panes(&mut app, &side, vec![
            Scripted::pane("term-held").with_agent("claude", PaneStatus::Working).in_dir("/repo"),
        ]);
        let asked_from = Some(PathBuf::from("elsewhere"));
        app.current_workspace = asked_from.clone();
        let tab = app.sessions[0].id;
        app.sessions.set_active(None, tab);
        let id = bind_pane_fixture(&mut app, &side, "term-held");
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (None, "native", "term-held"),
            reply_tx,
            AttachFocus::Leave,
        );

        assert_eq!(reply_rx.try_recv().unwrap(), Ok(json!({ "session_id": id })));
        assert_eq!(app.current_workspace, asked_from);
        assert_eq!(app.sessions.active(&None), Some(tab));
    }

    /// A background attach that has to wait on the multiplexer leaves the
    /// workspace on screen alone, and names its own workspace as the one a
    /// refusal restores so a late refusal cannot move the user either.
    #[test]
    fn attaching_without_focus_queues_an_attach_that_stays_in_the_background() {
        let mut app = lifecycle_app();
        adopt_panes(&mut app, &Side::Native, vec![
            Scripted::pane("term-loose").with_agent("claude", PaneStatus::Working).in_dir("/repo"),
        ]);
        let asked_from = app.current_workspace.clone();
        let (reply_tx, _reply_rx) = mpsc::channel();

        app.defer_attach_multiplexer_pane(
            &Context::default(),
            (None, "native", "term-loose"),
            reply_tx,
            AttachFocus::Leave,
        );

        let pending = app
            .multiplexers
            .scripted()
            .pending_attach()
            .first()
            .expect("the pane queued an attach");
        assert_eq!(pending.request.focus, AttachFocus::Leave);
        assert_eq!(
            pending.request.previous, pending.request.workspace,
            "a refusal restores nothing"
        );
        assert_eq!(app.current_workspace, asked_from);
    }

    /// Naming a side that no server answers on is a different failure from
    /// naming one that is not a side at all, and a caller retrying the second
    /// is retrying a typo.
    #[test]
    fn creating_a_pane_on_a_side_that_names_no_server_is_refused_by_name() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_create_multiplexer_pane(
            &Context::default(),
            (None, Some("bogus")),
            None,
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("`bogus` is not a side, expected `native` or `wsl:<distro>`".to_string())
        );
        assert!(
            app.multiplexers.scripted().pending_create().is_empty(),
            "a refused side still asked the multiplexer"
        );
    }

    /// With no herdr session focused and more than one server reachable,
    /// there is no side the request could have meant, so the refusal names
    /// the ones it could.
    #[test]
    fn creating_a_pane_with_no_side_and_several_servers_names_the_choices() {
        let mut app = test_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = true;
        let listing = r#"{"result":{"panes":[
            {"terminal_id":"term-a","pane_id":"w1:p1","agent":"claude","agent_status":"idle","cwd":"/repo"}
        ]}}"#;
        adopt_herdr_fixture(&mut app, Side::Native, listing, Instant::now());
        adopt_herdr_fixture(&mut app, Side::Wsl("ubuntu".into()), listing, Instant::now());
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_create_multiplexer_pane(
            &Context::default(),
            (None, None),
            None,
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("no herdr session is focused and native and wsl:ubuntu are answering; name one"
                .to_string())
        );
        assert!(
            app.multiplexers.herdr_for_test().pending_create_for_test().is_empty(),
            "an unresolved side still asked herdr"
        );
    }

    /// A key or palette gesture carries no side to name, so a request with
    /// nowhere to land must still tell the user rather than doing nothing.
    /// A key press that resolves to nothing looks like a broken binding.
    #[test]
    fn dispatching_new_multiplexer_pane_with_no_herdr_server_shows_an_error_dialog() {
        let mut app = test_app();

        app.dispatch_action(
            &Context::default(),
            BindingAction::Named(NamedAction::NewMultiplexerPane(action::NewMultiplexerPane)),
            ActionOrigin::Keyboard,
        );

        assert_eq!(
            app.modals.error_dialog.as_deref(),
            Some("no herdr server is answering; start one, or name a side")
        );
        assert!(
            app.multiplexers.herdr_for_test().pending_create_for_test().is_empty(),
            "an unresolved side still asked herdr"
        );
    }

    /// A disabled integration must say so, not blame a missing server: the
    /// no-server wording sends the user to start one, which does nothing
    /// while `enabled` stays false.
    #[test]
    fn dispatching_new_multiplexer_pane_while_disabled_names_the_integration() {
        let mut app = test_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = false;

        app.dispatch_action(
            &Context::default(),
            BindingAction::Named(NamedAction::NewMultiplexerPane(action::NewMultiplexerPane)),
            ActionOrigin::Keyboard,
        );

        assert_eq!(app.modals.error_dialog.as_deref(), Some(app.multiplexers.disabled_reason()));
        assert!(
            app.multiplexers.herdr_for_test().pending_create_for_test().is_empty(),
            "a disabled integration still asked herdr"
        );
    }

    /// The side of the pane already on screen is what asking for another one
    /// means, so a focused multiplexer session answers the question the
    /// request left open.
    #[test]
    fn creating_a_pane_takes_the_side_of_the_focused_multiplexer_session() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        adopt_panes(&mut app, &Side::Native, vec![
            Scripted::pane("term-native").with_agent("claude", PaneStatus::Idle).in_dir("/repo"),
        ]);
        let side = Side::Wsl("ubuntu".into());
        let id = bind_pane_fixture(&mut app, &side, "term-focused");
        app.set_active_in_current_workspace(id);

        assert_eq!(app.create_target(None, None).unwrap(), (MultiplexerKind::Scripted, side));
    }

    /// A side whose rows are still drawn through one missed poll is still
    /// the side answering, or a create would tell the user to start a server
    /// the sidebar shows running.
    #[test]
    fn a_side_that_missed_one_poll_is_still_the_one_a_create_means() {
        let mut app = test_app();
        adopt_herdr_fixture(
            &mut app,
            Side::Native,
            r#"{"result":{"panes":[
                {"terminal_id":"term-native","pane_id":"w1:p1","agent":"claude","agent_status":"idle","cwd":"/repo"}
            ]}}"#,
            Instant::now(),
        );

        app.multiplexers.herdr_mut_for_test().caches_mut_for_test()[0]
            .fail_listing_for_test(herdr::PollError::Absent("spawn_failed"));

        assert_eq!(app.create_target(None, None).unwrap(), (MultiplexerKind::Herdr, Side::Native));
    }

    /// A worktree the sidebar does not have is refused before the multiplexer
    /// is asked, so a typo never leaves a pane behind in it.
    #[test]
    fn creating_a_pane_in_an_unknown_worktree_is_refused() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let id = bind_pane_fixture(&mut app, &Side::Native, "term-focused");
        app.set_active_in_current_workspace(id);
        let unknown = PathBuf::from("no-such-worktree");
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_create_multiplexer_pane(
            &Context::default(),
            (None, Some("native")),
            Some(unknown.clone()),
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(reply_rx.try_recv().unwrap(), Err(unknown_worktree(&unknown)));
        assert!(
            app.multiplexers.scripted().pending_create().is_empty(),
            "an unknown worktree still asked the multiplexer"
        );
    }

    /// A pane opened somewhere other than the workspace would still have its
    /// session filed under that workspace, so a workspace the distro has no
    /// path for is refused before the multiplexer is asked and no pane is left
    /// behind.
    #[test]
    fn creating_a_pane_in_a_workspace_the_distro_cannot_see_is_refused() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        let share = PathBuf::from(r"\\fileserver\share\repo");
        app.current_workspace = Some(share.clone());
        let id = bind_pane_fixture(&mut app, &Side::Wsl("no-such-distro".into()), "term");
        app.set_active_in_current_workspace(id);

        app.dispatch_action(
            &Context::default(),
            BindingAction::Named(NamedAction::NewMultiplexerPane(action::NewMultiplexerPane)),
            ActionOrigin::Keyboard,
        );

        let expected = format!("{} has no path inside the no-such-distro distro", share.display());
        assert_eq!(app.modals.error_dialog.as_deref(), Some(expected.as_str()));
        assert!(
            app.multiplexers.scripted().pending_create().is_empty(),
            "an untranslatable workspace still asked the multiplexer"
        );
    }

    /// A create the multiplexer refused must answer whoever asked for the
    /// pane, not just leave a dialog on a window the caller cannot see.
    #[test]
    fn poll_pane_create_answers_the_waiter_when_the_multiplexer_refuses() {
        let mut app = test_app();
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers
            .scripted_mut()
            .enable()
            .answer_create(Err(PaneError::Scripted("boom".into())));
        app.create_multiplexer_pane(
            &Context::default(),
            (MultiplexerKind::Scripted, Side::Native),
            Some(PathBuf::from("some/workspace")),
            Some(reply_tx),
            AttachFocus::Take,
        );

        app.poll_pane_creates(&Context::default());

        assert_eq!(reply_rx.try_recv().unwrap(), Err("boom".to_string()));
        assert_eq!(app.modals.error_dialog.as_deref(), Some("boom"));
        assert_eq!(app.current_workspace, None, "a refused create moved the user");
    }

    /// A create whose worker panicked resolves through the same `failed()`
    /// path a stalled one does, and owes its waiter the same answer.
    #[test]
    fn poll_herdr_create_answers_the_waiter_when_the_create_never_finished() {
        let mut app = test_app();
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers.herdr_mut_for_test().pending_create_mut_for_test().push(PendingCreate {
            job: jobs::Job::panicked(),
            side: Side::Native,
            request: crate::multiplexer::CreateRequest {
                workspace: None,
                waiter: Some(reply_tx),
                focus: AttachFocus::Take,
            },
        });

        app.poll_pane_creates(&Context::default());

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("the herdr pane create did not finish".to_string())
        );
    }

    /// A create herdr answered, for the tests that follow the pane into its
    /// attach.  A WSL side is one where a pane holding an agent would be
    /// handed over directly under the default mode.
    fn created_pane_fixture(
        workspace: WorkspaceKey,
        waiter: Option<mpsc::Sender<ipc::protocol::IpcResult>>,
    ) -> PendingCreate {
        PendingCreate {
            job: jobs::Job::ready(Ok(CreatedPane {
                terminal_id: "term-new".into(),
                pane_id: "w1:p2".into(),
                tab_id: "w1:t2".into(),
            })),
            side: Side::Wsl("distro".into()),
            request: crate::multiplexer::CreateRequest {
                workspace,
                waiter,
                focus: AttachFocus::Take,
            },
        }
    }

    /// The pane a create answers with, for the tests that follow it into its
    /// attach.
    fn created_pane() -> CreatedPane {
        CreatedPane {
            terminal_id: "term-new".into(),
            pane_id: "w1:p2".into(),
            tab_id: "w1:t2".into(),
        }
    }

    /// `tab create` starts a shell, and every `herdr agent` subcommand
    /// refuses a pane with no agent in it, so the pane a create just made is
    /// reached through its tab even where an agent's pane would be handed
    /// over directly.
    #[test]
    fn poll_herdr_create_attaches_the_new_pane_through_its_tab() {
        let mut app = test_app();
        assert_eq!(
            app.multiplexers.herdr_mut_for_test().config_mut_for_test().attach,
            AttachMode::Agent,
            "the default mode"
        );
        // Not on disk, so an attach that took the direct branch is refused
        // before it can start a PTY.
        let workspace = Some(PathBuf::from("this/path/does/not/exist"));
        app.multiplexers
            .herdr_mut_for_test()
            .pending_create_mut_for_test()
            .push(created_pane_fixture(workspace.clone(), None));

        app.poll_pane_creates(&Context::default());

        let queued = app
            .multiplexers
            .herdr_for_test()
            .pending_attach_for_test()
            .first()
            .expect("the create queued a shared view");
        let side = Side::Wsl("distro".into());
        assert_eq!(queued.key, herdr_pane_key(side.clone(), "term-new"));
        assert_eq!(queued.target, PaneTarget { side, pane_id: "w1:p2".into(), has_agent: false });
        assert_eq!(queued.request.workspace, workspace);
        assert_eq!(app.current_workspace, workspace);
        assert!(app.multiplexers.herdr_for_test().pending_create_for_test().is_empty());
    }

    /// A created pane whose gesture the multiplexer refuses leaves the user in
    /// the workspace they were in, not the one the create switched to on its
    /// way, and tells whoever asked for the pane why.
    #[test]
    fn poll_pane_create_restores_the_workspace_when_the_gesture_is_refused() {
        let mut app = test_app();
        let (reply_tx, reply_rx) = mpsc::channel();
        let refusal = "the pane could not be focused: no such tab".to_string();
        app.multiplexers
            .scripted_mut()
            .enable()
            .answer_create(Ok(created_pane()))
            .answer_attach(Err(PaneError::Scripted(refusal.clone())));
        app.create_multiplexer_pane(
            &Context::default(),
            (MultiplexerKind::Scripted, Side::Native),
            Some(PathBuf::from("some/workspace")),
            Some(reply_tx),
            AttachFocus::Take,
        );
        app.poll_pane_creates(&Context::default());
        assert!(
            !app.multiplexers.scripted().pending_attach().is_empty(),
            "the create queued a shared view"
        );

        app.poll_pane_attaches(&Context::default());

        assert_eq!(reply_rx.try_recv().unwrap(), Err(refusal));
        assert_eq!(app.current_workspace, None, "a refused attach left the user moved");
    }

    /// The pane a create made is still unlisted when its session opens, and
    /// an unlisted pane reads as holding an agent, so a session that worked
    /// out its own kind there would record a direct attach and never follow
    /// the multiplexer's focus.
    #[test]
    fn a_created_pane_opens_a_session_that_shares_the_view() {
        let mut app = test_app();
        // The PTY opens on the pool, so the session's record and binding land
        // here without a client having to start.
        app.config.ui.async_session_spawn = true;
        app.multiplexers.scripted_mut().enable().answer_create(Ok(created_pane())).answer_attach(
            Ok(Launch { program: "alacritree-test-no-such-client".into(), argv: Vec::new() }),
        );
        app.create_multiplexer_pane(
            &Context::default(),
            (MultiplexerKind::Scripted, Side::Wsl("distro".into())),
            None,
            None,
            AttachFocus::Take,
        );
        app.poll_pane_creates(&Context::default());

        app.poll_pane_attaches(&Context::default());

        let key = Scripted::key(&Side::Wsl("distro".into()), "term-new");
        let id = app.pane_session(&key).expect("the gesture opened a session");
        let session = app.sessions.iter().find(|session| session.id == id).unwrap();
        assert!(session.shared_view, "the created pane's session was recorded as direct");
    }

    /// A click on a pane whose background attach is already waiting on herdr
    /// restarts the gesture with focus, since the running one left herdr where
    /// it was. A background request joining a click changes nothing.
    #[test]
    fn a_click_joining_a_background_attach_asks_herdr_again_with_focus() {
        let mut app = test_app();
        let mut created = created_pane_fixture(None, None);
        created.request.focus = AttachFocus::Leave;
        app.multiplexers.herdr_mut_for_test().pending_create_mut_for_test().push(created);
        app.poll_pane_creates(&Context::default());
        let running = || {
            Some(jobs::Job::ready(Ok(Launch {
                program: "alacritree-test-no-such-client".into(),
                argv: Vec::new(),
            })))
        };
        app.multiplexers.herdr_mut_for_test().pending_attach_mut_for_test()[0].job = running();
        let key = herdr_pane_key(Side::Wsl("distro".into()), "term-new");
        let unlisted =
            PaneTarget { side: key.side.clone(), pane_id: "w1:p2".into(), has_agent: false };
        let switch = WorkspaceSwitch { to: None, from: None };
        let ctx = Context::default();

        app.attach_pane(&ctx, key.clone(), unlisted.clone(), &switch, None, AttachFocus::Take);

        let pending = &app.multiplexers.herdr_for_test().pending_attach_for_test()[0];
        assert_eq!(pending.request.focus, AttachFocus::Take);
        assert!(pending.job.is_none(), "the gesture that left herdr's focus alone was kept");

        app.multiplexers.herdr_mut_for_test().pending_attach_mut_for_test()[0].job = running();
        app.attach_pane(&ctx, key, unlisted, &switch, None, AttachFocus::Leave);

        let pending = &app.multiplexers.herdr_for_test().pending_attach_for_test()[0];
        assert_eq!(pending.request.focus, AttachFocus::Take);
        assert!(pending.job.is_some(), "a background request restarted the click's gesture");
    }

    /// A background create the multiplexer refused answers its client and
    /// leaves the user's screen alone.
    #[test]
    fn a_refused_background_create_answers_only_its_client() {
        let mut app = test_app();
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers
            .scripted_mut()
            .enable()
            .answer_create(Err(PaneError::Scripted("boom".into())));
        app.create_multiplexer_pane(
            &Context::default(),
            (MultiplexerKind::Scripted, Side::Native),
            None,
            Some(reply_tx),
            AttachFocus::Leave,
        );

        app.poll_pane_creates(&Context::default());

        assert_eq!(reply_rx.try_recv().unwrap(), Err("boom".to_string()));
        assert!(app.modals.error_dialog.is_none());
    }

    /// A pane created in the background opens its session behind the tab its
    /// workspace already shows, and the multiplexer never hears it is on
    /// screen, so nothing asks it to focus the pane until the user goes there.
    #[test]
    fn a_pane_created_without_focus_opens_behind_the_active_tab() {
        let mut app = test_app();
        app.config.ui.async_session_spawn = true;
        let tab = app.sessions[0].id;
        app.sessions.set_active(None, tab);
        let asked_from = Some(PathBuf::from("elsewhere"));
        app.current_workspace = asked_from.clone();
        let side = Side::Wsl("distro".into());
        app.multiplexers.scripted_mut().enable().answer_create(Ok(created_pane()));
        app.create_multiplexer_pane(
            &Context::default(),
            (MultiplexerKind::Scripted, side.clone()),
            None,
            None,
            AttachFocus::Leave,
        );
        app.poll_pane_creates(&Context::default());
        let queued = app
            .multiplexers
            .scripted()
            .pending_attach()
            .first()
            .expect("the create queued a shared view");
        assert_eq!(queued.request.focus, AttachFocus::Leave);
        app.multiplexers.scripted_mut().answer_attach(Ok(Launch {
            program: "alacritree-test-no-such-client".into(),
            argv: Vec::new(),
        }));

        app.poll_pane_attaches(&Context::default());

        let key = Scripted::key(&side, "term-new");
        let id = app.pane_session(&key).expect("the gesture opened a session");
        assert_ne!(id, tab);
        assert_eq!(app.sessions.active(&None), Some(tab));
        assert_eq!(app.current_workspace, asked_from);
        assert!(app.multiplexers.scripted().attached().is_empty());
    }

    #[test]
    fn creating_a_pane_while_the_integration_is_disabled_says_so() {
        let mut app = test_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = false;
        let (reply_tx, reply_rx) = mpsc::channel();

        app.defer_create_multiplexer_pane(
            &Context::default(),
            (None, None),
            None,
            reply_tx,
            AttachFocus::Take,
        );

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err(app.multiplexers.disabled_reason().to_string())
        );
    }

    #[test]
    fn attached_herdr_palette_keeps_filtered_shell_metadata() {
        let mut app = herdr_lifecycle_app();
        app.config.ui.path_style.git_rows = PathStyle::Fish;
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().show_panes = false;
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().icon.glyph = Some("✦".into());
        let side = Side::Native;
        adopt_herdr_fixture(
            &mut app,
            side.clone(),
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","pane_id":"w1:p1","agent":"claude","agent_status":"working","terminal_title_stripped":"review work","cwd":"/private/project"}
        ]}}"#,
            Instant::now(),
        );
        let id = bind_herdr_fixture(&mut app, side.clone(), "term-shell");
        adopt_herdr_fixture(
            &mut app,
            side,
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","pane_id":"w1:p1","terminal_title_stripped":"review work","cwd":"/private/project"}
        ]}}"#,
            Instant::now(),
        );
        app.reconcile_pane_sessions(&Context::default());

        assert_eq!(app.sessions[0].id, id);
        assert!(app.pane_listing().is_empty());
        let items = app.palette_items();
        let row = items
            .iter()
            .position(|item| item.action == PaletteAction::ActivateSession(id))
            .unwrap();
        let item = &items[row];
        assert_eq!(item.primary, "review work");
        assert!(item.subtitle.as_deref().unwrap().starts_with("✦ "));
        assert_eq!(item.secondary, "herdr · shell");
        let hover = item.hover.as_deref().unwrap();
        for detail in [
            "Pane: w1:p1",
            "Terminal: term-shell",
            "Cwd: /private/project",
            "herdr, shared view",
            "Activate: switch to this session",
        ] {
            assert!(hover.contains(detail), "{detail}: {hover}");
        }
        assert!(!hover.contains("Status: working"));
        let mut palette = CommandPalette::new();
        *palette.query_mut() = "herdr".into();
        assert!(palette.rank(&items).contains(&row));
        for hidden in ["term-shell", "/private/project"] {
            *palette.query_mut() = hidden.into();
            assert!(!palette.rank(&items).contains(&row));
        }
    }

    #[test]
    fn attached_herdr_palette_keeps_known_details_after_failed_or_incomplete_poll() {
        for reply in [
            Err(herdr::PollError::Absent("spawn_failed")),
            Ok(r#"{"result":{"panes":[{"terminal_id":"term-kept"}]}}"#),
        ] {
            let mut app = herdr_lifecycle_app();
            app.config.ui.path_style.git_rows = PathStyle::Fish;
            app.multiplexers.herdr_mut_for_test().config_mut_for_test().show_panes = false;
            app.multiplexers.herdr_mut_for_test().config_mut_for_test().icon.glyph =
                Some("✦".into());
            let side = Side::Wsl("fixture-distro".into());
            adopt_herdr_fixture(
                &mut app,
                side.clone(),
                r#"{"result":{"panes":[
                {"terminal_id":"term-kept","pane_id":"w1:p1","agent":"claude","agent_status":"working","terminal_title_stripped":"review work","cwd":"/private/project"}
            ]}}"#,
                Instant::now(),
            );
            let id = bind_herdr_fixture(&mut app, side.clone(), "term-kept");
            let cache = app
                .multiplexers
                .herdr_mut_for_test()
                .caches_mut_for_test()
                .iter_mut()
                .find(|cache| cache.side() == &side)
                .unwrap();
            cache.complete_listing_for_test(
                reply,
                herdr::Listing::Panes,
                herdr::Listing::Agents,
                Instant::now(),
            );
            app.reconcile_pane_sessions(&Context::default());

            assert_eq!(app.sessions[0].id, id);
            let items = app.palette_items();
            let row = items
                .iter()
                .position(|item| item.action == PaletteAction::ActivateSession(id))
                .unwrap();
            let item = &items[row];
            assert_eq!(item.primary, "review work");
            assert!(item.subtitle.as_deref().unwrap().starts_with("✦ "));
            assert_eq!(item.secondary, "herdr · claude");
            let hover = item.hover.as_deref().unwrap();
            for detail in [
                "Pane: w1:p1",
                "Terminal: term-kept",
                "Side: wsl:fixture-distro",
                "Cwd: /private/project",
                "herdr",
                "Activate: switch to this session",
            ] {
                assert!(hover.contains(detail), "{detail}: {hover}");
            }
            assert!(!hover.contains("Status: working"));
            let mut palette = CommandPalette::new();
            *palette.query_mut() = "herdr".into();
            assert!(palette.rank(&items).contains(&row));
            for hidden in ["term-kept", "fixture-distro", "/private/project"] {
                *palette.query_mut() = hidden.into();
                assert!(!palette.rank(&items).contains(&row));
            }
        }
    }

    #[test]
    fn attached_herdr_palette_retains_details_across_a_pre_attachment_agent_reply() {
        let mut app = herdr_lifecycle_app();
        app.config.ui.path_style.git_rows = PathStyle::Fish;
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().show_panes = false;
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().icon.glyph = Some("✦".into());
        app.multiplexers.herdr_mut_for_test().caches_mut_for_test()[0].complete_listing_for_test(
            Ok(r#"{"result":{"agents":[{"terminal_id":"term-kept","pane_id":"w1:p1","agent":"claude","agent_status":"working","terminal_title_stripped":"review work","cwd":"/private/project","focused":true}]}}"#),
            herdr::Listing::Agents,
            herdr::Listing::Agents,
            Instant::now(),
        );
        let request_started = Instant::now();
        let id = bind_herdr_fixture(&mut app, Side::Native, "term-kept");
        app.multiplexers.herdr_mut_for_test().caches_mut_for_test()[0].complete_listing_for_test(
            Ok(r#"{"result":{"agents":[]}}"#),
            herdr::Listing::Agents,
            herdr::Listing::Agents,
            request_started,
        );

        for after_failure in [false, true] {
            if after_failure {
                app.multiplexers.herdr_mut_for_test().caches_mut_for_test()[0]
                    .complete_listing_for_test(
                        Err(herdr::PollError::Absent("spawn_failed")),
                        herdr::Listing::Panes,
                        herdr::Listing::Agents,
                        Instant::now(),
                    );
            }
            app.reconcile_pane_sessions(&Context::default());
            assert_eq!(app.sessions[0].id, id);
            assert!(app.multiplexers.herdr_for_test().caches_for_test()[0].inventory().is_none());
            let items = app.palette_items();
            let row = items
                .iter()
                .position(|item| item.action == PaletteAction::ActivateSession(id))
                .unwrap();
            let item = &items[row];
            assert_eq!(item.primary, "review work", "after failure: {after_failure}");
            assert_eq!(item.secondary, "herdr · claude");
            assert!(item.subtitle.as_deref().unwrap().starts_with("✦ "));
            let hover = item.hover.as_deref().unwrap();
            for detail in [
                "Pane: w1:p1",
                "Terminal: term-kept",
                "Cwd: /private/project",
                "herdr",
                "Activate: switch to this session",
            ] {
                assert!(hover.contains(detail), "{detail}: {hover}");
            }
            assert!(!hover.contains("Status: working"));
            let pane = app.multiplexers.herdr_for_test().caches_for_test()[0]
                .attachment_pane("term-kept")
                .unwrap();
            assert!(!pane.current);
            assert!(pane.agent.status.is_none());
            assert!(!pane.agent.focused);
            let mut palette = CommandPalette::new();
            *palette.query_mut() = "herdr".into();
            assert!(palette.rank(&items).contains(&row));
            for hidden in ["term-kept", "/private/project"] {
                *palette.query_mut() = hidden.into();
                assert!(!palette.rank(&items).contains(&row));
            }
        }
    }

    #[test]
    fn attached_herdr_palette_without_metadata_uses_the_binding() {
        let mut app = herdr_lifecycle_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().icon.glyph = Some("✦".into());
        let id = bind_herdr_fixture(&mut app, Side::Native, "term-unseen");
        let items = app.palette_items();
        let row = items
            .iter()
            .position(|item| item.action == PaletteAction::ActivateSession(id))
            .unwrap();
        let item = &items[row];
        assert!(item.subtitle.as_deref().unwrap().starts_with("✦"));
        assert_eq!(item.secondary, "herdr · shell");
        let hover = item.hover.as_deref().unwrap();
        assert!(hover.contains("Terminal: term-unseen"));
        assert!(hover.contains("herdr, shared view."));
        assert!(!hover.contains("Pane:"));
        assert!(!hover.contains("Status:"));
        let mut palette = CommandPalette::new();
        *palette.query_mut() = "herdr".into();
        assert!(palette.rank(&items).contains(&row));
    }

    /// One poll that could not spawn is not evidence that the agents went
    /// away, and the poll behind it usually answers.  Dropping the listing on
    /// the first failure takes every status on the side down at once, which on
    /// a loaded machine is what the user gets instead of an agent's state.
    #[test]
    fn a_single_failed_poll_leaves_a_rows_status_alone() {
        let mut app = herdr_lifecycle_app();
        let side = Side::Native;
        adopt_herdr_fixture(
            &mut app,
            side.clone(),
            r#"{"result":{"panes":[
            {"terminal_id":"term-kept","pane_id":"w1:p1","agent":"claude","agent_status":"working","cwd":"/private/project"}
        ]}}"#,
            Instant::now(),
        );
        let id = bind_herdr_fixture(&mut app, side, "term-kept");

        app.multiplexers.herdr_mut_for_test().caches_mut_for_test()[0]
            .fail_listing_for_test(herdr::PollError::Absent("spawn_failed"));

        assert_eq!(app.session_pane_status(&app.sessions[0]), Some(PaneStatus::Working));
        let items = app.palette_items();
        let item =
            items.iter().find(|item| item.action == PaletteAction::ActivateSession(id)).unwrap();
        assert!(item.hover.as_deref().unwrap().contains("Status: working"));
    }

    /// A stale retained pane still names herdr in the middle column.  The
    /// override that drops its status for staleness carries the "herdr" lead
    /// itself rather than relying on `pane_palette_content` to have already
    /// supplied one.
    #[test]
    fn a_stale_herdr_row_still_leads_its_middle_column_with_herdr() {
        let mut app = herdr_lifecycle_app();
        let side = Side::Native;
        adopt_herdr_fixture(
            &mut app,
            side.clone(),
            r#"{"result":{"panes":[
            {"terminal_id":"term-stale","pane_id":"w1:p1","terminal_title_stripped":"review work","cwd":"/private/project"}
        ]}}"#,
            Instant::now(),
        );
        let id = bind_herdr_fixture(&mut app, side.clone(), "term-stale");
        let cache = app
            .multiplexers
            .herdr_mut_for_test()
            .caches_mut_for_test()
            .iter_mut()
            .find(|cache| cache.side() == &side)
            .unwrap();
        cache.complete_listing_for_test(
            Err(herdr::PollError::Absent("spawn_failed")),
            herdr::Listing::Panes,
            herdr::Listing::Agents,
            Instant::now(),
        );
        app.reconcile_pane_sessions(&Context::default());

        assert_eq!(app.sessions[0].id, id);
        let pane = app.multiplexers.herdr_for_test().caches_for_test()[0]
            .attachment_pane("term-stale")
            .unwrap();
        assert!(!pane.current);
        let items = app.palette_items();
        let item =
            items.iter().find(|item| item.action == PaletteAction::ActivateSession(id)).unwrap();
        assert_eq!(item.secondary, "herdr · shell");
    }

    #[test]
    fn herdr_inventory_removes_only_the_closed_terminal_through_session_cleanup() {
        let mut app = herdr_lifecycle_app();
        let side = Side::Native;
        adopt_herdr_fixture(
            &mut app,
            side.clone(),
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","pane_id":"w1:p1"},
            {"terminal_id":"term-gone","pane_id":"w1:p2","agent":"claude","agent_status":"working"}
        ]}}"#,
            Instant::now(),
        );
        let shell = bind_herdr_fixture(&mut app, side.clone(), "term-shell");
        let gone = bind_herdr_fixture(&mut app, side.clone(), "term-gone");
        app.sessions.set_active(None, gone);
        adopt_herdr_fixture(
            &mut app,
            side,
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","pane_id":"w1:p1"}
        ]}}"#,
            Instant::now(),
        );

        app.reconcile_pane_sessions(&Context::default());

        assert_eq!(app.sessions.iter().map(|session| session.id).collect::<Vec<_>>(), [shell]);
        assert_eq!(app.sessions.active(&None), Some(shell));
        assert!(
            app.multiplexers.herdr_for_test().caches_for_test()[0]
                .attachment_pane("term-gone")
                .is_none()
        );
    }

    #[test]
    fn herdr_inventory_empty_reply_removes_rows_and_cancels_local_focus_bookkeeping() {
        let mut app = herdr_lifecycle_app();
        let side = Side::Native;
        let gone = bind_herdr_fixture(&mut app, side.clone(), "term-gone");
        let other = bind_herdr_fixture(&mut app, side.clone(), "term-other");
        app.sessions.set_active(None, gone);
        app.modals.pending_session_close = Some(gone);
        app.multiplexers.herdr_mut_for_test().view_mut_for_test().attached(
            gone,
            None,
            Instant::now(),
        );
        *app.multiplexers.herdr_mut_for_test().view_focus_mut_for_test() =
            Some(herdr::HerdrViewFocus {
                session: gone,
                key: herdr_pane_key(side.clone(), "term-gone"),
                job: jobs::Job::ready(Ok(())),
            });
        let key = app.sessions[0].pane_key.clone().unwrap();
        app.multiplexers.herdr_mut_for_test().pending_attach_mut_for_test().push(PendingAttach {
            target: PaneTarget::unlisted(&key, "w1:p1"),
            key,
            job: Some(jobs::Job::ready(Ok(Launch { program: "herdr".into(), argv: Vec::new() }))),
            request: AttachRequest {
                workspace: None,
                previous: None,
                waiters: Vec::new(),
                focus: AttachFocus::Take,
            },
        });
        adopt_herdr_fixture(&mut app, side, r#"{"result":{"panes":[]}}"#, Instant::now());

        app.reconcile_pane_sessions(&Context::default());

        assert!(!app.sessions.iter().any(|session| [gone, other].contains(&session.id)));
        assert!(!app.sessions.has_active(&None));
        assert!(app.modals.pending_session_close.is_none());
        assert!(app.multiplexers.herdr_mut_for_test().view_focus_mut_for_test().is_none());
        assert!(app.multiplexers.herdr_mut_for_test().view_mut_for_test().visible.is_none());
        assert!(app.multiplexers.herdr_mut_for_test().view_mut_for_test().focused.is_none());
        assert!(app.multiplexers.herdr_for_test().pending_attach_for_test().is_empty());
    }

    /// A shared-view gesture the multiplexer refused must still answer whoever
    /// attached to read the pane, not just clear the error dialog.
    #[test]
    fn poll_pane_attach_sends_a_refused_gestures_error_to_every_waiter() {
        let mut app = test_app();
        let key = Scripted::key(&Side::Native, "t1");
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers
            .scripted_mut()
            .enable()
            .answer_attach(Err(PaneError::Scripted("boom".into())));
        let unlisted = PaneTarget::unlisted(&key, "w1:p1");
        let switch = WorkspaceSwitch { to: None, from: None };
        app.attach_pane(
            &Context::default(),
            key,
            unlisted,
            &switch,
            Some(reply_tx),
            AttachFocus::Take,
        );

        app.poll_pane_attaches(&Context::default());

        assert_eq!(reply_rx.try_recv().unwrap(), Err("boom".to_string()));
    }

    /// A gesture whose worker panicked resolves through the same `failed()`
    /// path a stalled one does, and owes its waiters the same answer.
    #[test]
    fn poll_herdr_attach_sends_a_panicked_gestures_message_to_every_waiter() {
        let mut app = test_app();
        let key = herdr_pane_key(Side::Native, "t1");
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers.herdr_mut_for_test().pending_attach_mut_for_test().push(PendingAttach {
            job: Some(jobs::Job::panicked()),
            target: PaneTarget::unlisted(&key, "w1:p1"),
            key,
            request: AttachRequest {
                workspace: None,
                previous: None,
                waiters: vec![reply_tx],
                focus: AttachFocus::Take,
            },
        });

        app.poll_pane_attaches(&Context::default());

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("the herdr attach did not finish".to_string())
        );
    }

    /// `open_pane_session` can fail synchronously, with no process ever
    /// spawned: `spawn_session_with_shell` refuses a workspace
    /// `worktree_gone` cannot find on disk before it touches a PTY. A pending
    /// attach that resolves into that refusal still owes its waiters an
    /// answer, not just the error dialog.
    #[test]
    fn poll_pane_attach_answers_waiters_when_the_open_fails_before_any_pty() {
        let mut app = test_app();
        let key = Scripted::key(&Side::Native, "t1");
        let workspace = PathBuf::from("this/path/does/not/exist");
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers
            .scripted_mut()
            .enable()
            .answer_attach(Ok(Launch { program: "scripted".into(), argv: Vec::new() }));
        let unlisted = PaneTarget::unlisted(&key, "w1:p1");
        let switch = WorkspaceSwitch { to: Some(workspace.clone()), from: None };
        app.attach_pane(
            &Context::default(),
            key,
            unlisted,
            &switch,
            Some(reply_tx),
            AttachFocus::Take,
        );

        app.poll_pane_attaches(&Context::default());

        let expected = format!(
            "failed to attach scripted agent: worktree is no longer checked out: {}",
            workspace.display()
        );
        assert_eq!(reply_rx.try_recv().unwrap(), Err(expected));
    }

    /// The direct-attach branch of `attach_pane` hits the same
    /// synchronous refusal as the shared-view path above, without ever
    /// reaching `poll_pane_attaches`, and owes its waiter the same reason: a
    /// client has no window to read the dialog in.
    #[test]
    fn attaching_directly_answers_a_synchronous_open_failure() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable().attach_directly(true);
        let key = Scripted::key(&Side::Wsl("distro".into()), "t1");
        let workspace = PathBuf::from("this/path/does/not/exist");
        let expected = format!(
            "failed to attach scripted agent: worktree is no longer checked out: {}",
            workspace.display()
        );
        let (reply_tx, reply_rx) = mpsc::channel();

        let unlisted = PaneTarget::unlisted(&key, "w1:p1");
        let switch = WorkspaceSwitch { to: Some(workspace), from: None };
        let opened = app.attach_pane(
            &Context::default(),
            key,
            unlisted,
            &switch,
            Some(reply_tx),
            AttachFocus::Take,
        );

        assert!(!opened);
        assert_eq!(reply_rx.try_recv().unwrap(), Err(expected));
    }

    /// A pane a session already holds is not a second session's to open, or
    /// a batch attach would double every row it ran on.
    #[test]
    fn attaching_every_pane_skips_the_ones_already_attached() {
        let mut app = lifecycle_app();
        let side = Side::Native;
        adopt_panes(&mut app, &side, vec![
            Scripted::pane("term-held").with_agent("claude", PaneStatus::Working).in_dir("/repo"),
            Scripted::pane("term-loose").with_agent("claude", PaneStatus::Working).in_dir("/repo"),
        ]);
        let held = bind_pane_fixture(&mut app, &side, "term-held");

        app.attach_every_multiplexer_pane(&Context::default());

        let queued: Vec<&str> = app
            .multiplexers
            .scripted()
            .pending_attach()
            .iter()
            .map(|pending| pending.key.terminal_id.as_str())
            .collect();
        assert_eq!(queued, ["term-loose"]);
        let key = Scripted::key(&side, "term-held");
        assert_eq!(app.pane_session(&key), Some(held));
        assert_eq!(app.sessions.len(), 1, "the held pane opened no second session");
    }

    /// The batch was asked for the whole set rather than for one pane, so it
    /// files each session under its own pane's workspace without carrying the
    /// user along.  Each attach names its own workspace as the one to
    /// restore, since a refusal handing back the workspace the batch started
    /// in would move a user who has navigated since.
    #[test]
    fn attaching_every_pane_leaves_the_user_where_the_batch_was_asked_from() {
        let mut app = lifecycle_app();
        adopt_panes(&mut app, &Side::Native, vec![
            Scripted::pane("term-loose").with_agent("claude", PaneStatus::Working).in_dir("/repo"),
        ]);
        let asked_from = app.current_workspace.clone();

        app.attach_every_multiplexer_pane(&Context::default());

        let pending = app
            .multiplexers
            .scripted()
            .pending_attach()
            .first()
            .expect("the loose pane queued an attach");
        assert_eq!(pending.request.workspace, None, "an unmatched pane files under Home");
        assert_eq!(
            pending.request.previous, pending.request.workspace,
            "a refusal restores nothing"
        );
        assert_eq!(app.current_workspace, asked_from);
    }

    /// Focusing a herdr pane clears its notification, so a batch that took
    /// focus would walk herdr across every pane and wipe the done and
    /// attention state of each agent it attached.
    #[test]
    fn attaching_every_pane_leaves_the_multiplexer_focus_alone() {
        let mut app = lifecycle_app();
        adopt_panes(&mut app, &Side::Native, vec![
            Scripted::pane("term-one").with_agent("claude", PaneStatus::Done).in_dir("/repo"),
            Scripted::pane("term-two").with_agent("claude", PaneStatus::Blocked).in_dir("/repo"),
        ]);

        app.attach_every_multiplexer_pane(&Context::default());

        let queued: Vec<(&str, AttachFocus)> = app
            .multiplexers
            .scripted()
            .pending_attach()
            .iter()
            .map(|pending| (pending.key.terminal_id.as_str(), pending.request.focus))
            .collect();
        assert_eq!(queued, [("term-one", AttachFocus::Leave), ("term-two", AttachFocus::Leave)]);
    }

    /// An empty listing is not a failure: a machine with no multiplexer
    /// running must not put a dialog in front of the user for pressing a key.
    #[test]
    fn attaching_every_pane_with_no_panes_detected_reports_nothing() {
        let mut app = test_app();
        app.multiplexers.scripted_mut().enable();
        assert!(app.multiplexers.any_enabled(), "else the gate explains the silence");

        app.attach_every_multiplexer_pane(&Context::default());

        assert!(app.modals.error_dialog.is_none());
        assert!(app.multiplexers.scripted().pending_attach().is_empty());
        assert_eq!(app.sessions.len(), 1);
    }

    /// Closing walks the same list it mutates, so the ids have to be taken
    /// before the first close or the walk steps over its own removals.
    #[test]
    fn detaching_every_pane_ends_every_multiplexer_session_and_no_other() {
        let mut app = test_app();
        app.config.ui.confirm_session_detach = false;
        app.multiplexers.scripted_mut().enable();
        let shell = app.sessions.first().expect("a session").id;
        let side = Side::Native;
        bind_pane_fixture(&mut app, &side, "term-one");
        bind_pane_fixture(&mut app, &side, "term-two");

        app.detach_every_multiplexer_pane(&Context::default());

        assert_eq!(app.sessions.iter().map(|session| session.id).collect::<Vec<_>>(), [shell]);
    }

    /// One question for the batch: asking per session would put the same
    /// dialog in front of the user once per row, and nothing ends until it
    /// is answered.
    #[test]
    fn detaching_every_pane_asks_once_for_the_whole_batch() {
        let mut app = test_app();
        assert!(app.config.ui.confirm_session_detach, "the default is to ask");
        app.multiplexers.scripted_mut().enable();
        let side = Side::Native;
        bind_pane_fixture(&mut app, &side, "term-one");
        bind_pane_fixture(&mut app, &side, "term-two");

        app.detach_every_multiplexer_pane(&Context::default());

        assert_eq!(app.modals.pending_detach_all.as_deref().map(<[SessionId]>::len), Some(2));
        assert_eq!(app.sessions.len(), 3);
    }

    /// A disabled integration is why the whole-set actions found nothing, and
    /// silence there reads as a broken key rather than as a config the user
    /// can change.
    #[test]
    fn dispatching_the_whole_set_actions_while_disabled_names_the_integration() {
        let mut app = test_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = false;
        bind_herdr_fixture(&mut app, Side::Native, "term-one");

        app.dispatch_action(
            &Context::default(),
            BindingAction::Named(NamedAction::AttachAllMultiplexerPanes(
                action::AttachAllMultiplexerPanes,
            )),
            ActionOrigin::Keyboard,
        );
        assert_eq!(app.modals.error_dialog.as_deref(), Some(app.multiplexers.disabled_reason()));
        assert!(app.multiplexers.herdr_for_test().pending_attach_for_test().is_empty());

        app.modals.error_dialog = None;
        app.dispatch_action(
            &Context::default(),
            BindingAction::Named(NamedAction::DetachAllMultiplexerPanes(
                action::DetachAllMultiplexerPanes,
            )),
            ActionOrigin::Keyboard,
        );
        assert_eq!(app.modals.error_dialog.as_deref(), Some(app.multiplexers.disabled_reason()));
        assert!(app.modals.pending_detach_all.is_none());
        assert_eq!(app.sessions.len(), 2, "a refused detach ends nothing");
    }

    /// herdr reporting an agent in a pane says nothing about what the client
    /// already attached to that pane draws, so the focus a row is owed cannot
    /// be recomputed from the listing: a shared view whose pane picked up an
    /// agent would stop asking, and switching to its row would leave the user
    /// on whatever pane herdr happened to be showing.
    #[test]
    fn a_row_whose_pane_reports_an_agent_still_asks_herdr_for_focus() {
        let mut app = herdr_lifecycle_app();
        // The mode under which a pane with an agent gets a client of its
        // own, so the listing alone would say this row owes herdr no focus.
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().attach = AttachMode::Agent;
        let side = Side::Wsl("ubuntu".into());
        let id = bind_herdr_fixture(&mut app, side, "term-agent");
        app.activate_session_by_id(id);

        // No listing carries this pane, which is the reading that defaults it
        // to holding an agent, and which also keeps the focus call itself
        // from reaching herdr.
        app.sync_pane_views(&Context::default());

        assert_eq!(app.multiplexers.herdr_mut_for_test().view_mut_for_test().visible, Some(id));
        assert!(app.multiplexers.herdr_mut_for_test().view_focus_mut_for_test().is_none());
    }

    /// A session's client was settled when it attached, so the tooltip that
    /// calls it a shared view reads that record.  The listing says only what
    /// opening the pane now would give, and a created pane reads as an
    /// agent's until the multiplexer lists it.
    #[test]
    fn a_shared_view_session_says_so_before_the_multiplexer_lists_its_pane() {
        let mut app = lifecycle_app();
        app.multiplexers.scripted_mut().attach_directly(true);
        bind_pane_fixture(&mut app, &Side::Wsl("ubuntu".into()), "term-new");

        let managed = app.session_managed(&app.sessions[0]).expect("a bound session is managed");

        assert!(managed.shared_view, "the tooltip hides the shared view");
    }

    /// `park_attach_reply` builds the `Ok` reply itself when nothing is
    /// opening for the id; `in_flight.rs` proves `watch` hands the
    /// channel back in that case, but nothing there asserts what this
    /// method does with it, so a dropped `Ok` wrap or a wrong id would go
    /// uncaught.
    #[test]
    fn park_attach_reply_answers_at_once_when_nothing_is_opening_for_the_id() {
        let mut app = test_app();
        let id = app.sessions.first().expect("a session").id;
        let (reply_tx, reply_rx) = mpsc::channel();

        app.park_attach_reply(id, Some(reply_tx));

        assert_eq!(reply_rx.try_recv().unwrap(), Ok(json!({ "session_id": id })));
    }

    /// Closing a session must not silently drop a still-queued attach for its
    /// own pane: without the drain, its waiters would wait out their own
    /// timeout instead of learning the session went away.
    #[test]
    fn closing_a_session_answers_a_still_queued_attach_for_its_own_pane() {
        let mut app = test_app();
        let side = Side::Native;
        let id = bind_herdr_fixture(&mut app, side.clone(), "term-queued");
        let key = herdr_pane_key(side, "term-queued");
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers.herdr_mut_for_test().pending_attach_mut_for_test().push(PendingAttach {
            job: None,
            target: PaneTarget::unlisted(&key, "w1:p1"),
            key,
            request: AttachRequest {
                workspace: None,
                previous: None,
                waiters: vec![reply_tx],
                focus: AttachFocus::Take,
            },
        });

        app.close_session(&Context::default(), id);

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("the session behind this pane was closed before the attach finished".to_string())
        );
        assert!(app.multiplexers.herdr_for_test().pending_attach_for_test().is_empty());
    }

    /// Deleting a worktree closes its sessions the way a close does, so an
    /// attach queued on one of them is answered rather than dropped.
    #[test]
    fn deleting_a_worktree_answers_a_queued_attach_for_a_session_in_it() {
        let mut app = test_app();
        let worktree = PathBuf::from("doomed-worktree");
        let side = Side::Native;
        let id = bind_herdr_fixture(&mut app, side.clone(), "term-doomed");
        app.sessions.iter_mut().find(|s| s.id == id).unwrap().working_directory =
            Some(worktree.clone());
        let key = herdr_pane_key(side, "term-doomed");
        let (reply_tx, reply_rx) = mpsc::channel();
        app.multiplexers.herdr_mut_for_test().pending_attach_mut_for_test().push(PendingAttach {
            job: None,
            target: PaneTarget::unlisted(&key, "w1:p1"),
            key,
            request: AttachRequest {
                workspace: Some(worktree.clone()),
                previous: None,
                waiters: vec![reply_tx],
                focus: AttachFocus::Take,
            },
        });

        app.close_worktree_sessions(&Context::default(), &worktree);

        assert!(app.sessions.iter().all(|s| s.id != id));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            Err("the session behind this pane was closed before the attach finished".to_string())
        );
        assert!(app.multiplexers.herdr_for_test().pending_attach_for_test().is_empty());
    }

    /// A pending shell in `workspace`, pushed without opening a PTY.
    fn push_shell(app: &mut AlacritreeApp, workspace: WorkspaceKey) -> SessionId {
        let (session, _) = Session::pending_shell(
            Context::default(),
            &app.config,
            workspace,
            TermSize { columns: 80, screen_lines: 24 },
            (8.0, 16.0),
            None,
            None,
        );
        let id = session.id;
        app.sessions.push(session);
        id
    }

    /// Under `respawn`, deleting the on-screen worktree still goes home: a
    /// shell respawned into it would hold the directory being removed.
    #[test]
    fn deleting_the_on_screen_worktree_goes_home_without_respawning_into_it() {
        let mut app = test_app();
        assert_eq!(app.config.ui.last_session_close, LastSessionClose::Respawn);
        let home = app.sessions[0].id;
        app.sessions.set_active(None, home);
        let worktree = Some(PathBuf::from("doomed-worktree"));
        let doomed = push_shell(&mut app, worktree.clone());
        app.sessions.set_active(worktree.clone(), doomed);
        app.current_workspace = worktree.clone();

        app.close_worktree_sessions(&Context::default(), worktree.as_deref().unwrap());

        assert_eq!(app.current_workspace, None);
        assert!(app.sessions.iter().all(|s| s.working_directory != worktree));
        assert!(!app.sessions.has_active(&worktree));
        assert_eq!(app.sessions.active(&None), Some(home));
    }

    #[test]
    fn closing_the_last_session_of_the_on_screen_workspace_navigates_home() {
        let mut app = test_app();
        app.config.ui.last_session_close = LastSessionClose::Navigate;
        let home = app.sessions[0].id;
        app.sessions.set_active(None, home);
        let worktree = Some(PathBuf::from("wt"));
        let last = push_shell(&mut app, worktree.clone());
        app.sessions.set_active(worktree.clone(), last);
        app.current_workspace = worktree.clone();

        app.close_session(&Context::default(), last);

        assert_eq!(app.current_workspace, None);
        assert!(!app.sessions.has_active(&worktree));
        assert_eq!(app.sessions.active(&None), Some(home));
    }

    /// A move re-points both workspaces' active entries, so a close right
    /// after it still leaves each one naming a live session.
    #[test]
    fn a_move_then_a_close_leaves_both_workspaces_with_a_live_active_session() {
        let mut app = test_app();
        app.config.ui.last_session_close = LastSessionClose::Navigate;
        let moved = app.sessions[0].id;
        let stays = push_shell(&mut app, None);
        app.sessions.set_active(None, moved);
        app.current_workspace = None;
        let worktree = Some(PathBuf::from("wt"));

        app.move_session_to_key(moved, worktree.clone()).unwrap();

        assert_eq!(app.current_workspace, worktree, "the view follows a watched session");
        assert_eq!(app.sessions.active(&worktree), Some(moved));
        assert_eq!(app.sessions.active(&None), Some(stays));

        app.close_session(&Context::default(), moved);

        assert_eq!(app.current_workspace, None);
        assert!(!app.sessions.has_active(&worktree));
        assert_eq!(app.sessions.active(&None), Some(stays));
    }

    #[test]
    fn herdr_inventory_keeps_the_shell_after_its_agent_exits_with_panes_hidden() {
        let mut app = herdr_lifecycle_app();
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().show_panes = false;
        let side = Side::Native;
        adopt_herdr_fixture(
            &mut app,
            side.clone(),
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","pane_id":"w1:p1","agent":"claude","agent_status":"working"}
        ]}}"#,
            Instant::now(),
        );
        let shell = bind_herdr_fixture(&mut app, side.clone(), "term-shell");
        adopt_herdr_fixture(
            &mut app,
            side,
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","pane_id":"w1:p1"},
            {"terminal_id":"unattached-shell","pane_id":"w1:p2"},
            {"terminal_id":"unattached-agent","pane_id":"w1:p3","agent":"codex","agent_status":"idle"}
        ]}}"#,
            Instant::now(),
        );

        app.reconcile_pane_sessions(&Context::default());

        assert_eq!(app.sessions[0].id, shell);
        assert_eq!(
            app.pane_listing()
                .iter()
                .map(|listed| listed.pane.terminal_id.as_str())
                .collect::<Vec<_>>(),
            ["unattached-agent"]
        );
        assert!(
            app.multiplexers.herdr_for_test().caches_for_test()[0]
                .inventory()
                .unwrap()
                .terminal_ids
                .contains("term-shell")
        );
    }

    #[test]
    fn herdr_inventory_rejects_malformed_or_agent_only_replies_without_closing_rows() {
        for json in [
            "not json",
            "[]",
            "{}",
            r#"{"result":{}}"#,
            r#"{"result":{"panes":{}}}"#,
            r#"{"result":{"agents":[]}}"#,
            r#"{"error":{"code":"server_not_running"}}"#,
            r#"{"error":{},"result":{"panes":[]}}"#,
            r#"{"result":{"panes":[{}]}}"#,
            r#"{"result":{"panes":[{"terminal_id":null}]}}"#,
            r#"{"result":{"panes":[{"terminal_id":42}]}}"#,
            r#"{"result":{"panes":[{"terminal_id":" "}]}}"#,
        ] {
            let mut app = herdr_lifecycle_app();
            let side = Side::Native;
            let id = bind_herdr_fixture(&mut app, side.clone(), "term-gone");
            adopt_herdr_fixture(
                &mut app,
                side.clone(),
                r#"{"result":{"panes":[]}}"#,
                Instant::now(),
            );
            adopt_herdr_fixture(&mut app, side, json, Instant::now());

            app.reconcile_pane_sessions(&Context::default());

            assert_eq!(app.sessions[0].id, id, "{json}");
            assert!(
                app.multiplexers.herdr_for_test().caches_for_test()[0].inventory().is_none(),
                "{json}"
            );
        }
    }

    #[test]
    fn herdr_inventory_poll_failure_invalidates_successful_deletion_evidence() {
        for error in [
            herdr::PollError::Absent("spawn_failed"),
            herdr::PollError::Absent("herdr_unavailable"),
            herdr::PollError::Server("server_not_running".into()),
        ] {
            let mut app = herdr_lifecycle_app();
            let side = Side::Native;
            let id = bind_herdr_fixture(&mut app, side.clone(), "term-gone");
            adopt_herdr_fixture(&mut app, side, r#"{"result":{"panes":[]}}"#, Instant::now());
            app.multiplexers.herdr_mut_for_test().caches_mut_for_test()[0]
                .complete_listing_for_test(
                    Err(error),
                    herdr::Listing::Panes,
                    herdr::Listing::Panes,
                    Instant::now(),
                );

            app.reconcile_pane_sessions(&Context::default());

            assert_eq!(app.sessions[0].id, id);
            assert!(app.multiplexers.herdr_for_test().caches_for_test()[0].inventory().is_none());
        }
    }

    #[test]
    fn herdr_inventory_cannot_close_new_or_rebound_sessions_from_an_older_request() {
        let mut app = herdr_lifecycle_app();
        let side = Side::Native;
        let old = Instant::now() - Duration::from_secs(1);
        let id = bind_herdr_fixture(&mut app, side.clone(), "term-gone");
        adopt_herdr_fixture(&mut app, side.clone(), r#"{"result":{"panes":[]}}"#, old);
        app.reconcile_pane_sessions(&Context::default());
        assert_eq!(app.sessions[0].id, id);

        let before_rebind = Instant::now();
        let key = app.sessions[0].pane_key.clone().unwrap();
        app.sessions[0].bind_pane(key, true);
        adopt_herdr_fixture(&mut app, side, r#"{"result":{"panes":[]}}"#, before_rebind);
        app.reconcile_pane_sessions(&Context::default());
        assert_eq!(app.sessions[0].id, id);
    }

    #[test]
    fn herdr_inventory_requires_binding_provenance_and_enabled_integration() {
        let mut app = herdr_lifecycle_app();
        let id = bind_herdr_fixture(&mut app, Side::Native, "term-gone");
        adopt_herdr_fixture(&mut app, Side::Native, r#"{"result":{"panes":[]}}"#, Instant::now());
        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = false;
        app.reconcile_pane_sessions(&Context::default());
        assert_eq!(app.sessions[0].id, id);

        app.multiplexers.herdr_mut_for_test().config_mut_for_test().enabled = true;
        app.sessions[0].pane_bound_at = None;
        app.reconcile_pane_sessions(&Context::default());
        assert_eq!(app.sessions[0].id, id);
    }

    #[test]
    fn herdr_inventory_terminal_identity_is_scoped_to_the_endpoint() {
        let mut app = herdr_lifecycle_app();
        let native = bind_herdr_fixture(&mut app, Side::Native, "same-id");
        let wsl = Side::Wsl("ubuntu".into());
        let linux = bind_herdr_fixture(&mut app, wsl.clone(), "same-id");
        let absent = bind_herdr_fixture(&mut app, Side::Wsl("debian".into()), "same-id");
        adopt_herdr_fixture(&mut app, Side::Native, r#"{"result":{"panes":[]}}"#, Instant::now());
        adopt_herdr_fixture(
            &mut app,
            wsl,
            r#"{"result":{"panes":[{"terminal_id":"same-id"}]}}"#,
            Instant::now(),
        );

        app.reconcile_pane_sessions(&Context::default());

        assert!(!app.sessions.iter().any(|session| session.id == native));
        assert_eq!(app.sessions.iter().map(|session| session.id).collect::<Vec<_>>(), [
            linux, absent
        ]);
    }

    #[test]
    fn herdr_inventory_ignores_invalid_optional_display_metadata() {
        let mut app = herdr_lifecycle_app();
        let id = bind_herdr_fixture(&mut app, Side::Native, "term-shell");
        adopt_herdr_fixture(
            &mut app,
            Side::Native,
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","agent":"claude","cwd":42,"terminal_title_stripped":false}
        ]}}"#,
            Instant::now(),
        );

        app.reconcile_pane_sessions(&Context::default());

        assert_eq!(app.sessions[0].id, id);
        assert!(
            app.multiplexers.herdr_for_test().caches_for_test()[0]
                .inventory()
                .unwrap()
                .terminal_ids
                .contains("term-shell")
        );
    }

    #[test]
    fn herdr_focus_completion_for_a_removed_session_is_ignored() {
        let mut app = herdr_lifecycle_app();
        app.multiplexers.herdr_mut_for_test().view_mut_for_test().attached(
            999,
            None,
            Instant::now(),
        );
        *app.multiplexers.herdr_mut_for_test().view_focus_mut_for_test() =
            Some(herdr::HerdrViewFocus {
                session: 999,
                key: herdr_pane_key(Side::Native, "t1"),
                job: jobs::Job::ready(Ok(())),
            });

        app.sync_pane_views(&Context::default());

        assert!(app.multiplexers.herdr_mut_for_test().view_focus_mut_for_test().is_none());
        assert!(app.multiplexers.herdr_mut_for_test().view_mut_for_test().visible.is_none());
        assert!(app.multiplexers.herdr_mut_for_test().view_mut_for_test().focused.is_none());
    }

    fn ws(p: &str) -> WorkspaceKey {
        Some(PathBuf::from(p))
    }

    fn delete_request(project_idx: usize, path: &str, dirty: Option<Dirty>) -> DeleteRequest {
        DeleteRequest {
            project_idx,
            worktree_path: PathBuf::from(path),
            worktree_name: path.to_string(),
            branch: None,
            dirty,
            dirty_job: None,
            prunable: false,
            delete_branch: true,
            force: false,
        }
    }

    /// One frame of the dialogs, fed `events`.  The dialog under test has
    /// to be on screen for a frame before a key can reach it, the same as it
    /// would for a user.
    fn dialog_frame(app: &mut AlacritreeApp, ctx: &Context, events: Vec<egui::Event>) {
        let input = egui::RawInput { events, ..Default::default() };
        let _ = ctx.run(input, |ctx| app.paint_dialogs(ctx, true));
    }

    fn press_in_delete_dialog(app: &mut AlacritreeApp, key: egui::Key) {
        let ctx = Context::default();
        dialog_frame(app, &ctx, Vec::new());
        dialog_frame(app, &ctx, vec![key_ev(key, true)]);
    }

    #[test]
    fn an_enter_pressed_before_the_delete_dialog_appeared_deletes_nothing() {
        let mut app = test_app();
        app.projects.push(project_with("/repo", &["/repo/wt"]));
        let idx = app.projects.len() - 1;
        app.modals.pending_delete = Some(delete_request(idx, "/repo/wt", Some(Dirty::default())));

        dialog_frame(&mut app, &Context::default(), vec![key_ev(egui::Key::Enter, true)]);

        assert!(app.modals.pending_deletes.is_empty(), "the dialog answered an unseen key");
        assert!(app.modals.pending_delete.is_some());
    }

    #[test]
    fn enter_deletes_nothing_until_the_dirty_count_is_known() {
        let mut app = test_app();
        app.projects.push(project_with("/repo", &["/repo/wt"]));
        let idx = app.projects.len() - 1;
        app.modals.pending_delete = Some(delete_request(idx, "/repo/wt", None));

        press_in_delete_dialog(&mut app, egui::Key::Enter);

        assert!(app.modals.pending_deletes.is_empty(), "an unforced removal ran blind");
        assert!(app.modals.pending_delete.is_some());
    }

    #[test]
    fn enter_deletes_once_the_dirty_count_is_known() {
        let mut app = test_app();
        app.projects.push(project_with("/repo", &["/repo/wt"]));
        let idx = app.projects.len() - 1;
        app.modals.pending_delete = Some(delete_request(idx, "/repo/wt", Some(Dirty::default())));

        press_in_delete_dialog(&mut app, egui::Key::Enter);

        assert_eq!(app.modals.pending_deletes.len(), 1);
        assert!(app.modals.pending_delete.is_none());
    }

    #[test]
    fn every_delete_failure_in_a_batch_stays_readable() {
        let mut app = test_app();
        app.projects.push(project_with("/repo", &["/repo/wt1", "/repo/wt2"]));
        let idx = app.projects.len() - 1;
        for (path, reason) in [("/repo/wt1", "wt1 is locked"), ("/repo/wt2", "wt2 is busy")] {
            app.modals.pending_deletes.push(modals::DeleteTask {
                project_idx: idx,
                worktree_path: PathBuf::from(path),
                worktree_name: path.to_string(),
                branch: None,
                dirty: None,
                delete_branch: true,
                prunable: false,
                job: jobs::Job::ready(Err(wt::WorktreeError::Vcs(
                    alacritree_vcs::VcsError::Failed {
                        command: format!("git worktree remove {path}"),
                        stderr: reason.to_string(),
                    },
                ))),
            });
        }

        app.poll_pending_deletes(&Context::default());

        let shown = app.modals.error_dialog.expect("the failures are reported");
        assert!(shown.contains("wt1 is locked"), "{shown}");
        assert!(shown.contains("wt2 is busy"), "{shown}");
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
        let clean = Dirty::default();
        let message =
            dirty_warning(Some(&clean), true, false).expect("a forced confirm always warns");
        assert!(message.contains("--force"));
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
    fn search_enter_escape_and_shift_escape_dispatch_distinct_actions() {
        let binds = crate::bindings::parse_bindings(vec![]);
        let mut f = searching_filter();
        f.on_text("foo");

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &crate::shortcut::Shortcuts::new(&binds),
            egui::Key::Enter,
            egui::Modifiers::NONE,
            false,
        );
        assert!(!retain, "a matched search action consumes the key");
        assert!(matches!(steps.as_slice(), [SidebarNavStep::SearchAction(
            NamedAction::SidebarSearchConfirm(action::SidebarSearchConfirm)
        )]));
        // The filter is untouched by the drain. The action does the exit.
        assert_eq!(f.mode(), panel_filter::Mode::Search);
        assert_eq!(f.query(), "foo");

        let mut steps = Vec::new();
        drain_search_or_nav(
            &mut steps,
            &mut f,
            &crate::shortcut::Shortcuts::new(&binds),
            egui::Key::Escape,
            egui::Modifiers::NONE,
            false,
        );
        assert!(matches!(steps.as_slice(), [SidebarNavStep::SearchAction(
            NamedAction::SidebarSearchCancel(action::SidebarSearchCancel)
        )]));

        let mut steps = Vec::new();
        drain_search_or_nav(
            &mut steps,
            &mut f,
            &crate::shortcut::Shortcuts::new(&binds),
            egui::Key::Escape,
            egui::Modifiers::SHIFT,
            false,
        );
        assert!(
            matches!(steps.as_slice(), [SidebarNavStep::SearchAction(
                NamedAction::SidebarSearchCancelToTerminal(action::SidebarSearchCancelToTerminal)
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
            &crate::shortcut::Shortcuts::new(&binds),
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
            &crate::shortcut::Shortcuts::new(&binds),
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
            &crate::shortcut::Shortcuts::new(&binds),
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
            &crate::shortcut::Shortcuts::new(&binds),
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
        // activate. It falls through for the terminal/shortcuts instead.
        let binds: Vec<crate::bindings::KeyBinding> = Vec::new();
        let mut f = searching_filter();

        let mut steps = Vec::new();
        let retain = drain_search_or_nav(
            &mut steps,
            &mut f,
            &crate::shortcut::Shortcuts::new(&binds),
            egui::Key::Enter,
            egui::Modifiers::NONE,
            false,
        );
        assert!(retain, "an unbound Enter in search is retained, not consumed as nav");
        assert!(steps.is_empty());
    }

    /// A text-producing key in search mode is query input and must not also run a
    /// binding, including one bound to a search action, since text input is
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
            &crate::shortcut::Shortcuts::new(&binds),
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
            &crate::shortcut::Shortcuts::new(&binds),
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
            &crate::shortcut::Shortcuts::new(&binds),
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
            let retain = drain_search_or_nav(
                &mut steps,
                &mut f,
                &crate::shortcut::Shortcuts::new(&binds),
                key,
                egui::Modifiers::NONE,
                false,
            );
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
            &crate::shortcut::Shortcuts::new(&binds),
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
            &crate::shortcut::Shortcuts::new(&binds),
            egui::Key::S,
            egui::Modifiers::NONE,
            true,
        );

        assert!(retain);
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
    fn a_same_workspace_drop_uses_the_move_target_arithmetic() {
        // Dropping below your own row is a no-op, the same as for projects.
        assert_eq!(drop_position(true, 3, 1, 2), None);
        // Dropping onto the row below moves you past it.
        assert_eq!(drop_position(true, 3, 1, 3), Some(2));
        assert_eq!(moved(&["a", "b", "c"], 1, 3), vec!["a", "c", "b"]);
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
        sidebar_nav::WorkspaceEntry::Pane(herdr_pane_key(Side::Native, terminal_id))
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

    use crate::test_util::{listed_agent, titled_agent as titled};

    /// An agent with a working directory, for the bucketing cases.
    fn agent_in(dir: &str) -> Pane {
        Pane { cwd: Some(dir.into()), ..listed_agent(Some("claude")) }
    }

    #[test]
    fn untitled_panes_promote_their_directory() {
        let agent = agent_in("/home/dev/Git/devkit");
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            None,
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!(
            (content.primary, content.subtitle, content.secondary),
            ("/h/d/G/devkit".into(), "◆".into(), "herdr · claude · idle".into(),)
        );
    }

    /// The multiplexers with the scripted one listing `panes` natively.
    fn listing_of(panes: Vec<Pane>, show_unmatched: bool) -> Multiplexers {
        let mut multiplexers = Multiplexers::new(&crate::config::IntegrationsConfig::default());
        multiplexers
            .scripted_mut()
            .enable()
            .show_unmatched(show_unmatched)
            .set_panes(&Side::Native, panes);
        multiplexers
    }

    /// A listed pane with an agent in it, for the bucketing cases.
    fn bucketing_agent() -> Pane {
        Scripted::pane("term-a").with_agent("claude", PaneStatus::Idle)
    }

    /// `show_unmatched` is the sidebar's setting, and the palette reads the same
    /// listing, so an agent it hides is hidden from both by construction.
    #[test]
    fn an_unmatched_agent_is_absent_when_show_unmatched_is_off() {
        let panes = vec![bucketing_agent()];
        assert!(listing_of(panes.clone(), false).listed(&[], &[]).is_empty());
        assert_eq!(listing_of(panes, true).listed(&[], &[]).len(), 1);
    }

    /// An agent an open session holds is not listed: its row is that session's.
    #[test]
    fn a_claimed_agent_is_absent_from_the_listing() {
        let agent = bucketing_agent();
        let claimed = [Scripted::key(&Side::Native, &agent.terminal_id)];
        assert!(listing_of(vec![agent], true).listed(&claimed, &[]).is_empty());
    }

    /// The workspace an agent is bucketed under is the longest matching worktree
    /// path, which is what the palette carries in its attach payload.
    #[test]
    fn a_matched_agent_carries_its_workspace() {
        let dir = if cfg!(windows) { r"C:\p\wt" } else { "/p/wt" };
        let multiplexers = listing_of(vec![bucketing_agent().in_dir(dir)], true);
        let workspaces = vec![PathBuf::from(dir)];
        let listed = multiplexers.listed(&[], &workspaces);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].workspace, Some(PathBuf::from(dir)));
    }

    /// The native side would name itself the same word on every row of a machine
    /// that has only it, so only a WSL endpoint is worth spelling out.
    #[test]
    fn only_a_wsl_side_is_labelled() {
        assert_eq!(Side::Native.label(), None);
        assert_eq!(Side::Wsl("Ubuntu".into()).label(), Some("wsl:Ubuntu".into()));
    }

    #[test]
    fn a_row_is_named_by_the_reported_kind() {
        let row = pane_row(&listed_agent(Some("claude")), Side::Native, false);
        assert_eq!(row.name, RowName::plain("claude".into()));
    }

    #[test]
    fn a_row_name_falls_back_to_the_terminal_id_tail() {
        let row = pane_row(&listed_agent(None), Side::Native, false);
        assert_eq!(row.name, RowName::plain("300361".into()));
    }

    /// Asking for the session is honoured on a side that could have attached
    /// directly: the capability says what is possible, the config what to do.
    #[test]
    fn a_row_shares_the_view_when_the_multiplexer_says_so() {
        let row = pane_row(&listed_agent(None), Side::Wsl("d".into()), true);
        assert!(row.managed.shared_view);
    }

    /// The sidebar, the palette and the filter all call `pane_display_name`
    /// rather than resolving a title themselves, so pinning its output here
    /// pins what all three show.
    #[test]
    fn a_display_name_prefers_the_title() {
        let agent = titled(Some("claude"), Some("primary"));
        assert_eq!(pane_display_name(&agent), RowName {
            text: "primary".into(),
            context: Some("claude".into())
        });
    }

    #[test]
    fn a_display_name_falls_back_to_the_kind_without_a_title() {
        assert_eq!(
            pane_display_name(&listed_agent(Some("claude"))),
            RowName::plain("claude".into())
        );
    }

    #[test]
    fn a_display_name_falls_back_to_the_terminal_id_tail_without_a_kind() {
        assert_eq!(pane_display_name(&listed_agent(None)), RowName::plain("300361".into()));
    }

    #[test]
    fn configured_workspace_labels_and_multiplexer_icons_reach_palette_items() {
        let workspace = Some(PathBuf::from("/repo/feature"));
        let mut app = lifecycle_app();
        let mut project = project_with("/repo", &["/repo/feature"]);
        project.label = Some("renamed".into());
        project.checkouts[1].name = "feature".into();
        app.projects.push(project);
        app.multiplexers.scripted_mut().set_icon(glyph_icon("✦"));
        let side = Side::Native;
        adopt_panes(&mut app, &side, vec![
            Scripted::pane("configured-term")
                .with_agent("claude", PaneStatus::Idle)
                .with_title("claude"),
        ]);
        let id = bind_pane_fixture(&mut app, &side, "configured-term");
        app.sessions[0].working_directory = workspace;
        let items = app.palette_items();
        let item =
            items.iter().find(|item| item.action == PaletteAction::ActivateSession(id)).unwrap();
        assert_eq!(item.section, command_palette::PaletteSection::OpenSessions);
        assert_eq!(
            (item.primary.as_str(), item.subtitle.as_deref()),
            ("claude", Some("✦ renamed / feature"))
        );

        app.multiplexers.scripted_mut().set_icon(glyph_icon("  "));
        let items = app.palette_items();
        let item =
            items.iter().find(|item| item.action == PaletteAction::ActivateSession(id)).unwrap();
        assert_eq!(
            item.subtitle.as_deref(),
            Some(
                format!("{} renamed / feature", crate::config::DEFAULT_SESSION_ICON.as_str())
                    .as_str()
            )
        );
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

    fn row_of(kind: Option<&str>, title: Option<&str>) -> PaneRowData {
        pane_row(&titled(kind, title), Side::Wsl("d".into()), false)
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
        let agent = Pane {
            status: Some(PaneStatus::Working),
            ..titled(Some("claude"), Some("Claude Code"))
        };
        let mut row = pane_row(&agent, Side::Wsl("d".into()), false);
        assert_eq!(managed_tooltip(&row.managed), r#"working, scripted, `claude` "Claude Code"."#);

        row.managed.detach = Some("Ctrl+B q".to_string());
        assert_eq!(
            managed_tooltip(&row.managed),
            r#"working, scripted, `claude` "Claude Code". (detach with `Ctrl+B q`)"#
        );

        row.managed.shared_view = true;
        assert_eq!(
            managed_tooltip(&row.managed),
            r#"working, scripted, shared view, `claude` "Claude Code". (detach with `Ctrl+B q`)"#
        );
    }

    /// Losing a view costs a click to get back and losing a shell costs the
    /// shell, so the two prompts answer to separate switches. The busy
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

    /// A herdr pane has no other surface to appear on, so the threshold can
    /// never hide one, and a lone shell session beside one is listed rather
    /// than folded into the workspace row, which would leave a hole in a list
    /// its neighbour is already in.
    #[test]
    fn a_pane_row_is_listed_whatever_the_threshold_says() {
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

    use crate::projects::Project;

    /// A project whose main checkout is `root`, plus secondary worktrees.
    fn project_with(root: &str, extra: &[&str]) -> Project {
        let wt = |path: &str, is_main: bool| Checkout {
            name: path.to_string(),
            path: PathBuf::from(path),
            head: alacritree_vcs::Head::default(),
            is_main,
            gone: false,
            upstream: None,
        };
        Project {
            root: PathBuf::from(root),
            name: "p".to_string(),
            label: None,
            vcs: None,
            trunk: None,
            checkouts: std::iter::once(wt(root, true))
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

    #[test]
    fn sessions_filter_counts_a_listed_agent_only_when_the_flag_is_on() {
        let wt = ws("/a/wt1");
        let listed =
            sidebar_nav::ListedRows::from([(wt.clone(), vec![sidebar_nav::WorkspaceEntry::Pane(
                Scripted::key(&Side::Native, "term_a"),
            )])]);
        assert!(!sessions_filter_passes(&[], &listed, &wt, false));
        assert!(sessions_filter_passes(&[], &listed, &wt, true));
    }

    #[test]
    fn sessions_filter_fails_a_workspace_with_neither_session_nor_agent() {
        let wt = ws("/a/wt1");
        let listed = sidebar_nav::ListedRows::new();
        assert!(!sessions_filter_passes(&[], &listed, &wt, false));
        assert!(!sessions_filter_passes(&[], &listed, &wt, true));
    }

    #[test]
    fn sessions_filter_passes_a_folded_lone_shell_either_way() {
        // Below the row threshold `workspace_entries` folds the lone shell out
        // of `listed`, but the session itself is still live.
        let wt = ws("/a/wt1");
        let listed = sidebar_nav::ListedRows::new();
        let session_workspaces = [wt.clone()];
        assert!(sessions_filter_passes(&session_workspaces, &listed, &wt, false));
        assert!(sessions_filter_passes(&session_workspaces, &listed, &wt, true));
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
    fn toggle_expanded_resolves_child_rows_to_their_project_root() {
        let projects = vec![project_with("/repo", &["/repo/wt"])];
        let root = PathBuf::from("/repo");
        let none = |_id: SessionId| -> Option<WorkspaceKey> { None };

        // The project header resolves to itself.
        assert_eq!(
            row_project_root(&projects, none, &SidebarRow::Project(root.clone())),
            Some(root.clone())
        );
        // A worktree child resolves to the owning project root. This is the case the
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
        config.ui.sidebar_attention =
            Some(alacritty_terminal::vte::ansi::Rgb { r: 0xff, g: 0xb8, b: 0x6c });
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
    /// included. Tooltips live in their own layer, so the only way to see one
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

    /// The x-coordinate of every painted glyph, keyed by its text, for
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
    /// find where the glyph landed, then hover exactly that. This only holds
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
                    // labels out of the interactive set. The harness has to
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
    /// frame is the tooltip, true whether or not the row had room for it,
    /// which a plain "the full text appeared" check cannot tell apart.
    fn tooltip_shown(frames: &[Vec<(String, bool)>], name: &str) -> bool {
        frames.iter().any(|f| f.iter().filter(|(t, _)| t == name).count() >= 2)
    }

    /// Whether the row had to ellipsize `name`, the precondition every
    /// tooltip assertion below rests on.
    fn row_elided(frames: &[Vec<(String, bool)>], name: &str) -> bool {
        frames.iter().flatten().any(|(t, elided)| t == name && *elided)
    }

    /// A sidebar row too narrow for its name elides it, and egui offers the
    /// full text as a tooltip, but only to a widget the hit test marks
    /// hovered. The worktree row senses its click on the frame *around* the
    /// name, which takes that mark away from the label. Resting the pointer
    /// on such a row must still surface the whole name.
    #[test]
    fn hovering_an_elided_worktree_row_reveals_the_full_name() {
        let theme = Theme::from_config(&Config::default());
        let icons = crate::config::Icons::default().map_colors(rgb_to_color32);
        let wt = alacritree_vcs::Checkout {
            name: "feature/a-branch-name-far-too-long-for-the-sidebar".to_owned(),
            path: PathBuf::from("/repo/wt"),
            head: alacritree_vcs::Head::default(),
            is_main: false,
            gone: false,
            upstream: None,
        };

        let texts = texts_while_hovering(140.0, |ui| {
            worktree_row(ui, &plain_worktree_row(&wt, &icons, &theme));
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
    /// `resolve_icon`. This proves the wiring, not just the resolver in isolation.
    #[test]
    fn a_styled_upstream_badge_paints_its_configured_glyph_color_and_weight() {
        let theme = Theme::from_config(&Config::default());
        let ctx = ctx_with_ui_variant_faces();
        let mut icons = crate::config::Icons::default().map_colors(rgb_to_color32);
        icons.upstream_gone = IconStyle {
            glyph: Some("✕".to_string()),
            color: Some(Color32::RED),
            bold: true,
            italic: false,
            size: None,
        };
        let wt = alacritree_vcs::Checkout {
            name: "feature/x".to_owned(),
            path: PathBuf::from("/repo/wt"),
            head: alacritree_vcs::Head::default(),
            is_main: false,
            gone: false,
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
                worktree_row(ui, &plain_worktree_row(&wt, &icons, &theme));
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
        let icons = crate::config::Icons::default().map_colors(rgb_to_color32);
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
        let icons = crate::config::Icons::default().map_colors(rgb_to_color32);
        let wt = alacritree_vcs::Checkout {
            name: "wt".to_owned(),
            path: PathBuf::from("/repo/wt"),
            head: alacritree_vcs::Head::default(),
            is_main: false,
            gone: false,
            upstream: Some(UpstreamState::Level { upstream: "origin/x".into() }),
        };
        let pr = PrInfo {
            number: 7,
            base_branch: "main".into(),
            url: String::new(),
            state: PrState::Open,
        };
        let mut render = |ui: &mut egui::Ui| {
            worktree_row(ui, &WorktreeRowView {
                pr: Some(&pr),
                ..plain_worktree_row(&wt, &icons, theme)
            });
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

    /// Every icon a worktree row paints, buttons and status badges alike,
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
        let icons = PaintedIcons::new(
            &Config::default(),
            &Multiplexers::new(&Config::default().integrations),
        );
        for (icon_tooltips, want) in [(true, true), (false, false)] {
            let mut config = Config::default();
            config.ui.icon_tooltips = icon_tooltips;
            config.ui.sidebar_tooltips = SidebarTooltips::Off;
            let theme = Theme::from_config(&config);

            let row = SessionRowData {
                id: 1,
                name: RowName::plain("zsh".to_owned()),
                needs_attention: false,
                done: false,
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
                home_row(
                    ui,
                    true,
                    false,
                    false,
                    RowStatus::live(SessionActivity::Shell),
                    &icons,
                    &theme,
                );
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

    /// The letter a git row leads with is the whole report. `M`, `?`, `!` say
    /// nothing to a reader who does not already know porcelain. The row senses
    /// its own frame, so the badge needs the same recovery the sidebar icons do.
    #[test]
    fn icon_tooltips_gate_the_git_status_badge_hint() {
        for (icon_tooltips, want) in [(true, true), (false, false)] {
            let mut config = Config::default();
            config.ui.icon_tooltips = icon_tooltips;
            config.ui.sidebar_tooltips = SidebarTooltips::Off;
            let theme = Theme::from_config(&config);

            for (kind, glyph, hint) in [
                (alacritree_vcs::ChangeKind::Modified, "M", "modified"),
                (alacritree_vcs::ChangeKind::Untracked, "?", "untracked"),
                (alacritree_vcs::ChangeKind::Conflicted, "!", "conflicted"),
            ] {
                let change = alacritree_vcs::FileChange { path: "README.md".to_owned(), kind };
                let mut row = |ui: &mut egui::Ui| {
                    let _ = file_row(ui, &change, &theme, false);
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
    /// looked at. Both say so on hover, and each replaces the other in the
    /// same slot, so the ping is found where the idle glyph painted.
    #[test]
    fn icon_tooltips_gate_the_status_slot_hint() {
        const WIDTH: f32 = 220.0;
        let icons = PaintedIcons::new(
            &Config::default(),
            &Multiplexers::new(&Config::default().integrations),
        );
        let session = |attention, activity| SessionRowData {
            id: 1,
            name: RowName::plain("zsh".to_owned()),
            needs_attention: attention,
            done: false,
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
                hint_painted_over(
                    &mut agent_row,
                    DEFAULT_HOLLOW_MARK.as_str(),
                    "claude is running"
                ),
                want,
                "agent status, icon_tooltips = {icon_tooltips}"
            );

            let probe =
                frames_while_hovering_at(egui::Pos2::new(-100.0, -100.0), WIDTH, &mut agent_row);
            let slot = painted_glyph_positions(probe.last().expect("the row painted"))
                [DEFAULT_HOLLOW_MARK.as_str()];

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
                "attention mark, icon_tooltips = {icon_tooltips}"
            );
        }
    }

    /// End-to-end: the delete-worktree button in a real row paints its own
    /// configured styling, and that styling does not leak onto the sibling
    /// new-shell button. This pins the `icons.delete_worktree` binding at its
    /// call site, not just `resolve_icon` in isolation. Wiring the wrong key
    /// at that call site (e.g. `icons.close_session` where
    /// `icons.delete_worktree` belongs) would still compile and still paint
    /// a glyph, but this would fail.
    #[test]
    fn the_delete_worktree_button_paints_its_own_key_not_its_siblings() {
        let theme = Theme::from_config(&Config::default());
        let ctx = ctx_with_ui_variant_faces();
        let mut icons = crate::config::Icons::default().map_colors(rgb_to_color32);
        let distinctive = Color32::from_rgb(200, 30, 220);
        icons.delete_worktree =
            IconStyle { color: Some(distinctive), bold: true, ..Default::default() };
        let wt = alacritree_vcs::Checkout {
            name: "feature/x".to_owned(),
            path: PathBuf::from("/repo/wt"),
            head: alacritree_vcs::Head::default(),
            is_main: false,
            gone: false,
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
                worktree_row(ui, &plain_worktree_row(&wt, &icons, &theme));
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
    /// name that fits. That is what keeps a sweep down the list from losing
    /// egui's instant-reopen grace every time a short name goes by.
    #[test]
    fn sidebar_tooltips_modes_bound_the_row_tooltip() {
        let icons = crate::config::Icons::default().map_colors(rgb_to_color32);
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
            let wt = alacritree_vcs::Checkout {
                name: name.to_owned(),
                path: PathBuf::from("/repo/wt"),
                head: alacritree_vcs::Head::default(),
                is_main: false,
                gone: false,
                upstream: None,
            };

            let texts = texts_while_hovering(140.0, |ui| {
                worktree_row(ui, &WorktreeRowView {
                    display_name: name,
                    ..plain_worktree_row(&wt, &icons, &theme)
                });
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
        let icons = crate::config::Icons::default().map_colors(rgb_to_color32);
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
        let icons = crate::config::Icons::default().map_colors(rgb_to_color32);
        let wt = alacritree_vcs::Checkout {
            name: "feature/x".to_owned(),
            path: PathBuf::from("/repo/wt"),
            head: alacritree_vcs::Head::default(),
            is_main: false,
            gone: false,
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
                worktree_row(ui, &WorktreeRowView {
                    pr: Some(&pr),
                    ..plain_worktree_row(&wt, &icons, &theme)
                });
            });
        });
        output.shapes
    }

    /// `row_with_trailing` lays the trailing group out right-to-left, so call
    /// order is the reverse of what the user sees. Assert the rendering, not
    /// the call order. The two read as opposites.
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
        let icons = PaintedIcons::new(
            &Config::default(),
            &Multiplexers::new(&Config::default().integrations),
        );
        let row = SessionRowData {
            id: 1,
            name: RowName::plain("cargo test --workspace --all-features -- --nocapture".to_owned()),
            needs_attention: false,
            done: false,
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
            let change = alacritree_vcs::FileChange {
                path: path.to_owned(),
                kind: alacritree_vcs::ChangeKind::Modified,
            };
            let stat =
                alacritree_vcs::DiffStat { path: path.to_owned(), additions: 3, deletions: 1 };

            for (kind, is_diff) in [("file", false), ("diff", true)] {
                let texts = texts_while_hovering(140.0, |ui| {
                    if is_diff {
                        let _ = branch_diff_row(ui, &stat, &theme, false);
                    } else {
                        let _ = file_row(ui, &change, &theme, false);
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

    /// Herdr rows are navigable rows, so the arena has to carry them in the
    /// order the projection lists them.  Missing them parks the lockstep index
    /// on the first agent row, which marks every row below it unprojected and
    /// leaves the cursor with no node to sit on.
    #[test]
    fn pane_rows_are_projected_under_the_workspace_they_are_listed_in() {
        use crate::sidebar_focus::Parent;
        use crate::sidebar_nav::{self, SidebarRow};

        let projects = vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2"])];
        let live: Vec<(WorkspaceKey, SessionId)> = vec![(Some(PathBuf::from("/a/wt1")), 1)];
        let home_agent = SidebarRow::Pane(herdr_pane_key(Side::Native, "term_home"));
        let worktree_agent = SidebarRow::Pane(herdr_pane_key(Side::Wsl("d".into()), "term_wt"));
        let listed = sidebar_nav::ListedRows::from([
            (None, vec![sidebar_nav::WorkspaceEntry::Pane(herdr_pane_key(
                Side::Native,
                "term_home",
            ))]),
            (Some(PathBuf::from("/a/wt1")), vec![
                sidebar_nav::WorkspaceEntry::Session(1),
                sidebar_nav::WorkspaceEntry::Pane(herdr_pane_key(Side::Wsl("d".into()), "term_wt")),
            ]),
        ]);
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot = build_snapshot(&projects, &live, &listed, &rows, None, Default::default());

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

    /// Same lockstep hazard, with the doomed worktree carrying a pane row:
    /// the agent rows it owns have to be stepped over as well.
    #[test]
    fn rows_below_a_deleted_worktree_with_a_pane_row_stay_navigable() {
        use crate::sidebar_nav::{self, SidebarRow};

        let projects =
            vec![sidebar_nav::tests::project("/a", true, &["/a/wt1", "/a/wt2", "/a/wt3"])];
        let doomed = PathBuf::from("/a/wt2");
        let listed = sidebar_nav::ListedRows::from([(Some(doomed.clone()), vec![
            sidebar_nav::WorkspaceEntry::Pane(Scripted::key(&Side::Native, "term_doomed")),
        ])]);
        let rows = sidebar_nav::visible_rows(&projects, &listed);
        let snapshot = build_snapshot(
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

    #[test]
    fn the_git_filter_actions_map_to_their_identities() {
        for (action, identity) in [
            (NamedAction::ToggleModifiedFilter(action::ToggleModifiedFilter), Some('m')),
            (NamedAction::ToggleDeletedFilter(action::ToggleDeletedFilter), Some('d')),
            (NamedAction::ToggleUntrackedFilter(action::ToggleUntrackedFilter), Some('u')),
            (NamedAction::ClearGitFilters(action::ClearGitFilters), None),
            (NamedAction::ToggleSessionsFilter(action::ToggleSessionsFilter), None),
            (NamedAction::ToggleAttentionFilter(action::ToggleAttentionFilter), None),
            (NamedAction::TogglePrOpenFilter(action::TogglePrOpenFilter), None),
            (NamedAction::TogglePrDraftFilter(action::TogglePrDraftFilter), None),
            (NamedAction::TogglePrMergedFilter(action::TogglePrMergedFilter), None),
            (NamedAction::TogglePrClosedFilter(action::TogglePrClosedFilter), None),
            (NamedAction::Paste(action::Paste), None),
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

    /// The same path can be a worktree of two projects, and `PrCache` is keyed
    /// by path alone, so two pollers would burn a `gh` process per frame.
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

    struct PaintedPaletteText {
        rows: usize,
        elided: bool,
        pos: egui::Pos2,
        size: egui::Vec2,
        font_size: f32,
        color: Color32,
    }

    fn painted_palette_row(
        width: f32,
        item: &PaletteItem,
    ) -> (HashMap<String, PaintedPaletteText>, egui::Rect) {
        let ctx = egui::Context::default();
        let theme = Theme::from_config(&Config::default());
        let icons = Icons::default().map_colors(rgb_to_color32);
        let mut row_rect = None;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(900.0, 400.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 300.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let cols = PaletteColumns::new(theme.ui_scale, width);
                        row_rect = Some(
                            paint_palette_row(ui, &theme, &icons, &cols, item, None, 0, false).rect,
                        );
                    },
                );
            });
        });

        let mut text = HashMap::new();
        fn collect(shape: &egui::Shape, text: &mut HashMap<String, PaintedPaletteText>) {
            match shape {
                egui::Shape::Text(t) => {
                    let format = &t.galley.job.sections[0].format;
                    text.insert(t.galley.text().to_owned(), PaintedPaletteText {
                        rows: t.galley.rows.len(),
                        elided: t.galley.elided,
                        pos: t.pos,
                        size: t.galley.size(),
                        font_size: format.font_id.size,
                        color: format.color,
                    });
                },
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, text)),
                _ => {},
            }
        }
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut text);
        }
        (text, row_rect.expect("the palette row painted"))
    }

    fn palette_session(primary: &str, subtitle: &str, secondary: &str, hover: &str) -> PaletteItem {
        PaletteItem::session(
            1,
            primary.to_owned(),
            subtitle.to_owned(),
            secondary.to_owned(),
            hover.to_owned(),
            Some("codex"),
            None,
            Some(MultiplexerKind::Herdr),
        )
    }

    #[test]
    fn palette_session_rows_bound_title_subtitle_and_middle() {
        let title = "A deliberately long session title that wraps past two rows in the \
                     description column and must stop there instead of continuing through every \
                     remaining word in this sentence";
        let subtitle = "◆ alacritree / feature/herdr-palette";
        let middle = "codex · working on a deliberately long status message that needs more than \
                      three lines in the middle column before it is cut";
        let item = palette_session(title, subtitle, middle, "full session details");
        let (text, rect) = painted_palette_row(760.0, &item);
        let theme = Theme::from_config(&Config::default());
        let title_paint = &text[title];
        let subtitle_paint = &text[subtitle];
        let middle_paint = &text[middle];

        assert_eq!(title_paint.rows, 2);
        assert!(title_paint.elided);
        assert_eq!(title_paint.font_size, theme.font_normal);
        assert_eq!(title_paint.color, theme.text);
        assert_eq!(subtitle_paint.rows, 1);
        assert_eq!(subtitle_paint.font_size, (theme.font_normal - 1.0).max(8.0));
        assert_eq!(subtitle_paint.color, theme.text);
        assert!(subtitle_paint.pos.y >= title_paint.pos.y + title_paint.size.y - 0.5);
        assert_eq!(middle_paint.rows, 3);
        assert!(middle_paint.elided);
        assert_eq!(middle_paint.font_size, theme.font_normal);
        assert_eq!(middle_paint.color, theme.text_dim);

        let content_height = (title_paint.size.y + subtitle_paint.size.y).max(middle_paint.size.y);
        assert!((rect.height() - (content_height + 12.0)).abs() <= 1.0);
    }

    #[test]
    fn narrow_palette_session_rows_bound_long_tokens() {
        let title =
            "title-with-one-unbroken-token-that-is-much-too-wide-for-the-description-column";
        let subtitle = "/repo/worktrees/feature/one-unbroken-branch-name-that-is-too-wide";
        let middle = "one-unbroken-middle-cell-token-that-needs-to-stop-after-three-lines";
        let item = palette_session(title, subtitle, middle, "full narrow session details");
        let (text, rect) = painted_palette_row(240.0, &item);

        assert_eq!(text[title].rows, 2);
        assert!(text[title].elided);
        assert_eq!(text[subtitle].rows, 1);
        assert!(text[subtitle].elided);
        assert_eq!(text[middle].rows, 3);
        assert!(text[middle].elided);
        assert_eq!(rect.width(), 240.0);
    }

    #[test]
    fn palette_session_rows_reserve_an_empty_subtitle_line() {
        let empty = palette_session("shell", "", "shell", "empty subtitle details");
        let filled = palette_session("shell", "◆", "shell", "filled subtitle details");
        let action = PaletteItem::profile("shell".into(), "shell".into(), String::new(), "");

        let (_, empty_rect) = painted_palette_row(760.0, &empty);
        let (_, filled_rect) = painted_palette_row(760.0, &filled);
        let (_, action_rect) = painted_palette_row(760.0, &action);

        assert_eq!(empty_rect.height(), filled_rect.height());
        assert!(empty_rect.height() > action_rect.height());
    }

    #[test]
    fn palette_session_hover_owns_the_whole_row_including_the_status_mark() {
        let item = palette_session("shell", "◆ home", "shell", "persistent session details");
        let mark = (ShownState::Pinged, "competing status hint".to_owned());
        let theme = Theme::from_config(&Config::default());
        let icons = Icons::default().map_colors(rgb_to_color32);
        let texts = texts_while_hovering_at(egui::pos2(23.0, 20.0), 760.0, |ui| {
            let cols = PaletteColumns::new(theme.ui_scale, 760.0);
            paint_palette_row(ui, &theme, &icons, &cols, &item, Some(&mark), 0, false);
        });

        assert!(texts.iter().flatten().any(|(text, _)| text == "persistent session details"));
        assert!(!texts.iter().flatten().any(|(text, _)| text == "competing status hint"));
    }

    #[test]
    fn action_palette_rows_keep_elided_only_hover() {
        let theme = Theme::from_config(&Config::default());
        let icons = Icons::default().map_colors(rgb_to_color32);
        let short =
            PaletteItem::profile("Open config".into(), "Config".into(), "Ctrl+C".into(), "");
        let short_frames = texts_while_hovering(760.0, |ui| {
            let cols = PaletteColumns::new(theme.ui_scale, 760.0);
            paint_palette_row(ui, &theme, &icons, &cols, &short, None, 0, false);
        });
        assert!(!tooltip_shown(&short_frames, &short.primary));

        let long = PaletteItem::profile(
            "Open config".into(),
            "AnActionNameFarTooLongForTheComfortablePaletteActionColumnToPaintInFull".into(),
            "Ctrl+Shift+Something".into(),
            "",
        );
        let long_frames = texts_while_hovering(760.0, |ui| {
            let cols = PaletteColumns::new(theme.ui_scale, 760.0);
            paint_palette_row(ui, &theme, &icons, &cols, &long, None, 0, false);
        });
        assert!(row_elided(&long_frames, &long.secondary));
        assert!(tooltip_shown(&long_frames, &long.secondary));
    }

    const LIVE: SessionFocus = SessionFocus { scratchpad: false, exited: false };
    const EXITED: SessionFocus = SessionFocus { scratchpad: false, exited: true };
    const SCRATCHPAD: SessionFocus = SessionFocus { scratchpad: true, exited: false };

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
        let frame =
            [SessionBoost { raised: false, visible: false, pending: false }, SessionBoost {
                raised: false,
                visible: false,
                pending: true,
            }];

        assert!(!frame_holds_self_boost(frame.into_iter()));
    }

    #[test]
    fn a_grey_worktree_only_stays_in_the_workspace_ring_while_it_holds_sessions() {
        let wt = Checkout {
            name: "gone".into(),
            path: PathBuf::from("/repo-worktrees/gone"),
            head: alacritree_vcs::Head { name: Some("feature".into()), ..Default::default() },
            is_main: false,
            gone: false,
            upstream: None,
        };

        assert!(!worktree_is_switchable(&wt, Some(true), false));
        assert!(worktree_is_switchable(&wt, Some(true), true));
    }

    #[test]
    fn a_main_checkout_never_looks_prunable_from_the_row_probe() {
        let wt = Checkout {
            name: "main".into(),
            path: PathBuf::from("/plain-project"),
            head: alacritree_vcs::Head::default(),
            is_main: true,
            gone: false,
            upstream: None,
        };

        assert!(!worktree_looks_gone(&wt, Some(true)));
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
    fn a_cross_workspace_drop_takes_the_display_slot_as_the_position() {
        // The session is not in that workspace's list yet, so nothing shifts
        // down and every slot passes through, including the two the same
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
            reorder_subject(
                true,
                Some(&row),
                || None,
                |p| (p == Path::new("/b")).then_some(9),
                || Some(3)
            ),
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

    #[test]
    fn workspace_entries_keep_shell_sessions_in_spawn_order() {
        assert_eq!(workspace_entries(&[1, 3], Vec::new(), false), entries(&[1, 3]));
    }

    /// A herdr pane with no agent in it reports no state, so a plain shell in
    /// one carries no mark at all.  The palette reads `managed.status`
    /// directly while the sidebar goes through `session_status_mark`, so the
    /// two only agree while both answer "none" here.
    #[test]
    fn session_status_mark_leaves_an_agentless_pane_unmarked() {
        let managed = pane_row(&shell_pane(), Side::Native, false).managed;
        assert_eq!(managed.status, None);
        let status =
            RowStatus { managed: Some(&managed), ..RowStatus::live(SessionActivity::Shell) };
        assert!(session_status_mark(&status).is_none());
    }

    /// A pane herdr reports no agent in can still be running one alacritree's
    /// own title heuristic recognises.  The multiplexer has no status to
    /// report, so the mark falls through to alacritree's own reading.
    #[test]
    fn an_agentless_pane_falls_through_to_the_local_agent_reading() {
        let managed = pane_row(&shell_pane(), Side::Native, false).managed;
        let activity = SessionActivity::agent(Some("claude"), LiveState::Working);
        assert_eq!(pane_backed_activity(activity, None), activity);
        let status = RowStatus { managed: Some(&managed), ..RowStatus::live(activity) };
        let (mark, hint) = session_status_mark(&status).expect("the live axis still has one");
        assert_eq!(mark, ShownState::Working);
        assert_eq!(hint, agent_hint(ShownState::Working, Some("claude")));
    }

    #[test]
    fn an_attached_herdr_session_takes_herdrs_live_state() {
        let claude = SessionActivity::agent(Some("claude"), LiveState::Idle);

        // Not attached to herdr: nothing overrides the session's own reading.
        assert_eq!(pane_backed_activity(claude, None), claude);

        // herdr sees the approval dialog no title heuristic can.
        assert_eq!(
            pane_backed_activity(claude, Some(PaneStatus::Blocked)),
            SessionActivity::agent(Some("claude"), LiveState::Blocked)
        );

        // An attached pane holds an agent even when the process probe missed
        // one, so the gate closes on herdr's word alone.
        assert_eq!(
            pane_backed_activity(SessionActivity::Shell, Some(PaneStatus::Working)),
            SessionActivity::agent(None, LiveState::Working)
        );

        // `unknown` is herdr declining to say, not a claim of idleness: the
        // session keeps whatever it already knew.
        let working = SessionActivity::agent(Some("claude"), LiveState::Working);
        assert_eq!(pane_backed_activity(working, Some(PaneStatus::Unknown)), working);
    }

    /// A herdr pane running a plain shell.  herdr names no agent in it, so
    /// the only thing it can be called is the title it set itself.
    fn shell_pane() -> Pane {
        Pane { status: None, ..titled(None, Some("~/G/g/alacritree")) }
    }

    /// A shell pane has no kind to fall back to, and six characters of a
    /// terminal id name nothing a user would recognise.
    #[test]
    fn an_agentless_pane_is_named_by_its_title() {
        assert_eq!(pane_display_name(&shell_pane()), RowName::plain("~/G/g/alacritree".into()));
    }

    /// `unknown` is herdr's word for an agent it cannot classify, so a shell
    /// wearing it would claim an agent is there.
    #[test]
    fn an_agentless_pane_claims_no_status() {
        let agent = shell_pane();
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            Some("alacritree / master"),
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!(
            (content.primary, content.subtitle, content.secondary),
            ("~/G/g/alacritree".into(), "◆ alacritree / master".into(), "herdr · shell".into(),)
        );
    }

    /// The pane is still the multiplexer's, which is what the row says; the
    /// state is the part there is nothing to report.
    #[test]
    fn an_agentless_pane_paints_no_state_and_shares_the_view() {
        let row = pane_row(&shell_pane(), Side::Wsl("d".into()), true);
        assert_eq!(row.managed.status, None);
        assert!(row.managed.shared_view);
        assert_eq!(managed_tooltip(&row.managed), r#"scripted, shared view, "~/G/g/alacritree"."#);
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

    #[test]
    fn fallback_goes_home_from_home() {
        assert_eq!(close_fallback(&None, &None, &[], None), CloseFallback::Home);
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

    /// An IPC move is the inner program saying it is out of windows, so
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

    /// The terminal owning focus over a live session, which is what every
    /// scope test that does not say otherwise means.
    fn scope() -> BindingScope {
        BindingScope::default()
    }

    /// The mapping the filter chain cannot check for itself: which pane owns
    /// focus, and whether the session on screen still has a child.
    #[test]
    fn binding_scope_reads_focus_and_the_session_on_screen() {
        let terminal = |active| binding_scope(PaneFocus::Terminal, false, active);

        assert!(terminal(Some(EXITED)).exited_session_focused);
        assert!(!terminal(Some(LIVE)).exited_session_focused);
        assert!(!terminal(None).exited_session_focused);
        assert!(terminal(Some(SCRATCHPAD)).scratchpad_focused);
        assert!(!terminal(Some(LIVE)).scratchpad_focused);

        let sidebar = binding_scope(PaneFocus::ProjectsSidebar, false, Some(EXITED));
        assert!(sidebar.sidebar_focused);
        assert!(!sidebar.git_focused);
        assert!(
            !sidebar.exited_session_focused,
            "a session's chord is the terminal's, not the sidebar's"
        );

        let git = binding_scope(PaneFocus::GitSidebar, false, Some(EXITED));
        assert!(git.git_focused);
        assert!(!git.sidebar_focused);
        assert!(!git.exited_session_focused);
    }

    /// The palette owns every key while it is up, so no scope is live under it.
    #[test]
    fn an_open_palette_leaves_no_scope_active() {
        for focus in [PaneFocus::Terminal, PaneFocus::ProjectsSidebar, PaneFocus::GitSidebar] {
            let scope = binding_scope(focus, true, Some(EXITED));
            assert!(!scope.sidebar_focused, "{focus:?}");
            assert!(!scope.git_focused, "{focus:?}");
            assert!(!scope.scratchpad_focused, "{focus:?}");
            assert!(!scope.exited_session_focused, "{focus:?}");
        }
    }

    #[test]
    fn projects_filter_action_valid_when_projects_sidebar_focused() {
        let action =
            BindingAction::Named(NamedAction::ToggleSessionsFilter(action::ToggleSessionsFilter));
        assert!(valid_for_focus(&action, BindingScope { sidebar_focused: true, ..scope() }));
    }

    #[test]
    fn projects_filter_action_rejected_when_git_sidebar_focused() {
        let action =
            BindingAction::Named(NamedAction::ToggleSessionsFilter(action::ToggleSessionsFilter));
        assert!(!valid_for_focus(&action, BindingScope { git_focused: true, ..scope() }));
    }

    #[test]
    fn git_filter_action_valid_when_git_sidebar_focused() {
        let action =
            BindingAction::Named(NamedAction::ToggleModifiedFilter(action::ToggleModifiedFilter));
        assert!(valid_for_focus(&action, BindingScope { git_focused: true, ..scope() }));
    }

    #[test]
    fn git_filter_action_rejected_when_projects_sidebar_focused() {
        let action =
            BindingAction::Named(NamedAction::ToggleModifiedFilter(action::ToggleModifiedFilter));
        assert!(!valid_for_focus(&action, BindingScope { sidebar_focused: true, ..scope() }));
    }

    #[test]
    fn both_sidebar_filters_rejected_when_terminal_focused() {
        let projects_action =
            BindingAction::Named(NamedAction::ToggleSessionsFilter(action::ToggleSessionsFilter));
        let git_action =
            BindingAction::Named(NamedAction::ToggleModifiedFilter(action::ToggleModifiedFilter));
        assert!(!valid_for_focus(&projects_action, scope()));
        assert!(!valid_for_focus(&git_action, scope()));
    }

    /// `ScrollPageUp` is unscoped by pane focus, so only the scratchpad
    /// editor stealing it back (via `terminal_only`) should block it.
    #[test]
    fn terminal_only_action_yields_to_the_scratchpad_editor() {
        let action = BindingAction::Named(NamedAction::ScrollPageUp(action::ScrollPageUp));
        assert!(!valid_for_focus(&action, BindingScope { scratchpad_focused: true, ..scope() }));
        assert!(valid_for_focus(&action, scope()));
    }

    /// The one that decides whether the terminal stays usable: `Enter` is the
    /// default trigger for `CloseExitedSession`, and bindings are consumed
    /// ahead of `event_to_bytes`, so dispatching anything here would take the
    /// key away from every shell prompt in the app.
    #[test]
    fn a_live_session_keeps_its_enter() {
        let bindings = crate::bindings::parse_bindings(Vec::new());
        let shortcuts = crate::shortcut::Shortcuts::new(&bindings);
        let matched = shortcuts.matches(egui::Key::Enter, egui::Modifiers::NONE);
        assert!(
            matched.iter().any(|a| matches!(
                a,
                BindingAction::Named(NamedAction::CloseExitedSession(action::CloseExitedSession))
            )),
            "Enter must still reach the exited-session binding"
        );
        assert!(
            dispatched_actions(matched, scope()).is_empty(),
            "a live session's Enter must fall through to the PTY"
        );
    }

    /// Once the child is gone the same press closes the session instead.
    #[test]
    fn an_exited_session_dispatches_enter_to_the_close_action() {
        let bindings = crate::bindings::parse_bindings(Vec::new());
        let shortcuts = crate::shortcut::Shortcuts::new(&bindings);
        let matched = shortcuts.matches(egui::Key::Enter, egui::Modifiers::NONE);
        let scope = BindingScope { exited_session_focused: true, ..scope() };
        let dispatched = dispatched_actions(matched, scope);
        assert_eq!(dispatched.len(), 1, "{dispatched:?}");
        assert!(
            matches!(
                dispatched[0],
                BindingAction::Named(NamedAction::CloseExitedSession(action::CloseExitedSession))
            ),
            "{dispatched:?}"
        );
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

    /// A background move is silent, with no focus stealing, and only claims the
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
}
