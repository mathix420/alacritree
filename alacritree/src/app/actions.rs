use alacritty_terminal::grid::Scroll;
use enum_dispatch::enum_dispatch;

use super::*;

/// What running a keyboard action does, whether a binding, the palette or IPC
/// asked for it.
///
/// `enum_dispatch` copies this signature into the impl it generates for
/// `NamedAction`, and that impl can land in `bindings.rs`, so the signature
/// names every type by its full path.
#[enum_dispatch]
pub(crate) trait Action {
    fn run(
        &self,
        app: &mut crate::app::AlacritreeApp,
        ctx: &egui::Context,
        origin: crate::app::ActionOrigin,
    );
}

impl AlacritreeApp {
    pub(super) fn dispatch_action(
        &mut self,
        ctx: &Context,
        action: BindingAction,
        origin: ActionOrigin,
    ) {
        // A palette row is dispatched with the panel still searching, and the
        // cursor operations below act on a row the query may have hidden.  The
        // keyboard path cannot reach here mid-query at all: a letter's text is
        // swallowed by the query before the binding table sees the key.
        if origin == ActionOrigin::Palette
            && matches!(&action, BindingAction::Named(n) if n.requires_project_browsing())
            && self.sidebar.filter.mode() != panel_filter::Mode::Browsing
        {
            return;
        }
        match action {
            BindingAction::Chars(bytes) => self.dispatch_chars(ctx, bytes),
            BindingAction::Named(action) => action.run(self, ctx, origin),
            BindingAction::Unsupported(name) => {
                log::debug!("unsupported keyboard binding action: {name}")
            },
        }
    }

    fn dispatch_chars(&mut self, ctx: &Context, bytes: Vec<u8>) {
        if let Some(idx) = self.active_session_index() {
            let id = self.sessions[idx].id;
            if let Some(editor) = self.sessions[idx].scratchpad.as_mut() {
                // Custom `Chars` bindings can carry terminal control
                // sequences (Shift+Tab is ESC [ Z, for example).  A
                // document should only accept actual text here; native
                // editing keys are handled by egui's TextEdit itself.
                if let Ok(text) = std::str::from_utf8(&bytes)
                    && !text.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
                {
                    editor.insert_at_cursor(ctx, id, text);
                }
            } else {
                paste::on_terminal_input_start(&self.sessions[idx]);
                self.sessions[idx].write(bytes);
            }
        }
    }

    /// Copy the scratchpad editor's selection when one is on screen, and the
    /// terminal's otherwise.
    fn copy_active_selection(&mut self, ctx: &Context, target: Target) {
        let Some(idx) = self.active_session_index() else { return };
        if let Some(editor) = self.sessions[idx].scratchpad.as_ref() {
            if let Some(text) = editor.selected_text(ctx, self.sessions[idx].id) {
                clipboard::write(target, &text);
            }
        } else {
            paste::copy_selection(&self.sessions[idx], &self.config, target);
        }
    }
}

impl Action for action::Paste {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.paste_from_clipboard(ctx, Target::Clipboard);
    }
}

impl Action for action::PasteSelection {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.paste_from_clipboard(ctx, Target::Primary);
    }
}

impl Action for action::Copy {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.copy_active_selection(ctx, Target::Clipboard);
    }
}

impl Action for action::CopySelection {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.copy_active_selection(ctx, Target::Primary);
    }
}

impl Action for action::ScrollPageUp {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|_| Scroll::PageUp);
    }
}

impl Action for action::ScrollPageDown {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|_| Scroll::PageDown);
    }
}

impl Action for action::ScrollHalfPageUp {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|lines_per_page| Scroll::Delta(lines_per_page / 2));
    }
}

impl Action for action::ScrollHalfPageDown {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|lines_per_page| Scroll::Delta(-(lines_per_page / 2)));
    }
}

impl Action for action::ScrollLineUp {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|_| Scroll::Delta(1));
    }
}

impl Action for action::ScrollLineDown {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|_| Scroll::Delta(-1));
    }
}

impl Action for action::ScrollToTop {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|_| Scroll::Top);
    }
}

impl Action for action::ScrollToBottom {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.scroll_display(|_| Scroll::Bottom);
    }
}

impl Action for action::ClearHistory {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler};
        if let Some(idx) = app.active_session_index() {
            if app.sessions[idx].scratchpad.is_none() {
                app.sessions[idx].term.lock().clear_screen(ClearMode::Saved);
            }
        }
    }
}

impl Action for action::SpawnNewInstance {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        let ws = app.current_workspace.clone();
        if let Err(e) = app.spawn_session(ctx, ws.clone()) {
            app.report_spawn_failure(ctx, &ws, &e);
        }
    }
}

impl Action for action::ToggleFullscreen {
    fn run(&self, _: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        let on = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!on));
    }
}

