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

use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Rgb};
use egui::Color32;
use serde::Deserialize;

use crate::bindings::{self, KeyBinding};

#[derive(Debug, Clone)]
pub struct Config {
    pub palette: Palette,
    pub ui: UiTheme,
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
    /// Prompt only when the session looks busy (agent glyph or spinner title).
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

#[derive(Debug, Clone)]
pub struct UiTheme {
    pub sidebar_background: Option<Color32>,
    pub sidebar_foreground: Option<Color32>,
    pub sidebar_border: Option<Color32>,
    pub sidebar_accent: Option<Color32>,
    /// Fire a desktop notification when a non-visible session needs attention.
    pub notifications: bool,
    /// Ask before the sidebar's per-session `×` kills the PTY.
    pub confirm_session_close: ConfirmSessionClose,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self {
            sidebar_background: None,
            sidebar_foreground: None,
            sidebar_border: None,
            sidebar_accent: None,
            notifications: true,
            confirm_session_close: ConfirmSessionClose::Never,
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
    let alacritty_path = installed_config("alacritty", "toml");
    let alacritree_path = installed_config("alacritree", "toml");

    let empty = || toml::Value::Table(toml::value::Table::new());
    let mut merged = match alacritty_path.as_deref().map(read_toml_value) {
        Some(Ok(Some(v))) => v,
        Some(Err(e)) => {
            log::warn!("failed to load {}: {e}", alacritty_path.as_deref().unwrap().display());
            empty()
        },
        _ => empty(),
    };

    if let Some(path) = alacritree_path.as_deref() {
        match read_toml_value(path) {
            Ok(Some(over)) => merged = merge(merged, over),
            Ok(None) => {},
            Err(e) => log::warn!("failed to load {}: {e}", path.display()),
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
}

/// Subset of alacritty's `[general]` section that alacritree honors.  It
/// lives in the shared `alacritty.toml`, so disabling alacritty's socket
/// disables ours too — the two sockets are separate files, but the intent
/// ("no IPC") is the same.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawGeneral {
    ipc_socket: Option<bool>,
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawUi {
    sidebar_background: Option<RgbStr>,
    sidebar_foreground: Option<RgbStr>,
    sidebar_border: Option<RgbStr>,
    sidebar_accent: Option<RgbStr>,
    notifications: Option<bool>,
    /// When the sidebar × on a session row asks before killing the PTY:
    /// "never" (default) | "busy" | "always".
    confirm_session_close: Option<String>,
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
            confirm_session_close: parse_confirm_session_close(
                self.ui.confirm_session_close.as_deref(),
            ),
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

        Config {
            palette,
            ui,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ui_from_toml(input: &str) -> UiTheme {
        let value: toml::Value = toml::from_str(input).expect("valid toml");
        let raw: RawConfig = value.try_into().expect("valid config");
        raw.into_config().ui
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
}
