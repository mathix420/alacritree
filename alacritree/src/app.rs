use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};

use alacritty_terminal::tty::Shell;
use eframe::CreationContext;
use egui::{Color32, Context, Frame, Margin, RichText, ScrollArea, SidePanel, Stroke};

use serde_json::{Value, json};

use crate::bindings::{BindingAction, NamedAction};
use crate::clipboard::{self, Target};
use crate::colors::rgb_to_color32;
use crate::config::{Config, LastSessionClose};
use crate::doppler;
use crate::git_nav::{self, GitSection, SectionCount};
use crate::git_status::{self, ChangeKind, DirtyCounts, FileChange, GitStatus, StatusCache};
use crate::ipc;
use crate::panel_filter::{self, PanelFilter};
use crate::paste;
use crate::pr_status::PrCache;
use crate::projects::{Project, Worktree, project_json};
use crate::session::{Session, SessionId, SessionKind, TermSize};
use crate::sidebar_nav::{self, SidebarRow};
use crate::state::{self, PersistedProject};
use crate::terminal_view;
use crate::worktree::{self as wt, CreateRequest, Progress};
use crate::wsl::{self, ShellChoice};

/// `None` is the home workspace (sessions inherit `$PWD`); `Some` is a worktree path.
pub type WorkspaceKey = Option<PathBuf>;

/// Channel from notification-worker threads back to the app.  Set once by
/// `AlacritreeApp::new`; each worker reads it to deliver the workspace the
/// user clicked on.  Static because the worker has no other handle to the
/// app and there's only ever one app instance per process.
static NOTIFY_TX: OnceLock<Mutex<Sender<WorkspaceKey>>> = OnceLock::new();

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
}

impl Theme {
    fn from_config(config: &Config) -> Self {
        let terminal_bg = rgb_to_color32(config.palette.bg);
        let sidebar_bg = config.ui.sidebar_background.unwrap_or(terminal_bg);
        let text =
            config.ui.sidebar_foreground.unwrap_or_else(|| rgb_to_color32(config.palette.fg));
        let accent =
            config.ui.sidebar_accent.unwrap_or_else(|| rgb_to_color32(config.palette.normal[4])); // ANSI blue
        let border = config.ui.sidebar_border.unwrap_or_else(|| lighten(sidebar_bg, 0.10));
        Self {
            terminal_bg,
            sidebar_bg,
            sidebar_border: border,
            row_hover_bg: lighten(sidebar_bg, 0.05),
            row_active_bg: lighten(sidebar_bg, 0.10),
            text,
            text_dim: blend_toward(text, sidebar_bg, 0.35),
            text_muted: blend_toward(text, sidebar_bg, 0.55),
            accent,
            attention: rgb_to_color32(config.palette.normal[3]), // ANSI yellow
            font_heading: config.font.ui_heading_px(),
            font_normal: config.font.ui_normal_px(),
            ui_scale: config.font.ui_normal_px() / 11.25,
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneFocus {
    Terminal,
    ProjectsSidebar,
    GitSidebar,
}

pub struct AlacritreeApp {
    show_left_sidebar: bool,
    show_right_sidebar: bool,
    focus: PaneFocus,
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
    /// Fuzzy-search query and `s`/`a` toggle state for the projects panel.
    /// Transient: never persisted, never touches the `expanded` flag.
    project_filter: PanelFilter,
    /// Fuzzy-search query and `m`/`d`/`u` change-kind toggle state for the git
    /// panel.  Transient: never persisted.
    git_filter: PanelFilter,
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
    sessions: Vec<Session>,
    current_workspace: WorkspaceKey,
    active_session: HashMap<WorkspaceKey, SessionId>,
    projects: Vec<Project>,
    git_status: HashMap<PathBuf, StatusCache>,
    pr_cache: PrCache,
    config: Config,
    theme: Theme,
    last_error: Option<String>,
    /// A modal popup carrying a failure message the user must dismiss —
    /// louder than `last_error`, used when a background action (e.g. a
    /// worktree delete) fails after its dialog already closed.
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
    pending_project_remove: Option<ProjectRemoveState>,
    /// Worktrees already given a Doppler scope pass this app run, so opening
    /// more shells there doesn't re-invoke the doppler CLI.
    doppler_synced: HashSet<PathBuf>,
    pending_session_close: Option<SessionId>,
    notify_rx: Receiver<WorkspaceKey>,
    /// Requests from IPC connection threads, drained once per frame.
    ipc_rx: Option<Receiver<ipc::AppCall>>,
    /// Held for its Drop: unlinks the socket file on shutdown.
    _ipc_socket: Option<ipc::SocketHandle>,
    /// Shared across sessions; auto-invalidated when cell size changes.
    builtin_glyphs: crate::builtin_font::BuiltinGlyphCache,
    ime: crate::ime::Ime,
    color_glyphs: crate::color_glyph::ColorGlyphCache,
    /// In-flight background re-discoveries, keyed by project root.  WSL
    /// discovery shells out to wsl.exe and must never block paint; results
    /// are adopted in `poll_project_refreshes`.
    pending_project_refresh: HashMap<PathBuf, Receiver<Project>>,
    /// Resolved absolute path of `delta` inside each WSL distro, so diff panes
    /// stop re-sourcing a login profile on every open.  Successes only: a miss
    /// is never stored, so installing delta mid-session is picked up later.
    wsl_delta_paths: HashMap<String, String>,
    /// In-flight delta discoveries, keyed by distro, mirroring
    /// `pending_project_refresh` — resolved off the UI thread, adopted in
    /// `wsl_delta_path`.
    pending_delta: HashMap<String, Receiver<Option<String>>>,
}

struct DeleteRequest {
    project_idx: usize,
    worktree_path: PathBuf,
    worktree_name: String,
    branch: Option<String>,
    dirty: DirtyCounts,
    /// The checkout dir is already gone; confirm prunes metadata instead of
    /// removing a directory.
    prunable: bool,
    /// Checkbox state for the prune dialog's "also delete branch".
    delete_branch: bool,
}

/// An in-flight background delete/prune awaiting its git result.
struct DeleteTask {
    project_idx: usize,
    /// Marks the matching sidebar row with a spinner while the removal runs.
    worktree_path: PathBuf,
    /// Distinguishes the "prune" vs "delete" wording in a failure message.
    prunable: bool,
    result_rx: Receiver<Result<(), String>>,
}

enum CreateState {
    Prompt { project_idx: usize, branch: String, error: Option<String> },
    Running { project_idx: usize, branch: String, steps: Vec<String>, rx: Receiver<Progress> },
    Done { project_idx: usize, steps: Vec<String>, result: Result<PathBuf, String> },
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

/// Drag-and-drop payload for reordering the project list.  Carries the dragged
/// project's root rather than its index so a background refresh that shifts the
/// list mid-drag can't drop onto the wrong project.
#[derive(Clone)]
struct DraggedProject(PathBuf);

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
        let theme = Theme::from_config(&config);

        let font_chain = crate::fonts::install_terminal_fonts(&cc.egui_ctx, &config.font);
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
                    wsl::Location::Windows(_) => Project::discover(root),
                    wsl::Location::Wsl { .. } => Project::placeholder(root),
                };
                project.expanded = p.expanded;
                project.shell_override = p.shell.as_deref().and_then(wsl::ShellChoice::parse);
                project.label = p.label.clone();
                project
            })
            .collect();

        let (notify_tx, notify_rx) = mpsc::channel();
        // `set` may fail only if a previous instance already initialized the
        // static (e.g. tests).  In that case the old sender points at a dead
        // app, so overwriting via `Mutex` would be ideal — but since we only
        // ever spawn one app per process, ignoring the error is fine.
        let _ = NOTIFY_TX.set(Mutex::new(notify_tx));

        let mut app = Self {
            show_left_sidebar: persisted.show_left_sidebar,
            show_right_sidebar: persisted.show_right_sidebar,
            focus: PaneFocus::Terminal,
            sidebar_cursor: None,
            reorder_mode: false,
            sidebar_auto_shown: false,
            sidebar_cursor_moved: false,
            project_filter: PanelFilter::new(&['s', 'a']),
            git_filter: PanelFilter::new(&['m', 'd', 'u']),
            git_cursor: None,
            git_cursor_moved: false,
            git_rows: Vec::new(),
            git_branch_base: None,
            git_sidebar_auto_shown: false,
            sessions: Vec::new(),
            current_workspace: None,
            active_session: HashMap::new(),
            projects,
            git_status: HashMap::new(),
            pr_cache: PrCache::new(),
            config,
            theme,
            last_error: None,
            error_dialog: None,
            quit_dialog_open: false,
            pending_delete: None,
            pending_deletes: Vec::new(),
            pending_create: None,
            pending_creates: Vec::new(),
            pending_rename: None,
            pending_project_remove: None,
            doppler_synced: HashSet::new(),
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
            pending_project_refresh: HashMap::new(),
            wsl_delta_paths: HashMap::new(),
            pending_delta: HashMap::new(),
        };

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
            app.last_error = Some(format!("failed to spawn shell: {e}"));
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

