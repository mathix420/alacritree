//! Read user configuration from alacritty.toml + alacritree.toml.
//!
//! `alacritty.toml` is alacritty's own config — we share the file so the user
//! gets matching colors/cursor in both terminals.  `alacritree.toml` lives in
//! the same directory and overrides anything in `alacritty.toml` via a
//! deep-merge.  alacritree-specific options live under the `[ui]` (sidebar
//! colors, etc.) and `[workspace]` (worktree location) tables and are only
//! valid in `alacritree.toml`.
//!
//! Binding actions that only exist in alacritree (`ToggleLeftSidebar`,
//! `SelectNextWorkspace`, `AddProject`, …) belong in `alacritree.toml` too:
//! real alacritty warns about unknown actions if it sees them in the shared
//! `alacritty.toml`, and the array-concatenating merge means bindings placed
//! in `alacritree.toml` still add to (never clobber) the shared ones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Rgb};
use egui::Color32;
use serde::Deserialize;

use crate::bindings::{self, KeyBinding};
use crate::path_style::PathStyle;

#[derive(Debug, Clone)]
pub struct Config {
    pub palette: Palette,
    pub ui: UiTheme,
    pub ui_font: UiFont,
    pub workspace: WorkspaceConfig,
    pub font: FontConfig,
    pub cursor: CursorConfig,
    pub scrolling: ScrollingConfig,
    pub window: WindowConfig,
    pub env: HashMap<String, String>,
    pub shell: Option<ShellConfig>,
    pub selection: SelectionConfig,
    pub bindings: Vec<KeyBinding>,
    /// Offer the IPC socket that `alacritree mcp` connects to.  Mirrors
    /// alacritty's `[general] ipc_socket` (default on).
    pub ipc_socket: bool,
    /// Start dir for sessions with no explicit workspace (the home tab);
    /// worktree tabs always use their checkout path.  Mirrors alacritty's
    /// `[general] working_directory`, except a leading `~` expands to the
    /// home directory (upstream only expands `~` in config imports) so one
    /// shared config works on every platform.
    pub working_directory: Option<PathBuf>,
    pub wsl_automount_root: String,
    pub wsl_resident_helper: bool,
    /// Explicit `delta` program for the diff pane, from `[ui] delta_path`.
    /// When set it is used verbatim in git's `core.pager` on every platform
    /// and skips WSL delta autodiscovery; when unset, native diffs run bare
    /// `delta` (from PATH) and WSL diffs discover it inside the distro.
    pub delta_path: Option<String>,
    pub profiles: Vec<Profile>,
    /// Validated at load: always names an entry in `profiles` when `Some`.
    pub default_profile: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FontConfig {
    pub size: f32,
    pub normal: FontFace,
    pub bold: FontFace,
    pub italic: FontFace,
    pub bold_italic: FontFace,
    /// Extra spacing per cell, mirroring alacritty's `font.offset`.  Added to
    /// the per-cell width/height after the egui glyph metrics have been
    /// floored to whole device pixels.
    pub offset: FontDelta,
    /// Pixel offset applied when painting glyphs inside the cell, mirroring
    /// alacritty's `font.glyph_offset`.  Built-in glyphs deliberately ignore
    /// this offset (they already align to the cell), matching alacritty.
    pub glyph_offset: FontDelta,
    /// When true, render box drawing / block / Powerline / Symbols-for-Legacy-
    /// Computing characters from the built-in renderer instead of the font.
    /// Default `true` matches alacritty.
    pub builtin_box_drawing: bool,
    /// Ordered fallback families or font file paths, consulted after the four
    /// primary faces and before the automatic system fallback chain.
    pub fallback: Vec<String>,
    /// Draw emoji from their font's colour tables.  Turning this off falls
    /// through to the first fallback face that has ordinary outlines, so
    /// emoji render monochrome rather than in colour.
    pub color_glyphs: bool,
    /// Ceiling on the rasterized colour glyph cache.  The cache is already
    /// bounded by how many codepoints the colour fonts cover (a few thousand),
    /// but that ceiling moves with cell size and with the fallback list, so it
    /// is worth a budget rather than a promise.
    pub color_glyph_cache_mb: usize,
}

/// Pixel delta with x/y, mirroring alacritty's `Delta<i8>` for `font.offset`
/// and `font.glyph_offset`.  Kept as `i8` because that's the type alacritty's
/// schema accepts and going wider would silently lose round-trip equivalence.
#[derive(Debug, Clone, Copy, Default)]
pub struct FontDelta {
    pub x: i8,
    pub y: i8,
}

impl FontConfig {
    /// Sidebar/modal titles use this fraction of the terminal font size.
    /// Headings stay close to the grid's size for visual weight without
    /// crowding the chrome.
    pub const UI_HEADING_RATIO: f32 = 0.9;

    /// Normal UI text (rows, captions, button labels) is this fraction of the
    /// terminal font size so the chrome reads as secondary to the grid.
    pub const UI_NORMAL_RATIO: f32 = 0.8;

    /// Convert the user-configured size (typographic points, matching
    /// alacritty's `font.size`) into the logical-pixel value egui's `FontId`
    /// expects.  Without this step egui treats the number as logical pixels
    /// and renders 25% smaller than alacritty for the same config value.
    pub fn egui_size(&self) -> f32 {
        self.size * 96.0 / 72.0
    }

    /// Logical-pixel size for sidebar/modal titles.
    pub fn ui_heading_px(&self) -> f32 {
        self.egui_size() * Self::UI_HEADING_RATIO
    }

    /// Logical-pixel size for the dominant non-heading UI text.
    pub fn ui_normal_px(&self) -> f32 {
        self.egui_size() * Self::UI_NORMAL_RATIO
    }
}

/// A single weight/style face.  `family` mirrors alacritty's `[font.*].family`;
/// `style` mirrors `[font.*].style` (e.g. "Bold", "Italic", "Bold Italic"), and
/// is used both as a hint to the font matcher and to disambiguate faces that
/// only differ by style within a family.
#[derive(Debug, Clone, Default)]
pub struct FontFace {
    pub family: Option<String>,
    pub style: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct CursorConfig {
    pub shape: CursorShape,
    pub blinking: bool,
    pub unfocused_hollow: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollingConfig {
    pub history: usize,
    pub multiplier: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct WindowConfig {
    pub padding_x: f32,
    pub padding_y: f32,
    pub opacity: f32,
}

#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub program: String,
    pub args: Vec<String>,
}

/// A named shell launch profile from `[[ui.profiles]]`.  Program + args
/// only; cwd and env come from the session as usual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SelectionConfig {
    pub semantic_escape_chars: String,
    /// Mirror auto-copy of selections to the regular clipboard.  Off by default
    /// (matches alacritty); when off, drag-select still writes to the X11
    /// PRIMARY / Wayland primary-selection buffer for middle-click paste.
    pub save_to_clipboard: bool,
}

impl Config {
    pub fn cursor_style(&self) -> CursorStyle {
        CursorStyle { shape: self.cursor.shape, blinking: self.cursor.blinking }
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub bright_fg: Option<Rgb>,
    pub dim_fg: Option<Rgb>,
    pub cursor_fg: Option<Rgb>,
    pub cursor_bg: Option<Rgb>,
    pub selection_bg: Option<Rgb>,
    pub selection_fg: Option<Rgb>,
    pub normal: [Rgb; 8],
    pub bright: [Rgb; 8],
    pub dim: Option<[Rgb; 8]>,
    pub indexed: Vec<(u8, Rgb)>,
    pub draw_bold_with_bright: bool,
}

/// When the sidebar's per-session `×` asks before killing the PTY.
/// Confirmations otherwise exist only at worktree/app level, so the
/// default keeps session close immediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfirmSessionClose {
    #[default]
    Never,
    /// Prompt only when the session looks busy (running process, agent
    /// glyph, or spinner title).
    Busy,
    Always,
}

impl ConfirmSessionClose {
    pub fn requires_prompt(self, busy: bool) -> bool {
        match self {
            Self::Never => false,
            Self::Busy => busy,
            Self::Always => true,
        }
    }
}

fn parse_confirm_session_close(raw: Option<&str>) -> ConfirmSessionClose {
    match raw {
        None => ConfirmSessionClose::default(),
        Some("never") => ConfirmSessionClose::Never,
        Some("busy") => ConfirmSessionClose::Busy,
        Some("always") => ConfirmSessionClose::Always,
        Some(other) => {
            log::warn!("unknown ui.confirm_session_close value {other:?}, using \"never\"");
            ConfirmSessionClose::default()
        },
    }
}

/// How the sidebar scroll areas draw their scrollbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarStyle {
    /// egui's default: a thin bar overlaying the content edge, expanding on
    /// hover — which covers the icons at the right end of sidebar rows.
    #[default]
    Floating,
    /// A reserved gutter right of the content; the bar never covers icons.
    Solid,
}

fn parse_scrollbar(raw: Option<&str>) -> ScrollbarStyle {
    match raw {
        None => ScrollbarStyle::default(),
        Some("floating") => ScrollbarStyle::Floating,
        Some("solid") => ScrollbarStyle::Solid,
        Some(other) => {
            log::warn!("unknown ui.scrollbar value {other:?}, using \"floating\"");
            ScrollbarStyle::default()
        },
    }
}

fn parse_path_style(raw: Option<&str>) -> PathStyle {
    match raw {
        None => PathStyle::default(),
        Some("full") => PathStyle::Full,
        Some("fish") => PathStyle::Fish,
        Some("zed") => PathStyle::Zed,
        Some(other) => {
            log::warn!("unknown ui.path_style value {other:?}, using \"full\"");
            PathStyle::default()
        },
    }
}

fn text_emphasis(raw: &RawTextEmphasis) -> TextEmphasis {
    TextEmphasis {
        color: raw.color.map(|v| rgb_to_color32(v.0)),
        bold: raw.bold.unwrap_or(false),
        italic: raw.italic.unwrap_or(false),
    }
}

/// Text-presentation magnifier (U+2315).  Not in egui's bundled fonts; it
/// resolves through the system fallback chain `fonts.rs` registers.
const DEFAULT_SEARCH_ICON: &str = "⌕";

/// What happens when the on-screen workspace's last session closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LastSessionClose {
    /// Recycle a shell in place — the workspace always has a live session,
    /// so the last session is by design unclosable.
    #[default]
    Respawn,
    /// Move to the project's main checkout when it has a live session,
    /// otherwise home (which spawns a shell only if it has none).
    Navigate,
}