impl Action for action::ToggleMaximized {
    fn run(&self, _: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        let on = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!on));
    }
}

impl Action for action::Minimize {
    fn run(&self, _: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }
}

impl Action for action::Quit {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.modals.quit_dialog_open = true;
    }
}

impl Action for action::SelectNextTab {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.cycle_tabs(1);
    }
}

impl Action for action::SelectPreviousTab {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.cycle_tabs(-1);
    }
}

impl Action for action::SelectTab {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.select_tab(self.0);
    }
}

impl Action for action::SelectLastTab {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.select_last_tab();
    }
}

impl Action for action::SelectNextSession {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.cycle_sessions(ctx, 1);
    }
}

impl Action for action::SelectPreviousSession {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.cycle_sessions(ctx, -1);
    }
}

impl Action for action::SelectNextWorkspace {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.cycle_workspaces(ctx, 1);
    }
}

impl Action for action::SelectPreviousWorkspace {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.cycle_workspaces(ctx, -1);
    }
}

impl Action for action::OpenScratchpad {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.toggle_scratchpad_tab(ctx);
    }
}

impl Action for action::OpenTasks {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        if app.config.integrations.taskwarrior.enabled {
            app.toggle_tasks_tab(ctx);
        }
    }
}

impl Action for action::AddProject {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.add_project_via_dialog(ctx);
    }
}

impl Action for action::RefreshProjects {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        app.refresh_all_projects(ctx);
    }
}

impl Action for action::TogglePalette {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.palette.toggle();
    }
}

impl Action for action::FocusTerminal {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.focus_terminal();
    }
}

impl Action for action::FocusLeft {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, origin: ActionOrigin) {
        app.move_focus(FocusDir::Left, origin);
    }
}

impl Action for action::FocusRight {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, origin: ActionOrigin) {
        app.move_focus(FocusDir::Right, origin);
    }
}

impl Action for action::ToggleSessionRows {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.session_rows_always = !app.session_rows_always;
    }
}

impl Action for action::ToggleSessionTabs {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.session_tabs_always = !app.session_tabs_always;
    }
}

impl Action for action::ToggleSessionDrag {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.session_drag = !app.session_drag;
    }
}

impl Action for action::MoveSessionUp {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.step_session(-1);
    }
}

impl Action for action::MoveSessionDown {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.step_session(1);
    }
}

impl Action for action::ToggleDetachedSessionsFilter {
    fn run(&self, app: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {
        app.sessions_filter_counts_detached = !app.sessions_filter_counts_detached;
    }
}

impl Action for action::SpawnProfile {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        let n = self.0;
        match app.config.profiles.get((n - 1) as usize).map(|p| p.name.clone()) {
            Some(name) => app.spawn_profile_session(ctx, &name),
            None => {
                log::warn!(
                    "SpawnProfile{n}: only {} profiles configured",
                    app.config.profiles.len()
                );
                app.modals.error_dialog = Some(format!("SpawnProfile{n}: no such profile"));
            },
        }
    }
}

// No confirmation and no cursor: the child is already gone, so there is
// nothing left to interrupt and nothing to ask about.
impl Action for action::CloseExitedSession {
    fn run(&self, app: &mut AlacritreeApp, ctx: &Context, _: ActionOrigin) {
        if let Some(idx) = app.active_session_index()
            && app.sessions[idx].is_exited()
        {
            let id = app.sessions[idx].id;
            app.close_session(ctx, id);
        }
    }
}

macro_rules! runs_nothing {
    ($($ty:ident),* $(,)?) => {
        $(
            impl Action for action::$ty {
                fn run(&self, _: &mut AlacritreeApp, _: &Context, _: ActionOrigin) {}
            }
        )*
    };
}

// An unbind and alacritty's pass-through marker: the binding table acts on
// both, and running either does nothing.
runs_nothing!(NoOp, ReceiveChar);

// The palette moves its own cursor while it is open, in `consume_palette_keys`.
runs_nothing!(PaletteTop, PaletteBottom, PalettePageUp, PalettePageDown);

// Nothing resizes the font at runtime. The names still parse, so a shared
// alacritty.toml binding them is not reported as unsupported.
runs_nothing!(IncreaseFontSize, DecreaseFontSize, ResetFontSize);

/// Where a dispatched binding action came from. A keyboard action consumed
/// a real key press, so FocusLeft/FocusRight may re-synthesize it into the
/// PTY when the inner TUI should handle it. An IPC action has no key press
/// to forward. Its caller is typically that inner program declaring it has
/// no window in the requested direction, and passthrough would bounce the
/// key straight back to it. A palette action consumed a key press too, but
/// arrives with the panel still searching over a row the query may have
/// hidden, so actions that need a browsing cursor are refused at this origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionOrigin {
    Keyboard,
    Palette,
    Ipc,
}