    /// Windows projects re-discover synchronously (git2, fast).  WSL
    /// projects re-discover on a worker thread: wsl.exe takes ~400 ms warm
    /// and seconds while the distro VM boots.
    fn refresh_project(&mut self, ctx: &Context, idx: usize) {
        let root = self.projects[idx].root.clone();
        if matches!(wsl::classify(&root), wsl::Location::Windows(_)) {
            self.projects[idx].refresh();
            return;
        }
        if self.pending_project_refresh.contains_key(&root) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Project::discover(worker_root));
            ctx.request_repaint();
        });
        self.pending_project_refresh.insert(root, rx);
    }

    /// Adopt completed background discoveries.  Only worktrees and the
    /// default branch are copied — `expanded`, the shell override, and the
    /// label are user state that survives refreshes (mirrors
    /// `Project::refresh`).
    fn poll_project_refreshes(&mut self) {
        let projects = &mut self.projects;
        self.pending_project_refresh.retain(|root, rx| match rx.try_recv() {
            Ok(fresh) => {
                if let Some(project) = projects.iter_mut().find(|p| p.root == *root) {
                    project.worktrees = fresh.worktrees;
                    project.default_branch = fresh.default_branch;
                }
                false
            },
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
    }

    fn spawn_session(
        &mut self,
        ctx: &Context,
        working_directory: WorkspaceKey,
    ) -> std::io::Result<SessionId> {
        // Before the PTY exists, so the shell can't race `doppler run`
        // against the scope write.
        if let Some(dir) = &working_directory {
            self.sync_doppler_scopes(dir.clone());
        }
        let shell = self.resolve_shell(&working_directory);
        self.spawn_session_with_shell(ctx, working_directory, shell)
    }

    fn spawn_session_with_shell(
        &mut self,
        ctx: &Context,
        working_directory: WorkspaceKey,
        shell: Option<Shell>,
    ) -> std::io::Result<SessionId> {
        let session = Session::spawn(
            ctx.clone(),
            &self.config,
            working_directory.clone(),
            TermSize::new(80, 24),
            (8.0, 16.0),
            shell,
        )?;
        let id = session.id;
        self.sessions.push(session);
        self.active_session.insert(working_directory, id);
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
        let linked = doppler::mirror_scopes(&main_checkout, &worktree);
        if linked > 0 {
            log::info!("linked {linked} doppler scope(s) into {}", worktree.display());
        }
    }

    /// Spawn a named profile into the current workspace, bypassing the
    /// override/auto resolution chain — the user asked for this profile
    /// explicitly.
    fn spawn_profile_session(&mut self, ctx: &Context, name: &str) {
        let Some(profile) = self.config.profile(name) else {
            log::warn!("no shell profile named `{name}`");
            self.last_error = Some(format!("no shell profile named `{name}`"));
            return;
        };
        let shell = Some(profile_shell(profile));
        let ws = self.current_workspace.clone();
        if let Err(e) = self.spawn_session_with_shell(ctx, ws, shell) {
            self.last_error = Some(format!("failed to spawn profile `{name}`: {e}"));
        }
    }

    /// Shell for a workspace; `None` means "no override" — `Session::spawn`
    /// falls through to alacritty's config-driven shell with its
    /// OS-guaranteed fallback.  The home tab (`None` workspace) has no
    /// project or location, so only the default profile can apply there.
    fn resolve_shell(&self, workspace: &WorkspaceKey) -> Option<Shell> {
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
            ShellDecision::ConfigShell => None,
            // A WSL decision only arises from a workspace path (override or
            // location), never from the home tab.
            ShellDecision::WslDistro(distro) => path.map(|p| wsl_shell(&distro, p)),
            ShellDecision::Profile(name) => self.config.profile(&name).map(profile_shell),
        }
    }

    fn activate_worktree(&mut self, ctx: &Context, path: &Path) {
        // The dir can vanish between discovery marking the row live and the
        // click. Switching first would strand the user on a dead workspace
        // with a failed spawn — stay put and let the sidebar re-mark the row.
        if !path.is_dir() {
            self.last_error =
                Some("worktree directory is missing — prune it from the sidebar".to_string());
            if let Some(idx) =
                self.projects.iter().position(|p| p.worktrees.iter().any(|w| w.path == path))
            {
                self.projects[idx].refresh();
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
        if let Err(e) = self.spawn_session(ctx, self.current_workspace.clone()) {
            self.last_error = Some(format!("failed to spawn shell: {e}"));
        }
    }

    /// Re-attach to an existing session when the active id went stale
    /// (closed or reaped this frame). Never spawns: an emptied on-screen
    /// workspace either navigated away in `close_session` or shows the
    /// "no session" placeholder.
    fn adopt_active_session(&mut self) {
        let ws_idx = self.workspace_session_indices(&self.current_workspace);
        if let Some(&idx) = ws_idx.first() {
            let id = self.sessions[idx].id;
            self.active_session.insert(self.current_workspace.clone(), id);
        }
    }

    fn close_session(&mut self, ctx: &Context, id: SessionId) {
        let Some(idx) = self.sessions.iter().position(|s| s.id == id) else {
            return;
        };
        let workspace = self.sessions[idx].working_directory.clone();
        self.sessions.remove(idx);

        if self.active_session.get(&workspace).copied() == Some(id) {
            let fallback =
                self.sessions.iter().find(|s| s.working_directory == workspace).map(|s| s.id);
            match fallback {
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
        // unclosable), `navigate` falls back to the project main, then home.
        let remaining: Vec<(WorkspaceKey, SessionId)> =
            self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect();
        let main = workspace.as_deref().and_then(|p| project_main_for(&self.projects, p));
        let verdict = close_fallback(&workspace, &self.current_workspace, &remaining, main);
        if verdict != CloseFallback::Stay
            && self.config.ui.last_session_close == LastSessionClose::Respawn
        {
            if let Err(e) = self.spawn_session(ctx, workspace) {
                self.last_error = Some(format!("failed to spawn shell: {e}"));
            }
            return;
        }
        match verdict {
            CloseFallback::Stay => {},
            CloseFallback::Activate(main) => {
                // The fallback verified a session exists there, so this
                // adopts rather than spawns.
                self.current_workspace = Some(main);
                self.ensure_active_session(ctx);
                // Adopting an existing session produces no PTY events, so
                // nothing else would wake the next paint.
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
        if self.config.ui.confirm_session_close.requires_prompt(session.is_busy()) {
            self.pending_session_close = Some(id);
        } else {
            self.close_session(ctx, id);
        }
    }

    fn workspace_session_indices(&self, ws: &WorkspaceKey) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.working_directory == *ws)
            .map(|(i, _)| i)
            .collect()
    }

    fn current_session_indices(&self) -> Vec<usize> {
        self.workspace_session_indices(&self.current_workspace)
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

    fn workspace_order(&self) -> Vec<WorkspaceKey> {
        let mut order: Vec<WorkspaceKey> = vec![None];
        for project in &self.projects {
            for wt in &project.worktrees {
                // Prunable rows can't host a shell; cycling into one would
                // just bounce off the activate guard on every keypress.
                if !wt.prunable {
                    order.push(Some(wt.path.clone()));
                }
            }
        }
        order
    }

    fn add_project_via_dialog(&mut self, ctx: &Context) {
        let Some(path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let path = wsl::normalize_root(path);
        if self.projects.iter().any(|p| p.root == path) {
            return;
        }
        match wsl::classify(&path) {
            wsl::Location::Windows(_) => self.projects.push(Project::discover(path.clone())),
            wsl::Location::Wsl { .. } => {
                self.projects.push(Project::placeholder(path.clone()));
                let idx = self.projects.len() - 1;
                self.refresh_project(ctx, idx);
            },
        }
        self.persist_project(&path);
    }

    /// Put a project in the sidebar, discovering its worktrees.  A project that
    /// is already there is left alone rather than duplicated, so callers that
    /// cannot see the sidebar (IPC) need not check first.  WSL roots discover
    /// synchronously here (no `ctx` for a worker); the folder picker uses the
    /// async path in `add_project_via_dialog`.
    fn add_project(&mut self, path: PathBuf) -> &Project {
        if let Some(idx) = self.projects.iter().position(|p| p.root == path) {
            return &self.projects[idx];
        }
        self.projects.push(Project::discover(path.clone()));
        self.persist_project(&path);
        self.projects.last().expect("just pushed")
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
            &self.listed_session_ids(),
            self.active_session.get(&self.current_workspace).copied(),
        ));
        // Seeding reads the unfiltered tree, so a lingering filter from a prior
        // focus round-trip can leave the seeded row outside the current rows;
        // repair it immediately rather than waiting for the first key press.
        let rows = self.current_project_rows();
        self.sidebar_cursor = sidebar_nav::ensure_cursor(&rows, self.sidebar_cursor.as_ref());
        self.sidebar_cursor_moved = true;
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

    /// Match key events against the binding table (user bindings + defaults)
    /// before the terminal sees raw events, so a binding wins over plain
    /// text input.  Matched events are consumed unless every matched action
    /// is `ReceiveChar` (alacritty's pass-through marker).
    fn handle_shortcuts(&mut self, ctx: &Context) {
        let actions: Vec<BindingAction> = ctx.input_mut(|i| {
            let mut actions = Vec::new();
            i.events.retain(|ev| {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                    let matched =
                        crate::bindings::all_matches(&self.config.bindings, *key, *modifiers);
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
            self.dispatch_action(ctx, action);
        }
    }

    /// Arrow/Enter/Escape navigation while the projects sidebar owns
    /// keyboard focus.  Consumes only unmodified keys, so modifier-bound
    /// app shortcuts still match in `handle_shortcuts` afterwards.
    fn handle_sidebar_nav(&mut self, ctx: &Context) {
        let filter = &mut self.project_filter;
        let steps: Vec<SidebarNavStep> = ctx.input_mut(|i| {
            let mut steps = Vec::new();
            i.events.retain(|ev| match ev {
                egui::Event::Text(text) => match filter.on_text(text) {
                    Some(outcome) => {
                        steps.push(SidebarNavStep::Filter(outcome));
                        false
                    },
                    None => true,
                },
                egui::Event::Key { key, pressed: true, modifiers, .. } if modifiers.is_none() => {
                    if let Some(outcome) = filter.on_key(*key) {
                        steps.push(SidebarNavStep::Filter(outcome));
                        return false;
                    }
                    if is_sidebar_nav_key(*key) {
                        steps.push(SidebarNavStep::Nav(*key));
                        return false;
                    }
                    true
                },
                _ => true,
            });
            steps
        });
        for step in steps {
            match step {
                SidebarNavStep::Filter(outcome) => self.apply_filter_outcome(ctx, outcome),
                SidebarNavStep::Nav(key) => self.apply_sidebar_nav(ctx, key),
            }
        }
    }

    fn apply_filter_outcome(&mut self, ctx: &Context, outcome: panel_filter::Outcome) {
        use panel_filter::Outcome;
        match outcome {
            Outcome::FilterChanged => self.after_filter_changed(),
            Outcome::Consumed => {},
            Outcome::MoveCursor(delta) => self.move_sidebar_cursor(delta),
            // Enter clears the query before yielding Activate, so the filtered
            // row set is already gone; activate the cursor the preceding
            // MoveCursor/FilterChanged handling maintained against it, rather
            // than re-deriving rows and rejecting a now-hidden worktree.
            Outcome::Activate => {
                if let Some(cursor) = self.sidebar_cursor.clone() {
                    self.activate_sidebar_row(ctx, &cursor);
                }
            },
            Outcome::LeavePanel => self.focus_terminal(),
        }
    }

    /// Repair the cursor after the row set narrows or widens: keep it where it
    /// is when still visible, otherwise fall to the first surviving row.
    fn after_filter_changed(&mut self) {
        let rows = self.current_project_rows();
        let next = sidebar_nav::ensure_cursor(&rows, self.sidebar_cursor.as_ref());
        if next != self.sidebar_cursor {
            self.sidebar_cursor = next;
            self.sidebar_cursor_moved = true;
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

    /// Rows the sidebar cursor steps over this frame: the fuzzy/toggle-filtered
    /// set while a filter is active, the full visible set otherwise.
    fn current_project_rows(&mut self) -> Vec<SidebarRow> {
        let listed_sessions = self.listed_session_ids();
        if !self.project_filter.is_filtering() {
            return sidebar_nav::visible_rows(&self.projects, &listed_sessions);
        }

        let toggle_sessions = self.project_filter.is_toggled('s');
        let toggle_attention = self.project_filter.is_toggled('a');
        let any_toggle = toggle_sessions || toggle_attention;

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

        let toggles_pass = |key: &WorkspaceKey| {
            (!toggle_sessions || self.workspace_has_sessions(key))
                && (!toggle_attention || self.workspace_needs_attention(key))
        };
        let home = home_matches && toggles_pass(&None);
        let project_self =
            |p: &Project| !any_toggle && project_matches.get(&p.root).copied().unwrap_or(false);
        let mut worktree = |_p: &Project, wt: &Worktree| {
            worktree_matches.get(&wt.path).copied().unwrap_or(false)
                && toggles_pass(&Some(wt.path.clone()))
        };
        sidebar_nav::filtered_rows(
            &self.projects,
            &listed_sessions,
            sidebar_nav::RowPredicates {
                home,
                project_self: &project_self,
                worktree: &mut worktree,
            },
        )
    }

    fn workspace_has_sessions(&self, key: &WorkspaceKey) -> bool {
        self.sessions.iter().any(|s| s.working_directory == *key)
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
                SidebarRow::Worktree(_) | SidebarRow::Session(_) => {
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
        let steps: Vec<SidebarNavStep> = ctx.input_mut(|i| {
            let mut steps = Vec::new();
            i.events.retain(|ev| match ev {
                egui::Event::Text(text) => match filter.on_text(text) {
                    Some(outcome) => {
                        steps.push(SidebarNavStep::Filter(outcome));
                        false
                    },
                    None => true,
                },
                egui::Event::Key { key, pressed: true, modifiers, .. } if modifiers.is_none() => {
                    if let Some(outcome) = filter.on_key(*key) {
                        steps.push(SidebarNavStep::Filter(outcome));
                        return false;
                    }
                    if is_sidebar_nav_key(*key) {
                        steps.push(SidebarNavStep::Nav(*key));
                        return false;
                    }
                    true
                },
                _ => true,
            });
            steps
        });
        for step in steps {
            match step {
                SidebarNavStep::Filter(outcome) => self.apply_git_filter_outcome(ctx, outcome),
                SidebarNavStep::Nav(key) => self.apply_git_sidebar_nav(ctx, key),
            }
        }
    }

    fn apply_git_filter_outcome(&mut self, ctx: &Context, outcome: panel_filter::Outcome) {
        use panel_filter::Outcome;
        match outcome {
            Outcome::FilterChanged => self.after_git_filter_changed(),
            Outcome::Consumed => {},
            Outcome::MoveCursor(delta) => self.move_git_cursor(delta),
            // Enter clears the query before yielding Activate, so act on the
            // cursor the preceding movement maintained against the filtered
            // rows rather than re-deriving a now-widened set.  Focus stays on
            // the panel so the next file is one keystroke away.
            Outcome::Activate => {
                if let Some(cursor) = self.git_cursor.clone() {
                    if let Some(req) =
                        git_row_diff_request(&cursor, self.git_branch_base.as_deref())
                    {
                        self.open_diff(ctx, req);
                    }
                }
            },
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
        let m = self.git_filter.is_toggled('m');
        let d = self.git_filter.is_toggled('d');
        let u = self.git_filter.is_toggled('u');
        let any = m || d || u;
        let kind_pass = move |k: ChangeKind| {
            !any || (m && matches!(k, ChangeKind::Modified | ChangeKind::Renamed))
                || (d && k == ChangeKind::Deleted)
                || (u && matches!(k, ChangeKind::Untracked | ChangeKind::Added))
        };
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

    fn dispatch_action(&mut self, ctx: &Context, action: BindingAction) {
        match action {
            BindingAction::Chars(bytes) => {
                if let Some(idx) = self.active_session_index() {
                    paste::on_terminal_input_start(&self.sessions[idx]);
                    self.sessions[idx].write(bytes);
                }
            },
            BindingAction::Named(NamedAction::Paste) => {
                if let (Some(text), Some(idx)) =
                    (clipboard::read(Target::Clipboard), self.active_session_index())
                {
                    paste::paste(&self.sessions[idx], &text, true);
                }
            },
            BindingAction::Named(NamedAction::PasteSelection) => {
                if let (Some(text), Some(idx)) =
                    (clipboard::read(Target::Primary), self.active_session_index())
                {
                    paste::paste(&self.sessions[idx], &text, true);
                }
            },
            BindingAction::Named(NamedAction::Copy) => {
                if let Some(idx) = self.active_session_index() {
                    paste::copy_selection(&self.sessions[idx], &self.config, Target::Clipboard);
                }
            },
            BindingAction::Named(NamedAction::CopySelection) => {
                if let Some(idx) = self.active_session_index() {
                    paste::copy_selection(&self.sessions[idx], &self.config, Target::Primary);
                }
            },
            BindingAction::Named(NamedAction::SpawnNewInstance) => {
                let ws = self.current_workspace.clone();
                if let Err(e) = self.spawn_session(ctx, ws) {
                    self.last_error = Some(format!("failed to spawn shell: {e}"));
                }
            },
            BindingAction::Named(NamedAction::Quit) => {
                self.quit_dialog_open = true;
            },
            BindingAction::Named(NamedAction::ClearHistory) => {
                use alacritty_terminal::vte::ansi::{ClearMode, Handler};
                if let Some(idx) = self.active_session_index() {
                    self.sessions[idx].term.lock().clear_screen(ClearMode::Saved);
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
                        self.last_error = Some(format!("SpawnProfile{n}: no such profile"));
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
            BindingAction::Named(NamedAction::SelectNextWorkspace) => {
                self.cycle_workspaces(ctx, 1);
            },
            BindingAction::Named(NamedAction::SelectPreviousWorkspace) => {
                self.cycle_workspaces(ctx, -1);
            },
            BindingAction::Named(NamedAction::AddProject) => self.add_project_via_dialog(ctx),
            BindingAction::Named(NamedAction::ToggleSidebarFocus) => match self.focus {
                PaneFocus::Terminal => self.focus_sidebar(),
                PaneFocus::ProjectsSidebar => self.focus_terminal(),
                // Toggle stays "left ↔ terminal"; from the right panel it hops
                // to the left one rather than doing nothing.
                PaneFocus::GitSidebar => self.focus_sidebar(),
            },
            BindingAction::Named(NamedAction::CloseSession) => {
                if let Some(idx) = self.active_session_index() {
                    let id = self.sessions[idx].id;
                    self.request_close_session(ctx, id);
                }
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
            BindingAction::Named(other) => {
                self.dispatch_scroll_or_other(other);
            },
            BindingAction::Unsupported(name) => {
                log::debug!("unsupported keyboard binding action: {name}");
            },
        }
    }

    fn dispatch_scroll_or_other(&mut self, action: NamedAction) {
        use alacritty_terminal::grid::{Dimensions, Scroll};
        let Some(idx) = self.active_session_index() else {
            return;
        };
        let session = &mut self.sessions[idx];
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
        if indices.is_empty() {
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
        // Session segments only when there is a choice to make, but the
        // trailing + segment always renders alongside them once the strip
        // itself renders (i.e. at least one session exists).
        if indices.len() >= 2 {
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
            if let Err(e) = self.spawn_session(&ctx, ws) {
                self.last_error = Some(format!("failed to spawn shell: {e}"));
            }
        }
        if let Some(name) = spawn_profile {
            let ctx = ui.ctx().clone();
            self.spawn_profile_session(&ctx, &name);
        }
    }

    fn show_project_sidebar(&mut self, ctx: &Context, panel_frame: Frame) -> egui::Rect {
        let activate_request: std::cell::Cell<Option<PathBuf>> = std::cell::Cell::new(None);
        let delete_request: std::cell::Cell<Option<DeleteRequest>> = std::cell::Cell::new(None);
        let create_request: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
        let spawn_shell_request: std::cell::Cell<Option<WorkspaceKey>> = std::cell::Cell::new(None);
        let activate_session_request: std::cell::Cell<Option<(WorkspaceKey, SessionId)>> =
            std::cell::Cell::new(None);
        let close_session_request: std::cell::Cell<Option<SessionId>> = std::cell::Cell::new(None);
        // Drag-to-reorder: (dragged root, insert-before display index).
        let reorder_request: std::cell::Cell<Option<(PathBuf, usize)>> = std::cell::Cell::new(None);
        let mut add_project_clicked = false;
        let mut reorder_toggled = false;
        let mut refresh_idx: Option<usize> = None;
        let mut remove_request: Option<ProjectRemoveState> = None;
        let mut expand_toggled: Option<(PathBuf, bool)> = None;
        let mut home_clicked = false;
        let theme = self.theme;
        let reorder_mode = self.reorder_mode;
        let cursor_row = if self.focus == PaneFocus::ProjectsSidebar {
            self.sidebar_cursor.clone()
        } else {
            None
        };
        let cursor_moved = std::mem::take(&mut self.sidebar_cursor_moved);

        // Membership for the active filter, resolved once so paint can skip
        // non-surviving rows.  While filtering, matched projects render their
        // matched worktrees regardless of `expanded` (display-only — the flag
        // is never written).
        let filtering = self.project_filter.is_filtering();
        let mut home_visible = true;
        let mut visible_projects: HashSet<PathBuf> = HashSet::new();
        let mut visible_worktrees: HashSet<PathBuf> = HashSet::new();
        if filtering {
            home_visible = false;
            for row in self.current_project_rows() {
                match row {
                    SidebarRow::Home => home_visible = true,
                    SidebarRow::Project(root) => {
                        visible_projects.insert(root);
                    },
                    SidebarRow::Worktree(path) => {
                        visible_worktrees.insert(path);
                    },
                    // Session rows follow their workspace row's visibility.
                    SidebarRow::Session(_) => {},
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
        let home_session_rows = self.workspace_session_rows(&None);
        let worktree_session_rows: Vec<Vec<Vec<SessionRowData>>> = self
            .projects
            .iter()
            .map(|p| {
                p.worktrees
                    .iter()
                    .map(|wt| self.workspace_session_rows(&Some(wt.path.clone())))
                    .collect()
            })
            .collect();

        let worktree_listed: Vec<Vec<bool>> = worktree_session_rows
            .iter()
            .map(|v| v.iter().map(|rows| !rows.is_empty()).collect())
            .collect();

        // A rendered session list carries its own per-session dots and
        // glyphs; repeating them on the parent row reads as noise — the same
        // rule the project row applies when expanded.  Aggregates therefore
        // apply only while the list is hidden (fewer than two sessions).
        let home_attention = home_session_rows.is_empty() && self.workspace_needs_attention(&None);
        let home_agent_glyph =
            if home_session_rows.is_empty() { self.workspace_agent_glyph(&None) } else { None };
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
        let worktree_agent: Vec<Vec<Option<char>>> = self
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
                            None
                        } else {
                            self.workspace_agent_glyph(&Some(wt.path.clone()))
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
        let profile_names: Vec<String> =
            self.config.profiles.iter().map(|p| p.name.clone()).collect();
        let mut shell_override_changed: Option<PathBuf> = None;
        let mut label_cleared: Option<PathBuf> = None;
        let mut rename_request: Option<RenameState> = None;

        let panel_resp = SidePanel::left("left_sidebar")
            .resizable(true)
            .default_width(240.0 * theme.ui_scale)
            .min_width(180.0 * theme.ui_scale)
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Sidebar rows are click targets, not selectable prose; the
                // default I-beam-and-select on labels is the wrong affordance.
                ui.style_mut().interaction.selectable_labels = false;
                ui.horizontal(|ui| {
                    panel_header_filter_ui(
                        ui,
                        "Projects",
                        &self.project_filter,
                        &self.config.ui.icons.search,
                        &theme,
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, "+", theme.text_dim, &theme)
                            .on_hover_text("add project")
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
                        if icon_button(ui, "⇅", color, &theme).on_hover_text(hint).clicked() {
                            reorder_toggled = true;
                        }
                    });
                });
                ui.separator();

                ScrollArea::vertical().show(ui, |ui| {
                    if !filtering || home_visible {
                        let home_action = home_row(
                            ui,
                            self.current_workspace.is_none(),
                            matches!(&cursor_row, Some(SidebarRow::Home)),
                            cursor_moved,
                            home_attention,
                            home_agent_glyph,
                            &theme,
                        );
                        if home_action.activate {
                            home_clicked = true;
                        }
                        if home_action.spawn {
                            spawn_shell_request.set(Some(None));
                        }
                        for row in &home_session_rows {
                            let is_cursor = matches!(
                                &cursor_row,
                                Some(SidebarRow::Session(id)) if *id == row.id
                            );
                            let act = session_row(ui, row, is_cursor, cursor_moved, &theme);
                            if act.activate {
                                activate_session_request.set(Some((None, row.id)));
                            }
                            if act.close {
                                close_session_request.set(Some(row.id));
                            }
                        }
                        ui.add_space(2.0);
                    }

                    if self.projects.is_empty() {
                        ui.label(
                            RichText::new("Click + to add a project.")
                                .color(theme.text_dim)
                                .small(),
                        );
                        ui.add_space(4.0);
                        ui.label(RichText::new("Ctrl+B to toggle").small().color(theme.text_muted));
                    } else if filtered_empty {
                        ui.label(RichText::new("no matches").color(theme.text_dim).small());
                    }

                    for (idx, project) in self.projects.iter_mut().enumerate() {
                        if filtering && !visible_projects.contains(&project.root) {
                            continue;
                        }
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
                                let arrow = if project.expanded { "▾" } else { "▸" };
                                if icon_button(ui, arrow, theme.text_dim, &theme).clicked() {
                                    project.expanded = !project.expanded;
                                    expand_toggled = Some((project.root.clone(), project.expanded));
                                }
                                name_resp = Some(
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(project.display_name())
                                                .color(theme.text)
                                                .strong()
                                                .small(),
                                        )
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                    ),
                                );
                            },
                            |ui| {
                                if icon_button(ui, "×", theme.text_muted, &theme)
                                    .on_hover_text("remove from sidebar")
                                    .clicked()
                                {
                                    remove_request = Some(ProjectRemoveState {
                                        root: project_root.clone(),
                                        name: project_name.clone(),
                                    });
                                }
                                if icon_button(ui, "↻", theme.text_muted, &theme)
                                    .on_hover_text("refresh worktrees")
                                    .clicked()
                                {
                                    refresh_idx = Some(idx);
                                }
                                if icon_button(ui, "+", theme.text_muted, &theme)
                                    .on_hover_text("create new worktree")
                                    .clicked()
                                {
                                    create_request.set(Some(idx));
                                }
                                if show_proj_dot {
                                    attention_dot(ui, &theme);
                                }
                            },
                        );
                        if matches!(&cursor_row, Some(SidebarRow::Project(r)) if *r == project.root)
                        {
                            let rect = egui::Rect::from_x_y_ranges(
                                ui.max_rect().x_range(),
                                row_rect.y_range(),
                            );
                            paint_cursor_outline(ui, rect, &theme);
                            if cursor_moved {
                                ui.scroll_to_rect(rect, None);
                            }
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
                                let before = pointer.y < row_rect.center().y;
                                let y = if before { row_rect.top() } else { row_rect.bottom() };
                                ui.painter().hline(
                                    row_rect.x_range(),
                                    y,
                                    Stroke::new(2.0 * theme.ui_scale, theme.accent),
                                );
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
                                let wt_glyph = worktree_agent
                                    .get(idx)
                                    .and_then(|v| v.get(wt_idx))
                                    .copied()
                                    .unwrap_or(None);
                                let is_cursor = matches!(
                                    &cursor_row,
                                    Some(SidebarRow::Worktree(p)) if *p == wt.path
                                );
                                let is_deleting = deleting_paths.contains(&wt.path);
                                let action = worktree_row(
                                    ui,
                                    wt,
                                    is_active,
                                    is_cursor,
                                    cursor_moved,
                                    wt_attention,
                                    wt_glyph,
                                    is_deleting,
                                    &theme,
                                );
                                if action.activate {
                                    activate_request.set(Some(wt.path.clone()));
                                }
                                if action.delete {
                                    // Discovery marking can be stale; a dir
                                    // deleted since the last refresh should
                                    // still get the prune flow, not a doomed
                                    // `git worktree remove`.
                                    let prunable = wt.prunable || !wt.path.is_dir();
                                    delete_request.set(Some(DeleteRequest {
                                        project_idx: idx,
                                        worktree_path: wt.path.clone(),
                                        worktree_name: wt.name.clone(),
                                        branch: wt.branch.clone(),
                                        // A missing dir has nothing to be dirty;
                                        // skip the status probe.
                                        dirty: if prunable {
                                            DirtyCounts::default()
                                        } else {
                                            git_status::dirty_counts(&wt.path)
                                        },
                                        prunable,
                                        delete_branch: true,
                                    }));
                                }
                                if action.spawn {
                                    spawn_shell_request.set(Some(Some(wt.path.clone())));
                                }
                                let session_rows = worktree_session_rows
                                    .get(idx)
                                    .and_then(|v| v.get(wt_idx))
                                    .map(Vec::as_slice)
                                    .unwrap_or(&[]);
                                for row in session_rows {
                                    let is_cursor = matches!(
                                        &cursor_row,
                                        Some(SidebarRow::Session(id)) if *id == row.id
                                    );
                                    let act = session_row(ui, row, is_cursor, cursor_moved, &theme);
                                    if act.activate {
                                        activate_session_request
                                            .set(Some((Some(wt.path.clone()), row.id)));
                                    }
                                    if act.close {
                                        close_session_request.set(Some(row.id));
                                    }
                                }
                            }
                            for (_, branch) in creating.iter().filter(|(pi, _)| *pi == idx) {
                                creating_row(ui, branch, &theme);
                            }
                            ui.add_space(4.0);
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
        if home_clicked {
            self.activate_home(ctx);
        }
        if let Some(path) = activate_request.take() {
            self.activate_worktree(ctx, &path);
        }
        if let Some(req) = delete_request.take() {
            self.pending_delete = Some(req);
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
        }
        if let Some(id) = close_session_request.take() {
            self.request_close_session(ctx, id);
        }
        if let Some(ws) = spawn_shell_request.take() {
            // Spawning activates the workspace and the new session, matching
            // Ctrl+T and worktree-creation's open-on-done.
            self.current_workspace = ws.clone();
            if let Err(e) = self.spawn_session(ctx, ws) {
                self.last_error = Some(format!("failed to spawn shell: {e}"));
            }
        }
        panel_resp.response.rect
    }

    fn active_session_path(&self) -> Option<PathBuf> {
        self.current_workspace.clone()
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

    fn show_git_sidebar(&mut self, ctx: &Context, panel_frame: Frame) -> egui::Rect {
        let theme = self.theme;
        let palette = self.config.palette.clone();
        let active_diff_key = self.active_diff_key();
        let diff_request: std::cell::Cell<Option<DiffRequest>> = std::cell::Cell::new(None);
        let panel_resp = SidePanel::right("right_sidebar")
            .resizable(true)
            .default_width(300.0 * theme.ui_scale)
            .min_width(220.0 * theme.ui_scale)
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Sidebar rows are click targets, not selectable prose; the
                // default I-beam-and-select on labels is the wrong affordance.
                ui.style_mut().interaction.selectable_labels = false;
                ui.horizontal(|ui| {
                    panel_header_filter_ui(
                        ui,
                        "Git",
                        &self.git_filter,
                        &self.config.ui.icons.search,
                        &theme,
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
                // PR base takes precedence over the repo's default branch so
                // the sidebar diff matches what GitHub will review.
                let effective_default =
                    pr_info.as_ref().map(|p| p.base_branch.clone()).or(project_default);
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

                    ui.add(
                        egui::Label::new(
                            RichText::new(path.display().to_string())
                                .color(theme.text_muted)
                                .small(),
                        )
                        .truncate(),
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
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(default).color(theme.text_dim).small(),
                                        )
                                        .truncate(),
                                    );
                                    ui.label(RichText::new("vs").color(theme.text_muted).small());
                                }
                            },
                        );
                    }
                    ui.add_space(10.0);

                    section(ui, &theme, "Staged", staged_count, filtering, |ui| {
                        for f in &status.staged {
                            if !staged_visible.contains(&f.path) {
                                continue;
                            }
                            let req =
                                DiffRequest { file: f.path.clone(), source: DiffSource::Staged };
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
                    });

                    section(ui, &theme, "Unstaged", unstaged_count, filtering, |ui| {
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
                    });

                    if !status.branch_diff.is_empty() {
                        let base_label = match &status.default_branch {
                            Some(b) => format!("Changes vs {b}"),
                            None => "Changes vs default".to_string(),
                        };
                        let base = git_branch_base.clone();
                        let count_label = section_count_label(&branch_count, filtering);

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
                        ui.add_space(10.0);
                    }
                });
            });
        if let Some(req) = diff_request.take() {
            self.open_diff(ctx, req);
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
        let title = format!("diff: {}", req.file);
        match Session::spawn_command(
            ctx.clone(),
            &self.config,
            Some(workspace.clone()),
            TermSize::new(80, 24),
            (8.0, 16.0),
            program,
            args,
            title,
            SessionKind::Diff { key: new_key },
        ) {
            Ok(session) => {
                let id = session.id;
                self.sessions.push(session);
                self.active_session.insert(Some(workspace), id);
            },
            Err(e) => {
                self.last_error = Some(format!("failed to open diff: {e}"));
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
        match self.pending_delta.get(distro).map(Receiver::try_recv) {
            Some(Ok(Some(path))) => {
                self.pending_delta.remove(distro);
                self.wsl_delta_paths.insert(distro.to_string(), path);
            },
            Some(Ok(None)) | Some(Err(TryRecvError::Disconnected)) => {
                self.pending_delta.remove(distro);
            },
            _ => {},
        }

        if let Some(path) = self.wsl_delta_paths.get(distro) {
            return Some(path.clone());
        }

        if !self.pending_delta.contains_key(distro) {
            let (tx, rx) = mpsc::channel();
            let distro_owned = distro.to_string();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let found = wsl::discover_delta(&distro_owned);
                let _ = tx.send(found);
                ctx.request_repaint();
            });
            self.pending_delta.insert(distro.to_string(), rx);
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

fn dirty_warning(counts: &DirtyCounts) -> Option<String> {
    if !counts.is_dirty() {
        return None;
    }
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
    Some(format!(
        "Working tree has {} file(s) — they will be discarded with --force.",
        parts.join(", ")
    ))
}

fn modal_frame(theme: &Theme) -> Frame {
    let s = theme.ui_scale;
    let pad_x = (16.0 * s).round() as i8;
    let pad_y = (12.0 * s).round() as i8;
    Frame::default()
        .fill(theme.sidebar_bg)
        .stroke(Stroke::new(1.0_f32, theme.sidebar_border))
        .inner_margin(Margin { left: pad_x, right: pad_x, top: pad_y, bottom: pad_y })
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
fn section<R>(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: &str,
    count: SectionCount,
    filtering: bool,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) {
    if count.total == 0 {
        return;
    }
    let label = section_count_label(&count, filtering);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).color(theme.text).strong().small());
        ui.label(RichText::new(label).color(theme.text_muted).small());
    });
    ui.add_space(2.0);
    add_contents(ui);
    ui.add_space(10.0);
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
                ui.add(
                    egui::Label::new(
                        RichText::new(change.kind.glyph()).color(color).monospace().small(),
                    )
                    .selectable(false),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(&change.path).color(path_color).monospace().small(),
                    )
                    .truncate()
                    .selectable(false),
                );
                fill_row(ui);
            },
        )
        .response
        .interact(egui::Sense::click());
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
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&stat.path).color(path_color).monospace().small(),
                                )
                                .truncate()
                                .selectable(false),
                            );
                            fill_row(ui);
                        },
                    );
                }
            },
        )
        .response
        .interact(egui::Sense::click());
    paint_row_bg(ui, &resp, bg_idx, panel_x, theme, is_active);
    resp
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

/// Painted (rather than `RichText("●")`) so its size is independent of font
/// metrics — `RichText("●")` renders inconsistently across fallback fonts.
fn attention_dot(ui: &mut egui::Ui, theme: &Theme) {
    let (rect, _) = ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
    let radius = 3.0 * theme.ui_scale;
    ui.painter().circle_filled(rect.center(), radius, theme.attention);
}

/// Priority: attention dot > agent glyph (animated by the agent's own title
/// updates) > the row's default marker.
fn paint_row_status_icon(
    ui: &mut egui::Ui,
    theme: &Theme,
    attention: bool,
    agent_glyph: Option<char>,
    default_glyph: &str,
    is_active: bool,
) {
    if attention {
        attention_dot(ui, theme);
        return;
    }
    let s = theme.ui_scale;
    let (glyph, color) = match agent_glyph {
        Some(g) => (g.to_string(), if is_active { theme.accent } else { theme.text }),
        None => {
            (default_glyph.to_string(), if is_active { theme.accent } else { theme.text_muted })
        },
    };
    // Centered into the fixed slot, like `icon_button`: laying the glyph out as
    // text would size the slot to its advance width and shift the label with it.
    let (rect, _) = ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(10.0 * s),
        color,
    );
}

/// Gap between adjacent `icon_button`s. They already pad their own glyph, so
/// the default item spacing on top of that reads as a hole in the cluster.
/// Deliberately unscaled: the padding it supplements grows with `ui_scale`.
const ICON_CLUSTER_SPACING: f32 = 2.0;

/// Frameless, fixed-footprint icon button. Painter-drawn rather than a
/// `Button` because `Button` lays text out from the top-left of its rect, so
/// glyphs of different intrinsic heights (e.g. `+` vs `↻`) end up on different
/// baselines. `painter.text` with `CENTER_CENTER` centers the galley in the
/// rect, giving real grid alignment.
fn icon_button(ui: &mut egui::Ui, glyph: &str, color: Color32, theme: &Theme) -> egui::Response {
    let s = theme.ui_scale;
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
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(12.0 * s),
        painted,
    );
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
        ui.scroll_to_rect(rect, None);
    }
}

/// One drained event's effect on a sidebar panel: either a filter outcome
/// (search/toggle) or a plain browsing nav key.
enum SidebarNavStep {
    Filter(panel_filter::Outcome),
    Nav(egui::Key),
}

/// Panel title plus its filter chrome, shared by both sidebars: the heading,
/// then `[s]`-style chips for each active toggle, then a bordered
/// `<icon> query▌` input box while searching (`search_icon` comes from
/// `[ui] search_icon`).  Renders only the title when the filter is idle.
fn panel_header_filter_ui(
    ui: &mut egui::Ui,
    title: &str,
    filter: &PanelFilter,
    search_icon: &str,
    theme: &Theme,
) {
    ui.label(RichText::new(title).color(theme.text).strong());
    for key in filter.active_toggles() {
        ui.label(RichText::new(format!("[{key}]")).color(theme.accent).monospace().small());
    }
    if filter.mode() == panel_filter::Mode::Search || !filter.query().is_empty() {
        let s = theme.ui_scale;
        Frame::default()
            .stroke(Stroke::new(1.0_f32, theme.text_muted))
            .corner_radius((3.0 * s).round() as u8)
            .inner_margin(Margin::symmetric((4.0 * s).round() as i8, (1.0 * s).round() as i8))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 3.0 * s;
                ui.label(RichText::new(search_icon).color(theme.text_dim).small());
                ui.label(
                    RichText::new(format!("{}▌", filter.query()))
                        .color(theme.text)
                        .monospace()
                        .small(),
                );
            });
    }
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
}

