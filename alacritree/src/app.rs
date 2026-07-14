use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use eframe::CreationContext;
use egui::{Color32, Context, Frame, Margin, RichText, ScrollArea, SidePanel, Stroke};

use serde_json::{Value, json};

use crate::bindings::{BindingAction, NamedAction};
use crate::clipboard::{self, Target};
use crate::colors::rgb_to_color32;
use crate::config::Config;
use crate::doppler;
use crate::git_status::{self, ChangeKind, DirtyCounts, FileChange, StatusCache};
use crate::ipc;
use crate::paste;
use crate::pr_status::PrCache;
use crate::projects::{Project, Worktree};
use crate::session::{Session, SessionId, SessionKind, TermSize};
use crate::sidebar_nav::{self, SidebarRow};
use crate::state::{self, PersistedProject, PersistedState};
use crate::terminal_view;
use crate::worktree::{self as wt, CreateRequest, Progress};

/// `None` is the home workspace (sessions inherit `$PWD`); `Some` is a worktree path.
type WorkspaceKey = Option<PathBuf>;

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
    ctx.layer_painter(layer).vline(x, y_range, Stroke::new(1.0, color));
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
}

pub struct AlacritreeApp {
    show_left_sidebar: bool,
    show_right_sidebar: bool,
    focus: PaneFocus,
    sidebar_cursor: Option<SidebarRow>,
    /// The focus toggle opened a hidden sidebar; returning focus closes it
    /// again so a keyboard round trip leaves the layout untouched.
    sidebar_auto_shown: bool,
    /// One-shot: scroll the cursor row into view on the next sidebar paint.
    sidebar_cursor_moved: bool,
    sessions: Vec<Session>,
    current_workspace: WorkspaceKey,
    active_session: HashMap<WorkspaceKey, SessionId>,
    projects: Vec<Project>,
    git_status: HashMap<PathBuf, StatusCache>,
    pr_cache: PrCache,
    config: Config,
    theme: Theme,
    last_error: Option<String>,
    quit_dialog_open: bool,
    pending_delete: Option<DeleteRequest>,
    pending_create: Option<CreateState>,
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
}

struct DeleteRequest {
    project_idx: usize,
    worktree_path: PathBuf,
    worktree_name: String,
    branch: Option<String>,
    dirty: DirtyCounts,
}

enum CreateState {
    Prompt { project_idx: usize, branch: String, error: Option<String> },
    Running { project_idx: usize, branch: String, steps: Vec<String>, rx: Receiver<Progress> },
    Done { project_idx: usize, steps: Vec<String>, result: Result<PathBuf, String> },
}

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

impl AlacritreeApp {
    pub fn new(cc: &CreationContext<'_>, config: Config) -> Self {
        let theme = Theme::from_config(&config);

        crate::fonts::install_terminal_fonts(
            &cc.egui_ctx,
            &config.font.normal,
            &config.font.bold,
            &config.font.italic,
            &config.font.bold_italic,
        );

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
        let projects = persisted
            .projects
            .iter()
            .map(|p| {
                let mut project = Project::discover(p.root.clone());
                project.expanded = p.expanded;
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
            sidebar_auto_shown: false,
            sidebar_cursor_moved: false,
            sessions: Vec::new(),
            current_workspace: None,
            active_session: HashMap::new(),
            projects,
            git_status: HashMap::new(),
            pr_cache: PrCache::new(),
            config,
            theme,
            last_error: None,
            quit_dialog_open: false,
            pending_delete: None,
            pending_create: None,
            doppler_synced: HashSet::new(),
            pending_session_close: None,
            notify_rx,
            ipc_rx,
            _ipc_socket: ipc_socket,
            builtin_glyphs: crate::builtin_font::BuiltinGlyphCache::new(),
            ime: crate::ime::Ime::default(),
        };

        if let Err(e) = app.spawn_session(&cc.egui_ctx, None) {
            app.last_error = Some(format!("failed to spawn shell: {e}"));
        }

        app
    }

    fn persist(&self) {
        let state = PersistedState {
            projects: self
                .projects
                .iter()
                .map(|p| PersistedProject { root: p.root.clone(), expanded: p.expanded })
                .collect(),
            // Don't persist a sidebar the user never opened — an auto-shown
            // sidebar (e.g. from Ctrl+Shift+B while it was hidden) should not
            // reappear on next launch.
            show_left_sidebar: self.show_left_sidebar && !self.sidebar_auto_shown,
            show_right_sidebar: self.show_right_sidebar,
        };
        state::save(&state);
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
        let session = Session::spawn(
            ctx.clone(),
            &self.config,
            working_directory.clone(),
            TermSize::new(80, 24),
            (8.0, 16.0),
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

    fn activate_worktree(&mut self, ctx: &Context, path: &Path) {
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
        let ws_idx = self.workspace_session_indices(&self.current_workspace);
        if let Some(&idx) = ws_idx.first() {
            let id = self.sessions[idx].id;
            self.active_session.insert(self.current_workspace.clone(), id);
            return;
        }
        if let Err(e) = self.spawn_session(ctx, self.current_workspace.clone()) {
            self.last_error = Some(format!("failed to spawn shell: {e}"));
        }
    }

    fn close_session(&mut self, id: SessionId) {
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
                    self.active_session.insert(workspace, new_id);
                },
                None => {
                    self.active_session.remove(&workspace);
                },
            }
        }
    }

