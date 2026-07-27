//! Where a dropped file goes and what text it becomes.
//!
//! The decisions are functions of a pointer position, a set of paths and the
//! config, with two exceptions: `project_roots` stats the paths it is given,
//! and `screen_pointer` asks the OS and egui where the cursor is.  `app.rs`
//! supplies the region rectangles and owns the sinks; keeping the rest out of
//! the frame loop is what makes it testable without a window.

use std::path::{Path, PathBuf};

use crate::config::{DropConfig, ShellQuoting};
use crate::wsl;

/// Which region a drop landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Terminal,
    ProjectsSidebar,
    Scratchpad,
}

/// The drop-accepting rectangles of the current frame, in egui coordinates.
/// The git-status sidebar is deliberately absent: it accepts nothing, so a
/// drop over it falls through to `None`.
pub struct Regions {
    /// `None` when the projects sidebar is hidden or its target is disabled.
    pub sidebar: Option<egui::Rect>,
    pub central: egui::Rect,
}

impl Regions {
    /// A hidden sidebar and a disabled sidebar target collapse to the same
    /// `None`, so `route` needs to know about neither.
    pub fn new(sidebar: Option<egui::Rect>, central: egui::Rect, cfg: &DropConfig) -> Self {
        Self { sidebar: sidebar.filter(|_| cfg.sidebar), central }
    }
}

/// `None` when the drop lands nowhere useful — outside every region, over the
/// git-status sidebar, or on a target the config switched off.
///
/// An unknown pointer resolves to the central panel rather than nothing:
/// winit reports no cursor position during a drag on any platform, so off
/// Windows this is the only branch that ever runs, and pasting into the shell
/// is what every other terminal does with a drop.
pub fn route(
    pointer: Option<egui::Pos2>,
    regions: &Regions,
    active_is_scratchpad: bool,
    cfg: &DropConfig,
) -> Option<Target> {
    if !cfg.enabled {
        return None;
    }
    let central = match (active_is_scratchpad, cfg.scratchpad, cfg.terminal) {
        (true, true, _) => Some(Target::Scratchpad),
        (false, _, true) => Some(Target::Terminal),
        _ => None,
    };
    let Some(pointer) = pointer else {
        return central;
    };
    if regions.sidebar.is_some_and(|r| r.contains(pointer)) {
        return cfg.sidebar.then_some(Target::ProjectsSidebar);
    }
    if regions.central.contains(pointer) {
        return central;
    }
    None
}

/// Whether a path can go on a PTY without acting as input in its own right.
///
/// Every control character is rejected — C0, DEL and C1 — because far more of
/// them than the obvious two are commands to whatever is reading the line.
/// `paste::paste_bytes`'s unbracketed branch turns `\n` into `\r`, which is
/// Enter; readline binds `\x0f` to `operate-and-get-next`, which accepts the
/// line just as Enter does, `\x18\x05` to `edit-and-execute-command`, `\x04`
/// to end-of-file and `\t` to completion.  Quoting is no substitute for any of
/// it: the line editor acts on the byte before the shell parser ever sees the
/// quotes around it.
pub fn is_terminal_safe(path: &str) -> bool {
    !path.contains(char::is_control)
}

/// The text a set of dropped paths becomes for a shell: each path translated
/// and escaped, joined with spaces, with the trailing space wezterm appends so
/// the next argument does not run into the last path.
///
/// `distro` names the WSL distro the receiving session runs in, `None` for a
/// native session.  Paths that would act as terminal input are left out.
pub fn shell_payload(paths: &[PathBuf], distro: Option<&str>, cfg: &DropConfig) -> String {
    let mut out = String::new();
    for path in paths {
        let (word, quoting) = shell_word(path, distro, cfg);
        if !is_terminal_safe(&word) {
            log::warn!("dropped path {word:?} carries terminal control characters, skipping it");
            continue;
        }
        out.push_str(&quoting.escape(&word));
        out.push(' ');
    }
    out
}