fn home_row(
    ui: &mut egui::Ui,
    is_active: bool,
    is_cursor: bool,
    scroll_into_view: bool,
    attention: bool,
    agent_glyph: Option<char>,
    theme: &Theme,
) -> HomeAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut spawn_clicked = false;
    let mut spawn_rect: Option<egui::Rect> = None;
    let frame = Frame::default().inner_margin(Margin { left: 6, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            row_with_trailing(
                ui,
                |ui| {
                    paint_row_status_icon(ui, theme, attention, agent_glyph, "⌂", is_active);
                    ui.label(
                        RichText::new("Home")
                            .color(if is_active { theme.text } else { theme.text_dim })
                            .strong()
                            .small(),
                    );
                },
                |ui| {
                    let btn =
                        icon_button(ui, "+", theme.text_muted, theme).on_hover_text("new shell");
                    spawn_rect = Some(btn.rect);
                    if btn.clicked() {
                        spawn_clicked = true;
                    }
                },
            );
        })
        .response
        .interact(egui::Sense::click());

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
    if is_cursor {
        let full_rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
        paint_cursor_outline(ui, full_rect, theme);
        if scroll_into_view {
            ui.scroll_to_rect(full_rect, None);
        }
    }
    HomeAction { activate: resp.clicked() && !spawn_clicked, spawn: spawn_clicked }
}

