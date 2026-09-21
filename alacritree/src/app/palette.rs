//! The Ctrl+K command palette's window: building its rows from app state,
//! laying them out in columns, and carrying out the row the user picks.
//! The item model and ranking live in the crate-level `command_palette`.

use super::*;
use crate::multiplexer::{ListedPane, Multiplexer, MultiplexerKind, Pane};

impl AlacritreeApp {
    /// The Ctrl+K command palette: one fuzzy-searchable, executable list of
    /// every keyboard action, open session, and switchable workspace.  A real
    /// modal — while it is up, terminal input and bindings are suppressed (see
    /// `update`) and the palette owns its own keys.
    pub(super) fn show_command_palette(&mut self, ctx: &Context) {
        let theme = self.theme;
        let s = theme.ui_scale;

        // Drain the nav/confirm/cancel keys before the TextEdit runs so it
        // never steals Enter (run), Esc (clear then close), the arrows, or the
        // bound cursor jumps.  Ctrl+K shuts the palette with the same key that
        // opened it.
        let (cancel, confirm) = consume_modal_keys(ctx, &self.modals.gate, ModalKind::Palette);
        let (up, down) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            )
        });
        let jumps = consume_palette_keys(ctx, &self.shortcuts);
        let toggle = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::K));

        let items = self.palette_items();
        let marks = self.palette_marks(&items);
        let hint = palette_hint(&self.shortcuts);
        let content_w = palette_content_width(s, ctx.screen_rect().width());
        let mut chosen: Option<PaletteAction> = None;

        let modal = {
            let palette = &mut self.palette;
            egui::Modal::new(egui::Id::new("alacritree_command_palette"))
                .frame(modal_frame(&theme))
                .show(ctx, |ui| {
                    ui.set_width(content_w);
                    ui.spacing_mut().item_spacing.y = 6.0 * s;
                    let cols = PaletteColumns::new(s, ui.available_width());

                    let input_id = egui::Id::new("alacritree_command_palette_query");
                    let query_changed = ui
                        .add(
                            egui::TextEdit::singleline(palette.query_mut())
                                .id(input_id)
                                .hint_text("search actions, sessions, workspaces")
                                .desired_width(f32::INFINITY),
                        )
                        .changed();
                    focus_default(ui.ctx(), input_id);

                    let ranked = palette.rank(&items);
                    // A query edit reseeds to the top match; the cursor keys
                    // then move within this frame's results.
                    palette.reseed(query_changed, ranked.len());
                    let groups = command_palette::group(&items, &ranked);
                    // Sections reorder the ranked rows, so the cursor steps over
                    // this flattened view rather than the ranking itself.
                    let flat: Vec<usize> =
                        groups.iter().flat_map(|(_, rows)| rows.iter().copied()).collect();
                    if up {
                        palette.select_prev();
                    }
                    if down {
                        palette.select_next(flat.len());
                    }
                    for jump in &jumps {
                        match jump {
                            NamedAction::PaletteTop(_) => palette.select_top(),
                            NamedAction::PaletteBottom(_) => palette.select_bottom(flat.len()),
                            NamedAction::PalettePageUp(_) => palette.page_up(),
                            NamedAction::PalettePageDown(_) => palette.page_down(flat.len()),
                            _ => {},
                        }
                    }
                    let moved = query_changed || up || down || !jumps.is_empty();
                    if confirm {
                        chosen = flat.get(palette.selected()).map(|&i| items[i].action.clone());
                    }
                    let selected = palette.selected();

                    ui.add_space(2.0 * s);
                    paint_palette_header(ui, &theme, &cols);
                    egui::ScrollArea::vertical()
                        .max_height(400.0 * s)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0 * s;
                            if flat.is_empty() {
                                ui.add_space(4.0 * s);
                                ui.label(RichText::new("  no matches").color(theme.text_dim));
                                return;
                            }
                            let mut row = 0usize;
                            for (section, rows) in &groups {
                                paint_palette_section(ui, &theme, &cols, section.title());
                                for &i in rows {
                                    let is_sel = row == selected;
                                    let resp = paint_palette_row(
                                        ui,
                                        &theme,
                                        &self.icons,
                                        &cols,
                                        &items[i],
                                        marks[i].as_ref(),
                                        i,
                                        is_sel,
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                                    if resp.clicked() {
                                        chosen = Some(items[i].action.clone());
                                    }
                                    // Keep the keyboard-selected row in view as
                                    // it moves past the fold.
                                    if is_sel && moved {
                                        resp.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    row += 1;
                                }
                            }
                        });

                    ui.add_space(6.0 * s);
                    ui.label(RichText::new(hint).color(theme.text_muted).small());
                })
        };

        if chosen.is_some() || toggle {
            self.palette.close();
        } else if cancel {
            // Esc narrows before it closes, mirroring the sidebar filters.
            if self.palette.query().is_empty() {
                self.palette.close();
            } else {
                self.palette.clear_query();
            }
        } else if modal.should_close() {
            self.palette.close();
        }

        if let Some(action) = chosen {
            self.run_palette_action(ctx, action);
        }
    }

    /// Everything the palette can act on this frame: every runnable keyboard
    /// action, then each configured shell profile, then each open session,
    /// then each switchable workspace.  Rebuilt each frame — cheap beside
    /// ranking, and always current as sessions and worktrees come and go.
    pub(super) fn palette_items(&self) -> Vec<PaletteItem> {
        let mut items = command_palette::action_items(&self.shortcuts);
        for (i, profile) in self.config.profiles.iter().enumerate() {
            let index = i + 1;
            // SpawnProfile only binds indices 1..=9; past that there is no
            // config name to search by.
            let config_name =
                if index <= 9 { format!("SpawnProfile{index}") } else { String::new() };
            let command = profile_command(profile);
            let keys = command_palette::profile_keys(&self.shortcuts, index as u8);
            items.push(PaletteItem::profile(profile.name.clone(), command, keys, &config_name));
        }
        for session in &self.sessions {
            let agent = self.session_pane(session);
            let activity =
                pane_backed_activity(session.activity(), self.session_pane_status(session));
            let name = session_row_name(&session.title, activity, agent);
            if let Some(key) = session.pane_key.as_ref() {
                let multiplexer = self.multiplexers.get(key.multiplexer);
                let glyph = pane_glyph(multiplexer);
                let retained = multiplexer.retained(&key.side, &key.terminal_id);
                let current = retained.map_or(agent.is_some(), |(_, current)| current);
                let agent = retained.map(|(pane, _)| pane).or(agent);
                let workspace = session
                    .working_directory
                    .as_ref()
                    .map(|_| self.workspace_label(&session.working_directory));
                let mut content = if let Some(agent) = agent {
                    pane_palette_content(
                        Some(session_row_name(&session.title, activity, Some(agent)).text),
                        agent,
                        key.multiplexer,
                        workspace.as_deref(),
                        glyph,
                        self.config.ui.path_style.git_rows,
                        None,
                    )
                } else {
                    let mut content = native_palette_content(
                        name.text,
                        self.workspace_label(&session.working_directory),
                        None,
                        None,
                        "shell",
                    );
                    content.subtitle = pane_subtitle(
                        glyph,
                        (!content.subtitle.is_empty()).then_some(content.subtitle.as_str()),
                    );
                    content
                };
                let kind = agent.and_then(|agent| agent.kind.as_deref());
                let status = agent.filter(|_| current).and_then(|agent| agent.status);
                if !current {
                    let lead = key.multiplexer.to_string();
                    content.secondary = session_middle(Some(&lead), kind, None, "shell");
                }
                let mut managed = self.session_managed(session).unwrap();
                managed.kind = kind.map(str::to_owned);
                managed.title = agent
                    .and_then(|agent| agent.title.clone())
                    .filter(|title| Some(title.as_str()) != kind);
                managed.status = status;
                let side = key.side.label().unwrap_or_else(|| "native".to_string());
                let hover = palette_hover(
                    &content.title_for_hover,
                    kind,
                    status.map(|status| status.label()),
                    &side,
                    session.working_directory.as_deref(),
                    agent.and_then(Pane::working_directory),
                    agent.map(|agent| agent.pane_id.as_str()),
                    Some(&key.terminal_id),
                    Some(&managed),
                    "switch to this session",
                );
                items.push(PaletteItem::session(
                    session.id,
                    content.primary,
                    content.subtitle,
                    content.secondary,
                    hover,
                    kind,
                    agent.map(|agent| agent.pane_id.as_str()),
                    Some(key.multiplexer),
                ));
                continue;
            }
            let fallback_kind = session_fallback_kind(&session.kind);
            let (agent_kind, status) = match activity {
                SessionActivity::Agent { name, live } => (name, Some(live.label())),
                SessionActivity::Shell => {
                    (Some(fallback_kind), shell_state_for(&session.kind, session.is_busy()))
                },
            };
            let content = native_palette_content(
                name.text,
                self.workspace_label(&session.working_directory),
                agent_kind,
                status,
                fallback_kind,
            );
            let side = session
                .wsl_distro()
                .map(|distro| format!("wsl:{distro}"))
                .unwrap_or_else(|| "native".to_string());
            let hover = palette_hover(
                &content.title_for_hover,
                agent_kind
                    .or_else(|| (session.kind != SessionKind::Shell).then_some(fallback_kind)),
                status,
                &side,
                session.working_directory.as_deref(),
                None,
                None,
                None,
                None,
                "switch to this session",
            );
            items.push(PaletteItem::session(
                session.id,
                content.primary,
                content.subtitle,
                content.secondary,
                hover,
                agent_kind,
                None,
                None,
            ));
        }
        for ListedPane { workspace: ws, key, pane: agent } in self.pane_listing() {
            let workspace = ws.as_ref().map(|_| self.workspace_label(&ws));
            let content = pane_palette_content(
                agent.title.clone(),
                agent,
                key.multiplexer,
                workspace.as_deref(),
                pane_glyph(self.multiplexers.get(key.multiplexer)),
                self.config.ui.path_style.git_rows,
                None,
            );
            let managed = self.pane_managed(&key, agent);
            let hover = palette_hover(
                &content.title_for_hover,
                agent.kind.as_deref(),
                agent.status.map(|status| status.label()),
                &key.side.label().unwrap_or_else(|| "native".to_string()),
                ws.as_deref(),
                agent.working_directory(),
                Some(&agent.pane_id),
                Some(&agent.terminal_id),
                Some(&managed),
                &format!("attach to this {} pane", key.multiplexer),
            );
            items.push(PaletteItem::pane(
                command_palette::PaneAttach {
                    pane_id: agent.pane_id.clone(),
                    key,
                    workspace: ws.clone(),
                },
                content.primary,
                content.subtitle,
                content.secondary,
                hover,
                agent.kind.as_deref(),
            ));
        }
        for ws in self.workspace_order() {
            let (primary, secondary) = self.workspace_entry_label(&ws);
            items.push(PaletteItem::workspace(ws, primary, secondary));
        }
        for project in &self.projects {
            items.push(PaletteItem::create_worktree(
                project.root.clone(),
                format!("{}: new worktree", project.display_name()),
                format!("project · {}", wsl::display_path(&project.root)),
            ));
        }
        items
    }

    /// The status mark each palette row should paint, resolved the same way
    /// the sidebar resolves one for the same session or unattached pane, so
    /// the two can never disagree.  Kept apart from `palette_items`
    /// so building a row's text and picking its mark stay separate.  `None`
    /// while `[ui.session_display] palette_marks` is off, and for any row
    /// that is neither a session nor a multiplexer's pane.
    ///
    /// Such a pane has no `Session` of its own, so unlike a session row it
    /// carries no latches or live reading of its own: the status its
    /// multiplexer reports is all there is, and its hover is
    /// `managed_tooltip`, what the sidebar's own unattached-agent row shows
    /// too.
    fn palette_marks(&self, items: &[PaletteItem]) -> Vec<Option<(ShownState, String)>> {
        if !self.config.ui.session_display.palette_marks {
            return vec![None; items.len()];
        }
        items
            .iter()
            .map(|item| match &item.action {
                PaletteAction::ActivateSession(id) => {
                    let session = self.sessions.iter().find(|s| s.id == *id)?;
                    let managed = self.session_managed(session);
                    session_status_mark(&RowStatus {
                        pinged: session.needs_attention,
                        done: session.done,
                        activity: self.session_activity(session),
                        managed: managed.as_ref(),
                    })
                },
                PaletteAction::AttachPane(attach) => {
                    let pane = self.find_pane(&attach.key)?;
                    let managed = self.pane_managed(&attach.key, pane);
                    let status = managed.status?;
                    Some((ShownState::from(status), managed_tooltip(&managed)))
                },
                _ => None,
            })
            .collect()
    }

    /// Human label for a workspace: `project / worktree` for a known worktree,
    /// "Home" for the home tab, else the path's final component.
    fn workspace_label(&self, ws: &WorkspaceKey) -> String {
        workspace_label_for(&self.projects, ws)
    }

    /// The (primary, secondary) a workspace palette row shows.
    fn workspace_entry_label(&self, ws: &WorkspaceKey) -> (String, String) {
        let secondary = match ws {
            None => "workspace · home".to_string(),
            Some(path) => format!("workspace · {}", wsl::display_path(path)),
        };
        (self.workspace_label(ws), secondary)
    }

    /// Carry out a chosen palette row.  Actions dispatch exactly as their
    /// binding would; session/workspace rows switch to the target and hand
    /// focus back to the terminal so the user can type straight away; a project
    /// row opens the same new-worktree prompt the sidebar's `+` button does.
    fn run_palette_action(&mut self, ctx: &Context, action: PaletteAction) {
        match action {
            PaletteAction::Run(a) => {
                self.dispatch_action(ctx, BindingAction::Named(a), ActionOrigin::Palette);
            },
            PaletteAction::ActivateSession(id) => {
                self.activate_session_by_id(id);
                self.focus_terminal();
            },
            PaletteAction::SwitchWorkspace(ws) => {
                match ws {
                    None => self.activate_home(ctx),
                    Some(path) => self.activate_worktree(ctx, &path),
                }
                self.focus_terminal();
            },
            PaletteAction::CreateWorktree(root) => {
                if let Some(project_idx) = self.projects.iter().position(|p| p.root == root) {
                    self.modals.pending_create = Some(CreateState::Prompt {
                        project_idx,
                        branch: String::new(),
                        error: None,
                    });
                }
            },
            PaletteAction::SpawnProfile(name) => {
                self.spawn_profile_session(ctx, &name);
                self.focus_terminal();
            },
            PaletteAction::AttachPane(a) => {
                // Switches first, same as both sidebar paths: a refusal is only
                // visible if the workspace it happened in is on screen.
                let switch = self.switch_for_attach(&a.workspace, AttachFocus::Take);
                let unlisted = PaneTarget::unlisted(&a.key, &a.pane_id);
                if self.attach_pane(ctx, a.key, unlisted, &switch, None, AttachFocus::Take) {
                    self.focus_terminal();
                } else {
                    self.current_workspace = switch.from;
                }
            },
        }
    }
}

