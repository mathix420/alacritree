//! The modal dialogs: the state only they own, the dialogs that paint over
//! it, and the background deletes and creations those dialogs start.

use super::*;

/// Every dialog's pending state.  `None` (or empty) means that dialog is
/// closed; `update` paints whichever are open.
#[derive(Default)]
pub(super) struct Modals {
    /// A modal popup carrying a failure message the user must dismiss. Every
    /// failure that has no inline home lands here: a background action (e.g. a
    /// worktree delete) that failed after its dialog closed, or a shell that
    /// would not spawn. Dismissing it leaves the app usable, which an error
    /// painted over the terminal would not.
    pub(super) error_dialog: Option<String>,
    pub(super) quit_dialog_open: bool,
    pub(super) pending_delete: Option<DeleteRequest>,
    /// Confirmed deletes whose git removal is running off-thread; polled and
    /// adopted in `poll_pending_deletes`.
    pub(super) pending_deletes: Vec<DeleteTask>,
    pub(super) pending_create: Option<CreateState>,
    /// Creations the user minimized off the running modal; they keep streaming
    /// off-thread and are adopted in `poll_pending_creates`.
    pub(super) pending_creates: Vec<BackgroundCreate>,
    pub(super) pending_rename: Option<RenameState>,
    /// The base-branch picker modal.  Transient: never persisted.
    pub(super) pending_base_branch: Option<BaseBranchPicker>,
    pub(super) pending_project_remove: Option<ProjectRemoveState>,
    pub(super) pending_session_close: Option<SessionId>,
    /// The sessions a whole-set detach is waiting to be confirmed for.  The
    /// set is fixed when the question is asked, so the dialog detaches what
    /// it counted rather than whatever is attached by the time it is
    /// answered.
    pub(super) pending_detach_all: Option<Vec<SessionId>>,
    /// Whether the modal on screen was already in front of the user when
    /// the keys arriving now were pressed.
    pub(super) gate: ModalGate,
}

impl AlacritreeApp {
    pub(super) fn show_delete_dialog(&mut self, ctx: &Context) {
        if self.modals.pending_delete.is_none() {
            return;
        }

        // Consume Enter/Escape, and judge a confirm, before adopting a
        // dirty count below: adoption can flip `force` from `false` to
        // `true` this same frame, but the keypress was the user's reaction
        // to what was already painted (a previous frame's "checking…", read
        // as `force: false`). Judging the confirm here, against the request
        // as it stands before this frame's adoption runs, is what keeps
        // "what a confirm executes" equal to "what the user was shown", since
        // held Enter (key repeat) would otherwise hit the race on the exact
        // frame the probe lands. The key is consumed whether or not the
        // confirm may act on it: it was aimed at this dialog, and letting it
        // fall through would type it into the shell behind.
        let confirm_ready = self
            .modals
            .pending_delete
            .as_ref()
            .is_some_and(|req| delete_confirm_ready(req.dirty.as_ref(), req.force));
        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::Delete);
        if confirm_via_key && confirm_ready {
            self.run_pending_delete(ctx);
            return;
        }
        if cancel_via_key {
            self.modals.pending_delete = None;
            return;
        }

        let theme = self.theme;
        let danger = self.theme.error;
        let Some(req) = self.modals.pending_delete.as_mut() else {
            return;
        };
        if let Some(job) = req.dirty_job.as_ref() {
            match job.poll() {
                Some(counts) => {
                    // A known-dirty count preloads `force` so confirming goes
                    // straight to a forced removal, with the discard warning
                    // already on screen. That is the same outcome a warm cache
                    // gets at request time, just landing a frame later.
                    req.force = counts.is_dirty();
                    req.dirty = Some(counts);
                    req.dirty_job = None;
                },
                // A panicked probe never lands a count; drop the handle so
                // the dialog stops claiming to be checking and reads
                // "couldn't check" instead (see `dirty_warning`).
                None if job.failed() => req.dirty_job = None,
                None => {},
            }
        }
        let (title, detail, verb) = if req.prunable {
            (
                format!("Prune worktree `{}`?", req.worktree_name),
                "The worktree directory is already gone; this removes git's leftover metadata."
                    .to_string(),
                "Prune",
            )
        } else {
            (
                format!("Delete worktree `{}`?", req.worktree_name),
                match &req.branch {
                    Some(b) => format!("Removes the worktree directory and deletes branch `{b}`."),
                    None => "Removes the worktree directory.".to_string(),
                },
                "Delete",
            )
        };
        let warning = dirty_warning(req.dirty.as_ref(), req.force, req.dirty_job.is_some());
        let ready = delete_confirm_ready(req.dirty.as_ref(), req.force);
        // The probe finished without leaving a count, so waiting longer buys
        // nothing; offer the check again rather than stranding the dialog
        // behind a confirm that will never enable.
        let recheckable = !ready && req.dirty_job.is_none();