struct WorktreeAction {
    activate: bool,
    delete: bool,
    spawn: bool,
}

/// Everything a sidebar session row needs, snapshotted before the panel
/// closure so rendering doesn't borrow `self.sessions`.
struct SessionRowData {
    id: SessionId,
    title: String,
    needs_attention: bool,
    agent_glyph: Option<char>,
    /// This workspace's remembered active session (accent icon).
    is_active: bool,
    /// Active *and* the workspace is current — the session on screen
    /// (row background highlight).
    is_displayed: bool,
}

/// Spawn-ordered ids of the sessions in `ws`, or empty below the two-session
/// list threshold — a single-session workspace row keeps its compact form,
/// mirroring the tab strip. Pure over (workspace, id) pairs so the grouping
/// rule is testable without spawning PTYs.
fn sidebar_session_ids(pairs: &[(WorkspaceKey, SessionId)], ws: &WorkspaceKey) -> Vec<SessionId> {
    let ids: Vec<SessionId> = pairs.iter().filter(|(w, _)| w == ws).map(|(_, id)| *id).collect();
    if ids.len() < 2 { Vec::new() } else { ids }
}

/// Where the view goes after a session's removal.
#[derive(Debug, PartialEq)]
enum CloseFallback {
    /// Removal didn't empty the on-screen workspace — no navigation.
    Stay,
    /// Switch to the project's main checkout, which still has a session.
    Activate(PathBuf),
    /// Switch to home; `activate_home` spawns a shell there if none exists.
    Home,
}