/// Take this frame's palette cursor jumps off the event queue, honoring
/// rebinds.  The palette owns these keys only while it is up, which is why they
/// are read here rather than dispatched like an ordinary action — and why they
/// can share the sidebar's unmodified Home/End/PageUp/PageDown.
fn consume_palette_keys(ctx: &Context, shortcuts: &crate::shortcut::Shortcuts) -> Vec<NamedAction> {
    ctx.input_mut(|i| {
        let mut jumps = Vec::new();
        i.events.retain(|ev| {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = ev else {
                return true;
            };
            let matched: Vec<NamedAction> = shortcuts
                .matches(*key, *modifiers)
                .into_iter()
                .filter_map(|a| match a {
                    BindingAction::Named(n) if n.is_palette_scoped() => Some(*n),
                    _ => None,
                })
                .collect();
            if matched.is_empty() {
                return true;
            }
            jumps.extend(matched);
            false
        });
        jumps
    })
}

/// A palette column's text, laid out to `max_w`.  Whatever still does not fit
/// is ellipsized, which the caller reads back off the galley's `elided` flag to
/// offer the full text on hover.
fn column_galley(
    ctx: &Context,
    text: &str,
    family: egui::FontFamily,
    size: f32,
    color: Color32,
    max_w: f32,
    wrap: ColumnWrap,
) -> std::sync::Arc<egui::Galley> {
    use egui::text::{LayoutJob, TextFormat};
    let (max_rows, break_anywhere) = wrap.limits();
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat {
        font_id: egui::FontId::new(size, family),
        color,
        ..Default::default()
    });
    job.wrap.max_width = max_w.max(0.0);
    job.wrap.max_rows = max_rows;
    job.wrap.break_anywhere = break_anywhere;
    job.wrap.overflow_character = Some('…');
    ctx.fonts(|f| f.layout_job(job))
}

