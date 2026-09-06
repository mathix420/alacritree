# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.
`CLAUDE.md` imports it so Claude Code picks it up too.

## Repository layout

This is a Cargo workspace. There are five crates, but only one is original work:

- `alacritree/` — **the only crate this fork actually changes.** A small egui/eframe app that hosts `alacritty_terminal` and adds a worktree-aware sidebar. All agent-edited code should live here unless the user explicitly says otherwise.
- `alacritty/`, `alacritty_terminal/`, `alacritty_config/`, `alacritty_config_derive/` — vendored upstream alacritty. Treat as read-only dependencies. The `alacritty` GUI binary (winit/OpenGL) is **not** what this fork ships; we only use `alacritty_terminal` (the headless PTY + VT parser + grid).

`egui-winit/` sits alongside them but is not a workspace member — it is a vendored `egui-winit` carrying a one-line change, wired in through `[patch.crates-io]` in the root `Cargo.toml` so Ctrl+V falls through to a key event when the clipboard holds something other than text.

`CONTRIBUTING.md` is the upstream alacritty contributing guide, kept for the vendored crates' historical context. It does not constrain work on `alacritree/`.

## Build / run

```sh
cargo run -p alacritree            # debug build of the GUI
cargo build -p alacritree --release
cargo check -p alacritree          # fast type-check loop
cargo fmt                          # rustfmt is enforced (see rustfmt.toml)
cargo test -p alacritree           # unit tests live in-module under #[cfg(test)]
```

The workspace MSRV is 1.85 (edition 2024). The root `Makefile` is upstream alacritty's macOS bundling script; it is **not** wired up to alacritree.

There is a `[patch.crates-io]` pin on `x11-clipboard` in the root `Cargo.toml` (TODO from upstream) — leave it alone unless asked.

## Big-picture architecture

`alacritree` is an egui app that owns N PTY-backed terminal sessions and routes input/paint through a custom grid renderer. The pieces:

- `main.rs` — `eframe::run_native`, env_logger setup. Window opacity comes from config; transparency is a `ViewportBuilder` flag, so toggling it requires restart. Running with a subcommand hands off to `cli/` instead of opening a window.
- `cli/` — the clap CLI (`mcp`, `project`, `session`, `workspace`, `git-status`, `worktree`, `action`, `doctor`, `install`, `schema`, `completions`). The operational project/session/workspace/git/worktree/action commands map to `IpcRequest`, the same enum the MCP bridge speaks; `mcp`, `doctor`, `install`, `schema`, and `completions` are local or special-purpose commands that return before IPC dispatch. Dispatch is hybrid: a request goes to a running instance when one is listening, and otherwise to `cli/offline.rs`, which serves what it can from `state.toml` and git directly — commands that are meaningless without a window fail there rather than pretending. `cli/render.rs` turns replies into human-readable output; `--json` prints the raw reply instead.
- `ipc.rs` — local-socket IPC mirroring alacritty's `polling/ipc.rs`: on Unix, a socket under `$XDG_RUNTIME_DIR/alacritree` (or `/run/user/$UID/alacritree` on Linux when the environment variable is absent), falling back to the system temporary directory when the runtime path cannot be created; on Windows, a named pipe at `\\.\pipe\alacritree-<pid>.sock` (`interprocess` addresses both as a path). Advertised via `ALACRITREE_SOCKET`, one newline-delimited JSON request per connection with an `{"ok"}/{"error"}` reply. A client with no env var finds an instance by listing the socket directory — on Windows the pipe filesystem is itself listable. Requests that touch app state are forwarded to the UI thread as `AppCall`s (drained in `update`, woken by `request_repaint`); slow ones (git status, worktree creation) run on the connection thread. Disabled via `[general] ipc_socket = false`. Named pipes have no receive timeout, so the client bounds each request from its own side (worker thread + `recv_timeout`) rather than with `set_recv_timeout`.
- `mcp.rs` — `alacritree mcp`: a hand-rolled stdio MCP server (newline-delimited JSON-RPC, tools only) whose tool names/arguments map 1:1 onto `ipc::IpcRequest` serde tags. Deliberately SDK-free to keep the crate synchronous. Platform-agnostic — it only speaks stdio and `ipc::send_request`.
- `app.rs` — `AlacritreeApp` is the `eframe::App`. Owns `Vec<Session>`, the project list, the per-workspace active-session map, and the cached `Theme`. **Workspace model:** a `WorkspaceKey = Option<PathBuf>` — `None` is the "home" tab (sessions inherit `$PWD`), `Some(path)` is a worktree. The active session for a workspace persists across switches; sessions are *not* killed when you switch away. Sidebars: left = projects/worktrees, right = git status. Both are toggleable and persisted. Cursor repair for the left sidebar is reconciled once per frame in `sidebar_focus.rs` by diffing a snapshot of the tree, rather than by each mutation site reporting what it removed. The reconcile runs unconditionally, so its unchanged-frame path must stay allocation-free — `steady_state.rs` asserts that.
- `session.rs` — wraps `alacritty_terminal::event_loop::EventLoop`. Each `Session` has its own PTY, its own background read/write thread, and its own monotonic `window_id` (alacritty routes OSC 7 / signal events by id, so ids must be unique). `EventProxy` bridges terminal events into an `mpsc` + `egui::Context::request_repaint`. `Drop` sends `Msg::Shutdown` — don't bypass this.
- `terminal_view.rs` — the custom grid painter. Computes cell size from the egui font, resizes the session to fit, drains pending PTY events (`Title`, `ChildExit`, `PtyWrite`), and captures the grid into runs of one style. Those runs go to one of two painters: **the mesh path**, which builds a shape per glyph for epaint to tessellate, or **the GL path** under `[ui] gpu_grid`. Input goes through `input::event_to_bytes`.
- `grid_gl.rs`, `grid_instances.rs`, `decoration_sprites.rs`, `gpu_timing.rs` — the GL path, reached through an `egui::PaintCallback` when `[ui] gpu_grid` is set. Default off, and a context that cannot build it falls back to the mesh path for the session. `grid_instances.rs` holds one record per cell on the CPU side, `grid_gl.rs` uploads the dirty rows and draws them, `decoration_sprites.rs` rasterizes the underline styles, `[debug] gpu_timing` times each draw on the GPU.
- `input.rs` — translates `egui::Event` → terminal byte sequences (CSI/SS3 for arrows/F-keys, `ESC + key` for Alt, control bytes for Ctrl-letter). `Event::Text` is preferred for printable input because it handles dead keys / IME.
- `bindings.rs` — parses alacritty's `[[keyboard.bindings]]` TOML into egui `KeyboardShortcut`s. Vi/search-mode bindings are dropped (no mode tracking). `BindingAction::Chars` writes raw bytes; `Named` triggers app-level actions (paste, scroll, font-size, quit, …).
- `config.rs` — loads `alacritty.toml` then deep-merges `alacritree.toml` over it using **alacritty's merge semantics**: arrays *concatenate* (so `[[keyboard.bindings]]` in alacritree.toml *adds to* upstream bindings), tables merge recursively, primitives replace. Search path mirrors alacritty: `$XDG_CONFIG_HOME/alacritty/`, `~/.config/alacritty/`, `~/.alacritty.toml`, `/etc/alacritty/`. alacritree-only options live under `[ui]` (sidebar colors, etc.) and `[workspace]` (worktree location).
- `colors.rs` — converts alacritty's `Rgb` + `AnsiColor` (Named/Spec/Indexed) to `egui::Color32`, applying the 256-color palette and bright/dim variants.
- `fonts.rs` — loads a system monospace font via `fontdb` and registers it with egui.
- `projects.rs` — `Project::discover(path)` opens with `git2`, lists worktrees via `repo.worktrees()`, and detects the default branch (config `init.defaultBranch` → `refs/remotes/origin/HEAD` → fallback to `main`/`master`). Non-git roots get a single pseudo-worktree pointing at themselves so the user can still spawn a shell there.
- `git_status.rs` — `StatusCache` per worktree, throttled to 1.5 s. Computes staged/unstaged file lists and a diff-stat against the project's default branch for the right sidebar.
- `state.rs` — minimal persistence to `$XDG_CONFIG_HOME/alacritree/state.toml`: project roots, expanded state, sidebar visibility, per-worktree base branches. Serialized with `toml`. Failures are logged and ignored — never panic on missing/corrupt state.
- `logdir.rs` — where diagnostics live (`%LOCALAPPDATA%` / `$XDG_STATE_HOME`, deliberately not the roaming config dir) plus the per-process identity — UTC epoch nanos + pid + retry ordinal — that names both the crash artifact and the continuous log, and the per-platform "is this pid alive" check pruning depends on.
- `crash_log.rs` — the panic hook. Writes one artifact per GUI process; single writer, never shared, so no cross-process protocol. Armed only on the GUI path (after `cli::run` declines) because subcommands exit before config loads and no gate could govern them. Uses `try_lock`, never `lock`: a thread panicking while holding the recorder mutex would otherwise wait on itself forever. Retention is by age and liveness only — contents never decide deletion. Gated by `[debug] crash_log`, default on.
- `logging.rs` — `Tee`, which mirrors env_logger's stream into a per-process file whose sink is filled after config loads (env_logger cannot be retargeted post-`init`). Gated by `[debug] persistent_logging`, default off.
- `pr_status.rs` — shells out to `gh` to find the open PR for a branch and cache its base, so the git panel diffs against the PR's base instead of the repo's default branch. Best-effort: missing or unauthenticated `gh` silently falls back.
- `command_palette.rs` — data model and fuzzy ranking for the Ctrl+K palette. `panel_filter.rs` holds the equivalent per-panel search state for the sidebars.
- `scratchpad.rs` — persistent per-workspace notes and their built-in editor. Closing the tab or deleting a worktree must never delete the notes.
- `wsl.rs` — the only module that knows WSL exists: distro enumeration, Windows ↔ Linux path translation, `wsl.exe` command construction. `wsl_helper.rs` keeps one long-lived `sh` per distro so batch scripts don't pay process startup per call.
- `clipboard.rs`, `paste.rs`, `links.rs`, `mouse.rs`, `ime.rs`, `file_drop.rs` — the input/interaction surface around the grid: the two clipboards, bracketed paste, link detection, mouse-report encodings mirroring alacritty's, IME composition state, and where a dropped file goes.
- `glyph_cache.rs`, `color_glyph.rs`, `builtin_font.rs` — the paint path's caches: reused single-character galleys, emoji rasterized from a font's colour tables, and hand-drawn box-drawing glyphs that must fully cover their cell.
- `sidebar_nav.rs`, `git_nav.rs`, `row_label.rs`, `path_style.rs` — pure models behind the sidebars (cursor movement, row templating, abbreviated paths), deliberately free of egui so they can be unit-tested.
- `command_ext.rs` — alacritree is a GUI-subsystem binary with no console, so every `git`/`gh`/`cmd` child needs a flag to avoid flashing a console window on Windows. Spawn children through this, not `Command` directly.