/// Post-close navigation for the workspace that just lost a session.
/// `remaining` is the session list after removal; `main_checkout` is the
/// removed workspace's project main (None when the workspace *is* the main,
/// is home, or belongs to no known project). Pure over (workspace, id)
/// pairs for the same reason as `sidebar_session_ids`.
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

/// The owning project's main checkout for `ws`, or None when `ws` already
/// is the main (including non-git roots, whose single pseudo-worktree is
/// its own main) or belongs to no known project.
fn project_main_for(projects: &[Project], ws: &Path) -> Option<PathBuf> {
    let project = projects.iter().find(|p| p.worktrees.iter().any(|w| w.path == ws))?;
    let main = project.worktrees.iter().find(|w| w.is_main)?;
    if main.path == ws { None } else { Some(main.path.clone()) }
}

/// Agent glyphs usually come from the title's own leading char
/// (`Session::agent_glyph`), and the session row paints that glyph as its
/// status icon right next to the title — showing it in both places doubles
/// the icon. Drop the leading glyph from the label when it's exactly what
/// the icon paints, unless that would leave the label empty.
fn session_row_title(title: &str, agent_glyph: Option<char>) -> String {
    if let Some(g) = agent_glyph {
        if let Some(rest) = title.strip_prefix(g) {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    title.to_string()
}

/// Sidebar placeholder for a worktree whose creation the user minimized: a
/// spinner stands in until `poll_pending_creates` refreshes the project and the
/// real worktree row takes its place.  Indentation and the leading glyph match
/// `worktree_row` so it lines up with its future sibling.
fn creating_row(ui: &mut egui::Ui, branch: &str, theme: &Theme) {
    let s = theme.ui_scale;
    let frame = Frame::default().inner_margin(Margin { left: 16, right: 0, top: 3, bottom: 3 });
    frame.show(ui, |ui| {
        row_with_trailing(
            ui,
            |ui| {
                ui.label(RichText::new("○").color(theme.text_muted).size(10.0 * s));
                ui.add(
                    egui::Label::new(RichText::new(branch).color(theme.text_muted).small())
                        .truncate(),
                );
            },
            |ui| {
                ui.add(egui::Spinner::new().size(12.0 * s).color(theme.text_muted));
            },
        );
    });
}

fn worktree_row(
    ui: &mut egui::Ui,
    wt: &Worktree,
    is_active: bool,
    is_cursor: bool,
    scroll_into_view: bool,
    attention: bool,
    agent_glyph: Option<char>,
    deleting: bool,
    theme: &Theme,
) -> WorktreeAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut delete_clicked = false;
    let mut delete_rect: Option<egui::Rect> = None;
    let mut spawn_clicked = false;
    let mut spawn_rect: Option<egui::Rect> = None;
    // right: 0 keeps the worktree `×` at the same x as the project row's `×`,
    // which has no frame margin and sits flush against the panel's outer padding.
    let frame = Frame::default().inner_margin(Margin { left: 16, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            let default_icon = if wt.is_main { "●" } else { "○" };
            let name_color = if wt.prunable || deleting {
                theme.text_muted
            } else if is_active {
                theme.text
            } else {
                theme.text_dim
            };
            row_with_trailing(
                ui,
                |ui| {
                    paint_row_status_icon(
                        ui,
                        theme,
                        attention,
                        agent_glyph,
                        default_icon,
                        is_active,
                    );
                    ui.add(
                        egui::Label::new(RichText::new(&wt.name).color(name_color).small())
                            .truncate(),
                    );
                },
                |ui| {
                    // Mid-removal the row is inert: swap its controls for a
                    // spinner so the user sees the delete is in flight.
                    if deleting {
                        ui.add(
                            egui::Spinner::new()
                                .size(12.0 * theme.ui_scale)
                                .color(theme.text_muted),
                        );
                        return;
                    }
                    if !wt.is_main {
                        let hover = if wt.prunable {
                            "prune worktree"
                        } else {
                            "delete worktree and branch"
                        };
                        let btn =
                            icon_button(ui, "×", theme.text_muted, theme).on_hover_text(hover);
                        delete_rect = Some(btn.rect);
                        if btn.clicked() {
                            delete_clicked = true;
                        }
                    }
                    let btn =
                        icon_button(ui, "+", theme.text_muted, theme).on_hover_text("new shell");
                    spawn_rect = Some(btn.rect);
                    if btn.clicked() {
                        spawn_clicked = true;
                    }
                },
            );
        })
        .response
        .interact(egui::Sense::click());
    let resp = if wt.prunable {
        resp.on_hover_text("worktree directory is missing — × prunes it")
    } else {
        resp
    };

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
        if scroll_into_view {
            ui.scroll_to_rect(full_rect, None);
        }
    }
    WorktreeAction {
        activate: !deleting && resp.clicked() && !delete_clicked && !spawn_clicked && !wt.prunable,
        delete: delete_clicked,
        spawn: spawn_clicked,
    }
}

