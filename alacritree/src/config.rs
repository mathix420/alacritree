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
use schemars::JsonSchema;
use serde::Deserialize;

use crate::bindings::{self, KeyBinding};
use crate::path_style::PathStyle;

/// `[env]` carries whatever the user's environment carries, and a config dump
/// ends up attached to bug reports.  Key names survive: that `FOO` was set is
/// diagnostic, what it was set to is not.
const REDACTED_VALUE: &str = "<redacted>";

#[derive(Debug, Clone, serde::Serialize)]
pub struct Config {
    pub palette: Palette,
    pub ui: UiTheme,
    pub ui_font: UiFont,
    pub workspace: WorkspaceConfig,
    pub font: FontConfig,
    pub cursor: CursorConfig,
    pub scrolling: ScrollingConfig,
    pub window: WindowConfig,
    #[serde(serialize_with = "redacted_env")]
    pub env: HashMap<String, String>,
    pub shell: Option<ShellConfig>,
    pub selection: SelectionConfig,
    pub bindings: Vec<KeyBinding>,
    /// Offer the IPC socket that `alacritree mcp` connects to.  Mirrors
    /// alacritty's `[general] ipc_socket` (default on).
    pub ipc_socket: bool,
    pub debug: DebugConfig,
    /// Start dir for sessions with no explicit workspace (the home tab);
    /// worktree tabs always use their checkout path.  Mirrors alacritty's
    /// `[general] working_directory`, except a leading `~` expands to the
    /// home directory (upstream only expands `~` in config imports) so one
    /// shared config works on every platform.
    pub working_directory: Option<PathBuf>,
    /// Where `state.toml` and the scratchpad notes live, from `[general]
    /// state_dir`.  `None` means the per-user config base.
    pub state_dir: Option<PathBuf>,
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

/// Environment values never serialise.  Enforced on the field rather than by
/// the caller, so no dump of a `Config` can leak one by forgetting to ask.
fn redacted_env<S: serde::Serializer>(
    env: &HashMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(Some(env.len()))?;
    for key in env.keys() {
        map.serialize_entry(key, REDACTED_VALUE)?;
    }
    map.end()
}

impl Config {
    /// The effective config as one line of JSON, carrying only what differs
    /// from the defaults; `None` when nothing does.  Follows ghostty's
    /// `+show-config`, whose `changes-only` is on by default.
    ///
    /// Effective values rather than the config file as written, so reading one
    /// out of a log needs no knowledge of this version's defaults.  Diffed
    /// against the defaults because a whole config is mostly the 256-entry
    /// indexed palette and the sidebar colours, which almost nobody touches
    /// and which would bury the handful of keys that explain a run.
    ///
    /// One line because a log file interleaves writers, and a pretty-printed
    /// block is a block another thread can land in the middle of.
    pub fn changed_from_defaults(&self) -> Option<String> {
        let mine = serde_json::to_value(self).ok()?;
        let stock = serde_json::to_value(stock_config()).ok()?;
        let changed = changes(&mine, &stock)?;
        Some(changed.to_string())
    }
}

/// The config an install with no config file gets, and what "defaults" means
/// anywhere the real config cannot be used.  Not `Config::default`: the
/// built-in key bindings are filled in on the way through `RawConfig`, so the
/// bare struct default carries none, which makes it both the wrong baseline to
/// diff a dump against and the wrong config to hand a running window.
fn stock_config() -> Config {
    RawConfig::default().into_config()
}

/// Every key of `mine` whose value differs from `stock`, recursing into
/// objects so one changed field does not drag its whole section along.
/// Arrays compare whole: a changed element is a changed list, and an index
/// diff would print something that is not a config value.
fn changes(mine: &serde_json::Value, stock: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::Value;

    if mine == stock {
        return None;
    }
    let (Value::Object(mine), Value::Object(stock)) = (mine, stock) else {
        return Some(mine.clone());
    };
    Some(Value::Object(
        mine.iter()
            .filter_map(|(key, value)| {
                Some((key.clone(), changes(value, stock.get(key).unwrap_or(&Value::Null))?))
            })
            .collect(),
    ))
}

/// alacritty's `[debug]` section, plus one alacritree-only key.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DebugConfig {
    /// alacritree-only, set in `alacritree.toml`.  Default on: a crash that
    /// leaves no record is the failure this exists to prevent.
    pub crash_log: bool,
    /// Upstream's name and upstream's default.
    pub persistent_logging: bool,
    /// alacritree-only, set in `alacritree.toml`.  Log what the GPU grid's
    /// paint callback costs: the wall time of issuing a frame, and the GPU's
    /// own time for the upload and each of the three draws.  Off by default;
    /// timer queries are cheap but not free, and the line is only meaningful
    /// to someone reading it.  Needs `[ui] gpu_grid` and a GL 3.3 context.
    /// Keeps this session's log file for as long as it is on, since the
    /// report has nowhere else to go.
    pub gpu_timing: bool,
    /// alacritree-only, set in `alacritree.toml`.  Off by default;
    /// `ALACRITREE_FRAME_LOG` overrides it.
    pub frame_log: bool,
    /// alacritree-only, set in `alacritree.toml`.  Crash artifacts and session
    /// logs go here.  `None` means whatever `logdir::log_dir` resolves.
    pub log_dir: Option<PathBuf>,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            crash_log: true,
            persistent_logging: false,
            gpu_timing: false,
            frame_log: false,
            log_dir: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
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
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FontFace {
    pub family: Option<String>,
    pub style: Option<String>,
}

/// `CursorShape` comes from `vte` and carries no serde derives, so it is
/// written by name.  The spellings are the ones `alacritty.toml` accepts, so a
/// dumped value can be pasted back into a config file.
fn cursor_shape_name<S: serde::Serializer>(
    shape: &CursorShape,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(match shape {
        CursorShape::Block => "Block",
        CursorShape::Underline => "Underline",
        CursorShape::Beam => "Beam",
        CursorShape::HollowBlock => "HollowBlock",
        CursorShape::Hidden => "Hidden",
    })
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CursorConfig {
    #[serde(serialize_with = "cursor_shape_name")]
    pub shape: CursorShape,
    pub blinking: bool,
    pub unfocused_hollow: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ScrollingConfig {
    pub history: usize,
    pub multiplier: u8,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct WindowConfig {
    pub padding_x: f32,
    pub padding_y: f32,
    pub opacity: f32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShellConfig {
    pub program: String,
    pub args: Vec<String>,
}

/// A named shell launch profile from `[[ui.profiles]]`.  Program + args
/// only; cwd and env come from the session as usual.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Profile {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
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

/// The command a profile runs, as shown to the user: `program` followed by
/// `args`, joined by single spaces. A profile with no args shows just the
/// program.
pub fn profile_command(p: &Profile) -> String {
    std::iter::once(p.program.as_str())
        .chain(p.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
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

/// `[ui.drop] quote` as written in the config.  The five concrete modes are
/// ported from wezterm's `quote_dropped_files` so an existing wezterm config
/// carries over unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum Quoting {
    /// Decide per session: a path headed into a distro is a POSIX shell word
    /// no matter what the host OS is.
    #[default]
    Auto,
    None,
    SpacesOnly,
    Posix,
    Windows,
    WindowsAlwaysQuoted,
}

/// `Quoting` with `Auto` already decided against the receiving shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ShellQuoting {
    None,
    SpacesOnly,
    Posix,
    Windows,
    WindowsAlwaysQuoted,
}

impl Quoting {
    pub fn resolve(self, wsl: bool) -> ShellQuoting {
        match self {
            Self::Auto if wsl => ShellQuoting::Posix,
            Self::Auto if cfg!(windows) => ShellQuoting::Windows,
            Self::Auto => ShellQuoting::SpacesOnly,
            Self::None => ShellQuoting::None,
            Self::SpacesOnly => ShellQuoting::SpacesOnly,
            Self::Posix => ShellQuoting::Posix,
            Self::Windows => ShellQuoting::Windows,
            Self::WindowsAlwaysQuoted => ShellQuoting::WindowsAlwaysQuoted,
        }
    }
}

impl ShellQuoting {
    #[must_use]
    pub fn escape(self, path: &str) -> String {
        match self {
            Self::None => path.to_string(),
            Self::SpacesOnly => path.replace(' ', "\\ "),
            // A quoting failure is only possible for a NUL byte, which no
            // path from the OS carries; wezterm collapses it the same way.
            Self::Posix => shlex::try_quote(path).unwrap_or_default().into_owned(),
            Self::Windows => {
                const NEEDS_QUOTING: [char; 5] = [' ', '\t', '\n', '\x0b', '"'];
                if path.chars().any(|c| NEEDS_QUOTING.contains(&c)) {
                    format!("\"{path}\"")
                } else {
                    path.to_string()
                }
            },
            Self::WindowsAlwaysQuoted => format!("\"{path}\""),
        }
    }
}

fn parse_quoting(raw: Option<&str>) -> Quoting {
    match raw {
        None => Quoting::default(),
        Some("auto") => Quoting::Auto,
        Some("none") => Quoting::None,
        Some("spaces_only") => Quoting::SpacesOnly,
        Some("posix") => Quoting::Posix,
        Some("windows") => Quoting::Windows,
        Some("windows_always_quoted") => Quoting::WindowsAlwaysQuoted,
        Some(other) => {
            log::warn!("unknown ui.drop.quote value {other:?}, using \"auto\"");
            Quoting::default()
        },
    }
}

/// How a path is written for the shell that receives it.  Separate from
/// `DropConfig` because a paste spells paths too, and must not be handed flags
/// about whether drops are accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PathSpelling {
    pub quote: Quoting,
    /// Rewrite a Windows path to its distro-side spelling before it reaches a
    /// WSL shell, where a `C:\` path resolves to nothing.
    pub wsl_translate: bool,
}

impl Default for PathSpelling {
    fn default() -> Self {
        Self { quote: Quoting::Auto, wsl_translate: true }
    }
}

/// `[ui.drop]`: what dragging files onto the window does.  Every target
/// accepts drops by default; each one can be switched off on its own, and
/// `enabled` turns the lot off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DropConfig {
    /// Master switch; false ignores every drop.
    pub enabled: bool,
    pub terminal: bool,
    pub sidebar: bool,
    pub scratchpad: bool,
    pub spelling: PathSpelling,
    /// Tint the region a drop would land on while files hover.
    pub highlight: bool,
}

impl Default for DropConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            terminal: true,
            sidebar: true,
            scratchpad: true,
            spelling: PathSpelling::default(),
            highlight: true,
        }
    }
}

/// `[ui.paste]`: what Paste does when the clipboard holds no text.  Both
/// fallbacks are independent — one can be off without affecting the other, and
/// both off leaves Paste exactly as it was.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PasteConfig {
    /// Paste the paths of files and folders copied in a file manager.
    pub files: bool,
    /// Write a clipboard bitmap to a PNG and paste its path.
    pub image: bool,
    /// Where those PNGs go.  `None` is the app-owned default, the only
    /// directory the count cap is ever applied to.
    pub image_dir: Option<PathBuf>,
    /// How many generated PNGs the owned directory keeps.  At least one: the
    /// file a paste just handed to the shell has to still be there when the
    /// shell opens it, so zero is not a reachable state.
    pub image_keep: usize,
}

impl Default for PasteConfig {
    fn default() -> Self {
        Self { files: true, image: true, image_dir: None, image_keep: 20 }
    }
}

impl PasteConfig {
    /// The directory to write into, and whether alacritree owns it.  Ownership
    /// is what licenses deleting anything: a directory the user named may hold
    /// files alacritree never wrote.
    pub fn image_target(&self) -> (PathBuf, bool) {
        match &self.image_dir {
            Some(dir) => (dir.clone(), false),
            None => (default_image_dir(), true),
        }
    }
}

/// `[ui.herdr]`: whether alacritree lists agents running under a herdr server
/// in the sidebar. On by default; a probe with no herdr binary or server
/// present costs nothing, so an unmodified config pays no price for it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HerdrConfig {
    /// Discover herdr servers and list their agents in the sidebar.
    pub enabled: bool,
    /// How often a reachable herdr server is re-polled for agent state.
    pub poll_interval: Duration,
    /// List agents whose working directory matches no worktree, under Home.
    pub show_unmatched: bool,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self { enabled: true, poll_interval: Duration::from_millis(2000), show_unmatched: true }
    }
}

/// Disposable by nature.  Unix keeps captures in the user's cache rather than
/// a shared fixed-name tmp directory; Windows' `%TEMP%` is already per-user and
/// remains reachable from WSL through the usual automount.
#[cfg(unix)]
pub fn default_image_dir() -> PathBuf {
    let cache_home = xdg::BaseDirectories::with_prefix("alacritree").get_cache_home();
    // SAFETY: `geteuid` takes no arguments and has no safety preconditions.
    let uid = unsafe { libc::geteuid() };
    unix_default_image_dir(cache_home, &std::env::temp_dir(), uid)
}

#[cfg(unix)]
fn unix_default_image_dir(cache_home: Option<PathBuf>, temp_dir: &Path, uid: u32) -> PathBuf {
    cache_home.unwrap_or_else(|| temp_dir.join(format!("alacritree-{uid}")))
}

#[cfg(not(unix))]
pub fn default_image_dir() -> PathBuf {
    std::env::temp_dir().join("alacritree").join("clipboard")
}

/// How the sidebar scroll areas draw their scrollbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
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

/// A glyph alacritree ships and guarantees coverage for.  Paint helpers take
/// this rather than `&str` so a built-in glyph cannot be introduced as a bare
/// literal that the baked subset never learns about.  User-configured
/// `[ui.icons]` overrides stay plain strings — they are outside the guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BakedGlyph(&'static str);