/// Prose laid out to `max_w`.  Wrapping at spaces reads best, but a word wider
/// than the column overruns it instead of breaking, so a galley that came back
/// too wide is laid out again mid-word.
fn prose_galley(
    ctx: &Context,
    text: &str,
    family: egui::FontFamily,
    size: f32,
    color: Color32,
    max_w: f32,
    max_rows: usize,
) -> std::sync::Arc<egui::Galley> {
    let wrapped = column_galley(ctx, text, family.clone(), size, color, max_w, ColumnWrap::Words {
        max_rows,
    });
    if wrapped.size().x <= max_w {
        return wrapped;
    }
    column_galley(ctx, text, family, size, color, max_w, ColumnWrap::Anywhere { max_rows })
}

/// The accent bar a selected row paints along its left edge.
const PALETTE_SELECTION_BAR_W: f32 = 2.5;

/// The palette's column captions, on the same grid as its rows and outside the
/// scrolling list so they stay put while it moves.
fn paint_palette_header(ui: &mut egui::Ui, theme: &Theme, cols: &PaletteColumns) {
    let s = theme.ui_scale;
    let size = (theme.font_normal - 2.0).max(8.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(cols.width, size + 10.0 * s), egui::Sense::hover());
    let painter = ui.painter().clone();
    let left = rect.left();
    for (text, x, w) in [
        ("DESCRIPTION", cols.desc_x(left), cols.desc),
        ("ACTION", cols.action_x(left), cols.action),
        ("KEYS", cols.keys_x(left), cols.keys),
    ] {
        let g = column_galley(
            ui.ctx(),
            text,
            egui::FontFamily::Proportional,
            size,
            theme.text_muted,
            w,
            ColumnWrap::Clip,
        );
        painter.galley(egui::pos2(x, rect.top() + 2.0 * s), g, theme.text_muted);
    }
    painter.hline(rect.x_range(), rect.bottom(), Stroke::new(1.0_f32, theme.sidebar_border));
}

