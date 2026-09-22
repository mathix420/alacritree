//! The module tree behind the `alacritree` binary.
//!
//! A library target lets integration tests under `tests/` name crate types
//! directly instead of spawning the binary, and gives the allocation gate a
//! test binary of its own, where a `#[global_allocator]` affects nothing else.

#![warn(unreachable_pub)]

pub mod alloc_count;
pub mod app;
pub(crate) mod bindings;
pub(crate) mod builtin_font;
pub mod cli;
pub(crate) mod clipboard;
pub(crate) mod clipboard_image;
pub(crate) mod color_glyph;
pub(crate) mod colors;
pub mod command_ext;
pub(crate) mod command_palette;
pub mod config;
pub mod crash_log;
pub(crate) mod cursor;
pub(crate) mod decoration_sprites;
pub mod default_branch;
pub(crate) mod diff_viewer;
pub(crate) mod digest;
pub mod dll_search;
pub(crate) mod doppler;
pub(crate) mod file_drop;
pub(crate) mod focus_priority;
pub(crate) mod fonts;
pub mod frame_log;
pub(crate) mod git_nav;
pub(crate) mod git_status;
pub(crate) mod glyph_cache;
pub(crate) mod gpu_timing;
pub(crate) mod grid_gl;
pub(crate) mod grid_instances;
pub mod herdr;
pub(crate) mod ime;
pub mod in_flight;
pub(crate) mod input;
pub(crate) mod ipc;
pub(crate) mod jobs;
pub(crate) mod links;
pub mod logdir;
pub mod logging;
pub(crate) mod mcp;
pub(crate) mod modal_gate;
pub(crate) mod mouse;
pub(crate) mod mouse_hide;
pub mod multiplexer;
pub(crate) mod notify;
pub(crate) mod panel_filter;
pub(crate) mod paste;
pub(crate) mod path_style;
pub(crate) mod pr_query;
pub(crate) mod pr_status;
pub(crate) mod process_probe;
pub mod projects;
#[cfg(windows)]
pub(crate) mod pty_rearm;
pub(crate) mod repaint;
pub(crate) mod row_label;
pub(crate) mod scratchpad;
pub(crate) mod session;
pub(crate) mod shell_decision;
pub(crate) mod shortcut;
pub mod sidebar_focus;
pub(crate) mod sidebar_nav;
pub(crate) mod stale_exe;
pub mod startup_log;
pub mod state;
pub(crate) mod tasks;
pub(crate) mod terminal_view;
#[cfg(test)]
mod test_util;
pub mod tools;
pub(crate) mod upstream;
#[cfg(windows)]
pub mod win_session;
pub(crate) mod workspace;
pub(crate) mod worktree;
pub(crate) mod worktree_liveness;
pub mod wsl;
pub mod wsl_helper;
pub mod wsl_spare;
pub mod zellij;

/// The timing reports inside unit tests read allocation counts too.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: alloc_count::CountingAllocator = alloc_count::CountingAllocator;