impl BakedGlyph {
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Declares a glyph constant and enrols it in `$slice` in one step, so the
/// aggregate cannot drift from the constants it describes.
macro_rules! baked_glyphs {
    ($slice:ident: $($(#[$m:meta])* $name:ident = $glyph:literal;)*) => {
        $($(#[$m])* pub(crate) const $name: BakedGlyph = BakedGlyph($glyph);)*
        #[cfg(test)]
        pub(crate) const $slice: &[BakedGlyph] = &[$($name),*];
    };
}

baked_glyphs! {
    DEFAULT_ICON_GLYPHS:
    /// Text-presentation magnifier (U+2315).  Not in egui's bundled fonts; it
    /// resolves through the system fallback chain `fonts.rs` registers.
    DEFAULT_SEARCH_ICON = "⌕";
    /// Default glyphs for every other `[ui.icons]` key, shared between
    /// `Icons::default()` and the paint sites' `resolve_icon` fallback — a
    /// table override that styles a key without setting `glyph` still needs
    /// the real default to fall back to, not a blank string.
    DEFAULT_WORKTREE_MAIN_ICON = "●";
    DEFAULT_WORKTREE_ICON = "○";
    DEFAULT_SESSION_ICON = "▪";
    /// A pane a terminal workspace manager owns rather than alacritree.  A
    /// split square says "multiplexed elsewhere" at row size, where a
    /// vendor's logo would only say "smudge" — and it stays neutral as more
    /// than one such manager becomes supportable.
    DEFAULT_HERDR_ICON = "◫";
    DEFAULT_HOME_ICON = "⌂";
    DEFAULT_PROJECT_EXPANDED_ICON = "▾";
    DEFAULT_PROJECT_COLLAPSED_ICON = "▸";
    DEFAULT_PR_OPEN_ICON = "⬤";
    DEFAULT_PR_DRAFT_ICON = "◯";
    DEFAULT_PR_MERGED_ICON = "⬤";
    DEFAULT_PR_CLOSED_ICON = "⬤";
    DEFAULT_UPSTREAM_LEVEL_ICON = "✓";
    DEFAULT_UPSTREAM_DIVERGED_ICON = "⇅";
    DEFAULT_UPSTREAM_GONE_ICON = "⌫";
    DEFAULT_UPSTREAM_UNTRACKED_ICON = "↑";
}

baked_glyphs! {
    CHROME_GLYPHS:
    /// Every recognized agent shares one status mark; identity belongs in the
    /// tooltip and title instead of changing the sidebar's visual grammar.
    DEFAULT_AGENT_ICON = "◇";
    /// An agent held at a dialog it needs a human to answer.  Solid against
    /// the idle diamond's outline, so the two read apart before the attention
    /// color does any work.  Deliberately the codepoint
    /// `DEFAULT_WORKTREE_MAIN_ICON` already carries: the two never share a
    /// slot, and a new one would mean rebuilding the baked face for a shape
    /// it already has.
    DEFAULT_BLOCKED_ICON = "●";
    /// Action buttons.  Each takes a config key of its own; the glyphs are
    /// declared here because coverage is owed regardless of who names them.
    DEFAULT_ADD_ICON = "+";
    DEFAULT_CLOSE_ICON = "×";
    DEFAULT_REFRESH_ICON = "↻";
    DEFAULT_REORDER_ICON = "⇅";
    /// Painted directly as literals at their call sites — labels, hover text —
    /// rather than through these constants, so nothing in ordinary builds
    /// reads them.  They are declared here only to give the coverage check
    /// something to assert against, hence gated to test builds; the glyphs
    /// themselves ship in the app regardless of that gate.
    #[cfg(test)]
    DEFAULT_MIDDOT_GLYPH = "·";
    #[cfg(test)]
    DEFAULT_EMDASH_GLYPH = "—";
    #[cfg(test)]
    DEFAULT_BULLET_GLYPH = "•";
    #[cfg(test)]
    DEFAULT_ELLIPSIS_GLYPH = "…";
    #[cfg(test)]
    DEFAULT_DOWN_ARROW_GLYPH = "↓";
    #[cfg(test)]
    DEFAULT_DRAG_HANDLE_GLYPH = "⠿";
    #[cfg(test)]
    DEFAULT_CURSOR_BLOCK_GLYPH = "▌";
    /// herdr's "working" mark in the symbol set it offers alongside its dots.
    /// A harness-owned row paints the state its harness reports in that
    /// harness's vocabulary, so the glyph ships even though no alacritree
    /// default names it.  The rest of that vocabulary is `● ○ · × ✓`, which
    /// the slices above already carry.
    #[cfg(test)]
    DEFAULT_HALF_CIRCLE_GLYPH = "◐";
}

/// What happens when the on-screen workspace stops having sessions, whether a
/// close or a worktree deletion took the last one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum LastSessionClose {
    /// Recycle a shell in place — the workspace always has a live session,
    /// so the last session is by design unclosable.
    #[default]
    Respawn,
    /// Move to the project's main checkout when it has a live session,
    /// otherwise home (which spawns a shell only if it has none).
    Navigate,
    /// Move to the nearest session in the flat session ring, otherwise home.
    RingGlobal,
    /// Move to the nearest session in the removed workspace's own project,
    /// then to the nearest anywhere in the ring, otherwise home.
    RingProject,
}

impl LastSessionClose {
    /// Whether the destination comes from the session ring.  Both removal
    /// paths build that ring only when this is true, so the default costs
    /// no allocation.
    pub fn rings(self) -> bool {
        matches!(self, Self::RingGlobal | Self::RingProject)
    }

    /// Whether the search is confined to the removed workspace's project
    /// before it widens to the whole ring.
    pub fn prefers_project(self) -> bool {
        matches!(self, Self::RingProject)
    }
}

fn parse_last_session_close(raw: Option<&str>) -> LastSessionClose {
    match raw {
        None => LastSessionClose::default(),
        Some("respawn") => LastSessionClose::Respawn,
        Some("navigate") => LastSessionClose::Navigate,
        Some("ring_global") => LastSessionClose::RingGlobal,
        Some("ring_project") => LastSessionClose::RingProject,
        Some(other) => {
            log::warn!("unknown ui.last_session_close value {other:?}, using \"respawn\"");
            LastSessionClose::default()
        },
    }
}

/// How far the projects sidebar goes when the cursor's row stops being
/// rendered.  Both values keep the cursor; they differ only in whether the
/// terminal comes along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum SidebarFocus {
    /// A filtered-out cursor climbs to its nearest visible ancestor and is
    /// restored when the filter widens; a removed cursor slides to a sibling
    /// bounded by its parent.  The terminal stays where it is.
    #[default]
    Preserve,
    /// `Preserve`, and a removal landing that has a live session also moves
    /// the terminal to it.
    Follow,
}

impl SidebarFocus {
    pub fn follows(self) -> bool {
        matches!(self, Self::Follow)
    }
}

fn parse_sidebar_focus(raw: Option<&str>) -> SidebarFocus {
    match raw {
        None => SidebarFocus::default(),
        Some("preserve") => SidebarFocus::Preserve,
        Some("follow") => SidebarFocus::Follow,
        Some(other) => {
            log::warn!("unknown ui.sidebar_focus value {other:?}, using \"preserve\"");
            SidebarFocus::default()
        },
    }
}

/// `[ui] sidebar_scroll_align`: where a row a sidebar scrolled to is parked.
/// Governs both panels and both reasons to scroll, because it describes the
/// resting position rather than what chose the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum ScrollAlign {
    /// egui's minimal scroll: move just far enough to bring the row into
    /// view, which leaves it against whichever edge it entered from.
    #[default]
    Minimal,
    /// Park the row in the middle of the panel.  egui clamps to the scroll
    /// range, so a short list stays put instead of overscrolling.
    Center,
}

impl ScrollAlign {
    pub fn align(self) -> Option<egui::Align> {
        match self {
            Self::Minimal => None,
            Self::Center => Some(egui::Align::Center),
        }
    }
}

fn parse_scroll_align(raw: Option<&str>) -> ScrollAlign {
    match raw {
        None => ScrollAlign::default(),
        Some("minimal") => ScrollAlign::Minimal,
        Some("center") => ScrollAlign::Center,
        Some(other) => {
            log::warn!("unknown ui.sidebar_scroll_align value {other:?}, using \"minimal\"");
            ScrollAlign::default()
        },
    }
}

/// `[ui] search_scope`: whether a fuzzy query is confined by the panel's active
/// toggle filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum SearchScope {
    /// A query narrows the rows the toggles already allow.
    #[default]
    Filtered,
    /// A query is evaluated against every row; the toggles stand aside until it
    /// empties.
    All,
}

fn parse_search_scope(raw: Option<&str>) -> SearchScope {
    match raw {
        None => SearchScope::default(),
        Some("filtered") => SearchScope::Filtered,
        Some("all") => SearchScope::All,
        Some(other) => {
            log::warn!("unknown ui.search_scope value {other:?}, using \"filtered\"");
            SearchScope::default()
        },
    }
}

/// `[ui.session_reorder] scope`: how far a session may travel when the user
/// reorders it.  Widening it makes a reorder step able to change which
/// workspace a session belongs to, which is why the default keeps a session
/// inside the one it was spawned in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum ReorderScope {
    /// Only among the sessions of its own workspace.
    #[default]
    Workspace,
    /// Across the worktrees of the project that owns its workspace.  Home
    /// belongs to no project, so a home session stays home.
    Project,
    /// Home and every project's worktrees, in sidebar order.
    Anywhere,
}

fn parse_reorder_scope(raw: Option<&str>) -> ReorderScope {
    match raw {
        None => ReorderScope::default(),
        Some("workspace") => ReorderScope::Workspace,
        Some("project") => ReorderScope::Project,
        Some("anywhere") => ReorderScope::Anywhere,
        Some(other) => {
            log::warn!("unknown ui.session_reorder.scope value {other:?}, using \"workspace\"");
            ReorderScope::default()
        },
    }
}

/// Whether session rows can be dragged, and how far a reorder may carry a
/// session.  `drag` is a startup default only: the app copies it into runtime
/// state that `ToggleSessionDrag` flips, and nothing is persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct SessionReorder {
    pub drag: bool,
    pub scope: ReorderScope,
}

/// `[ui] sidebar_tooltips`: when a sidebar row offers its full name on hover.
/// Governs both sidebars — a git panel row's path answers to it the same way a
/// worktree or session name does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum SidebarTooltips {
    /// Never — a name the panel cut off stays cut off.
    Off,
    /// Only where the row had to ellipsize the name.
    #[default]
    Elided,
    /// On every row.  egui opens the next tooltip instantly while one was just
    /// shown, so a row that offers none breaks that chain and the name after it
    /// has to wait out the delay again; offering one everywhere keeps a sweep
    /// down the list from stalling on the short names.
    Always,
}

fn parse_sidebar_tooltips(raw: Option<&str>) -> SidebarTooltips {
    match raw {
        None => SidebarTooltips::default(),
        Some("off") => SidebarTooltips::Off,
        Some("elided") => SidebarTooltips::Elided,
        Some("always") => SidebarTooltips::Always,
        Some(other) => {
            log::warn!("unknown ui.sidebar_tooltips value {other:?}, using \"elided\"");
            SidebarTooltips::default()
        },
    }
}

/// Whether per-session UI (sidebar session rows, tab-strip segments) renders
/// for a single-session workspace instead of waiting for the two-session
/// threshold.  These are startup defaults only: the app copies them into
/// runtime state that key bindings can toggle, and nothing is persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SessionDisplay {
    pub sidebar_always: bool,
    pub tabs_always: bool,
}

/// alacritree-only `[ui.font]`: font family/size for the chrome (sidebars,
/// modals — everything that isn't the terminal grid).  Both fields default
/// to deriving from `[font]`, so an absent table changes nothing.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UiFont {
    pub family: Option<String>,
    /// Typographic points, same unit as `[font] size`; clamped to ≥ 1.0.
    pub size: Option<f32>,
    /// Family used for bold chrome text; falls back to `family` when unset.
    pub bold_family: Option<String>,
    /// Family used for italic chrome text; falls back to `family` when unset.
    pub italic_family: Option<String>,
    /// Family used for bold-italic chrome text; falls back to `family` when unset.
    pub bold_italic_family: Option<String>,
    /// Register the bundled symbol face as the last resort in each chrome
    /// family.  On by default: it is only ever reached for a glyph no earlier
    /// face could draw, so it cannot change a chrome that already renders.
    pub builtin_symbols: bool,
}

impl Default for UiFont {
    fn default() -> Self {
        Self {
            family: None,
            size: None,
            bold_family: None,
            italic_family: None,
            bold_italic_family: None,
            builtin_symbols: true,
        }
    }
}

/// Sidebar glyphs, each independently overridable from `[ui.icons]` as a bare
/// glyph or a table styling color/weight/slant/size.  An absent key falls back
/// to the default below; a table with no `glyph` key keeps the default glyph
/// but applies its own styling.
///
/// One key per action, not per glyph: the three `×` buttons remove a project,
/// delete a worktree and its branch, and close a session, so only separate
/// keys let the destructive one be marked. `reorder` and `upstream_diverged`
/// share a default glyph and are otherwise unrelated.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Icons {
    /// Glyph prefixing the sidebar search prompt.
    pub search: IconStyle,
    pub worktree_main: IconStyle,
    pub worktree: IconStyle,
    pub session: IconStyle,
    pub herdr: IconStyle,
    pub home: IconStyle,
    pub project_expanded: IconStyle,
    pub project_collapsed: IconStyle,
    pub pr_open: IconStyle,
    pub pr_draft: IconStyle,
    pub pr_merged: IconStyle,
    pub pr_closed: IconStyle,
    pub upstream_level: IconStyle,
    pub upstream_diverged: IconStyle,
    pub upstream_gone: IconStyle,
    pub upstream_untracked: IconStyle,
    pub add_project: IconStyle,
    pub new_worktree: IconStyle,
    pub new_session: IconStyle,
    pub remove_project: IconStyle,
    pub delete_worktree: IconStyle,
    pub close_session: IconStyle,
    pub refresh: IconStyle,
    pub reorder: IconStyle,
}

/// `[ui.focus_outline]`: stroke a border around a panel while it owns
/// keyboard focus.  Per-panel toggles (`sidebar` covers both side panels),
/// shared color/thickness; both toggles default off so unmodified config
/// keeps today's look.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
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

/// A default icon: just the glyph, no styling.
fn glyph(g: BakedGlyph) -> IconStyle {
    IconStyle { glyph: Some(g.as_str().to_string()), ..Default::default() }
}

impl Default for Icons {
    fn default() -> Self {
        Self {
            search: glyph(DEFAULT_SEARCH_ICON),
            worktree_main: glyph(DEFAULT_WORKTREE_MAIN_ICON),
            worktree: glyph(DEFAULT_WORKTREE_ICON),
            session: glyph(DEFAULT_SESSION_ICON),
            herdr: glyph(DEFAULT_HERDR_ICON),
            home: glyph(DEFAULT_HOME_ICON),
            project_expanded: glyph(DEFAULT_PROJECT_EXPANDED_ICON),
            project_collapsed: glyph(DEFAULT_PROJECT_COLLAPSED_ICON),
            pr_open: glyph(DEFAULT_PR_OPEN_ICON),
            pr_draft: glyph(DEFAULT_PR_DRAFT_ICON),
            pr_merged: glyph(DEFAULT_PR_MERGED_ICON),
            pr_closed: glyph(DEFAULT_PR_CLOSED_ICON),
            upstream_level: glyph(DEFAULT_UPSTREAM_LEVEL_ICON),
            upstream_diverged: glyph(DEFAULT_UPSTREAM_DIVERGED_ICON),
            upstream_gone: glyph(DEFAULT_UPSTREAM_GONE_ICON),
            upstream_untracked: glyph(DEFAULT_UPSTREAM_UNTRACKED_ICON),
            add_project: glyph(DEFAULT_ADD_ICON),
            new_worktree: glyph(DEFAULT_ADD_ICON),
            new_session: glyph(DEFAULT_ADD_ICON),
            remove_project: glyph(DEFAULT_CLOSE_ICON),
            delete_worktree: glyph(DEFAULT_CLOSE_ICON),
            close_session: glyph(DEFAULT_CLOSE_ICON),
            refresh: glyph(DEFAULT_REFRESH_ICON),
            reorder: glyph(DEFAULT_REORDER_ICON),
        }
    }
}

/// A sidebar icon's glyph and how to paint it.  Parses from a bare string,
/// accepted as glyph-only, or a table that also styles color, weight, slant,
/// and size.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct IconStyle {
    pub glyph: Option<String>,
    pub color: Option<Color32>,
    pub bold: bool,
    pub italic: bool,
    /// Logical pixels before `ui_scale`; clamped to the icon's slot at paint.
    pub size: Option<f32>,
}

impl IconStyle {
    pub fn or_glyph<'a>(&'a self, default: &'a str) -> &'a str {
        self.glyph.as_deref().map(str::trim).filter(|g| !g.is_empty()).unwrap_or(default)
    }
}

/// How one text span is emphasized.  `color: None` inherits whatever color the
/// site normally paints, so an emphasis that sets only `bold` still tracks the
/// theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct TextEmphasis {
    pub color: Option<Color32>,
    pub bold: bool,
    pub italic: bool,
}

/// `[ui.path_style]`: how each site spells a path, plus the two emphases the
/// `Zed` style paints with.  Every field defaults to today's rendering.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
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

/// One correction to a decoration the font placed: a shift in physical pixels,
/// a shift in points, or a multiplier.  kitty's grammar, so a value copied
/// from a kitty config parses the same way here.  Where it lands can still
/// differ: kitty derives its double and curly underline positions from the
/// face's underline position, while here those two styles are placed from
/// the descent instead, so `underline_position` does not reach them.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum Adjust {
    Pixels(f32),
    Points(f32),
    Scale(f32),
}

impl Adjust {
    /// Draw what the font asked for.
    pub const NONE: Self = Self::Pixels(0.0);

    /// `"2px"`, `"2pt"`, a bare `"2"` (points, which is how kitty spells it),
    /// or `"150%"`.  `None` for anything else, a signed percentage included.
    pub fn parse(raw: &str) -> Option<Self> {
        if let Some(number) = raw.strip_suffix('%') {
            let percent = finite(number)?;
            return (percent >= 0.0).then_some(Self::Scale(percent / 100.0));
        }
        if let Some(number) = raw.strip_suffix("px") {
            return finite(number).map(Self::Pixels);
        }
        finite(raw.strip_suffix("pt").unwrap_or(raw)).map(Self::Points)
    }

