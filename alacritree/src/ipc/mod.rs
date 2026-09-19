//! IPC socket for driving alacritree from outside the process (`alacritree
//! mcp` and anything else that speaks the protocol).
//!
//! Follows alacritty's `polling/ipc.rs`: a local socket advertised through the
//! `ALACRITREE_SOCKET` environment variable (so processes running *inside* an
//! alacritree session find their own instance), one newline-delimited JSON
//! request per connection, one JSON reply line back.  Unlike alacritty we need
//! replies with data, so every request gets a `{"ok": …}` / `{"error": …}`
//! response instead of fire-and-forget.
//!
//! The transport is a unix domain socket under `$XDG_RUNTIME_DIR/alacritree`
//! on unix and a named pipe under `\\.\pipe\` on Windows — `interprocess`
//! addresses both as a path, so the two differ only in where the path points.
//! Alacritty's IPC is unix-only, but nothing above the transport is, and the
//! MCP bridge is worth as much on Windows.

pub(crate) mod protocol;
pub(crate) mod route;
pub(crate) mod server;
