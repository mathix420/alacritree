//! Restricting Windows' DLL search order so a pseudoconsole loads its own
//! `conpty.dll` instead of one from another terminal's install directory.

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
///
/// The first `LoadLibraryW` decides which module answers every later one, so
/// this has to run before the first pseudoconsole opens.  `main` does it at
/// startup and every pseudoconsole open repeats it, because a test binary has
/// no `main` to do it for them.
#[cfg(windows)]
pub fn harden_dll_search_path() {
    use std::sync::Once;

    use windows_sys::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
    };

    static HARDENED: Once = Once::new();

    HARDENED.call_once(|| {
        // Failure only leaves the default search order in place, which is what
        // we had before, so it is not worth refusing to start over.
        if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) } == 0 {
            log::warn!(
                "failed to restrict the DLL search path: {}",
                std::io::Error::last_os_error()
            );
        }
    });
}

#[cfg(not(windows))]
pub fn harden_dll_search_path() {}