/// A path as the receiving shell should spell it, with the quoting rules that
/// spelling implies.  A path rewritten for a distro is a POSIX shell word even
/// when the configured mode says otherwise — `windows` quoting fed to `bash` is
/// broken by construction.
fn shell_word(path: &Path, distro: Option<&str>, cfg: &DropConfig) -> (String, ShellQuoting) {
    if let Some(distro) = distro.filter(|_| cfg.wsl_translate) {
        if let Some(linux) = distro_path(path, distro) {
            return (linux, ShellQuoting::Posix);
        }
        log::debug!("no path in {distro} for {}, pasting it as-is", path.display());
    }
    (path.to_string_lossy().into_owned(), cfg.quote.resolve(distro.is_some()))
}

/// The path as `distro` resolves it, or `None` when it has no spelling there.
///
/// `windows_to_linux` discards which distro a UNC path names (`wsl.rs:141`), so
/// handing it `\\wsl.localhost\Ubuntu\home\a` inside a Kali session would return
/// `/home/a` — a different file, with nothing to show for it.  Classify first
/// and only accept a UNC path that belongs to this distro.
fn distro_path(path: &Path, distro: &str) -> Option<String> {
    match wsl::classify(path) {
        wsl::Location::Wsl { distro: owner, linux_path } => {
            if owner.eq_ignore_ascii_case(distro) {
                return Some(linux_path);
            }
            // Louder than the plain-UNC case below: a share that no distro owns
            // has no distro-side spelling at all, but this one looks as though
            // it does and silently would not resolve to the same file.
            log::warn!("{} belongs to {owner}, not {distro}; pasting it as-is", path.display());
            None
        },
        wsl::Location::Windows(_) => wsl::windows_to_linux(path),
    }
}

/// Dropped paths as text for the Markdown scratchpad: no shell quoting, one per
/// line, since a document is not a command line.
///
/// `preceding` and `following` are the characters either side of the insertion
/// point.  Without the boundary newlines a drop into the middle of a line welds
/// the first path onto the text before it and the last onto the text after.
pub fn document_payload(
    paths: &[PathBuf],
    preceding: Option<char>,
    following: Option<char>,
) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let needs_newline = |edge: Option<char>| matches!(edge, Some(c) if c != '\n');
    let mut out = String::new();
    if needs_newline(preceding) {
        out.push('\n');
    }
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&path.to_string_lossy());
    }
    if needs_newline(following) {
        out.push('\n');
    }
    out
}

/// The project roots a set of dropped paths names.  A directory is its own
/// root; a file means the directory holding it, which is what dragging a file
/// out of a checkout is asking for.  Dragging several files from one folder is
/// ordinary, so repeats collapse rather than adding the same project twice.
pub fn project_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for path in paths {
        let root = if path.is_dir() {
            Some(path.clone())
        } else if path.is_file() {
            path.parent().map(Path::to_path_buf)
        } else {
            // Both queries also come back false when the metadata call was
            // refused, so this covers a permission error as well as a path
            // that has gone away between the drag and the drop.
            log::debug!("dropped path {} is neither a file nor a directory", path.display());
            None
        };
        if let Some(root) = root.filter(|r| !roots.contains(r)) {
            roots.push(root);
        }
    }
    roots
}

/// The OS cursor in egui coordinates, or `None` when it cannot be known.
///
/// winit 0.30 discards the drag position on every platform — its Windows
/// `DragEnter`/`DragOver`/`Drop` handlers all ignore their `POINTL`, and no
/// `CursorMoved` is synthesized during a drag — so egui's own pointer is still
/// wherever it was before the drag began.  Asking Win32 directly is the only
/// way to learn where a drop landed.  No other platform has an equivalent
/// here, so they route by the central-panel fallback in `route`.
#[cfg(windows)]
pub fn screen_pointer(ctx: &egui::Context) -> Option<egui::Pos2> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `GetCursorPos` only writes a `POINT` through the pointer it is
    // given, and `point` is a live, correctly aligned local.
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return None;
    }
    let origin = ctx.input(|i| i.viewport().inner_rect)?.min;
    Some(to_egui_pos(point.x as f32, point.y as f32, origin, ctx.pixels_per_point()))
}

#[cfg(not(windows))]
pub fn screen_pointer(_ctx: &egui::Context) -> Option<egui::Pos2> {
    None
}

