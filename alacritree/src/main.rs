#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bindings;
mod builtin_font;
mod cli;
mod clipboard;
mod color_glyph;
mod colors;
mod command_ext;
mod config;
mod doppler;
mod fonts;
mod git_nav;
mod git_status;
mod ime;
mod input;
mod ipc;
mod links;
mod mcp;
mod mouse;
mod panel_filter;
mod paste;
mod pr_status;
mod projects;
mod session;
mod sidebar_nav;
mod stale_exe;
mod state;
mod terminal_view;
#[cfg(test)]
mod test_util;
mod worktree;
mod wsl;

use app::AlacritreeApp;
use clap::Parser;

/// Pre-resized from the 2048x2048 source so we don't embed a 4 MB blob for
/// what egui only needs at ~256x256.
const WINDOW_ICON: &[u8] = include_bytes!("../assets/icon-256.png");

/// Drop PATH and the working directory from the DLL search order, leaving the
/// executable's own directory plus the system directories.
///
/// `alacritty_terminal` opens the pseudoconsole by `LoadLibraryW("conpty.dll")`
/// so a build of OpenConsole shipped alongside the binary can be preferred over
/// the one in Windows.  Windows has no `conpty.dll` of its own — the API lives
/// in `kernel32` — so that bare name matches nothing until some *other* app's
/// install directory is on PATH, at which point every PTY is hosted in a foreign
/// terminal's console server.  WezTerm's blocks the child process for three
/// seconds waiting on a device-attributes reply, which shows up as a multi-second
/// stall opening any pane.
#[cfg(windows)]
fn harden_dll_search_path() {
    use windows_sys::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
    };

    // Failure only leaves the default search order in place, which is what we
    // had before, so it is not worth refusing to start over.
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) } == 0 {
        log::warn!("failed to restrict the DLL search path: {}", std::io::Error::last_os_error());
    }
}

#[cfg(not(windows))]
fn harden_dll_search_path() {}

fn main() -> eframe::Result<()> {
    harden_dll_search_path();

    // egui_winit warns on every cold X11 clipboard probe even when it recovers.
    let default_filter = "info,egui_winit::clipboard=error";
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .init();

    // A subcommand talks to an alacritree instead of being one.  Log output
    // goes to stderr (env_logger's default), leaving stdout to the reply.
    attach_parent_console();
    if let Some(code) = cli::run(cli::Cli::parse()) {
        std::process::exit(code);
    }

    let config = config::load();
    wsl::set_automount_root(config.wsl_automount_root.clone());
    let translucent = config.window.opacity < 1.0;

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([640.0, 400.0])
        .with_title("Alacritree")
        .with_transparent(translucent);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "Alacritree",
        native_options,
        Box::new(move |cc| Ok(Box::new(AlacritreeApp::new(cc, config)))),
    )
}

/// Borrow the console of whatever shell launched us, if there is one.
///
/// A `windows_subsystem = "windows"` binary starts with no console attached, so
/// in a release build `println!` writes to a handle that goes nowhere and the
/// CLI is silent at a prompt.  (A debug build has a console and looks fine,
/// which is how this hides.)  Redirected output is unaffected either way —
/// `GetStdHandle` returns the pipe — so only the interactive case needs this.
///
/// Must run before anything touches `std::io::stdout()`, which caches the
/// handle it first sees.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

    // Fails when the parent has no console (launched from a GUI shell), which
    // is exactly when there is nothing to attach to and nothing to report.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_parent_console() {}

/// A bad icon is cosmetic — log and fall back to the OS default rather than
/// refusing to start.
fn load_window_icon() -> Option<egui::IconData> {
    let decoder = png::Decoder::new(std::io::Cursor::new(WINDOW_ICON));
    let mut reader = match decoder.read_info() {
        Ok(reader) => reader,
        Err(err) => {
            log::warn!("failed to read window icon header: {err}");
            return None;
        },
    };
    let mut rgba = vec![0; reader.output_buffer_size()];
    let info = match reader.next_frame(&mut rgba) {
        Ok(info) => info,
        Err(err) => {
            log::warn!("failed to decode window icon: {err}");
            return None;
        },
    };
    rgba.truncate(info.buffer_size());
    Some(egui::IconData { rgba, width: info.width, height: info.height })
}