    /// `value` is already in physical pixels, so a point shift scales by
    /// `pixels_per_point` and a percentage multiplies what the font resolved
    /// to rather than the em fraction it was read from.
    pub fn apply(self, value: f32, pixels_per_point: f32) -> f32 {
        match self {
            Self::Pixels(px) => value + px,
            Self::Points(pt) => value + pt * pixels_per_point,
            Self::Scale(factor) => value * factor,
        }
    }
}

/// `"inf"` and `"nan"` parse as `f32` and would put a line nowhere at all.
fn finite(raw: &str) -> Option<f32> {
    raw.parse::<f32>().ok().filter(|value| value.is_finite())
}

/// `[ui.decorations]`: corrections to what the font reports for its underline
/// and strikeout, for a face whose tables are wrong.  Every knob is a no-op by
/// default, so an unmodified config draws what the face asked for.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Decorations {
    pub underline_position: Adjust,
    pub underline_thickness: Adjust,
    pub strikeout_position: Adjust,
    pub strikeout_thickness: Adjust,
}

impl Default for Decorations {
    fn default() -> Self {
        Self {
            underline_position: Adjust::NONE,
            underline_thickness: Adjust::NONE,
            strikeout_position: Adjust::NONE,
            strikeout_thickness: Adjust::NONE,
        }
    }
}

/// A knob that will not parse logs and behaves as `"0"`, the way the rest of
/// this file treats a value it does not recognize.
fn parse_adjust(field: &str, raw: Option<&str>) -> Adjust {
    let Some(text) = raw else {
        return Adjust::NONE;
    };
    Adjust::parse(text).unwrap_or_else(|| {
        log::warn!("unusable ui.decorations.{field} value {text:?}, using \"0\"");
        Adjust::NONE
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UiTheme {
    pub sidebar_background: Option<Color32>,
    pub sidebar_foreground: Option<Color32>,
    pub sidebar_border: Option<Color32>,
    pub sidebar_accent: Option<Color32>,
    pub sidebar_attention: Option<Color32>,
    /// Fire a desktop notification when a non-visible session needs attention.
    pub notifications: bool,
    /// How long an attention trigger must survive without the session going
    /// back to work before it pings.  Zero pings on the trigger itself.
    pub attention_grace: Duration,
    /// Ask before the sidebar's per-session `×` kills the PTY.
    pub confirm_session_close: ConfirmSessionClose,
    /// Ask before the sidebar's `×` detaches from a harness-managed pane.
    /// Its own switch rather than a mode of [`Self::confirm_session_close`]:
    /// a detach destroys nothing — the pane keeps running under its harness
    /// and its row comes back — so the busy question a close asks has no
    /// answer here, and a user who wants no close prompt may still want to
    /// be asked before losing the view.
    pub confirm_session_detach: bool,
    /// What closing the last session in the on-screen workspace does.
    pub last_session_close: LastSessionClose,
    /// How the projects sidebar repairs a cursor whose row stopped rendering.
    pub sidebar_focus: SidebarFocus,
    /// Whether the projects sidebar scrolls to the session on screen when it
    /// changes.
    pub sidebar_follow_active: bool,
    /// Where a row a sidebar scrolled to is parked.
    pub sidebar_scroll_align: ScrollAlign,
    /// Whether a fuzzy query is confined by the panel's active toggle filters.
    pub search_scope: SearchScope,
    /// When a sidebar row spells its full name out on hover.
    pub sidebar_tooltips: SidebarTooltips,
    /// Whether a sidebar icon explains itself on hover — what a button does,
    /// what a status badge reports.  A separate axis from
    /// [`Self::sidebar_tooltips`], which reveals a name the row had to cut off:
    /// an icon's hint never depends on the panel's width.
    pub icon_tooltips: bool,
    /// Show single-session sidebar rows / tab segments ([`SessionDisplay`]).
    pub session_display: SessionDisplay,
    /// Mouse-drag gate and travel limit for reordering sessions
    /// ([`SessionReorder`]).
    pub session_reorder: SessionReorder,
    /// Draw the terminal grid through an OpenGL paint callback instead of
    /// handing epaint a mesh: one twelve-byte record per cell, and the vertex
    /// shader derives the quads.  Off by default — it needs a GL 3 context and
    /// bypasses the renderer every other panel goes through, so an unmodified
    /// config keeps the path that has always drawn the grid.  A context too
    /// old for instanced arrays logs once, costs the frame it was found on,
    /// and paints the mesh from the next one.
    pub gpu_grid: bool,
    /// Corrections applied to the underline and strikeout the font placed
    /// ([`Decorations`]).  Only the GPU grid reads these; the mesh path draws
    /// a straight rule at a fixed offset either way.
    pub decorations: Decorations,
    /// Paint PR-status badges on worktree rows (and poll `gh` for expanded
    /// projects' worktrees).  Off by default so an unmodified config spawns
    /// no `gh` processes; when enabled it is best-effort like the diff-base
    /// lookup: no `gh`, no auth, or no PR silently paints nothing.
    pub pr_status: bool,
    /// Paint a badge showing each worktree branch's upstream state.  Off by
    /// default so an unmodified config does no extra ref work.  The state comes
    /// from local refs only — nothing fetches, so a branch deleted on the remote
    /// still reads as tracked until something prunes.
    pub upstream_status: bool,
    /// Re-check on a 1.5 s tick whether each listed worktree's checkout is
    /// still on disk, so a `git worktree remove` typed into one of our own
    /// sessions greys the row without waiting for a manual refresh.  On by
    /// default; the escape hatch exists because the probe is a `stat` per
    /// listed row and an exotic filesystem could make that expensive.
    pub worktree_liveness: bool,
    /// `[ui] pr_status_concurrency`: max `gh` lookups in flight at once.
    /// Unset lets the pool decide, which is one below its own background
    /// ceiling so a lookup can never take the last slot local work needs.
    /// A value lowers that; nothing raises it, because the pool's ceiling
    /// binds underneath either way.
    pub pr_status_concurrency: Option<usize>,
    pub icons: Icons,
    pub focus_outline: FocusOutline,
    /// `[ui] scrollbar`: sidebar scrollbar style, "floating" (default) or
    /// "solid" (reserved gutter, never covers row icons).
    pub scrollbar: ScrollbarStyle,
    /// `[ui] sidebar_click_focus`: clicking a sidebar moves keyboard focus to
    /// it (so filter typing works without the focus shortcut).  Off by default
    /// so unmodified configs keep click-through-to-terminal behavior.
    pub sidebar_click_focus: bool,
    /// `[ui] focus_priority_boost`: put the session on screen one scheduling
    /// class above normal — its shell and every process that shell starts, at
    /// any depth — so a build saturating the machine cannot starve what the
    /// user is typing into.  Follows focus, and raises nothing while the
    /// window is in the background.  Off by default.  Windows only.
    pub focus_priority_boost: bool,
    /// `[ui] async_session_spawn`: open a session's PTY on a worker instead
    /// of inside the frame that asked for it.  Creating a console process
    /// costs milliseconds when the machine is idle and hundreds when it is
    /// busy, and the frame pays all of it, so the click that opens a tab is
    /// what stutters.  The tab appears at once and starts painting when its
    /// PTY attaches; anything typed in between is replayed.  Off by default.
    pub async_session_spawn: bool,
    /// `[ui] reap_descendants_on_close`: end everything a session started when
    /// that session closes, at any depth.  The console reaps only the clients
    /// attached to it, so a process that left the console — an editor's search
    /// helper, anything started detached — otherwise outlives the terminal.  A
    /// process that means to survive can still say so with
    /// `CREATE_BREAKAWAY_FROM_JOB`.  Off by default.  Windows only.
    pub reap_descendants_on_close: bool,
    /// `[ui] vsync`: block each present until the display's next refresh.  On
    /// by default, as upstream eframe has it.  Turning it off presents a
    /// finished frame immediately, trading tearing for the queueing delay
    /// between a keystroke's frame and the screen.
    pub vsync: bool,
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
    /// `[ui.drop]`: what a file dragged onto the window does.
    pub drop: DropConfig,
    /// `[ui.paste]`: what Paste does with a clipboard that holds no text.
    pub paste: PasteConfig,
    /// `[ui.herdr]`: whether agents running under a herdr server appear in
    /// the sidebar, and how often their state is re-polled.
    pub herdr: HerdrConfig,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self {
            sidebar_background: None,
            sidebar_foreground: None,
            sidebar_border: None,
            sidebar_accent: None,
            sidebar_attention: None,
            notifications: true,
            attention_grace: Duration::ZERO,
            confirm_session_close: ConfirmSessionClose::Never,
            confirm_session_detach: true,
            last_session_close: LastSessionClose::Respawn,
            sidebar_focus: SidebarFocus::default(),
            sidebar_follow_active: false,
            sidebar_scroll_align: ScrollAlign::default(),
            search_scope: SearchScope::default(),
            sidebar_tooltips: SidebarTooltips::default(),
            icon_tooltips: true,
            session_display: SessionDisplay::default(),
            session_reorder: SessionReorder::default(),
            gpu_grid: false,
            decorations: Decorations::default(),
            pr_status: false,
            upstream_status: false,
            worktree_liveness: true,
            pr_status_concurrency: None,
            icons: Icons::default(),
            focus_outline: FocusOutline::default(),
            scrollbar: ScrollbarStyle::Floating,
            sidebar_click_focus: false,
            focus_priority_boost: false,
            async_session_spawn: false,
            reap_descendants_on_close: false,
            vsync: true,
            worktree_name: None,
            project_name: None,
            path_style: PathStyleConfig::default(),
            drop: DropConfig::default(),
            paste: PasteConfig::default(),
            herdr: HerdrConfig::default(),
        }
    }
}

/// Where new git worktrees are created.  alacritree-only, lives under
/// `[workspace]` in `alacritree.toml`.  Every base directory — default,
/// global, or override — gets the `<project>-<hash>/<branch>` layout beneath
/// it; changing these options never moves existing worktrees because
/// discovery goes through `git worktree list`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct WorkspaceConfig {
    /// Global base directory for new worktrees; `None` means the built-in
    /// `~/.alacritree/worktrees`.
    pub worktree_dir: Option<PathBuf>,
    pub overrides: Vec<WorktreeOverride>,
}

/// Per-project base-directory override, matched against the project root.
#[derive(Debug, Clone, serde::Serialize)]
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
            debug: DebugConfig::default(),
            working_directory: None,
            state_dir: None,
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

/// One config file inside an explicitly named directory.  Both stems resolve
/// there and nowhere else, so a directory holding only `alacritree.toml` runs
/// without an `alacritty.toml` rather than quietly merging the installed one:
/// an override the search path can still reach is not an override.
fn named_config(dir: &Path, stem: &str, suffix: &str) -> Option<PathBuf> {
    let candidate = dir.join(format!("{stem}.{suffix}"));
    candidate.exists().then_some(candidate)
}

/// Where an `alacritree.toml` belongs when none exists yet: the head of the
/// search path, so a file written there is the one [`load`] picks up.
pub fn preferred_alacritree_path() -> PathBuf {
    alacritty_config_dir().join("alacritree.toml")
}

#[cfg(not(windows))]
fn alacritty_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("alacritty")
}

#[cfg(windows)]
fn alacritty_config_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join("alacritty")
}