/// `ViewportInfo::inner_rect` is in monitor space at ui-point scale, which is
/// physical pixels divided by `Context::pixels_per_point` — the same divisor
/// `egui-winit` used to produce it.
///
/// Only the Windows `screen_pointer` has a pixel position to convert; the
/// conversion itself is platform-independent, so its test still runs everywhere.
#[cfg(any(windows, test))]
fn to_egui_pos(
    x_px: f32,
    y_px: f32,
    window_origin: egui::Pos2,
    pixels_per_point: f32,
) -> egui::Pos2 {
    egui::pos2(x_px / pixels_per_point - window_origin.x, y_px / pixels_per_point - window_origin.y)
}

#[cfg(test)]
mod tests {
    use egui::{Rect, pos2};

    use super::*;
    use crate::config::{DropConfig, Quoting};

    fn regions() -> Regions {
        Regions {
            sidebar: Some(Rect::from_min_max(pos2(0.0, 0.0), pos2(200.0, 600.0))),
            central: Rect::from_min_max(pos2(200.0, 0.0), pos2(1000.0, 600.0)),
        }
    }

    #[test]
    fn a_pointer_over_the_sidebar_routes_to_the_sidebar() {
        let cfg = DropConfig::default();
        assert_eq!(
            route(Some(pos2(100.0, 300.0)), &regions(), false, &cfg),
            Some(Target::ProjectsSidebar)
        );
    }

    #[test]
    fn a_pointer_over_the_central_panel_routes_to_the_terminal() {
        let cfg = DropConfig::default();
        assert_eq!(
            route(Some(pos2(600.0, 300.0)), &regions(), false, &cfg),
            Some(Target::Terminal)
        );
    }

    #[test]
    fn the_central_panel_routes_to_the_scratchpad_when_that_tab_is_active() {
        let cfg = DropConfig::default();
        assert_eq!(
            route(Some(pos2(600.0, 300.0)), &regions(), true, &cfg),
            Some(Target::Scratchpad)
        );
    }

    /// No cursor position is available off Windows, and the central panel is
    /// the only sane assumption: the terminal is what every other terminal
    /// emulator does with a drop.
    #[test]
    fn an_unknown_pointer_falls_back_to_the_central_panel() {
        let cfg = DropConfig::default();
        assert_eq!(route(None, &regions(), false, &cfg), Some(Target::Terminal));
        assert_eq!(route(None, &regions(), true, &cfg), Some(Target::Scratchpad));
    }

    /// A hidden sidebar, or `[ui.drop] sidebar = false`, is passed as `None`.
    #[test]
    fn a_missing_sidebar_region_can_never_be_hit() {
        let cfg = DropConfig::default();
        let regions = Regions { sidebar: None, ..regions() };
        assert_eq!(route(Some(pos2(100.0, 300.0)), &regions, false, &cfg), None);
    }

    /// A hidden sidebar and `[ui.drop] sidebar = false` are the same thing to
    /// `route`: no rectangle, so no hit.
    #[test]
    fn regions_drop_the_sidebar_when_it_is_hidden_or_disabled() {
        let visible = Rect::from_min_max(pos2(0.0, 0.0), pos2(200.0, 600.0));
        let central = Rect::from_min_max(pos2(200.0, 0.0), pos2(1000.0, 600.0));
        let on = DropConfig::default();
        let off = DropConfig { sidebar: false, ..DropConfig::default() };

        assert!(Regions::new(Some(visible), central, &on).sidebar.is_some());
        assert!(Regions::new(None, central, &on).sidebar.is_none());
        assert!(Regions::new(Some(visible), central, &off).sidebar.is_none());
    }

    #[test]
    fn a_pointer_outside_every_region_routes_nowhere() {
        let cfg = DropConfig::default();
        assert_eq!(route(Some(pos2(5000.0, 5000.0)), &regions(), false, &cfg), None);
    }

    #[test]
    fn each_target_can_be_switched_off_on_its_own() {
        let no_terminal = DropConfig { terminal: false, ..DropConfig::default() };
        assert_eq!(route(Some(pos2(600.0, 300.0)), &regions(), false, &no_terminal), None);

        let no_scratchpad = DropConfig { scratchpad: false, ..DropConfig::default() };
        assert_eq!(route(Some(pos2(600.0, 300.0)), &regions(), true, &no_scratchpad), None);
    }

