<p align="center">
    <img width="200" alt="Alacritree Logo" src="alacritree/assets/icon.png">
</p>

<h1 align="center">Alacritree</h1>

<p align="center">
    A native terminal that turns Git worktrees into first-class workspaces, built on <a href="https://github.com/alacritty/alacritty">Alacritty</a>.
</p>

## About

The first ultrafast, FOSS alternative to the LLM/worktree management apps cropping up everywhere. Built around the amazing [Alacritty] terminal emulator and drop-in compatible with your `alacritty.toml`.

Minimalist approach, with the terminal at the center:

- **Worktree management.** Sidebar lists projects and worktrees; one click spawns a shell. Create fresh worktrees in seconds with pre-configured AI configs.
- **Git status bar.** Per-workspace panel with branch + staged/unstaged files, refreshed in the background.
- **Branch diffs.** Beautiful and meaningful diffs powered by [Delta].
- **Workspace scratchpads.** `Ctrl+Backtick` opens a minimal built-in Markdown
  editor that saves every change, with one file per worktree and MCP access.

No Chromium, no bundled agents, no telemetry. No company behind it, and there never will be.

[Alacritty]: https://github.com/alacritty/alacritty
[Delta]: https://github.com/dandavison/delta

## Screenshots

https://github.com/user-attachments/assets/c0b0aa23-59f1-49d3-a3aa-dcdf1eff7363

> **Status:** early, single-author project. Linux is the only platform
> with a working build today — the GUI deps currently target Linux, so the
> macOS/Windows entries in the install section below are scaffolded but
> not yet shipping binaries.

## Install

### Linux

**Arch (AUR)** — two flavours:

- `alacritree-bin` — prebuilt binary from the latest GitHub release, no
  Rust toolchain required, supports `x86_64` and `aarch64`.
- `alacritree-git` — VCS package that compiles the latest `master` locally.

```sh
yay -S alacritree-bin      # or `alacritree-git`
```

**Prebuilt tarball** — every tagged release publishes Linux tarballs
(`x86_64` and `aarch64`) at <https://github.com/mathix420/alacritree/releases>:

```sh
tag=v0.1.0   # pick the release you want
arch=x86_64  # or aarch64
curl -fLO "https://github.com/mathix420/alacritree/releases/download/${tag}/alacritree-${tag}-${arch}-linux.tar.gz"
tar -xzf "alacritree-${tag}-${arch}-linux.tar.gz"
install -Dm755 alacritree ~/.local/bin/alacritree
```

**From source** — see the [Build](#build) section.

### Windows (Scoop)

```powershell
scoop bucket add alacritree https://github.com/mathix420/alacritree
scoop install alacritree
```

The manifest lives in [`bucket/alacritree.json`](bucket/alacritree.json) and
is bumped automatically when a release is published. Windows binaries are
not produced yet (see *Status* above); the bucket is wired up and waiting
for the first cross-platform release.

### macOS (Homebrew, Apple Silicon)

```sh
brew tap mathix420/alacritree https://github.com/mathix420/alacritree
brew install alacritree
```

The formula lives in [`Formula/alacritree.rb`](Formula/alacritree.rb)
and is bumped automatically on every release. Only `aarch64-apple-darwin`
is shipped — Intel Macs need to build from source via the
[Build](#build) section (install `cmake`, `pkg-config`, `fontconfig`
and `freetype` through Homebrew first).

## Build

Workspace MSRV is **Rust 1.85** (edition 2024). System packages required on
Debian/Ubuntu:

```sh
sudo apt install \
    cmake pkg-config \
    libfreetype6-dev libfontconfig1-dev \
    libxkbcommon-dev libxcb-shape0-dev libxcb-xfixes0-dev \
    libwayland-dev libgl1-mesa-dev libegl1-mesa-dev
```

Then:

```sh
cargo run -p alacritree              # debug
cargo build -p alacritree --release  # release → target/release/alacritree
```

## Configuration

Alacritree reads the same files Alacritty does, in the same order:

1. `$XDG_CONFIG_HOME/alacritty/alacritty.toml`
2. `$XDG_CONFIG_HOME/alacritty.toml`
3. `$HOME/.config/alacritty/alacritty.toml`
4. `$HOME/.alacritty.toml`
5. `/etc/alacritty/alacritty.toml`

After loading `alacritty.toml`, Alacritree deep-merges an optional
`alacritree.toml` (same search path) on top. Merge semantics match
Alacritty's: arrays concatenate (so `[[keyboard.bindings]]` in
`alacritree.toml` *adds to* the upstream bindings rather than replacing
them), tables merge recursively, primitives replace.