        let frame = modal_frame(&theme);
        let mut confirmed = false;
        let mut cancelled = false;
        let mut recheck = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_delete_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(360.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(title).color(theme.text).strong());
                ui.label(RichText::new(detail).color(theme.text_muted).small());
                if let Some(w) = &warning {
                    ui.label(RichText::new(w).color(danger).small());
                }
                if req.prunable {
                    if let Some(b) = req.branch.clone() {
                        ui.checkbox(
                            &mut req.delete_branch,
                            RichText::new(format!("Also delete branch `{b}`"))
                                .color(theme.text_muted)
                                .small(),
                        );
                    }
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    let hint = if ready {
                        format!("Enter to {} | Esc to cancel", verb.to_lowercase())
                    } else {
                        "Esc to cancel".to_string()
                    };
                    ui.label(RichText::new(hint).color(theme.text_muted).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let delete = ui
                            .add_enabled_ui(ready, |ui| modal_button(ui, &theme, verb, danger))
                            .inner;
                        // A click was aimed at the dialog as last painted,
                        // so it answers to the same judgement as a key.
                        if delete.clicked() && confirm_ready {
                            confirmed = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                        if recheckable
                            && modal_button(ui, &theme, "Check again", theme.text).clicked()
                        {
                            recheck = true;
                        }
                        focus_default(ui.ctx(), delete.id);
                    });
                });
            },
        );

        if recheck {
            let vcs = self
                .modals
                .pending_delete
                .as_ref()
                .and_then(|req| self.vcs_for(&req.worktree_path));
            if let Some(req) = self.modals.pending_delete.as_mut() {
                let path = req.worktree_path.clone();
                req.dirty_job =
                    Some(jobs::pool().spawn(jobs::Priority::Interactive, move |blocking| {
                        vcs.map_or_else(Dirty::default, |vcs| {
                            vcs.dirty(&path, blocking).unwrap_or_default()
                        })
                    }));
            }
            return;
        }
        if confirmed {
            self.run_pending_delete(ctx);
            return;
        }
        if cancelled || modal.should_close() {
            self.modals.pending_delete = None;
        }
    }

    pub(super) fn show_close_session_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = self.theme.error;
        let Some(id) = self.modals.pending_session_close else {
            return;
        };
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            // Exited between the click and this frame. Nothing is left to close.
            self.modals.pending_session_close = None;
            return;
        };
        // A managed session's attach client is always running, so the busy
        // warning would fire every time and warn about nothing: what it
        // guards against is losing work, and detaching loses none.
        let managed = session.pane_key.is_some();
        let title = if managed {
            format!("Detach from `{}`?", session.title)
        } else {
            format!("Close session `{}`?", session.title)
        };
        let busy = session.is_busy() && !managed;

        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::CloseSession);
        let frame = modal_frame(&theme);
        let mut confirmed = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_close_session_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(320.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(title).color(theme.text).strong());
                if busy {
                    ui.label(
                        RichText::new("A process appears to be running.").color(danger).small(),
                    );
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    let keys = if managed {
                        "Enter to detach · Esc to cancel"
                    } else {
                        "Enter to close · Esc to cancel"
                    };
                    ui.label(RichText::new(keys).color(theme.text_muted).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (verb, tint) =
                            if managed { ("Detach", theme.text) } else { ("Close", danger) };
                        let close_btn = modal_button(ui, &theme, verb, tint);
                        if close_btn.clicked() {
                            confirmed = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), close_btn.id);
                    });
                });
            });

        if confirm_via_key || confirmed {
            self.modals.pending_session_close = None;
            self.close_session(ctx, id);
            return;
        }
        if cancel_via_key || cancelled || modal.should_close() {
            self.modals.pending_session_close = None;
        }
    }

    /// The one question a whole-set detach asks.  Every session in the set is
    /// managed, so the busy warning a close carries has nothing to warn
    /// about: the panes keep running and their rows come back.
    pub(super) fn show_detach_all_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let Some(ids) = self.modals.pending_detach_all.clone() else {
            return;
        };
        // A session that ended elsewhere while the question was up is no
        // longer this dialog's to end, and one that took the last of them
        // leaves nothing to ask about.
        let ids: Vec<SessionId> =
            ids.into_iter().filter(|id| self.sessions.iter().any(|s| s.id == *id)).collect();
        if ids.is_empty() {
            self.modals.pending_detach_all = None;
            return;
        }
        let count = ids.len();
        let title = format!(
            "Detach from {count} multiplexer {}?",
            if count == 1 { "pane" } else { "panes" }
        );

        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::DetachAll);
        let frame = modal_frame(&theme);
        let mut confirmed = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_detach_all_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(320.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(title).color(theme.text).strong());
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to detach · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let detach_btn = modal_button(ui, &theme, "Detach", theme.text);
                        if detach_btn.clicked() {
                            confirmed = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), detach_btn.id);
                    });
                });
            });

        if confirm_via_key || confirmed {
            self.modals.pending_detach_all = None;
            self.detach_sessions(ctx, &ids);
            return;
        }
        if cancel_via_key || cancelled || modal.should_close() {
            self.modals.pending_detach_all = None;
        }
    }

    pub(super) fn show_remove_project_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = self.theme.error;
        let Some(state) = self.modals.pending_project_remove.as_ref() else {
            return;
        };
        let title = format!("Remove `{}` from the sidebar?", state.name);

        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::RemoveProject);
        let frame = modal_frame(&theme);
        let mut confirmed = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_remove_project_dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(340.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(title).color(theme.text).strong());
                ui.label(
                    RichText::new("Nothing on disk is touched; open sessions keep running.")
                        .color(theme.text_muted)
                        .small(),
                );
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to remove · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let remove = ui.add(
                            egui::Button::new(RichText::new("Remove").color(danger)).frame(false),
                        );
                        if remove.clicked() {
                            confirmed = true;
                        }
                        let cancel = ui.add(
                            egui::Button::new(RichText::new("Cancel").color(theme.text_dim))
                                .frame(false),
                        );
                        if cancel.clicked() {
                            cancelled = true;
                        }
                        focus_default(ui.ctx(), remove.id);
                    });
                });
            });

        if confirm_via_key || confirmed {
            // Re-resolve by root: the list may have shifted (reorder, IPC) while
            // the modal was up.
            if let Some(state) = self.modals.pending_project_remove.take() {
                if let Some(idx) = self.projects.iter().position(|p| p.root == state.root) {
                    self.remove_project(idx);
                }
            }
            return;
        }
        if cancel_via_key || cancelled || modal.should_close() {
            self.modals.pending_project_remove = None;
        }
    }

    pub(super) fn show_error_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = self.theme.error;
        let Some(message) = self.modals.error_dialog.clone() else {
            return;
        };

        // Enter and Esc both just dismiss, since there's nothing to confirm.
        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::Error);
        let frame = modal_frame(&theme);
        let mut dismissed = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_error_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(360.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new("Something went wrong").color(danger).strong());
                ui.label(RichText::new(&message).color(theme.text_muted).small());
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter or Esc to dismiss").color(theme.text_muted).small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ok = ui.add(
                            egui::Button::new(RichText::new("OK").color(theme.text)).frame(false),
                        );
                        if ok.clicked() {
                            dismissed = true;
                        }
                        focus_default(ui.ctx(), ok.id);
                    });
                });
            },
        );

        if confirm_via_key || cancel_via_key || dismissed || modal.should_close() {
            self.modals.error_dialog = None;
        }
    }

    fn run_pending_delete(&mut self, ctx: &Context) {
        let Some(req) = self.modals.pending_delete.take() else {
            return;
        };
        let project_root = self.projects[req.project_idx].root.clone();
        let Some(vcs) = self.vcs_for(&req.worktree_path) else {
            self.modals.error_dialog =
                Some(alacritree_vcs::VcsError::NotARepository(project_root).to_string());
            return;
        };
        // Drop sessions whose cwd is the worktree before deleting it; the PTY
        // would otherwise block the directory removal on some filesystems.
        self.close_worktree_sessions(ctx, &req.worktree_path);

        // The git removal (shellouts, branch delete, checkout hooks) is slow
        // enough to stutter paint, so run it off-thread and adopt the result in
        // `poll_pending_deletes`; the dialog closes immediately either way and
        // the sidebar row shows a spinner meanwhile.
        let worktree_path = req.worktree_path.clone();
        let worktree_name = req.worktree_name.clone();
        let branch = req.branch.clone();
        let checkout = Checkout {
            name: req.worktree_name,
            path: req.worktree_path,
            head: alacritree_vcs::Head { name: req.branch, ..Default::default() },
            is_main: false,
            gone: req.prunable,
            upstream: None,
        };
        // `req.force` already reflects a resolved dirty count (set in
        // `request_worktree_delete` or when its probe landed), since the
        // dialog refuses to confirm before one is known. A tree that went
        // dirty after a clean count still gets git's refusal, which
        // `poll_pending_deletes` reopens as a forced retry. Only the prune
        // dialog asks whether the branch goes too.
        let remove = alacritree_vcs::RemoveCheckout {
            main: project_root,
            checkout,
            force: req.force,
            delete_name: !req.prunable || req.delete_branch,
        };
        let hooks = crate::checkout_hooks::from_config(&self.config.integrations);
        let job = wt::spawn_delete(vcs, remove, hooks, ctx.clone());
        self.modals.pending_deletes.push(DeleteTask {
            project_idx: req.project_idx,
            worktree_path,
            worktree_name,
            branch,
            dirty: req.dirty,
            delete_branch: req.delete_branch,
            prunable: req.prunable,
            job,
        });
    }

    pub(super) fn close_worktree_sessions(&mut self, ctx: &Context, worktree_path: &Path) {
        let workspace = Some(worktree_path.to_path_buf());
        let ids: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|s| s.working_directory == workspace)
            .map(|s| s.id)
            .collect();
        self.close_sessions(ctx, &ids, workspace, CloseReason::WorktreeDeleted);
    }

    /// Adopt finished background deletes: pop up any failure and refresh the
    /// affected project so the removed worktree (or its spinner) drops out of
    /// the sidebar. A refusal that names a dirty or untracked tree reopens the
    /// confirm as a forced retry instead of surfacing a plain error. Git is
    /// the authority on whether the removal would lose work, not a count read
    /// while the user was staring at the first dialog.
    pub(super) fn poll_pending_deletes(&mut self, ctx: &Context) {
        struct Finished {
            project_idx: usize,
            worktree_path: PathBuf,
            worktree_name: String,
            branch: Option<String>,
            dirty: Option<Dirty>,
            delete_branch: bool,
            prunable: bool,
            result: Result<(), wt::WorktreeError>,
        }
        let mut finished: Vec<Finished> = Vec::new();
        self.modals.pending_deletes.retain(|task| match task.job.poll() {
            Some(result) => {
                finished.push(Finished {
                    project_idx: task.project_idx,
                    worktree_path: task.worktree_path.clone(),
                    worktree_name: task.worktree_name.clone(),
                    branch: task.branch.clone(),
                    dirty: task.dirty,
                    delete_branch: task.delete_branch,
                    prunable: task.prunable,
                    result,
                });
                false
            },
            // A panicked delete job never lands a result; without this the
            // sidebar row's spinner would spin forever instead of surfacing
            // a failure.
            None if task.job.failed() => {
                finished.push(Finished {
                    project_idx: task.project_idx,
                    worktree_path: task.worktree_path.clone(),
                    worktree_name: task.worktree_name.clone(),
                    branch: task.branch.clone(),
                    dirty: task.dirty,
                    delete_branch: task.delete_branch,
                    prunable: task.prunable,
                    result: Err(wt::WorktreeError::WorkerPanicked),
                });
                false
            },
            None => true,
        });
        for f in finished {
            match f.result {
                Ok(()) => {},
                // Only opens the retry when no confirm is currently on
                // screen. Reopening unconditionally would swap the dialog
                // contents under a user looking at an unrelated confirm
                // (same modal id, so an in-flight Enter would force-delete
                // the wrong worktree), and a second refusal landing in this
                // same batch would silently overwrite the first retry
                // instead of surfacing it.
                Err(e) if !f.prunable && offers_force(&e) => {
                    if self.modals.pending_delete.is_none() {
                        self.modals.pending_delete = Some(DeleteRequest {
                            project_idx: f.project_idx,
                            worktree_path: f.worktree_path,
                            worktree_name: f.worktree_name,
                            branch: f.branch,
                            dirty: f.dirty,
                            dirty_job: None,
                            prunable: false,
                            delete_branch: f.delete_branch,
                            force: true,
                        });
                    } else {
                        push_error(&mut self.modals.error_dialog, format!("Delete failed.\n\n{e}"));
                    }
                },
                Err(e) => {
                    let action = if f.prunable { "Prune" } else { "Delete" };
                    push_error(&mut self.modals.error_dialog, format!("{action} failed.\n\n{e}"));
                },
            }
            self.refresh_project(ctx, f.project_idx);
        }
    }

    /// Adopt minimized creates once their worker finishes: pop up any failure
    /// (its modal is long gone) and refresh the project so the new worktree
    /// replaces its sidebar placeholder.  A successful create is deliberately
    /// not activated: the user minimized to work elsewhere, so don't yank them
    /// into the new worktree.
    pub(super) fn poll_pending_creates(&mut self, ctx: &Context) {
        let mut finished: Vec<(usize, Result<PathBuf, wt::WorktreeError>)> = Vec::new();
        self.modals.pending_creates.retain_mut(|task| {
            let mut done = None;
            loop {
                match task.rx.try_recv() {
                    Ok(Progress::Step(_)) => {},
                    Ok(Progress::Done(result)) => {
                        done = Some(result);
                        break;
                    },
                    // Nothing more this frame either way. A worker that
                    // unwound instead of reporting comes back through the
                    // failure latch below, so its placeholder row is
                    // replaced rather than left standing forever.
                    Err(_) => break,
                }
            }
            let _ = task.job.poll();
            let done = done
                .or_else(|| task.job.failed().then_some(Err(wt::WorktreeError::WorkerPanicked)));
            match done {
                Some(result) => {
                    finished.push((task.project_idx, result));
                    false
                },
                None => true,
            }
        });
        for (project_idx, result) in finished {
            if let Err(e) = result {
                self.modals.error_dialog = Some(format!("Worktree creation failed.\n\n{e}"));
            }
            self.refresh_project(ctx, project_idx);
        }
    }

    pub(super) fn show_rename_dialog(&mut self, ctx: &Context) {
        let Some(RenameState { root, mut label }) = self.modals.pending_rename.take() else {
            return;
        };
        // The project can vanish under the modal (IPC remove_project);
        // nothing is left to rename then.
        let Some(dir_name) = self.projects.iter().find(|p| p.root == root).map(|p| p.name.clone())
        else {
            return;
        };
        let theme = self.theme;
        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::Rename);
        let frame = modal_frame(&theme);
        let mut rename_clicked = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_rename_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new(format!("Rename `{dir_name}`")).color(theme.text).strong());
                ui.label(
                    RichText::new("Sidebar name only. The directory is untouched.")
                        .color(theme.text_muted)
                        .small(),
                );
                let input_id = egui::Id::new("alacritree_rename_input");
                let edit = egui::TextEdit::singleline(&mut label)
                    .id(input_id)
                    .hint_text(dir_name.as_str())
                    .desired_width(f32::INFINITY);
                let resp = ui.add(edit);
                focus_default(ui.ctx(), input_id);
                if resp.lost_focus() && resp.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    rename_clicked = true;
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to rename · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if modal_button(ui, &theme, "Rename", theme.accent).clicked() {
                            rename_clicked = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                    });
                });
            },
        );

        if cancel_via_key || cancelled || modal.should_close() {
            return;
        }
        if confirm_via_key || rename_clicked {
            let _ = self.rename_project(&root, Some(label));
            return;
        }
        self.modals.pending_rename = Some(RenameState { root, label });
    }

    pub(super) fn show_base_branch_picker(&mut self, ctx: &Context) {
        let Some(mut picker) = self.modals.pending_base_branch.take() else {
            return;
        };
        if let Some(job) = picker.branches_job.as_ref() {
            match job.poll() {
                Some(branches) => {
                    picker.branches = Some(branches);
                    picker.branches_job = None;
                },
                // A panicked listing never lands a result; drop the handle
                // so the picker shows the failure row instead of "loading
                // branches…" forever.
                None if job.failed() => {
                    picker.branches = Some(Err(wt::WorktreeError::WorkerPanicked));
                    picker.branches_job = None;
                },
                None => {},
            }
        }
        let theme = self.theme;
        let danger = self.theme.error;
        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::BaseBranchPicker);
        let (up, down) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            )
        });
        let frame = modal_frame(&theme);
        let current = self.git_panel.base_branch_overrides.get(&picker.worktree).cloned();
        let s = theme.ui_scale;

        // Row 0 is always "Auto"; branch rows follow, narrowed by the query.
        // Populated inside the modal closure, after the TextEdit runs, so the
        // rows reflect this frame's query rather than the previous one.
        let mut filtered: Vec<String> = Vec::new();
        let mut chosen: Option<Option<String>> = None; // Some(None) = Auto
        let modal = egui::Modal::new(egui::Id::new("alacritree_base_branch_picker"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 4.0 * s;
                let name = picker
                    .worktree
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| wsl::display_path(&picker.worktree));
                ui.label(
                    RichText::new(format!("Base branch for `{name}`")).color(theme.text).strong(),
                );
                ui.label(
                    RichText::new("The git panel diffs this worktree against it.")
                        .color(theme.text_muted)
                        .small(),
                );
                let input_id = egui::Id::new("alacritree_base_branch_query");
                let edit = egui::TextEdit::singleline(&mut picker.query)
                    .id(input_id)
                    .hint_text("filter branches")
                    .desired_width(f32::INFINITY);
                let query_changed = ui.add(edit).changed();
                focus_default(ui.ctx(), input_id);

                if let Some(Err(e)) = &picker.branches {
                    ui.label(RichText::new(e.to_string()).color(danger).small());
                }

                filtered = match &picker.branches {
                    Some(Ok(branches)) => filter_branches(branches, &picker.query),
                    Some(Err(_)) | None => Vec::new(),
                };
                picker.cursor = picker_cursor(
                    query_changed,
                    picker.query.is_empty(),
                    picker.cursor,
                    filtered.len(),
                );

                let mark = |selected: bool| if selected { "• " } else { "   " };
                egui::ScrollArea::vertical().max_height(240.0 * s).show(ui, |ui| {
                    let auto_label = match &picker.detected {
                        Some(d) => format!("{}Auto ({d})", mark(current.is_none())),
                        None => format!("{}Auto", mark(current.is_none())),
                    };
                    let auto = ui.selectable_label(picker.cursor == 0, auto_label);
                    if auto.clicked() {
                        chosen = Some(None);
                    }
                    if picker.branches.is_none() {
                        ui.add_enabled(
                            false,
                            egui::Label::new(
                                RichText::new("loading branches…").color(theme.text_muted),
                            ),
                        );
                    }
                    for (i, branch) in filtered.iter().enumerate() {
                        let selected = current.as_deref() == Some(branch.as_str());
                        let resp = ui.selectable_label(
                            picker.cursor == i + 1,
                            format!("{}{branch}", mark(selected)),
                        );
                        if resp.clicked() {
                            chosen = Some(Some(branch.clone()));
                        }
                    }
                });
                ui.label(
                    RichText::new("↑↓ move · Enter apply · Esc cancel")
                        .color(theme.text_muted)
                        .small(),
                );
            });

        if up {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
        if down {
            picker.cursor = (picker.cursor + 1).min(filtered.len());
        }
        // While the listing is pending or failed, `filtered` is empty, so
        // cursor 0 would resolve to Auto, and applying it on Enter would clear
        // an existing override on a reflexive keypress rather than the no-op
        // that state should produce. Clicking Auto still works either way
        // (see `auto.clicked()` above); only the keyboard shortcut is gated.
        if confirm_via_key && matches!(picker.branches, Some(Ok(_))) {
            chosen = Some(if picker.cursor == 0 {
                None
            } else {
                filtered.get(picker.cursor - 1).cloned()
            });
        }
        if cancel_via_key || modal.should_close() {
            return;
        }
        if let Some(branch) = chosen {
            self.apply_base_branch(picker.worktree, branch);
            return;
        }
        self.modals.pending_base_branch = Some(picker);
    }

    pub(super) fn show_create_dialog(&mut self, ctx: &Context) {
        let Some(state) = self.modals.pending_create.take() else {
            return;
        };
        let next = match state {
            CreateState::Prompt { project_idx, branch, error } => {
                self.show_create_prompt(ctx, project_idx, branch, error)
            },
            CreateState::Running { project_idx, branch, mut steps, rx, job } => {
                let mut done: Option<Result<PathBuf, wt::WorktreeError>> = None;
                while let Ok(p) = rx.try_recv() {
                    match p {
                        Progress::Step(s) => steps.push(s),
                        Progress::Done(r) => done = Some(r),
                    }
                }
                // A panicked worker sends no `Progress::Done`, so without the
                // latch the modal would sit on its last step forever.
                let _ = job.poll();
                if done.is_none() && job.failed() {
                    done = Some(Err(wt::WorktreeError::WorkerPanicked));
                }
                let minimized = self.show_create_running(ctx, project_idx, &branch, &steps);
                match done {
                    // A finished job goes to its result even if a minimize press
                    // lands on the same frame, so the outcome is never lost.
                    Some(result) => Some(CreateState::Done { project_idx, steps, result }),
                    // Minimized: hand the still-running create off to
                    // `poll_pending_creates` and dismiss the modal.
                    None if minimized => {
                        self.modals.pending_creates.push(BackgroundCreate {
                            project_idx,
                            branch,
                            rx,
                            job,
                        });
                        None
                    },
                    None => Some(CreateState::Running { project_idx, branch, steps, rx, job }),
                }
            },
            CreateState::Done { project_idx, steps, result } => {
                if self.show_create_done(ctx, project_idx, &steps, &result) {
                    if let Ok(path) = &result {
                        self.refresh_project(ctx, project_idx);
                        let path = path.clone();
                        self.activate_worktree(ctx, &path);
                    }
                    None
                } else {
                    Some(CreateState::Done { project_idx, steps, result })
                }
            },
        };
        self.modals.pending_create = next;
    }

    fn show_create_prompt(
        &mut self,
        ctx: &Context,
        project_idx: usize,
        mut branch: String,
        mut error: Option<alacritree_vcs::VcsError>,
    ) -> Option<CreateState> {
        let theme = self.theme;
        let danger = self.theme.error;
        let project_name = self.projects[project_idx].display_name().to_string();
        let default_branch = self.projects[project_idx].trunk.clone();
        let project_root = self.projects[project_idx].root.clone();

        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::CreatePrompt);
        let frame = modal_frame(&theme);
        let mut create_clicked = false;
        let mut cancelled = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_create_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(
                    RichText::new(format!("New worktree in `{project_name}`"))
                        .color(theme.text)
                        .strong(),
                );
                ui.label(
                    RichText::new(match default_branch.as_deref() {
                        Some(b) => format!("Branched from origin/{b}"),
                        None => "Base branch will be resolved from origin/HEAD.".to_string(),
                    })
                    .color(theme.text_muted)
                    .small(),
                );
                let input_id = egui::Id::new("alacritree_create_input");
                let edit = egui::TextEdit::singleline(&mut branch)
                    .id(input_id)
                    .hint_text("branch name")
                    .desired_width(f32::INFINITY);
                let resp = ui.add(edit);
                focus_default(ui.ctx(), input_id);
                if resp.lost_focus() && resp.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create_clicked = true;
                }
                if let Some(e) = &error {
                    ui.label(RichText::new(e.to_string()).color(danger).small());
                }
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to create · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if modal_button(ui, &theme, "Create", theme.accent).clicked() {
                            create_clicked = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancelled = true;
                        }
                    });
                });
            },
        );

        if cancel_via_key || cancelled || modal.should_close() {
            return None;
        }
        if confirm_via_key || create_clicked {
            // Whitespace runs become single hyphens: `some text like this` →
            // `some-text-like-this`.
            let canonical: String = branch.split_whitespace().collect::<Vec<_>>().join("-");
            let Some(vcs) = self.vcs_for(&project_root) else {
                error = Some(alacritree_vcs::VcsError::NotARepository(project_root));
                return Some(CreateState::Prompt { project_idx, branch, error });
            };
            if let Err(invalid) = vcs.validate_name(&canonical) {
                error = Some(invalid);
                return Some(CreateState::Prompt { project_idx, branch, error });
            }
            let req = CreateRequest::new(
                project_root,
                default_branch,
                canonical.clone(),
                &self.config.workspace,
                vcs,
            );
            let hooks = crate::checkout_hooks::from_config(&self.config.integrations);
            let (rx, job) = wt::spawn_create(req, hooks, ctx.clone());
            return Some(CreateState::Running {
                project_idx,
                branch: canonical,
                steps: Vec::new(),
                rx,
                job,
            });
        }
        Some(CreateState::Prompt { project_idx, branch, error })
    }

    /// Renders the live progress view and returns `true` when the user asks to
    /// minimize (Enter, Escape, or a click outside), sending the create to the
    /// background so they can keep working.  The git operation can't be
    /// cancelled mid-flight, so every dismiss path minimizes rather than aborts.
    fn show_create_running(
        &self,
        ctx: &Context,
        project_idx: usize,
        branch: &str,
        steps: &[String],
    ) -> bool {
        let theme = self.theme;
        let project_name = self.projects[project_idx].display_name().to_string();
        let frame = modal_frame(&theme);
        let s = theme.ui_scale;
        let (minimize_via_esc, minimize_via_enter) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::CreateRunning);
        let modal = egui::Modal::new(egui::Id::new("alacritree_create_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(
                    RichText::new(format!("Creating `{branch}` in `{project_name}`"))
                        .color(theme.text)
                        .strong(),
                );
                ui.add_space(4.0 * s);
                for (i, step) in steps.iter().enumerate() {
                    let is_last = i + 1 == steps.len();
                    let bullet_color = if is_last { theme.accent } else { theme.text_dim };
                    let text_color = if is_last { theme.text } else { theme.text_dim };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("•").color(bullet_color));
                        ui.label(RichText::new(step).color(text_color).small());
                    });
                }
                if steps.is_empty() {
                    ui.label(RichText::new("Starting…").color(theme.text_muted).small());
                }
                ui.add_space(4.0 * s);
                ui.label(
                    RichText::new("Enter to keep working while it finishes in the background")
                        .color(theme.text_muted)
                        .small(),
                );
            },
        );
        minimize_via_esc || minimize_via_enter || modal.should_close()
    }

    fn show_create_done(
        &self,
        ctx: &Context,
        project_idx: usize,
        steps: &[String],
        result: &Result<PathBuf, wt::WorktreeError>,
    ) -> bool {
        let theme = self.theme;
        let danger = self.theme.error;
        let ok = self.theme.ok;
        let project_name = self.projects[project_idx].display_name().to_string();
        let frame = modal_frame(&theme);
        let mut close = false;
        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::CreateDone);

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_create_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(380.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                let (title, color) = match result {
                    Ok(_) => (format!("Created worktree in `{project_name}`"), ok),
                    Err(_) => ("Worktree creation failed".to_string(), danger),
                };
                ui.label(RichText::new(title).color(color).strong());
                let last = steps.len().saturating_sub(1);
                for (i, step) in steps.iter().enumerate() {
                    let failed_step = result.is_err() && i == last;
                    let bullet_color = if failed_step { danger } else { ok };
                    let text_color = if failed_step { danger } else { theme.text_dim };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("•").color(bullet_color));
                        ui.label(RichText::new(step).color(text_color).small());
                    });
                }
                if let Err(e) = result {
                    ui.add_space(4.0 * s);
                    ui.label(RichText::new(e.to_string()).color(danger).small());
                }
                ui.add_space(4.0 * s);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if result.is_ok() { "Open" } else { "Close" };
                    let btn = modal_button(ui, &theme, label, theme.accent);
                    if btn.clicked() {
                        close = true;
                    }
                    focus_default(ui.ctx(), btn.id);
                });
            },
        );

        if confirm_via_key || cancel_via_key || close || modal.should_close() {
            return true;
        }
        false
    }

    pub(super) fn show_quit_dialog(&mut self, ctx: &Context) {
        let theme = self.theme;
        let danger = self.theme.error;
        let n = self.sessions.len();

        let (cancel_via_key, confirm_via_key) =
            consume_modal_keys(ctx, &self.modals.gate, ModalKind::Quit);
        let frame = modal_frame(&theme);
        let mut quit_clicked = false;
        let mut cancel_clicked = false;

        let s = theme.ui_scale;
        let modal = egui::Modal::new(egui::Id::new("alacritree_quit_dialog")).frame(frame).show(
            ctx,
            |ui| {
                ui.set_width(320.0 * s);
                ui.spacing_mut().item_spacing.y = 6.0 * s;
                ui.label(RichText::new("Quit alacritree?").color(theme.text).strong());
                let msg = match n {
                    0 => "No sessions running.".to_string(),
                    1 => "1 session will be terminated.".to_string(),
                    n => format!("{n} sessions will be terminated."),
                };
                ui.label(RichText::new(msg).color(theme.text_muted).small());
                ui.add_space(4.0 * s);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Enter to quit · Esc to cancel")
                            .color(theme.text_muted)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let quit = modal_button(ui, &theme, "Quit", danger);
                        if quit.clicked() {
                            quit_clicked = true;
                        }
                        if modal_button(ui, &theme, "Cancel", theme.text_dim).clicked() {
                            cancel_clicked = true;
                        }
                        focus_default(ui.ctx(), quit.id);
                    });
                });
            },
        );

        if confirm_via_key || quit_clicked {
            self.modals.quit_dialog_open = false;
            crash_log::record_reason(ExitReason::UserQuit);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cancel_via_key || cancel_clicked || modal.should_close() {
            self.modals.quit_dialog_open = false;
        }
    }
}