fn parse_last_session_close(raw: Option<&str>) -> LastSessionClose {
    match raw {
        None => LastSessionClose::default(),
        Some("respawn") => LastSessionClose::Respawn,
        Some("navigate") => LastSessionClose::Navigate,
        Some(other) => {
            log::warn!("unknown ui.last_session_close value {other:?}, using \"respawn\"");
            LastSessionClose::default()
        },
    }
}

/// Whether per-session UI (sidebar session rows, tab-strip segments) renders
/// for a single-session workspace instead of waiting for the two-session
/// threshold.  These are startup defaults only: the app copies them into
/// runtime state that key bindings can toggle, and nothing is persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionDisplay {
    pub sidebar_always: bool,
    pub tabs_always: bool,
}

/// alacritree-only `[ui.font]`: font family/size for the chrome (sidebars,
/// modals — everything that isn't the terminal grid).  Both fields default
/// to deriving from `[font]`, so an absent table changes nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiFont {
    pub family: Option<String>,
    /// Typographic points, same unit as `[font] size`; clamped to ≥ 1.0.
    pub size: Option<f32>,
}

/// Sidebar status glyphs, each independently overridable from `[ui.icons]`.
/// Overrides are trimmed and a blank value falls back to the default, so a
/// row marker can never be rendered empty.  Action buttons (×, +, ↻, ⇅) are
/// controls, not status, and stay fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icons {
    /// Glyph prefixing the sidebar search prompt.
    pub search: String,
    pub worktree_main: String,
    pub worktree: String,
    pub session: String,
    pub home: String,
    pub project_expanded: String,
    pub project_collapsed: String,
    pub pr_open: String,
    pub pr_draft: String,
    pub pr_merged: String,
    pub pr_closed: String,
}

/// `[ui.focus_outline]`: stroke a border around a panel while it owns
/// keyboard focus.  Per-panel toggles (`sidebar` covers both side panels),
/// shared color/thickness; both toggles default off so unmodified config
/// keeps today's look.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusOutline {
    pub sidebar: bool,
    pub terminal: bool,
    /// `None` falls back to the theme accent at resolution time.
    pub color: Option<Color32>,
    /// Absolute logical pixels (deliberately not ui_scale-multiplied);
    /// clamped to ≥ 0.5.
    pub thickness: f32,
}

impl Default for FocusOutline {
    fn default() -> Self {
        Self { sidebar: false, terminal: false, color: None, thickness: 1.0 }
    }
}

impl Default for Icons {
    fn default() -> Self {
        Self {
            search: DEFAULT_SEARCH_ICON.into(),
            worktree_main: "●".into(),
            worktree: "○".into(),
            session: "▪".into(),
            home: "⌂".into(),
            project_expanded: "▾".into(),
            project_collapsed: "▸".into(),
            pr_open: "⬤".into(),
            pr_draft: "◯".into(),
            pr_merged: "⬤".into(),
            pr_closed: "⬤".into(),
        }
    }
}

/// How one text span is emphasized.  `color: None` inherits whatever color the
/// site normally paints, so an emphasis that sets only `bold` still tracks the
/// theme.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TextEmphasis {
    pub color: Option<Color32>,
    pub bold: bool,
    pub italic: bool,
}

/// `[ui.path_style]`: how each site spells a path, plus the two emphases the
/// `Zed` style paints with.  Every field defaults to today's rendering.
#[derive(Debug, Clone, Copy, Default)]
pub struct PathStyleConfig {
    /// The `diff: <path>` pane title.
    pub diff_title: PathStyle,
    /// Staged / Unstaged / Changes-vs file rows in the git panel.
    pub git_rows: PathStyle,
    /// The workspace path atop the git panel.
    pub git_header: PathStyle,
    /// `Zed` style only, and only at the two egui sites.
    pub filename: TextEmphasis,
    pub parent: TextEmphasis,
}

#[derive(Debug, Clone)]
pub struct UiTheme {
    pub sidebar_background: Option<Color32>,
    pub sidebar_foreground: Option<Color32>,
    pub sidebar_border: Option<Color32>,
    pub sidebar_accent: Option<Color32>,
    /// Fire a desktop notification when a non-visible session needs attention.
    pub notifications: bool,
    /// How long an attention trigger must survive without the session going
    /// back to work before it pings.  Zero pings on the trigger itself.
    pub attention_grace: Duration,
    /// Ask before the sidebar's per-session `×` kills the PTY.
    pub confirm_session_close: ConfirmSessionClose,
    /// What closing the last session in the on-screen workspace does.
    pub last_session_close: LastSessionClose,
    /// Show single-session sidebar rows / tab segments ([`SessionDisplay`]).
    pub session_display: SessionDisplay,
    /// Paint PR-status badges on worktree rows (and poll `gh` for expanded
    /// projects' worktrees).  Off by default so an unmodified config spawns
    /// no `gh` processes; when enabled it is best-effort like the diff-base
    /// lookup: no `gh`, no auth, or no PR silently paints nothing.
    pub pr_status: bool,
    pub icons: Icons,
    pub focus_outline: FocusOutline,
    /// `[ui] scrollbar`: sidebar scrollbar style, "floating" (default) or
    /// "solid" (reserved gutter, never covers row icons).
    pub scrollbar: ScrollbarStyle,
    /// `[ui] sidebar_click_focus`: clicking a sidebar moves keyboard focus to
    /// it (so filter typing works without the focus shortcut).  Off by default
    /// so unmodified configs keep click-through-to-terminal behavior.
    pub sidebar_click_focus: bool,
    /// `[ui] worktree_name`: template for worktree row labels (subst syntax:
    /// `$name`, `$branch`, `$path`, `${var:fallback}`; `$pr` is the branch's
    /// PR number as `#123`, absent when none is known — it needs
    /// `pr_status = true`, which is what polls `gh`).  `None` keeps the
    /// plain worktree name.
    pub worktree_name: Option<String>,
    /// `[ui] project_name`: template for project row labels (`$name`, `$path`).
    /// A manual rename (`Project.label`) always wins over the template.
    pub project_name: Option<String>,
    /// `[ui.path_style]`: per-site path abbreviation.  All `Full` by default,
    /// which renders every path byte-for-byte as it does today.
    pub path_style: PathStyleConfig,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self {
            sidebar_background: None,
            sidebar_foreground: None,
            sidebar_border: None,
            sidebar_accent: None,
            notifications: true,
            attention_grace: Duration::ZERO,
            confirm_session_close: ConfirmSessionClose::Never,
            last_session_close: LastSessionClose::Respawn,
            session_display: SessionDisplay::default(),
            pr_status: false,
            icons: Icons::default(),
            focus_outline: FocusOutline::default(),
            scrollbar: ScrollbarStyle::Floating,
            sidebar_click_focus: false,
            worktree_name: None,
            project_name: None,
            path_style: PathStyleConfig::default(),
        }
    }
}