Alacritree-only options live in `alacritree.toml`: `[ui]` for sidebar
colours, panel visibility, etc., and `[workspace]` for where new git
worktrees are created (`worktree_dir`, plus per-project
`[[workspace.overrides]]`). See `alacritree/src/config.rs` for the current
schema.

## MCP server

Alacritree can be driven by an LLM agent over the
[Model Context Protocol](https://modelcontextprotocol.io). `alacritree mcp`
starts a stdio MCP server that talks to the running app — an agent can list
your projects and worktrees, open shells in them, type into terminals, read
their output, inspect git status, and create worktrees. Register it with any
MCP client:

```sh
claude mcp add alacritree -- alacritree mcp
```

An agent running *inside* an Alacritree session automatically targets its host
instance (advertised via the `ALACRITREE_SOCKET` env var); other clients can
pass `alacritree mcp --socket <path>`. The transport mirrors Alacritty's IPC
design — disable it with `ipc_socket = false` under `[general]`. See
[`docs/alacritree.md`](docs/alacritree.md#mcp-server--drive-alacritree-from-an-llm)
for the full tool list.

## Command line

The same surface is available as a CLI, for agents that shell out rather than
speak MCP — and for setting Alacritree up without the folder picker:

```sh
alacritree project add ~/Git/myrepo     # also: list, remove, refresh
alacritree worktree create ~/Git/myrepo my-feature
alacritree git-status ~/Git/myrepo

alacritree session create --workspace ~/Git/myrepo   # prints the new session id
alacritree session send-text 3 'cargo test' --enter
alacritree session read-screen 3
```

`send-text` types the text; `--enter` submits it. (A shell passes arguments
through verbatim, so a trailing `\r` in the text would arrive as a backslash and
an `r`.)

Commands print a short human summary; `--json` prints the raw reply instead,
which is what a script or an agent wants.

Anything that needs a window — sessions, workspace selection — requires a
running Alacritree. The rest do not: with no instance listening, projects, git
status, and worktree creation are served straight from `state.toml` and git, so
an agent can set Alacritree up before anyone has opened it.

`alacritree completions <shell>` writes a completion script to stdout.

### Diagnosing a setup

```sh
alacritree doctor          # --json for the machine-readable form
```

Alacritree degrades quietly on purpose: a missing `gh` falls back to the repo's
default branch, a missing `doppler` skips scope mirroring, a malformed
`alacritty.toml` loads defaults, and a corrupt `state.toml` opens an empty
sidebar. Each is the right call on its own — none should stop a terminal from
opening — but together they mean a broken setup looks much like a working one.

`doctor` reports the external tools it found (with versions and paths), which
config files were loaded and whether they actually parse, whether the persisted
projects still exist on disk, and whether a running instance is reachable. It
needs no running window — "nothing happens when I run it" is exactly when it
gets used.

It exits non-zero only when something is genuinely broken. A missing optional
tool is a warning, and a tool driving a feature you have never opted into (an
absent `doppler` on a machine with no Doppler config) is not even that — a report
that always carries a warning is a report nobody reads.

## Documentation

- [`docs/alacritree.md`](docs/alacritree.md) — full feature reference for the
  fork: workspaces and sessions, the project/worktree sidebar (create/delete
  flows, AI-config copy, branch validation), the git-status panel, the
  terminal grid (built-in box-drawing, OSC 8 + regex links, OSC 52 clipboard),
  the two-file config model, the built-in MCP/IPC server, and how Alacritree
  compares against worktree CLIs, AI-agent orchestrators, and other native
  terminals in the space.
- [`docs/keyboard-shortcuts.md`](docs/keyboard-shortcuts.md) — every key
  binding the app understands, split between hard-coded app shortcuts
  (sidebar toggles, workspace and session switching, modals) and the
  configurable `[[keyboard.bindings]]` layer, including the full list of
  supported `action = "…"` values and which Alacritty actions are
  intentionally not wired up.
- [`docs/features.md`](docs/features.md) — upstream Alacritty's feature
  overview (vi mode, search, hints, selection expansion). Kept for reference;
  not everything listed is implemented in the egui shell yet.

## Repository layout

This is a Cargo workspace:

- `alacritree/` — **the fork.** GUI shell, sidebars, worktree integration.
- `alacritty_terminal/` — vendored from upstream Alacritty; used as a library.
- `alacritty/`, `alacritty_config/`, `alacritty_config_derive/` — vendored
  upstream crates. Treated as read-only here; the upstream `alacritty` GUI
  binary is **not** what this fork ships.

## Relationship to upstream Alacritty

Alacritree is not a competitor to or replacement for Alacritty. It depends on
upstream's terminal crate and would not exist without it.

## License

Released under the [Apache License, Version 2.0](LICENSE-APACHE), matching
upstream Alacritty.