/// A modal action button. Framed and filled so it reads as clickable.
/// Frameless text buttons looked like captions and users reached for the
/// keyboard hint instead of the mouse.
fn modal_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    text_color: Color32,
) -> egui::Response {
    let s = theme.ui_scale;
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(10.0 * s, 3.0 * s);
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = theme.row_hover_bg;
        widgets.inactive.bg_stroke = Stroke::new(1.0_f32, theme.sidebar_border);
        widgets.hovered.weak_bg_fill = theme.row_active_bg;
        widgets.hovered.bg_stroke = Stroke::new(1.0_f32, theme.sidebar_border);
        widgets.active.weak_bg_fill = theme.row_active_bg;
        ui.add(egui::Button::new(RichText::new(label).color(text_color)))
    })
    .inner
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub(super) struct DeleteRequest {
    pub(super) project_idx: usize,
    pub(super) worktree_path: PathBuf,
    pub(super) worktree_name: String,
    pub(super) branch: Option<String>,
    /// `None` until a count lands. The cache answers for a worktree the git
    /// panel has shown; one never selected has to wait for the job.
    pub(super) dirty: Option<Dirty>,
    /// Fills `dirty` when the cache was cold.
    pub(super) dirty_job: Option<jobs::Job<Dirty>>,
    /// The checkout dir is already gone; confirm prunes metadata instead of
    /// removing a directory.
    pub(super) prunable: bool,
    /// Checkbox state for the prune dialog's "also delete branch".
    pub(super) delete_branch: bool,
    /// Whether this confirm's removal passes `--force`: preset `true` when
    /// the dirty count is already known dirty (a warm cache, or a cold
    /// probe that landed before the confirm), left `false` while the count
    /// is unknown, and set `true` when reopening as the retry after an
    /// unforced removal was refused by git.
    pub(super) force: bool,
}

