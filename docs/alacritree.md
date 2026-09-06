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

### herdr agents

herdr is a terminal workspace manager for coding agents. When a herdr server is running, the agents it manages appear in the sidebar under the worktree each agent's working directory matches, dimmed and carrying the `◫` mark that says the pane belongs to herdr rather than to alacritree. A row names the agent's pane title where herdr reports one, with the agent kind in front of it as context, so two agents of one kind in one checkout can be told apart; an agent with no title is named by its kind alone. An agent whose directory matches no worktree — including one whose checkout has been removed — is listed under Home.

- **Attaching.** Enter or a click opens a session attached to that agent, and the row is replaced by the session's own row, which keeps the herdr mark so an attached agent still says where it lives. Hovering either row spells out the same sentence — the state, the harness, and what herdr calls the pane, with the way out after them. Detaching is herdr's own chord, not one of alacritree's, read from herdr's `config.toml` so a rebound `keys.prefix` or `keys.detach` is what you are told. The row's `×` ends the attach and leaves the pane running under herdr, so it offers to detach rather than to close, and the agent's own row comes back. Whether it asks first is `[ui] confirm_session_detach`, a switch of its own: a detach destroys nothing, so the busy question `confirm_session_close` asks has no answer here, and turning one off says nothing about the other.
- **Order.** A workspace draws its own shell sessions first, then every herdr pane it holds — the sessions attached to one and the agents nothing is attached to alike — in herdr's own order. Attaching therefore changes how a pane is drawn and never where it sits, and neither does detaching or a restart. Reordering is for alacritree's own sessions: a herdr pane's place belongs to herdr, so drag and `MoveSessionUp` / `MoveSessionDown` pass over one. Sort the panes in herdr and the sidebar follows within a poll.
- **Status.** The row's mark is herdr's own, so a pane carries one symbol whether you read it in herdr or in the sidebar. herdr distinguishes blocked, working, done and idle, and offers two indicator sets; alacritree follows whichever its `[ui] status_indicators` selects, drawing `● ● ● ○` for the dotted set and `× ◐ ✓ ○` for the symbol one, coloured red, yellow, teal and green out of your own palette. A status alacritree does not recognise is drawn as `·` rather than as idle. An attached agent's session row takes herdr's word too, so a dialog the pane title never mentions still reaches the sidebar, and attaching never repaints a pane in a different vocabulary.
- **Native Windows.** herdr cannot attach a single agent there, so attaching focuses the pane in your own herdr window and shares the whole herdr session — the view resizes with the alacritree pane, and the tooltip says `shared view`. Such a session is a window on herdr rather than on one pane, so it follows herdr's focus: the row reports whichever agent herdr is showing, including one started long after the attach, and that agent is never listed a second time as unattached. One window is all a side has: clicking the row of the pane it is already on brings the session up, and clicking another agent's row moves the view to that pane. herdr's client reads the focused pane when it starts and keeps that view afterwards, so moving it means attaching again — the session is replaced once its successor is up, rather than a second client being left showing the same herdr twice. Both halves of that gesture are herdr processes, so with `[ui] async_session_spawn` on they run off the UI thread and the window keeps painting while herdr answers.
- **Discovery.** Alacritree asks the native `herdr` on your `PATH` and each running WSL distro. Nothing is installed or started. A side with no herdr on it is asked once and then left alone, so a machine without herdr pays a single failed spawn; a side whose herdr answered — even to say its server is down — is retried on a slow clock, so starting herdr after alacritree brings its agents in without a restart. See `[ui.herdr]` under [Configuration](#configuration) for the poll interval and the opt-out.

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

### Over-wide icons

An icon outside the built-in ranges comes from whichever fallback face has it,
sized to *that* face's em rather than to the cell. On a narrow cell — a
CJK-derived face's half-width advance, say — a Nerd Font icon is wider than
the column the terminal gave it and spills into the next one, where the next
run's background paints over the part that escaped.

Such an icon is drawn across the blank cells that follow it instead, centred
on the span it ends up with, up to four extra cells. This is kitty's
behaviour; alacritty and Windows Terminal both let the overflow happen.

Only the private use areas grow. A letter that happens to arrive from an
over-wide fallback face keeps its cell however far it overruns, which is why
this needs no switch — ordinary text can never move. `U+E0A0`–`U+E0A3` and
`U+E0C0`–`U+E0C7` are held back as well, matching kitty's `narrow_symbols`
default: those marks read as part of the segment beside them, so a wider one
looks wrong where a clipped one only looks cramped.

Blanks are the only cells an icon may take — anything else is a character it
would paint over — and one wanting more room than it gets stays where it is
rather than being pulled left. A blank draws nothing but its background, so it
counts even when its foreground differs from the icon's, which is what lets an
icon and the differently-highlighted space beside it share a run. Icons are
commonly authored with exactly that trailing space (`" "`).

Two things it leaves alone. A double-width character already owns two columns
and is sized to them, so it never grows — it also never shares a run, since
the flags marking its second column end one. And a block cursor parked on a
grown icon redraws it at its own cell while the cursor is there, so the icon
shifts back for as long as the cursor sits on it.

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

### Drag and drop

Dropping files on the window does something different per region:

- **Terminal grid** — the paths are *pasted*, quoted for the shell, joined by
  spaces, with a trailing space. It is a paste, not typing: bracketed paste is
  honoured, and any path carrying a control character is left out of the payload
  (see below), so what arrives is a command line waiting on your Enter.
  Dropping an image on a session running Claude Code gives you `[Image #N]`,
  because Claude Code resolves a pasted image path.
- **Projects sidebar** — a folder is added as a project; a file adds the folder
  containing it. Several files from one folder add it once.
- **Scratchpad tab** — the plain path is inserted at the cursor, one per line
  and unquoted, since a document is not a command line. Dropping mid-line puts
  the block on lines of its own rather than welding it to the text either side.

Paths dropped into a WSL session are rewritten to their distro-side spelling
(`C:\pics\a.png` becomes `/mnt/c/pics/a.png`) and quoted POSIX-style, because a
`C:` path resolves to nothing inside a distro. That POSIX quoting applies even
when `quote` names another mode — `windows` quoting fed to `bash` is broken by
construction. A `\\wsl.localhost\<distro>\…` path is only rewritten when it
belongs to the distro the session is running in; one from a *different* distro
is pasted as-is, because the same Linux path means a different file there.

Filenames carrying any control character are skipped. An application that has
not enabled bracketed paste cannot tell a paste from typing, and a line editor
acts on those bytes as commands: a newline arrives as Enter, and readline
accepts the line on `Ctrl-O` as well. Quoting is no defence, because the editor
binds the byte before the shell ever parses the quotes around it.

While files hover, the region that would receive them is tinted.

Windows only, for now: winit reports no cursor position during a drag, so on
other platforms the sidebar cannot be targeted and no tint is drawn. Drops
still reach the terminal or the scratchpad there, chosen by which tab is
active.

## Input and key bindings

Input handling is layered:

1. **Key bindings** — parsed from `[[keyboard.bindings]]` in the TOML config.
   Alacritty's default set is preloaded, and so are alacritree's own (sidebar
   toggles, workspace switches, session spawn / cycle, palette); your entries
   are checked first so any default can be overridden, forwarded to the
   terminal (`action = "ReceiveChar"`), or consumed without an action (`action
   = "None"`).
2. **Modal Enter/Escape** — consumed by whichever dialog is open. These are
   not bindings and cannot be rebound.
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

### `builtin_symbols`

`true` by default. Alacritree bundles a small font carrying only the glyphs
it paints itself (`⌂`, `⌫`, `⇅`, `✓` and about 25 others), registered as the
**last** entry in each chrome font family.

Because it is last, a font that already has a glyph keeps rendering it — your
font choice is unaffected, and the bundled face is reached only where the
alternative was an empty box.

Set `false` to skip it entirely.

```toml
[ui.font]
builtin_symbols = false
```

One limit is worth knowing: a font can claim a codepoint in its character map
and still have nothing to draw for it. egui takes the first font that claims
a glyph, so such a font wins over the bundled one and the box stays empty.
This is uncommon, and no ordering fixes it.

Run `alacritree --licenses` for the bundled font's licence.

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
sidebar_attention  = "#f1c40f"   # optional; unset derives the attention badge
                                 # from the palette's normal[3] (ANSI yellow),
                                 # the same fallback pattern sidebar_accent
                                 # uses for its own default (normal[4], blue)
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
sidebar_focus      = "preserve"  # how far the projects sidebar goes when the
                                  # cursor's row stops being rendered.
                                  # "preserve" (default): a filtered-out cursor
                                  # climbs to its nearest visible ancestor and
                                  # returns when the filter widens; a deleted row
                                  # slides to a sibling bounded by its parent.
                                  # "follow": also moves the terminal to a delete
                                  # landing that has a live session, and lands a
                                  # closed session on its neighbour instead of on
                                  # the workspace's first session
sidebar_follow_active = false  # scroll the projects sidebar to the session on
                                # screen whenever it changes — a cycling key, a
                                # click, the palette, an IPC request (default
                                # false); the cursor is left where it was
sidebar_scroll_align = "minimal"  # where a row a sidebar scrolled to is
                                   # parked. "minimal" (default) rests it
                                   # against whichever edge it entered from.
                                   # "center" re-centres the list on every
                                   # cursor step, and scrolls a clicked row
                                   # out from under the pointer — not a
                                   # scrolloff, the row always lands mid-panel
vsync              = true   # restart required — wait for the display's refresh
                            # before showing a finished frame (default true).
                            # false presents each frame as soon as it is drawn,
                            # trading tearing for lower keystroke-to-screen delay
focus_priority_boost = false  # Windows only, restart required — put the
                              # session on screen one scheduling class above
                              # normal (default false).
                              # A program that redraws its line as you type
                              # needs CPU for every keystroke, and at normal
                              # priority a build saturating every core starves
                              # it: nushell echoed in 10 ms boosted, against up
                              # to 1.9 s unboosted. Covers the shell and
                              # everything it starts, at any depth, including
                              # commands that live only a moment. Follows
                              # focus, and raises nothing while the window is
                              # in the background
async_session_spawn = false   # open a session's PTY on a worker rather than
                              # in the frame that asked for it, so spawning
                              # does not stutter (default false).
                              # Creating a console process costs milliseconds
                              # when the machine is idle and hundreds when it is
                              # busy, and the frame pays all of it. The tab
                              # appears at once and starts painting when its PTY
                              # attaches; anything typed in between is replayed
reap_descendants_on_close = false  # Windows only, restart required — when a
                              # session closes, end everything it started at
                              # any depth (default false).
                              # The console reaps only the programs attached to
                              # it, so a helper that left the console — an
                              # editor's background search, anything started
                              # detached — outlives the terminal and keeps
                              # piling up. This also covers alacritree being
                              # killed or crashing, since the kernel does the
                              # reaping when the last handle closes. A process
                              # that means to outlive the terminal must ask
                              # with CREATE_BREAKAWAY_FROM_JOB
search_scope       = "filtered"  # whether a sidebar search is confined by the
                                 # active toggle filters
                                 # "filtered" (default): a query narrows what
                                 # the toggles already allow
                                 # "all": a query reaches every row; the
                                 # toggles resume when it empties
sidebar_tooltips   = "elided"    # when a sidebar row spells its full name out
                                 # on hover — both sidebars, so a git panel
                                 # path answers to it like a worktree name
                                 # "elided" (default): only where the row had
                                 # to cut the name off
                                 # "always": on every row — a row without a
                                 # tooltip ends the run in which egui reopens
                                 # the next one instantly, so a sweep down the
                                 # list stalls on each name that fits
                                 # "off": never
icon_tooltips      = true        # whether a sidebar icon explains itself on
                                 # hover (default true) — what a button does
                                 # ("add project", "close session", …) and what
                                 # a status badge reports: the agent running in
                                 # a row, a row asking to be looked at, a
                                 # branch's PR and upstream state, and the
                                 # letter a git panel row leads with
                                 # independent of sidebar_tooltips, which is
                                 # about a name the row had to cut off: an
                                 # icon's hint never depends on panel width
confirm_session_close = "never"  # when the sidebar × asks before killing a PTY:
                                 # "never" (default) | "busy" | "always"
confirm_session_detach = true    # whether the × on a harness-managed row asks
                                 # before detaching. Its own switch, since a
                                 # detach leaves the pane running under its
                                 # harness and lists its row again
last_session_close = "respawn"   # what happens when the on-screen workspace
                                 # stops having sessions, whether the last one
                                 # closed or the worktree was deleted:
                                 # "respawn" (default) starts a fresh one,
                                 # "navigate" moves to another workspace,
                                 # "ring_global" and "ring_project" move to the
                                 # nearest surviving session in the ring, else
                                 # home
pr_status          = false  # poll `gh` for each branch's open PR, which drives
                            # the PR row icons, the PR-state filters, and $pr
                            # below (default false)
pr_status_concurrency = 8   # cap concurrent `gh` PR lookups; default 8,
                            # clamped to a minimum of 1
upstream_status    = false  # paint a badge on each worktree row for its
                            # branch's upstream state — level, diverged, gone,
                            # or untracked (default false; also gates whether
                            # the state is computed at all, so it costs
                            # nothing when off)
                            # Local refs only: nothing fetches, so a branch
                            # deleted on the remote still reads as tracked
                            # until something prunes locally.
                            # extensions.worktreeConfig is unsupported: a
                            # linked worktree that overrides branch.* in its
                            # own config.worktree is read from the project
                            # root instead, so that override is not seen.
delta_path         = "delta"     # explicit delta binary for the diff pane;
                                 # unset discovers it on PATH
worktree_name      = "$name ${pr: }"  # template for worktree row labels:
                            # $name, $branch, $path, $pr (as #123, needs
                            # pr_status), and ${var:fallback}. Unset keeps the
                            # plain worktree name
project_name       = "$name"     # same for project rows ($name, $path). A
                                 # manual rename always wins over the template

[ui.font]                   # chrome only — sidebars and modals, not the grid
family = "Inter"            # unset derives from [font]
size   = 12.0               # points, same unit as [font] size
bold_family        = "Inter Display"  # unset falls back to family
italic_family      = "Inter"          # unset falls back to family
bold_italic_family = "Inter Display"  # unset falls back to family

[ui.session_display]        # startup defaults; key bindings toggle both at runtime
sidebar_always = false      # keep a sidebar session row even with one session
tabs_always    = false      # keep a tab-strip segment even with one session

[ui.session_reorder]        # startup default; ToggleSessionDrag flips drag at runtime
drag  = false               # drag a session row with the mouse to reorder it
scope = "workspace"         # how far a reorder may carry a session:
                            # "workspace" | "project" | "anywhere"

[ui.focus_outline]          # off by default, which keeps the current look
sidebar   = false           # outline the projects sidebar when it has focus
terminal  = false           # outline the terminal when it has focus
color     = "#6a9fb5"       # unset falls back to the theme accent
thickness = 1.0             # logical pixels, not scaled by ui_scale

[ui.path_style]             # per-site path abbreviation, all "full" by default
diff_title = "full"         # "full" | "fish" (a/b/c) | "zed" (leading dirs cut)
git_rows   = "full"
git_header = "full"

[ui.path_style.filename]    # emphasis for the last path segment
color  = "#d8d8d8"
bold   = true
italic = false

[ui.path_style.parent]      # emphasis for the leading directories
color  = "#8a8a8a"

[ui.icons]                  # sidebar glyph overrides (e.g. Nerd Font icons).
                             # Each key takes a bare string (glyph only, as
                             # below) or a table that also styles color,
                             # weight, slant, and size — see "Icon styling"
                             # below. The bare-string form keeps working
                             # unchanged.
search = "⌕"                # glyph prefixing the sidebar search prompt
worktree_main = "●"         # the project's main checkout
worktree = "○"
session = "▪"
home = "⌂"
project_expanded = "▾"
project_collapsed = "▸"
pr_open = "⬤"               # the four PR glyphs need pr_status = true; they
pr_draft = "◯"              # differ by colour, so overriding one shape is
pr_merged = "⬤"             # usually not what you want
pr_closed = "⬤"
upstream_level = "✓"        # the four upstream glyphs need upstream_status =
upstream_diverged = "⇅"     # true; each carries its own default color from
upstream_gone = "⌫"         # the theme, which a table override replaces
upstream_untracked = "↑"
add_project = "+"           # the eight action-button icons in the sidebar's
new_worktree = "+"          # chrome — each is independent even where the
new_session = "+"           # default glyph is shared, e.g. styling
remove_project = "×"        # delete_worktree red leaves close_session and
delete_worktree = "×"       # remove_project alone, and reorder is separate
close_session = "×"         # from upstream_diverged despite matching "⇅"
refresh = "↻"
reorder = "⇅"

[ui.drop]                   # what dragging files onto the window does
enabled       = true        # master switch; false ignores every drop
terminal      = true        # paste the paths into the shell
sidebar       = true        # a folder dropped on the projects sidebar becomes a project
scratchpad    = true        # insert the path into an open scratchpad tab
quote         = "auto"      # "auto" (POSIX inside a distro, otherwise the host
                            # default), or "none" / "spaces_only" / "posix" /
                            # "windows" / "windows_always_quoted" to force one.
                            # A path rewritten for WSL is always POSIX-quoted.
                            # No mode escapes shell metacharacters beyond what
                            # the receiving OS needs: "posix" is the only one
                            # that makes an arbitrary filename inert.
wsl_translate = true        # rewrite C:\x as /mnt/c/x for a WSL session
highlight     = true        # tint the region a drop would land on

[ui.paste]                  # what Paste does when the clipboard holds no text
files       = true          # paste the paths of files and folders copied in a
                            # file manager, as Windows Terminal does
image       = true          # write a clipboard bitmap (a Win+Shift+S capture)
                            # to a PNG and paste its path
image_dir   = "~/shots"     # where those PNGs go (default: the per-user cache
                            # on Unix; %TEMP%/alacritree/clipboard on Windows).
                            # A directory you name here is never swept — set it
                            # and you keep every image and clean up yourself
image_keep  = 20            # how many PNGs the default directory keeps.
                            # Minimum 1 — the image a paste just handed to the
                            # shell always survives the sweep

[ui.herdr]                  # agents running under a herdr server, listed in
                            # the sidebar under the worktree each agent's
                            # working directory matches
enabled          = true     # false does no herdr work at all: no polling,
                            # no rows
poll_interval_ms = 2000     # how often a reachable server is asked for its
                            # agents. A side with no herdr on it is given up on
                            # after one attempt, so a machine without herdr
                            # pays a single failed spawn; a side that has one
                            # is retried even while its server is down
show_unmatched   = true     # list an agent whose directory matches no
                            # worktree under Home; false hides it instead

[workspace]
worktree_dir = "~/dev/worktrees"   # base dir for new worktrees (default ~/.alacritree/worktrees)

[[workspace.overrides]]            # optional per-project override
project = "~/Git/github/alacritree"
worktree_dir = "D:/wt"

[wsl]                       # how the app talks to distros, not presentation
resident_helper = true      # keep one helper process per distro for foreground
                            # probes, batched git queries, and tool discovery
                            # (default true). false restores one-shot wsl.exe
                            # spawns; WSL sessions then always report "no TUI",
                            # so FocusLeft/FocusRight always move panel focus
automount_root = "/mnt"     # distro-side mount point for Windows drives,
                            # mirroring wsl.conf's [automount] root. Only
                            # applies to paths alacritree translates itself;
                            # `wsl.exe --cd` uses the distro's real mount table
                            # either way. Supersedes the older [ui.wsl] key

[window]
opacity = 0.92   # restart required — transparency is a ViewportBuilder flag
```

Text always wins: a clipboard carrying both text and an image pastes the text.
Only the regular clipboard is checked for files and images — the X11 PRIMARY
selection is text, so middle-click paste is unchanged. Both options `false`
restores the original behavior exactly, where a paste with no text does nothing.
A default Unix image directory is private to its owner (`0700`), and generated
files are `0600`. A directory named with `image_dir` keeps its existing sharing
permissions and is never swept, while the generated image files remain `0600`.
A pasted path is quoted and, inside a WSL session, translated exactly as a
dropped one — both follow `[ui.drop]`'s `quote` and `wsl_translate`, so those
keys still apply even with `[ui.drop] enabled = false`. `quote = "posix"` is
the only mode that makes an arbitrary filename inert; see the comment on
`[ui.drop] quote` above for why.

The sidebar cursor used to drop to the first row whenever its own row stopped
being rendered — by a filter, or by deleting a session or worktree. It now
climbs or slides instead, under `sidebar_focus = "preserve"`. There is no
setting that restores the old drop-to-first-row behavior.

Closing a session is the one removal the cursor cannot speak for: `Ctrl+Shift+W`
from the terminal and a shell exiting on its own both originate outside the
sidebar, so there is no cursor to slide. Under `"preserve"` the workspace falls
back to its first session, whichever one closed. `"follow"` lands on the closed
session's neighbour instead — the next one, or the previous when the last
session closed, the same ordinal rule the cursor slides by.

Alacritty's palette, cursor, scrolling, window padding, shell, env, and binding
tables are read by the same `Raw*` structs, so those parts of an existing
`alacritty.toml` carry over. The structs cover the fields alacritree acts on
rather than Alacritty's full schema — the JSON Schema below lists exactly what
a given table accepts.

### Editor support — completion and validation

Alacritree publishes a JSON Schema for everything it reads out of the two
files. Editors that speak the TOML language server — [taplo][taplo] and the
[Even Better TOML][ebt] VS Code extension built on it — use it for key
completion, hover documentation and validation.

Point a config at it by running:

```sh
alacritree schema init                        # the alacritree.toml in use
alacritree schema init path/to/alacritty.toml # or a specific file
```

which prepends a header naming the published schema:

```toml
#:schema https://github.com/mathix420/alacritree/releases/latest/download/alacritree-config.json
```

`latest/download` always resolves to the newest released schema. To validate
against the version you actually run, name your tag instead —
`releases/download/v0.9.0/alacritree-config.json`. A file that already carries
a `#:schema` header is left alone, so the command is safe to re-run.

`alacritree schema` prints the document to stdout if you would rather host it
yourself or point at a local copy; it is also committed at
`schema/alacritree-config.json` in this repository.

Two things worth knowing about what the schema does and does not do:

- **Unknown keys are not errors.** The two files are layers, and
  `alacritty.toml` legitimately carries keys only the real alacritty acts on —
  `[hints]`, `[bell]`, `[mouse]`, `[general] import`. Those get no completion,
  but they are not flagged.
- **Closed-value keys are completed.** `confirm_session_close`, `scrollbar`, `sidebar_focus`, `sidebar_scroll_align`, `search_scope`, `sidebar_tooltips`, `last_session_close`, `path_style.*` and `drop.quote` offer their accepted spellings. A binding's `action` completes from every action alacritree implements but rejects nothing, so an alacritty-only action still validates. Cursor `shape` and `blinking`, where Alacritty accepts more than one spelling for the same value, are deliberately left unconstrained, so a working config is never marked wrong.

[taplo]: https://taplo.tamasfe.dev/
[ebt]: https://marketplace.visualstudio.com/items?itemName=tamasfe.even-better-toml

### Icon styling

Every `[ui.icons]` key takes either a bare glyph string, as shown above, or a
table that styles it further:

```toml
[ui.icons]
upstream_gone = { glyph = "⌫", color = "#ff5555", bold = true, italic = false, size = 8 }
```

`glyph` is optional in table form — a table with no `glyph` key keeps that
icon's default glyph and only applies the styling. `size` is in logical
pixels, measured before `ui_scale`, and is clamped to the space the sidebar
row reserves for that icon, so it cannot grow past its slot. Status markers
and badges — `upstream_gone` above, and 11 other `[ui.icons]` keys covering
worktree/session/home icons, PR badges, and the other upstream states — paint
in a 10 px slot with a 10 px default, so `size` on those can only shrink.
The project expand/collapse arrow and the eight action-button icons
(`add_project`, `new_worktree`, `new_session`, `remove_project`,
`delete_worktree`, `close_session`, `refresh`, `reorder`) paint in a 16 px
slot with a 12 px default, and the sidebar search icon has a slot of its own
tied to the UI font size — all three groups can grow past their defaults.

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

Under the hood this mirrors Alacritty's IPC design, on every platform. On Unix,
the app listens under `$XDG_RUNTIME_DIR/alacritree`, or under
`/run/user/$UID/alacritree` on Linux when that environment variable is absent;
if the runtime path cannot be created, it falls back to the system temporary
directory. On Windows it uses a named pipe at
`\\.\pipe\alacritree-<pid>.sock`. The path is advertised to child PTYs via
`ALACRITREE_SOCKET`, so an agent running *inside* an Alacritree session
automatically targets the instance hosting it. Other clients fall back to
scanning the socket directory, or can pass
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