/// Load the config, and report which files it came from so the startup log
/// can record them.
pub fn load(config_dir: Option<&Path>, overrides: &[toml::Value]) -> (Config, Vec<ConfigFile>) {
    let (files, merged) = assemble(config_dir, overrides);
    for file in &files {
        if let (Some(path), Some(e)) = (&file.path, &file.error) {
            log::warn!("failed to load {}: {e}", path.display());
        }
    }

    let raw: RawConfig = match merged.try_into() {
        Ok(r) => r,
        Err(e) => {
            // `stock_config`, not `Config::default`: the built-in key bindings
            // are filled in on the way through `RawConfig`, so falling back to
            // the bare struct default would answer a typo in the config with a
            // terminal that has no bindings at all — no paste, no copy, no font
            // size, and no way to reach the config to fix it.
            log::warn!("invalid alacritty/alacritree config, using defaults: {e}");
            return (stock_config(), files);
        },
    };

    (raw.into_config(), files)
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

pub fn diagnose(config_dir: Option<&Path>, overrides: &[toml::Value]) -> ConfigDiagnosis {
    let (files, merged) = assemble(config_dir, overrides);
    let schema_error = merged.try_into::<RawConfig>().err().map(|e| e.to_string());
    ConfigDiagnosis { files, schema_error }
}

/// Read both config files off the search path and merge them, alacritree over
/// alacritty, then the `-o` overrides over both.  A file that fails to parse
/// contributes nothing and is reported through its [`ConfigFile::error`].
fn assemble(
    config_dir: Option<&Path>,
    overrides: &[toml::Value],
) -> (Vec<ConfigFile>, toml::Value) {
    let mut merged = toml::Value::Table(toml::value::Table::new());
    let mut files = Vec::new();

    for stem in ["alacritty", "alacritree"] {
        let path = match config_dir {
            Some(dir) => named_config(dir, stem, "toml"),
            None => installed_config(stem, "toml"),
        };
        let mut error = None;
        match path.as_deref().map(read_toml_value) {
            Some(Ok(Some(value))) => merged = merge(merged, value),
            Some(Err(e)) => error = Some(e.to_string()),
            _ => {},
        }
        files.push(ConfigFile { stem, path, error });
    }

    // Through the same merge as the files, so `-o` and a line in
    // `alacritree.toml` mean the same thing.  One consequence worth knowing:
    // arrays concatenate, so `-o` adds a key binding rather than replacing the
    // list, exactly as writing it into the file would.
    for value in overrides {
        merged = merge(merged, value.clone());
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

/// The JSON Schema for a config file, reflected off the same `Raw*` structs
/// serde reads, so the published schema cannot describe a key the parser does
/// not accept.  Lives here rather than beside the CLI command that prints it
/// because those structs are private to this module.
pub fn json_schema() -> schemars::Schema {
    schemars::schema_for!(RawConfig)
}

// --- Raw deserialization ---------------------------------------------------
//
// These structs are the whole of what alacritree reads out of the two TOML
// files, so they are also what `alacritree schema` reflects over to publish a
// JSON Schema.  Every field's doc comment becomes the hover text an editor
// shows for that key; a field left undocumented is a key nobody can look up
// without reading this file.
//
// Fields whose value is a closed set carry `#[schemars(extend("enum" = ...))]`
// so an editor completes and checks the spellings.  Only keys with one
// spelling per value get one: the cursor parser below accepts `"Block"` and
// `"block"` alike, and an `enum` listing one of the pair would mark a working
// config as an error.

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawConfig {
    /// Terminal palette: the sixteen ANSI colors plus the primary, cursor and
    /// selection pairs.
    colors: RawColors,
    /// alacritree's own presentation: sidebar colors, icons, tooltips, shell
    /// profiles, and everything else the terminal grid does not own.  Belongs
    /// in `alacritree.toml` — upstream alacritty warns about it.
    ui: RawUi,
    /// Where alacritree creates git worktrees.  `alacritree.toml` only.
    workspace: RawWorkspace,
    /// The terminal grid's font: the four faces, size, cell offsets, and
    /// alacritree's fallback chain.
    font: RawFont,
    /// Cursor shape, blinking, and how it renders when unfocused.
    cursor: RawCursor,
    /// Scrollback depth and mouse-wheel step.
    scrolling: RawScrolling,
    /// Window padding and background opacity.
    window: RawWindow,
    /// Environment variables added to every process alacritree spawns,
    /// including the shell.  Entries here may override variables alacritree
    /// sets itself.
    #[serde(default)]
    env: HashMap<String, String>,
    /// The program each session runs.
    terminal: RawTerminal,
    /// What counts as a word when double-clicking, and whether a selection
    /// reaches the clipboard on its own.
    selection: RawSelection,
    /// Key bindings.  Arrays concatenate across the two files, so bindings
    /// written in `alacritree.toml` add to the shared ones rather than
    /// replacing them.
    keyboard: RawKeyboard,
    /// Options that fit no other table.
    general: RawGeneral,
    /// Diagnostics written to disk.
    debug: RawDebug,
    /// How alacritree talks to WSL distros.  `alacritree.toml` only.
    wsl: RawWsl,
}

/// Subset of alacritty's `[general]` section that alacritree honors.  It
/// lives in the shared `alacritty.toml`, so disabling alacritty's socket
/// disables ours too — the two sockets are separate files, but the intent
/// ("no IPC") is the same.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawGeneral {
    /// Offer the local socket that `alacritree <command>` and the MCP bridge
    /// connect to.  Default `true`.
    ipc_socket: Option<bool>,
    /// Directory sessions on the home tab start in; worktree tabs always start
    /// in their checkout.  A leading `~` expands to the home directory.  Unset
    /// inherits the launching process's directory.
    working_directory: Option<String>,
    /// Where alacritree keeps what it remembers between runs: `state.toml`
    /// (project roots, expanded rows, sidebar visibility, per-worktree base
    /// branches) and the per-workspace scratchpad notes.  alacritree-only, so
    /// it belongs in `alacritree.toml`.  A leading `~` expands to the home
    /// directory; a relative path is ignored.
    ///
    /// Unset keeps the per-user config base, where these files have always
    /// lived: `%APPDATA%\alacritree` on Windows, `$XDG_CONFIG_HOME/alacritree`
    /// or `~/.config/alacritree` elsewhere.
    ///
    /// Setting this moves nothing.  The old state and notes stay where they
    /// are and the new directory starts empty, so move the files across
    /// yourself if you want them.  Every alacritree on the machine needs the
    /// same value: the CLI resolves this key the way the window does, so a
    /// command run against a different config reads a state file the window is
    /// not writing.
    state_dir: Option<String>,
}

/// alacritty's `[debug]` section, plus one alacritree-only key.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawDebug {
    /// Write an artifact when the process panics.  alacritree-only, so it
    /// belongs in `alacritree.toml`.  Default `true`: a crash that leaves no
    /// record is the failure this exists to prevent.
    crash_log: Option<bool>,
    /// Keep the log file after quitting.  Upstream's name and upstream's
    /// default (`false`).
    persistent_logging: Option<bool>,
    /// Log what the GPU grid's paint callback costs: the wall time of
    /// issuing a frame, and the GPU's own time for the upload and each of
    /// the three draws.  alacritree-only, so it belongs in
    /// `alacritree.toml`.  Default `false`; timer queries are cheap but not
    /// free, and the line is only meaningful to someone reading it.  Needs
    /// `[ui] gpu_grid` and a GL 3.3 context.  Keeps this session's log file
    /// for as long as it is on, since the report has nowhere else to go.
    gpu_timing: Option<bool>,
    /// Measure whole frames and report the period, CPU time, grid share and
    /// keystroke echo every few seconds.  alacritree-only, so it belongs in
    /// `alacritree.toml`.  Default `false`.
    ///
    /// `ALACRITREE_FRAME_LOG` wins over this key both ways: `1` turns
    /// measurements on, `0` and the empty string turn them off.  The variable
    /// is the only switch available before the config is read.
    ///
    /// Keeps this session's log file for as long as it is on.  The report goes
    /// to the log stream, and a GUI-subsystem binary has no console.
    frame_log: Option<bool>,
    /// Where crash artifacts and session logs are written.  alacritree-only,
    /// so it belongs in `alacritree.toml`.  A leading `~` expands to the home
    /// directory; a relative path is ignored.
    ///
    /// Unset writes to the machine-local state directory: `%LOCALAPPDATA%\
    /// alacritree` on Windows, `$XDG_STATE_HOME/alacritree` or
    /// `~/.local/state/alacritree` elsewhere.  Logs stay out of the config
    /// directory, which on Windows roams between machines.
    ///
    /// Setting this moves no log already written, and a panic during config
    /// parsing still lands in the default directory: the crash hook is armed
    /// before this key can be read.
    log_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawKeyboard {
    /// Key bindings, as `[[keyboard.bindings]]` entries.  Vi- and search-mode
    /// bindings are accepted and ignored: alacritree tracks neither mode.
    bindings: Vec<bindings::RawBinding>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawFont {
    /// Font size in points.  Default `11.25`.
    size: Option<f32>,
    /// The face ordinary text is drawn with.
    normal: RawFontFace,
    /// The bold face.  An unset family falls back to `normal`'s.
    bold: RawFontFace,
    /// The italic face.  An unset family falls back to `normal`'s.
    italic: RawFontFace,
    /// The bold-italic face.  An unset family falls back to `normal`'s.
    bold_italic: RawFontFace,
    /// Extra space around each cell in pixels: `y` is line spacing, `x` is
    /// letter spacing.  Default `{ x = 0, y = 0 }`.
    offset: RawFontDelta,
    /// Where the glyph sits inside its cell, in pixels.  Increasing `x` moves
    /// it right, increasing `y` moves it up.  Built-in glyphs ignore this,
    /// matching alacritty.
    glyph_offset: RawFontDelta,
    /// Draw box-drawing (U+2500–U+259F), legacy computing (U+1FB00–U+1FB3B)
    /// and Powerline (U+E0B0–U+E0BF) characters with the built-in renderer
    /// instead of the font.  Default `true`.
    builtin_box_drawing: Option<bool>,
    /// Ordered list of fallback font families or font file paths, tried in
    /// order after the four primary faces and before the automatic system
    /// chain.  Recommended home is `alacritree.toml`: upstream alacritty
    /// warns about unknown keys, so putting it in the shared `alacritty.toml`
    /// would make the real alacritty noisy.
    fallback: Option<Vec<String>>,
    /// Draw emoji from their font's colour tables.  Turning this off falls
    /// through to the first fallback face with ordinary outlines, so emoji
    /// render monochrome.  Also alacritree-only, so it belongs in
    /// `alacritree.toml` alongside `fallback`.  Default `true`.
    color_glyphs: Option<bool>,
    /// Budget in megabytes for the rasterized colour-glyph cache.  The cache
    /// is already bounded by how many codepoints the colour fonts cover, but
    /// that ceiling moves with cell size and with the fallback list.
    color_glyph_cache_mb: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawFontFace {
    /// Family name as the system font database spells it, e.g.
    /// `"JetBrainsMono Nerd Font"`.
    family: Option<String>,
    /// Style within the family, e.g. `"Regular"`, `"Bold"`, `"Italic"`.
    style: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawFontDelta {
    /// Horizontal offset in pixels.
    x: Option<i8>,
    /// Vertical offset in pixels.
    y: Option<i8>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawCursor {
    /// Cursor shape and blinking.  Older alacritty configs write just
    /// `style = "Block"` rather than `style.shape = "Block"`; both are
    /// accepted.
    style: Option<RawCursorStyle>,
    /// Render the cursor as a hollow box when the window is not focused.
    /// Default `true`.
    unfocused_hollow: Option<bool>,
    /// Blink interval in milliseconds.  Default `750`.
    blink_interval: Option<u64>,
    /// Seconds after which the cursor stops blinking; `0` never stops.
    /// Default `5`.
    blink_timeout: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum RawCursorStyle {
    /// Just the shape: `"Block"`, `"Underline"`, `"Beam"`, `"HollowBlock"` or
    /// `"Hidden"`.  Lowercase spellings are accepted too.
    Shape(String),
    /// Shape and blinking together.
    Detailed {
        /// `"Block"`, `"Underline"`, `"Beam"`, `"HollowBlock"` or `"Hidden"`.
        /// Lowercase spellings are accepted too.
        shape: Option<String>,
        /// `"Never"`, `"Off"`, `"On"` or `"Always"`.  alacritree has no vi
        /// mode, so `On` and `Always` both blink and the other two do not.
        blinking: Option<String>,
    },
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawScrolling {
    /// Maximum number of lines kept in the scrollback buffer.
    /// Default `10000`.
    history: Option<u32>,
    /// Lines scrolled per mouse-wheel increment.  Default `3`.
    multiplier: Option<u8>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawWindow {
    /// Blank space around the terminal grid, in pixels, added at both
    /// opposing sides.
    padding: Option<RawPadding>,
    /// Background opacity from `0.0` (transparent) to `1.0` (opaque).
    /// Changing it requires a restart: transparency is a window flag set
    /// before the window exists.  Default `1.0`.
    opacity: Option<f32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawPadding {
    /// Horizontal padding in pixels.
    x: Option<f32>,
    /// Vertical padding in pixels.
    y: Option<f32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawTerminal {
    /// The program each session runs, as either a bare path or a table with
    /// arguments.  Unset uses `$SHELL` (the login shell as a fallback) on
    /// Unix and PowerShell on Windows.
    shell: Option<RawShell>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum RawShell {
    /// Just the program, e.g. `"/bin/zsh"`.
    Program(String),
    /// Program and its arguments.
    Detailed {
        /// Path to the program, e.g. `"/bin/zsh"`.
        program: String,
        /// Arguments passed to the program.
        #[serde(default)]
        args: Vec<String>,
    },
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawSelection {
    /// Characters that separate "semantic words" for double-click selection.
    semantic_escape_chars: Option<String>,
    /// Copy selected text to the system clipboard as soon as it is selected.
    /// Default `false`.
    save_to_clipboard: Option<bool>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawColors {
    /// Default foreground and background, plus the bright and dim foreground
    /// variants.
    #[serde(default)]
    primary: RawPrimary,
    /// Colors the cursor is drawn with.
    #[serde(default)]
    cursor: RawInverted,
    /// Colors a selection is drawn with.
    #[serde(default)]
    selection: RawInverted,
    /// The eight normal ANSI colors (0–7).
    #[serde(default)]
    normal: RawSet,
    /// The eight bright ANSI colors (8–15).
    #[serde(default)]
    bright: RawSet,
    /// The eight dim ANSI colors.  Unset derives them from `normal`.
    #[serde(default)]
    dim: Option<RawSet>,
    /// Overrides within the 16–255 range of the 256-color palette.  Unlisted
    /// indices keep their standard values.
    #[serde(default)]
    indexed_colors: Vec<RawIndexed>,
    /// Draw bold text with the bright color variants.  Default `false`.
    #[serde(default)]
    draw_bold_text_with_bright_colors: bool,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawPrimary {
    /// Default text color.
    foreground: Option<RgbStr>,
    /// Default background color.
    background: Option<RgbStr>,
    /// Foreground for bold text, used only when
    /// `draw_bold_text_with_bright_colors` is `true`.  Unset uses
    /// `foreground`.
    bright_foreground: Option<RgbStr>,
    /// Foreground for dimmed text.  Unset derives it from `foreground`.
    dim_foreground: Option<RgbStr>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawInverted {
    /// Foreground glyph color.  Alacritty calls this `text`; we accept both.
    text: Option<RgbStr>,
    /// Background block color.  Alacritty calls this `cursor`; we accept both.
    cursor: Option<RgbStr>,
    /// Alias for `text`.
    foreground: Option<RgbStr>,
    /// Alias for `cursor`.
    background: Option<RgbStr>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawSet {
    /// ANSI color 0.
    black: Option<RgbStr>,
    /// ANSI color 1.
    red: Option<RgbStr>,
    /// ANSI color 2.
    green: Option<RgbStr>,
    /// ANSI color 3.
    yellow: Option<RgbStr>,
    /// ANSI color 4.
    blue: Option<RgbStr>,
    /// ANSI color 5.
    magenta: Option<RgbStr>,
    /// ANSI color 6.
    cyan: Option<RgbStr>,
    /// ANSI color 7.
    white: Option<RgbStr>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RawIndexed {
    /// Palette slot to override, 16–255.
    index: u8,
    /// The color that slot takes.
    color: RgbStr,
}

/// Top-level `[wsl]`: platform-integration options.  Lives outside `[ui]`
/// because nothing here is presentation — it governs how the app talks to
/// distros.
#[derive(Debug, Default, Deserialize, JsonSchema)]
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

/// `[ui.icons]`: sidebar glyph overrides.  A bare string sets the glyph
/// alone; a table also styles color/weight/slant/size.  Any glyph works, so
/// Nerd Font users can substitute their own icons.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawIcons {
    /// The panel search box.
    search: Option<RawIconStyle>,
    /// A project's main checkout.
    worktree_main: Option<RawIconStyle>,
    /// A linked worktree.
    worktree: Option<RawIconStyle>,
    /// A terminal session row.
    session: Option<RawIconStyle>,
    /// A pane owned by a terminal workspace manager such as herdr.
    herdr: Option<RawIconStyle>,
    /// The home tab, whose sessions inherit the launch directory.
    home: Option<RawIconStyle>,
    /// An expanded project.
    project_expanded: Option<RawIconStyle>,
    /// A collapsed project.
    project_collapsed: Option<RawIconStyle>,
    /// A branch with an open pull request.
    pr_open: Option<RawIconStyle>,
    /// A branch whose pull request is a draft.
    pr_draft: Option<RawIconStyle>,
    /// A branch whose pull request was merged.
    pr_merged: Option<RawIconStyle>,
    /// A branch whose pull request was closed unmerged.
    pr_closed: Option<RawIconStyle>,
    /// A branch level with its upstream.
    upstream_level: Option<RawIconStyle>,
    /// A branch that has both moved ahead of and fallen behind its upstream.
    upstream_diverged: Option<RawIconStyle>,
    /// A branch whose upstream no longer exists locally.
    upstream_gone: Option<RawIconStyle>,
    /// A branch that tracks nothing.
    upstream_untracked: Option<RawIconStyle>,
    /// The "add project" button.
    add_project: Option<RawIconStyle>,
    /// The "new worktree" button.
    new_worktree: Option<RawIconStyle>,
    /// The "new session" button.
    new_session: Option<RawIconStyle>,
    /// The "remove project" button.
    remove_project: Option<RawIconStyle>,
    /// The "delete worktree" button.
    delete_worktree: Option<RawIconStyle>,
    /// The "close session" button.
    close_session: Option<RawIconStyle>,
    /// The "refresh" button.
    refresh: Option<RawIconStyle>,
    /// The drag handle a row is reordered by.
    reorder: Option<RawIconStyle>,
}

/// An absent key falls back to the key's default style (glyph included); a
/// present one always wins, even if it styles without setting `glyph`.
fn style_or(raw: Option<RawIconStyle>, default: &IconStyle) -> IconStyle {
    raw.map(IconStyle::from).unwrap_or_else(|| default.clone())
}

fn build_icons(raw: RawIcons) -> Icons {
    let d = Icons::default();
    Icons {
        search: style_or(raw.search, &d.search),
        worktree_main: style_or(raw.worktree_main, &d.worktree_main),
        worktree: style_or(raw.worktree, &d.worktree),
        session: style_or(raw.session, &d.session),
        herdr: style_or(raw.herdr, &d.herdr),
        home: style_or(raw.home, &d.home),
        project_expanded: style_or(raw.project_expanded, &d.project_expanded),
        project_collapsed: style_or(raw.project_collapsed, &d.project_collapsed),
        pr_open: style_or(raw.pr_open, &d.pr_open),
        pr_draft: style_or(raw.pr_draft, &d.pr_draft),
        pr_merged: style_or(raw.pr_merged, &d.pr_merged),
        pr_closed: style_or(raw.pr_closed, &d.pr_closed),
        upstream_level: style_or(raw.upstream_level, &d.upstream_level),
        upstream_diverged: style_or(raw.upstream_diverged, &d.upstream_diverged),
        upstream_gone: style_or(raw.upstream_gone, &d.upstream_gone),
        upstream_untracked: style_or(raw.upstream_untracked, &d.upstream_untracked),
        add_project: style_or(raw.add_project, &d.add_project),
        new_worktree: style_or(raw.new_worktree, &d.new_worktree),
        new_session: style_or(raw.new_session, &d.new_session),
        remove_project: style_or(raw.remove_project, &d.remove_project),
        delete_worktree: style_or(raw.delete_worktree, &d.delete_worktree),
        close_session: style_or(raw.close_session, &d.close_session),
        refresh: style_or(raw.refresh, &d.refresh),
        reorder: style_or(raw.reorder, &d.reorder),
    }
}

// The bare form is listed first so a plain string never attempts the table
// arm.
/// A styled icon override: either a bare glyph string (`worktree = "◆"`) or a
/// table (`worktree = { glyph = "◆", color = "#ff5555", bold = true }`).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum RawIconStyle {
    /// The glyph alone.
    Glyph(String),
    /// The glyph with styling.
    Table {
        /// The character to draw.  Unset keeps the built-in glyph and applies
        /// only the styling.
        glyph: Option<String>,
        /// Glyph color.  Unset inherits the row's foreground.
        color: Option<RgbStr>,
        /// Draw the glyph bold.
        #[serde(default)]
        bold: bool,
        /// Draw the glyph italic.
        #[serde(default)]
        italic: bool,
        /// Point size, clamped to a minimum of `1.0`.  Unset uses the sidebar
        /// font size.
        size: Option<f32>,
    },
}

impl From<RawIconStyle> for IconStyle {
    fn from(raw: RawIconStyle) -> Self {
        match raw {
            RawIconStyle::Glyph(glyph) => IconStyle { glyph: Some(glyph), ..Default::default() },
            RawIconStyle::Table { glyph, color, bold, italic, size } => IconStyle {
                glyph,
                color: color.map(|c| rgb_to_color32(c.0)),
                bold,
                italic,
                size: size.map(|s| s.max(1.0)),
            },
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawUiWsl {
    /// Deprecated location: `[wsl] automount_root` supersedes this and wins
    /// when both are set; kept so existing configs keep working.
    automount_root: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawSessionDisplay {
    /// Show a workspace's sidebar session row even with a single session.
    sidebar_always: Option<bool>,
    /// Draw a tab-strip segment even with a single session.
    tabs_always: Option<bool>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawSessionReorder {
    /// Let a session row be dragged with the mouse to reorder it.
    drag: Option<bool>,
    /// How far a reorder may carry a session: "workspace" (default) |
    /// "project" | "anywhere".
    #[schemars(extend("enum" = ["workspace", "project", "anywhere"]))]
    scope: Option<String>,
}

/// Corrections applied to what the font reports for its underline and
/// strikeout.  Each value is `"2px"` (physical pixels, added), `"2pt"` or a
/// bare `"2"` (points, added), or `"150%"` (a multiplier).  Positive moves a
/// line down, matching kitty and ghostty.  A percentage takes no sign.
/// Default `"0"`, which draws what the font asked for.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawDecorations {
    /// Shift or scale of how far the underline sits from the top of the
    /// cell, for the straight, dotted and dashed styles.  The double and
    /// curly styles are placed from the font's descent instead, so this
    /// knob does not reach them.
    #[schemars(extend("pattern" = r"^(-?[0-9]*\.?[0-9]+(px|pt)?|[0-9]*\.?[0-9]+%)$"))]
    underline_position: Option<String>,
    /// Shift or scale of the underline's stroke weight.  Every style draws
    /// with this value, including double and curly.
    #[schemars(extend("pattern" = r"^(-?[0-9]*\.?[0-9]+(px|pt)?|[0-9]*\.?[0-9]+%)$"))]
    underline_thickness: Option<String>,
    /// Shift or scale of how far the strikeout sits from the top of the cell.
    #[schemars(extend("pattern" = r"^(-?[0-9]*\.?[0-9]+(px|pt)?|[0-9]*\.?[0-9]+%)$"))]
    strikeout_position: Option<String>,
    /// Shift or scale of the strikeout bar's weight.
    #[schemars(extend("pattern" = r"^(-?[0-9]*\.?[0-9]+(px|pt)?|[0-9]*\.?[0-9]+%)$"))]
    strikeout_thickness: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawUiFont {
    /// Family for sidebars, tabs and dialogs.  Unset uses the terminal font.
    family: Option<String>,
    /// Point size for the sidebar font.
    size: Option<f32>,
    /// Family used where the sidebar draws bold.  Unset uses `family`.
    bold_family: Option<String>,
    /// Family used where the sidebar draws italic.  Unset uses `family`.
    italic_family: Option<String>,
    /// Family used where the sidebar draws bold italic.  Unset uses `family`.
    bold_italic_family: Option<String>,
    /// Draw the sidebar's own symbols from the bundled subset rather than from
    /// the configured family, so a font missing them still renders.
    builtin_symbols: Option<bool>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawFocusOutline {
    /// Outline the sidebar when it holds keyboard focus.
    sidebar: Option<bool>,
    /// Outline the terminal when it holds keyboard focus.
    terminal: Option<bool>,
    /// Outline color.  Unset uses the sidebar accent.
    color: Option<RgbStr>,
    /// Outline thickness in pixels.
    thickness: Option<f32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawUiDrop {
    /// Accept dropped files at all.  `false` turns every target off.
    enabled: Option<bool>,
    /// Write a dropped file's path into the terminal.
    terminal: Option<bool>,
    /// Let a file dropped on the projects sidebar add its repository.
    sidebar: Option<bool>,
    /// Write a dropped file's path into the workspace scratchpad.
    scratchpad: Option<bool>,
    /// How a path is quoted for the shell that receives it.  The five concrete
    /// modes are wezterm's `quote_dropped_files` values.
    #[schemars(extend("enum" = [
        "auto",
        "none",
        "spaces_only",
        "posix",
        "windows",
        "windows_always_quoted"
    ]))]
    quote: Option<String>,
    /// Rewrite a Windows path to its distro spelling when the session runs
    /// inside WSL.
    wsl_translate: Option<bool>,
    /// Highlight the target a drag is over.
    highlight: Option<bool>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawUiPaste {
    /// Paste files held on the clipboard as their paths.
    files: Option<bool>,
    /// Paste an image held on the clipboard by writing it to a file and
    /// pasting that path.
    image: Option<bool>,
    /// Where pasted images are written.  Unset uses a cache directory.
    image_dir: Option<String>,
    /// How many pasted images to keep before the oldest are removed.
    image_keep: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawHerdr {
    /// Discover herdr servers and list their agents in the sidebar.  Inert
    /// when no herdr binary or server is present.
    enabled: Option<bool>,
    /// How often a reachable herdr server is re-polled for agent state.
    poll_interval_ms: Option<u64>,
    /// List agents whose working directory matches no worktree, under Home.
    show_unmatched: Option<bool>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawUi {
    /// Sidebar background.  Unset derives it from the terminal palette.
    sidebar_background: Option<RgbStr>,
    /// Sidebar text color.  Unset derives it from the terminal palette.
    sidebar_foreground: Option<RgbStr>,
    /// Color of the line between a sidebar and the terminal.
    sidebar_border: Option<RgbStr>,
    /// Accent for selected rows and focus outlines.  Unset uses the palette's
    /// `normal.blue`.
    sidebar_accent: Option<RgbStr>,
    /// Badge color for a session asking to be looked at.  Unset uses the
    /// palette's `normal.yellow`.
    sidebar_attention: Option<RgbStr>,
    /// Post a desktop notification when a hidden session rings the bell;
    /// clicking it focuses that session.
    notifications: Option<bool>,
    /// Grace window in milliseconds before an attention trigger pings; a
    /// session that resumes work inside it swallows the ping.  Default 0.
    attention_grace_ms: Option<u64>,
    /// When the sidebar × on a session row asks before killing the PTY:
    /// "never" (default) | "busy" | "always".
    #[schemars(extend("enum" = ["never", "busy", "always"]))]
    confirm_session_close: Option<String>,
    /// Whether the sidebar × on a harness-managed row asks before detaching.
    /// Separate from `confirm_session_close` because a detach leaves the
    /// pane running and its row listed again. Default true.
    confirm_session_detach: Option<bool>,
    /// What happens when the on-screen workspace stops having sessions,
    /// whether a close or a worktree deletion took the last one:
    /// "respawn" (default) | "navigate" | "ring_global" | "ring_project".
    #[schemars(extend("enum" = ["respawn", "navigate", "ring_global", "ring_project"]))]
    last_session_close: Option<String>,
    /// How far the projects sidebar goes when the cursor's row stops being
    /// rendered: "preserve" (default) | "follow".
    #[schemars(extend("enum" = ["preserve", "follow"]))]
    sidebar_focus: Option<String>,
    /// Whether the projects sidebar scrolls to the session on screen whenever
    /// it changes — a cycling key, a click, the palette, an IPC request.
    /// The sidebar cursor is left where it was: `false` (default).
    sidebar_follow_active: Option<bool>,
    /// Where a row the sidebar scrolled to is parked:
    /// "minimal" (default) | "center".  Under "center" every cursor step
    /// re-centres the list, and clicking a row near the panel edge scrolls it
    /// out from under the pointer.
    #[schemars(extend("enum" = ["minimal", "center"]))]
    sidebar_scroll_align: Option<String>,
    /// Whether a fuzzy query is confined by the panel's active toggle filters:
    /// "filtered" (default) | "all".
    #[schemars(extend("enum" = ["filtered", "all"]))]
    search_scope: Option<String>,
    /// When a sidebar row spells its full name out on hover:
    /// "elided" (default) | "always" | "off".
    #[schemars(extend("enum" = ["elided", "always", "off"]))]
    sidebar_tooltips: Option<String>,
    /// Whether a sidebar icon explains itself on hover: `true` (default).
    icon_tooltips: Option<bool>,
    /// Whether per-session rows and tabs appear before a workspace has two
    /// sessions.
    session_display: RawSessionDisplay,
    /// Whether session rows can be dragged, and how far a reorder may carry
    /// a session.
    session_reorder: RawSessionReorder,
    /// Explicit `delta` program for the diff pane.  Set, it is used verbatim
    /// in git's `core.pager` and skips WSL delta autodiscovery; unset, native
    /// diffs run bare `delta` from PATH.
    delta_path: Option<String>,
    /// Sidebar glyph overrides.
    icons: RawIcons,
    /// Sidebar scrollbar style: "floating" (default) | "solid".
    #[schemars(extend("enum" = ["floating", "solid"]))]
    scrollbar: Option<String>,
    /// Draw the terminal grid through an OpenGL paint callback instead of
    /// handing epaint a mesh.  Default `false`: it needs a GL 3 context and
    /// bypasses the renderer every other panel goes through, so an
    /// unmodified config keeps the path that has always drawn the grid.  A
    /// context too old for instanced arrays logs once and paints the mesh
    /// from the next frame on.
    gpu_grid: Option<bool>,
    /// Corrections to the underline and strikeout the font placed
    /// ([`RawDecorations`]).
    decorations: RawDecorations,
    /// Poll `gh` for each branch's open pull request, which drives the PR row
    /// icons, the PR-state filters, and `$pr` in row templates.
    pr_status: Option<bool>,
    /// Paint a badge on each worktree row for its branch's upstream state.
    /// Local refs only: nothing fetches, so a branch deleted on the remote
    /// reads as tracked until something prunes locally.
    upstream_status: Option<bool>,
    /// Re-check on a 1.5 s tick whether each listed worktree's checkout is
    /// still on disk, so a `git worktree remove` typed into one of our own
    /// sessions greys the row without waiting for a manual refresh.  Default
    /// `true`; the probe is one `stat` per listed row, which an exotic
    /// filesystem could make expensive.
    worktree_liveness: Option<bool>,
    /// Max `gh` lookups in flight at once.  Unset lets the pool decide, which
    /// is one below its own background ceiling so a lookup can never take
    /// the last slot local work needs.  A value lowers that; nothing raises
    /// it, because the pool's ceiling binds underneath either way.
    pr_status_concurrency: Option<usize>,
    /// The font sidebars, tabs and dialogs are drawn with.
    font: RawUiFont,
    /// Template for a worktree row's label, e.g. `"$branch $pr"`.
    worktree_name: Option<String>,
    /// Template for a project row's label.
    project_name: Option<String>,
    /// Deprecated WSL options, superseded by the top-level `[wsl]` table.
    wsl: RawUiWsl,
    /// Named shell launch profiles, offered when starting a session.
    profiles: Vec<RawProfile>,
    /// Name of the profile new sessions use.  Must match a `[[ui.profiles]]`
    /// entry, or it is ignored with a warning.
    default_profile: Option<String>,
    /// Outline drawn around whichever pane holds keyboard focus.
    focus_outline: RawFocusOutline,
    /// Clicking a sidebar moves keyboard focus to it.  Default false.
    sidebar_click_focus: Option<bool>,
    /// Put the session on screen one scheduling class above normal — its
    /// shell and every process that shell starts — so a busy machine cannot
    /// starve what the user is typing into.  Follows focus.  Windows only.
    /// Default false.
    focus_priority_boost: Option<bool>,
    /// Open a session's PTY on a worker rather than in the frame that asked
    /// for it, so spawning does not stutter.  Default false.
    async_session_spawn: Option<bool>,
    /// End everything a session started when that session closes, at any
    /// depth, except processes that ask to break away.  Windows only.
    /// Default false.
    reap_descendants_on_close: Option<bool>,
    /// Wait for the display's refresh before showing a finished frame.
    /// Default true.
    vsync: Option<bool>,
    /// How paths are abbreviated where the UI writes them.
    path_style: RawPathStyle,
    /// What a file dragged onto the window does.  Default: every target on.
    drop: RawUiDrop,
    /// What the clipboard's non-text contents paste as.
    paste: RawUiPaste,
    /// Whether agents running under a herdr server appear in the sidebar.
    herdr: RawHerdr,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawPathStyle {
    /// "full" (default) | "fish" | "zed", per site.
    ///
    /// The diff pane's title.
    #[schemars(extend("enum" = ["full", "fish", "zed"]))]
    diff_title: Option<String>,
    /// Paths in the git panel's file rows.
    #[schemars(extend("enum" = ["full", "fish", "zed"]))]
    git_rows: Option<String>,
    /// The path in the git panel's header.
    #[schemars(extend("enum" = ["full", "fish", "zed"]))]
    git_header: Option<String>,
    /// How the last path segment is emphasized.
    filename: RawTextEmphasis,
    /// How the leading path segments are emphasized.
    parent: RawTextEmphasis,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawTextEmphasis {
    /// Text color.  Unset inherits the row's foreground.
    color: Option<RgbStr>,
    /// Draw bold.
    bold: Option<bool>,
    /// Draw italic.
    italic: Option<bool>,
}

/// One `[[ui.profiles]]` entry.  Fields are optional so a malformed entry
/// degrades to a warning instead of failing the whole config parse.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawProfile {
    /// Name shown in the session picker and matched by `default_profile`.
    name: Option<String>,
    /// Program the profile launches.
    program: Option<String>,
    /// Arguments passed to `program`.
    args: Vec<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct RawWorkspace {
    /// Where new worktrees are created.  `$project` expands to the
    /// repository's directory name.
    worktree_dir: Option<String>,
    /// Per-project overrides of `worktree_dir`.
    overrides: Vec<RawWorktreeOverride>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RawWorktreeOverride {
    /// Path to the project this override applies to.
    project: String,
    /// Where that project's worktrees are created.
    worktree_dir: String,
}

/// Wrapper that parses `"0xrrggbb"`, `"#rrggbb"`, or `"rrggbb"` into an `Rgb`.
#[derive(Debug, Clone, Copy)]
struct RgbStr(Rgb);

/// Hand-written because `RgbStr` deserializes from a string it parses itself,
/// so nothing about the accepted spellings is visible to a derive.
impl JsonSchema for RgbStr {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Color".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(0[xX]|#)?[0-9a-fA-F]{6}$",
            "description": "An RGB color, written as \"#rrggbb\", \"0xrrggbb\" or \"rrggbb\".",
            "examples": ["#1c1c1c", "0x6a9fb5"],
        })
    }
}

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
            sidebar_attention: self.ui.sidebar_attention.map(|v| rgb_to_color32(v.0)),
            notifications: self.ui.notifications.unwrap_or(true),
            attention_grace: Duration::from_millis(self.ui.attention_grace_ms.unwrap_or(0)),
            confirm_session_close: parse_confirm_session_close(
                self.ui.confirm_session_close.as_deref(),
            ),
            confirm_session_detach: self.ui.confirm_session_detach.unwrap_or(true),
            last_session_close: parse_last_session_close(self.ui.last_session_close.as_deref()),
            sidebar_focus: parse_sidebar_focus(self.ui.sidebar_focus.as_deref()),
            sidebar_follow_active: self.ui.sidebar_follow_active.unwrap_or(false),
            sidebar_scroll_align: parse_scroll_align(self.ui.sidebar_scroll_align.as_deref()),
            search_scope: parse_search_scope(self.ui.search_scope.as_deref()),
            sidebar_tooltips: parse_sidebar_tooltips(self.ui.sidebar_tooltips.as_deref()),
            icon_tooltips: self.ui.icon_tooltips.unwrap_or(true),
            session_display: SessionDisplay {
                sidebar_always: self.ui.session_display.sidebar_always.unwrap_or(false),
                tabs_always: self.ui.session_display.tabs_always.unwrap_or(false),
            },
            session_reorder: SessionReorder {
                drag: self.ui.session_reorder.drag.unwrap_or(false),
                scope: parse_reorder_scope(self.ui.session_reorder.scope.as_deref()),
            },
            gpu_grid: self.ui.gpu_grid.unwrap_or(false),
            decorations: Decorations {
                underline_position: parse_adjust(
                    "underline_position",
                    self.ui.decorations.underline_position.as_deref(),
                ),
                underline_thickness: parse_adjust(
                    "underline_thickness",
                    self.ui.decorations.underline_thickness.as_deref(),
                ),
                strikeout_position: parse_adjust(
                    "strikeout_position",
                    self.ui.decorations.strikeout_position.as_deref(),
                ),
                strikeout_thickness: parse_adjust(
                    "strikeout_thickness",
                    self.ui.decorations.strikeout_thickness.as_deref(),
                ),
            },
            pr_status: self.ui.pr_status.unwrap_or(false),
            upstream_status: self.ui.upstream_status.unwrap_or(false),
            worktree_liveness: self.ui.worktree_liveness.unwrap_or(true),
            pr_status_concurrency: self.ui.pr_status_concurrency,
            icons: build_icons(self.ui.icons),
            focus_outline: FocusOutline {
                sidebar: self.ui.focus_outline.sidebar.unwrap_or(false),
                terminal: self.ui.focus_outline.terminal.unwrap_or(false),
                color: self.ui.focus_outline.color.map(|v| rgb_to_color32(v.0)),
                thickness: self.ui.focus_outline.thickness.map_or(1.0, |t| t.max(0.5)),
            },
            scrollbar: parse_scrollbar(self.ui.scrollbar.as_deref()),
            sidebar_click_focus: self.ui.sidebar_click_focus.unwrap_or(false),
            focus_priority_boost: self.ui.focus_priority_boost.unwrap_or(false),
            async_session_spawn: self.ui.async_session_spawn.unwrap_or(false),
            reap_descendants_on_close: self.ui.reap_descendants_on_close.unwrap_or(false),
            vsync: self.ui.vsync.unwrap_or(true),
            worktree_name: self.ui.worktree_name.clone().filter(|t| !t.trim().is_empty()),
            project_name: self.ui.project_name.clone().filter(|t| !t.trim().is_empty()),
            path_style: PathStyleConfig {
                diff_title: parse_path_style(self.ui.path_style.diff_title.as_deref()),
                git_rows: parse_path_style(self.ui.path_style.git_rows.as_deref()),
                git_header: parse_path_style(self.ui.path_style.git_header.as_deref()),
                filename: text_emphasis(&self.ui.path_style.filename),
                parent: text_emphasis(&self.ui.path_style.parent),
            },
            drop: DropConfig {
                enabled: self.ui.drop.enabled.unwrap_or(true),
                terminal: self.ui.drop.terminal.unwrap_or(true),
                sidebar: self.ui.drop.sidebar.unwrap_or(true),
                scratchpad: self.ui.drop.scratchpad.unwrap_or(true),
                spelling: PathSpelling {
                    quote: parse_quoting(self.ui.drop.quote.as_deref()),
                    wsl_translate: self.ui.drop.wsl_translate.unwrap_or(true),
                },
                highlight: self.ui.drop.highlight.unwrap_or(true),
            },
            paste: PasteConfig {
                files: self.ui.paste.files.unwrap_or(true),
                image: self.ui.paste.image.unwrap_or(true),
                image_dir: self
                    .ui
                    .paste
                    .image_dir
                    .as_deref()
                    .and_then(|raw| parse_config_path(raw, "ui.paste.image_dir")),
                image_keep: self.ui.paste.image_keep.unwrap_or(20).max(1),
            },
            herdr: HerdrConfig {
                enabled: self.ui.herdr.enabled.unwrap_or(true),
                poll_interval: Duration::from_millis(
                    self.ui.herdr.poll_interval_ms.unwrap_or(2000),
                ),
                show_unmatched: self.ui.herdr.show_unmatched.unwrap_or(true),
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
            bold_family: self.ui.font.bold_family.clone().filter(|f| !f.trim().is_empty()),
            italic_family: self.ui.font.italic_family.clone().filter(|f| !f.trim().is_empty()),
            bold_italic_family: self
                .ui
                .font
                .bold_italic_family
                .clone()
                .filter(|f| !f.trim().is_empty()),
            builtin_symbols: self.ui.font.builtin_symbols.unwrap_or(true),
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
            debug: DebugConfig {
                crash_log: self.debug.crash_log.unwrap_or(true),
                persistent_logging: self.debug.persistent_logging.unwrap_or(false),
                gpu_timing: self.debug.gpu_timing.unwrap_or(false),
                frame_log: self.debug.frame_log.unwrap_or(false),
                log_dir: self
                    .debug
                    .log_dir
                    .as_deref()
                    .and_then(|raw| parse_config_path(raw, "debug.log_dir")),
            },
            working_directory: self
                .general
                .working_directory
                .as_deref()
                .and_then(|raw| parse_config_path(raw, "general.working_directory")),
            state_dir: self
                .general
                .state_dir
                .as_deref()
                .and_then(|raw| parse_config_path(raw, "general.state_dir")),
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
    /// A config directory holding the given files, kept alive by the caller.
    fn config_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).expect("write a config file");
        }
        dir
    }

    #[test]
    fn a_named_directory_supplies_both_config_files() {
        let dir = config_dir(&[
            ("alacritty.toml", "[terminal.shell]\nprogram = \"nu\"\n"),
            ("alacritree.toml", "[ui]\nasync_session_spawn = true\n"),
        ]);

        let (config, files) = super::load(Some(dir.path()), &[]);

        assert!(config.ui.async_session_spawn, "alacritree.toml applies");
        assert_eq!(
            config.shell.as_ref().map(|s| s.program.as_str()),
            Some("nu"),
            "alacritty.toml applies, so the two still merge"
        );
        for file in &files {
            assert_eq!(
                file.path.as_deref().and_then(|p| p.parent()),
                Some(dir.path()),
                "{} came from the named directory",
                file.stem
            );
        }
    }

    /// The point of the override: a directory with no `alacritty.toml` runs
    /// without one. Falling back to the search path for the missing half would
    /// silently mix the machine's real config into a run meant to be isolated.
    #[test]
    fn a_file_absent_from_the_named_directory_is_not_looked_up_elsewhere() {
        let dir = config_dir(&[("alacritree.toml", "[ui]\ngpu_grid = true\n")]);

        let (_, files) = super::load(Some(dir.path()), &[]);

        let alacritty = files.iter().find(|f| f.stem == "alacritty").expect("both stems reported");
        assert_eq!(alacritty.path, None, "the installed alacritty.toml is not reached");
    }

    /// A fragment is a whole TOML document, so a dotted key nests on its own.
    #[test]
    fn an_override_beats_the_file_that_set_the_same_key() {
        let dir = config_dir(&[("alacritree.toml", "[ui]\ngpu_grid = true\n")]);
        let off = toml::from_str("ui.gpu_grid=false").expect("a valid fragment");

        let (config, _) = super::load(Some(dir.path()), &[off]);

        assert!(!config.ui.gpu_grid, "the file won over the override");
    }

    /// Automated runs vary one key against no config at all, so an override
    /// has to apply with nothing underneath it to merge into.
    #[test]
    fn an_override_applies_with_no_config_file_present() {
        let dir = config_dir(&[]);
        let on = toml::from_str("ui.async_session_spawn=true").expect("a valid fragment");

        let (config, _) = super::load(Some(dir.path()), &[on]);

        assert!(config.ui.async_session_spawn);
    }

    /// `-o` twice over one key is a command line the caller edited without
    /// deleting the old value; the one they typed last is the one they meant.
    #[test]
    fn the_last_override_of_a_key_wins() {
        let dir = config_dir(&[]);
        let first = toml::from_str("ui.gpu_grid=true").expect("a valid fragment");
        let second = toml::from_str("ui.gpu_grid=false").expect("a valid fragment");

        let (config, _) = super::load(Some(dir.path()), &[first, second]);

        assert!(!config.ui.gpu_grid);
    }

    /// The startup log diffs the resolved config, so an override reaches it
    /// with no separate reporting path of its own.
    #[test]
    fn an_override_shows_up_in_the_settings_dump() {
        let dir = config_dir(&[]);
        let on = toml::from_str("ui.async_session_spawn=true").expect("a valid fragment");

        let (config, _) = super::load(Some(dir.path()), &[on]);

        let dumped = config.changed_from_defaults().expect("the override is a change");
        assert!(dumped.contains("async_session_spawn"), "{dumped}");
    }

    #[test]
    fn an_empty_named_directory_yields_the_stock_config() {
        let dir = config_dir(&[]);

        let (config, _) = super::load(Some(dir.path()), &[]);

        assert_eq!(config.changed_from_defaults(), None);
    }

    /// A config that parses as TOML but does not fit the schema drops *every*
    /// setting in *both* files.  The fallback has to be the config a fresh
    /// install runs, or one mistyped value answers with a terminal that cannot
    /// paste, copy, or resize its font — and cannot reach the file to fix it.
    #[test]
    fn a_config_that_fails_the_schema_still_leaves_the_built_in_bindings() {
        let dir = config_dir(&[("alacritree.toml", "[ui]\nasync_session_spawn = \"yes\"\n")]);

        let (config, _) = super::load(Some(dir.path()), &[]);

        assert!(!config.ui.async_session_spawn, "the unusable setting is dropped");
        assert_eq!(
            config.bindings.len(),
            super::stock_config().bindings.len(),
            "a broken config keeps every built-in binding"
        );
    }

    use super::*;

    /// The `Config` `toml` resolves to, defaults filled in as `load` fills
    /// them.
    fn config_from(toml: &str) -> Config {
        let raw: RawConfig = toml::from_str(toml).expect("valid TOML");
        raw.into_config()
    }

    /// The changes `toml` makes to a stock config, as JSON.
    fn changed(toml: &str) -> serde_json::Value {
        let dump = config_from(toml).changed_from_defaults().expect("something changed");
        serde_json::from_str(&dump).expect("valid JSON")
    }

    /// An install with no config file writes nothing.  The baseline has to be
    /// the config that path produces, not `Config::default`, which carries no
    /// key bindings and would report all of the built-in ones as changes.
    #[test]
    fn a_stock_install_reports_no_changes_at_all() {
        assert_eq!(config_from("").changed_from_defaults(), None);
    }

    #[test]
    fn one_changed_key_brings_nothing_else_with_it() {
        let json = changed("[ui]\ngpu_grid = true\n");

        assert_eq!(json["ui"]["gpu_grid"], serde_json::json!(true));
        assert!(json.get("palette").is_none(), "an untouched section must not be dumped");
        assert!(
            json["ui"].get("async_session_spawn").is_none(),
            "an untouched sibling must not ride along with its section"
        );
    }

    /// The effective value is what lands in the dump, whatever spelling the
    /// config file used to ask for it.
    #[test]
    fn a_changed_value_is_dumped_resolved() {
        let json = changed("[cursor.style]\nshape = \"Beam\"\n");

        assert_eq!(json["cursor"]["shape"], serde_json::json!("Beam"));
    }

    #[test]
    fn changed_env_values_are_redacted_but_their_names_survive() {
        let json = changed("[env]\nGITHUB_TOKEN = \"ghp_secret\"\n");

        assert_eq!(json["env"]["GITHUB_TOKEN"], serde_json::json!(REDACTED_VALUE));
        assert!(
            !json.to_string().contains("ghp_secret"),
            "a token in [env] must not reach the log"
        );
    }

    /// The shell is a path like every project root already in the log, and
    /// which shell ran is most of a terminal bug report.
    #[test]
    fn the_changed_shell_is_not_redacted() {
        let json = changed("[terminal.shell]\nprogram = \"nu\"\nargs = [\"-l\"]\n");

        assert_eq!(json["shell"]["program"], serde_json::json!("nu"));
        assert_eq!(json["shell"]["args"], serde_json::json!(["-l"]));
    }

    #[test]
    fn the_dump_is_one_line() {
        let dump = config_from("[ui]\ngpu_grid = true\n")
            .changed_from_defaults()
            .expect("something changed");

        assert!(!dump.contains('\n'), "a multi-line dump can be interleaved");
    }

    #[test]
    fn herdr_defaults_to_enabled_with_a_two_second_poll() {
        let config = Config::default();
        assert!(config.ui.herdr.enabled);
        assert_eq!(config.ui.herdr.poll_interval, Duration::from_millis(2000));
        assert!(config.ui.herdr.show_unmatched);
    }

    #[test]
    fn herdr_can_be_turned_off() {
        let toml = "[ui.herdr]\nenabled = false\npoll_interval_ms = 5000\n";
        let config = config_from(toml);
        assert!(!config.ui.herdr.enabled);
        assert_eq!(config.ui.herdr.poll_interval, Duration::from_millis(5000));
    }
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
            "[ui.path_style.filename]\ncolor = \"#e6e6e6\"\nbold = \
             true\n[ui.path_style.parent]\nitalic = true\n",
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

    /// Losing the view costs a click to get back, so the ask is on by
    /// default even though the close prompt is not.
    #[test]
    fn confirm_session_detach_defaults_to_asking() {
        assert!(ui_from_toml("").confirm_session_detach);
    }

    #[test]
    fn confirm_session_detach_can_be_turned_off() {
        let ui = ui_from_toml("[ui]\nconfirm_session_detach = false");
        assert!(!ui.confirm_session_detach);
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
        assert_eq!(ui_from_toml("").icons.search.or_glyph(""), DEFAULT_SEARCH_ICON.as_str());
        assert_eq!(
            ui_from_toml("[ui.icons]\nsearch = \"\u{f002}\"").icons.search.or_glyph(""),
            "\u{f002}"
        );
    }

    #[test]
    fn icons_defaults_are_unchanged_for_every_pre_existing_key() {
        let ui = ui_from_toml("");
        assert_eq!(ui.icons.worktree_main.or_glyph(""), "●");
        assert_eq!(ui.icons.worktree.or_glyph(""), "○");
        assert_eq!(ui.icons.session.or_glyph(""), "▪");
        assert_eq!(ui.icons.home.or_glyph(""), "⌂");
        assert_eq!(ui.icons.project_expanded.or_glyph(""), "▾");
        assert_eq!(ui.icons.project_collapsed.or_glyph(""), "▸");
        assert_eq!(ui.icons.pr_open.or_glyph(""), "⬤");
        assert_eq!(ui.icons.pr_draft.or_glyph(""), "◯");
    }

    #[test]
    fn a_bare_string_icon_override_parses_through_the_real_config_path() {
        let ui = ui_from_toml("[ui.icons]\nworktree = \"◆\"");
        assert_eq!(ui.icons.worktree.or_glyph("○"), "◆");
        assert_eq!(ui.icons.worktree.color, None);
        assert!(!ui.icons.worktree.bold);
        // An untouched key keeps its default glyph, unaffected by a sibling
        // key's override.
        assert_eq!(ui.icons.worktree_main.or_glyph(""), "●");
    }

    #[test]
    fn a_table_icon_override_parses_glyph_color_weight_slant_and_size_through_the_real_config_path()
    {
        let ui = ui_from_toml(
            "[ui.icons]\nupstream_gone = { glyph = \"⌫\", color = \"#ff5555\", bold = true, \
             italic = true, size = 14 }",
        );
        let style = &ui.icons.upstream_gone;
        assert_eq!(style.or_glyph(""), "⌫");
        assert_eq!(style.color, Some(Color32::from_rgb(0xff, 0x55, 0x55)));
        assert!(style.bold);
        assert!(style.italic);
        assert_eq!(style.size, Some(14.0));
    }

    #[derive(Deserialize)]
    struct IconStyleWrapper {
        icon: RawIconStyle,
    }

    fn icon_style_from_toml(input: &str) -> IconStyle {
        let wrapper: IconStyleWrapper = toml::from_str(input).expect("valid toml");
        wrapper.icon.into()
    }

    #[test]
    fn icon_style_parses_a_bare_string() {
        let icon = icon_style_from_toml("icon = \"◆\"");
        assert_eq!(icon.or_glyph("○"), "◆");
        assert_eq!(icon.color, None);
        assert!(!icon.bold);
    }

    #[test]
    fn icon_style_parses_a_table() {
        let icon = icon_style_from_toml(
            "icon = { glyph = \"⌫\", color = \"#ff5555\", bold = true, size = 12 }",
        );
        assert_eq!(icon.or_glyph("x"), "⌫");
        assert_eq!(icon.color, Some(Color32::from_rgb(0xff, 0x55, 0x55)));
        assert!(icon.bold);
        assert_eq!(icon.size, Some(12.0));
    }

    #[test]
    fn a_blank_glyph_falls_back_to_the_default() {
        let icon = icon_style_from_toml("icon = \"   \"");
        assert_eq!(icon.or_glyph("○"), "○");

        let icon = icon_style_from_toml("icon = { color = \"#ff0000\" }");
        assert_eq!(icon.or_glyph("○"), "○", "a table with no glyph keeps the default");
    }

    #[test]
    fn last_session_close_defaults_to_respawn() {
        let ui = ui_from_toml("");
        assert_eq!(ui.last_session_close, LastSessionClose::Respawn);
    }

    #[test]
    fn last_session_close_parses_all_values() {
        for (raw, expected) in [
            ("respawn", LastSessionClose::Respawn),
            ("navigate", LastSessionClose::Navigate),
            ("ring_global", LastSessionClose::RingGlobal),
            ("ring_project", LastSessionClose::RingProject),
        ] {
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
    fn only_the_ring_values_ring() {
        assert!(!LastSessionClose::Respawn.rings());
        assert!(!LastSessionClose::Navigate.rings());
        assert!(LastSessionClose::RingGlobal.rings());
        assert!(LastSessionClose::RingProject.rings());
    }

    #[test]
    fn sidebar_focus_defaults_to_preserve() {
        let ui = ui_from_toml("");
        assert_eq!(ui.sidebar_focus, SidebarFocus::Preserve);
    }

    #[test]
    fn sidebar_focus_parses_all_values() {
        for (raw, expected) in
            [("preserve", SidebarFocus::Preserve), ("follow", SidebarFocus::Follow)]
        {
            let ui = ui_from_toml(&format!("[ui]\nsidebar_focus = \"{raw}\""));
            assert_eq!(ui.sidebar_focus, expected, "value {raw:?}");
        }
    }

    #[test]
    fn sidebar_focus_invalid_falls_back_to_preserve() {
        let ui = ui_from_toml("[ui]\nsidebar_focus = \"sideways\"");
        assert_eq!(ui.sidebar_focus, SidebarFocus::Preserve);
    }

    #[test]
    fn a_retired_sidebar_focus_value_still_parses_to_the_default() {
        // "reset" named the pre-reconciler behavior and was removed rather than
        // kept as a mode.  A config file carrying it must start, not refuse.
        let ui = ui_from_toml("[ui]\nsidebar_focus = \"reset\"");
        assert_eq!(ui.sidebar_focus, SidebarFocus::Preserve);
    }

    #[test]
    fn only_follow_moves_the_terminal() {
        assert!(!SidebarFocus::Preserve.follows());
        assert!(SidebarFocus::Follow.follows());
    }

    #[test]
    fn sidebar_follow_active_defaults_to_off() {
        assert!(!ui_from_toml("").sidebar_follow_active);
    }

    #[test]
    fn sidebar_follow_active_parses() {
        assert!(ui_from_toml("[ui]\nsidebar_follow_active = true").sidebar_follow_active);
    }

    #[test]
    fn sidebar_scroll_align_defaults_to_minimal() {
        assert_eq!(ui_from_toml("").sidebar_scroll_align, ScrollAlign::Minimal);
    }

    #[test]
    fn sidebar_scroll_align_parses_all_values() {
        for (raw, expected) in [("minimal", ScrollAlign::Minimal), ("center", ScrollAlign::Center)]
        {
            let ui = ui_from_toml(&format!("[ui]\nsidebar_scroll_align = \"{raw}\""));
            assert_eq!(ui.sidebar_scroll_align, expected, "value {raw:?}");
        }
    }

    #[test]
    fn sidebar_scroll_align_invalid_falls_back_to_minimal() {
        let ui = ui_from_toml("[ui]\nsidebar_scroll_align = \"middle-ish\"");
        assert_eq!(ui.sidebar_scroll_align, ScrollAlign::Minimal);
    }

    /// The hints are what an unmodified config already shows, so the key has
    /// to default on: a `false` default would take them away from everyone who
    /// never asked for the setting.
    #[test]
    fn icon_tooltips_default_on_and_can_be_refused() {
        assert!(ui_from_toml("").icon_tooltips);
        assert!(ui_from_toml("[ui]\nicon_tooltips = true").icon_tooltips);
        assert!(!ui_from_toml("[ui]\nicon_tooltips = false").icon_tooltips);
    }

    /// Row names and button hints answer to different keys, so silencing one
    /// must leave the other untouched.
    #[test]
    fn icon_tooltips_and_sidebar_tooltips_are_independent() {
        let ui = ui_from_toml("[ui]\nsidebar_tooltips = \"off\"");
        assert!(ui.icon_tooltips);

        let ui = ui_from_toml("[ui]\nicon_tooltips = false");
        assert_eq!(ui.sidebar_tooltips, SidebarTooltips::Elided);
    }

    #[test]
    fn sidebar_tooltips_defaults_to_elided() {
        assert_eq!(ui_from_toml("").sidebar_tooltips, SidebarTooltips::Elided);
    }

    #[test]
    fn sidebar_tooltips_parses_every_value() {
        for (raw, expected) in [
            ("off", SidebarTooltips::Off),
            ("elided", SidebarTooltips::Elided),
            ("always", SidebarTooltips::Always),
        ] {
            let ui = ui_from_toml(&format!("[ui]\nsidebar_tooltips = \"{raw}\""));
            assert_eq!(ui.sidebar_tooltips, expected, "value {raw:?}");
        }
    }

    #[test]
    fn sidebar_tooltips_invalid_falls_back_to_elided() {
        let ui = ui_from_toml("[ui]\nsidebar_tooltips = \"hover\"");
        assert_eq!(ui.sidebar_tooltips, SidebarTooltips::Elided);
    }

    #[test]
    fn search_scope_defaults_to_filtered() {
        let ui = ui_from_toml("");
        assert_eq!(ui.search_scope, SearchScope::Filtered);
    }

    #[test]
    fn search_scope_parses_both_values() {
        for (raw, expected) in [("filtered", SearchScope::Filtered), ("all", SearchScope::All)] {
            let ui = ui_from_toml(&format!("[ui]\nsearch_scope = \"{raw}\""));
            assert_eq!(ui.search_scope, expected, "value {raw:?}");
        }
    }

    #[test]
    fn search_scope_invalid_falls_back_to_filtered() {
        let ui = ui_from_toml("[ui]\nsearch_scope = \"everywhere\"");
        assert_eq!(ui.search_scope, SearchScope::Filtered);
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
        assert_eq!(config.profiles[0], Profile {
            name: "pwsh".into(),
            program: "pwsh".into(),
            args: vec!["-NoLogo".into()]
        });
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
    fn profile_command_joins_program_and_args() {
        let no_args = Profile { name: "cmd".into(), program: "cmd.exe".into(), args: vec![] };
        assert_eq!(profile_command(&no_args), "cmd.exe");

        let with_args = Profile {
            name: "ubuntu".into(),
            program: "wsl.exe".into(),
            args: vec!["-d".into(), "ubuntu".into()],
        };
        assert_eq!(profile_command(&with_args), "wsl.exe -d ubuntu");
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
    fn session_reorder_defaults_to_off_and_workspace_scope() {
        let ui = ui_from_toml("");
        assert!(!ui.session_reorder.drag);
        assert_eq!(ui.session_reorder.scope, ReorderScope::Workspace);
    }

    #[test]
    fn session_reorder_parses_every_scope() {
        for (raw, expected) in [
            ("workspace", ReorderScope::Workspace),
            ("project", ReorderScope::Project),
            ("anywhere", ReorderScope::Anywhere),
        ] {
            let ui = ui_from_toml(&format!("[ui.session_reorder]\nscope = \"{raw}\""));
            assert_eq!(ui.session_reorder.scope, expected, "value {raw:?}");
        }
    }

    #[test]
    fn session_reorder_invalid_scope_falls_back_to_workspace() {
        let ui = ui_from_toml("[ui.session_reorder]\nscope = \"everywhere\"");
        assert_eq!(ui.session_reorder.scope, ReorderScope::Workspace);
    }

    #[test]
    fn session_reorder_partial_table_leaves_the_other_key_alone() {
        let ui = ui_from_toml("[ui.session_reorder]\ndrag = true");
        assert!(ui.session_reorder.drag);
        assert_eq!(ui.session_reorder.scope, ReorderScope::Workspace);
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
    fn ui_font_variant_families_parse_and_default_to_none() {
        let config = parse("[ui.font]\nfamily = \"Inter\"\nbold_family = \"Inter Display\"");
        assert_eq!(config.ui_font.bold_family.as_deref(), Some("Inter Display"));
        assert_eq!(config.ui_font.italic_family, None);
        assert_eq!(config.ui_font.bold_italic_family, None);
    }

    #[test]
    fn blank_ui_font_variant_families_are_ignored() {
        let config = parse("[ui.font]\nbold_family = \"  \"");
        assert_eq!(config.ui_font.bold_family, None);
    }

    /// The face is appended last, so enabling it cannot disturb a font that
    /// already renders a glyph — which is why it is on unless refused.
    #[test]
    fn builtin_symbols_defaults_on_and_can_be_refused() {
        assert!(parse("").ui_font.builtin_symbols);
        assert!(parse("[ui.font]\nsize = 12").ui_font.builtin_symbols);
        assert!(!parse("[ui.font]\nbuiltin_symbols = false").ui_font.builtin_symbols);
        assert!(parse("[ui.font]\nbuiltin_symbols = true").ui_font.builtin_symbols);
    }

    /// A derived `Default` would make the documented default silently invert.
    #[test]
    fn the_ui_font_default_is_not_the_derived_zero_value() {
        assert!(UiFont::default().builtin_symbols);
    }

    #[test]
    fn icons_default_to_todays_glyphs() {
        let ui = ui_from_toml("");
        assert_eq!(ui.icons, Icons::default());
        assert_eq!(ui.icons.worktree_main.or_glyph(""), "●");
        assert_eq!(ui.icons.worktree.or_glyph(""), "○");
        assert_eq!(ui.icons.session.or_glyph(""), "▪");
        assert_eq!(ui.icons.home.or_glyph(""), "⌂");
        assert_eq!(ui.icons.project_expanded.or_glyph(""), "▾");
        assert_eq!(ui.icons.project_collapsed.or_glyph(""), "▸");
        assert_eq!(ui.icons.pr_open.or_glyph(""), "⬤");
        assert_eq!(ui.icons.pr_draft.or_glyph(""), "◯");
        assert_eq!(ui.icons.pr_merged.or_glyph(""), "⬤");
        assert_eq!(ui.icons.pr_closed.or_glyph(""), "⬤");
    }

    #[test]
    fn icon_overrides_apply_and_trim() {
        let ui = ui_from_toml("[ui.icons]\nworktree = \" W \"\nhome = \"H\"");
        assert_eq!(ui.icons.worktree.or_glyph(""), "W");
        assert_eq!(ui.icons.home.or_glyph(""), "H");
        assert_eq!(ui.icons.worktree_main.or_glyph(""), "●", "untouched fields keep defaults");
    }

    #[test]
    fn blank_icon_override_falls_back() {
        // `build_icons` stores a blank override's glyph as-is (blank, not
        // `None`) and defers filtering to `or_glyph` at the paint site — the
        // same deferral a table with no `glyph` key gets. A sentinel default
        // that differs from both the raw blank input and every built-in
        // glyph makes the assertion fail if that filtering ever breaks.
        let ui = ui_from_toml("[ui.icons]\nworktree_main = \"   \"\nsession = \"\"");
        assert_eq!(ui.icons.worktree_main.or_glyph("sentinel"), "sentinel");
        assert_eq!(ui.icons.session.or_glyph("sentinel"), "sentinel");
    }

    #[test]
    fn upstream_icons_have_defaults() {
        let ui = ui_from_toml("");
        assert_eq!(ui.icons.upstream_level.or_glyph(""), "✓");
        assert_eq!(ui.icons.upstream_diverged.or_glyph(""), "⇅");
        assert_eq!(ui.icons.upstream_gone.or_glyph(""), "⌫");
        assert_eq!(ui.icons.upstream_untracked.or_glyph(""), "↑");
    }

    /// Three buttons paint the same glyph for three different actions, one of
    /// which deletes a branch.  Separate keys are what let the destructive one
    /// be marked without touching the others.
    #[test]
    fn each_chrome_action_takes_its_own_icon_key() {
        let ui = ui_from_toml("");
        assert_eq!(ui.icons.add_project.or_glyph(""), "+");
        assert_eq!(ui.icons.new_worktree.or_glyph(""), "+");
        assert_eq!(ui.icons.new_session.or_glyph(""), "+");
        assert_eq!(ui.icons.remove_project.or_glyph(""), "×");
        assert_eq!(ui.icons.delete_worktree.or_glyph(""), "×");
        assert_eq!(ui.icons.close_session.or_glyph(""), "×");
        assert_eq!(ui.icons.refresh.or_glyph(""), "↻");
        assert_eq!(ui.icons.reorder.or_glyph(""), "⇅");

        let ui =
            ui_from_toml("[ui.icons]\ndelete_worktree = { glyph = \"✖\", color = \"#ff5555\" }");
        assert_eq!(ui.icons.delete_worktree.or_glyph(""), "✖");
        assert_eq!(ui.icons.delete_worktree.color, Some(Color32::from_rgb(0xff, 0x55, 0x55)));
        // A sibling sharing the same default glyph is unaffected.
        assert_eq!(ui.icons.close_session.or_glyph(""), "×");
        assert_eq!(ui.icons.close_session.color, None);
    }

    /// `reorder` and `upstream_diverged` default to the same glyph but are
    /// independent keys.
    #[test]
    fn reorder_and_upstream_diverged_are_configured_separately() {
        let ui = ui_from_toml("[ui.icons]\nreorder = \"⇕\"");
        assert_eq!(ui.icons.reorder.or_glyph(""), "⇕");
        assert_eq!(ui.icons.upstream_diverged.or_glyph(""), "⇅");
    }

    #[test]
    fn pr_status_defaults_off_and_parses_on() {
        assert!(!ui_from_toml("").pr_status);
        assert!(ui_from_toml("[ui]\npr_status = true").pr_status);
    }

    #[test]
    fn upstream_status_defaults_off_and_parses_on() {
        assert!(!ui_from_toml("").upstream_status);
        assert!(ui_from_toml("[ui]\nupstream_status = true").upstream_status);
    }

    #[test]
    fn pr_status_concurrency_is_unset_by_default() {
        assert_eq!(ui_from_toml("").pr_status_concurrency, None);
        assert_eq!(ui_from_toml("[ui]\npr_status_concurrency = 4").pr_status_concurrency, Some(4));
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
            "[ui.focus_outline]\nsidebar = true\nterminal = true\ncolor = \"#89b4fa\"\nthickness \
             = 2.5",
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

    /// A boosted session outranks everything else the machine is doing, so an
    /// unmodified config must never get it.
    #[test]
    fn focus_priority_boost_defaults_off() {
        assert!(!ui_from_toml("").focus_priority_boost);
    }

    #[test]
    fn focus_priority_boost_parses() {
        assert!(ui_from_toml("[ui]\nfocus_priority_boost = true").focus_priority_boost);
    }

    /// Killing what a closing session started is a change of behavior the
    /// killed process has no say in, so an unmodified config must not get it.
    #[test]
    fn reap_descendants_on_close_defaults_off() {
        assert!(!ui_from_toml("").reap_descendants_on_close);
    }

    #[test]
    fn reap_descendants_on_close_parses() {
        let ui = ui_from_toml("[ui]\nreap_descendants_on_close = true");
        assert!(ui.reap_descendants_on_close);
    }

    #[test]
    fn vsync_defaults_on() {
        assert!(ui_from_toml("").vsync);
    }

    #[test]
    fn vsync_parses() {
        assert!(!ui_from_toml("[ui]\nvsync = false").vsync);
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

    #[test]
    fn quoting_none_passes_the_path_through() {
        assert_eq!(ShellQuoting::None.escape("hello ($world)"), "hello ($world)");
    }

    #[test]
    fn quoting_spaces_only_escapes_spaces_and_nothing_else() {
        assert_eq!(ShellQuoting::SpacesOnly.escape("hello ($world)"), "hello\\ ($world)");
    }

    #[test]
    fn quoting_posix_single_quotes_a_path_with_shell_metacharacters() {
        assert_eq!(ShellQuoting::Posix.escape("hello ($world)"), "'hello ($world)'");
        assert_eq!(ShellQuoting::Posix.escape("/mnt/c/plain.png"), "/mnt/c/plain.png");
        assert_eq!(
            ShellQuoting::Posix.escape("/mnt/c/Users/Lev/my pic.png"),
            "'/mnt/c/Users/Lev/my pic.png'"
        );
    }

    #[test]
    fn quoting_windows_quotes_only_when_the_path_needs_it() {
        assert_eq!(ShellQuoting::Windows.escape("hello ($world)"), "\"hello ($world)\"");
        assert_eq!(ShellQuoting::Windows.escape("C:\\pics\\plain.png"), "C:\\pics\\plain.png");
    }

    #[test]
    fn quoting_windows_always_quoted_quotes_unconditionally() {
        assert_eq!(
            ShellQuoting::WindowsAlwaysQuoted.escape("C:\\pics\\plain.png"),
            "\"C:\\pics\\plain.png\""
        );
    }

    #[test]
    fn auto_quoting_picks_posix_inside_a_distro() {
        assert_eq!(Quoting::Auto.resolve(true), ShellQuoting::Posix);
    }

    #[test]
    fn auto_quoting_picks_the_host_default_outside_a_distro() {
        let expected = if cfg!(windows) { ShellQuoting::Windows } else { ShellQuoting::SpacesOnly };
        assert_eq!(Quoting::Auto.resolve(false), expected);
    }

    #[test]
    fn an_explicit_quoting_mode_ignores_the_shell() {
        assert_eq!(Quoting::None.resolve(true), ShellQuoting::None);
        assert_eq!(Quoting::Windows.resolve(true), ShellQuoting::Windows);
    }

    #[test]
    fn drop_options_default_to_on_with_auto_quoting() {
        let ui = ui_from_toml("");
        assert_eq!(ui.drop, DropConfig {
            enabled: true,
            terminal: true,
            sidebar: true,
            scratchpad: true,
            spelling: PathSpelling { quote: Quoting::Auto, wsl_translate: true },
            highlight: true,
        });
    }

    #[test]
    fn drop_options_parse_from_the_ui_drop_table() {
        let ui = ui_from_toml(
            "[ui.drop]\nenabled = false\nterminal = false\nsidebar = false\nscratchpad = \
             false\nquote = \"posix\"\nwsl_translate = false\nhighlight = false\n",
        );
        assert_eq!(ui.drop, DropConfig {
            enabled: false,
            terminal: false,
            sidebar: false,
            scratchpad: false,
            spelling: PathSpelling { quote: Quoting::Posix, wsl_translate: false },
            highlight: false,
        });
    }

    #[test]
    fn sidebar_attention_parses_and_defaults_to_none() {
        assert_eq!(ui_from_toml("").sidebar_attention, None);
        assert_eq!(
            ui_from_toml("[ui]\nsidebar_attention = \"#ffb86c\"").sidebar_attention,
            Some(Color32::from_rgb(0xff, 0xb8, 0x6c))
        );
    }

    #[test]
    fn paste_options_default_to_on_with_the_owned_image_dir() {
        let ui = ui_from_toml("");
        assert_eq!(ui.paste, PasteConfig {
            files: true,
            image: true,
            image_dir: None,
            image_keep: 20
        });
        let (dir, owned) = ui.paste.image_target();
        assert_eq!(dir, default_image_dir());
        assert!(owned, "the default directory is alacritree's own");
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_image_default_prefers_the_user_cache() {
        let cache = PathBuf::from("/home/example/.cache/alacritree");
        assert_eq!(unix_default_image_dir(Some(cache.clone()), Path::new("/tmp"), 1234), cache);
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_image_fallback_is_namespaced_by_user() {
        assert_eq!(
            unix_default_image_dir(None, Path::new("/tmp"), 1234),
            PathBuf::from("/tmp/alacritree-1234")
        );
    }

    #[test]
    fn paste_options_parse_from_the_ui_paste_table() {
        let home = home::home_dir().expect("a home directory");
        let ui = ui_from_toml(
            "[ui.paste]\nfiles = false\nimage = false\nimage_dir = \"~/shots\"\nimage_keep = 5\n",
        );
        assert_eq!(ui.paste, PasteConfig {
            files: false,
            image: false,
            image_dir: Some(home.join("shots")),
            image_keep: 5,
        });
    }

    /// A directory the user chose may hold files alacritree never wrote, so it is
    /// never swept — that is what makes pointing this at a pictures folder safe.
    #[test]
    fn a_configured_image_dir_is_not_owned() {
        let ui = ui_from_toml("[ui.paste]\nimage_dir = \"~/shots\"");
        let (dir, owned) = ui.paste.image_target();
        assert_eq!(dir, home::home_dir().expect("a home directory").join("shots"));
        assert!(!owned);
    }

    /// A relative path is rejected by `parse_config_path`, which must leave the
    /// owned default in place rather than writing somewhere arbitrary.
    #[test]
    fn an_unusable_image_dir_falls_back_to_the_owned_default() {
        let ui = ui_from_toml("[ui.paste]\nimage_dir = \"relative/path\"");
        assert_eq!(ui.paste.image_dir, None);
        assert!(ui.paste.image_target().1);
    }

    /// The cap can never reach zero: a paste hands the shell a path, and the shell
    /// opens it after the sweep has already run.
    #[test]
    fn an_image_keep_of_zero_is_raised_to_one() {
        assert_eq!(ui_from_toml("[ui.paste]\nimage_keep = 0").paste.image_keep, 1);
    }

    #[test]
    fn every_quoting_name_parses() {
        for (raw, expected) in [
            ("auto", Quoting::Auto),
            ("none", Quoting::None),
            ("spaces_only", Quoting::SpacesOnly),
            ("posix", Quoting::Posix),
            ("windows", Quoting::Windows),
            ("windows_always_quoted", Quoting::WindowsAlwaysQuoted),
        ] {
            let ui = ui_from_toml(&format!("[ui.drop]\nquote = \"{raw}\""));
            assert_eq!(ui.drop.spelling.quote, expected, "{raw}");
        }
    }

    #[test]
    fn an_unknown_quoting_name_falls_back_to_auto() {
        let ui = ui_from_toml("[ui.drop]\nquote = \"shell\"");
        assert_eq!(ui.drop.spelling.quote, Quoting::Auto);
    }

    /// Asserts against bare literals independent of the constants, like
    /// `the_chrome_slice_carries_the_action_and_decorative_glyphs` below, so a
    /// typo'd `DEFAULT_*_ICON` fails here.  Three PR-status icons share `⬤`,
    /// so a per-glyph `contains` would miss a typo hiding behind a duplicate;
    /// comparing the full sorted multiset catches it instead.
    #[test]
    fn the_icon_slice_carries_exactly_the_default_icon_glyphs() {
        let mut icons: Vec<&str> = DEFAULT_ICON_GLYPHS.iter().map(|g| g.as_str()).collect();
        icons.sort_unstable();
        let mut expected =
            ["⌕", "●", "○", "▪", "◫", "⌂", "▾", "▸", "⬤", "◯", "⬤", "⬤", "✓", "⇅", "⌫", "↑"];
        expected.sort_unstable();
        assert_eq!(icons, expected);
    }

    /// The action-button and decorative glyphs are painted from literals that no
    /// config key names, so nothing else would notice their absence.
    #[test]
    fn the_chrome_slice_carries_the_action_and_decorative_glyphs() {
        let chrome: Vec<&str> = CHROME_GLYPHS.iter().map(|g| g.as_str()).collect();
        for g in ["◇", "+", "×", "↻", "⇅", "·", "—", "•", "…", "↓", "⠿", "▌", "◐"]
        {
            assert!(chrome.contains(&g), "{g} is missing from CHROME_GLYPHS");
        }
        // The blocked mark shares `DEFAULT_WORKTREE_MAIN_ICON`'s codepoint, so
        // the icon slice's own multiset check cannot notice it going missing.
        assert!(chrome.contains(&"●"), "● is missing from CHROME_GLYPHS");
    }

    /// A derived `Default` on a bare `bool` would make this false and silently
    /// invert the intended default, so the raw field is an `Option` resolved with
    /// `unwrap_or` — the same shape `wsl.resident_helper` uses.
    #[test]
    fn crash_logging_is_on_unless_asked_otherwise() {
        let raw: RawConfig = toml::from_str("").unwrap();

        let config = raw.into_config();

        assert!(config.debug.crash_log);
        assert!(!config.debug.persistent_logging);
    }

    #[test]
    fn crash_logging_can_be_turned_off() {
        let raw: RawConfig = toml::from_str("[debug]\ncrash_log = false").unwrap();

        assert!(!raw.into_config().debug.crash_log);
    }

    #[test]
    fn persistent_logging_can_be_turned_on() {
        let raw: RawConfig = toml::from_str("[debug]\npersistent_logging = true").unwrap();

        assert!(raw.into_config().debug.persistent_logging);
    }

    #[test]
    fn state_dir_defaults_to_none_and_expands_a_leading_tilde() {
        let unset: RawConfig = toml::from_str("").unwrap();
        let raw: RawConfig = toml::from_str("[general]\nstate_dir = \"~/alacritree\"").unwrap();

        assert_eq!(unset.into_config().state_dir, None);
        assert_eq!(raw.into_config().state_dir, Some(home::home_dir().unwrap().join("alacritree")));
    }

    #[test]
    fn log_dir_defaults_to_none_and_expands_a_leading_tilde() {
        let unset: RawConfig = toml::from_str("").unwrap();
        let raw: RawConfig = toml::from_str("[debug]\nlog_dir = \"~/logs\"").unwrap();

        assert_eq!(unset.into_config().debug.log_dir, None);
        assert_eq!(raw.into_config().debug.log_dir, Some(home::home_dir().unwrap().join("logs")));
    }

    /// A relative path would resolve against the process CWD, which for a GUI
    /// launch is wherever the desktop happened to start it.
    #[test]
    fn a_relative_log_dir_is_ignored() {
        let raw: RawConfig = toml::from_str("[debug]\nlog_dir = \"logs\"").unwrap();

        assert_eq!(raw.into_config().debug.log_dir, None);
    }

    #[test]
    fn frame_logging_is_off_unless_asked_for() {
        let off: RawConfig = toml::from_str("").unwrap();
        let on: RawConfig = toml::from_str("[debug]\nframe_log = true").unwrap();

        assert!(!off.into_config().debug.frame_log);
        assert!(on.into_config().debug.frame_log);
    }

    #[test]
    fn gpu_timing_is_off_unless_asked_for() {
        let off: RawConfig = toml::from_str("").unwrap();
        let on: RawConfig = toml::from_str("[debug]\ngpu_timing = true").unwrap();

        assert!(!off.into_config().debug.gpu_timing);
        assert!(on.into_config().debug.gpu_timing);
    }

    /// `[debug]` in both files merges key by key rather than the later table
    /// replacing the earlier one wholesale.
    #[test]
    fn a_debug_table_in_both_files_merges_key_by_key() {
        let alacritty: toml::Value = toml::from_str("[debug]\npersistent_logging = true").unwrap();
        let alacritree: toml::Value = toml::from_str("[debug]\ncrash_log = false").unwrap();

        let merged = merge(alacritty, alacritree);
        let config: RawConfig = merged.try_into().unwrap();
        let config = config.into_config();

        assert!(config.debug.persistent_logging, "the alacritty.toml key was dropped");
        assert!(!config.debug.crash_log, "the alacritree.toml key was dropped");
    }

    #[test]
    fn every_accepted_adjustment_spelling_parses() {
        assert_eq!(Adjust::parse("0"), Some(Adjust::Points(0.0)));
        assert_eq!(Adjust::parse("-2"), Some(Adjust::Points(-2.0)));
        assert_eq!(Adjust::parse("1.5"), Some(Adjust::Points(1.5)));
        assert_eq!(Adjust::parse("2pt"), Some(Adjust::Points(2.0)));
        assert_eq!(Adjust::parse("2px"), Some(Adjust::Pixels(2.0)));
        assert_eq!(Adjust::parse("-2px"), Some(Adjust::Pixels(-2.0)));
        assert_eq!(Adjust::parse("150%"), Some(Adjust::Scale(1.5)));
    }

    /// A percentage is a magnitude.  kitty silently takes the absolute value of a
    /// negative one, which gives back a line the user did not ask for and no way
    /// to tell that happened.
    #[test]
    fn unusable_adjustment_spellings_are_rejected() {
        for text in ["", "abc", "2 px", "-150%", "px", "%", "nan", "inf"] {
            assert_eq!(Adjust::parse(text), None, "{text:?} should not parse");
        }
    }

    /// The two spellings of "leave it alone" have to agree, since one is the
    /// default and the other is what a user writes to say the same thing.
    #[test]
    fn a_zero_adjustment_is_the_identity_in_both_units() {
        assert_eq!(Adjust::parse("0").unwrap().apply(7.0, 2.0), 7.0);
        assert_eq!(Adjust::parse("100%").unwrap().apply(7.0, 2.0), 7.0);
        assert_eq!(Adjust::NONE.apply(7.0, 2.0), 7.0);
    }

    /// Pixels are physical and points are not, which is the whole reason both
    /// spellings exist.
    #[test]
    fn pixels_are_absolute_and_points_scale_with_the_display() {
        assert_eq!(Adjust::parse("2px").unwrap().apply(10.0, 2.0), 12.0);
        assert_eq!(Adjust::parse("2pt").unwrap().apply(10.0, 2.0), 14.0);
        assert_eq!(Adjust::parse("150%").unwrap().apply(10.0, 2.0), 15.0);
    }

    /// A malformed knob must not fail the whole config load, and must not leave
    /// the line somewhere the user cannot predict.
    #[test]
    fn a_malformed_adjustment_behaves_as_zero() {
        assert_eq!(parse_adjust("underline_position", Some("2 px")), Adjust::NONE);
        assert_eq!(parse_adjust("underline_position", None), Adjust::NONE);
    }
}
