use super::*;

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
            BindingAction::Named(action) => self.dispatch_named_action(ctx, action, origin),
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

    fn dispatch_named_action(&mut self, ctx: &Context, action: NamedAction, origin: ActionOrigin) {
        match action {
            NamedAction::Paste => self.paste_from_clipboard(ctx, Target::Clipboard),
            NamedAction::PasteSelection => self.paste_from_clipboard(ctx, Target::Primary),
            NamedAction::Quit => self.modals.quit_dialog_open = true,
            NamedAction::Minimize => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
            NamedAction::SelectNextTab => self.cycle_tabs(1),
            NamedAction::SelectPreviousTab => self.cycle_tabs(-1),
            NamedAction::SelectNextSession => self.cycle_sessions(ctx, 1),
            NamedAction::SelectPreviousSession => self.cycle_sessions(ctx, -1),
            NamedAction::SelectTab(n) => self.select_tab(n),
            NamedAction::SelectLastTab => self.select_last_tab(),
            NamedAction::NoOp => {},
            NamedAction::ReceiveChar => {},
            NamedAction::ToggleSessionRows => self.session_rows_always = !self.session_rows_always,
            NamedAction::ToggleSessionTabs => self.session_tabs_always = !self.session_tabs_always,
            NamedAction::MoveSessionUp => self.step_session(-1),
            NamedAction::MoveSessionDown => self.step_session(1),
            NamedAction::ToggleSessionDrag => self.session_drag = !self.session_drag,
            NamedAction::ToggleDetachedSessionsFilter => {
                self.sessions_filter_counts_detached = !self.sessions_filter_counts_detached
            },
            NamedAction::SelectNextWorkspace => self.cycle_workspaces(ctx, 1),
            NamedAction::SelectPreviousWorkspace => self.cycle_workspaces(ctx, -1),
            NamedAction::OpenScratchpad => self.toggle_scratchpad_tab(ctx),
            NamedAction::AddProject => self.add_project_via_dialog(ctx),
            NamedAction::RefreshProjects => self.refresh_all_projects(ctx),
            NamedAction::TogglePalette => self.palette.toggle(),
            NamedAction::FocusTerminal => self.focus_terminal(),
            NamedAction::FocusLeft => self.move_focus(FocusDir::Left, origin),
            NamedAction::FocusRight => self.move_focus(FocusDir::Right, origin),
            other => {
                if self.dispatch_sidebar_action(ctx, other)
                    || self.dispatch_git_action(ctx, other)
                    || self.dispatch_herdr_action(ctx, other)
                    || self.dispatch_search_action(other)
                    || self.dispatch_session_action(ctx, other)
                {
                    return;
                }
                self.dispatch_filter_or_other(other);
            },
        }
    }

    fn dispatch_session_action(&mut self, ctx: &Context, action: NamedAction) -> bool {
        match action {
            NamedAction::Copy => {
                if let Some(idx) = self.active_session_index() {
                    if let Some(editor) = self.sessions[idx].scratchpad.as_ref() {
                        if let Some(text) = editor.selected_text(ctx, self.sessions[idx].id) {
                            clipboard::write(Target::Clipboard, &text);
                        }
                    } else {
                        paste::copy_selection(&self.sessions[idx], &self.config, Target::Clipboard);
                    }
                }
            },
            NamedAction::CopySelection => {
                if let Some(idx) = self.active_session_index() {
                    if let Some(editor) = self.sessions[idx].scratchpad.as_ref() {
                        if let Some(text) = editor.selected_text(ctx, self.sessions[idx].id) {
                            clipboard::write(Target::Primary, &text);
                        }
                    } else {
                        paste::copy_selection(&self.sessions[idx], &self.config, Target::Primary);
                    }
                }
            },
            NamedAction::SpawnNewInstance => {
                let ws = self.current_workspace.clone();
                if let Err(e) = self.spawn_session(ctx, ws.clone()) {
                    self.report_spawn_failure(ctx, &ws, &e);
                }
            },
            NamedAction::ClearHistory => {
                use alacritty_terminal::vte::ansi::{ClearMode, Handler};
                if let Some(idx) = self.active_session_index() {
                    if self.sessions[idx].scratchpad.is_none() {
                        self.sessions[idx].term.lock().clear_screen(ClearMode::Saved);
                    }
                }
            },
            NamedAction::ToggleFullscreen => {
                let on = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!on));
            },
            NamedAction::ToggleMaximized => {
                let on = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!on));
            },
            NamedAction::SpawnProfile(n) => {
                match self.config.profiles.get((n - 1) as usize).map(|p| p.name.clone()) {
                    Some(name) => self.spawn_profile_session(ctx, &name),
                    None => {
                        log::warn!(
                            "SpawnProfile{n}: only {} profiles configured",
                            self.config.profiles.len()
                        );
                        self.modals.error_dialog =
                            Some(format!("SpawnProfile{n}: no such profile"));
                    },
                }
            },
            // No confirmation and no cursor: the child is already gone, so
            // there is nothing left to interrupt and nothing to ask about.
            NamedAction::CloseExitedSession => {
                if let Some(idx) = self.active_session_index()
                    && self.sessions[idx].is_exited()
                {
                    let id = self.sessions[idx].id;
                    self.close_session(ctx, id);
                }
            },
            _ => return false,
        }
        true
    }

    fn dispatch_filter_or_other(&mut self, action: NamedAction) {
        if self.dispatch_project_filter(action) || self.dispatch_git_filter(action) {
            return;
        }
        self.dispatch_scroll_or_other(action);
    }
}

/// Where a dispatched binding action came from.  A keyboard action consumed
/// a real key press, so FocusLeft/FocusRight may re-synthesize it into the
/// PTY when the inner TUI should handle it.  An IPC action has no key press
/// to forward — the caller is typically that inner program declaring it has
/// no window in the requested direction, and passthrough would bounce the
/// key straight back to it.  A palette action consumed a key press too, but
/// arrives with the panel still searching over a row the query may have
/// hidden — so actions that need a browsing cursor are refused at this origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionOrigin {
    Keyboard,
    Palette,
    Ipc,
}