/// An in-flight background delete/prune awaiting its git result.
pub(super) struct DeleteTask {
    pub(super) project_idx: usize,
    /// Marks the matching sidebar row with a spinner while the removal runs.
    pub(super) worktree_path: PathBuf,
    pub(super) worktree_name: String,
    pub(super) branch: Option<String>,
    pub(super) dirty: Option<Dirty>,
    pub(super) delete_branch: bool,
    /// Distinguishes the "prune" vs "delete" wording in a failure message.
    pub(super) prunable: bool,
    pub(super) job: jobs::Job<Result<(), wt::WorktreeError>>,
}

pub(super) enum CreateState {
    Prompt {
        project_idx: usize,
        branch: String,
        error: Option<alacritree_vcs::VcsError>,
    },
    Running {
        project_idx: usize,
        branch: String,
        steps: Vec<String>,
        rx: Receiver<Progress>,
        /// Kept alive so dropping it doesn't cancel the still-running create
        /// on the pool.  `rx` carries the result, so the handle is polled
        /// only for the failure latch a panicked create reports through.
        job: jobs::Job<()>,
    },
    Done {
        project_idx: usize,
        steps: Vec<String>,
        result: Result<PathBuf, wt::WorktreeError>,
    },
}

/// A worktree creation the user minimized from the running modal: it keeps
/// running off-thread while they work, and its result is adopted in
/// `poll_pending_creates`.
pub(super) struct BackgroundCreate {
    pub(super) project_idx: usize,
    /// Shown on the sidebar placeholder row until the finished worktree
    /// replaces it on refresh.
    pub(super) branch: String,
    pub(super) rx: Receiver<Progress>,
    /// See `CreateState::Running::job`.
    pub(super) job: jobs::Job<()>,
}