/// Where new git worktrees are created.  alacritree-only, lives under
/// `[workspace]` in `alacritree.toml`.  Every base directory — default,
/// global, or override — gets the `<project>-<hash>/<branch>` layout beneath
/// it; changing these options never moves existing worktrees because
/// discovery goes through `git worktree list`.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceConfig {
    /// Global base directory for new worktrees; `None` means the built-in
    /// `~/.alacritree/worktrees`.
    pub worktree_dir: Option<PathBuf>,
    pub overrides: Vec<WorktreeOverride>,
}

/// Per-project base-directory override, matched against the project root.
#[derive(Debug, Clone)]
pub struct WorktreeOverride {
    pub project: PathBuf,
    pub worktree_dir: PathBuf,
}

impl WorkspaceConfig {
    /// Base directory for a project's new worktrees: first matching override,
    /// then the global `worktree_dir`, then `None` (the caller falls back to
    /// the built-in default).  Paths compare canonicalized so a symlinked
    /// spelling of the same root still matches; canonicalization failure
    /// (path doesn't exist) falls back to the literal path.
    pub fn base_dir_for(&self, project_root: &Path) -> Option<PathBuf> {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let root = canonical(project_root);
        self.overrides
            .iter()
            .find(|o| canonical(&o.project) == root)
            .map(|o| o.worktree_dir.clone())
            .or_else(|| self.worktree_dir.clone())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            palette: Palette::default(),
            ui: UiTheme::default(),
            ui_font: UiFont::default(),
            workspace: WorkspaceConfig::default(),
            font: FontConfig::default(),
            cursor: CursorConfig::default(),
            scrolling: ScrollingConfig::default(),
            window: WindowConfig::default(),
            env: HashMap::new(),
            shell: None,
            selection: SelectionConfig::default(),
            bindings: Vec::new(),
            ipc_socket: true,
            working_directory: None,
            wsl_automount_root: "/mnt".to_string(),
            wsl_resident_helper: true,
            delta_path: None,
            profiles: Vec::new(),
            default_profile: None,
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        // Match alacritty's default of 11.25pt.  See `FontConfig::egui_size`
        // for the pt-to-logical-pixel conversion applied at use sites.
        Self {
            size: 11.25,
            normal: FontFace::default(),
            bold: FontFace::default(),
            italic: FontFace::default(),
            bold_italic: FontFace::default(),
            offset: FontDelta::default(),
            glyph_offset: FontDelta::default(),
            builtin_box_drawing: true,
            fallback: Vec::new(),
            color_glyphs: true,
            color_glyph_cache_mb: 10,
        }
    }
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self { shape: CursorShape::Block, blinking: false, unfocused_hollow: true }
    }
}

impl Default for ScrollingConfig {
    fn default() -> Self {
        Self { history: 10_000, multiplier: 3 }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { padding_x: 0.0, padding_y: 0.0, opacity: 1.0 }
    }
}