    #[test]
    fn the_master_switch_disables_every_target() {
        let off = DropConfig { enabled: false, ..DropConfig::default() };
        assert_eq!(route(Some(pos2(100.0, 300.0)), &regions(), false, &off), None);
        assert_eq!(route(Some(pos2(600.0, 300.0)), &regions(), false, &off), None);
        assert_eq!(route(None, &regions(), true, &off), None);
    }

    #[test]
    fn a_shell_payload_joins_paths_with_spaces_and_ends_with_one() {
        let cfg =
            DropConfig { quote: Quoting::None, wsl_translate: false, ..DropConfig::default() };
        let paths = [PathBuf::from("/a/one.png"), PathBuf::from("/a/two.png")];
        assert_eq!(shell_payload(&paths, None, &cfg), "/a/one.png /a/two.png ");
    }

    #[test]
    fn a_shell_payload_quotes_a_path_containing_spaces() {
        let cfg =
            DropConfig { quote: Quoting::Posix, wsl_translate: false, ..DropConfig::default() };
        let paths = [PathBuf::from("/a/my pic.png")];
        assert_eq!(shell_payload(&paths, None, &cfg), "'/a/my pic.png' ");
    }

    #[test]
    fn an_empty_drop_produces_no_payload() {
        let cfg = DropConfig::default();
        assert_eq!(shell_payload(&[], None, &cfg), "");
    }

    /// `\x0f` is readline's `operate-and-get-next`, which accepts the line as
    /// if Enter had been pressed — the byte submits a command without a
    /// carriage return anywhere in the payload.
    #[test]
    fn a_path_carrying_any_control_character_is_not_safe() {
        assert!(is_terminal_safe("/a/ordinary name.png"));
        assert!(!is_terminal_safe("/a/two\nlines.png"));
        assert!(!is_terminal_safe("/a/carriage\rreturn.png"));
        assert!(!is_terminal_safe("/a/escape\x1b[0m.png"));
        assert!(!is_terminal_safe("/a/interrupt\x03.png"));
        assert!(!is_terminal_safe("/a/accept\x0frm -rf ~"));

        for c in ('\0'..='\u{9f}').filter(|c| c.is_control()) {
            assert!(!is_terminal_safe(&format!("/a/name{c}.png")), "{c:?} passed the filter");
        }
    }

    /// `paste::paste_bytes`'s unbracketed branch rewrites `\n` to `\r`, and
    /// `\r` is Enter — so a filename containing a newline would run whatever
    /// follows it without the user touching the keyboard.  Quoting does not
    /// help: `shlex` wraps the newline inside single quotes and the rewrite
    /// still fires.
    #[test]
    fn an_unsafe_path_is_dropped_and_the_rest_of_the_batch_survives() {
        let cfg =
            DropConfig { quote: Quoting::None, wsl_translate: false, ..DropConfig::default() };
        let paths = [
            PathBuf::from("/a/one.png"),
            PathBuf::from("/a/evil\nrm -rf ~"),
            PathBuf::from("/a/two.png"),
        ];
        assert_eq!(shell_payload(&paths, None, &cfg), "/a/one.png /a/two.png ");
    }

