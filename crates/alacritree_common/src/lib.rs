//! What every alacritree crate that runs an external program needs: a child
//! built without a console window, a pool that keeps the wait off the UI
//! thread, the WSL side of a Windows host, and where each tool lives.

pub mod command_ext;
pub mod jobs;
pub mod side;
pub mod tools;
pub mod wsl;
pub mod wsl_helper;