struct SessionRowAction {
    activate: bool,
    close: bool,
}

fn session_row(
    ui: &mut egui::Ui,
    row: &SessionRowData,
    is_cursor: bool,
    scroll_into_view: bool,
    theme: &Theme,
) -> SessionRowAction {
    // Reserve a slot *before* the labels so the hover bg paints beneath them.
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let panel_x = ui.max_rect().x_range();

    let mut close_clicked = false;
    let mut close_rect: Option<egui::Rect> = None;
    // One indent level deeper than worktree rows (16); right: 0 keeps the ×
    // at the same x as the other rows' trailing icons.
    let frame = Frame::default().inner_margin(Margin { left: 28, right: 0, top: 3, bottom: 3 });
    let resp = frame
        .show(ui, |ui| {
            let title_color = if row.is_active { theme.text } else { theme.text_dim };
            row_with_trailing(
                ui,
                |ui| {
                    paint_row_status_icon(
                        ui,
                        theme,
                        row.needs_attention,
                        row.agent_glyph,
                        "▪",
                        row.is_active,
                    );
                    ui.add(
                        egui::Label::new(RichText::new(&row.title).color(title_color).small())
                            .truncate(),
                    );
                },
                |ui| {
                    let btn = icon_button(ui, "×", theme.text_muted, theme)
                        .on_hover_text("close session");
                    close_rect = Some(btn.rect);
                    if btn.clicked() {
                        close_clicked = true;
                    }
                },
            );
        })
        .response
        .interact(egui::Sense::click());

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
        if scroll_into_view {
            ui.scroll_to_rect(full_rect, None);
        }
    }
    SessionRowAction { activate: resp.clicked() && !close_clicked, close: close_clicked }
}

