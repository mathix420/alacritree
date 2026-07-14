#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bindings;
mod builtin_font;
mod clipboard;
mod colors;
mod command_ext;
mod config;
mod doppler;
mod fonts;
mod git_status;
mod ime;
mod input;
mod ipc;
mod links;
mod mcp;
mod mouse;
mod paste;
mod pr_status;
mod projects;
mod session;
mod sidebar_nav;
mod state;
mod terminal_view;
mod worktree;

use app::AlacritreeApp;

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

    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("mcp") {
        run_mcp_server(args);
    }

    let config = config::load();
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

/// `alacritree mcp [--socket <path>]`: run as an MCP stdio server bridging to
/// a running instance's IPC socket instead of opening a window.  Log output
/// goes to stderr (env_logger's default), leaving stdout to the protocol.
fn run_mcp_server(mut args: impl Iterator<Item = String>) -> ! {
    let mut socket = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(std::path::PathBuf::from),
            other => {
                eprintln!("unknown argument to `alacritree mcp`: {other}");
                std::process::exit(2);
            },
        }
    }
    mcp::run(socket);
    std::process::exit(0);
}

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