    /// The guard in `app.rs` skips the paste on an empty payload, so a batch
    /// where nothing survives the filter must produce exactly that.
    #[test]
    fn a_batch_of_only_unsafe_paths_produces_no_payload() {
        let cfg =
            DropConfig { quote: Quoting::None, wsl_translate: false, ..DropConfig::default() };
        let paths = [PathBuf::from("/a/evil\nrm -rf ~"), PathBuf::from("/a/accept\x0frm -rf ~")];
        assert_eq!(shell_payload(&paths, None, &cfg), "");
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_path_becomes_a_distro_path_for_a_wsl_shell() {
        let cfg = DropConfig::default();
        let paths = [PathBuf::from(r"C:\pics\a.png")];
        assert_eq!(shell_payload(&paths, Some("Ubuntu"), &cfg), "/mnt/c/pics/a.png ");
    }

    /// Translation forces POSIX rules even under `quote = "windows"`: the
    /// string is a word for a Linux shell once it has been rewritten.
    #[cfg(windows)]
    #[test]
    fn a_translated_path_is_quoted_posix_style_whatever_the_mode_says() {
        let cfg = DropConfig { quote: Quoting::Windows, ..DropConfig::default() };
        let paths = [PathBuf::from(r"C:\pics\my pic.png")];
        assert_eq!(shell_payload(&paths, Some("Ubuntu"), &cfg), "'/mnt/c/pics/my pic.png' ");
    }

    #[cfg(windows)]
    #[test]
    fn translation_off_leaves_the_windows_path_alone() {
        let cfg =
            DropConfig { wsl_translate: false, quote: Quoting::None, ..DropConfig::default() };
        let paths = [PathBuf::from(r"C:\pics\a.png")];
        assert_eq!(shell_payload(&paths, Some("Ubuntu"), &cfg), "C:\\pics\\a.png ");
    }

    /// `auto` follows the receiving shell, not the path: a WSL session gets
    /// POSIX quoting even where translation is off and the word being quoted is
    /// still a `C:\` path, whose separators the quoting then escapes so the
    /// shell hands the backslashes on intact.
    #[cfg(windows)]
    #[test]
    fn an_untranslated_path_for_a_wsl_shell_is_still_quoted_posix_style() {
        let cfg =
            DropConfig { wsl_translate: false, quote: Quoting::Auto, ..DropConfig::default() };
        let paths = [PathBuf::from(r"C:\pics\my pic.png")];
        assert_eq!(shell_payload(&paths, Some("Ubuntu"), &cfg), r#""C:\\pics\\my pic.png" "#);
    }

    /// A plain UNC share has no distro-side spelling.  Pasting the raw path is
    /// more useful than pasting nothing.
    #[cfg(windows)]
    #[test]
    fn an_untranslatable_path_falls_back_to_the_raw_spelling() {
        let cfg = DropConfig { quote: Quoting::None, ..DropConfig::default() };
        let paths = [PathBuf::from(r"\\fileserver\share\a.png")];
        assert_eq!(shell_payload(&paths, Some("Ubuntu"), &cfg), "\\\\fileserver\\share\\a.png ");
    }

    /// The distro is compared the way Windows compares the UNC share name.
    #[cfg(windows)]
    #[test]
    fn a_unc_path_for_this_distro_strips_to_its_linux_form() {
        let cfg = DropConfig { quote: Quoting::None, ..DropConfig::default() };
        let paths = [PathBuf::from(r"\\wsl.localhost\Ubuntu\home\lev\a.png")];
        assert_eq!(shell_payload(&paths, Some("ubuntu"), &cfg), "/home/lev/a.png ");
    }

    /// `windows_to_linux` throws the distro away (`wsl.rs:141`), so stripping a
    /// UNC prefix that belongs to another distro would name a different file
    /// with no error.  Refuse it: an unresolvable Windows path fails loudly.
    #[cfg(windows)]
    #[test]
    fn a_unc_path_for_another_distro_is_never_stripped() {
        let cfg = DropConfig { quote: Quoting::None, ..DropConfig::default() };
        let paths = [PathBuf::from(r"\\wsl.localhost\Ubuntu\home\lev\a.png")];
        assert_eq!(
            shell_payload(&paths, Some("kali-linux"), &cfg),
            "\\\\wsl.localhost\\Ubuntu\\home\\lev\\a.png "
        );
    }

    #[test]
    fn a_document_payload_is_unquoted_and_newline_separated() {
        let paths = [PathBuf::from("/a/my pic.png"), PathBuf::from("/a/two.png")];
        assert_eq!(document_payload(&paths, None, None), "/a/my pic.png\n/a/two.png");
    }

    /// Dropping mid-line must not weld the first path onto the text before the
    /// cursor, nor the last onto the text after it.
    #[test]
    fn a_document_payload_adds_the_newlines_its_neighbours_need() {
        let paths = [PathBuf::from("/a/one.png")];
        assert_eq!(document_payload(&paths, Some('c'), Some('d')), "\n/a/one.png\n");
    }

    #[test]
    fn a_document_payload_adds_no_newline_where_there_already_is_one() {
        let paths = [PathBuf::from("/a/one.png")];
        assert_eq!(document_payload(&paths, Some('\n'), Some('\n')), "/a/one.png");
    }

    /// Each boundary is decided on its own, so a drop at the start of a
    /// non-empty line needs a newline after it and none before.
    #[test]
    fn a_document_payload_treats_its_two_boundaries_separately() {
        let paths = [PathBuf::from("/a/one.png")];
        assert_eq!(document_payload(&paths, None, Some('d')), "/a/one.png\n");
        assert_eq!(document_payload(&paths, Some('c'), None), "\n/a/one.png");
    }

    #[test]
    fn project_roots_keeps_a_directory_and_lifts_a_file_to_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();

        let roots = project_roots(&[dir.path().to_path_buf(), file]);

        assert_eq!(roots, vec![dir.path().to_path_buf()]);
    }

    /// Selecting several files in one folder is the ordinary way to drag, and
    /// every one of them names the same root.
    #[test]
    fn project_roots_collapses_repeats_in_first_seen_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for dir in [&first, &second] {
            std::fs::write(dir.path().join("a.txt"), "x").unwrap();
            std::fs::write(dir.path().join("b.txt"), "x").unwrap();
        }

        let roots = project_roots(&[
            second.path().join("a.txt"),
            first.path().join("a.txt"),
            second.path().join("b.txt"),
            first.path().join("b.txt"),
        ]);

        assert_eq!(roots, vec![second.path().to_path_buf(), first.path().to_path_buf()]);
    }