impl Default for SelectionConfig {
    fn default() -> Self {
        // Mirrors alacritty_terminal::term::SEMANTIC_ESCAPE_CHARS.
        Self {
            semantic_escape_chars: String::from(",│`|:\"' ()[]{}<>\t"),
            save_to_clipboard: false,
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        // Mirrors alacritty's built-in defaults so a user with no config sees
        // the same colors in both terminals.
        Self {
            fg: rgb(0xd8, 0xd8, 0xd8),
            bg: rgb(0x18, 0x18, 0x18),
            bright_fg: None,
            dim_fg: None,
            cursor_fg: None,
            cursor_bg: None,
            selection_bg: None,
            selection_fg: None,
            normal: [
                rgb(0x18, 0x18, 0x18),
                rgb(0xac, 0x42, 0x42),
                rgb(0x90, 0xa9, 0x59),
                rgb(0xf4, 0xbf, 0x75),
                rgb(0x6a, 0x9f, 0xb5),
                rgb(0xaa, 0x75, 0x9f),
                rgb(0x75, 0xb5, 0xaa),
                rgb(0xd8, 0xd8, 0xd8),
            ],
            bright: [
                rgb(0x6b, 0x6b, 0x6b),
                rgb(0xc5, 0x55, 0x55),
                rgb(0xaa, 0xc4, 0x74),
                rgb(0xfe, 0xca, 0x88),
                rgb(0x82, 0xb8, 0xc8),
                rgb(0xc2, 0x8c, 0xb8),
                rgb(0x93, 0xd3, 0xc3),
                rgb(0xf8, 0xf8, 0xf8),
            ],
            dim: None,
            indexed: Vec::new(),
            draw_bold_with_bright: false,
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Search order for `alacritty.{suffix}` (alacritty's own search order, see
/// `alacritty::config::installed_config`):
///   1. `$XDG_CONFIG_HOME/alacritty/alacritty.{suffix}`
///   2. `$XDG_CONFIG_HOME/alacritty.{suffix}`
///   3. `$HOME/.config/alacritty/alacritty.{suffix}`
///   4. `$HOME/.alacritty.{suffix}`
///   5. `/etc/alacritty/alacritty.{suffix}`
///
/// `alacritree.toml` is searched in the same locations and overrides whatever
/// `alacritty.toml` provided via the same merge semantics alacritty uses.
#[cfg(not(windows))]
fn installed_config(stem: &str, suffix: &str) -> Option<PathBuf> {
    let file_name = format!("{stem}.{suffix}");

    // Match alacritty: prefer XDG, then home fallbacks, then /etc.
    if let Some(p) = xdg::BaseDirectories::with_prefix("alacritty").find_config_file(&file_name) {
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(p) = xdg::BaseDirectories::new().find_config_file(&file_name) {
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(home) = home::home_dir() {
        let candidate = home.join(".config").join("alacritty").join(&file_name);
        if candidate.exists() {
            return Some(candidate);
        }
        let hidden = home.join(format!(".{file_name}"));
        if hidden.exists() {
            return Some(hidden);
        }
    }
    let etc = PathBuf::from("/etc/alacritty").join(&file_name);
    etc.exists().then_some(etc)
}

#[cfg(windows)]
fn installed_config(stem: &str, suffix: &str) -> Option<PathBuf> {
    let file_name = format!("{stem}.{suffix}");
    // `%APPDATA%\alacritty\<file>` is what upstream alacritty looks at; using
    // `std::env::var_os` here avoids pulling in the `dirs` crate just for
    // one path lookup.
    let appdata = std::env::var_os("APPDATA")?;
    let candidate = PathBuf::from(appdata).join("alacritty").join(&file_name);
    candidate.exists().then_some(candidate)
}

pub fn load() -> Config {
    let (files, merged) = assemble();
    for file in &files {
        if let (Some(path), Some(e)) = (&file.path, &file.error) {
            log::warn!("failed to load {}: {e}", path.display());
        }
    }

    let raw: RawConfig = match merged.try_into() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("invalid alacritty/alacritree config, using defaults: {e}");
            return Config::default();
        },
    };

    raw.into_config()
}

/// One of the two config files alacritree reads.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    pub stem: &'static str,
    /// Where it was found, or `None` if nothing on the search path matched.
    pub path: Option<PathBuf>,
    /// Why its settings are being ignored, if they are.
    pub error: Option<String>,
}

/// What [`load`] papers over.
///
/// A broken config must never stop a terminal from opening, so `load` logs the
/// problem and carries on with defaults.  The cost is that an ignored file looks
/// exactly like an absent one; this reports what `load` swallowed.
#[derive(Debug, Clone)]
pub struct ConfigDiagnosis {
    pub files: Vec<ConfigFile>,
    /// Set when the merged config does not fit alacritree's schema, in which
    /// case *every* setting in *both* files is dropped in favour of defaults.
    pub schema_error: Option<String>,
}

pub fn diagnose() -> ConfigDiagnosis {
    let (files, merged) = assemble();
    let schema_error = merged.try_into::<RawConfig>().err().map(|e| e.to_string());
    ConfigDiagnosis { files, schema_error }
}

/// Read both config files off the search path and merge them, alacritree over
/// alacritty.  A file that fails to parse contributes nothing and is reported
/// through its [`ConfigFile::error`].
fn assemble() -> (Vec<ConfigFile>, toml::Value) {
    let mut merged = toml::Value::Table(toml::value::Table::new());
    let mut files = Vec::new();

    for stem in ["alacritty", "alacritree"] {
        let path = installed_config(stem, "toml");
        let mut error = None;
        match path.as_deref().map(read_toml_value) {
            Some(Ok(Some(value))) => merged = merge(merged, value),
            Some(Err(e)) => error = Some(e.to_string()),
            _ => {},
        }
        files.push(ConfigFile { stem, path, error });
    }

    (files, merged)
}

fn read_toml_value(path: &std::path::Path) -> std::io::Result<Option<toml::Value>> {
    // toml 0.9's `<Value as FromStr>::from_str` is broken; go through the
    // serde entry point instead.  This matches alacritty's `deserialize_config`.
    match std::fs::read_to_string(path) {
        Ok(mut contents) => {
            // Strip UTF-8 BOM the same way alacritty does.
            if contents.starts_with('\u{FEFF}') {
                contents = contents.split_off(3);
            }
            match toml::from_str::<toml::Value>(&contents) {
                Ok(v) => Ok(Some(v)),
                Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Merge two TOML values using alacritty's semantics: arrays are
/// **concatenated** (not replaced), tables are merged recursively, and
/// primitives are replaced.  This matches `alacritty::config::serde_utils::merge`
/// so a `[[keyboard.bindings]]` array in `alacritree.toml` adds to (rather
/// than replaces) the bindings from `alacritty.toml`.
fn merge(base: toml::Value, replacement: toml::Value) -> toml::Value {
    use toml::Value;
    match (base, replacement) {
        (Value::Array(mut base), Value::Array(mut over)) => {
            base.append(&mut over);
            Value::Array(base)
        },
        (Value::Table(base), Value::Table(over)) => Value::Table(merge_tables(base, over)),
        (_, value) => value,
    }
}

fn merge_tables(
    mut base: toml::value::Table,
    replacement: toml::value::Table,
) -> toml::value::Table {
    for (key, value) in replacement {
        let value = match base.remove(&key) {
            Some(existing) => merge(existing, value),
            None => value,
        };
        base.insert(key, value);
    }
    base
}

// --- Raw deserialization ---------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    colors: RawColors,
    ui: RawUi,
    workspace: RawWorkspace,
    font: RawFont,
    cursor: RawCursor,
    scrolling: RawScrolling,
    window: RawWindow,
    #[serde(default)]
    env: HashMap<String, String>,
    terminal: RawTerminal,
    selection: RawSelection,
    keyboard: RawKeyboard,
    general: RawGeneral,
    wsl: RawWsl,
}

/// Subset of alacritty's `[general]` section that alacritree honors.  It
/// lives in the shared `alacritty.toml`, so disabling alacritty's socket
/// disables ours too — the two sockets are separate files, but the intent
/// ("no IPC") is the same.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawGeneral {
    ipc_socket: Option<bool>,
    working_directory: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawKeyboard {
    bindings: Vec<bindings::RawBinding>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawFont {
    size: Option<f32>,
    normal: RawFontFace,
    bold: RawFontFace,
    italic: RawFontFace,
    bold_italic: RawFontFace,
    offset: RawFontDelta,
    glyph_offset: RawFontDelta,
    builtin_box_drawing: Option<bool>,
    /// Ordered list of fallback font families or font file paths, tried in
    /// order after the four primary faces and before the automatic system
    /// chain.  Recommended home is `alacritree.toml`: upstream alacritty
    /// warns about unknown keys, so putting it in the shared `alacritty.toml`
    /// would make the real alacritty noisy.
    fallback: Option<Vec<String>>,
    /// Also alacritree-only, so it belongs in `alacritree.toml` alongside
    /// `fallback`.
    color_glyphs: Option<bool>,
    color_glyph_cache_mb: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawFontFace {
    family: Option<String>,
    style: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawFontDelta {
    x: Option<i8>,
    y: Option<i8>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawCursor {
    style: Option<RawCursorStyle>,
    /// Older alacritty configs accept just `style = "Block"` rather than
    /// `style.shape = "Block"`.  We allow both via `RawCursorStyle`.
    unfocused_hollow: Option<bool>,
    blink_interval: Option<u64>,
    blink_timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawCursorStyle {
    Shape(String),
    Detailed { shape: Option<String>, blinking: Option<String> },
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawScrolling {
    history: Option<u32>,
    multiplier: Option<u8>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawWindow {
    padding: Option<RawPadding>,
    opacity: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPadding {
    x: Option<f32>,
    y: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTerminal {
    shell: Option<RawShell>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawShell {
    Program(String),
    Detailed {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawSelection {
    semantic_escape_chars: Option<String>,
    save_to_clipboard: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawColors {
    #[serde(default)]
    primary: RawPrimary,
    #[serde(default)]
    cursor: RawInverted,
    #[serde(default)]
    selection: RawInverted,
    #[serde(default)]
    normal: RawSet,
    #[serde(default)]
    bright: RawSet,
    #[serde(default)]
    dim: Option<RawSet>,
    #[serde(default)]
    indexed_colors: Vec<RawIndexed>,
    #[serde(default)]
    draw_bold_text_with_bright_colors: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPrimary {
    foreground: Option<RgbStr>,
    background: Option<RgbStr>,
    bright_foreground: Option<RgbStr>,
    dim_foreground: Option<RgbStr>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawInverted {
    /// Foreground glyph color.  Alacritty calls this `text`; we accept both.
    text: Option<RgbStr>,
    /// Background block color.  Alacritty calls this `cursor`; we accept both.
    cursor: Option<RgbStr>,
    foreground: Option<RgbStr>,
    background: Option<RgbStr>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawSet {
    black: Option<RgbStr>,
    red: Option<RgbStr>,
    green: Option<RgbStr>,
    yellow: Option<RgbStr>,
    blue: Option<RgbStr>,
    magenta: Option<RgbStr>,
    cyan: Option<RgbStr>,
    white: Option<RgbStr>,
}

#[derive(Debug, Deserialize)]
struct RawIndexed {
    index: u8,
    color: RgbStr,
}

/// Top-level `[wsl]`: platform-integration options.  Lives outside `[ui]`
/// because nothing here is presentation — it governs how the app talks to
/// distros.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawWsl {
    /// Keep a resident helper process per distro for foreground probes,
    /// batched git queries, and tool discovery.  `false` restores one-shot
    /// wsl.exe spawns everywhere; WSL sessions then always report "no
    /// TUI", so FocusLeft/FocusRight always move panel focus.
    resident_helper: Option<bool>,
    /// Distro-side mount point for Windows drives, mirroring wsl.conf's
    /// `[automount] root`.  Only used for paths *we* translate (git output
    /// from inside a distro); `wsl.exe --cd` translates with the distro's
    /// real mount table regardless of this value.
    automount_root: Option<String>,
}

/// `[ui.icons]`: sidebar glyph overrides.  Any string works, so Nerd Font
/// users can substitute their own icons.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawIcons {
    search: Option<String>,
    worktree_main: Option<String>,
    worktree: Option<String>,
    session: Option<String>,
    home: Option<String>,
    project_expanded: Option<String>,
    project_collapsed: Option<String>,
    pr_open: Option<String>,
    pr_draft: Option<String>,
    pr_merged: Option<String>,
    pr_closed: Option<String>,
}

/// A trimmed, non-blank override — or the default.
fn icon_or(raw: Option<String>, default: &str) -> String {
    raw.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn build_icons(raw: RawIcons) -> Icons {
    let d = Icons::default();
    Icons {
        search: icon_or(raw.search, &d.search),
        worktree_main: icon_or(raw.worktree_main, &d.worktree_main),
        worktree: icon_or(raw.worktree, &d.worktree),
        session: icon_or(raw.session, &d.session),
        home: icon_or(raw.home, &d.home),
        project_expanded: icon_or(raw.project_expanded, &d.project_expanded),
        project_collapsed: icon_or(raw.project_collapsed, &d.project_collapsed),
        pr_open: icon_or(raw.pr_open, &d.pr_open),
        pr_draft: icon_or(raw.pr_draft, &d.pr_draft),
        pr_merged: icon_or(raw.pr_merged, &d.pr_merged),
        pr_closed: icon_or(raw.pr_closed, &d.pr_closed),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawUiWsl {
    /// Deprecated location: `[wsl] automount_root` supersedes this and wins
    /// when both are set; kept so existing configs keep working.
    automount_root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawSessionDisplay {
    /// Show a workspace's sidebar session row even with a single session.
    sidebar_always: Option<bool>,
    /// Draw a tab-strip segment even with a single session.
    tabs_always: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawUiFont {
    family: Option<String>,
    size: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawFocusOutline {
    sidebar: Option<bool>,
    terminal: Option<bool>,
    color: Option<RgbStr>,
    thickness: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawUi {
    sidebar_background: Option<RgbStr>,
    sidebar_foreground: Option<RgbStr>,
    sidebar_border: Option<RgbStr>,
    sidebar_accent: Option<RgbStr>,
    notifications: Option<bool>,
    /// Grace window in milliseconds before an attention trigger pings; a
    /// session that resumes work inside it swallows the ping.  Default 0.
    attention_grace_ms: Option<u64>,
    /// When the sidebar × on a session row asks before killing the PTY:
    /// "never" (default) | "busy" | "always".
    confirm_session_close: Option<String>,
    /// What closing the on-screen workspace's last session does:
    /// "respawn" (default) | "navigate".
    last_session_close: Option<String>,
    session_display: RawSessionDisplay,
    delta_path: Option<String>,
    icons: RawIcons,
    /// Sidebar scrollbar style: "floating" (default) | "solid".
    scrollbar: Option<String>,
    pr_status: Option<bool>,
    font: RawUiFont,
    worktree_name: Option<String>,
    project_name: Option<String>,
    wsl: RawUiWsl,
    profiles: Vec<RawProfile>,
    default_profile: Option<String>,
    focus_outline: RawFocusOutline,
    /// Clicking a sidebar moves keyboard focus to it.  Default false.
    sidebar_click_focus: Option<bool>,
    path_style: RawPathStyle,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPathStyle {
    /// "full" (default) | "fish" | "zed", per site.
    diff_title: Option<String>,
    git_rows: Option<String>,
    git_header: Option<String>,
    filename: RawTextEmphasis,
    parent: RawTextEmphasis,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTextEmphasis {
    color: Option<RgbStr>,
    bold: Option<bool>,
    italic: Option<bool>,
}

/// One `[[ui.profiles]]` entry.  Fields are optional so a malformed entry
/// degrades to a warning instead of failing the whole config parse.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawProfile {
    name: Option<String>,
    program: Option<String>,
    args: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawWorkspace {
    worktree_dir: Option<String>,
    overrides: Vec<RawWorktreeOverride>,
}

#[derive(Debug, Deserialize)]
struct RawWorktreeOverride {
    project: String,
    worktree_dir: String,
}

/// Wrapper that parses `"0xrrggbb"`, `"#rrggbb"`, or `"rrggbb"` into an `Rgb`.
#[derive(Debug, Clone, Copy)]
struct RgbStr(Rgb);

impl<'de> Deserialize<'de> for RgbStr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        parse_hex_rgb(&s)
            .map(RgbStr)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid color string: {s:?}")))
    }
}

fn parse_hex_rgb(s: &str) -> Option<Rgb> {
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| s.strip_prefix('#'))
        .unwrap_or(s);
    if stripped.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&stripped[0..2], 16).ok()?;
    let g = u8::from_str_radix(&stripped[2..4], 16).ok()?;
    let b = u8::from_str_radix(&stripped[4..6], 16).ok()?;
    Some(Rgb { r, g, b })
}

/// Expand a leading `~` to the home directory and require the result to be
/// absolute.  Relative paths are rejected rather than resolved against the
/// process CWD, which is meaningless for a GUI app; `~user` expansion is not
/// supported.  Returns `None` (after logging) for anything unusable.
fn parse_config_path(raw: &str, key: &str) -> Option<PathBuf> {
    let path = if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
        let Some(home) = home::home_dir() else {
            log::warn!("{key}: cannot expand `~` in {raw:?}: no home directory");
            return None;
        };
        home.join(raw[1..].trim_start_matches(['/', '\\']))
    } else {
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        log::warn!("{key}: ignoring non-absolute path {raw:?}");
        return None;
    }
    Some(path)
}

impl RawConfig {
    fn into_config(self) -> Config {
        let config = Config::default();
        let mut palette = config.palette;
        let c = self.colors;

        if let Some(v) = c.primary.foreground {
            palette.fg = v.0;
        }
        if let Some(v) = c.primary.background {
            palette.bg = v.0;
        }
        palette.bright_fg = c.primary.bright_foreground.map(|v| v.0);
        palette.dim_fg = c.primary.dim_foreground.map(|v| v.0);

        // Cursor: alacritty's [colors.cursor] uses {text, cursor} for {fg, bg};
        // we accept the literal {foreground, background} too.
        palette.cursor_fg = c.cursor.text.map(|v| v.0).or_else(|| c.cursor.foreground.map(|v| v.0));
        palette.cursor_bg =
            c.cursor.cursor.map(|v| v.0).or_else(|| c.cursor.background.map(|v| v.0));

        palette.selection_fg =
            c.selection.text.map(|v| v.0).or_else(|| c.selection.foreground.map(|v| v.0));
        palette.selection_bg = c.selection.background.map(|v| v.0);

        apply_set(&mut palette.normal, c.normal);
        apply_set(&mut palette.bright, c.bright);
        if let Some(d) = c.dim {
            let mut dim = palette.normal;
            apply_set(&mut dim, d);
            palette.dim = Some(dim);
        }

        palette.indexed = c
            .indexed_colors
            .into_iter()
            .filter(|i| i.index >= 16)
            .map(|i| (i.index, i.color.0))
            .collect();

        palette.draw_bold_with_bright = c.draw_bold_text_with_bright_colors;

        let ui = UiTheme {
            sidebar_background: self.ui.sidebar_background.map(|v| rgb_to_color32(v.0)),
            sidebar_foreground: self.ui.sidebar_foreground.map(|v| rgb_to_color32(v.0)),
            sidebar_border: self.ui.sidebar_border.map(|v| rgb_to_color32(v.0)),
            sidebar_accent: self.ui.sidebar_accent.map(|v| rgb_to_color32(v.0)),
            notifications: self.ui.notifications.unwrap_or(true),
            attention_grace: Duration::from_millis(self.ui.attention_grace_ms.unwrap_or(0)),
            confirm_session_close: parse_confirm_session_close(
                self.ui.confirm_session_close.as_deref(),
            ),
            last_session_close: parse_last_session_close(self.ui.last_session_close.as_deref()),
            session_display: SessionDisplay {
                sidebar_always: self.ui.session_display.sidebar_always.unwrap_or(false),
                tabs_always: self.ui.session_display.tabs_always.unwrap_or(false),
            },
            pr_status: self.ui.pr_status.unwrap_or(false),
            icons: build_icons(self.ui.icons),
            focus_outline: FocusOutline {
                sidebar: self.ui.focus_outline.sidebar.unwrap_or(false),
                terminal: self.ui.focus_outline.terminal.unwrap_or(false),
                color: self.ui.focus_outline.color.map(|v| rgb_to_color32(v.0)),
                thickness: self.ui.focus_outline.thickness.map_or(1.0, |t| t.max(0.5)),
            },
            scrollbar: parse_scrollbar(self.ui.scrollbar.as_deref()),
            sidebar_click_focus: self.ui.sidebar_click_focus.unwrap_or(false),
            worktree_name: self.ui.worktree_name.clone().filter(|t| !t.trim().is_empty()),
            project_name: self.ui.project_name.clone().filter(|t| !t.trim().is_empty()),
            path_style: PathStyleConfig {
                diff_title: parse_path_style(self.ui.path_style.diff_title.as_deref()),
                git_rows: parse_path_style(self.ui.path_style.git_rows.as_deref()),
                git_header: parse_path_style(self.ui.path_style.git_header.as_deref()),
                filename: text_emphasis(&self.ui.path_style.filename),
                parent: text_emphasis(&self.ui.path_style.parent),
            },
        };

        // ---- Font ----
        let mut font = config.font.clone();
        if let Some(s) = self.font.size {
            font.size = s.max(1.0);
        }
        font.normal = FontFace {
            family: self.font.normal.family.clone(),
            style: self.font.normal.style.clone(),
        };
        font.bold =
            FontFace { family: self.font.bold.family.clone(), style: self.font.bold.style.clone() };
        font.italic = FontFace {
            family: self.font.italic.family.clone(),
            style: self.font.italic.style.clone(),
        };
        font.bold_italic = FontFace {
            family: self.font.bold_italic.family.clone(),
            style: self.font.bold_italic.style.clone(),
        };
        font.offset = FontDelta {
            x: self.font.offset.x.unwrap_or(font.offset.x),
            y: self.font.offset.y.unwrap_or(font.offset.y),
        };
        font.glyph_offset = FontDelta {
            x: self.font.glyph_offset.x.unwrap_or(font.glyph_offset.x),
            y: self.font.glyph_offset.y.unwrap_or(font.glyph_offset.y),
        };
        if let Some(b) = self.font.builtin_box_drawing {
            font.builtin_box_drawing = b;
        }
        font.fallback = self.font.fallback.clone().unwrap_or_default();
        if let Some(c) = self.font.color_glyphs {
            font.color_glyphs = c;
        }
        if let Some(mb) = self.font.color_glyph_cache_mb {
            font.color_glyph_cache_mb = mb;
        }

        // ---- Cursor ----
        let mut cursor = config.cursor;
        if let Some(style) = self.cursor.style {
            apply_cursor_style(&mut cursor, style);
        }
        if let Some(v) = self.cursor.unfocused_hollow {
            cursor.unfocused_hollow = v;
        }

        // ---- Scrolling ----
        let mut scrolling = config.scrolling;
        if let Some(h) = self.scrolling.history {
            scrolling.history = h as usize;
        }
        if let Some(m) = self.scrolling.multiplier {
            scrolling.multiplier = m;
        }

        // ---- Window padding ----
        let mut window = config.window;
        if let Some(p) = self.window.padding {
            if let Some(x) = p.x {
                window.padding_x = x;
            }
            if let Some(y) = p.y {
                window.padding_y = y;
            }
        }
        if let Some(o) = self.window.opacity {
            window.opacity = o.clamp(0.0, 1.0);
        }

        // ---- Selection ----
        let mut selection = config.selection.clone();
        if let Some(s) = self.selection.semantic_escape_chars {
            selection.semantic_escape_chars = s;
        }
        if let Some(v) = self.selection.save_to_clipboard {
            selection.save_to_clipboard = v;
        }

        // ---- Shell ----
        let shell = self.terminal.shell.map(|s| match s {
            RawShell::Program(program) => ShellConfig { program, args: Vec::new() },
            RawShell::Detailed { program, args } => ShellConfig { program, args },
        });

        let bindings = bindings::parse_bindings(self.keyboard.bindings);

        // ---- Workspace ----
        let workspace = WorkspaceConfig {
            worktree_dir: self
                .workspace
                .worktree_dir
                .as_deref()
                .and_then(|raw| parse_config_path(raw, "workspace.worktree_dir")),
            overrides: self
                .workspace
                .overrides
                .iter()
                .filter_map(|o| {
                    let project = parse_config_path(&o.project, "workspace.overrides.project")?;
                    let worktree_dir =
                        parse_config_path(&o.worktree_dir, "workspace.overrides.worktree_dir")?;
                    Some(WorktreeOverride { project, worktree_dir })
                })
                .collect(),
        };

        // ---- WSL ----
        // `[wsl]` supersedes the deprecated `[ui.wsl]` location.
        let wsl_automount_root = self
            .wsl
            .automount_root
            .or(self.ui.wsl.automount_root)
            .map(|r| r.trim_end_matches('/').to_string())
            .filter(|r| r.starts_with('/') && r.len() > 1)
            .unwrap_or_else(|| "/mnt".to_string());
        let wsl_resident_helper = self.wsl.resident_helper.unwrap_or(true);

        // ---- UI Font ----
        let ui_font = UiFont {
            family: self.ui.font.family.clone().filter(|f| !f.trim().is_empty()),
            size: self.ui.font.size.map(|s| s.max(1.0)),
        };

        // ---- Profiles ----
        let profiles = build_profiles(self.ui.profiles);
        let default_profile = self.ui.default_profile.filter(|n| {
            let known = profiles.iter().any(|p| &p.name == n);
            if !known {
                log::warn!("default_profile `{n}` names no [[ui.profiles]] entry; ignoring");
            }
            known
        });

        Config {
            palette,
            ui,
            ui_font,
            workspace,
            font,
            cursor,
            scrolling,
            window,
            env: self.env,
            shell,
            selection,
            bindings,
            ipc_socket: self.general.ipc_socket.unwrap_or(true),
            working_directory: self
                .general
                .working_directory
                .as_deref()
                .and_then(|raw| parse_config_path(raw, "general.working_directory")),
            wsl_automount_root,
            wsl_resident_helper,
            delta_path: self.ui.delta_path.filter(|s| !s.trim().is_empty()),
            profiles,
            default_profile,
        }
    }
}

fn apply_cursor_style(cursor: &mut CursorConfig, style: RawCursorStyle) {
    let (shape, blinking) = match style {
        RawCursorStyle::Shape(s) => (Some(s), None),
        RawCursorStyle::Detailed { shape, blinking } => (shape, blinking),
    };
    if let Some(s) = shape.as_deref() {
        cursor.shape = match s {
            "Block" | "block" => CursorShape::Block,
            "Underline" | "underline" => CursorShape::Underline,
            "Beam" | "beam" => CursorShape::Beam,
            "HollowBlock" | "hollow_block" => CursorShape::HollowBlock,
            "Hidden" | "hidden" => CursorShape::Hidden,
            other => {
                log::warn!("unknown cursor shape: {other}");
                cursor.shape
            },
        };
    }
    if let Some(b) = blinking.as_deref() {
        cursor.blinking = matches!(b, "On" | "on" | "Always" | "always");
    }
}

fn apply_set(target: &mut [Rgb; 8], set: RawSet) {
    let names =
        [set.black, set.red, set.green, set.yellow, set.blue, set.magenta, set.cyan, set.white];
    for (slot, val) in target.iter_mut().zip(names) {
        if let Some(v) = val {
            *slot = v.0;
        }
    }
}

fn rgb_to_color32(r: Rgb) -> Color32 {
    Color32::from_rgb(r.r, r.g, r.b)
}

/// Drop unusable `[[ui.profiles]]` entries instead of failing the parse:
/// bad config degrades with a warning, matching the rest of this module.
fn build_profiles(raw: Vec<RawProfile>) -> Vec<Profile> {
    let mut out: Vec<Profile> = Vec::with_capacity(raw.len());
    for (i, p) in raw.into_iter().enumerate() {
        let name = p.name.filter(|n| !n.is_empty());
        let program = p.program.filter(|x| !x.is_empty());
        let (name, program) = match (name, program) {
            (Some(name), Some(program)) => (name, program),
            (Some(name), None) => {
                log::warn!("[[ui.profiles]] entry `{name}` needs a non-empty `program`; dropping");
                continue;
            },
            (None, _) => {
                log::warn!("[[ui.profiles]] entry {i} needs a non-empty `name`; dropping");
                continue;
            },
        };
        if out.iter().any(|e| e.name == name) {
            log::warn!("duplicate profile name `{name}`; keeping the first");
            continue;
        }
        out.push(Profile { name, program, args: p.args });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui_from_toml(input: &str) -> UiTheme {
        let value: toml::Value = toml::from_str(input).expect("valid toml");
        let raw: RawConfig = value.try_into().expect("valid config");
        raw.into_config().ui
    }

    #[test]
    fn automount_root_defaults_and_normalizes() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.into_config().wsl_automount_root, "/mnt");

        let raw: RawConfig = toml::from_str("[ui.wsl]\nautomount_root = \"/drives/\"").unwrap();
        assert_eq!(raw.into_config().wsl_automount_root, "/drives");

        // Nonsense values fall back rather than corrupting every translation.
        let raw: RawConfig = toml::from_str("[ui.wsl]\nautomount_root = \"mnt\"").unwrap();
        assert_eq!(raw.into_config().wsl_automount_root, "/mnt");
    }

    #[test]
    fn wsl_section_wins_over_deprecated_ui_location() {
        let raw: RawConfig = toml::from_str("[wsl]\nautomount_root = \"/drives\"").unwrap();
        assert_eq!(raw.into_config().wsl_automount_root, "/drives");

        let both = "[wsl]\nautomount_root = \"/new\"\n[ui.wsl]\nautomount_root = \"/old\"";
        let raw: RawConfig = toml::from_str(both).unwrap();
        assert_eq!(raw.into_config().wsl_automount_root, "/new");

        // Existing configs keep working through the deprecated location.
        let raw: RawConfig = toml::from_str("[ui.wsl]\nautomount_root = \"/old\"").unwrap();
        assert_eq!(raw.into_config().wsl_automount_root, "/old");
    }

    #[test]
    fn resident_helper_defaults_on() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert!(raw.into_config().wsl_resident_helper);

        let raw: RawConfig = toml::from_str("[wsl]\nresident_helper = false").unwrap();
        assert!(!raw.into_config().wsl_resident_helper);
    }

    #[test]
    fn delta_path_parses_and_blank_is_none() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.into_config().delta_path, None);

        let raw: RawConfig = toml::from_str("[ui]\ndelta_path = \"/opt/delta\"").unwrap();
        assert_eq!(raw.into_config().delta_path.as_deref(), Some("/opt/delta"));

        // A blank override is treated as unset so discovery still runs.
        let raw: RawConfig = toml::from_str("[ui]\ndelta_path = \"  \"").unwrap();
        assert_eq!(raw.into_config().delta_path, None);
    }

    #[test]
    fn path_style_defaults_to_full_everywhere() {
        let ui = ui_from_toml("");
        assert_eq!(ui.path_style.diff_title, PathStyle::Full);
        assert_eq!(ui.path_style.git_rows, PathStyle::Full);
        assert_eq!(ui.path_style.git_header, PathStyle::Full);
        assert_eq!(ui.path_style.filename, TextEmphasis::default());
        assert_eq!(ui.path_style.parent, TextEmphasis::default());
    }

    #[test]
    fn path_style_parses_per_site_and_falls_back_on_nonsense() {
        let ui = ui_from_toml("[ui.path_style]\ndiff_title = \"zed\"\ngit_rows = \"fish\"");
        assert_eq!(ui.path_style.diff_title, PathStyle::Zed);
        assert_eq!(ui.path_style.git_rows, PathStyle::Fish);
        // An omitted key is not an error, it is "full".
        assert_eq!(ui.path_style.git_header, PathStyle::Full);

        let ui = ui_from_toml("[ui.path_style]\ngit_header = \"zeb\"");
        assert_eq!(ui.path_style.git_header, PathStyle::Full);
    }

    #[test]
    fn path_style_emphasis_parses_and_a_blank_color_is_an_error() {
        let ui = ui_from_toml(
            "[ui.path_style.filename]\ncolor = \"#e6e6e6\"\nbold = true\n\
             [ui.path_style.parent]\nitalic = true\n",
        );
        assert_eq!(ui.path_style.filename.color, Some(Color32::from_rgb(0xe6, 0xe6, 0xe6)));
        assert!(ui.path_style.filename.bold);
        assert!(!ui.path_style.filename.italic);
        assert_eq!(ui.path_style.parent.color, None);
        assert!(ui.path_style.parent.italic);

        // `RgbStr` rejects a blank string and a raw-schema error discards the
        // whole merged config, so an empty color is a mistake to fix, not a way
        // to say "absent" — omit the key instead.
        let value: toml::Value =
            toml::from_str("[ui.path_style.filename]\ncolor = \"\"").expect("valid toml");
        let raw: Result<RawConfig, _> = value.try_into();
        assert!(raw.is_err(), "a blank color must not parse as absent");
    }

    #[test]
    fn confirm_session_close_defaults_to_never() {
        let ui = ui_from_toml("");
        assert_eq!(ui.confirm_session_close, ConfirmSessionClose::Never);
    }

    #[test]
    fn confirm_session_close_parses_all_values() {
        for (raw, expected) in [
            ("never", ConfirmSessionClose::Never),
            ("busy", ConfirmSessionClose::Busy),
            ("always", ConfirmSessionClose::Always),
        ] {
            let ui = ui_from_toml(&format!("[ui]\nconfirm_session_close = \"{raw}\""));
            assert_eq!(ui.confirm_session_close, expected, "value {raw:?}");
        }
    }

    #[test]
    fn confirm_session_close_invalid_falls_back_to_never() {
        let ui = ui_from_toml("[ui]\nconfirm_session_close = \"sometimes\"");
        assert_eq!(ui.confirm_session_close, ConfirmSessionClose::Never);
    }

    #[test]
    fn scrollbar_defaults_to_floating() {
        let ui = ui_from_toml("");
        assert_eq!(ui.scrollbar, ScrollbarStyle::Floating);
    }

    #[test]
    fn scrollbar_parses_all_values() {
        for (raw, expected) in
            [("floating", ScrollbarStyle::Floating), ("solid", ScrollbarStyle::Solid)]
        {
            let ui = ui_from_toml(&format!("[ui]\nscrollbar = \"{raw}\""));
            assert_eq!(ui.scrollbar, expected, "value {raw:?}");
        }
    }

    #[test]
    fn scrollbar_invalid_falls_back_to_floating() {
        let ui = ui_from_toml("[ui]\nscrollbar = \"chunky\"");
        assert_eq!(ui.scrollbar, ScrollbarStyle::Floating);
    }

    #[test]
    fn search_icon_defaults_and_overrides() {
        assert_eq!(ui_from_toml("").icons.search, DEFAULT_SEARCH_ICON);
        assert_eq!(ui_from_toml("[ui.icons]\nsearch = \"\u{f002}\"").icons.search, "\u{f002}");
    }

    #[test]
    fn last_session_close_defaults_to_respawn() {
        let ui = ui_from_toml("");
        assert_eq!(ui.last_session_close, LastSessionClose::Respawn);
    }

    #[test]
    fn last_session_close_parses_all_values() {
        for (raw, expected) in
            [("respawn", LastSessionClose::Respawn), ("navigate", LastSessionClose::Navigate)]
        {
            let ui = ui_from_toml(&format!("[ui]\nlast_session_close = \"{raw}\""));
            assert_eq!(ui.last_session_close, expected, "value {raw:?}");
        }
    }

    #[test]
    fn last_session_close_invalid_falls_back_to_respawn() {
        let ui = ui_from_toml("[ui]\nlast_session_close = \"panic\"");
        assert_eq!(ui.last_session_close, LastSessionClose::Respawn);
    }

    #[test]
    fn requires_prompt_covers_policy_matrix() {
        use ConfirmSessionClose::*;
        for (policy, busy, expected) in [
            (Never, false, false),
            (Never, true, false),
            (Busy, false, false),
            (Busy, true, true),
            (Always, false, true),
            (Always, true, true),
        ] {
            assert_eq!(policy.requires_prompt(busy), expected, "{policy:?} busy={busy}");
        }
    }

    fn abs(tail: &str) -> String {
        if cfg!(windows) { format!("C:\\{tail}") } else { format!("/{tail}") }
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = home::home_dir().unwrap();
        assert_eq!(parse_config_path("~/wt", "test"), Some(home.join("wt")));
        assert_eq!(parse_config_path("~", "test"), Some(home));
    }

    #[test]
    fn absolute_path_passes_through() {
        let raw = abs("wt");
        assert_eq!(parse_config_path(&raw, "test"), Some(PathBuf::from(raw)));
    }

    #[test]
    fn relative_and_user_tilde_paths_are_rejected() {
        assert_eq!(parse_config_path("relative/dir", "test"), None);
        assert_eq!(parse_config_path("~user/dir", "test"), None);
    }

    #[test]
    fn general_working_directory_defaults_to_none() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(raw.into_config().working_directory, None);
    }

    #[test]
    fn general_working_directory_expands_tilde_and_forward_slashes() {
        let home = home::home_dir().unwrap();
        let raw: RawConfig =
            toml::from_str("[general]\nworking_directory = \"~/projects\"").unwrap();
        assert_eq!(raw.into_config().working_directory, Some(home.join("projects")));
    }

    #[test]
    fn general_working_directory_accepts_absolute_paths() {
        let toml_src = format!(
            "[general]\nworking_directory = \"{}\"",
            abs("somewhere").replace('\\', "\\\\")
        );
        let raw: RawConfig = toml::from_str(&toml_src).unwrap();
        assert_eq!(raw.into_config().working_directory, Some(PathBuf::from(abs("somewhere"))));
    }

    #[cfg(windows)]
    #[test]
    fn general_working_directory_accepts_forward_slash_windows_paths() {
        let raw: RawConfig =
            toml::from_str("[general]\nworking_directory = \"C:/somewhere\"").unwrap();
        assert_eq!(raw.into_config().working_directory, Some(PathBuf::from("C:/somewhere")));
    }

    #[test]
    fn general_working_directory_rejects_relative_paths() {
        let raw: RawConfig =
            toml::from_str("[general]\nworking_directory = \"relative/dir\"").unwrap();
        assert_eq!(raw.into_config().working_directory, None);
    }

    #[test]
    fn workspace_table_parses_into_config() {
        let toml_src = format!(
            r#"
            [workspace]
            worktree_dir = "{global}"

            [[workspace.overrides]]
            project = "{proj}"
            worktree_dir = "{over}"
            "#,
            global = abs("global-wt").replace('\\', "\\\\"),
            proj = abs("proj").replace('\\', "\\\\"),
            over = abs("proj-wt").replace('\\', "\\\\"),
        );
        let raw: RawConfig = toml::from_str(&toml_src).unwrap();
        let config = raw.into_config();
        assert_eq!(config.workspace.worktree_dir, Some(PathBuf::from(abs("global-wt"))));
        assert_eq!(config.workspace.overrides.len(), 1);
        assert_eq!(config.workspace.overrides[0].project, PathBuf::from(abs("proj")));
        assert_eq!(config.workspace.overrides[0].worktree_dir, PathBuf::from(abs("proj-wt")));
    }

    #[test]
    fn base_dir_for_prefers_override_then_global_then_none() {
        let ws = WorkspaceConfig {
            worktree_dir: Some(PathBuf::from(abs("global-wt"))),
            overrides: vec![WorktreeOverride {
                project: PathBuf::from(abs("proj")),
                worktree_dir: PathBuf::from(abs("proj-wt")),
            }],
        };
        assert_eq!(ws.base_dir_for(Path::new(&abs("proj"))), Some(PathBuf::from(abs("proj-wt"))));
        assert_eq!(
            ws.base_dir_for(Path::new(&abs("other"))),
            Some(PathBuf::from(abs("global-wt")))
        );
        let empty = WorkspaceConfig::default();
        assert_eq!(empty.base_dir_for(Path::new(&abs("proj"))), None);
    }

    fn parse(s: &str) -> Config {
        let value: toml::Value = toml::from_str(s).unwrap();
        let raw: RawConfig = value.try_into().unwrap();
        raw.into_config()
    }

    #[test]
    fn font_fallback_list_parses() {
        let config = parse(
            r#"
            [font]
            fallback = ["JetBrainsMono Nerd Font", "C:\\Fonts\\custom.ttf"]
            "#,
        );
        assert_eq!(config.font.fallback, ["JetBrainsMono Nerd Font", "C:\\Fonts\\custom.ttf"]);
    }

    #[test]
    fn font_fallback_defaults_empty() {
        assert!(parse("").font.fallback.is_empty());
    }

    #[test]
    fn font_fallback_arrays_concatenate_across_files() {
        // alacritty merge semantics: an array in alacritree.toml appends to
        // the same array from alacritty.toml rather than replacing it.
        let base: toml::Value = toml::from_str("[font]\nfallback = [\"A\"]").unwrap();
        let over: toml::Value = toml::from_str("[font]\nfallback = [\"B\"]").unwrap();
        let merged = merge(base, over);
        let raw: RawConfig = merged.try_into().unwrap();
        assert_eq!(raw.into_config().font.fallback, ["A", "B"]);
    }

    #[test]
    fn profiles_parse_and_validate() {
        let toml_src = r#"
[ui]
default_profile = "pwsh"

[[ui.profiles]]
name = "pwsh"
program = "pwsh"
args = ["-NoLogo"]

[[ui.profiles]]
name = "ubuntu"
program = "wsl.exe"
args = ["-d", "ubuntu"]
"#;
        let raw: RawConfig = toml::from_str(toml_src).unwrap();
        let config = raw.into_config();
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(
            config.profiles[0],
            Profile { name: "pwsh".into(), program: "pwsh".into(), args: vec!["-NoLogo".into()] }
        );
        assert_eq!(config.default_profile.as_deref(), Some("pwsh"));
        assert_eq!(config.profile("ubuntu").unwrap().program, "wsl.exe");
        assert!(config.profile("nope").is_none());
    }

    #[test]
    fn invalid_profiles_are_dropped() {
        let toml_src = r#"
[ui]
default_profile = "ghost"

[[ui.profiles]]
name = ""
program = "pwsh"

[[ui.profiles]]
name = "noprog"

[[ui.profiles]]
name = "dup"
program = "first"

[[ui.profiles]]
name = "dup"
program = "second"
"#;
        let raw: RawConfig = toml::from_str(toml_src).unwrap();
        let config = raw.into_config();
        assert_eq!(config.profiles.len(), 1, "empty name, missing program, and dup dropped");
        assert_eq!(config.profiles[0].program, "first");
        assert!(config.profiles[0].args.is_empty(), "no args in TOML defaults to empty");
        assert_eq!(config.default_profile, None, "dangling default_profile is ignored");
    }

    #[test]
    fn no_profiles_by_default() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config();
        assert!(config.profiles.is_empty());
        assert_eq!(config.default_profile, None);
    }

    #[test]
    fn session_display_defaults_to_hidden() {
        let ui = ui_from_toml("");
        assert!(!ui.session_display.sidebar_always);
        assert!(!ui.session_display.tabs_always);
    }

    #[test]
    fn session_display_parses_both_flags() {
        let ui = ui_from_toml("[ui.session_display]\nsidebar_always = true\ntabs_always = true");
        assert!(ui.session_display.sidebar_always);
        assert!(ui.session_display.tabs_always);
    }

    #[test]
    fn session_display_partial_table_leaves_the_other_flag_off() {
        let ui = ui_from_toml("[ui.session_display]\nsidebar_always = true");
        assert!(ui.session_display.sidebar_always);
        assert!(!ui.session_display.tabs_always);
    }

    /// alacritree.toml merges over alacritty.toml key-by-key, so setting one
    /// flag per file must yield both.
    #[test]
    fn session_display_merges_key_by_key() {
        let base: toml::Value =
            toml::from_str("[ui.session_display]\nsidebar_always = true").unwrap();
        let over: toml::Value = toml::from_str("[ui.session_display]\ntabs_always = true").unwrap();
        let raw: RawConfig = merge(base, over).try_into().unwrap();
        let sd = raw.into_config().ui.session_display;
        assert!(sd.sidebar_always);
        assert!(sd.tabs_always);
    }

    #[test]
    fn ui_font_defaults_to_none() {
        let config = parse("");
        assert_eq!(config.ui_font, UiFont::default());
    }

    #[test]
    fn ui_font_parses_family_and_size() {
        let config = parse("[ui.font]\nfamily = \"Inter\"\nsize = 12.5");
        assert_eq!(config.ui_font.family.as_deref(), Some("Inter"));
        assert_eq!(config.ui_font.size, Some(12.5));
    }

    #[test]
    fn ui_font_size_clamps_to_one() {
        let config = parse("[ui.font]\nsize = 0.1");
        assert_eq!(config.ui_font.size, Some(1.0));
    }

    #[test]
    fn blank_ui_font_family_is_ignored() {
        let config = parse("[ui.font]\nfamily = \"  \"");
        assert_eq!(config.ui_font.family, None);
    }

    #[test]
    fn icons_default_to_todays_glyphs() {
        let ui = ui_from_toml("");
        assert_eq!(ui.icons, Icons::default());
        assert_eq!(ui.icons.worktree_main, "●");
        assert_eq!(ui.icons.worktree, "○");
        assert_eq!(ui.icons.session, "▪");
        assert_eq!(ui.icons.home, "⌂");
        assert_eq!(ui.icons.project_expanded, "▾");
        assert_eq!(ui.icons.project_collapsed, "▸");
        assert_eq!(ui.icons.pr_open, "⬤");
        assert_eq!(ui.icons.pr_draft, "◯");
        assert_eq!(ui.icons.pr_merged, "⬤");
        assert_eq!(ui.icons.pr_closed, "⬤");
    }

    #[test]
    fn icon_overrides_apply_and_trim() {
        let ui = ui_from_toml("[ui.icons]\nworktree = \" W \"\nhome = \"H\"");
        assert_eq!(ui.icons.worktree, "W");
        assert_eq!(ui.icons.home, "H");
        assert_eq!(ui.icons.worktree_main, "●", "untouched fields keep defaults");
    }

    #[test]
    fn blank_icon_override_falls_back() {
        let ui = ui_from_toml("[ui.icons]\nworktree_main = \"   \"\nsession = \"\"");
        assert_eq!(ui.icons.worktree_main, "●");
        assert_eq!(ui.icons.session, "▪");
    }

    #[test]
    fn pr_status_defaults_off_and_parses_on() {
        assert!(!ui_from_toml("").pr_status);
        assert!(ui_from_toml("[ui]\npr_status = true").pr_status);
    }

    #[test]
    fn focus_outline_defaults_off() {
        let fo = ui_from_toml("").focus_outline;
        assert!(!fo.sidebar);
        assert!(!fo.terminal);
        assert_eq!(fo.color, None);
        assert_eq!(fo.thickness, 1.0);
    }

    #[test]
    fn focus_outline_parses_all_fields() {
        let fo = ui_from_toml(
            "[ui.focus_outline]\nsidebar = true\nterminal = true\ncolor = \"#89b4fa\"\nthickness = 2.5",
        )
        .focus_outline;
        assert!(fo.sidebar);
        assert!(fo.terminal);
        assert_eq!(fo.color, Some(Color32::from_rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(fo.thickness, 2.5);
    }

    #[test]
    fn focus_outline_thickness_clamps() {
        let fo = ui_from_toml("[ui.focus_outline]\nthickness = 0.1").focus_outline;
        assert_eq!(fo.thickness, 0.5);
    }

    #[test]
    fn sidebar_click_focus_defaults_off() {
        assert!(!ui_from_toml("").sidebar_click_focus);
    }

    #[test]
    fn sidebar_click_focus_parses() {
        assert!(ui_from_toml("[ui]\nsidebar_click_focus = true").sidebar_click_focus);
    }

    #[test]
    fn name_templates_default_to_none() {
        let ui = ui_from_toml("");
        assert_eq!(ui.worktree_name, None);
        assert_eq!(ui.project_name, None);
    }

    #[test]
    fn name_templates_parse() {
        let ui =
            ui_from_toml("[ui]\nworktree_name = \"${branch:$name}\"\nproject_name = \"[$name]\"");
        assert_eq!(ui.worktree_name.as_deref(), Some("${branch:$name}"));
        assert_eq!(ui.project_name.as_deref(), Some("[$name]"));
    }

    #[test]
    fn blank_name_templates_are_dropped() {
        let ui = ui_from_toml("[ui]\nworktree_name = \"  \"");
        assert_eq!(ui.worktree_name, None);
    }
}
