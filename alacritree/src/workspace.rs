//! The key identifying which workspace (home, or a worktree) a session or
//! sidebar row belongs to.

use std::path::PathBuf;

/// `None` is the home workspace (sessions inherit `$PWD`); `Some` is a worktree path.
pub(crate) type WorkspaceKey = Option<PathBuf>;