    /// Derived from a real temporary directory rather than a hardcoded
    /// absolute path: a literal is a guess about the host, and a Unix-shaped
    /// one is a guess about the platform too.
    #[test]
    fn project_roots_skips_a_path_that_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");

        assert!(project_roots(&[missing]).is_empty());
    }

    /// The seam, not either half.  `shell_payload` cannot see what `paste.rs`
    /// does to its output and `paste.rs` cannot see where the text came from,
    /// so a filename carrying a line-submitting byte is inert only if the two
    /// agree.  This is the assertion that fails if the control-character filter
    /// is ever removed as redundant with quoting — `paste::paste_bytes`'s
    /// unbracketed branch turns `\n` into `\r`, and readline accepts the line
    /// on `\x0f` with no `\r` involved at all.  It stops at the bytes `paste`
    /// would write — reaching the PTY needs a spawned child.
    ///
    /// Run under `Auto` as well as `None` because `Auto` is what ships.
    #[test]
    fn a_payload_reaching_an_unbracketed_paste_submits_nothing() {
        let paths = [
            PathBuf::from("/a/evil\nrm -rf ~"),
            PathBuf::from("/a/accept\x0frm -rf ~"),
            PathBuf::from("/a/ok.png"),
        ];

        for quote in [Quoting::None, Quoting::Auto] {
            let cfg = DropConfig { quote, wsl_translate: false, ..DropConfig::default() };

            let on_the_pty =
                crate::paste::paste_bytes(&shell_payload(&paths, None, &cfg), true, false);

            assert!(
                !on_the_pty.contains(&b'\r'),
                "{quote:?}: {on_the_pty:?} would press Enter on its own"
            );
            assert!(
                !on_the_pty.contains(&0x0f),
                "{quote:?}: {on_the_pty:?} would accept the line through operate-and-get-next"
            );
            assert_eq!(on_the_pty, b"/a/ok.png ".to_vec(), "{quote:?}");
        }
    }

    /// `GetCursorPos` reports physical screen pixels while egui works in
    /// logical points measured from the window's content origin.
    #[test]
    fn a_screen_pixel_converts_to_a_window_relative_egui_point() {
        let origin = pos2(100.0, 50.0);
        assert_eq!(to_egui_pos(300.0, 250.0, origin, 1.0), pos2(200.0, 200.0));
        assert_eq!(to_egui_pos(600.0, 500.0, origin, 2.0), pos2(200.0, 200.0));
    }

    /// A monitor left of or above the primary one has negative coordinates, and
    /// a window on it has a negative origin — the ordinary multi-display
    /// layout, not an edge case.
    #[test]
    fn a_window_on_a_monitor_left_of_the_primary_converts_the_same_way() {
        let origin = pos2(-1920.0, -200.0);
        assert_eq!(to_egui_pos(-1720.0, -100.0, origin, 1.0), pos2(200.0, 100.0));
        assert_eq!(to_egui_pos(-3840.0, -400.0, origin, 2.0), pos2(0.0, 0.0));
    }
}