/// A section heading in the palette list.  Not selectable — the cursor steps
/// over rows only.
fn paint_palette_section(ui: &mut egui::Ui, theme: &Theme, cols: &PaletteColumns, title: &str) {
    let s = theme.ui_scale;
    let size = (theme.font_normal - 2.0).max(8.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(cols.width, size + 14.0 * s), egui::Sense::hover());
    // A heading spans the row rather than the description column, so a narrow
    // grid does not cut it down to the width of the text beside it.
    let g = column_galley(
        ui.ctx(),
        &title.to_uppercase(),
        egui::FontFamily::Proportional,
        size,
        theme.accent,
        cols.width - 2.0 * cols.pad - cols.mark,
        ColumnWrap::Clip,
    );
    ui.painter().galley(
        egui::pos2(cols.desc_x(rect.left()), rect.bottom() - size - 3.0 * s),
        g,
        theme.accent,
    );
}

/// Paint one command-palette row on the shared column grid. Session rows bound
/// their title and middle cell, reserve a location line, and use their detail
/// tooltip across the entire hit target. Action rows retain their existing
/// layout and elided-text tooltip.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_palette_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &Icons<Color32>,
    cols: &PaletteColumns,
    item: &PaletteItem,
    mark: Option<&(ShownState, String)>,
    item_index: usize,
    selected: bool,
) -> egui::Response {
    let s = theme.ui_scale;
    let v_pad = 6.0 * s;
    let ctx = ui.ctx();

    let is_session = item.subtitle.is_some();
    let desc = prose_galley(
        ctx,
        &item.primary,
        egui::FontFamily::Proportional,
        theme.font_normal,
        theme.text,
        cols.desc,
        if is_session { 2 } else { usize::MAX },
    );
    let subtitle_size = (theme.font_normal - 1.0).max(8.0);
    let subtitle = item.subtitle.as_deref().map(|text| {
        column_galley(
            ctx,
            text,
            egui::FontFamily::Proportional,
            subtitle_size,
            theme.text,
            cols.desc,
            ColumnWrap::Clip,
        )
    });
    let action = if is_session {
        prose_galley(
            ctx,
            &item.secondary,
            egui::FontFamily::Proportional,
            theme.font_normal,
            theme.text_dim,
            cols.action,
            3,
        )
    } else {
        column_galley(
            ctx,
            &item.secondary,
            egui::FontFamily::Proportional,
            theme.font_normal,
            theme.text_dim,
            cols.action,
            cols.token_wrap(),
        )
    };
    let keys = column_galley(
        ctx,
        &item.keys,
        egui::FontFamily::Monospace,
        theme.font_normal,
        theme.accent,
        cols.keys,
        cols.token_wrap(),
    );
    let elided_hover = item
        .hover
        .is_none()
        .then(|| {
            elided_hover(&[
                (desc.elided, item.primary.as_str()),
                (
                    subtitle.as_ref().is_some_and(|subtitle| subtitle.elided),
                    item.subtitle.as_deref().unwrap_or_default(),
                ),
                (action.elided, item.secondary.as_str()),
                (keys.elided, item.keys.as_str()),
            ])
        })
        .flatten();
    let subtitle_h = subtitle.as_ref().map_or(0.0, |subtitle| {
        let font = egui::FontId::new(subtitle_size, egui::FontFamily::Proportional);
        subtitle.size().y.max(ctx.fonts(|fonts| fonts.row_height(&font)))
    });
    let description_h = desc.size().y + subtitle_h;
    let row_h = (description_h.max(action.size().y).max(keys.size().y) + 2.0 * v_pad).round();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(cols.width, row_h), egui::Sense::click());
    let painter = ui.painter().clone();

    if selected {
        let wash = Color32::from_rgba_unmultiplied(
            theme.accent.r(),
            theme.accent.g(),
            theme.accent.b(),
            46,
        );
        painter.rect_filled(rect, 5.0 * s, wash);
        let bar = egui::Rect::from_min_size(
            rect.left_top(),
            egui::vec2(PALETTE_SELECTION_BAR_W * s, rect.height()),
        );
        painter.rect_filled(bar, 0.0, theme.accent);
    } else if resp.hovered() {
        painter.rect_filled(rect, 5.0 * s, theme.row_hover_bg);
    }

    // Top-aligned, so a wrapped description's first line shares a baseline with
    // the single-line columns beside it.
    let (left, top) = (rect.left(), rect.top() + v_pad);
    if let Some((mark, hint)) = mark {
        let mark_rect = egui::Rect::from_min_size(
            egui::pos2(cols.mark_x(left), top),
            row_status_icon_size(theme),
        );
        paint_status_mark(ui, *mark, icons, mark_rect, theme);
        if item.hover.is_none() {
            let mark_id = ui.id().with(("palette_status_mark", item_index));
            ui.interact(mark_rect, mark_id, egui::Sense::hover()).on_hover_text(hint.clone());
        }
    }
    let subtitle_y = top + desc.size().y;
    painter.galley(egui::pos2(cols.desc_x(left), top), desc, theme.text);
    if let Some(subtitle) = subtitle {
        painter.galley(egui::pos2(cols.desc_x(left), subtitle_y), subtitle, theme.text);
    }
    painter.galley(egui::pos2(cols.action_x(left), top), action, theme.text_dim);
    painter.galley(egui::pos2(cols.keys_x(left), top), keys, theme.accent);

    match &item.hover {
        Some(text) => resp.on_hover_text(text),
        None => match elided_hover {
            Some(text) => resp.on_hover_text(text),
            None => resp,
        },
    }
}

/// The palette's footer, naming the keys actually bound to its cursor moves so
/// a rebind shows up here instead of the hint quietly going stale.
fn palette_hint(shortcuts: &crate::shortcut::Shortcuts) -> String {
    let mut parts = vec!["↑↓ move".to_string()];
    for (action, label) in [
        (NamedAction::PaletteTop(action::PaletteTop), "top"),
        (NamedAction::PaletteBottom(action::PaletteBottom), "bottom"),
        (NamedAction::PalettePageUp(action::PalettePageUp), "page up"),
        (NamedAction::PalettePageDown(action::PalettePageDown), "page down"),
    ] {
        if let Some(key) = command_palette::first_key(shortcuts, action) {
            parts.push(format!("{key} {label}"));
        }
    }
    parts.push("Enter run".into());
    parts.push("Esc close".into());
    parts.join(" · ")
}

