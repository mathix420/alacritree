# Configuration reference

Every key alacritree reads from `alacritty.toml` and `alacritree.toml`, with its type and default. Generated from the JSON Schema; regenerate with `ALACRITREE_UPDATE_SCHEMA=1 cargo test -p alacritree --test config_schema`.

## `[colors]`

Terminal palette: the sixteen ANSI colors plus the primary, cursor and selection pairs.

- `draw_bold_text_with_bright_colors` (boolean, default `false`): Draw bold text with the bright color variants.

### `[colors.bright]`

The eight bright ANSI colors (8–15).

- `black` (string, default `"#6b6b6b"`): ANSI color 0.
- `blue` (string, default `"#82b8c8"`): ANSI color 4.
- `cyan` (string, default `"#93d3c3"`): ANSI color 6.
- `green` (string, default `"#aac474"`): ANSI color 2.
- `magenta` (string, default `"#c28cb8"`): ANSI color 5.
- `red` (string, default `"#c55555"`): ANSI color 1.
- `white` (string, default `"#f8f8f8"`): ANSI color 7.
- `yellow` (string, default `"#feca88"`): ANSI color 3.

### `[colors.cursor]`

Colors the cursor is drawn with.

- `background` (string): Alias for `cursor`.
- `cursor` (string): Background block color. Alacritty calls this `cursor`; we accept both.
- `foreground` (string): Alias for `text`.
- `text` (string): Foreground glyph color. Alacritty calls this `text`; we accept both.

### `[colors.dim]`

The eight dim ANSI colors. Unset derives them from `normal`.

- `black` (string): ANSI color 0.
- `blue` (string): ANSI color 4.
- `cyan` (string): ANSI color 6.
- `green` (string): ANSI color 2.
- `magenta` (string): ANSI color 5.
- `red` (string): ANSI color 1.
- `white` (string): ANSI color 7.
- `yellow` (string): ANSI color 3.

### `[[colors.indexed_colors]]`

Overrides within the 16–255 range of the 256-color palette. Unlisted indices keep their standard values.

- `color` (string): The color that slot takes.
- `index` (integer): Palette slot to override, 16–255.

### `[colors.normal]`

The eight normal ANSI colors (0–7).

- `black` (string, default `"#181818"`): ANSI color 0.
- `blue` (string, default `"#6a9fb5"`): ANSI color 4.
- `cyan` (string, default `"#75b5aa"`): ANSI color 6.
- `green` (string, default `"#90a959"`): ANSI color 2.
- `magenta` (string, default `"#aa759f"`): ANSI color 5.
- `red` (string, default `"#ac4242"`): ANSI color 1.
- `white` (string, default `"#d8d8d8"`): ANSI color 7.
- `yellow` (string, default `"#f4bf75"`): ANSI color 3.

### `[colors.primary]`

Default foreground and background, plus the bright and dim foreground variants.

- `background` (string, default `"#181818"`): Default background color.
- `bright_foreground` (string): Foreground for bold text, used only when `draw_bold_text_with_bright_colors` is `true`. Unset uses `foreground`.
- `dim_foreground` (string): Foreground for dimmed text. Unset derives it from `foreground`.
- `foreground` (string, default `"#d8d8d8"`): Default text color.

### `[colors.selection]`

Colors a selection is drawn with.

- `background` (string): Alias for `cursor`.
- `cursor` (string): Background block color. Alacritty calls this `cursor`; we accept both.
- `foreground` (string): Alias for `text`.
- `text` (string): Foreground glyph color. Alacritty calls this `text`; we accept both.

## `[cursor]`

Cursor shape, blinking, and how it renders when unfocused.

- `blink_interval` (integer, default `750`): How long one show or one hide of the blink lasts, in milliseconds. Anything under 10 is raised to 10, which is where alacritty stops calling it a blink.
- `blink_timeout` (integer, default `5`): Seconds of blinking after which the cursor is left solid; `0` blinks forever. A timeout that would cut the first blink short is stretched to one full show and hide.
- `style` (string or table): Cursor shape and blinking. Older alacritty configs write just `style = "Block"` rather than `style.shape = "Block"`; both are accepted.
  - `blinking` (string): `"Never"`, `"Off"`, `"On"` or `"Always"`. Lowercase spellings are accepted too. `Never` and `Always` overrule what the running program asks for; `Off` and `On` only choose the starting state.
  - `shape` (string): `"Block"`, `"Underline"`, `"Beam"`, `"HollowBlock"` or `"Hidden"`. Lowercase spellings are accepted too.
- `unfocused_hollow` (boolean, default `true`): Render the cursor as a hollow box while the window is not focused.

## `[debug]`

Diagnostics written to disk.

