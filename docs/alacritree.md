# Alacritree

Alacritree is a native desktop terminal built on top of `alacritty_terminal`
(the headless PTY + VT parser + grid that powers Alacritty) and rendered with
egui/eframe. On top of that base it adds a worktree-aware sidebar, multi-session
workspaces, and a git-status panel — turning a single Alacritty-grade window
into the command centre for parallel Git work.

This document describes what Alacritree ships today. For the upstream terminal
features inherited from Alacritty (vi mode, search, hints), see
[`features.md`](./features.md). For the full key binding surface, see
[`keyboard-shortcuts.md`](./keyboard-shortcuts.md).

## Workspaces and sessions

A **workspace** in Alacritree is either the *home* workspace (cwd = `$PWD`) or
a specific Git **worktree** registered in the left sidebar. Each workspace
keeps an independent list of PTY-backed **sessions**, and the active session
per workspace is remembered as you switch between them.

- `Ctrl+T` (or `Cmd+T` / `Cmd+N` on macOS) opens a new shell session in the
  current workspace. The session inherits the workspace directory as cwd.
- `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle sessions within the current workspace;
  on macOS `Cmd+1` … `Cmd+9` / `Cmd+Shift+]` / `Cmd+Shift+[` mirror Terminal.app.
- `Alt+Right` / `Alt+Left` jump between workspaces.
- Sessions are **not** killed when you switch workspaces — only when you close
  them (or quit the app). Scrollback, running commands, and PTY state survive
  arbitrary switches between worktrees.

Each session has its own background read/write thread, a unique `window_id`
(so OSC 7 / signal events route correctly), and forwards terminal events
through an `EventProxy` that requests an egui repaint on every PTY message.

## Left sidebar — projects and worktrees

The left sidebar (`Ctrl+B`) lists projects you have registered and, under each
project, its Git worktrees.

- **Adding a project.** Drop any directory in. If it's a Git repo, Alacritree
  enumerates worktrees via `libgit2` (`repo.worktrees()`); if it's not, a
  single pseudo-worktree pointing at the directory is created so you can still
  spawn a shell there.
- **Default-branch detection.** Tried in order: `init.defaultBranch` →
  `refs/remotes/origin/HEAD` → presence of `main` or `master`. This is what
  the create dialog branches from and what the right sidebar diffs against.
- **Persisted state.** The list of project roots, their expand/collapse state,
  and the sidebar visibility flags are written to
  `$XDG_CONFIG_HOME/alacritree/state.toml`. Failures are logged and ignored —
  a missing or corrupt state file never crashes the app.

### Creating a worktree

The create modal validates the proposed branch name against `git
check-ref-format` rules (no whitespace, no `..`, no `~^:?*[\`, no leading `-`,
no trailing `.` or `.lock`, etc.) before doing anything. Creation runs on a
background thread and streams progress steps back to the UI:

1. Verify `origin` is configured.
2. Verify the base branch exists locally or on `origin`.
3. `git fetch origin <base>`.
4. `git worktree add <target> -b <branch> origin/<base>` (or local fallback).
5. Copy AI-assistant configs from the project root into the new worktree —
   `CLAUDE.md`, `CLAUDE.local.md`, `.claude/`, `.clauderc`, `AGENTS.md`,
   `.cursorrules`, `.cursor/`, `.aider.conf.yml`, `.aiderignore`,
   `.copilot-instructions.md`, `.github/copilot-instructions.md`,
   `.windsurfrules`, `.roomodes`, `.roo/`, `.codeium/`, `.continue/`. Existing
   destination files are left alone.
6. Set `preferredNotifChannel: terminal_bell` in
   `.claude/settings.local.json` so Claude Code's completion bell fires through
   the terminal — every other key in the file is preserved.

Worktrees are created under
`~/.alacritree/worktrees/<project>-<hash>/<branch>` so they never clutter the
repo's parent directory and stay grouped per app. The `<hash>` disambiguates
same-named repos in different locations. `/` in branch names is rewritten to
`-`, and a numeric suffix is appended if the target already exists.

### Deleting a worktree

The delete modal pre-computes a cheap dirty-status summary (staged / modified
/ untracked counts) so the user can see what would be lost before confirming.
Confirmation runs `git worktree remove` (with `--force` if requested) and then
`git branch -D <branch>` — the branch deletion is best-effort so a detached
HEAD doesn't block worktree cleanup.

## Right sidebar — git status

The right sidebar (`Ctrl+G`) shows live status for the active workspace's
worktree:

- Current branch (or short OID on detached HEAD).
- Staged and unstaged file lists with one-character glyphs (`A`/`M`/`D`/`R`/`?`/`!`).
- A file-level diff summary against the **merge base** with the default branch
  — so local-only commits still show up when the default branch hasn't moved.

Status is cached per-worktree with a 1.5 s refresh interval (`StatusCache`),
so the panel stays responsive even on large repos. A faster cheap path
(`dirty_counts`) is used by the delete modal — it skips the branch-diff work
and just counts what `git worktree remove` would reject.

## Terminal grid

Alacritree paints its grid cell-by-cell using the egui font system, with the
cell size computed from the configured font. Resizing the window resizes the
PTY (`Term::resize`) on the fly. The terminal drains pending events on every
frame and handles:

- `Title` → updates the window title.
- `ChildExit` → marks the session as exited and shows it in the session list.
- `PtyWrite` → forwards bytes from terminal modes (e.g. clipboard responses)
  back into the PTY.
- `ClipboardStore` / `ClipboardLoad` → OSC 52 read/write, routed through the
  same clipboard wrapper described below.

### Built-in box-drawing glyphs

Unicode box-drawing and powerline glyphs are rendered from a vector spec
(`builtin_font.rs`) rather than fetched from the font file. This guarantees
seamless cells regardless of the user's monospace font choice — borders,
braille blocks, and powerline separators always tile perfectly. The behaviour
can be toggled with `font.builtin_box_drawing = false`.

### Clickable links

URL detection mirrors Alacritty's default URL hint behaviour:

- **OSC 8 hyperlinks** take priority over regex matches — they carry an
  explicit URI that may differ from the visible text.
- **Regex matches** use exactly Alacritty's URL pattern (`ipfs:`, `ipns:`,
  `magnet:`, `mailto:`, `gemini://`, `gopher://`, `https://`, `http://`,
  `news:`, `file:`, `git://`, `ssh:`, `ftp://`).
- **Post-processing** strips trailing punctuation and unbalanced brackets so a
  URL embedded in prose (`see (https://example.com).`) opens at the right
  bound.

Clicking a recognised link hands it to the OS handler — `xdg-open` on
Linux/BSDs, `open` on macOS, `cmd /c start` on Windows.

### Clipboard

Two clipboards are distinguished:

- **System clipboard** — `Ctrl+Shift+C` / `Ctrl+Shift+V` (also `Cmd+C` /
  `Cmd+V` on macOS).
- **PRIMARY selection** on Linux — `Shift+Insert` paste, with arboard's
  `SetExtLinux` / `GetExtLinux` backed by `wayland-data-control` so X11 and
  Wayland both work. Platforms without a separate PRIMARY fall back to the
  system clipboard.

OSC 52 in the terminal flows through the same wrapper.

## Input and key bindings

Input handling is layered:

1. **Built-in app shortcuts** — sidebar toggles, workspace switches, session
   spawn / cycle, modal Enter/Escape. Hard-coded today.
2. **Configurable terminal bindings** — parsed from `[[keyboard.bindings]]`
   in the TOML config. Alacritty's default set is preloaded; your entries are
   checked first so any default can be overridden or unbound (`action =
   "None"`).
3. **Egui text events** — preferred for printable input because they handle
   dead keys and IME correctly. Control bytes (`Ctrl-<letter>`), CSI sequences
   for arrows / function keys, and `ESC + key` for `Alt+<key>` are derived
   directly from `egui::Event::Key`.

Vi mode, search mode, and hint regex actions from Alacritty's config grammar
are parsed but treated as no-ops (with a `debug`-level log). They depend on
state the egui grid does not yet track.

Full action list, defaults, and customisation examples live in
[`keyboard-shortcuts.md`](./keyboard-shortcuts.md).

## Fonts

A system monospace font is loaded via `fontdb` and registered with egui at
startup. Font size matches Alacritty's default of 11.25 pt and is adjustable
at runtime with `Ctrl+0` / `Ctrl+=` / `Ctrl+-` (mirrored on `Cmd+…` on macOS).
Bold, italic, and bold-italic faces can be picked independently in
`font.normal` / `font.bold` / `font.italic` / `font.bold_italic`. Per-cell
`offset` and per-glyph `glyph_offset` tuning is supported, again to match
Alacritty's config surface.

## Configuration

Two TOML files are loaded and **deep-merged using Alacritty's own merge
semantics** — arrays concatenate (so `[[keyboard.bindings]]` in
`alacritree.toml` *adds to* upstream bindings rather than replacing them),
tables merge recursively, primitives replace.

Search path (matches Alacritty exactly):

1. `$XDG_CONFIG_HOME/alacritty/alacritty.toml`
2. `~/.config/alacritty/alacritty.toml`
3. `~/.alacritty.toml`
4. `/etc/alacritty/alacritty.toml`

Then the same locations for `alacritree.toml`. The two-file split keeps
shared options (palette, cursor, scrolling, shell, key bindings) in
`alacritty.toml` — usable by both the upstream alacritty terminal and
Alacritree — while Alacritree-specific UI options live in `alacritree.toml`
under `[ui]`:

```toml
[ui]
sidebar_background = "#1c1c1c"
sidebar_foreground = "#d8d8d8"
sidebar_border     = "#2a2a2a"
sidebar_accent     = "#6a9fb5"
notifications      = true   # desktop notification when a hidden session bells

[window]
opacity = 0.92   # restart required — transparency is a ViewportBuilder flag
```

Everything Alacritty's TOML accepts for palette, cursor, scrolling, window
padding, shell, env, and bindings is parsed by the same `Raw*` structs.

## Persistence

Two files are written:

- `$XDG_CONFIG_HOME/alacritree/state.toml` — projects, expanded state,
  sidebar visibility.
- `<worktree>/.claude/settings.local.json` — touched only during worktree
  creation, only to set `preferredNotifChannel = "terminal_bell"`.

No telemetry, no analytics, no background network traffic.

## Quit confirmation

`Ctrl+Q` (or `Cmd+Q` on macOS) opens a quit modal. The window close button
goes through the same modal so a stray Cmd-W doesn't kill live sessions.
Modal Enter/Escape are intercepted before the terminal sees them.

---

## Why Alacritree beats every competitor in this space

Every other tool that touches Git worktrees today falls into one of three
buckets, and each bucket gives up something Alacritree refuses to. Pure
worktree CLIs (branchlet, gtr, gwq, par, jackiotyu's VS Code extension) hand
you a worktree and walk away — you still need a terminal, you still re-launch
sessions every time you switch, you still lose scrollback. The growing pile of
AI-agent orchestrators (hive, ouijit, amux, agent-of-empires, uzi, genie,
mozzie, superset, emdash, capy) bury the terminal inside a Kanban app, ship a
100 MB Electron / Tauri / Chromium runtime, and lock you into a specific
agent stack you didn't choose. The one product in the closest neighbourhood,
aizen.win, is macOS-only, Apple-Silicon-only, and paid. Alacritree is a fast,
native, open-source app — `alacritty_terminal`'s nine-year-battle-tested VT
engine rendered in egui — that boots in milliseconds, reads your existing
`alacritty.toml` unchanged, persists per-worktree sessions across switches,
and stays neutral about what you actually run inside them. The worktree
sidebar is opinionated where it should be (per-project layout, AI-config copy,
branch validation, dirty-state warning before delete) and invisible where it
shouldn't be (no agent assumptions, no telemetry, no Chromium). That
combination — Alacritty-grade terminal first, worktree UX second, no AI
baggage — is genuinely unoccupied territory in the current landscape, and
it's what makes Alacritree both lighter than every "agent IDE" *and* more
useful than every plain worktree CLI.