    fn request_close_session(&mut self, id: SessionId) {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        if self.config.ui.confirm_session_close.requires_prompt(session.is_busy()) {
            self.pending_session_close = Some(id);
        } else {
            self.close_session(id);
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
                order.push(Some(wt.path.clone()));
            }
        }
        order
    }

    fn add_project_via_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            if !self.projects.iter().any(|p| p.root == path) {
                self.projects.push(Project::discover(path));
                self.persist();
            }
        }
    }

    fn is_modal_open(&self) -> bool {
        self.quit_dialog_open
            || self.pending_delete.is_some()
            || self.pending_create.is_some()
            || self.pending_session_close.is_some()
    }

    fn focus_sidebar(&mut self) {
        if !self.show_left_sidebar {
            self.show_left_sidebar = true;
            self.sidebar_auto_shown = true;
            self.persist();
        }
        self.focus = PaneFocus::ProjectsSidebar;
        self.sidebar_cursor =
            Some(sidebar_nav::seed(&self.projects, self.current_workspace.as_deref()));
        self.sidebar_cursor_moved = true;
    }

    fn focus_terminal(&mut self) {
        self.focus = PaneFocus::Terminal;
        if self.sidebar_auto_shown {
            self.show_left_sidebar = false;
            self.sidebar_auto_shown = false;
            self.persist();
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
        use egui::Key;
        let keys: Vec<Key> = ctx.input_mut(|i| {
            let mut pressed = Vec::new();
            i.events.retain(|ev| {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                    if modifiers.is_none() && is_sidebar_nav_key(*key) {
                        pressed.push(*key);
                        return false;
                    }
                }
                true
            });
            pressed
        });
        for key in keys {
            self.apply_sidebar_nav(ctx, key);
        }
    }

    fn apply_sidebar_nav(&mut self, ctx: &Context, key: egui::Key) {
        use egui::Key;
        let rows = sidebar_nav::visible_rows(&self.projects);
        let cursor = match self.sidebar_cursor.clone() {
            Some(c) if rows.contains(&c) => c,
            // Stale or unseeded cursor (worktree removed, project collapsed
            // by mouse): land on Home and let the next press act from there.
            _ => {
                self.set_sidebar_cursor(SidebarRow::Home);
                return;
            },
        };
        match key {
            Key::ArrowUp => self.set_sidebar_cursor(sidebar_nav::step(&rows, &cursor, -1)),
            Key::ArrowDown => self.set_sidebar_cursor(sidebar_nav::step(&rows, &cursor, 1)),
            Key::ArrowRight => {
                if let SidebarRow::Project(root) = &cursor {
                    self.set_project_expanded(root, true);
                }
            },
            Key::ArrowLeft => match &cursor {
                SidebarRow::Project(root) => self.set_project_expanded(root, false),
                SidebarRow::Worktree(_) => {
                    if let Some(target) = sidebar_nav::left_target(&rows, &cursor) {
                        self.set_sidebar_cursor(target);
                    }
                },
                SidebarRow::Home => {},
            },
            Key::Enter => match &cursor {
                SidebarRow::Home => {
                    self.activate_home(ctx);
                    self.focus_terminal();
                },
                SidebarRow::Worktree(path) => {
                    let path = path.clone();
                    self.activate_worktree(ctx, &path);
                    self.focus_terminal();
                },
                SidebarRow::Project(root) => {
                    let root = root.clone();
                    let expanded =
                        self.projects.iter().find(|p| p.root == root).is_some_and(|p| p.expanded);
                    self.set_project_expanded(&root, !expanded);
                },
            },
            Key::Escape => self.focus_terminal(),
            _ => {},
        }
    }

    fn set_sidebar_cursor(&mut self, row: SidebarRow) {
        if self.sidebar_cursor.as_ref() != Some(&row) {
            self.sidebar_cursor = Some(row);
            self.sidebar_cursor_moved = true;
        }
    }

    fn set_project_expanded(&mut self, root: &Path, expanded: bool) {
        if let Some(p) = self.projects.iter_mut().find(|p| p.root == *root) {
            if p.expanded != expanded {
                p.expanded = expanded;
                self.persist();
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
                self.persist();
            },
            BindingAction::Named(NamedAction::ToggleRightSidebar) => {
                self.show_right_sidebar = !self.show_right_sidebar;
                self.persist();
            },
            BindingAction::Named(NamedAction::SelectNextWorkspace) => {
                self.cycle_workspaces(ctx, 1);
            },
            BindingAction::Named(NamedAction::SelectPreviousWorkspace) => {
                self.cycle_workspaces(ctx, -1);
            },
            BindingAction::Named(NamedAction::AddProject) => self.add_project_via_dialog(),
            BindingAction::Named(NamedAction::ToggleSidebarFocus) => match self.focus {
                PaneFocus::Terminal => self.focus_sidebar(),
                PaneFocus::ProjectsSidebar => self.focus_terminal(),
            },
            BindingAction::Named(NamedAction::FocusProjectsSidebar) => {
                if self.focus != PaneFocus::ProjectsSidebar {
                    self.focus_sidebar();
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
        if indices.len() < 2 {
            ui.add_space(2.0);
            return;
        }
        let active_idx = self.active_session_index();

        // Reserve a 2px-tall strip across the full width of the terminal pane.
        let strip_height = 2.0;
        let gap = 4.0;
        let avail = ui.available_width();
        let segment_width =
            ((avail - gap * (indices.len() as f32 - 1.0)) / indices.len() as f32).max(1.0);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(avail, strip_height + 2.0), egui::Sense::hover());

        let mut activate: Option<SessionId> = None;
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

        if let Some(id) = activate {
            self.set_active_in_current_workspace(id);
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
        let mut add_project_clicked = false;
        let mut refresh_idx: Option<usize> = None;
        let mut remove_idx: Option<usize> = None;
        let mut expand_toggled = false;
        let mut home_clicked = false;
        let theme = self.theme;
        let cursor_row = if self.focus == PaneFocus::ProjectsSidebar {
            self.sidebar_cursor.clone()
        } else {
            None
        };
        let cursor_moved = std::mem::take(&mut self.sidebar_cursor_moved);

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

        let panel_resp = SidePanel::left("left_sidebar")
            .resizable(true)
            .default_width(240.0 * theme.ui_scale)
            .min_width(180.0 * theme.ui_scale)
            .frame(panel_frame)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Projects").color(theme.text).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, "+", theme.text_dim, &theme)
                            .on_hover_text("add project")
                            .clicked()
                        {
                            add_project_clicked = true;
                        }
                    });
                });
                ui.separator();

                ScrollArea::vertical().show(ui, |ui| {
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
                        let act = session_row(ui, row, &theme);
                        if act.activate {
                            activate_session_request.set(Some((None, row.id)));
                        }
                        if act.close {
                            close_session_request.set(Some(row.id));
                        }
                    }
                    ui.add_space(2.0);

                    if self.projects.is_empty() {
                        ui.label(
                            RichText::new("Click + to add a project.")
                                .color(theme.text_dim)
                                .small(),
                        );
                        ui.add_space(4.0);
                        ui.label(RichText::new("Ctrl+B to toggle").small().color(theme.text_muted));
                    }

                    for (idx, project) in self.projects.iter_mut().enumerate() {
                        let proj_attention = project_attention.get(idx).copied().unwrap_or(false);
                        // Bubble attention up to the project row only when the
                        // project is collapsed — once expanded, the actual
                        // worktree rows already show the dot, and doubling it
                        // on the parent reads as noise.
                        let show_proj_dot = proj_attention && !project.expanded;
                        let row_rect = row_with_trailing(
                            ui,
                            |ui| {
                                let arrow = if project.expanded { "▾" } else { "▸" };
                                if icon_button(ui, arrow, theme.text_dim, &theme).clicked() {
                                    project.expanded = !project.expanded;
                                    expand_toggled = true;
                                }
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&project.name)
                                            .color(theme.text)
                                            .strong()
                                            .small(),
                                    )
                                    .truncate(),
                                );
                            },
                            |ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if icon_button(ui, "×", theme.text_muted, &theme)
                                    .on_hover_text("remove from sidebar")
                                    .clicked()
                                {
                                    remove_idx = Some(idx);
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

                        if project.expanded {
                            for (wt_idx, wt) in project.worktrees.iter().enumerate() {
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
                                let action = worktree_row(
                                    ui,
                                    wt,
                                    is_active,
                                    is_cursor,
                                    cursor_moved,
                                    wt_attention,
                                    wt_glyph,
                                    &theme,
                                );
                                if action.activate {
                                    activate_request.set(Some(wt.path.clone()));
                                }
                                if action.delete {
                                    delete_request.set(Some(DeleteRequest {
                                        project_idx: idx,
                                        worktree_path: wt.path.clone(),
                                        worktree_name: wt.name.clone(),
                                        branch: wt.branch.clone(),
                                        dirty: git_status::dirty_counts(&wt.path),
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
                                    let act = session_row(ui, row, &theme);
                                    if act.activate {
                                        activate_session_request
                                            .set(Some((Some(wt.path.clone()), row.id)));
                                    }
                                    if act.close {
                                        close_session_request.set(Some(row.id));
                                    }
                                }
                            }
                            ui.add_space(4.0);
                        }
                    }
                });
            });

        if add_project_clicked {
            self.add_project_via_dialog();
        }
        if let Some(idx) = refresh_idx {
            self.projects[idx].refresh();
        }
        if let Some(idx) = remove_idx {
            self.projects.remove(idx);
            self.persist();
        }
        if expand_toggled {
            self.persist();
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
            // active_session_index() misses and ensure_active_session picks
            // an existing shell or spawns one.
            self.current_workspace = ws.clone();
            self.active_session.insert(ws, id);
        }
        if let Some(id) = close_session_request.take() {
            self.request_close_session(id);
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
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Git").color(theme.text).strong());
                });
                ui.separator();

                let path = match self.active_session_path() {
                    Some(p) => p,
                    None => {
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
                // changed since the last completed compute.
                let status = cache.poll(effective_default.as_deref(), ctx);

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
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("on").color(theme.text_muted).small());
                            ui.add(
                                egui::Label::new(
                                    RichText::new(branch).color(theme.accent).small().strong(),
                                )
                                .truncate(),
                            );
                            if let Some(default) = &status.default_branch {
                                if Some(branch) != Some(default) {
                                    ui.label(RichText::new("vs").color(theme.text_muted).small());
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(default).color(theme.text_dim).small(),
                                        )
                                        .truncate(),
                                    );
                                }
                            }
                        });
                    }
                    ui.add_space(10.0);

                    section(ui, &theme, "Staged", status.staged.len(), |ui| {
                        for f in &status.staged {
                            let req =
                                DiffRequest { file: f.path.clone(), source: DiffSource::Staged };
                            let is_active = active_diff_key.as_deref() == Some(&diff_key(&req));
                            if file_row(ui, f, &theme, &palette, is_active).clicked() {
                                diff_request.set(Some(req));
                            }
                        }
                    });

                    section(ui, &theme, "Unstaged", status.unstaged.len(), |ui| {
                        for f in &status.unstaged {
                            let source = if f.kind == ChangeKind::Untracked {
                                DiffSource::Untracked
                            } else {
                                DiffSource::Worktree
                            };
                            let req = DiffRequest { file: f.path.clone(), source };
                            let is_active = active_diff_key.as_deref() == Some(&diff_key(&req));
                            if file_row(ui, f, &theme, &palette, is_active).clicked() {
                                diff_request.set(Some(req));
                            }
                        }
                    });

                    if !status.branch_diff.is_empty() {
                        let base_label = match &status.default_branch {
                            Some(b) => format!("Changes vs {b}"),
                            None => "Changes vs default".to_string(),
                        };
                        // Prefer the resolved ref (e.g. `refs/remotes/origin/main`) so the
                        // sidebar's merge-base diff matches what delta will show.
                        let base = status
                            .default_branch_resolved
                            .clone()
                            .or_else(|| status.default_branch.clone());
                        let count = status.branch_diff.len();

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
                            ui.label(
                                RichText::new(format!("{count}")).color(theme.text_muted).small(),
                            );
                        });
                        ui.add_space(2.0);
                        for stat in &status.branch_diff {
                            let Some(base) = base.clone() else {
                                branch_diff_row(ui, stat, &theme, &palette, false);
                                continue;
                            };
                            let req = DiffRequest {
                                file: stat.path.clone(),
                                source: DiffSource::Branch { base },
                            };
                            let is_active = active_diff_key.as_deref() == Some(&diff_key(&req));
                            if branch_diff_row(ui, stat, &theme, &palette, is_active).clicked() {
                                diff_request.set(Some(req));
                            }
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
        let already_showing = self.sessions.iter().any(|s| {
            s.working_directory.as_deref() == Some(&workspace)
                && matches!(&s.kind, SessionKind::Diff { key } if key == &new_key)
        });
        self.sessions.retain(|s| {
            !(matches!(s.kind, SessionKind::Diff { .. })
                && s.working_directory.as_deref() == Some(&workspace))
        });
        if already_showing {
            // Active-session fallback to the workspace's shell happens next
            // frame: `active_session_index()` returns None for the stale id, and
            // `ensure_active_session` picks up an existing shell or spawns one.
            return;
        }

        let (program, args) = build_diff_command(&req);
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

/// Show the clicked file's `git diff` in delta, wired in as git's pager so git
/// drives the pipe itself.  This drops the POSIX-`sh` dependency the old
/// `sh -c '… | delta'` had — which had no equivalent on Windows, so diffs never
/// opened there.  Paths/branches stay in argv, so no file name is shell-parsed.
fn build_diff_command(req: &DiffRequest) -> (String, Vec<String>) {
    let mut args =
        vec!["-c".to_string(), "core.pager=delta --paging=always".to_string(), "diff".to_string()];
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
    ("git".to_string(), args)
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
        .stroke(Stroke::new(1.0, theme.sidebar_border))
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

/// Render a collapsed-when-empty git section.
///
/// Empty sections are skipped entirely — a placeholder glyph for "no files
/// here" added visual noise without communicating anything the count badge
/// didn't already say.
fn section<R>(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: &str,
    count: usize,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) {
    if count == 0 {
        return;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).color(theme.text).strong().small());
        ui.label(RichText::new(format!("{count}")).color(theme.text_muted).small());
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

/// Lay out a row whose `trailing` widgets pin to the right edge while `leading`
/// fills the remaining width — so a `Label::truncate()` inside `leading` knows
/// exactly how much space it has and ellipsizes cleanly when the panel is narrow.
///
/// The row is pre-sized to `interact_size.y` (mirroring `Ui::horizontal`'s own
/// internals) so it doesn't claim the parent's full remaining height when nested
/// in a vertical layout — without this, `Align::Center` would push the row's
/// content to the middle of the column and leave a giant gap before the next row.
/// Frameless, fixed-footprint icon button. Painter-drawn rather than a
/// `Button` because `Button` lays text out from the top-left of its rect, so
/// glyphs of different intrinsic heights (e.g. `+` vs `↻`) end up on different
/// baselines. `painter.text` with `CENTER_CENTER` centers the galley in the
/// rect, giving real grid alignment.
/// Painted (rather than `RichText("●")`) so its size is independent of font
/// metrics — `RichText("●")` renders inconsistently across fallback fonts.
fn attention_dot(ui: &mut egui::Ui, theme: &Theme) {
    let s = theme.ui_scale;
    let size = egui::vec2(10.0 * s, 14.0 * s);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let radius = 3.0 * s;
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
    ui.label(RichText::new(glyph).color(color).size(10.0 * s));
}

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

fn row_with_trailing<L, T>(ui: &mut egui::Ui, leading: L, trailing: T) -> egui::Rect
where
    L: FnOnce(&mut egui::Ui),
    T: FnOnce(&mut egui::Ui),
{
    let row_size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(row_size, egui::Layout::right_to_left(egui::Align::Center), |ui| {
        trailing(ui);
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
        egui::Stroke::new(1.0, theme.accent),
        egui::StrokeKind::Inside,
    );
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

fn worktree_row(
    ui: &mut egui::Ui,
    wt: &Worktree,
    is_active: bool,
    is_cursor: bool,
    scroll_into_view: bool,
    attention: bool,
    agent_glyph: Option<char>,
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
            let name_color = if is_active { theme.text } else { theme.text_dim };
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
                    if !wt.is_main {
                        let btn = icon_button(ui, "×", theme.text_muted, theme)
                            .on_hover_text("delete worktree and branch");
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
        activate: resp.clicked() && !delete_clicked && !spawn_clicked,
        delete: delete_clicked,
        spawn: spawn_clicked,
    }
}

struct SessionRowAction {
    activate: bool,
    close: bool,
}

fn session_row(ui: &mut egui::Ui, row: &SessionRowData, theme: &Theme) -> SessionRowAction {
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
    if bg != Color32::TRANSPARENT {
        let rect = egui::Rect::from_x_y_ranges(panel_x, resp.rect.y_range());
        ui.painter().set(bg_idx, egui::Shape::rect_filled(rect, 0.0, bg));
    }
    SessionRowAction { activate: resp.clicked() && !close_clicked, close: close_clicked }
}

impl AlacritreeApp {
    fn reap_exited_sessions(&mut self) {
        let exited_ids: Vec<SessionId> =
            self.sessions.iter().filter(|s| s.is_exited()).map(|s| s.id).collect();
        for id in exited_ids {
            self.close_session(id);
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
        let Some(req) = self.pending_delete.as_ref() else {
            return;
        };
        let title = format!("Delete worktree `{}`?", req.worktree_name);
        let detail = match &req.branch {
            Some(b) => format!("Removes the worktree directory and deletes branch `{b}`."),
            None => "Removes the worktree directory.".to_string(),
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
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to delete · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let delete = ui.add(
                            egui::Button::new(RichText::new("Delete").color(danger)).frame(false),
                        );
                        if delete.clicked() {
                            confirmed = true;
                        }
                        let cancel = ui.add(
                            egui::Button::new(RichText::new("Cancel").color(theme.text_dim))
                                .frame(false),
                        );
                        if cancel.clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), delete.id);
                    });
                });
            },
        );

        if confirm_via_key || confirmed {
            self.run_pending_delete();
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
                        let close_btn = ui.add(
                            egui::Button::new(RichText::new("Close").color(danger)).frame(false),
                        );
                        if close_btn.clicked() {
                            confirmed = true;
                        }
                        let cancel = ui.add(
                            egui::Button::new(RichText::new("Cancel").color(theme.text_dim))
                                .frame(false),
                        );
                        if cancel.clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), close_btn.id);
                    });
                });
            });

        if confirm_via_key || confirmed {
            self.pending_session_close = None;
            self.close_session(id);
            return;
        }
        if cancel_via_key || cancelled || modal.should_close() {
            self.pending_session_close = None;
        }
    }

    fn run_pending_delete(&mut self) {
        let Some(req) = self.pending_delete.take() else {
            return;
        };
        let project_root = self.projects[req.project_idx].root.clone();

        // Drop sessions whose cwd is the worktree before deleting it; the PTY
        // would otherwise block the directory removal on some filesystems.
        self.sessions.retain(|s| s.working_directory.as_deref() != Some(&req.worktree_path));
        if self.current_workspace.as_deref() == Some(&req.worktree_path) {
            self.current_workspace = None;
        }
        self.active_session.remove(&Some(req.worktree_path.clone()));

        let force = req.dirty.is_dirty();
        if let Err(e) =
            wt::delete_worktree(&project_root, &req.worktree_path, req.branch.as_deref(), force)
        {
            self.last_error = Some(format!("delete failed: {e}"));
        }
        self.projects[req.project_idx].refresh();
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
                self.show_create_running(ctx, project_idx, &branch, &steps);
                match done {
                    Some(result) => Some(CreateState::Done { project_idx, steps, result }),
                    None => Some(CreateState::Running { project_idx, branch, steps, rx }),
                }
            },
            CreateState::Done { project_idx, steps, result } => {
                if self.show_create_done(ctx, project_idx, &steps, &result) {
                    if let Ok(path) = &result {
                        self.projects[project_idx].refresh();
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
        let project_name = self.projects[project_idx].name.clone();
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
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Create").color(theme.accent))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            create_clicked = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Cancel").color(theme.text_dim))
                                    .frame(false),
                            )
                            .clicked()
                        {
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
            let req = CreateRequest { project_root, default_branch, branch: canonical.clone() };
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

    fn show_create_running(
        &self,
        ctx: &Context,
        project_idx: usize,
        branch: &str,
        steps: &[String],
    ) {
        let theme = self.theme;
        let project_name = self.projects[project_idx].name.clone();
        let frame = modal_frame(&theme);
        let s = theme.ui_scale;
        let _ = egui::Modal::new(egui::Id::new("alacritree_create_dialog")).frame(frame).show(
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
            },
        );
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
        let project_name = self.projects[project_idx].name.clone();
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
                    let btn = ui.add(
                        egui::Button::new(RichText::new(label).color(theme.accent)).frame(false),
                    );
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
                        let quit_id = egui::Id::new("alacritree_quit_btn");
                        let quit = ui.add(
                            egui::Button::new(RichText::new("Quit").color(danger)).frame(false),
                        );
                        if quit.clicked() {
                            quit_clicked = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Cancel").color(theme.text_dim))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            cancel_clicked = true;
                        }
                        focus_default(ui.ctx(), quit_id);
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
                self.close_session(session_id);
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

fn project_json(project: &Project) -> Value {
    json!({
        "name": project.name,
        "root": project.root,
        "default_branch": project.default_branch,
        "worktrees": project
            .worktrees
            .iter()
            .map(|wt| json!({
                "name": wt.name,
                "path": wt.path,
                "branch": wt.branch,
                "is_main": wt.is_main,
            }))
            .collect::<Vec<_>>(),
    })
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
        let modal_open = self.is_modal_open();
        // Keys pressed mid-composition drive the IME's candidate window,
        // not the app — alacritty's key_input returns early the same way,
        // above binding dispatch.
        if !modal_open && self.ime.preedit().is_none() {
            if self.focus == PaneFocus::ProjectsSidebar {
                self.handle_sidebar_nav(ctx);
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
                    self.ensure_active_session(ctx);
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
        if self.quit_dialog_open {
            self.show_quit_dialog(ctx);
        }

        self.reap_exited_sessions();
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
}