/// The rename dialog, keyed by root rather than index: an IPC `remove_project`
/// can reorder `projects` while the modal is open.
pub(super) struct RenameState {
    pub(super) root: PathBuf,
    /// Text being edited; seeded with the current display name.
    pub(super) label: String,
}

/// The "remove project" confirmation modal.  Keyed by root, like the rename
/// dialog, so a reorder or IPC removal under the modal can't retarget it.
pub(super) struct ProjectRemoveState {
    pub(super) root: PathBuf,
    /// Display name, kept for the prompt after `projects` may have shifted.
    pub(super) name: String,
}

/// Modal state for choosing a worktree's diff base.
pub(super) struct BaseBranchPicker {
    pub(super) worktree: PathBuf,
    pub(super) query: String,
    /// `None` until the listing lands; the picker opens before git answers.
    /// `Err` is what git said when listing failed (not a repo, WSL down…).
    pub(super) branches: Option<Result<Vec<String>, wt::WorktreeError>>,
    pub(super) branches_job: Option<jobs::Job<Result<Vec<String>, wt::WorktreeError>>>,
    /// Auto-detected base shown on the "Auto" row.
    pub(super) detected: Option<String>,
    pub(super) cursor: usize,
}

/// Whether a failed removal refused only because the checkout held unsaved
/// work, which a forced retry would discard.
pub(super) fn offers_force(error: &wt::WorktreeError) -> bool {
    matches!(error, wt::WorktreeError::Vcs(alacritree_vcs::VcsError::Unsaved { .. }))
}

