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

### Workspace scratchpads

`Ctrl+Backtick` opens the scratchpad tab dedicated to the current workspace.
If its editor tab already exists, Alacritree selects that tab; otherwise it
creates and activates a new tab. Scratchpads participate in the normal tab
strip, sidebar session list, session cycling, command palette, and close flow.
Pressing `Ctrl+Backtick` while the scratchpad is already active closes it
immediately without confirmation because every edit has already been saved.

The first invocation creates a Markdown document under Alacritree's config
folder and opens it in a built-in, borderless text editor. Its padded writing
surface inherits the terminal background and deliberately has no toolbar or
save button. Every text change is written to the backing file immediately,
including typing, deletion, paste, undo, and redo. A filesystem error is shown
inside the pane while the in-memory text remains intact.

Switching to another tab or workspace leaves the editor state intact. Closing
the scratchpad tab releases that state; the next invocation reloads the same
auto-saved file. Deleting a worktree also retains its scratchpad document.

Files live in `$XDG_CONFIG_HOME/alacritree/scratchpads/` (normally
`~/.config/alacritree/scratchpads/`) or `%APPDATA%\alacritree\scratchpads\` on
Windows. Home uses `home.md`; worktrees use a readable leaf name plus a stable
path digest so same-named worktrees cannot collide.

Each terminal session has its own background read/write thread, a unique
`window_id` (so OSC 7 / signal events route correctly), and forwards terminal
events through an `EventProxy` that requests an egui repaint on every PTY
message.

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
`<base>/<project>-<hash>/<branch>`, where `<base>` defaults to
`~/.alacritree/worktrees` so they never clutter the repo's parent directory
and stay grouped per app. The base is configurable per `[workspace]` in
`alacritree.toml` (see Configuration below); changing it never moves existing
worktrees — discovery goes through `git worktree list`. The `<hash>`
disambiguates same-named repos in different locations. `/` in branch names is
rewritten to `-`, and a numeric suffix is appended if the target already
exists.

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

### Per-worktree base branch

The git panel diffs each worktree against an automatically picked base: the
open PR's base branch if there is one, otherwise the project's default branch.
To override it for a single worktree (e.g. a branch cut from `develop`):

- right-click the worktree in the left sidebar → *Set base branch…*, or
- click the `vs <branch>` label in the git panel, or
- bind the `SetBaseBranch` action and press it (targets the sidebar-cursored
  worktree when the sidebar has focus, the current worktree otherwise):

  ```toml
  [[keyboard.bindings]]
  key = "B"
  mods = "Control|Alt"
  action = "SetBaseBranch"
  ```

Picking *Auto* returns to automatic detection. Overrides persist in
`state.toml` per worktree path.

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
Alacritree — while Alacritree-specific options live in `alacritree.toml`
under `[ui]` and `[workspace]`:

```toml
[ui]
sidebar_background = "#1c1c1c"
sidebar_foreground = "#d8d8d8"
sidebar_border     = "#2a2a2a"
sidebar_accent     = "#6a9fb5"
notifications      = true   # desktop notification when a hidden session bells;
                            # clicking it focuses the session that pinged
attention_grace_ms = 0      # hold pings this long and drop them if the session
                            # resumes work (agents that continue between tasks);
                            # 0 pings immediately
scrollbar          = "floating"  # sidebar scrollbar: "floating" (default, thin
                                 # overlay that expands over the row icons on
                                 # hover) or "solid" (reserved gutter that
                                 # never covers the icons)
sidebar_click_focus = true  # clicking a sidebar moves keyboard focus to it;
                            # picking a session/worktree focuses the terminal
                            # instead (default false)

[ui.icons]                  # sidebar glyph overrides (e.g. Nerd Font icons)
search = "⌕"                # glyph prefixing the sidebar search prompt

[workspace]
worktree_dir = "~/dev/worktrees"   # base dir for new worktrees (default ~/.alacritree/worktrees)

[[workspace.overrides]]            # optional per-project override
project = "~/Git/github/alacritree"
worktree_dir = "D:/wt"

[window]
opacity = 0.92   # restart required — transparency is a ViewportBuilder flag
```

Everything Alacritty's TOML accepts for palette, cursor, scrolling, window
padding, shell, env, and bindings is parsed by the same `Raw*` structs.

### Shell launch profiles

Named launch profiles live in `alacritree.toml`:

```toml
[ui]
default_profile = "ubuntu"       # what plain new-session (Ctrl+T) uses

[[ui.profiles]]
name = "ubuntu"
program = "wsl.exe"
args = ["-d", "ubuntu"]

