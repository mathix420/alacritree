//! alacritree is a GUI-subsystem binary with no console of its own, so on
//! Windows each `git`/`gh`/`cmd` child gets a fresh console window unless we
//! pass `CREATE_NO_WINDOW`. `hidden` is the crate's one sanctioned way to
//! build a `Command`, so that flag can never be forgotten at a call site.

use std::ffi::OsStr;
use std::process::Command;

/// The flag that suppresses the console window.  Public because creation flags
/// are one field: a caller that names another flag has to pass this one too
/// rather than calling [`CommandExt::hide_console`].
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub trait CommandExt {
    /// Suppress the console window Windows would spawn for this child. No-op
    /// elsewhere.
    fn hide_console(&mut self) -> &mut Self;
}

impl CommandExt for Command {
    #[cfg(windows)]
    fn hide_console(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt as _;
        self.creation_flags(CREATE_NO_WINDOW)
    }

    #[cfg(not(windows))]
    fn hide_console(&mut self) -> &mut Self {
        self
    }
}

/// Build a `Command` for `program`, pre-armed to skip the console window
/// Windows would otherwise pop for it. No-op elsewhere.
#[allow(clippy::disallowed_methods)] // the sanctioned spawner
pub fn hidden(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.hide_console();
    cmd
}