/// What a palette column does with text too wide for it.  epaint overruns the
/// column rather than splitting a word unless told it may break anywhere, so
/// the choice follows the content: prose can rely on its spaces, a lone
/// identifier or key chord cannot.
#[derive(Clone, Copy)]
enum ColumnWrap {
    /// One line, ellipsized at the column edge — the scannable default.
    Clip,
    /// Wrap at word boundaries, stopping after `max_rows`.
    Words { max_rows: usize },
    /// Wrap mid-token if that is the only way to stay inside the column,
    /// stopping after `max_rows`.
    Anywhere { max_rows: usize },
}

impl ColumnWrap {
    fn limits(self) -> (usize, bool) {
        match self {
            Self::Clip => (1, true),
            Self::Words { max_rows } => (max_rows, false),
            Self::Anywhere { max_rows } => (max_rows, true),
        }
    }
}

/// The hover text for a row: the full text of whatever its columns had to cut,
/// and nothing at all when everything already reads in place.
fn elided_hover(columns: &[(bool, &str)]) -> Option<String> {
    let full: Vec<&str> =
        columns.iter().filter(|(elided, _)| *elided).map(|(_, text)| *text).collect();
    (!full.is_empty()).then(|| full.join("\n"))
}

/// How wide the palette's content may be.  A window too narrow for the
/// comfortable width sizes the palette against the window instead, so the modal
/// keeps a margin either side rather than running past both edges.
fn palette_content_width(scale: f32, screen_w: f32) -> f32 {
    let budget = screen_w * PALETTE_SCREEN_FRACTION - 2.0 * modal_pad_x(scale);
    budget.min(PALETTE_WIDTH * scale).max(0.0)
}

/// The palette's comfortable content width, and the share of a window it may
/// take instead when the window cannot hold that.
const PALETTE_WIDTH: f32 = 760.0;

const PALETTE_SCREEN_FRACTION: f32 = 0.8;

/// The action and keys columns' fixed widths, and the narrowest the description
/// still reads at beside them.
const PALETTE_ACTION_W: f32 = 200.0;

const PALETTE_KEYS_W: f32 = 180.0;

const PALETTE_DESC_MIN: f32 = 160.0;

/// Clear space between a row's status mark and the description after it.
const PALETTE_MARK_GAP: f32 = 6.0;

/// Geometry for the palette's `description | action | keys` grid.  Every row and
/// the header lay out against the same widths, so the columns line up down the
/// list instead of each row packing its own way.  A grid with room for the fixed
/// widths gets them; a tighter one shrinks all three by the same factor and
/// wraps their text, rather than letting the last column run off the edge.
pub(super) struct PaletteColumns {
    width: f32,
    pad: f32,
    /// The leading status-mark gutter: the mark's own footprint plus the space
    /// after it.  Every row claims it, marked or not, so the descriptions line
    /// up whether or not a row has a mark to show.
    mark: f32,
    desc: f32,
    action: f32,
    keys: f32,
    gap: f32,
    /// Set once the grid is tighter than its fixed widths, at which point every
    /// column wraps instead of ellipsizing.
    narrow: bool,
}

impl PaletteColumns {
    pub(super) fn new(scale: f32, width: f32) -> Self {
        let pad = 10.0 * scale;
        let gap = 14.0 * scale;
        let mark = (ROW_STATUS_ICON_W + PALETTE_MARK_GAP) * scale;
        let content = (width - 2.0 * pad - mark - 2.0 * gap).max(0.0);
        let action = PALETTE_ACTION_W * scale;
        let keys = PALETTE_KEYS_W * scale;
        let comfortable = PALETTE_DESC_MIN * scale + action + keys;
        if content >= comfortable {
            let desc = content - action - keys;
            return Self { width, pad, mark, desc, action, keys, gap, narrow: false };
        }
        let shrink = content / comfortable;
        Self {
            width,
            pad,
            mark,
            desc: PALETTE_DESC_MIN * scale * shrink,
            action: action * shrink,
            keys: keys * shrink,
            gap,
            narrow: true,
        }
    }

    /// How the action and keys columns lay out.  Their text is one unbroken
    /// token, so a narrow grid has to split it mid-word; a comfortable one
    /// keeps every row one line tall and ellipsizes the overflow.
    fn token_wrap(&self) -> ColumnWrap {
        if self.narrow { ColumnWrap::Anywhere { max_rows: usize::MAX } } else { ColumnWrap::Clip }
    }

    /// Where a row's status mark sits.  Clear of the selected row's accent
    /// bar, which is painted hard against the row's left edge.
    fn mark_x(&self, left: f32) -> f32 {
        left + self.pad
    }

    fn desc_x(&self, left: f32) -> f32 {
        self.mark_x(left) + self.mark
    }

    fn action_x(&self, left: f32) -> f32 {
        self.desc_x(left) + self.desc + self.gap
    }

    fn keys_x(&self, left: f32) -> f32 {
        self.action_x(left) + self.action + self.gap
    }
}

pub(super) struct PaletteSessionContent {
    pub(super) primary: String,
    pub(super) subtitle: String,
    pub(super) secondary: String,
    pub(super) title_for_hover: String,
}

/// The middle column's words, most general first: where the row comes from,
/// what runs in it, what that is doing.  The kind is spelled out whether or
/// not the title repeats it, so every row in one state reads identically.
fn session_middle(
    lead: Option<&str>,
    kind: Option<&str>,
    status: Option<&str>,
    fallback: &str,
) -> String {
    let mut parts: Vec<String> = lead.map(str::to_owned).into_iter().collect();
    parts.extend(kind.map(str::to_lowercase));
    parts.extend(status.map(str::to_owned));
    if kind.is_none() && status.is_none() {
        parts.push(fallback.to_string());
    }
    parts.join(" · ")
}

/// The second line names the workspace, blanked only on an exact string
/// match — a title that merely reads like a directory into the workspace is
/// not matched against it.  Only a titleless row gives the first line up to
/// the workspace, and then the second has nothing left to say.
fn native_palette_content(
    title: String,
    workspace: String,
    kind: Option<&str>,
    status: Option<&str>,
    fallback: &str,
) -> PaletteSessionContent {
    let primary = if title.trim().is_empty() { workspace.clone() } else { title };
    PaletteSessionContent {
        title_for_hover: primary.clone(),
        secondary: session_middle(None, kind, status, fallback),
        subtitle: if workspace == primary { String::new() } else { workspace },
        primary,
    }
}

/// The glyph a palette row names `multiplexer`'s panes with.
fn pane_glyph(multiplexer: &Multiplexer) -> &str {
    let (icon, default) = multiplexer.icon();
    icon.or_glyph(default.as_str())
}