/// Whether the delete confirm may execute.
///
/// A removal is only safe to run once the dirty count is resolved.  The
/// sessions living in the worktree are torn down before `git worktree
/// remove` runs, so an unforced attempt that git refuses as dirty has
/// already cost the user their shells by the time the refusal arrives.  A
/// resolved count presets `--force`, which git will not refuse for
/// dirtiness; a forced retry has already been through that refusal.
fn delete_confirm_ready(counts: Option<&Dirty>, force: bool) -> bool {
    force || counts.is_some()
}

/// Fold a failure into the single-slot error dialog rather than replacing
/// what it holds.  One frame can finish several background deletes, and the
/// dialog shows one message: replacing it would leave only the last
/// failure, with the earlier explanations gone before the user ever read
/// them.
fn push_error(slot: &mut Option<String>, message: String) {
    match slot {
        Some(shown) => {
            shown.push_str("\n\n");
            shown.push_str(&message);
        },
        None => *slot = Some(message),
    }
}

pub(super) fn dirty_parts(counts: &Dirty) -> String {
    let mut parts = Vec::new();
    if counts.staged > 0 {
        parts.push(format!("{} staged", counts.staged));
    }
    if counts.modified > 0 {
        parts.push(format!("{} modified", counts.modified));
    }
    if counts.untracked > 0 {
        parts.push(format!("{} untracked", counts.untracked));
    }
    parts.join(", ")
}