## Conventions specific to this fork

- Mirror upstream alacritty wherever possible. Before implementing input handling, config parsing, terminal behavior, key bindings, clipboard, scrolling, selection, or anything else that alacritty already solves, look at how `alacritty/` does it and follow the same approach. This fork swaps the renderer (egui instead of winit/OpenGL) but should otherwise behave like alacritty — divergence is a last resort, not the default, and should be justified in a comment when unavoidable.
- Two TOML files: `alacritty.toml` (shared with the alacritty terminal — palette, cursor, scrolling, shell, key bindings) and `alacritree.toml` (alacritree-only options under `[ui]` and `[workspace]`). When adding a config field, decide whether it belongs in the shared file or the alacritree-only file, and document it with a doc comment on the relevant `Raw*` struct in `config.rs` — those doc comments are the hover text the published JSON Schema carries. Regenerate the schema afterwards with `ALACRITREE_UPDATE_SCHEMA=1 cargo test -p alacritree --test config_schema`; the test fails the build while `schema/alacritree-config.json` is stale.
- Sessions outlive workspace switches. Don't introduce code that drops a `Session` just because it isn't visible.
- `EventProxy::send_event` calls `request_repaint` — this is what wakes the egui loop on PTY output. Anything that produces terminal events on a background thread must go through an `EventProxy` (or otherwise call `request_repaint`) or it will appear to hang until the next input event.
- Logs use the `log` crate. `egui_winit::clipboard=error` is filtered down by default in `main.rs` because cold X11 clipboard probes warn noisily; keep that filter unless you have a reason to remove it.
- Comments in `alacritree/` follow the "explain the *why*, not the *what*" pattern already in the file headers (e.g. `state.rs`, `config.rs`, `projects.rs`). Match that style — short, reason-giving, no rote restatements of the code.
- Always follow clean code practices: clear naming, small focused functions, no dead code, no premature abstractions. Never add useless comments (rote what-restatements, "added by X", task references), and never remove existing comments unless they are demonstrably wrong or made obsolete by the change you are making.
- Always use [Conventional Commits](https://www.conventionalcommits.org/) for commit messages (`feat:`, `fix:`, `refactor:`, `docs:`, `chore:`, etc., with an optional scope like `feat(sidebar):`). Keep the subject line imperative and under ~72 chars.