- `crash_log` (boolean, default `true`): Write an artifact when the process panics. alacritree-only, so it belongs in `alacritree.toml`. A crash that leaves no record is the failure this exists to prevent.
- `frame_log` (boolean, default `false`): Measure whole frames and report the period, CPU time, grid share and keystroke echo every few seconds. alacritree-only, so it belongs in `alacritree.toml`. `ALACRITREE_FRAME_LOG` wins over this key both ways: `1` turns measurements on, `0` and the empty string turn them off. The variable is the only switch available before the config is read. Keeps this session's log file for as long as it is on. The report goes to the log stream, and a GUI-subsystem binary has no console.
- `gpu_timing` (boolean, default `false`): Log what the GPU grid's paint callback costs: the wall time of issuing a frame, and the GPU's own time for the upload and each of the three draws. alacritree-only, so it belongs in `alacritree.toml`. Timer queries are cheap but not free, and the line is only meaningful to someone reading it. Needs `[ui] gpu_grid` and a GL 3.3 context. Keeps this session's log file for as long as it is on, since the report has nowhere else to go.
- `log_dir` (string): Where crash artifacts and session logs are written. alacritree-only, so it belongs in `alacritree.toml`. A leading `~` expands to the home directory; a relative path is ignored. Unset writes to the machine-local state directory: `%LOCALAPPDATA%\ alacritree` on Windows, `$XDG_STATE_HOME/alacritree` or `~/.local/state/alacritree` elsewhere. Logs stay out of the config directory, which on Windows roams between machines. Setting this moves no log already written, and a panic during config parsing still lands in the default directory: the crash hook is armed before this key can be read.
- `persistent_logging` (boolean, default `false`): Keep the log file after quitting. Upstream's name and upstream's default.

## `[env]`

Environment variables added to every process alacritree spawns, including the shell. Entries here may override variables alacritree sets itself.


## `[font]`

The terminal grid's font: the four faces, size, cell offsets, and alacritree's fallback chain.

- `builtin_box_drawing` (boolean, default `true`): Draw box-drawing (U+2500–U+259F), legacy computing (U+1FB00–U+1FB3B) and Powerline (U+E0B0–U+E0BF) characters with the built-in renderer instead of the font.
- `color_glyph_cache_mb` (integer, default `10`): Budget in megabytes for the rasterized colour-glyph cache. The cache is already bounded by how many codepoints the colour fonts cover, but that ceiling moves with cell size and with the fallback list.
- `color_glyphs` (boolean, default `true`): Draw emoji from their font's colour tables. Turning this off falls through to the first fallback face with ordinary outlines, so emoji render monochrome. Also alacritree-only, so it belongs in `alacritree.toml` alongside `fallback`.
- `fallback` (array of string, default `[]`): Ordered list of fallback font families or font file paths, tried in order after the four primary faces and before the automatic system chain. Recommended home is `alacritree.toml`: upstream alacritty warns about unknown keys, so putting it in the shared `alacritty.toml` would make the real alacritty noisy.
- `size` (number, default `11.25`): Font size in points.

### `[font.bold]`

The bold face. An unset family falls back to `normal`'s.

- `family` (string): Family name as the system font database spells it, e.g. `"JetBrainsMono Nerd Font"`.
- `style` (string): Style within the family, e.g. `"Regular"`, `"Bold"`, `"Italic"`.

### `[font.bold_italic]`

The bold-italic face. An unset family falls back to `normal`'s.

- `family` (string): Family name as the system font database spells it, e.g. `"JetBrainsMono Nerd Font"`.
- `style` (string): Style within the family, e.g. `"Regular"`, `"Bold"`, `"Italic"`.

### `[font.glyph_offset]`

Where the glyph sits inside its cell, in pixels. Increasing `x` moves it right, increasing `y` moves it up. Built-in glyphs ignore this, matching alacritty.

- `x` (integer, default `0`): Horizontal offset in pixels.
- `y` (integer, default `0`): Vertical offset in pixels.

### `[font.italic]`

The italic face. An unset family falls back to `normal`'s.

- `family` (string): Family name as the system font database spells it, e.g. `"JetBrainsMono Nerd Font"`.
- `style` (string): Style within the family, e.g. `"Regular"`, `"Bold"`, `"Italic"`.

### `[font.normal]`

The face ordinary text is drawn with.

- `family` (string): Family name as the system font database spells it, e.g. `"JetBrainsMono Nerd Font"`.
- `style` (string): Style within the family, e.g. `"Regular"`, `"Bold"`, `"Italic"`.

### `[font.offset]`

Extra space around each cell in pixels: `y` is line spacing, `x` is letter spacing.

- `x` (integer, default `0`): Horizontal offset in pixels.
- `y` (integer, default `0`): Vertical offset in pixels.

## `[general]`

Options that fit no other table.

- `ipc_socket` (boolean, default `true`): Offer the local socket that `alacritree <command>` and the MCP bridge connect to.
- `state_dir` (string): Where alacritree keeps what it remembers between runs: `state.toml` (project roots, expanded rows, sidebar visibility, per-worktree base branches) and the per-workspace scratchpad notes. alacritree-only, so it belongs in `alacritree.toml`. A leading `~` expands to the home directory; a relative path is ignored. Unset keeps the per-user config base, where these files have always lived: `%APPDATA%\alacritree` on Windows, `$XDG_CONFIG_HOME/alacritree` or `~/.config/alacritree` elsewhere. Setting this moves nothing. The old state and notes stay where they are and the new directory starts empty, so move the files across yourself if you want them. Every alacritree on the machine needs the same value: the CLI resolves this key the way the window does, so a command run against a different config reads a state file the window is not writing.
- `working_directory` (string): Directory sessions on the home tab start in; worktree tabs always start in their checkout. A leading `~` expands to the home directory. Unset inherits the launching process's directory.