/// The delete confirm's warning line.
///
/// `counts` is `None` until a count lands (`checking`) or after a probe
/// failed and left nothing to show (`!checking`). `force` is whether this
/// confirm would pass `--force`: a first attempt whose resolved count is
/// already known dirty, or the retry after git refused an unforced removal.
///
/// `force` is checked first: a forced retry followed git's own refusal, so
/// it is never safe to render "nothing to warn about" for it, regardless of
/// what `counts` holds, whether a stale-clean read or none at all (the
/// request was confirmed before its probe landed, which cancelled the probe).
pub(super) fn dirty_warning(counts: Option<&Dirty>, force: bool, checking: bool) -> Option<String> {
    if force {
        return Some(match counts.filter(|c| c.is_dirty()) {
            Some(counts) => {
                format!(
                    "Working tree has {} file(s). They will be discarded with --force.",
                    dirty_parts(counts)
                )
            },
            None => "git reported local changes; they will be discarded with --force.".to_string(),
        });
    }
    match counts {
        Some(counts) if counts.is_dirty() => {
            Some(format!("Working tree has {} file(s) with local changes.", dirty_parts(counts)))
        },
        Some(_) => None,
        None if checking => Some("Checking working tree for uncommitted changes…".to_string()),
        None => Some("Couldn't check the working tree for local changes.".to_string()),
    }
}