fn pane_subtitle(glyph: &str, location: Option<&str>) -> String {
    location.map(|location| format!("{glyph} {location}")).unwrap_or_else(|| glyph.to_string())
}

pub(super) fn pane_palette_content(
    title: Option<String>,
    agent: &Pane,
    multiplexer: MultiplexerKind,
    workspace: Option<&str>,
    glyph: &str,
    cwd_style: PathStyle,
    cwd_home: Option<&str>,
) -> PaletteSessionContent {
    let cwd = agent.working_directory();
    let abbreviated_cwd = cwd.map(|cwd| path_style::render(cwd, cwd_style, cwd_home));
    let (primary, title_for_hover) =
        if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
            (title.clone(), title)
        } else {
            match workspace {
                Some(workspace) => (workspace.to_string(), workspace.to_string()),
                None => match (abbreviated_cwd.clone(), cwd) {
                    (Some(cwd), Some(full_cwd)) => (cwd, full_cwd.to_string()),
                    _ => ("Home".to_string(), "Home".to_string()),
                },
            }
        };
    // A workspace names the project a pane belongs to, which its path does
    // not, so it holds the second line whatever the title says.
    let location = match workspace {
        Some(workspace) => Some(workspace.to_string()),
        None => abbreviated_cwd.clone().filter(|_| cwd != Some(primary.as_str())),
    };
    let subtitle = match location.filter(|location| *location != primary) {
        Some(location) => pane_subtitle(glyph, Some(&location)),
        None => glyph.to_string(),
    };
    PaletteSessionContent {
        primary,
        subtitle,
        secondary: session_middle(
            Some(&multiplexer.to_string()),
            agent.kind.as_deref(),
            agent.status.map(|status| status.label()),
            "shell",
        ),
        title_for_hover,
    }
}

fn palette_hover(
    title: &str,
    kind: Option<&str>,
    status: Option<&str>,
    side: &str,
    workspace: Option<&Path>,
    cwd: Option<&str>,
    pane_id: Option<&str>,
    terminal_id: Option<&str>,
    managed: Option<&Managed>,
    activation: &str,
) -> String {
    let mut lines = vec![format!("Title: {title}")];
    lines.extend(kind.map(|kind| format!("Kind: {kind}")));
    lines.extend(status.map(|status| format!("Status: {status}")));
    lines.push(format!("Side: {side}"));
    lines.extend(workspace.map(|workspace| format!("Workspace: {}", wsl::display_path(workspace))));
    lines.extend(cwd.map(|cwd| format!("Cwd: {cwd}")));
    lines.extend(pane_id.map(|pane_id| format!("Pane: {pane_id}")));
    lines.extend(terminal_id.map(|terminal_id| format!("Terminal: {terminal_id}")));
    lines.extend(managed.map(managed_tooltip));
    lines.push(format!("Activate: {activation}"));
    lines.join("\n")
}

fn session_fallback_kind(kind: &SessionKind) -> &'static str {
    match kind {
        SessionKind::Shell => "shell",
        SessionKind::Diff { .. } => "diff",
        SessionKind::Scratchpad { .. } => "scratchpad",
    }
}