impl AlacritreeApp {
    fn reap_exited_sessions(&mut self, ctx: &Context) {
        let exited_ids: Vec<SessionId> =
            self.sessions.iter().filter(|s| s.is_exited()).map(|s| s.id).collect();
        for id in exited_ids {
            self.close_session(ctx, id);
        }
    }

    /// Handle workspace-switch requests from clicked notifications.  Only
    /// the most recent click is honored — if multiple toasts piled up, the
    /// user most likely meant the latest one.
    fn process_notification_actions(&mut self, ctx: &Context) {
        let mut latest: Option<WorkspaceKey> = None;
        while let Ok(ws) = self.notify_rx.try_recv() {
            latest = Some(ws);
        }
        let Some(ws) = latest else { return };
        match ws {
            None => self.activate_home(ctx),
            Some(p) => self.activate_worktree(ctx, &p),
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Drain every session's PTY events and surface "needs attention" for
    /// any session the user isn't currently looking at.
    fn process_session_events(&mut self, ctx: &Context) {
        let visible_idx = self.active_session_index();
        // `viewport().focused` is `None` on platforms that don't report focus;
        // treat unknown as "focused" so we don't pile up stale attention dots.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);

        for idx in 0..self.sessions.len() {
            let outcome = self.sessions[idx].drain_events(&self.config.palette);
            // Ahead of the attention early-out: a background session copying
            // with OSC 52 still owns the clipboard.
            for (target, text) in &outcome.clipboard {
                clipboard::write(*target, text);
            }
            if !outcome.attention {
                continue;
            }
            let is_visible_to_user = Some(idx) == visible_idx && focused;
            if is_visible_to_user {
                continue;
            }
            // Only toast on the *transition* into needs_attention — otherwise
            // BEL + title-transition firing in the same idle cycle would
            // produce two toasts for the same "Claude is done" event.
            let was_attending = self.sessions[idx].needs_attention;
            self.sessions[idx].needs_attention = true;
            if !was_attending && self.config.ui.notifications {
                notify_attention(&self.sessions[idx], ctx);
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

    /// Prefer the active session's glyph so two parallel agents don't fight
    /// over which icon the sidebar shows.
    fn workspace_agent_glyph(&self, ws: &WorkspaceKey) -> Option<char> {
        let active_id = self.active_session.get(ws).copied();
        let mut active_glyph = None;
        let mut other_glyph = None;
        for s in &self.sessions {
            if s.working_directory != *ws {
                continue;
            }
            let Some(g) = s.agent_glyph() else { continue };
            if Some(s.id) == active_id {
                active_glyph = Some(g);
                break;
            }
            if other_glyph.is_none() {
                other_glyph = Some(g);
            }
        }
        active_glyph.or(other_glyph)
    }

    /// The session rows every workspace currently lists, for the keyboard
    /// cursor model.  Built from the same `sidebar_session_ids` rule the
    /// paint pass uses, so cursor rows and painted rows cannot drift.
    fn listed_session_ids(&self) -> sidebar_nav::ListedSessions {
        let pairs: Vec<(WorkspaceKey, SessionId)> =
            self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect();
        let mut listed = sidebar_nav::ListedSessions::new();
        for (ws, _) in &pairs {
            if !listed.contains_key(ws) {
                let ids = sidebar_session_ids(&pairs, ws);
                if !ids.is_empty() {
                    listed.insert(ws.clone(), ids);
                }
            }
        }
        listed
    }

    /// Session rows for `ws`'s sidebar list, per `sidebar_session_ids`'s
    /// two-session threshold.
    fn workspace_session_rows(&self, ws: &WorkspaceKey) -> Vec<SessionRowData> {
        let pairs: Vec<(WorkspaceKey, SessionId)> =
            self.sessions.iter().map(|s| (s.working_directory.clone(), s.id)).collect();
        let ids = sidebar_session_ids(&pairs, ws);
        let active = self.active_session.get(ws).copied();
        let is_current = self.current_workspace == *ws;
        ids.iter()
            .filter_map(|id| self.sessions.iter().find(|s| s.id == *id))
            .map(|s| SessionRowData {
                id: s.id,
                title: session_row_title(&s.title, s.agent_glyph()),
                needs_attention: s.needs_attention,
                agent_glyph: s.agent_glyph(),
                is_active: active == Some(s.id),
                is_displayed: is_current && active == Some(s.id),
            })
            .collect()
    }

    fn show_delete_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = rgb_to_color32(self.config.palette.normal[1]);
        let Some(req) = self.pending_delete.as_mut() else {
            return;
        };
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
        let warning = dirty_warning(&req.dirty);

        let (cancel_via_key, confirm_via_key) = consume_modal_keys(ctx);

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

        if confirm_via_key || confirmed {
            self.run_pending_delete(ctx);
            return;
        }
        if cancel_via_key || cancelled || modal.should_close() {
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
        let title = format!("Close session `{}`?", session.title);
        let busy = session.is_busy();

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
                    ui.label(
                        RichText::new("Enter to close · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let close_btn = modal_button(ui, &theme, "Close", danger);
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

    fn run_pending_delete(&mut self, ctx: &Context) {
        let Some(req) = self.pending_delete.take() else {
            return;
        };
        let project_root = self.projects[req.project_idx].root.clone();

        // Drop sessions whose cwd is the worktree before deleting it; the PTY
        // would otherwise block the directory removal on some filesystems.
        self.sessions.retain(|s| s.working_directory.as_deref() != Some(&req.worktree_path));
        self.active_session.remove(&Some(req.worktree_path.clone()));
        if self.current_workspace.as_deref() == Some(&req.worktree_path) {
            // Deleting the on-screen worktree is an explicit user action, so
            // home should greet with a live shell rather than the "no
            // session" placeholder.
            self.activate_home(ctx);
        }

        // The git removal (shellouts, branch delete, doppler cleanup) is slow
        // enough to stutter paint, so run it off-thread and adopt the result in
        // `poll_pending_deletes`; the dialog closes immediately either way and
        // the sidebar row shows a spinner meanwhile.
        let worktree_path = req.worktree_path.clone();
        let job = if req.prunable {
            wt::DeleteJob::Prune {
                worktree_name: req.worktree_name,
                branch: req.branch,
                delete_branch: req.delete_branch,
            }
        } else {
            wt::DeleteJob::Remove {
                worktree_path: req.worktree_path,
                branch: req.branch,
                force: req.dirty.is_dirty(),
            }
        };
        let result_rx = wt::spawn_delete(project_root, job, ctx.clone());
        self.pending_deletes.push(DeleteTask {
            project_idx: req.project_idx,
            worktree_path,
            prunable: req.prunable,
            result_rx,
        });
    }

    /// Adopt finished background deletes: pop up any failure and refresh the
    /// affected project so the removed worktree (or its spinner) drops out of
    /// the sidebar.
    fn poll_pending_deletes(&mut self, ctx: &Context) {
        let mut finished: Vec<(usize, bool, Result<(), String>)> = Vec::new();
        self.pending_deletes.retain(|task| match task.result_rx.try_recv() {
            Ok(result) => {
                finished.push((task.project_idx, task.prunable, result));
                false
            },
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
        for (project_idx, prunable, result) in finished {
            if let Err(e) = result {
                let action = if prunable { "Prune" } else { "Delete" };
                self.error_dialog = Some(format!("{action} failed.\n\n{e}"));
            }
            self.refresh_project(ctx, project_idx);
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
            loop {
                match task.rx.try_recv() {
                    Ok(Progress::Step(_)) => {},
                    Ok(Progress::Done(result)) => {
                        finished.push((task.project_idx, result));
                        break false;
                    },
                    Err(mpsc::TryRecvError::Empty) => break true,
                    Err(mpsc::TryRecvError::Disconnected) => break false,
                }
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

    fn show_create_dialog(&mut self, ctx: &Context) {
        let Some(state) = self.pending_create.take() else {
            return;
        };
        let next = match state {
            CreateState::Prompt { project_idx, branch, error } => {
                self.show_create_prompt(ctx, project_idx, branch, error)
            },
            CreateState::Running { project_idx, branch, mut steps, rx } => {
                let mut done: Option<Result<PathBuf, String>> = None;
                while let Ok(p) = rx.try_recv() {
                    match p {
                        Progress::Step(s) => steps.push(s),
                        Progress::Done(r) => done = Some(r),
                    }
                }
                let minimized = self.show_create_running(ctx, project_idx, &branch, &steps);
                match done {
                    // A finished job goes to its result even if a minimize press
                    // lands on the same frame, so the outcome is never lost.
                    Some(result) => Some(CreateState::Done { project_idx, steps, result }),
                    // Minimized: hand the still-running create off to
                    // `poll_pending_creates` and dismiss the modal.
                    None if minimized => {
                        self.pending_creates.push(BackgroundCreate { project_idx, branch, rx });
                        None
                    },
                    None => Some(CreateState::Running { project_idx, branch, steps, rx }),
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
            // Whitespace runs become single hyphens — `some text like this` → `some-text-like-this`.
            let canonical: String = branch.split_whitespace().collect::<Vec<_>>().join("-");
            if let Err(msg) = wt::validate_branch_name(&canonical) {
                error = Some(msg);
                return Some(CreateState::Prompt { project_idx, branch, error });
            }
            let base_dir = self.config.workspace.base_dir_for(&project_root);
            let req =
                CreateRequest { project_root, default_branch, branch: canonical.clone(), base_dir };
            let rx = wt::spawn_create(req, ctx.clone());
            return Some(CreateState::Running {
                project_idx,
                branch: canonical,
                steps: Vec::new(),
                rx,
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
            let result = self.handle_ipc_request(ctx, call.request);
            // A send error means the client gave up waiting — nothing to do.
            let _ = call.reply_tx.send(result);
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
            Req::CreateSession { workspace } => {
                let workspace = match workspace {
                    None => None,
                    Some(p) => {
                        Some(self.known_worktree_path(&p).ok_or_else(|| unknown_worktree(&p))?)
                    },
                };
                let id = self
                    .spawn_session(ctx, workspace)
                    .map_err(|e| format!("failed to spawn shell: {e}"))?;
                Ok(json!({ "session_id": id }))
            },
            Req::CloseSession { session_id } => {
                if !self.sessions.iter().any(|s| s.id == session_id) {
                    return Err(format!("no session with id {session_id}"));
                }
                self.close_session(ctx, session_id);
                Ok(json!({ "closed": session_id }))
            },
            Req::SendText { session_id, text } => {
                let session = self
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .ok_or_else(|| format!("no session with id {session_id}"))?;
                paste::on_terminal_input_start(session);
                let bytes = text.into_bytes();
                let written = bytes.len();
                session.write(bytes);
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
            Req::RefreshProject { root } => {
                let project =
                    self.projects.iter_mut().find(|p| p.root == root).ok_or_else(|| {
                        format!("{} is not a project in the sidebar", root.display())
                    })?;
                project.refresh();
                Ok(project_json(project))
            },
            Req::AddProject { path } => Ok(project_json(self.add_project(path))),
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
        },
        "columns": session.size.columns,
        "lines": session.size.screen_lines,
        "is_active_tab": is_active_tab,
        "needs_attention": session.needs_attention,
    })
}

impl eframe::App for AlacritreeApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let bg = self.theme.terminal_bg;
        let n = |c: u8| c as f32 / 255.0;
        [n(bg.r()), n(bg.g()), n(bg.b()), self.config.window.opacity]
    }

    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_project_refreshes();
        self.poll_pending_deletes(ctx);
        self.poll_pending_creates(ctx);
        let modal_open = self.is_modal_open();
        // Keys pressed mid-composition drive the IME's candidate window,
        // not the app — alacritty's key_input returns early the same way,
        // above binding dispatch.
        if !modal_open && self.ime.preedit().is_none() {
            match self.focus {
                PaneFocus::ProjectsSidebar => self.handle_sidebar_nav(ctx),
                PaneFocus::GitSidebar => self.handle_git_sidebar_nav(ctx),
                PaneFocus::Terminal => {},
            }
            self.handle_shortcuts(ctx);
        }
        self.process_notification_actions(ctx);
        self.process_ipc_calls(ctx);
        self.process_session_events(ctx);
        let theme = self.theme;
        // GL clear is the sole source of the bg when opacity < 1; painting any
        // panel fill on top would compound the alpha through egui's blend.
        let translucent = self.config.window.opacity < 1.0;
        let sidebar_fill = if translucent { Color32::TRANSPARENT } else { theme.sidebar_bg };
        let central_fill = if translucent { Color32::TRANSPARENT } else { theme.terminal_bg };

        let panel_frame = Frame::default().fill(sidebar_fill).inner_margin(Margin::same(8));

        if self.show_left_sidebar {
            let r = self.show_project_sidebar(ctx, panel_frame.clone());
            paint_panel_border(ctx, r.right(), r.y_range(), theme.sidebar_border);
        }

        if self.show_right_sidebar {
            let r = self.show_git_sidebar(ctx, panel_frame);
            paint_panel_border(ctx, r.left(), r.y_range(), theme.sidebar_border);
        }

        egui::CentralPanel::default()
            .frame(Frame::default().fill(central_fill).inner_margin(Margin::same(0)))
            .show(ctx, |ui| {
                self.show_tab_strip(ui);

                if let Some(err) = self.last_error.as_deref() {
                    // A preedit can only be finalized or cancelled by the terminal
                    // view's event drain, so without a session view to run it the
                    // preedit would go stale and keep shortcuts suppressed forever.
                    self.ime.clear();
                    ui.label(
                        RichText::new(err)
                            .color(rgb_to_color32(self.config.palette.normal[1]))
                            .monospace(),
                    );
                    return;
                }

                if self.active_session_index().is_none() {
                    self.adopt_active_session();
                }

                let Some(idx) = self.active_session_index() else {
                    // Same rationale as the last_error branch above: without an
                    // active session view, no code path can advance the preedit.
                    self.ime.clear();
                    ui.label(
                        RichText::new("no session — Ctrl+T to open one").color(theme.text_dim),
                    );
                    return;
                };
                let session = &mut self.sessions[idx];
                let ime = &mut self.ime;
                let response = terminal_view::show(
                    ui,
                    session,
                    &self.config,
                    !modal_open && self.focus == PaneFocus::Terminal,
                    &mut self.builtin_glyphs,
                    ime,
                    &mut self.color_glyphs,
                );
                // egui fake-clicks the natively focused widget on Space/Enter,
                // and the terminal keeps native focus while the sidebar owns
                // app focus — so keyboard "clicks" must not steal it back.
                if response.clicked_by(egui::PointerButton::Primary)
                    && self.focus != PaneFocus::Terminal
                {
                    self.focus_terminal();
                }
            });

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
        if self.pending_project_remove.is_some() {
            self.show_remove_project_dialog(ctx);
        }
        if self.error_dialog.is_some() {
            self.show_error_dialog(ctx);
        }
        if self.quit_dialog_open {
            self.show_quit_dialog(ctx);
        }

        self.reap_exited_sessions(ctx);
    }
}

/// Spawn a throwaway thread so `notify-rust`'s synchronous D-Bus / WinRT
/// calls don't stall the egui paint loop.  On Linux the thread sticks around
/// for `wait_for_action` and posts the session's workspace back through
/// `NOTIFY_TX` when the user clicks the notification.
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
    let key = session.working_directory.clone();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("alacritree-notify".into())
        .spawn(move || notify_worker(body, key, ctx))
        .ok();
}

#[cfg(all(unix, not(target_os = "macos")))]
fn notify_worker(body: String, key: WorkspaceKey, ctx: egui::Context) {
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
        if let Some(lock) = NOTIFY_TX.get() {
            if let Ok(tx) = lock.lock() {
                let _ = tx.send(key.clone());
                ctx.request_repaint();
            }
        }
    });
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn notify_worker(body: String, _key: WorkspaceKey, _ctx: egui::Context) {
    // mac-notification-sys / WinRT don't expose blocking action waits via
    // notify-rust today — fall back to a fire-and-forget toast.
    if let Err(e) = notify_rust::Notification::new().summary("alacritree").body(&body).show() {
        log::debug!("desktop notification failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(p: &str) -> WorkspaceKey {
        Some(PathBuf::from(p))
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
    fn session_ids_filter_by_workspace_and_keep_spawn_order() {
        let pairs = vec![(None, 1), (ws("/a"), 2), (None, 3), (ws("/b"), 4), (ws("/a"), 5)];
        assert_eq!(sidebar_session_ids(&pairs, &None), vec![1, 3]);
        assert_eq!(sidebar_session_ids(&pairs, &ws("/a")), vec![2, 5]);
        // /b has a single session, below the two-session list threshold.
        assert!(sidebar_session_ids(&pairs, &ws("/b")).is_empty());
    }

    #[test]
    fn session_ids_empty_for_unknown_workspace() {
        let pairs = vec![(None, 1)];
        assert!(sidebar_session_ids(&pairs, &ws("/missing")).is_empty());
    }

    #[test]
    fn session_row_title_drops_glyph_the_icon_already_shows() {
        assert_eq!(session_row_title("✳ claude", Some('✳')), "claude");
        // Attention/plain rows keep the title untouched.
        assert_eq!(session_row_title("✳ claude", None), "✳ claude");
        // A static process glyph absent from the title strips nothing.
        assert_eq!(session_row_title("node build", Some('◇')), "node build");
        // Never strip down to an empty label.
        assert_eq!(session_row_title("✳ ", Some('✳')), "✳ ");
    }

    #[test]
    fn session_ids_apply_two_session_threshold() {
        let no_match: Vec<(WorkspaceKey, SessionId)> = vec![(ws("/other"), 1)];
        assert!(sidebar_session_ids(&no_match, &ws("/a")).is_empty());

        let one_match = vec![(ws("/a"), 1), (ws("/other"), 2)];
        assert!(sidebar_session_ids(&one_match, &ws("/a")).is_empty());

        let two_match = vec![(ws("/a"), 1), (ws("/other"), 2), (ws("/a"), 3)];
        assert_eq!(sidebar_session_ids(&two_match, &ws("/a")), vec![1, 3]);
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
        assert_eq!(
            args[..8],
            [
                "-d",
                "kali-linux",
                "--cd",
                r"\\wsl.localhost\kali-linux\home\lev\proj",
                "--exec",
                "sh",
                "-c",
                r#"export LESS="${LESS-R}"; exec git -c "core.pager=/home/lev/.cargo/bin/delta --paging=always" "$@""#,
            ]
        );
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
        assert_eq!(
            args[..7],
            [
                "-d",
                "kali-linux",
                "--cd",
                r"\\wsl.localhost\kali-linux\home\lev\proj",
                "--exec",
                "sh",
                "-c"
            ]
        );
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
}