/// Branches whose name contains `query`, case-insensitively.
pub(super) fn filter_branches(branches: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    branches.iter().filter(|b| b.to_lowercase().contains(&query)).cloned().collect()
}

/// Where the picker cursor lands after this frame's filter changes.  Row 0 is
/// always Auto, so reseeding a query edit to 0 would apply Auto on the primary
/// "type a branch name, press Enter" flow.  A non-empty query instead seeds
/// the first branch row (1), clamped to 0 when nothing matches; an empty
/// query seeds Auto.  With no query change, the previous cursor is kept,
/// clamped to the (possibly shrunk) filtered length.
pub(super) fn picker_cursor(
    query_changed: bool,
    query_empty: bool,
    prev: usize,
    filtered_len: usize,
) -> usize {
    if query_changed {
        if query_empty { 0 } else { 1.min(filtered_len) }
    } else {
        prev.min(filtered_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_warning_stays_quiet_for_a_known_clean_unforced_tree() {
        let clean = Dirty::default();
        assert_eq!(dirty_warning(Some(&clean), false, false), None);
    }

    #[test]
    fn dirty_warning_distinguishes_checking_from_unavailable() {
        let checking = dirty_warning(None, false, true).expect("still checking");
        assert!(checking.to_lowercase().contains("checking"));
        let unavailable = dirty_warning(None, false, false).expect("probe failed or was skipped");
        assert!(!unavailable.to_lowercase().contains("checking"));
    }

    #[test]
    fn an_unsaved_refusal_offers_force() {
        let err = wt::WorktreeError::Vcs(alacritree_vcs::VcsError::Unsaved { message: "x".into() });
        assert!(offers_force(&err));
    }

    #[test]
    fn any_other_failure_does_not_offer_force() {
        let err = wt::WorktreeError::Vcs(alacritree_vcs::VcsError::Failed {
            command: "git x".into(),
            stderr: "y".into(),
        });
        assert!(!offers_force(&err));
        assert!(!offers_force(&wt::WorktreeError::WorkerPanicked));
    }

    #[test]
    fn picker_filter_is_a_case_insensitive_contains() {
        let branches =
            vec!["main".to_string(), "develop".to_string(), "origin/develop".to_string()];
        assert_eq!(filter_branches(&branches, ""), branches);
        assert_eq!(filter_branches(&branches, "DEV"), vec!["develop", "origin/develop"]);
        assert!(filter_branches(&branches, "zz").is_empty());
    }

    #[test]
    fn picker_cursor_seeds_the_first_match_on_a_non_empty_query_change() {
        // Typing a query that matches something jumps past Auto to the first
        // match, so Enter applies that match instead of Auto.
        assert_eq!(picker_cursor(true, false, 0, 3), 1);
        // A query with no matches has nothing to land on but Auto.
        assert_eq!(picker_cursor(true, false, 0, 0), 0);
        // Clearing the query back to empty returns the cursor to Auto.
        assert_eq!(picker_cursor(true, true, 5, 3), 0);
        // No query change this frame: clamp the previous cursor to the
        // (possibly shrunk) filtered length instead of reseeding it.
        assert_eq!(picker_cursor(false, false, 5, 3), 3);
        assert_eq!(picker_cursor(false, false, 2, 3), 2);
    }
}