[[ui.profiles]]
name = "pwsh"
program = "pwsh"
args = ["-NoLogo"]
```

Launch a profile from the small **+** segment at the right end of the
session tab strip (left-click: default new session; right-click: pick a
profile), bind one to a key with the `SpawnProfile1`…`SpawnProfile9`
actions (1-indexed into the `[[ui.profiles]]` order), or right-click a
project row and pin a profile as that project's shell override.

Shell selection precedence for a plain new session: per-project override →
WSL auto-selection by project location → `default_profile` →
`[terminal.shell]` / OS default.

## Persistence

Persistent files written by Alacritree:

- `$XDG_CONFIG_HOME/alacritree/state.toml` — projects, expanded state,
  sidebar visibility.
- `$XDG_CONFIG_HOME/alacritree/scratchpads/*.md` — one persistent Markdown
  scratchpad per workspace. Worktree deletion does not remove these notes.
- `<worktree>/.claude/settings.local.json` — touched only during worktree
  creation, only to set `preferredNotifChannel = "terminal_bell"`.

No telemetry, no analytics, no background network traffic.

## Quit confirmation

`Ctrl+Q` (or `Cmd+Q` on macOS) opens a quit modal. The window close button
goes through the same modal so a stray Cmd-W doesn't kill live sessions.
Modal Enter/Escape are intercepted before the terminal sees them.

## MCP server — drive Alacritree from an LLM

Alacritree exposes its features to LLM agents through the
[Model Context Protocol](https://modelcontextprotocol.io). `alacritree mcp`
runs a stdio MCP server that talks to the running app, so an agent can browse
your projects and worktrees, open shells in them, type into terminals, read
their output, and inspect git state. Register it with your MCP client, e.g.:

```sh
claude mcp add alacritree -- alacritree mcp
```

Tools:

| Tool | What it does |
| --- | --- |
| `list_projects` | Sidebar projects with their worktrees, branches, and default branch |
| `list_sessions` | All sessions: id, title, workspace, kind, size, active tab, attention flag |
| `select_workspace` | Focus a workspace, like clicking it in the sidebar |
| `create_session` | Open a new shell session in a workspace |
| `close_session` | Close a session |
| `send_text` | Type into a terminal or insert into a scratchpad; scratchpad changes auto-save |
| `read_screen` | Read a session's screen text, cursor position, and optional scrollback |
| `read_scratchpad` | Read the auto-saved Markdown scratchpad for the current, Home, or a specified workspace |
| `move_session` | Re-home a session under another worktree (`alacritree session move <session_id> <path>`); path may be anywhere inside it |
| `git_status` | Staged/unstaged files and per-file +/- vs the default branch |
| `create_worktree` | Create a worktree + branch, same flow as the sidebar's `+` button |
| `refresh_project` | Re-scan a project's worktrees |

`read_scratchpad` reads the backing file directly, so it remains useful when
the editor tab is closed. Because the built-in editor writes every change
immediately, MCP clients see the same auto-saved contents as the editor.

Under the hood this mirrors Alacritty's IPC design (unix only): the app
listens on `$XDG_RUNTIME_DIR/alacritree/alacritree-<pid>.sock` and advertises
the path to child PTYs via `ALACRITREE_SOCKET` — so an agent running *inside*
an Alacritree session automatically targets the instance hosting it. Other
clients fall back to scanning the socket directory, or can pass
`alacritree mcp --socket <path>` explicitly. Set `ipc_socket = false` under
`[general]` (shared with Alacritty's option of the same name) to disable the
socket entirely.

### Shell integration: following the cwd

alacritree never guesses a session's directory — a session tells it, via
`ALACRITREE_SESSION_ID` (exported into every session) and
`alacritree session move`. Two opt-in hooks cover the common flows; add the
one(s) you want to your shell config.

**Sidebar follows the shell** — report the cwd at every prompt:

```sh
# bash (~/.bashrc)
_alacritree_report_cwd() {
  [ -n "$ALACRITREE_SESSION_ID" ] || return 0
  alacritree session move "$ALACRITREE_SESSION_ID" "$PWD" >/dev/null 2>&1 || true
}
PROMPT_COMMAND="_alacritree_report_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"

# zsh (~/.zshrc)
precmd_functions+=(_alacritree_report_cwd)
```

```powershell
# PowerShell ($PROFILE) — wrap your existing prompt function
function prompt {
  if ($env:ALACRITREE_SESSION_ID) {
    alacritree session move $env:ALACRITREE_SESSION_ID "$PWD" *> $null
  }
  "PS $PWD> "
}
```

Paths outside any known worktree are rejected by alacritree and ignored by
the hook, so `cd /tmp` moves nothing.

**Shell follows the sidebar** — when an agent moved the session (e.g. via the
`move_session` MCP tool), land the shell there at the next prompt. Only the
shell can change its own cwd, which is why this is a hook and not an app
feature (requires `jq`):

```sh
# bash (~/.bashrc)
_alacritree_follow() {
  [ -n "$ALACRITREE_SESSION_ID" ] || return 0
  local ws
  ws=$(alacritree session list --json 2>/dev/null | jq -r --arg id "$ALACRITREE_SESSION_ID" \
    '.sessions[] | select((.id | tostring) == $id) | .workspace // empty')
  [ -n "$ws" ] || return 0
  case "$PWD" in "$ws"|"$ws"/*) ;; *) cd "$ws" ;; esac
}
PROMPT_COMMAND="_alacritree_follow${PROMPT_COMMAND:+;$PROMPT_COMMAND}"

# zsh (~/.zshrc)
precmd_functions=(_alacritree_follow "${precmd_functions[@]}")
```

Both hooks cost one local-socket round trip per prompt; running both at once
is fine — `_alacritree_follow` only `cd`s when the session's workspace points
outside the current worktree, so it doesn't fight `_alacritree_report_cwd`
over ordinary subdirectory moves within the same worktree. If you install
both, `_alacritree_follow` must run before `_alacritree_report_cwd` in the
same prompt (as shown above with `PROMPT_COMMAND`/`precmd_functions`
prepending), so the follow hook `cd`s into an agent-moved workspace before
report-cwd stamps the session with the (otherwise stale) `$PWD`.

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