## `[integrations]`

The other tools alacritree can notice and cooperate with. `alacritree.toml` only.


### `[integrations.delta]`

The pager the delta diff viewer runs.

- `path` (string, default `"delta"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

### `[integrations.diff_viewer]`

What the git panel's diff pane runs.

- `button_icon` (string, default `"review"`): The glyph or word the section header button shows.
- `preset` ("delta" | "tuicr" | "custom", default `"delta"`): "delta" pipes git's diff through delta. "tuicr" opens tuicr's review TUI, which saves each comment for agents to read. "custom" runs `[integrations.diff_viewer.custom]`.
- `section_buttons` (boolean, default `false`): Draw a button on each git panel section header that opens the whole section in the viewer. The ReviewStaged, ReviewUnstaged and ReviewBranch actions work either way.

### `[integrations.diff_viewer.custom]`

The viewer `preset = "custom"` runs.

- `branch` (array of string, default `[]`): Arguments for a `Changes vs` row. `{file}` is the row's path and `{base}` the branch it diffs against.
- `branch_scope` (array of string, default `[]`): Arguments for the `Changes vs` section header. `{base}` is the branch it diffs against.
- `pager` (string, default `""`): Pager mode: a command git runs as `core.pager` for the panel's own `git diff`. Set this or `path`, never both.
- `path` (string, default `""`): Direct mode: a program that renders the diff itself, run with the argument list below that matches what was chosen. An empty list makes that row kind or section open nothing.
- `staged` (array of string, default `[]`): Arguments for a staged row. `{file}` is the row's path.
- `staged_scope` (array of string, default `[]`): Arguments for the Staged section header.
- `unstaged` (array of string, default `[]`): Arguments for an unstaged row. `{file}` is the row's path.
- `unstaged_scope` (array of string, default `[]`): Arguments for the Unstaged section header.
- `untracked` (array of string, default `[]`): Arguments for an untracked row. `{file}` is the row's path.
- `wsl_pager` (string, default `""`): Pager mode inside WSL: the command git runs as `core.pager` there. Empty runs `pager` through the distro's login shell.
- `wsl_path` (string, default `""`): Direct mode inside WSL: the program to run there, as written. Empty runs `path` through the distro's login shell, which finds a bare name.

### `[integrations.doppler]`

The Doppler CLI behind scope mirroring for new worktrees.

- `path` (string, default `"doppler"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

### `[integrations.gh]`

The GitHub CLI behind PR badges and diff base branches.

- `path` (string, default `"gh"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `pr_status` (boolean, default `false`): Poll `gh` for each branch's open pull request, which drives the PR row icons, the PR-state filters, and `$pr` in row templates.
- `pr_status_concurrency` (integer): Max `gh` lookups in flight at once. Unset lets the pool decide, which is one below its own background ceiling so a lookup can never take the last slot local work needs. A value lowers that; nothing raises it, because the pool's ceiling binds underneath either way.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

### `[integrations.git]`

The git CLI, for the commands alacritree spawns. Repository reads go through libgit2, and scripts inside WSL find git on that distro's PATH.

- `path` (string, default `"git"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

### `[integrations.herdr]`

Agents running under a herdr server.

- `attach` ("agent" | "session", default `"agent"`): Whether opening a row attaches to that agent's pane directly ("agent") or to the herdr session around it with the pane focused ("session"). "session" hands the mouse to herdr's own client, where a selection joins soft-wrapped rows and copy mode works; a direct attach is repainted row by row, so the host terminal sees every wrap as a line break. Honoured per side: the native side of a Windows host always attaches to the session, because herdr implements no direct attach there.
- `enabled` (boolean, default `true`): Discover herdr servers and list their agents in the sidebar. Inert when no herdr binary or server is present. Changes arrive on herdr's event stream, read through `herdr remote-api-bridge`. 0.9.1 has it and 0.8.2 does not; a herdr without it lists nothing.
- `follow_focus` ("off" | "herdr" | "always", default `"herdr"`): Whether a focus change made inside herdr moves alacritree to the matching session. "off" never moves it. "herdr" moves it only while the active session is already showing herdr's view, which is what an unmodified config has always done. "always" also moves it from a plain native session, on any reachable side, after a gap in typing.
- `icon` (string or table, default `"􏼀"`): The glyph on a herdr pane's sidebar row and palette entry. A bare string sets the glyph; a table also styles its color, weight, slant and size, the way `[ui.icons]` keys do. The default draws a ram's head from the bundled symbol font. An ordinary character such as `"◫"` is drawn by your own fonts instead.
  - `bold` (boolean, default `false`): Draw the glyph bold.
  - `color` (string): Glyph color. Unset inherits the row's foreground.
  - `glyph` (string): The character to draw. Unset keeps the built-in glyph and applies only the styling.
  - `italic` (boolean, default `false`): Draw the glyph italic.
  - `size` (number): Point size, clamped to a minimum of `1.0`. Unset uses the sidebar font size.
- `path` (string, default `"herdr"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `show_panes` (boolean, default `false`): List every pane a herdr server owns, not only the ones it detected an agent in. A pane running a plain shell gets a row named by its own title, carrying no status, and opening it shares herdr's view of the tab that holds it rather than attaching to the pane. Needs a herdr that knows `pane list` (0.8.2 does). An older one answers with a usage error, which reads as no herdr on that side. A side that has never answered is then abandoned for the process lifetime; one that answered before this was turned on keeps retrying and recovers when it goes back off.
- `show_unmatched` (boolean, default `true`): List panes whose working directory matches no worktree, under Home.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

### `[integrations.taskwarrior]`

Task lists kept in taskwarrior.

- `enabled` (boolean, default `false`): Show task lists kept in taskwarrior in a tab (`OpenTasks`, Ctrl+~). Agents write the same lists with `task` and read them through `alacritree hook`. Off leaves the binding inert and the palette entry out.
- `path` (string, default `"task"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

### `[integrations.tuicr]`

The review TUI the tuicr diff viewer runs.

- `path` (string, default `"tuicr"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

### `[integrations.zellij]`

Panes of running zellij sessions.

- `enabled` (boolean, default `false`): List the panes of every running zellij session in the sidebar. Opening one attaches to its whole session with the pane focused, since zellij has no attach for a single pane.
- `icon` (string or table, default `"􏼁"`): The glyph on a zellij pane's sidebar row and palette entry. A bare string sets the glyph; a table also styles its color, weight, slant and size, the way `[ui.icons]` keys do. The default draws a hexagon from the bundled symbol font. An ordinary character such as `"⬡"` is drawn by your own fonts instead.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `path` (string, default `"zellij"`): The program to run on Windows or natively. Its own name is looked up on PATH; any other value runs as written.
- `poll_interval_ms` (integer, default `2000`): How often each side's zellij sessions are re-listed.
- `session` (string, default `""`): The session a new pane opens in. Empty takes the one session running on that side and refuses when there are several.
- `show_unmatched` (boolean, default `true`): List panes whose working directory matches no worktree, under Home.
- `wsl_path` (string, default `""`): The program to run inside every WSL distro, as written. Empty finds it by name through the distro's login shell.

## `[keyboard]`

Key bindings. Arrays concatenate across the two files, so bindings written in `alacritree.toml` add to the shared ones rather than replacing them.


### `[[keyboard.bindings]]`

Key bindings, as `[[keyboard.bindings]]` entries. Vi- and search-mode bindings are accepted and ignored: alacritree tracks neither mode.

- `action` ("Paste" | "PasteSelection" | "Copy" | "CopySelection" | "ScrollPageUp" | "ScrollPageDown" | "ScrollHalfPageUp" | "ScrollHalfPageDown" | "ScrollLineUp" | "ScrollLineDown" | "ScrollToTop" | "ScrollToBottom" | "ClearHistory" | "SpawnNewInstance" | "IncreaseFontSize" | "DecreaseFontSize" | "ResetFontSize" | "ToggleFullscreen" | "ToggleMaximized" | "Minimize" | "SelectNextTab" | "SelectPreviousTab" | "SelectLastTab" | "SelectNextSession" | "SelectPreviousSession" | "SelectNextWorkspace" | "SelectPreviousWorkspace" | "OpenScratchpad" | "ToggleLeftSidebar" | "ToggleRightSidebar" | "AddProject" | "ToggleSidebarFocus" | "CloseSession" | "CloseExitedSession" | "NewMultiplexerPane" | "AttachAllMultiplexerPanes" | "DetachAllMultiplexerPanes" | "SidebarTop" | "SidebarBottom" | "SidebarNextProject" | "SidebarPreviousProject" | "FocusProjectsSidebar" | "FocusGitSidebar" | "FocusTerminal" | "FocusLeft" | "FocusRight" | "ToggleSessionRows" | "ToggleSessionTabs" | "ToggleSessionDrag" | "MoveSessionUp" | "MoveSessionDown" | "SetBaseBranch" | "SidebarSearchConfirm" | "SidebarSearchCancel" | "SidebarSearchCancelToTerminal" | "Quit" | "TogglePalette" | "PaletteTop" | "PaletteBottom" | "PalettePageUp" | "PalettePageDown" | "None" | "ReceiveChar" | "ToggleSessionsFilter" | "ToggleDetachedSessionsFilter" | "ToggleAttentionFilter" | "TogglePrOpenFilter" | "TogglePrDraftFilter" | "TogglePrMergedFilter" | "TogglePrClosedFilter" | "ClearProjectFilters" | "ToggleModifiedFilter" | "ToggleDeletedFilter" | "ToggleUntrackedFilter" | "ClearGitFilters" | "ToggleSearchScope" | "RefreshPrStatus" | "ReviewStaged" | "ReviewUnstaged" | "ReviewBranch" | "RefreshProjects" | "DeleteSelected" | "RenameSelected" | "ToggleProjectExpanded" | "SelectTab1" | "SelectTab2" | "SelectTab3" | "SelectTab4" | "SelectTab5" | "SelectTab6" | "SelectTab7" | "SelectTab8" | "SelectTab9" | "SpawnProfile1" | "SpawnProfile2" | "SpawnProfile3" | "SpawnProfile4" | "SpawnProfile5" | "SpawnProfile6" | "SpawnProfile7" | "SpawnProfile8" | "SpawnProfile9" or string): Named action to run, e.g. `"Paste"`, `"ToggleLeftSidebar"`. The schema suggests every name alacritree implements without rejecting the rest: the shared `alacritty.toml` legitimately carries actions only the real alacritty implements, and alacritree ignores those rather than rejecting them. `docs/keyboard-shortcuts.md` says what each one does.
- `chars` (string): Bytes to write to the PTY, with the usual escapes (`\x1b`, `\u001b`).
- `command` (string or table): External program to run. alacritree parses it so the binding still displaces alacritty's default for that key, but never runs it.
  - `args` (array of string, default `[]`): Arguments passed to the program. Optional.
  - `program` (string): Path to the program.
- `key` (string): The key, as alacritty spells it: a character (`"A"`), a named key (`"F5"`, `"PageUp"`), or a scancode. A key alacritree cannot map is dropped with a warning.
- `mode` (string): Terminal mode the binding applies in, e.g. `"Vi"` or `"~Search"`. alacritree tracks neither mode, so a binding with a `mode` is read and ignored.
- `mods` (string): Modifiers held with the key, joined by `|`: `"Control"`, `"Shift"`, `"Alt"`, `"Super"`. Unset means no modifiers.

## `[mouse]`

What the mouse pointer does around the grid.

- `hide_when_typing` (boolean, default `false`): Hide the mouse pointer while typing. The next pointer motion, click or wheel tick brings it back.

## `[scrolling]`

Scrollback depth and mouse-wheel step.

- `history` (integer, default `10000`): Maximum number of lines kept in the scrollback buffer.
- `multiplier` (integer, default `3`): Lines scrolled per mouse-wheel increment.

## `[selection]`

What counts as a word when double-clicking, and whether a selection reaches the clipboard on its own.

- `save_to_clipboard` (boolean, default `false`): Copy selected text to the system clipboard as soon as it is selected.
- `semantic_escape_chars` (string, default `",│`|:\"' ()[]{}<>\t"`): Characters that separate "semantic words" for double-click selection.

## `[terminal]`

The program each session runs.

- `shell` (string or table): The program each session runs, as either a bare path or a table with arguments. Unset uses `$SHELL` (the login shell as a fallback) on Unix and PowerShell on Windows.
  - `args` (array of string, default `[]`): Arguments passed to the program.
  - `program` (string): Path to the program, e.g. `"/bin/zsh"`.

## `[ui]`

alacritree's own presentation: sidebar colors, icons, tooltips, shell profiles, and everything else the terminal grid does not own. Belongs in `alacritree.toml` — upstream alacritty warns about it.

- `async_session_spawn` (boolean, default `false`): Open a session's PTY on a worker rather than in the frame that asked for it, so spawning does not stutter.
- `attention_grace_ms` (integer, default `0`): Grace window in milliseconds before an attention trigger pings; a session that resumes work inside it swallows the ping.
- `confirm_session_close` ("never" | "busy" | "always", default `"never"`): When the sidebar × on a session row asks before killing the PTY: "never" | "busy" | "always".
- `confirm_session_detach` (boolean, default `true`): Whether the sidebar × on a harness-managed row asks before detaching, and whether DetachAllMultiplexerPanes asks once for the whole batch. Separate from `confirm_session_close` because a detach leaves the pane running and its row listed again.
- `default_profile` (string): Name of the profile new sessions use. Must match a `[[ui.profiles]]` entry, or it is ignored with a warning.
- `delta_path` (string): Deprecated. This value applies on each side while `[integrations.delta]` `path` or `wsl_path` has its built-in value. Remove it after migration.
- `focus_priority_boost` (boolean, default `false`): Put the session on screen, its shell and every process that shell starts, one scheduling class above normal, so a busy machine cannot starve what the user is typing into. Follows focus. Windows only; changing it requires a restart.
- `gpu_grid` (boolean, default `false`): Draw the terminal grid through an OpenGL paint callback instead of handing epaint a mesh. It needs a GL 3 context and bypasses the renderer every other panel goes through, so an unmodified config keeps the path that has always drawn the grid. A context too old for instanced arrays logs once and paints the mesh from the next frame on.
- `hold_exited_sessions` ("never" | "on_error" | "always", default `"never"`): Whether a session whose child has exited stays on screen instead of closing with it: "never" | "on_error" | "always". A held session writes one line into its own grid naming the key that closes it. A herdr attach that was refused is held whatever this says, since its refusal message is the only report of what happened.
- `icon_tooltips` (boolean, default `true`): Whether a sidebar icon explains itself on hover.
- `last_session_close` ("respawn" | "navigate" | "ring_global" | "ring_project", default `"respawn"`): What happens when the on-screen workspace stops having sessions, whether a close or a worktree deletion took the last one: "respawn" | "navigate" | "ring_global" | "ring_project".
- `notifications` (boolean, default `true`): Post a desktop notification when a hidden session rings the bell; clicking it focuses that session.
- `pr_status` (boolean): Deprecated. This value applies only while `[integrations.gh] pr_status` is omitted. Remove it after migration.
- `pr_status_concurrency` (integer): Deprecated. This value applies only while `[integrations.gh]` `pr_status_concurrency` is omitted. Remove it after migration.
- `project_name` (string): Template for a project row's label, taking `$name` and `$path`. A manual rename always wins over it.
- `reap_descendants_on_close` (boolean, default `false`): End everything a session started when that session closes, at any depth, except processes that ask to break away. Windows only; changing it requires a restart.
- `scrollbar` ("floating" | "solid", default `"floating"`): Sidebar scrollbar style: "floating" | "solid".
- `search_depth` ("workspaces" | "sessions", default `"workspaces"`): How far a sidebar query reaches: "workspaces" matches project and worktree names only; "sessions" also matches session titles and herdr agent names.
- `search_scope` ("filtered" | "all", default `"filtered"`): Whether a fuzzy query is confined by the panel's active toggle filters: "filtered" | "all".
- `sessions_filter_counts_detached` (boolean, default `false`): Whether the sidebar's sessions toggle counts an unattached herdr row the same as a live session: an agent nothing is attached to, and, once `show_panes` is on, an agentless pane. Off keeps the toggle's original session-only behavior.
- `sidebar_accent` (string): Accent for selected rows and focus outlines. Unset uses the palette's `normal.blue`.
- `sidebar_attention` (string): Badge color for a session asking to be looked at. Unset uses the palette's `normal.yellow`.
- `sidebar_background` (string): Sidebar background. Unset derives it from the terminal palette.
- `sidebar_border` (string): Color of the line between a sidebar and the terminal.
- `sidebar_click_focus` (boolean, default `false`): Clicking a sidebar moves keyboard focus to it.
- `sidebar_focus` ("preserve" | "follow", default `"preserve"`): How far the projects sidebar goes when the cursor's row stops being rendered: "preserve" | "follow".
- `sidebar_follow_active` (boolean, default `false`): Whether the projects sidebar scrolls to the session on screen whenever it changes — a cycling key, a click, the palette, an IPC request. The sidebar cursor is left where it was.
- `sidebar_foreground` (string): Sidebar text color. Unset derives it from the terminal palette.
- `sidebar_scroll_align` ("minimal" | "center", default `"minimal"`): Where a row the sidebar scrolled to is parked: "minimal" | "center". Under "center" every cursor step re-centres the list, and clicking a row near the panel edge scrolls it out from under the pointer.
- `sidebar_tooltips` ("off" | "elided" | "always", default `"elided"`): When a sidebar row spells its full name out on hover: "elided" | "always" | "off".
- `status_indicators` ("dots" | "symbols", default `"dots"`): Glyph set for agent status marks, on native and multiplexer rows alike: "dots" | "symbols". Dots draws two same-sized circles, hollow and filled, and tells states apart by colour; symbols by shape. `[ui.icons]` overrides one state at a time.
- `upstream_status` (boolean, default `false`): Paint a badge on each worktree row for its branch's upstream state. Local refs only: nothing fetches, so a branch deleted on the remote reads as tracked until something prunes locally. A linked worktree that overrides `branch.*` in its own `config.worktree` is read from the project root, so that override is not seen.
- `vsync` (boolean, default `true`): Wait for the display's refresh before showing a finished frame. Off trades tearing for lower keystroke-to-screen delay. Changing it requires a restart.
- `worktree_liveness` (boolean, default `true`): Re-check on a 1.5 s tick whether each listed worktree's checkout is still on disk, so a `git worktree remove` typed into one of our own sessions greys the row without waiting for a manual refresh. The probe is one `stat` per listed row, which an exotic filesystem could make expensive.
- `worktree_name` (string): Template for a worktree row's label, e.g. `"$branch $pr"`. Takes `$name`, `$branch`, `$path`, `$pr` (as `#123`, needs `[integrations.gh] pr_status`) and `${var:fallback}`. Unset keeps the plain worktree name.

### `[ui.cursor]`

Cursor movement, which alacritty's `[cursor]` has no keys for.

- `animate` (boolean, default `false`): Glide the cursor to its new cell instead of redrawing it there.
- `animation_min_cells` (integer, default `2`): Cells the cursor has to jump before the glide is worth playing. Below this it snaps, which keeps ordinary typing from smearing.
- `animation_ms` (integer, default `80`): How long that glide takes, in milliseconds. Zero draws every cell directly, the same as leaving `animate` off.

### `[ui.decorations]`

Corrections to the underline and strikeout the font placed ([`RawDecorations`]).

- `strikeout_position` (string, default `"0px"`): Shift or scale of how far the strikeout sits from the top of the cell.
- `strikeout_thickness` (string, default `"0px"`): Shift or scale of the strikeout bar's weight.
- `underline_position` (string, default `"0px"`): Shift or scale of how far the underline sits from the top of the cell, for the straight, dotted and dashed styles. The double and curly styles are placed from the font's descent instead, so this knob does not reach them.
- `underline_thickness` (string, default `"0px"`): Shift or scale of the underline's stroke weight. Every style draws with this value, including double and curly.

### `[ui.drop]`

What a file dragged onto the window does.

- `enabled` (boolean, default `true`): Accept dropped files at all. `false` turns every target off.
- `highlight` (boolean, default `true`): Highlight the target a drag is over.
- `quote` ("auto" | "none" | "spaces_only" | "posix" | "windows" | "windows_always_quoted", default `"auto"`): How a path is quoted for the shell that receives it. `"auto"` is POSIX inside a distro and the host's own style elsewhere; the five concrete modes are wezterm's `quote_dropped_files` values. Only `"posix"` makes an arbitrary filename inert.
- `scratchpad` (boolean, default `true`): Write a dropped file's path into the workspace scratchpad.
- `sidebar` (boolean, default `true`): Let a file dropped on the projects sidebar add its repository.
- `terminal` (boolean, default `true`): Write a dropped file's path into the terminal.
- `wsl_translate` (boolean, default `true`): Rewrite a Windows path to its distro spelling when the session runs inside WSL.

### `[ui.focus_outline]`

Outline drawn around whichever pane holds keyboard focus.

- `color` (string): Outline color. Unset uses the sidebar accent.
- `sidebar` (boolean, default `false`): Outline the sidebar when it holds keyboard focus.
- `terminal` (boolean, default `false`): Outline the terminal when it holds keyboard focus.
- `thickness` (number, default `1.0`): Outline thickness in pixels.

### `[ui.font]`

The font sidebars, tabs and dialogs are drawn with.

- `bold_family` (string): Family used where the sidebar draws bold. Unset uses `family`.
- `bold_italic_family` (string): Family used where the sidebar draws bold italic. Unset uses `family`.
- `builtin_symbols` (boolean, default `true`): Draw the sidebar's own symbols from the bundled subset rather than from the configured family, so a font missing them still renders.
- `family` (string): Family for sidebars, tabs and dialogs. Unset uses the terminal font.
- `italic_family` (string): Family used where the sidebar draws italic. Unset uses `family`.
- `size` (number): Point size for the sidebar font.

### `[ui.icons]`

Sidebar glyph overrides.

- `add_project` (string or table, default `"+"`): The "add project" button.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `agent_blocked` (string or table): An agent held at a dialog it needs a human to answer. Unset follows `[ui] status_indicators`: `⬤` for dots, `×` for symbols.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `agent_done` (string or table): An agent that finished a turn while nobody was looking. Unset follows `[ui] status_indicators`: `⬤` for dots, `✓` for symbols.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `agent_idle` (string or table): An agent waiting with nothing in flight. Unset follows `[ui] status_indicators`: `◯` in both sets.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `agent_unknown` (string or table): An agent nothing could read a state from. Unset follows `[ui] status_indicators`: `◯` for dots, `?` for symbols.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `agent_working` (string or table): An agent at work. Unset draws the braille loader; a glyph replaces it.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `attention` (string or table): A session that rang while nobody was looking. Unset draws `⬤` in both sets.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `close_session` (string or table, default `"×"`): The "close session" button.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `delete_worktree` (string or table, default `"×"`): The "delete worktree" button.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `herdr` (string or table): Deprecated. This value applies only while `[integrations.herdr] icon` is omitted. Remove it after migration.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `home` (string or table, default `"⌂"`): The home tab, whose sessions inherit the launch directory.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `new_session` (string or table, default `"+"`): The "new session" button.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `new_worktree` (string or table, default `"+"`): The "new worktree" button.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `pr_closed` (string or table, default `"⬤"`): A branch whose pull request was closed unmerged.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `pr_draft` (string or table, default `"◯"`): A branch whose pull request is a draft.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `pr_merged` (string or table, default `"⬤"`): A branch whose pull request was merged.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `pr_open` (string or table, default `"⬤"`): A branch with an open pull request.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `project_collapsed` (string or table, default `"▸"`): A collapsed project.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `project_expanded` (string or table, default `"▾"`): An expanded project.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `refresh` (string or table, default `"↻"`): The "refresh" button.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `remove_project` (string or table, default `"×"`): The "remove project" button.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `reorder` (string or table, default `"⇅"`): The drag handle a row is reordered by.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `search` (string or table, default `"⌕"`): The panel search box.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `session` (string or table, default `"▪"`): A terminal session row.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `upstream_diverged` (string or table, default `"⇅"`): A branch that has both moved ahead of and fallen behind its upstream.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `upstream_gone` (string or table, default `"⌫"`): A branch whose upstream no longer exists locally.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `upstream_level` (string or table, default `"✓"`): A branch level with its upstream.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `upstream_untracked` (string or table, default `"↑"`): A branch that tracks nothing.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `worktree` (string or table, default `"○"`): A linked worktree.
  - The table form takes the same keys as `integrations.herdr.icon`.
- `worktree_main` (string or table, default `"●"`): A project's main checkout.
  - The table form takes the same keys as `integrations.herdr.icon`.

### `[ui.paste]`

What the clipboard's non-text contents paste as.

- `files` (boolean, default `true`): Paste files held on the clipboard as their paths.
- `image` (boolean, default `true`): Paste an image held on the clipboard by writing it to a file and pasting that path.
- `image_dir` (string): Where pasted images are written. Unset uses a cache directory.
- `image_keep` (integer, default `20`): How many pasted images to keep before the oldest are removed, at least one. A directory named by `image_dir` is never swept.

### `[ui.path_style]`

How paths are abbreviated where the UI writes them.

- `diff_title` ("full" | "fish" | "zed", default `"full"`): "full" | "fish" | "zed", per site. The diff pane's title.
- `git_header` ("full" | "fish" | "zed", default `"full"`): The path in the git panel's header.
- `git_rows` ("full" | "fish" | "zed", default `"full"`): Paths in the git panel's file rows.

### `[ui.path_style.filename]`

How the last path segment is emphasized.

- `bold` (boolean, default `false`): Draw bold.
- `color` (string): Text color. Unset inherits the row's foreground.
- `italic` (boolean, default `false`): Draw italic.

### `[ui.path_style.parent]`

How the leading path segments are emphasized.

- `bold` (boolean, default `false`): Draw bold.
- `color` (string): Text color. Unset inherits the row's foreground.
- `italic` (boolean, default `false`): Draw italic.

### `[[ui.profiles]]`

Named shell launch profiles, offered when starting a session.

- `args` (array of string, default `[]`): Arguments passed to `program`.
- `name` (string): Name shown in the session picker and matched by `default_profile`.
- `program` (string): Program the profile launches.

### `[ui.session_display]`

Whether per-session rows and tabs appear before a workspace has two sessions.

- `palette_marks` (boolean, default `false`): Paint a session's sidebar status mark in its command-palette row too.
- `sidebar_always` (boolean, default `false`): Show a workspace's sidebar session row even with a single session.
- `tabs_always` (boolean, default `false`): Draw a tab-strip segment even with a single session.

### `[ui.session_reorder]`

Whether session rows can be dragged, and how far a reorder may carry a session.

- `drag` (boolean, default `false`): Let a session row be dragged with the mouse to reorder it.
- `scope` ("workspace" | "project" | "anywhere", default `"workspace"`): How far a reorder may carry a session: "workspace" | "project" | "anywhere".

### `[ui.wsl]`

Deprecated WSL options, superseded by the top-level `[wsl]` table.

- `automount_root` (string): Deprecated location: `[wsl] automount_root` supersedes this and wins when both are set; kept so existing configs keep working.

## `[window]`

Window padding and background opacity.

- `opacity` (number, default `1.0`): Background opacity from `0.0` (transparent) to `1.0` (opaque). Changing it requires a restart: transparency is a window flag set before the window exists.

### `[window.padding]`

Blank space around the terminal grid, in pixels, added at both opposing sides.

- `x` (number, default `0.0`): Horizontal padding in pixels.
- `y` (number, default `0.0`): Vertical padding in pixels.

## `[workspace]`

Where alacritree creates git worktrees. `alacritree.toml` only.

- `worktree_dir` (string): Where new worktrees are created. `$project` expands to the repository's directory name.

### `[[workspace.overrides]]`

Per-project overrides of `worktree_dir`.

- `project` (string): Path to the project this override applies to.
- `worktree_dir` (string): Where that project's worktrees are created.

## `[wsl]`

How alacritree talks to WSL distros. `alacritree.toml` only.

- `automount_root` (string): Distro-side mount point for Windows drives, mirroring wsl.conf's `[automount] root`. Only used for paths *we* translate (git output from inside a distro); `wsl.exe --cd` translates with the distro's real mount table regardless of this value. Unset means `/mnt`; the key stays optional so the deprecated `[ui.wsl]` spelling can still win when this one is absent.
- `resident_helper` (boolean, default `true`): Keep a resident helper process per distro for foreground probes, batched git queries, and tool discovery. `false` restores one-shot wsl.exe spawns everywhere; WSL sessions then always report "no TUI", so FocusLeft/FocusRight always move panel focus.