/// Whether a shell row can say what it is doing.  Only a plain shell has a
/// foreground job to read; a diff or scratchpad row has no process behind it
/// and would be inventing a state.
fn shell_state_for(kind: &SessionKind, busy: bool) -> Option<&'static str> {
    match kind {
        SessionKind::Shell => Some(if busy { "busy" } else { "idle" }),
        SessionKind::Diff { .. } | SessionKind::Scratchpad { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{listed_agent, pane_key, titled_agent as titled};

    /// The mark has a gutter of its own ahead of the description.  A gutter
    /// only as wide as the glyph leaves the label butted against the mark.
    #[test]
    fn the_palette_mark_gutter_clears_the_description() {
        let theme = Theme::from_config(&Config::default());
        let cols = PaletteColumns::new(theme.ui_scale, PALETTE_WIDTH);
        let mark_right = cols.mark_x(0.0) + row_status_icon_size(&theme).x;
        let desc_left = cols.desc_x(0.0);
        assert!(
            desc_left > mark_right,
            "the description starts at {desc_left}, inside a mark ending at {mark_right}"
        );
    }

    /// The mark starts past the accent bar a selected row paints along its left
    /// edge, so selecting a row does not clip its mark.
    #[test]
    fn the_palette_mark_clears_the_selected_rows_accent_bar() {
        let theme = Theme::from_config(&Config::default());
        let cols = PaletteColumns::new(theme.ui_scale, PALETTE_WIDTH);
        assert!(cols.mark_x(0.0) > PALETTE_SELECTION_BAR_W * theme.ui_scale);
    }

    #[test]
    fn specific_herdr_titles_keep_workspace_and_status_separate() {
        let agent = Pane {
            status: Some(PaneStatus::Working),
            ..titled(Some("claude"), Some("fix the wrap bug"))
        };
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            Some("alacritree / master"),
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!(
            (content.primary, content.subtitle, content.secondary),
            (
                "fix the wrap bug".into(),
                "◆ alacritree / master".into(),
                "herdr · claude · working".into(),
            )
        );
    }

    /// herdr's own word for the pane is what the row is called, even when it
    /// says no more than the kind already in the middle column: the line under
    /// it is the workspace, which is the part that tells two rows apart.
    #[test]
    fn a_herdr_title_repeating_its_kind_keeps_the_workspace_below_it() {
        let agent = titled(Some("claude"), Some("claude"));
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            Some("renamed / main"),
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!(
            (content.primary, content.subtitle, content.secondary),
            ("claude".into(), "◆ renamed / main".into(), "herdr · claude · idle".into())
        );
    }

    /// A title herdr took from the pane's own directory says nothing about
    /// which project holds it, so the workspace label still gets its line.
    #[test]
    fn a_herdr_title_naming_its_directory_keeps_the_workspace_below_it() {
        let agent = Pane {
            cwd: Some("/home/dev/Git/devkit".into()),
            ..titled(Some("claude"), Some("devkit"))
        };
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            Some("devkit / main"),
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!(
            (content.primary, content.subtitle),
            ("devkit".into(), "◆ devkit / main".into())
        );
    }

    #[test]
    fn untitled_herdr_panes_without_a_directory_use_home() {
        let agent = listed_agent(Some("claude"));
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            None,
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!(
            (content.primary, content.subtitle, content.secondary),
            ("Home".into(), "◆".into(), "herdr · claude · idle".into(),)
        );
    }

    /// Two unmatched panes titled with the same cwd read identically down
    /// both lines, so nothing in the row's identity tells them apart; a query
    /// still reaches each one on its own through fields the row's identity
    /// does not carry, like the terminal id.
    #[test]
    fn duplicate_unmatched_herdr_panes_share_a_primary_but_stay_searchable() {
        let chezmoi = |terminal_id: &str, pane_id: &str, status| Pane {
            terminal_id: terminal_id.into(),
            pane_id: pane_id.into(),
            status: Some(status),
            cwd: Some("~/.local/share/chezmoi".into()),
            ..titled(Some("codex"), Some("~/.local/share/chezmoi"))
        };
        let panes =
            [chezmoi("t1", "w7:p1", PaneStatus::Working), chezmoi("t2", "w7:p3", PaneStatus::Idle)];
        let side = Side::Wsl("kali-linux".into());
        let mut items = Vec::new();
        for agent in &panes {
            let workspace = None;
            let content = pane_palette_content(
                agent.title.clone(),
                agent,
                MultiplexerKind::Herdr,
                None,
                "◆",
                PathStyle::Fish,
                None,
            );
            items.push(PaletteItem::pane(
                command_palette::PaneAttach {
                    key: pane_key(side.clone(), &agent.terminal_id),
                    pane_id: agent.pane_id.clone(),
                    workspace,
                },
                content.primary,
                content.subtitle,
                content.secondary,
                "hover".into(),
                agent.kind.as_deref(),
            ));
        }
        assert_eq!(items[0].primary, "~/.local/share/chezmoi");
        assert_eq!(items[1].primary, "~/.local/share/chezmoi");
        assert_eq!(items[0].subtitle.as_deref(), Some("◆"));
        assert_eq!(items[1].subtitle.as_deref(), Some("◆"));
        assert_eq!(items[0].secondary, "herdr · codex · working");
        assert_eq!(items[1].secondary, "herdr · codex · idle");
        let mut palette = CommandPalette::new();
        let ranked = palette.rank(&items);
        assert_eq!(command_palette::group(&items, &ranked), vec![(
            command_palette::PaletteSection::MultiplexerPanes,
            vec![0, 1],
        )]);
        for (query, expected) in
            [("w7:p1", 0), ("w7:p3", 1), ("working", 0), ("idle", 1), ("chezmoi", 0)]
        {
            palette.clear_query();
            palette.query_mut().push_str(query);
            assert_eq!(palette.rank(&items).first(), Some(&expected));
        }
    }

    #[test]
    fn untitled_herdr_panes_promote_home_before_terminal_id_fallback() {
        let agent =
            Pane { kind: None, title: None, cwd: None, foreground_cwd: None, ..listed_agent(None) };
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            None,
            "◆",
            PathStyle::Fish,
            None,
        );
        let item = PaletteItem::pane(
            command_palette::PaneAttach {
                key: pane_key(Side::Native, &agent.terminal_id),
                pane_id: agent.pane_id.clone(),
                workspace: None,
            },
            content.primary,
            content.subtitle,
            content.secondary,
            "hover".into(),
            agent.kind.as_deref(),
        );
        assert_eq!(item.primary, "Home");
        assert_eq!(item.subtitle.as_deref(), Some("◆"));
    }

    /// An attached row always resolves a title, even an empty one from a PTY
    /// that never set an OSC title — `session.rs` leaves it unfiltered. The
    /// blank string must fall through exactly like no title at all.
    #[test]
    fn attached_untitled_herdr_panes_promote_home_and_keep_their_glyph() {
        let agent = listed_agent(None);
        let content = pane_palette_content(
            Some(String::new()),
            &agent,
            MultiplexerKind::Herdr,
            None,
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!((content.primary, content.subtitle), ("Home".into(), "◆".into()));
    }

    /// The palette names a row the way the sidebar does: herdr's title when it
    /// has one, else the session's own PTY title, so a pane herdr reports no
    /// title for still keeps that name instead of losing it to the workspace.
    #[test]
    fn an_attached_panes_pty_title_survives_a_titleless_herdr_report() {
        let agent = listed_agent(None);
        let content = pane_palette_content(
            Some("vim src/main.rs".into()),
            &agent,
            MultiplexerKind::Herdr,
            Some("alacritree / master"),
            "◆",
            PathStyle::Fish,
            None,
        );
        assert_eq!(
            (content.primary, content.subtitle),
            ("vim src/main.rs".into(), "◆ alacritree / master".into())
        );
    }

    #[test]
    fn configured_workspace_label_and_herdr_glyph_survive_untitled_rows() {
        let agent = listed_agent(Some("claude"));
        let content = pane_palette_content(
            agent.title.clone(),
            &agent,
            MultiplexerKind::Herdr,
            Some("◆ renamed / main"),
            "✦",
            PathStyle::Fish,
            None,
        );
        assert_eq!((content.primary, content.subtitle), ("◆ renamed / main".into(), "✦".into()));
    }

    /// A herdr-backed row names herdr ahead of the agent, so the palette says
    /// where a row comes from without the reader decoding a glyph.
    #[test]
    fn a_herdr_rows_middle_column_leads_with_herdr() {
        assert_eq!(
            session_middle(Some("herdr"), Some("codex"), Some("working"), "shell"),
            "herdr · codex · working"
        );
    }

    /// The palette asks for a comfortable fixed width, but a window too narrow
    /// to hold it must still show the whole modal, margins included.
    #[test]
    fn the_palette_shrinks_to_fit_a_narrow_window() {
        assert_eq!(
            palette_content_width(1.0, 1920.0),
            PALETTE_WIDTH,
            "a wide window gets the comfortable width unchanged"
        );

        for screen in [1000.0_f32, 820.0, 700.0, 520.0, 400.0] {
            let outer = palette_content_width(1.0, screen) + 2.0 * modal_pad_x(1.0);
            assert!(
                outer <= screen * PALETTE_SCREEN_FRACTION + 0.5,
                "at {screen}px the modal is {outer}px, past its share of the window"
            );
        }
    }

    /// Too narrow for the fixed grid, every column shrinks by the same factor
    /// rather than the last one running off the edge.
    #[test]
    fn narrow_columns_shrink_together_and_stay_inside_the_row() {
        for width in [520.0_f32, 440.0, 360.0, 240.0, 120.0] {
            let cols = PaletteColumns::new(1.0, width);
            assert!(cols.narrow, "at {width}px the fixed grid cannot fit");
            let right = cols.keys_x(0.0) + cols.keys;
            assert!(
                right <= width - cols.pad + 0.5,
                "at {width}px the keys column ends at {right}, past the row"
            );
            assert!(cols.desc > 0.0 && cols.action > 0.0 && cols.keys > 0.0);
            assert!((cols.action / cols.desc - 200.0 / 160.0).abs() < 1e-3);
            assert!((cols.keys / cols.desc - 180.0 / 160.0).abs() < 1e-3);
        }
    }

    /// The grid does not jump as it crosses from fixed to proportional.
    #[test]
    fn the_columns_are_continuous_across_the_narrow_threshold() {
        let threshold = (1..2000)
            .map(|w| w as f32)
            .find(|&w| !PaletteColumns::new(1.0, w).narrow)
            .expect("the grid reaches its fixed widths at some width");
        let fixed = PaletteColumns::new(1.0, threshold);
        let narrow = PaletteColumns::new(1.0, threshold - 1.0);
        assert!(!fixed.narrow && narrow.narrow);
        assert!((fixed.desc - narrow.desc).abs() < 1.0);
        assert!((fixed.action - narrow.action).abs() < 1.0);
        assert!((fixed.keys - narrow.keys).abs() < 1.0);
    }

    /// The row never compares its title against its own path, so a title that
    /// reads like a directory is named no differently than one that does
    /// not — the workspace label still keeps the line under it.
    #[test]
    fn native_titles_naming_their_directory_keep_the_workspace_label() {
        let content = native_palette_content(
            "/repo/feature".into(),
            "◆ renamed / main".into(),
            Some("claude"),
            Some("idle"),
            "shell",
        );
        assert_eq!(content.primary, "/repo/feature");
        assert_eq!(content.subtitle, "◆ renamed / main");
    }

    /// Nothing but the workspace label is left to name a titleless row, and
    /// once it takes the first line the second would only repeat it.
    #[test]
    fn a_titleless_native_row_is_named_by_its_workspace() {
        let content =
            native_palette_content(String::new(), "renamed / main".into(), None, None, "shell");
        assert_eq!((content.primary, content.subtitle), ("renamed / main".into(), String::new()));
    }

    #[test]
    fn native_home_titles_reach_palette_items() {
        let content = native_palette_content(
            "claude".into(),
            "Home".into(),
            Some("claude"),
            Some("idle"),
            "shell",
        );
        let item = PaletteItem::session(
            1,
            content.primary,
            content.subtitle,
            content.secondary,
            "hover".into(),
            Some("claude"),
            None,
            None,
        );
        assert_eq!(item.primary, "claude");
        assert_eq!(item.subtitle.as_deref(), Some("Home"));
        assert_eq!(item.secondary, "claude · idle");
    }

    /// The kind is spelled out even when the title already carries it: two
    /// rows in one state must read the same, and one repeated word is a
    /// cheaper price than a column that changes shape per row.
    #[test]
    fn the_middle_column_spells_the_kind_out_beside_a_title_that_shares_it() {
        assert_eq!(session_middle(None, Some("claude"), Some("idle"), "shell"), "claude · idle");
    }

    /// A lead with nothing after it still names itself rather than falling
    /// through to the fallback: the row is herdr-backed whatever else is
    /// unknown about it.
    #[test]
    fn a_lead_survives_an_otherwise_empty_middle_column() {
        assert_eq!(session_middle(Some("herdr"), None, None, "shell"), "herdr · shell");
    }

    /// A shell row reports whether a job holds the terminal, which is the one
    /// thing about a shell worth reading off a list.  A kind with no
    /// foreground job of its own reports nothing rather than a state it
    /// cannot observe.
    #[test]
    fn only_a_plain_shell_reports_a_busy_state() {
        assert_eq!(shell_state_for(&SessionKind::Shell, true), Some("busy"));
        assert_eq!(shell_state_for(&SessionKind::Shell, false), Some("idle"));
        assert_eq!(shell_state_for(&SessionKind::Scratchpad { path: PathBuf::new() }, true), None);
        assert_eq!(shell_state_for(&SessionKind::Diff { key: "k".into() }, true), None);
    }

    #[test]
    fn native_shells_keep_a_shell_middle_cell() {
        let content = native_palette_content("terminal".into(), "Home".into(), None, None, "shell");
        assert_eq!(content.secondary, "shell");
    }

    /// Every scratchpad row carries the same one-word title, so the workspace
    /// label under it is what tells one from another.
    #[test]
    fn a_scratchpad_reads_its_kind_over_its_workspace() {
        let content = native_palette_content(
            "scratchpad".into(),
            "◆ renamed / main".into(),
            Some("scratchpad"),
            None,
            "shell",
        );
        assert_eq!(content.primary, "scratchpad");
        assert_eq!(content.subtitle, "◆ renamed / main");
        assert_eq!(content.secondary, "scratchpad");
    }

    #[test]
    fn the_palette_never_asks_for_a_negative_width() {
        assert!(palette_content_width(1.0, 10.0) >= 0.0);
    }

    /// A window wide enough keeps the fixed grid, so the columns line up exactly
    /// where they always have.
    #[test]
    fn wide_columns_keep_the_fixed_grid() {
        let cols = PaletteColumns::new(1.0, 760.0);
        assert_eq!(cols.action, 200.0);
        assert_eq!(cols.keys, 180.0);
        let mark = ROW_STATUS_ICON_W + PALETTE_MARK_GAP;
        assert_eq!(cols.desc, 760.0 - 2.0 * 10.0 - mark - 2.0 * 14.0 - 380.0);
        assert!(!cols.narrow, "a wide palette ellipsizes its columns rather than wrapping them");
    }

    #[test]
    fn only_a_cut_column_offers_its_full_text_on_hover() {
        assert_eq!(elided_hover(&[(false, "Copy"), (false, "Copy"), (false, "Ctrl+C")]), None);
        assert_eq!(
            elided_hover(&[
                (false, "Increase the font size"),
                (true, "IncreaseFontSize"),
                (true, "Ctrl+Plus, Ctrl+="),
            ]),
            Some("IncreaseFontSize\nCtrl+Plus, Ctrl+=".to_string())
        );
    }
}
