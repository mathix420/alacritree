use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::search::Match;
use alacritty_terminal::term::{Term, TermDamage};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape};
use egui::{
    Color32, CursorIcon, Event, FontFamily, FontId, ImeEvent, Modifiers, MouseWheelUnit,
    PointerButton, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2,
};

use crate::builtin_font::{BuiltinGlyphCache, Metrics, is_builtin_glyph};
use crate::clipboard::{self, Target};
use crate::color_glyph::{CachedColorGlyph, ColorGlyphCache};
use crate::colors::{background, default_background, foreground, resolve, rgb_to_color32};
use crate::config::{Config, Palette};
use crate::decoration_sprites;
use crate::fonts::{BOLD_FAMILY, BOLD_ITALIC_FAMILY, ITALIC_FAMILY};
use crate::glyph_cache::{Face, GlyphCache, MAX_EXTRA_CELLS, growth_offset, may_grow};
use crate::grid_gl::{Frame as GridFrame, GpuGrid};
use crate::grid_instances::RunView;
use crate::input::{associated_text, event_to_bytes};
use crate::jobs;
use crate::links::{self, Link};
use crate::mouse;
use crate::paste;
use crate::session::{EventProxy, Session, SessionId, SessionKind, TermSize};

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    session: &mut Session,
    config: &Config,
    face_metrics: &crate::fonts::FaceMetrics,
    allow_focus: bool,
    builtin_glyphs: &mut BuiltinGlyphCache,
    ime: &mut crate::ime::Ime,
    color_glyphs: &mut ColorGlyphCache,
    glyphs: &mut GlyphCache,
    snapshot: &mut GridSnapshot,
    gpu: Option<&GpuGrid>,
    detached_jobs: &mut Vec<jobs::Job<()>>,
) -> Response {
    let font_id = FontId::monospace(config.font.egui_size());
    let (cell_w_pt, cell_h_pt) =
        ui.ctx().fonts(|f| (f.glyph_width(&font_id, 'M'), f.row_height(&font_id)));
    // `Fonts` exposes no ascent, and deriving one from the face would miss the
    // quantization `FontImpl::new` applies when it stores its own.
    // `font_ascent` on a laid-out glyph is the number epaint draws at, and the
    // grid's own cache already holds that galley at this size, so reading it
    // here is a lookup rather than a fresh layout every frame.
    let font_ascent_pt = glyphs
        .get(ui.ctx(), 'M', Face::Normal, font_id.size)
        .rows
        .first()
        .and_then(|row| row.glyphs.first())
        .map_or(cell_h_pt, |glyph| glyph.font_ascent);
    // Floor cell size to whole device pixels — matches alacritty's
    // `compute_cell_size`.  Without this, fractional cell widths combined
    // with egui's AA fringe leave visible seams between adjacent cells.
    // `font.offset` is added in pixel space so the round-trip through ppp is
    // identical to alacritty (which adds offset to the integer cell metrics).
    let ppp = ui.ctx().pixels_per_point();
    let offset_x = config.font.offset.x as f32;
    let offset_y = config.font.offset.y as f32;
    let cell_w = ((cell_w_pt * ppp).floor() + offset_x).max(1.0) / ppp;
    let cell_h = ((cell_h_pt * ppp).floor() + offset_y).max(1.0) / ppp;

    let pad_x = config.window.padding_x;
    let pad_y = config.window.padding_y;
    let avail = ui.available_size();
    let inner_w = (avail.x - 2.0 * pad_x).max(cell_w);
    let inner_h = (avail.y - 2.0 * pad_y).max(cell_h);
    let cols = (inner_w / cell_w).floor().max(1.0) as usize;
    let rows = (inner_h / cell_h).floor().max(1.0) as usize;
    session.resize(TermSize::new(cols, rows), (cell_w, cell_h));

    if pad_x > 0.0 || pad_y > 0.0 {
        ui.add_space(pad_y);
    }
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(
            cols as f32 * cell_w + 2.0 * pad_x,
            rows as f32 * cell_h + (if pad_y > 0.0 { 0.0 } else { 0.0 }),
        ),
        Sense::click_and_drag(),
    );
    // Snap the grid origin so column/row boundaries stay on integer pixels.
    let snap = |v: f32| (v * ppp).round() / ppp;
    let rect = Rect::from_min_size(
        Pos2::new(snap(rect.min.x + pad_x), snap(rect.min.y)),
        Vec2::new(cols as f32 * cell_w, rows as f32 * cell_h),
    );

    if allow_focus && !response.has_focus() {
        response.request_focus();
    }
    ime.retarget(session.id);

    let painter = ui.painter_at(rect);

    let peek = peek_term(ui, &response, session, rect, cell_w, cell_h, cols, rows);
    if peek.link.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    // Apps that negotiate mouse tracking want the raw button/motion stream, not
    // local selection — matching alacritty, Shift is the escape hatch that still
    // selects text while the app is in mouse mode.
    let mouse_mode = peek.mode.intersects(TermMode::MOUSE_MODE);
    let report_mouse = mouse_mode && !ui.input(|i| i.modifiers.shift);
    if report_mouse {
        handle_mouse_reporting(ui, session, rect, cell_w, cell_h, cols, rows, &peek);
    } else {
        handle_selection(
            ui,
            &response,
            session,
            config,
            rect,
            cell_w,
            cell_h,
            cols,
            rows,
            peek.link.as_ref(),
            detached_jobs,
        );
    }
    handle_wheel_scroll(ui, &response, session, config, rect, cell_w, cell_h, cols, rows, &peek);
    dispatch_input(ui, &response, session, ime, allow_focus, peek.mode);
    // Built-in renderer expects the *unadjusted* pixel cell size so it can
    // re-apply `font.offset` itself — passing `cell_w * ppp` (which already
    // includes the offset) would double-add it.  Descent is zero here: the
    // alacritty renderer's `top - descent` math collapses when descent is
    // zero, and `paint_builtin_glyph` positions images using that simplified
    // form.
    let metrics = Metrics {
        average_advance: (cell_w_pt * ppp).floor() as f64,
        line_height: (cell_h_pt * ppp).floor() as f64,
        descent: 0.0,
    };
    // The guard is a temporary so it is dropped at the end of this statement:
    // nothing below may run while the terminal is locked.
    snapshot.capture(
        &mut session.term.lock(),
        config,
        session.id,
        peek.link.as_ref().map(|l| &l.bounds),
        // The preedit overlay replaces the cursor while composing
        // (alacritty hides it the same way, display/content.rs).
        ime.preedit().is_some(),
    );
    match gpu.filter(|gpu| config.ui.gpu_grid && !gpu.unavailable()) {
        Some(gpu) => {
            paint_grid_gpu(
                gpu,
                &painter,
                rect,
                snapshot,
                config,
                face_metrics,
                font_ascent_pt,
                cell_w,
                cell_h,
                cols,
                rows,
                ppp,
                &metrics,
                builtin_glyphs,
                color_glyphs,
                glyphs,
                ui.ctx(),
            );
            if let Some(cursor) = &snapshot.cursor {
                paint_cursor(&painter, rect, cursor, cell_w, cell_h, &font_id);
            }
        },
        None => paint_grid(
            &painter,
            rect,
            snapshot,
            config,
            &font_id,
            cell_w,
            cell_h,
            ppp,
            &metrics,
            builtin_glyphs,
            color_glyphs,
            glyphs,
            ui.ctx(),
        ),
    }

    let preedit_caret = ime
        .preedit()
        .map(|p| p.to_owned())
        .and_then(|p| paint_preedit(&painter, rect, session, config, &font_id, cell_w, cell_h, &p));

    if allow_focus && response.has_focus() {
        // Setting `PlatformOutput::ime` is what makes egui-winit call
        // `set_ime_allowed(true)` — without it the OS IME never engages.
        // The rect drives `set_ime_cursor_area`, so the candidate window
        // follows the caret like alacritty's `update_ime_position`
        // (TextEdit passes its whole widget rect there, which for a
        // fullscreen terminal would pin the popup to the window corner).
        let caret = preedit_caret
            .or_else(|| cursor_cell_rect(snapshot, rect, cell_w, cell_h))
            .unwrap_or(rect);
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput { rect: caret, cursor_rect: caret });
        });
    }

    response
}

/// Drain this frame's keyboard and IME events into the terminal.
///
/// Runs ahead of the paint so a keystroke reaches the PTY without queueing
/// behind a full-screen grid walk, and so the selection clear and snap-to-
/// prompt that typing triggers are visible in the frame the user typed in
/// rather than the next one.  The preedit is likewise resolved before the grid
/// is built, so a composition starting or ending this frame is painted this
/// frame.
fn dispatch_input(
    ui: &Ui,
    response: &Response,
    session: &mut Session,
    ime: &mut crate::ime::Ime,
    allow_focus: bool,
    mode: TermMode,
) {
    if allow_focus && response.has_focus() {
        let consumed = ui.input(|i| consume_events(&i.events, mode));
        for event in consumed {
            match event {
                ConsumedEvent::Ime(ev) => {
                    if let Some(text) = ime.process(&ev) {
                        // Mirrors alacritty: single-char commits skip
                        // bracketed paste (event.rs "Don't use bracketed
                        // paste for single char input").
                        paste::paste(session, &text, text.chars().count() > 1);
                    }
                },
                // While composing, candidate-window navigation
                // (Space/Enter/arrows/Backspace/Escape) arrives as ordinary
                // key events; none of it may reach the PTY.  Mirrors
                // alacritty's early return in `key_input`. That early return
                // also runs before alacritty dispatches keyboard-triggered
                // clipboard paste, so a keyboard paste shortcut is likewise
                // dropped while composing (alacritty/src/input/keyboard.rs,
                // alacritty/src/input/mod.rs `key_input`).
                _ if ime.preedit().is_some() => {},
                ConsumedEvent::Bytes(bytes) => {
                    // Typing drops the selection and snaps back to the prompt
                    // so the user sees their input — matches alacritty's
                    // on_terminal_input_start.
                    paste::on_terminal_input_start(session);
                    crate::frame_log::keystroke_sent();
                    session.write(bytes);
                },
            }
        }
    } else {
        // IMEs commonly auto-commit an in-progress composition when focus
        // moves away (winit pairs the `Commit` with an immediate
        // `Disabled`, both landing in the same event batch as the focus
        // loss). If a composition was in progress, drain Ime events one
        // more time so that commit still reaches the PTY instead of being
        // silently discarded. An unfocused terminal with no composition in
        // flight must never consume Ime events — they belong to whatever
        // widget (e.g. a modal dialog's text field) is focused instead.
        if ime.preedit().is_some() {
            let events: Vec<ImeEvent> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        Event::Ime(ev) => Some(ev.clone()),
                        _ => None,
                    })
                    .collect()
            });
            for ev in events {
                if let Some(text) = ime.process(&ev) {
                    paste::paste(session, &text, text.chars().count() > 1);
                }
            }
        }
        // The IME's `Disabled` event arrives only while input is still
        // drained; a composition abandoned without any terminal Ime event
        // (e.g. the OS cancels it outright) never sends one, so this is the
        // backstop that drops the painted preedit either way.
        ime.clear();
    }
}

/// Whether the grid owns the pointer at `pos`: inside `rect` with no floating
/// layer (modal, window, context menu) above it.  `layer_id_at` resolves only
/// floating `Area` layers, so `None` means the position reaches the background
/// panels hosting the grid — and while a modal is open egui resolves *every*
/// position to the modal's layer.  Raw pointer reads bypass egui's
/// response-level layer blocking, so each one must apply this check itself.
fn pointer_owns_grid(
    ctx: &egui::Context,
    grid_layer: egui::LayerId,
    rect: Rect,
    pos: Pos2,
) -> bool {
    rect.contains(pos) && ctx.layer_id_at(pos).is_none_or(|l| l == grid_layer)
}

/// Everything a frame's input handlers need from the terminal, read under one
/// lock.
///
/// The PTY reader leases the terminal for the whole of every parse, so each
/// separate `lock()` a frame takes queues behind one.  Reading these together
/// keeps a burst of output from costing the frame one parse per handler.
struct TermPeek {
    mode: TermMode,
    display_offset: i32,
    /// Link under the mouse pointer.  `None` when the pointer is outside the
    /// grid, when no link covers that cell, or when the pointer is driving a
    /// drag, so click-to-open never fights with text selection.
    link: Option<Link>,
}

#[allow(clippy::too_many_arguments)]
fn peek_term(
    ui: &Ui,
    response: &Response,
    session: &Session,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
) -> TermPeek {
    // Resolved before the lock: `layer_id_at` walks egui's layer list and has
    // no business running with the terminal held.
    let hover = (!response.dragged())
        .then(|| ui.input(|i| i.pointer.hover_pos()))
        .flatten()
        .filter(|pos| pointer_owns_grid(ui.ctx(), ui.layer_id(), rect, *pos));

    let term = session.term.lock();
    let display_offset = term.grid().display_offset() as i32;
    let link = hover.and_then(|pos| {
        let (point, _) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
        links::link_at(&term, point)
    });
    TermPeek { mode: *term.mode(), display_offset, link }
}

#[allow(clippy::too_many_arguments)]
fn handle_selection(
    ui: &Ui,
    response: &Response,
    session: &Session,
    config: &Config,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    hovered_link: Option<&Link>,
    detached_jobs: &mut Vec<jobs::Job<()>>,
) {
    let primary = PointerButton::Primary;
    let secondary = PointerButton::Secondary;
    let middle = PointerButton::Middle;
    let modifiers = ui.input(|i| i.modifiers);

    // Middle-click pastes the PRIMARY (selection) buffer — alacritty's default.
    if response.clicked_by(middle) {
        if let Some(text) = clipboard::read(Target::Primary) {
            paste::paste(session, &text, true);
        }
        return;
    }

    // Right-click extends the active selection's far edge to the click point,
    // matching alacritty's `ExpandSelection` mouse action.
    if response.clicked_by(secondary) {
        if let Some(pos) = click_position(ui, response) {
            let mut term = session.term.lock();
            if term.selection.is_some() {
                let display_offset = term.grid().display_offset() as i32;
                let (point, side) =
                    cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
                if let Some(sel) = term.selection.as_mut() {
                    sel.update(point, side);
                }
                paste::write_selection(&term, config, Target::Primary);
            }
        }
        return;
    }

    // Triple/double clicks set Lines/Semantic immediately and copy on the same release.
    if response.triple_clicked_by(primary) {
        if let Some(pos) = click_position(ui, response) {
            start_selection_at(
                session,
                config,
                rect,
                cell_w,
                cell_h,
                cols,
                rows,
                pos,
                SelectionType::Lines,
            );
        }
        return;
    }
    if response.double_clicked_by(primary) {
        if let Some(pos) = click_position(ui, response) {
            start_selection_at(
                session,
                config,
                rect,
                cell_w,
                cell_h,
                cols,
                rows,
                pos,
                SelectionType::Semantic,
            );
        }
        return;
    }

    if response.drag_started_by(primary) {
        // Anchor at the press origin, not the current pointer: egui only fires
        // `drag_started` once the pointer has moved past its ~6 px click/drag
        // threshold, so `interact_pointer_pos` has already drifted off the cell
        // the user actually clicked — losing the first character of selections.
        if let Some(pos) = ui.input(|i| i.pointer.press_origin()) {
            let ty = if modifiers.ctrl { SelectionType::Block } else { SelectionType::Simple };
            let mut term = session.term.lock();
            let display_offset = term.grid().display_offset() as i32;
            let (point, side) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
            term.selection = Some(Selection::new(ty, point, side));
            if let Some(cur) = response.interact_pointer_pos() {
                let (cur_point, cur_side) =
                    cell_at_pos(cur, rect, cell_w, cell_h, cols, rows, display_offset);
                if let Some(sel) = term.selection.as_mut() {
                    sel.update(cur_point, cur_side);
                }
            }
        }
    } else if response.dragged_by(primary) {
        if let Some(pos) = response.interact_pointer_pos() {
            let mut term = session.term.lock();
            let display_offset = term.grid().display_offset() as i32;
            let (point, side) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
        }
    } else if response.drag_stopped_by(primary) {
        paste::write_selection(&session.term.lock(), config, Target::Primary);
    } else if response.clicked_by(primary) {
        // A bare primary click on a link follows it instead of clearing the
        // selection.  That matches alacritty's default URL hint, which fires
        // on release without any modifier.
        if let Some(link) = hovered_link {
            detached_jobs.push(links::open(&link.uri));
            return;
        }
        // Bare click outside an existing drag clears the selection, matching alacritty.
        session.term.lock().selection = None;
    }
}

/// Forward raw button and motion events to a mouse-tracking app, mirroring
/// alacritty's `on_mouse_press` / `on_mouse_release` / `mouse_moved`.  Presses
/// and releases report the clicked cell; motion reports only when the pointer
/// crosses into a new cell and the app opted into motion (any-motion) or drag
/// tracking.  Events outside the grid — or under an overlay above it — are
/// ignored so sidebar clicks and drags inside dialogs don't leak.
#[allow(clippy::too_many_arguments)]
fn handle_mouse_reporting(
    ui: &Ui,
    session: &mut Session,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    peek: &TermPeek,
) {
    let mode = peek.mode;
    let motion_tracked = mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG);

    enum Raw {
        Button { pos: Pos2, code: u8, pressed: bool, modifiers: Modifiers },
        Motion { pos: Pos2, modifiers: Modifiers },
    }

    let (raws, held) = ui.input(|i| {
        let held = if i.pointer.primary_down() {
            Some(mouse::BUTTON_LEFT)
        } else if i.pointer.middle_down() {
            Some(mouse::BUTTON_MIDDLE)
        } else if i.pointer.secondary_down() {
            Some(mouse::BUTTON_RIGHT)
        } else {
            None
        };
        let motion_mods = i.modifiers;
        let raws: Vec<Raw> = i
            .events
            .iter()
            .filter_map(|e| match e {
                Event::PointerButton { pos, button, pressed, modifiers } => button_code(*button)
                    .map(|code| Raw::Button {
                        pos: *pos,
                        code,
                        pressed: *pressed,
                        modifiers: *modifiers,
                    }),
                Event::PointerMoved(pos) if motion_tracked => {
                    Some(Raw::Motion { pos: *pos, modifiers: motion_mods })
                },
                _ => None,
            })
            .collect();
        (raws, held)
    });

    if raws.is_empty() {
        return;
    }

    let display_offset = peek.display_offset;
    let mut bytes = Vec::new();
    for raw in raws {
        match raw {
            Raw::Button { pos, code, pressed, modifiers } => {
                if !pointer_owns_grid(ui.ctx(), ui.layer_id(), rect, pos) {
                    continue;
                }
                let (point, _) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
                session.last_report_cell = Some(point);
                if let Some(report) = mouse::mouse_report(mode, point, code, pressed, modifiers) {
                    bytes.extend_from_slice(&report);
                }
            },
            Raw::Motion { pos, modifiers } => {
                if !pointer_owns_grid(ui.ctx(), ui.layer_id(), rect, pos) {
                    continue;
                }
                let (point, _) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
                if session.last_report_cell == Some(point) {
                    continue;
                }
                session.last_report_cell = Some(point);
                let base = match held {
                    Some(button) => button + mouse::MOTION_OFFSET,
                    None if mode.contains(TermMode::MOUSE_MOTION) => mouse::MOTION_NONE,
                    None => continue,
                };
                if let Some(report) = mouse::mouse_report(mode, point, base, true, modifiers) {
                    bytes.extend_from_slice(&report);
                }
            },
        }
    }
    if !bytes.is_empty() {
        session.write(bytes);
    }
}

fn button_code(button: PointerButton) -> Option<u8> {
    match button {
        PointerButton::Primary => Some(mouse::BUTTON_LEFT),
        PointerButton::Middle => Some(mouse::BUTTON_MIDDLE),
        PointerButton::Secondary => Some(mouse::BUTTON_RIGHT),
        _ => None,
    }
}

/// Mouse-wheel scrolling.  Mirrors alacritty's `scroll_terminal`: accumulate
/// pixel deltas across frames, divide by cell height for whole-line steps,
/// and route to the PTY or scrollback depending on terminal mode.
#[allow(clippy::too_many_arguments)]
fn handle_wheel_scroll(
    ui: &Ui,
    response: &Response,
    session: &mut Session,
    config: &Config,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    peek: &TermPeek,
) {
    if !response.hovered() {
        return;
    }
    let wheels: Vec<(MouseWheelUnit, Vec2, Modifiers)> = ui.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                Event::MouseWheel { unit, delta, modifiers } => Some((*unit, *delta, *modifiers)),
                _ => None,
            })
            .collect()
    });
    if wheels.is_empty() {
        return;
    }
    // Mouse-tracking apps receive wheel reports addressed to the hovered cell.
    let pointer_cell = ui
        .input(|i| i.pointer.hover_pos())
        .map(|pos| cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, peek.display_offset).0);
    let cell_w_pt = cell_w as f64;
    let cell_h_pt = cell_h as f64;
    for (unit, delta, modifiers) in wheels {
        let (dx_pt, dy_pt) = match unit {
            MouseWheelUnit::Point => (delta.x as f64, delta.y as f64),
            MouseWheelUnit::Line => (delta.x as f64 * cell_w_pt, delta.y as f64 * cell_h_pt),
            MouseWheelUnit::Page => (
                delta.x as f64 * session.size.columns as f64 * cell_w_pt,
                delta.y as f64 * session.size.screen_lines as f64 * cell_h_pt,
            ),
        };
        apply_scroll(
            session,
            config,
            dx_pt,
            dy_pt,
            cell_w_pt,
            cell_h_pt,
            modifiers,
            pointer_cell,
            peek.mode,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_scroll(
    session: &mut Session,
    config: &Config,
    dx_pt: f64,
    dy_pt: f64,
    cell_w_pt: f64,
    cell_h_pt: f64,
    modifiers: Modifiers,
    pointer_cell: Option<Point>,
    mode: TermMode,
) {
    let mouse_mode = mode.intersects(TermMode::MOUSE_MODE);
    // ConPTY interprets a pager's alternate-screen switch itself and repaints
    // onto the primary screen, so ALT_SCREEN never reaches this Term on
    // Windows.  A diff pane runs `delta --paging=always` by construction —
    // route its wheel to arrow keys as if the alt screen were visible.
    let on_alt_screen = mode.contains(TermMode::ALT_SCREEN)
        || (cfg!(windows) && matches!(session.kind, SessionKind::Diff { .. }));
    let alt_alt_scroll = on_alt_screen && mode.contains(TermMode::ALTERNATE_SCROLL);

    // alacritty: the user's `scrolling.multiplier` only applies when *we* are
    // consuming the wheel — when the app is reading raw mouse events it gets
    // one report per physical click, no amplification.
    let multiplier = if mouse_mode { 1.0 } else { config.scrolling.multiplier as f64 };
    session.accumulated_scroll.0 += dx_pt * multiplier;
    session.accumulated_scroll.1 += dy_pt * multiplier;

    let lines = (session.accumulated_scroll.1 / cell_h_pt).abs() as usize;
    let columns = (session.accumulated_scroll.0 / cell_w_pt).abs() as usize;
    let is_up = dy_pt > 0.0;

    if mouse_mode {
        // One button report per accumulated line/column tick, addressed to the
        // hovered cell — alacritty's `scroll_terminal` in mouse mode.
        if let Some(point) = pointer_cell {
            let mut bytes = Vec::new();
            let line_btn = if is_up { mouse::WHEEL_UP } else { mouse::WHEEL_DOWN };
            for _ in 0..lines {
                if let Some(report) = mouse::wheel_report(mode, point, line_btn, modifiers) {
                    bytes.extend_from_slice(&report);
                }
            }
            let column_btn = if dx_pt > 0.0 { mouse::WHEEL_LEFT } else { mouse::WHEEL_RIGHT };
            for _ in 0..columns {
                if let Some(report) = mouse::wheel_report(mode, point, column_btn, modifiers) {
                    bytes.extend_from_slice(&report);
                }
            }
            if !bytes.is_empty() {
                session.write(bytes);
            }
        }
    } else if alt_alt_scroll && !modifiers.shift {
        // Alt-screen apps (vim/less/man) opted into ALTERNATE_SCROLL ask for
        // arrow keys instead of touching the scrollback (which doesn't exist
        // on the alt screen).  Shift overrides this so users can still scroll
        // back the host history if anything ever lands there.
        let line_cmd = if is_up { b'A' } else { b'B' };
        let column_cmd = if dx_pt > 0.0 { b'D' } else { b'C' };
        let mut bytes = Vec::with_capacity(3 * (lines + columns));
        for _ in 0..lines {
            bytes.extend_from_slice(b"\x1bO");
            bytes.push(line_cmd);
        }
        for _ in 0..columns {
            bytes.extend_from_slice(b"\x1bO");
            bytes.push(column_cmd);
        }
        if !bytes.is_empty() {
            session.write(bytes);
        }
    } else if lines != 0 {
        let delta = if is_up { lines as i32 } else { -(lines as i32) };
        session.term.lock().scroll_display(Scroll::Delta(delta));
    }

    session.accumulated_scroll.0 %= cell_w_pt;
    session.accumulated_scroll.1 %= cell_h_pt;
}

#[allow(clippy::too_many_arguments)]
fn start_selection_at(
    session: &Session,
    config: &Config,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    pos: Pos2,
    ty: SelectionType,
) {
    let mut term = session.term.lock();
    let display_offset = term.grid().display_offset() as i32;
    let (point, side) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
    term.selection = Some(Selection::new(ty, point, side));
    paste::write_selection(&term, config, Target::Primary);
}

/// Pointer position to use for click handlers.  Triple/double click are
/// reported only on release, by which point `interact_pointer_pos` has already
/// dropped the press location, so fall back to the last hover position.
fn click_position(ui: &Ui, response: &Response) -> Option<Pos2> {
    response.interact_pointer_pos().or_else(|| ui.input(|i| i.pointer.hover_pos()))
}

/// Grid cell under `pos`, and which half of that cell the pointer sits on.
///
/// A drag captures the pointer, so `pos` routinely lands outside the grid.
/// Clamping before the side is derived is what makes those positions anchor
/// left of column 0 and right of the last column, as alacritty's saturating
/// `Mouse::point` and `cell_side` do.
fn cell_at_pos(
    pos: Pos2,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    display_offset: i32,
) -> (Point, Side) {
    let col_f = ((pos.x - rect.min.x) / cell_w).clamp(0.0, cols as f32);
    let row_f = ((pos.y - rect.min.y) / cell_h).clamp(0.0, rows as f32);
    let col = (col_f as usize).min(cols - 1);
    let row = (row_f as usize).min(rows - 1);
    let side = if col_f - (col as f32) < 0.5 { Side::Left } else { Side::Right };
    (Point::new(Line(row as i32 - display_offset), Column(col)), side)
}

enum ConsumedEvent {
    Bytes(Vec<u8>),
    Ime(ImeEvent),
}

/// Classify a frame's input events for the focused terminal.
///
/// Each event is classified with its successor in hand: egui-winit splits one
/// winit key press into an `Event::Key` followed by an `Event::Text`, and the
/// kitty protocol's associated-text field needs the two back together.
fn consume_events(events: &[Event], mode: TermMode) -> Vec<ConsumedEvent> {
    events
        .iter()
        .enumerate()
        .filter_map(|(idx, event)| match event {
            Event::Ime(ev) => Some(ConsumedEvent::Ime(ev.clone())),
            _ => consumed_event(event, events.get(idx + 1), mode),
        })
        .collect()
}

/// Classify an input event for the focused terminal.
///
/// `Event::Paste` is dropped rather than pasted: egui-winit synthesizes it for
/// every `command+V` press, Shift included, so acting on it would paste on
/// Ctrl+V regardless of the binding table and leave the shortcut impossible to
/// rebind or unbind.  Keyboard paste runs through `NamedAction::Paste`, which
/// reads the clipboard itself.  Text widgets outside the terminal still consume
/// the event normally.  `Event::Ime` is handled separately by the caller.
fn consumed_event(event: &Event, next: Option<&Event>, mode: TermMode) -> Option<ConsumedEvent> {
    match event {
        Event::Paste(_) => None,
        _ => event_to_bytes(event, associated_text(next), mode).map(ConsumedEvent::Bytes),
    }
}

/// Viewport rect of the terminal cursor's cell; `None` while the cursor is
/// scrolled out of view.
fn cursor_cell_rect(snapshot: &GridSnapshot, rect: Rect, cell_w: f32, cell_h: f32) -> Option<Rect> {
    let (column, row) = snapshot.caret?;
    Some(Rect::from_min_size(
        Pos2::new(rect.min.x + column as f32 * cell_w, rect.min.y + row as f32 * cell_h),
        Vec2::new(cell_w, cell_h),
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: AnsiColor,
    bg: AnsiColor,
    flags: Flags,
}

impl Style {
    fn from_cell(cell: &Cell, underline_link: bool) -> Self {
        // The two halves of a wide glyph are written from one cursor template,
        // so the spacer carries the same colours and the same SGR flags as the
        // character it belongs to and differs only in which of these two bits
        // is set.  Reading them as style would end a run between the halves of
        // a single character and leave the right half with no background and
        // no decoration.  kitty sidesteps the question by storing the spacer as
        // a copy of the lead cell (`screen.c`, `draw_text_loop`); alacritty
        // instead drops spacers from its renderable cells and puts the column
        // back when it draws a line (`renderer/rects.rs`).
        let mut flags = cell.flags;
        flags.remove(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER);
        if underline_link {
            flags.insert(Flags::UNDERLINE);
        }
        Self { fg: cell.fg, bg: cell.bg, flags }
    }

    /// Whether a blank cell styled as `other` can be painted as part of a run
    /// in this style.
    ///
    /// A blank draws nothing but its background, so its foreground is free to
    /// differ — unless something in the run paints with the foreground, which
    /// underlines, strikeout and reverse video all do.  Keeping the two
    /// together is what lets an over-wide glyph grow into the space an icon is
    /// authored with, since growth may only claim blanks its own run holds;
    /// kitty reaches the same place by copying the icon's foreground into the
    /// blank.
    fn absorbs(&self, other: &Self, blank: bool) -> bool {
        blank
            && other.bg == self.bg
            && other.flags == self.flags
            && !self.flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT | Flags::INVERSE)
    }
}

/// The visible grid, copied out from under the terminal lock.
///
/// The PTY thread applies output under the same `FairMutex` the painter needs,
/// so every microsecond a frame holds it is a microsecond the terminal cannot
/// parse — and the echo of a keystroke waits behind that.  Mirrors alacritty's
/// `Display::draw`, which collects its renderable cells and drops the terminal
/// guard before rendering anything.
///
/// Both buffers are reused across frames, and colours are resolved during the
/// copy, so painting from a snapshot needs neither the lock nor the palette.
#[derive(Default)]
pub struct GridSnapshot {
    /// One entry per viewport row.  Each row owns its bytes so re-walking one
    /// row never moves another's, which is what lets a capture skip the rows
    /// the terminal reports as clean.
    rows: Vec<RowSnapshot>,
    cursor: Option<CursorSnapshot>,
    /// Viewport cell holding the terminal cursor, recorded whether or not the
    /// cursor is drawn: the IME candidate window follows the caret even while
    /// the running app keeps the cursor hidden.
    caret: Option<(usize, i32)>,
    /// The terminal's background as of the last capture.  `None` before the
    /// first one, when there is no terminal to have an opinion yet.
    default_bg: Option<Color32>,
    /// Rows the last capture rewrote, merged into one span.
    dirty_rows: std::ops::Range<usize>,
    /// Scratch for the rows a capture is about to walk, reused so reading
    /// damage costs no allocation.
    damaged: Vec<usize>,
    context: CaptureContext,
}

#[derive(Default)]
struct RowSnapshot {
    text: String,
    runs: Vec<Run>,
}

/// What a capture depends on that the terminal's own damage tracking does not
/// cover.  `Term::damage` documents the selection as caller-tracked, and it
/// knows nothing about link highlighting or our configured palette, so a
/// change to any of these invalidates rows the terminal calls clean.
#[derive(Default, PartialEq)]
struct CaptureContext {
    /// One snapshot is reused for every session, so the rows it holds belong
    /// to whichever terminal filled them last.
    session: Option<SessionId>,
    selection: Option<SelectionRange>,
    link: Option<Match>,
    palette: Option<Palette>,
    dimensions: (usize, usize),
}

/// A span of cells sharing one resolved style, indexing into its row's text.
struct Run {
    text: std::ops::Range<usize>,
    start_col: usize,
    row: i32,
    flags: Flags,
    fg: Color32,
    bg: Color32,
    selected: bool,
}

struct CursorSnapshot {
    shape: CursorShape,
    column: usize,
    row: i32,
    color: Color32,
    /// The glyph a solid block covers, with the colour that keeps it legible.
    glyph: Option<(char, Flags, Color32)>,
}

impl GridSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// The terminal's background as of the last capture, for everything that
    /// paints behind the grid as well as the grid itself.  Falls back to the
    /// configured colour before the first capture.
    pub fn default_bg(&self, palette: &Palette) -> Color32 {
        self.default_bg.unwrap_or_else(|| background(palette))
    }

    /// Every run in the snapshot, paired with the text it covers.
    fn runs(&self) -> impl Iterator<Item = (&str, &Run)> {
        self.rows.iter().flat_map(row_runs)
    }

    /// Every run on exactly these rows, in the order given.  A frame that
    /// rewrote three rows reads three rows: walking the runs of a full screen
    /// costs more than writing the records of the ones that changed, and two
    /// damaged rows far apart span every clean row between them.
    fn runs_in_rows(
        &self,
        rows: impl IntoIterator<Item = usize>,
    ) -> impl Iterator<Item = (&str, &Run)> {
        let len = self.rows.len();
        rows.into_iter().filter(move |&row| row < len).flat_map(|row| row_runs(&self.rows[row]))
    }

    /// Rows the last capture rewrote, merged into one span.  The GPU path
    /// uploads exactly this much.
    fn dirty_rows(&self) -> std::ops::Range<usize> {
        self.dirty_rows.clone()
    }

    /// The viewport rows this capture re-walked, sorted and without repeats.
    fn damaged_rows(&self) -> &[usize] {
        &self.damaged
    }

    /// Fill `damaged` with the viewport rows this capture has to re-walk, and
    /// `dirty_rows` with the span covering them.
    ///
    /// Mirrors `Display::update_damage` in alacritty: take the terminal's own
    /// damage, then add what it documents as the caller's to track.  Anything
    /// that moves every row — a resize, a scroll, a palette change, the first
    /// capture — comes back as the full range.
    fn collect_damage(
        &mut self,
        term: &mut Term<EventProxy>,
        config: &Config,
        session: SessionId,
        link_bounds: Option<&Match>,
        selection: Option<SelectionRange>,
        cols: usize,
        screen_lines: usize,
    ) {
        self.damaged.clear();

        let context = CaptureContext {
            session: Some(session),
            selection,
            link: link_bounds.cloned(),
            palette: Some(config.palette.clone()),
            dimensions: (cols, screen_lines),
        };
        let rebuilt = self.rows.len() != screen_lines
            || self.context.session != context.session
            || self.context.dimensions != context.dimensions
            || self.context.palette != context.palette;
        if rebuilt {
            self.rows.clear();
            self.rows.resize_with(screen_lines, RowSnapshot::default);
        }

        let display_offset = term.grid().display_offset() as i32;
        let mut full = rebuilt;
        match term.damage() {
            TermDamage::Full => full = true,
            TermDamage::Partial(lines) => {
                self.damaged.extend(lines.map(|line| line.line).filter(|&l| l < screen_lines));
            },
        }
        term.reset_damage();

        // A selection or a link highlight restyles cells the terminal never
        // wrote to, so both edges of the change have to be re-walked.
        for range in [self.context.selection, context.selection] {
            if let Some(range) = range {
                self.damaged.extend(viewport_rows(
                    range.start,
                    range.end,
                    display_offset,
                    screen_lines,
                ));
            }
        }
        for link in [self.context.link.as_ref(), context.link.as_ref()] {
            if let Some(link) = link {
                self.damaged.extend(viewport_rows(
                    *link.start(),
                    *link.end(),
                    display_offset,
                    screen_lines,
                ));
            }
        }
        self.context = context;

        if full {
            self.damaged.clear();
            self.damaged.extend(0..screen_lines);
        } else {
            self.damaged.sort_unstable();
            self.damaged.dedup();
        }
        self.dirty_rows = match (self.damaged.first(), self.damaged.last()) {
            (Some(&first), Some(&last)) => first..last + 1,
            _ => 0..0,
        };
    }

    fn capture(
        &mut self,
        term: &mut Term<EventProxy>,
        config: &Config,
        session: SessionId,
        link_bounds: Option<&Match>,
        cursor_hidden: bool,
    ) {
        self.cursor = None;
        self.caret = None;

        let display_offset = term.grid().display_offset() as i32;
        let screen_lines = term.grid().screen_lines();
        let cols = term.grid().columns();
        let selection_range = term.selection.as_ref().and_then(|s| s.to_range(term));

        self.collect_damage(
            term,
            config,
            session,
            link_bounds,
            selection_range,
            cols,
            screen_lines,
        );

        let runtime_palette = term.colors();
        self.default_bg = Some(default_background(runtime_palette, &config.palette));
        let grid = term.grid();
        let in_link = |line: Line, column: Column| {
            link_bounds.is_some_and(|b| b.contains(&Point::new(line, column)))
        };

        for &row in &self.damaged {
            let line = Line(row as i32 - display_offset);
            let cells = &grid[line];
            let dest = &mut self.rows[row];
            dest.text.clear();
            dest.runs.clear();

            let mut col = 0;
            while col < cols {
                let start = col;
                let style = Style::from_cell(&cells[Column(col)], in_link(line, Column(col)));
                let selected = is_selected(selection_range.as_ref(), line, Column(col));
                let text_start = dest.text.len();
                while col < cols {
                    let cell = &cells[Column(col)];
                    let cell_style = Style::from_cell(cell, in_link(line, Column(col)));
                    let blank = matches!(cell.c, ' ' | '\0');
                    if (cell_style != style && !style.absorbs(&cell_style, blank))
                        || is_selected(selection_range.as_ref(), line, Column(col)) != selected
                    {
                        break;
                    }
                    let ch = if cell.c == '\0' || cell.flags.contains(Flags::HIDDEN) {
                        ' '
                    } else {
                        cell.c
                    };
                    dest.text.push(ch);
                    col += 1;
                }
                if dest.text.len() == text_start {
                    continue;
                }
                let (fg, bg) = run_colors(style, selected, runtime_palette, config);
                dest.runs.push(Run {
                    text: text_start..dest.text.len(),
                    start_col: start,
                    row: row as i32,
                    flags: style.flags,
                    fg,
                    bg,
                    selected,
                });
            }
        }

        let cursor_point: Point = grid.cursor.point;
        let cursor_row = cursor_point.line.0 + display_offset;
        let in_view = cursor_row >= 0 && cursor_row < screen_lines as i32;
        self.caret = in_view.then_some((cursor_point.column.0, cursor_row));

        let shape = cursor_shape(term);
        if cursor_hidden || matches!(shape, CursorShape::Hidden) || !in_view {
            return;
        }

        let cell = &grid[Line(cursor_point.line.0)][cursor_point.column];
        let color = runtime_palette[alacritty_terminal::vte::ansi::NamedColor::Cursor]
            .map(rgb_to_color32)
            .or_else(|| config.palette.cursor_bg.map(rgb_to_color32))
            .unwrap_or_else(|| foreground(&config.palette));
        // Only the solid block covers the glyph underneath it.
        let glyph = (matches!(shape, CursorShape::Block)
            && cell.c != '\0'
            && !cell.flags.contains(Flags::HIDDEN))
        .then(|| {
            let glyph_color = config.palette.cursor_fg.map(rgb_to_color32).unwrap_or_else(|| {
                rgb_to_color32(resolve(
                    cell.bg,
                    cell.flags,
                    runtime_palette,
                    &config.palette,
                    false,
                ))
            });
            let glyph_color =
                if glyph_color == color { background(&config.palette) } else { glyph_color };
            (cell.c, cell.flags, glyph_color)
        });

        self.cursor = Some(CursorSnapshot {
            shape,
            column: cursor_point.column.0,
            row: cursor_row,
            color,
            glyph,
        });
    }
}

fn row_runs(row: &RowSnapshot) -> impl Iterator<Item = (&str, &Run)> {
    row.runs.iter().map(move |run| (&row.text[run.text.clone()], run))
}

/// Viewport rows a buffer-coordinate span covers, clipped to what is on
/// screen.  A span entirely in scrollback comes back empty.
fn viewport_rows(
    start: Point,
    end: Point,
    display_offset: i32,
    screen_lines: usize,
) -> std::ops::Range<usize> {
    let (first, last) = (start.line.0 + display_offset, end.line.0 + display_offset);
    if last < 0 || first >= screen_lines as i32 {
        return 0..0;
    }
    first.max(0) as usize..(last as usize + 1).min(screen_lines)
}

/// Foreground and background a run is drawn with, after INVERSE and the
/// selection highlight.
fn run_colors(
    style: Style,
    selected: bool,
    runtime: &alacritty_terminal::term::color::Colors,
    config: &Config,
) -> (Color32, Color32) {
    let inverse = style.flags.contains(Flags::INVERSE);
    let cell_fg = resolve(
        if inverse { style.bg } else { style.fg },
        style.flags,
        runtime,
        &config.palette,
        true,
    );
    let cell_bg = resolve(
        if inverse { style.fg } else { style.bg },
        style.flags,
        runtime,
        &config.palette,
        false,
    );
    if !selected {
        return (rgb_to_color32(cell_fg), rgb_to_color32(cell_bg));
    }
    // When `colors.selection.background` is set we honor it; otherwise we swap
    // fg/bg of the underlying cell so the highlight is always visible without
    // requiring a config entry.
    let sel_bg =
        config.palette.selection_bg.map(rgb_to_color32).unwrap_or_else(|| rgb_to_color32(cell_fg));
    let sel_fg = config.palette.selection_fg.map(rgb_to_color32).unwrap_or_else(|| {
        if config.palette.selection_bg.is_some() {
            rgb_to_color32(cell_fg)
        } else {
            rgb_to_color32(cell_bg)
        }
    });
    (sel_fg, sel_bg)
}

/// Fill the GPU grid's buffers from this frame's snapshot and hand egui the
/// callback that draws them.
///
/// Only the glyph and background layers move to the GPU.  Emoji and
/// box-drawing glyphs carry their own textures and underlines are their own
/// geometry, so those stay ordinary shapes emitted after the callback — which
/// also keeps them above every background, the order `paint_grid` enforces by
/// running its two passes separately.
/// The rows a frame owes new records: the ones the terminal damaged, or
/// every row when the glyph table renumbered under records already written.
///
/// Allocates nothing, because `steady_state` holds a quiet frame to no
/// allocations at all and this runs on every frame that paints.
fn rows_to_rewrite(
    damaged: &[usize],
    rows: usize,
    renumbered: bool,
) -> impl Iterator<Item = usize> + Clone + '_ {
    let all = if renumbered { 0..rows } else { 0..0 };
    let some = if renumbered { &[][..] } else { damaged };
    all.chain(some.iter().copied())
}

/// The decoration tile a cell carrying `flags` samples.
///
/// Each underline style keeps its own shape, so a curl stays a curl rather
/// than degrading to the straight rule an arithmetic shader can describe.
fn decoration_tile(flags: Flags) -> u16 {
    let underline = if flags.contains(Flags::DOUBLE_UNDERLINE) {
        decoration_sprites::DOUBLE
    } else if flags.contains(Flags::UNDERCURL) {
        decoration_sprites::CURLY
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        decoration_sprites::DOTTED
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        decoration_sprites::DASHED
    } else if flags.intersects(Flags::ALL_UNDERLINES) {
        decoration_sprites::STRAIGHT
    } else {
        decoration_sprites::NONE
    };
    decoration_sprites::tile(underline, flags.contains(Flags::STRIKEOUT))
}

#[allow(clippy::too_many_arguments)]
fn paint_grid_gpu(
    gpu: &GpuGrid,
    painter: &egui::Painter,
    rect: Rect,
    snapshot: &GridSnapshot,
    config: &Config,
    face_metrics: &crate::fonts::FaceMetrics,
    font_ascent_pt: f32,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    ppp: f32,
    metrics: &Metrics,
    builtin_glyphs: &mut BuiltinGlyphCache,
    color_glyphs: &mut ColorGlyphCache,
    glyphs: &mut GlyphCache,
    ctx: &egui::Context,
) {
    let default_bg = snapshot.default_bg(&config.palette);
    let size = config.font.egui_size();
    // Collected under the lock and drawn after it: painting needs the glyph
    // caches, and the grid state has no business being held while they work.
    let mut overlays = Vec::new();

    {
        let mut state = gpu.state.lock().expect("grid state");
        state.instances.resize(cols, rows, default_bg);
        // The strip is rasterized in physical pixels so its lines land on whole
        // ones; the quad sampling it is a cell in points, as everything else is.
        let ppp = ctx.pixels_per_point();
        // The mesh path still draws a straight rule at a fixed offset, so the
        // two paths deliberately disagree; it is on its way out rather than
        // waiting to be brought along.
        let geometry = decoration_sprites::Geometry::resolve(
            [(cell_w * ppp) as usize, (cell_h * ppp) as usize],
            font_ascent_pt,
            ppp,
            face_metrics,
            &config.ui.decorations,
        );
        let strip = state.decorations.texture(ctx, geometry);
        state.frame = GridFrame {
            // egui sets the GL viewport to the callback's rect, so the grid
            // starts at its own corner rather than the window's.
            origin: [0.0, 0.0],
            cell: [cell_w, cell_h],
            grid: [cols as u32, rows as u32],
            decorations: strip,
            decoration_tiles: decoration_sprites::TILES as u32,
            default_bg: default_bg.to_array().map(|c| c as f32 / 255.0),
        };
        let (instances, table) = state.buffers();
        // Only the rows this frame's capture rewrote need new records; the
        // rest of the buffer still holds what the GPU already has — unless the
        // table just renumbered itself, which leaves those records pointing at
        // whatever character now holds their old index.
        let renumbered = table.begin_frame(ctx, size);
        let touched = rows_to_rewrite(snapshot.damaged_rows(), rows, renumbered);
        // The upload stays the span covering them: it goes out as one
        // `glBufferSubData`, and the rows it carries between two damaged ones
        // hold records nothing has invalidated.
        let upload = if renumbered { 0..rows } else { snapshot.dirty_rows() };
        // Hidden runs are not filtered out: `capture` already replaced their
        // characters with blanks, and their background and decorations are
        // drawn the same as any other run's, as alacritty's `draw_cell` does.
        let runs = snapshot.runs_in_rows(touched.clone()).map(|(text, run)| RunView {
            text,
            start_col: run.start_col,
            row: run.row as usize,
            face: Face::new(run.flags.contains(Flags::BOLD), run.flags.contains(Flags::ITALIC)),
            deco: decoration_tile(run.flags),
            fg: run.fg,
            bg: run.bg,
        });
        instances.write_rows(touched, runs, default_bg, |ch, face| {
            // ASCII is neither box drawing nor emoji, and it is most of
            // what a terminal holds, so it never reaches the caches.
            if !ch.is_ascii() {
                if config.font.builtin_box_drawing && is_builtin_glyph(ch) {
                    return None;
                }
                if config.font.color_glyphs
                    && color_glyphs.get(ctx, ch, metrics, char_cells(ch)).is_some()
                {
                    return None;
                }
            }
            Some(table.slot(ch, face, || glyphs.get(ctx, ch, face, size)))
        });
        overlays.extend(instances.overlays());
        state.mark_rows_dirty(upload);
    }
    painter.add(gpu.callback(rect, ctx, config.debug.gpu_timing));
    // After the callback, so they land over the grid the way the mesh path
    // draws them over its own backgrounds.  Both caches are consulted again
    // rather than held across the lock; every lookup here is a cache hit,
    // because deciding to overlay the cell is what put it in the atlas.
    for (row, cell) in overlays {
        let (cell_x, cell_y) =
            (rect.min.x + cell.col as f32 * cell_w, rect.min.y + row as f32 * cell_h);
        if config.font.builtin_box_drawing
            && is_builtin_glyph(cell.ch)
            && let Some(cached) = builtin_glyphs.get(
                ctx,
                cell.ch,
                metrics,
                &config.font.offset,
                &config.font.glyph_offset,
            )
        {
            paint_builtin_glyph(
                painter,
                cached,
                cell_x,
                cell_y,
                cell_h,
                ppp,
                Color32::from_rgba_premultiplied(cell.fg[0], cell.fg[1], cell.fg[2], cell.fg[3]),
            );
            continue;
        }
        if let Some(cached) = color_glyphs.get(ctx, cell.ch, metrics, char_cells(cell.ch)) {
            paint_color_glyph(painter, cached, cell_x, cell_y, ppp);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_grid(
    painter: &egui::Painter,
    rect: Rect,
    snapshot: &GridSnapshot,
    config: &Config,
    font_id: &FontId,
    cell_w: f32,
    cell_h: f32,
    ppp: f32,
    metrics: &Metrics,
    builtin_glyphs: &mut BuiltinGlyphCache,
    color_glyphs: &mut ColorGlyphCache,
    glyphs: &mut GlyphCache,
    ctx: &egui::Context,
) {
    let bg_color = snapshot.default_bg(&config.palette);
    // Every background goes down before any glyph does.  A background is an
    // opaque fill over the whole run, so painting one run at a time cuts off
    // whatever the run before it overhung into its first cell — which is how a
    // Nerd Font icon a shade wider than its cell loses its right edge.
    // Alacritty's renderer already works this way: one background pass over
    // the whole batch, then the text passes (`renderer/text/gles2.rs`).
    for (text, run) in snapshot.runs() {
        paint_run_background(painter, rect, text, run, cell_w, cell_h, bg_color);
    }
    for (text, run) in snapshot.runs() {
        paint_run_glyphs(
            painter,
            rect,
            text,
            run,
            config,
            font_id,
            cell_w,
            cell_h,
            ppp,
            metrics,
            builtin_glyphs,
            color_glyphs,
            glyphs,
            ctx,
        );
    }

    if let Some(cursor) = &snapshot.cursor {
        paint_cursor(painter, rect, cursor, cell_w, cell_h, font_id);
    }
}

/// The cursor shape the terminal wants drawn, mirroring alacritty's
/// `RenderableCursor::new`.  `cursor_style()` reports the configured shape and
/// never `Hidden`, so DECTCEM has to be read off the mode: full-screen apps
/// hide the cursor while they repaint and leave it parked wherever their last
/// write landed, and drawing it regardless puts a block in an arbitrary spot
/// on top of their UI.
fn cursor_shape(term: &Term<EventProxy>) -> CursorShape {
    if term.mode().contains(TermMode::SHOW_CURSOR) {
        term.cursor_style().shape
    } else {
        CursorShape::Hidden
    }
}

fn is_selected(range: Option<&SelectionRange>, line: Line, column: Column) -> bool {
    range.is_some_and(|r| r.contains(Point::new(line, column)))
}

fn font_for_flags(flags: Flags, normal: &FontId) -> FontId {
    let bold = flags.contains(Flags::BOLD);
    let italic = flags.contains(Flags::ITALIC);
    let family = match (bold, italic) {
        (true, true) => FontFamily::Name(BOLD_ITALIC_FAMILY.into()),
        (true, false) => FontFamily::Name(BOLD_FAMILY.into()),
        (false, true) => FontFamily::Name(ITALIC_FAMILY.into()),
        (false, false) => return normal.clone(),
    };
    FontId::new(normal.size, family)
}

/// The cells `style` covers, in screen points.
fn run_rect(rect: Rect, run: &str, style: &Run, cell_w: f32, cell_h: f32) -> Rect {
    let width = run.chars().count() as f32 * cell_w;
    let x = rect.min.x + style.start_col as f32 * cell_w;
    let y = rect.min.y + style.row as f32 * cell_h;
    Rect::from_min_size(Pos2::new(x, y), Vec2::new(width, cell_h))
}

/// A run matching the terminal's own background needs no fill: the window is
/// already that colour, and emitting one shape per run would multiply the
/// frame's geometry for nothing.
fn paint_run_background(
    painter: &egui::Painter,
    rect: Rect,
    run: &str,
    style: &Run,
    cell_w: f32,
    cell_h: f32,
    default_bg: Color32,
) {
    if style.bg != default_bg || style.selected {
        painter.rect_filled(run_rect(rect, run, style, cell_w, cell_h), 0.0, style.bg);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_run_glyphs(
    painter: &egui::Painter,
    rect: Rect,
    run: &str,
    style: &Run,
    config: &Config,
    font_id: &FontId,
    cell_w: f32,
    cell_h: f32,
    ppp: f32,
    metrics: &Metrics,
    builtin_glyphs: &mut BuiltinGlyphCache,
    color_glyphs: &mut ColorGlyphCache,
    glyphs: &mut GlyphCache,
    ctx: &egui::Context,
) {
    let fg = style.fg;
    let cells = run_rect(rect, run, style, cell_w, cell_h);
    let (x, y) = (cells.min.x, cells.min.y);

    if !style.flags.contains(Flags::HIDDEN) {
        // Per-glyph paint: egui's run layout drifts off the cursor's `col * cell_w` grid (worse with zoom).
        let face =
            Face::new(style.flags.contains(Flags::BOLD), style.flags.contains(Flags::ITALIC));
        let glyph_dx = config.font.glyph_offset.x as f32;
        let glyph_dy = config.font.glyph_offset.y as f32;
        for (i, (byte, ch)) in run.char_indices().enumerate() {
            if ch == ' ' {
                continue;
            }
            let cell_x = x + i as f32 * cell_w;
            if config.font.builtin_box_drawing
                && is_builtin_glyph(ch)
                && let Some(cached) = builtin_glyphs.get(
                    ctx,
                    ch,
                    metrics,
                    &config.font.offset,
                    &config.font.glyph_offset,
                )
            {
                paint_builtin_glyph(painter, cached, cell_x, y, cell_h, ppp, fg);
                continue;
            }
            // Emoji are resolved against the normal chain whatever the cell's
            // style: colour fonts ship one set of artwork, and a bold or italic
            // variant of it would be synthesized rather than drawn.
            if config.font.color_glyphs
                && let Some(cached) = color_glyphs.get(ctx, ch, metrics, char_cells(ch))
            {
                paint_color_glyph(painter, cached, cell_x, y, ppp);
                continue;
            }
            let galley = glyphs.get(ctx, ch, face, font_id.size);
            // A private-use icon wider than its cell is drawn across the
            // blanks that follow rather than over the top of them, centred on
            // the span it ends up with, the way kitty grows one.
            let grow_dx = if may_grow(ch) {
                let spare = run[byte + ch.len_utf8()..]
                    .chars()
                    .take(MAX_EXTRA_CELLS)
                    .take_while(|c| *c == ' ')
                    .count();
                growth_offset(galley.size().x, cell_w, spare)
            } else {
                0.0
            };
            painter.add(
                egui::epaint::TextShape::new(
                    Pos2::new(cell_x + glyph_dx + grow_dx, y + glyph_dy),
                    galley,
                    fg,
                )
                .with_override_text_color(fg),
            );
        }
    }

    // Decorations belong to the glyph pass: drawn with the backgrounds, the
    // next run's fill would bury them.
    let width = cells.width();
    if style.flags.intersects(Flags::ALL_UNDERLINES) {
        let uy = y + cell_h - 1.5;
        painter
            .line_segment([Pos2::new(x, uy), Pos2::new(x + width, uy)], Stroke::new(1.0_f32, fg));
    }
    if style.flags.contains(Flags::STRIKEOUT) {
        let sy = y + cell_h * 0.5;
        painter
            .line_segment([Pos2::new(x, sy), Pos2::new(x + width, sy)], Stroke::new(1.0_f32, fg));
    }
}

fn paint_cursor(
    painter: &egui::Painter,
    rect: Rect,
    cursor: &CursorSnapshot,
    cell_w: f32,
    cell_h: f32,
    font_id: &FontId,
) {
    use alacritty_terminal::vte::ansi::CursorShape::*;

    let x = rect.min.x + cursor.column as f32 * cell_w;
    let y = rect.min.y + cursor.row as f32 * cell_h;
    let cursor_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));

    match cursor.shape {
        Block => {
            painter.rect_filled(cursor_rect, 0.0, cursor.color);
        },
        HollowBlock => {
            painter.rect_stroke(
                cursor_rect,
                0.0,
                Stroke::new(1.0_f32, cursor.color),
                egui::StrokeKind::Inside,
            );
        },
        Beam => {
            let bar = Rect::from_min_size(Pos2::new(x, y), Vec2::new(2.0, cell_h));
            painter.rect_filled(bar, 0.0, cursor.color);
        },
        Underline => {
            let bar = Rect::from_min_size(Pos2::new(x, y + cell_h - 2.0), Vec2::new(cell_w, 2.0));
            painter.rect_filled(bar, 0.0, cursor.color);
        },
        Hidden => return,
    }

    // The solid block covers the glyph; redraw it in inverted color so it stays legible.
    if let Some((ch, flags, color)) = cursor.glyph {
        painter.text(
            Pos2::new(x, y),
            egui::Align2::LEFT_TOP,
            ch.to_string(),
            font_for_flags(flags, font_id),
            color,
        );
    }
}

/// Draw the in-progress IME composition at the cursor, mirroring alacritty's
/// `draw_ime_preview`: default foreground on default background, underlined,
/// with a beam caret after the last char (egui-winit drops the preedit
/// cursor offset, so the caret can only sit at the end).  Returns the caret
/// cell rect so the candidate window can follow it.
#[allow(clippy::too_many_arguments)]
fn paint_preedit(
    painter: &egui::Painter,
    rect: Rect,
    session: &Session,
    config: &Config,
    font_id: &FontId,
    cell_w: f32,
    cell_h: f32,
    preedit: &str,
) -> Option<Rect> {
    let (cursor_col, line, cols) = {
        let term = session.term.lock();
        let grid = term.grid();
        let line = grid.cursor.point.line.0 + grid.display_offset() as i32;
        if line < 0 || line >= grid.screen_lines() as i32 {
            return None;
        }
        (grid.cursor.point.column.0, line, grid.columns())
    };

    let layout = crate::ime::preedit_layout(preedit, cursor_col, cols);
    let fg = foreground(&config.palette);
    let bg = background(&config.palette);
    let y = rect.min.y + line as f32 * cell_h;
    let x = rect.min.x + layout.start_col as f32 * cell_w;
    let width_pt = layout.width as f32 * cell_w;

    painter.rect_filled(Rect::from_min_size(Pos2::new(x, y), Vec2::new(width_pt, cell_h)), 0.0, bg);

    let mut col = layout.start_col;
    let mut buf = [0u8; 4];
    for ch in layout.visible.chars() {
        painter.text(
            Pos2::new(rect.min.x + col as f32 * cell_w, y),
            egui::Align2::LEFT_TOP,
            ch.encode_utf8(&mut buf).to_string(),
            font_id.clone(),
            fg,
        );
        col += crate::ime::char_cells(ch);
    }

    let uy = y + cell_h - 1.5;
    painter.line_segment([Pos2::new(x, uy), Pos2::new(x + width_pt, uy)], Stroke::new(1.0_f32, fg));

    // Beam caret on the cell the next char lands in, clamped to the grid.
    let caret_col = (layout.start_col + layout.width).min(cols.saturating_sub(1));
    let caret_x = rect.min.x + caret_col as f32 * cell_w;
    painter.rect_filled(
        Rect::from_min_size(Pos2::new(caret_x, y), Vec2::new(2.0, cell_h)),
        0.0,
        fg,
    );
    Some(Rect::from_min_size(Pos2::new(caret_x, y), Vec2::new(cell_w, cell_h)))
}

/// How many cells a character occupies, so a double-width emoji is fitted to
/// both of them rather than squeezed into the first.
fn char_cells(c: char) -> u32 {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(1).max(1) as u32
}

/// Blit a colour glyph into its cell.  Unlike the built-in glyphs, this carries
/// its own colours, so it is tinted white (a no-op multiply) rather than with
/// the cell's foreground.  Placement is already centred within the cell box by
/// the cache, so the offsets only need converting from pixels to points.
fn paint_color_glyph(
    painter: &egui::Painter,
    cached: &CachedColorGlyph,
    cell_x: f32,
    cell_y: f32,
    ppp: f32,
) {
    let rect = Rect::from_min_size(
        Pos2::new(cell_x + cached.left as f32 / ppp, cell_y + cached.top as f32 / ppp),
        Vec2::new(cached.width as f32 / ppp, cached.height as f32 / ppp),
    );
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    painter.image(cached.texture.id(), rect, uv, Color32::WHITE);
}

/// Place the cached pixel-space glyph into the cell.  alacritty positions
/// glyphs as `screen_y_top = baseline - top` with `baseline = cell_bottom`;
/// because we pass `descent = 0` to the renderer, that simplifies to
/// `cell_h - top`.  We do the same arithmetic in logical points by dividing
/// the pixel offsets by `ppp`.
fn paint_builtin_glyph(
    painter: &egui::Painter,
    cached: &crate::builtin_font::CachedGlyph,
    cell_x: f32,
    cell_y: f32,
    cell_h: f32,
    ppp: f32,
    fg: Color32,
) {
    let w_pt = cached.width as f32 / ppp;
    let h_pt = cached.height as f32 / ppp;
    let dy_pt = cell_h - cached.top as f32 / ppp;
    let dx_pt = cached.left as f32 / ppp;
    let glyph_rect =
        Rect::from_min_size(Pos2::new(cell_x + dx_pt, cell_y + dy_pt), Vec2::new(w_pt, h_pt));
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    painter.image(cached.texture.id(), glyph_rect, uv, fg);
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
    use egui::Key;

    use super::*;

    fn term_running(output: &[u8]) -> Term<EventProxy> {
        let (proxy, _events) = EventProxy::new(egui::Context::default());
        let mut term = Term::new(TermConfig::default(), &TermSize::new(80, 24), proxy);
        Processor::<StdSyncHandler>::new().advance(&mut term, output);
        term
    }

    /// A PTY-less session whose grid can be driven straight from a byte
    /// stream.  `spawn_scratchpad` is the only constructor that builds a
    /// `Session` without a child process; dropping the editor afterwards
    /// leaves a plain terminal session behind.
    fn headless_session(ctx: &egui::Context, config: &Config) -> (Session, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scratch.md");
        std::fs::write(&path, "").expect("write scratch file");
        let mut session = Session::spawn_scratchpad(
            ctx.clone(),
            config,
            Some(dir.path().to_path_buf()),
            TermSize::new(80, 24),
            (8.0, 16.0),
            path,
        )
        .expect("scratchpad session");
        session.scratchpad = None;
        (session, dir)
    }

    /// Everything the painter reuses between frames, as the app holds it.
    struct Caches {
        builtin: BuiltinGlyphCache,
        colors: ColorGlyphCache,
        glyphs: GlyphCache,
        ime: crate::ime::Ime,
        snapshot: GridSnapshot,
        detached_jobs: Vec<jobs::Job<()>>,
    }

    impl Caches {
        fn new() -> Self {
            Self {
                builtin: BuiltinGlyphCache::new(),
                colors: ColorGlyphCache::new(Vec::new(), 0),
                glyphs: GlyphCache::new(),
                ime: crate::ime::Ime::default(),
                snapshot: GridSnapshot::new(),
                detached_jobs: Vec::new(),
            }
        }
    }

    /// One full paint of the grid: layout, shape building, and tessellation —
    /// everything between a PTY wakeup and the vertex buffer the GPU gets.
    fn paint_one_frame(
        ctx: &egui::Context,
        session: &mut Session,
        config: &Config,
        caches: &mut Caches,
        screen: Vec2,
    ) -> FrameCost {
        paint_one_frame_on(ctx, session, config, caches, screen, None)
    }

    /// The same frame with the grid routed to `gpu`.  With no GL context the
    /// callback is emitted and never invoked, so what this times is exactly the
    /// CPU half — which is the half that runs on the UI thread and delays a
    /// keystroke.
    fn paint_one_frame_on(
        ctx: &egui::Context,
        session: &mut Session,
        config: &Config,
        caches: &mut Caches,
        screen: Vec2,
        gpu: Option<&crate::grid_gl::GpuGrid>,
    ) -> FrameCost {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let out = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(
                    ui,
                    session,
                    config,
                    &crate::fonts::FaceMetrics::default(),
                    false,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
                    gpu,
                    &mut caches.detached_jobs,
                );
            });
        });
        let build = started.elapsed();
        let ppp = out.pixels_per_point;
        let started = std::time::Instant::now();
        let primitives = ctx.tessellate(out.shapes, ppp);
        let tessellate = started.elapsed();
        let vertices = primitives
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.len(),
                _ => 0,
            })
            .sum();
        FrameCost { build, tessellate, vertices }
    }

    /// Every glyph a focused frame painted, in paint order, after feeding it
    /// `events`.  Reading the frame's own shapes is the only way to tell what
    /// the user saw *that* frame rather than what the terminal state became by
    /// the end of it.
    fn painted_text(
        ctx: &egui::Context,
        session: &mut Session,
        config: &Config,
        caches: &mut Caches,
        screen: Vec2,
        events: Vec<Event>,
    ) -> String {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
            events,
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(
                    ui,
                    session,
                    config,
                    &crate::fonts::FaceMetrics::default(),
                    true,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
                    None,
                    &mut caches.detached_jobs,
                );
            });
        });
        let mut text = String::new();
        for clipped in &out.shapes {
            collect_text(&clipped.shape, &mut text);
        }
        // Tessellate as the real loop does: nothing is on screen until the
        // shapes have become vertices, so a caller timing a frame that skipped
        // this would be timing half of one.
        ctx.tessellate(out.shapes, out.pixels_per_point);
        text
    }

    fn collect_text(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => out.push_str(text.galley.text()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_text(s, out)),
            _ => {},
        }
    }

    /// A context with the three named terminal families bound, as
    /// `fonts::install_terminal_fonts` leaves it in the app.  egui panics on a
    /// family it was never given, so any fixture with a bold or italic cell
    /// needs them.
    fn ctx_with_terminal_faces() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let mono = fonts.families[&FontFamily::Monospace].clone();
        for name in [BOLD_FAMILY, ITALIC_FAMILY, BOLD_ITALIC_FAMILY] {
            fonts.families.insert(FontFamily::Name(name.into()), mono.clone());
        }
        ctx.set_fonts(fonts);
        ctx
    }

    /// A glyph as it was painted: which character, in what colour, and from
    /// which font family.
    #[derive(Debug, PartialEq)]
    struct PaintedGlyph {
        ch: String,
        color: Color32,
        family: FontFamily,
    }

    /// Every glyph and every filled rectangle a focused frame painted.  The
    /// snapshot resolves colours and faces before the painter ever runs, so
    /// this is where a mistake in that resolution becomes visible.
    fn painted_cells(
        ctx: &egui::Context,
        session: &mut Session,
        config: &Config,
        caches: &mut Caches,
        screen: Vec2,
    ) -> (Vec<PaintedGlyph>, Vec<Color32>) {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(
                    ui,
                    session,
                    config,
                    &crate::fonts::FaceMetrics::default(),
                    true,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
                    None,
                    &mut caches.detached_jobs,
                );
            });
        });
        let (mut glyphs, mut fills) = (Vec::new(), Vec::new());
        for clipped in &out.shapes {
            collect_cells(&clipped.shape, &mut glyphs, &mut fills);
        }
        (glyphs, fills)
    }

    fn collect_cells(
        shape: &egui::Shape,
        glyphs: &mut Vec<PaintedGlyph>,
        fills: &mut Vec<Color32>,
    ) {
        match shape {
            egui::Shape::Text(text) => glyphs.push(PaintedGlyph {
                ch: text.galley.text().to_owned(),
                color: text.override_text_color.unwrap_or(text.fallback_color),
                family: text.galley.job.sections[0].format.font_id.family.clone(),
            }),
            egui::Shape::Rect(rect) => fills.push(rect.fill),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_cells(s, glyphs, fills)),
            _ => {},
        }
    }

    /// A blank paints nothing but its background, so a run can hold one in
    /// whatever foreground it carries.  Icons are authored with a trailing
    /// space that the surrounding highlight usually owns, and an over-wide
    /// glyph can only grow into blanks its own run holds.
    #[test]
    fn a_blank_in_another_foreground_joins_the_run() {
        let mut term = term_running(b"\x1b[31mA\x1b[39m \x1b[31mB");
        let mut snapshot = GridSnapshot::new();

        snapshot.capture(&mut term, &Config::default(), 0, None, true);

        let (text, _) =
            snapshot.runs().find(|(text, _)| text.starts_with('A')).expect("a run holding 'A'");
        assert!(text.starts_with("A "), "the blank after 'A' left the run");
    }

    /// The first capture has nothing to reuse, so it walks the whole viewport
    /// however little the terminal reports as damaged.
    #[test]
    fn the_first_capture_covers_every_row() {
        let mut term = term_running(b"hello");
        let mut snapshot = GridSnapshot::new();

        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        assert_eq!(snapshot.dirty_rows(), 0..24);
    }

    /// The whole point of reading damage: a frame that changed one row rewrites
    /// one row.  The cursor stays on the line being written and `Term::damage`
    /// always reports its line, so this is the narrowest span a frame produces.
    #[test]
    fn writing_one_line_dirties_only_that_line() {
        let mut term = term_running(b"first\r\nsecond");
        let mut snapshot = GridSnapshot::new();
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        Processor::<StdSyncHandler>::new().advance(&mut term, b"!");
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        assert_eq!(snapshot.dirty_rows(), 1..2);
    }

    /// Rows the terminal never touched keep the text the last capture gave
    /// them, so a partial capture still describes the whole screen.
    #[test]
    fn an_undamaged_row_keeps_its_text() {
        let mut term = term_running(b"first\r\nsecond");
        let mut snapshot = GridSnapshot::new();
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        Processor::<StdSyncHandler>::new().advance(&mut term, b"!");
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        assert!(
            snapshot.runs().any(|(text, run)| run.row == 0 && text.starts_with("first")),
            "row 0 lost its text when only row 1 was damaged"
        );
    }

    /// One snapshot serves every session, so rows left over from the terminal
    /// it captured last must not survive into the next one.
    #[test]
    fn switching_session_rewrites_every_row() {
        let mut term = term_running(b"first\r\nsecond");
        let mut snapshot = GridSnapshot::new();
        snapshot.capture(&mut term, &Config::default(), 7, None, false);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"!");

        snapshot.capture(&mut term, &Config::default(), 9, None, false);

        assert_eq!(snapshot.dirty_rows(), 0..24);
    }

    /// `Term::damage` documents the selection as the caller's to track, so a
    /// selection appearing over cells the terminal never rewrote has to dirty
    /// them itself.
    #[test]
    fn a_new_selection_dirties_the_rows_it_covers() {
        let mut term = term_running(b"first\r\nsecond\r\nthird\r\nfourth");
        let mut snapshot = GridSnapshot::new();
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        let mut selection =
            Selection::new(SelectionType::Simple, Point::new(Line(1), Column(0)), Side::Left);
        selection.update(Point::new(Line(2), Column(3)), Side::Right);
        term.selection = Some(selection);
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        let dirty = snapshot.dirty_rows();
        assert!(dirty.start <= 1 && dirty.end >= 3, "selection rows 1..3 missing from {dirty:?}");
    }

    /// What the GPU path reads to rebuild records.  Reading the runs of a
    /// clean row costs more than writing the records of the dirty one, and a
    /// row between two damaged ones is exactly as clean as one outside them.
    #[test]
    fn only_the_damaged_rows_are_re_read_for_the_upload() {
        let mut term = term_running(b"first\r\nsecond\r\nthird");
        let mut snapshot = GridSnapshot::new();
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[1;1Hone\x1b[3;1Hthree");
        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        let read: Vec<&str> = snapshot
            .runs_in_rows(snapshot.damaged_rows().iter().copied())
            .map(|(t, _)| t)
            .collect();
        assert!(read.iter().any(|t| t.starts_with("one")), "{read:?}");
        assert!(read.iter().any(|t| t.starts_with("three")), "{read:?}");
        assert!(
            !read.iter().any(|t| t.starts_with("second")),
            "a clean row between two damaged ones was re-read: {read:?}"
        );
    }

    /// A decoration lives in its cells' records, which the GPU holds between
    /// frames, so a frame that rewrites only the rows it was told changed
    /// leaves an underline elsewhere on screen standing.
    #[test]
    fn an_underline_outside_the_damage_survives_the_frame() {
        let grid = crate::grid_gl::GpuGrid::new();
        let mut case = Case::new(Some(&grid));
        let screen = Vec2::new(1280.0, 720.0);
        case.paint(screen);
        case.advance(b"\x1b[4mlinked\x1b[0m\r\nplain");
        case.paint(screen);

        case.advance(b"\x1b[2;1Hrewritten");
        case.paint(screen);

        let state = grid.state.lock().expect("grid state");
        assert_eq!(
            state.instances.glyphs[0].deco,
            decoration_sprites::STRAIGHT,
            "an undamaged row lost its underline",
        );
    }

    /// A wide glyph owns two cells and its decoration belongs to both.  The
    /// terminal writes the spacer from the same cursor template as the
    /// character, so the two differ only in one flag; reading that flag as
    /// style ends the run between the halves and leaves the right half of
    /// every CJK character bare.
    #[test]
    fn a_wide_glyph_decorates_both_of_its_cells() {
        let grid = crate::grid_gl::GpuGrid::new();
        let mut case = Case::new(Some(&grid));
        let screen = Vec2::new(1280.0, 720.0);
        case.paint(screen);
        case.advance("\x1b[4;41m\u{4f60}a".as_bytes());
        case.paint(screen);

        let state = grid.state.lock().expect("grid state");
        let cells = &state.instances.glyphs;
        assert_eq!(
            (0..3).map(|c| cells[c].deco).collect::<Vec<_>>(),
            vec![decoration_sprites::STRAIGHT; 3],
            "the spacer cell of a wide glyph lost its underline",
        );
        assert_eq!(cells[1].bg, cells[0].bg, "the spacer cell of a wide glyph lost its background",);
    }

    /// Both painters take a run's extent from its character count, so a
    /// spacer has to reach them as a character of the run rather than be
    /// dropped on the way.
    #[test]
    fn a_wide_glyph_run_holds_a_character_per_cell() {
        let mut term = term_running("\x1b[41m\u{4f60}\u{597d}".as_bytes());
        let mut snapshot = GridSnapshot::new();

        snapshot.capture(&mut term, &Config::default(), 0, None, true);

        let (text, _) = snapshot
            .runs()
            .find(|(text, _)| text.starts_with('\u{4f60}'))
            .expect("a run holding the wide glyphs");
        assert_eq!(text, "\u{4f60} \u{597d} ", "a spacer went missing from its run");
    }

    /// An underline spans the whole run and is drawn in the run's foreground,
    /// so a blank carrying a different one would come out underlined in the
    /// wrong colour.
    #[test]
    fn an_underlined_blank_in_another_foreground_keeps_its_own_run() {
        let mut term = term_running(b"\x1b[4;31mA\x1b[39m \x1b[31mB");
        let mut snapshot = GridSnapshot::new();

        snapshot.capture(&mut term, &Config::default(), 0, None, true);

        let (text, _) =
            snapshot.runs().find(|(text, _)| text.starts_with('A')).expect("a run holding 'A'");
        assert_eq!(text, "A", "an underlined blank was absorbed");
    }

    /// The IME candidate window is placed from the caret, so the caret has to
    /// keep tracking the cursor cell while the running app has the cursor
    /// hidden (`CSI ?25l`).  Only the drawn cursor goes away; losing the cell
    /// too would pin the candidate popup to the corner of the grid.
    #[test]
    fn a_hidden_cursor_still_leaves_a_caret() {
        let mut term = term_running(b"\x1b[?25labc");
        let mut snapshot = GridSnapshot::new();

        snapshot.capture(&mut term, &Config::default(), 0, None, false);

        assert!(snapshot.cursor.is_none(), "a hidden cursor was drawn");
        assert_eq!(snapshot.caret, Some((3, 0)), "the caret lost the cursor cell");
        assert_eq!(
            cursor_cell_rect(
                &snapshot,
                Rect::from_min_size(Pos2::ZERO, Vec2::new(80.0, 24.0)),
                8.0,
                16.0
            ),
            Some(Rect::from_min_size(Pos2::new(24.0, 0.0), Vec2::new(8.0, 16.0))),
        );
    }

    /// A context whose monospace fallback is scaled far past the primary face,
    /// so a character the primary lacks comes back several times wider than
    /// the cell — a Nerd Font icon against a half-width cell, in miniature.

    /// Where each glyph of a painted frame was placed.
    fn painted_at(
        ctx: &egui::Context,
        session: &mut Session,
        config: &Config,
        caches: &mut Caches,
        screen: Vec2,
    ) -> Vec<(String, f32)> {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(
                    ui,
                    session,
                    config,
                    &crate::fonts::FaceMetrics::default(),
                    true,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
                    None,
                    &mut caches.detached_jobs,
                );
            });
        });
        let mut found = Vec::new();
        for clipped in &out.shapes {
            collect_positions(&clipped.shape, &mut found);
        }
        found
    }

    fn collect_positions(shape: &egui::Shape, out: &mut Vec<(String, f32)>) {
        match shape {
            egui::Shape::Text(text) => out.push((text.galley.text().to_owned(), text.pos.x)),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_positions(s, out)),
            _ => {},
        }
    }
    /// A context whose monospace fallbacks are scaled far past the primary
    /// face, so a character the primary lacks comes back several times wider
    /// than the cell — a Nerd Font icon against a half-width cell, in
    /// miniature.  U+E600 arrives from the bundled icon font, U+FB01 from
    /// Ubuntu-Light.
    fn ctx_with_oversized_fallback() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        // Scaled to overrun by more than the slack but by less than the four
        // extra cells growth may claim, so the icon is centred rather than
        // refused for want of room.
        for (name, scale) in
            [("Ubuntu-Light", 4.0), ("emoji-icon-font", 2.0), ("NotoEmoji-Regular", 2.0)]
        {
            let mut fallback = (*fonts.font_data[name]).clone();
            fallback.tweak.scale = scale;
            fonts.font_data.insert(name.into(), std::sync::Arc::new(fallback));
        }
        let mono = fonts.families[&FontFamily::Monospace].clone();
        for name in [BOLD_FAMILY, ITALIC_FAMILY, BOLD_ITALIC_FAMILY] {
            fonts.families.insert(FontFamily::Name(name.into()), mono.clone());
        }
        ctx.set_fonts(fonts);
        ctx
    }

    /// Every glyph's x position after one frame of a terminal fed `row`.
    fn painted_row(
        ctx: &egui::Context,
        config: &Config,
        screen: Vec2,
        row: &[u8],
    ) -> Vec<(String, f32)> {
        let (mut session, _dir) = headless_session(ctx, config);
        let mut caches = Caches::new();
        painted_at(ctx, &mut session, config, &mut caches, screen);
        let (cols, rows) = (session.size.columns, session.size.screen_lines);
        {
            let mut term = session.term.lock();
            term.resize(TermSize::new(cols, rows));
            Processor::<StdSyncHandler>::new().advance(&mut *term, row);
        }
        painted_at(ctx, &mut session, config, &mut caches, screen)
    }

    fn x_of(painted: &[(String, f32)], ch: &str) -> f32 {
        painted.iter().find(|(g, _)| g == ch).unwrap_or_else(|| panic!("{ch} was not painted")).1
    }

    /// The cell's width and the left edge of column 0, read off a painted row
    /// of characters the primary face serves at its own advance.
    fn cell_geometry(ctx: &egui::Context, config: &Config, screen: Vec2) -> (f32, f32) {
        let row = painted_row(ctx, config, screen, b"MM");
        let xs: Vec<f32> = row.iter().filter(|(g, _)| g == "M").map(|(_, x)| *x).collect();
        let (first, second) = (xs[0].min(xs[1]), xs[0].max(xs[1]));
        (first, second - first)
    }

    /// An icon sized to its own face's em overruns a narrower cell.  It is
    /// drawn across the blanks that follow instead, centred on the span it was
    /// granted.
    #[test]
    fn an_over_wide_icon_is_centred_over_the_blanks_after_it() {
        let ctx = ctx_with_oversized_fallback();
        let config = Config::default();
        let screen = Vec2::new(640.0, 480.0);
        let (origin, cell_w) = cell_geometry(&ctx, &config, screen);

        let painted = painted_row(&ctx, &config, screen, "M\u{e600}".as_bytes());

        let glyph_w =
            ctx.fonts(|f| f.glyph_width(&FontId::monospace(config.font.egui_size()), '\u{e600}'));
        let cells = crate::glyph_cache::grown_cells(glyph_w, cell_w, MAX_EXTRA_CELLS);
        assert!(cells > 1, "the fixture's fallback glyph is not over-wide");

        let start = origin + cell_w;
        let left = x_of(&painted, "\u{e600}") - start;
        let right = start + cells as f32 * cell_w - (x_of(&painted, "\u{e600}") + glyph_w);
        assert!(left > 0.5, "the over-wide icon was left in its own cell");
        assert!((left - right).abs() < 0.5, "not centred: {left} left, {right} right");
    }

    /// Growth may only claim blanks, so an icon can want more cells than it
    /// gets.  Centring it on the shorter span would put it left of its own
    /// cell, over the character before it — worse than the right-hand overrun
    /// growth exists to avoid.  It stays where it is instead.
    #[test]
    fn an_over_wide_icon_is_not_pulled_left_when_the_room_falls_short() {
        let ctx = ctx_with_oversized_fallback();
        let config = Config::default();
        let screen = Vec2::new(640.0, 480.0);
        let (origin, cell_w) = cell_geometry(&ctx, &config, screen);

        let painted = painted_row(&ctx, &config, screen, "M\u{e600} X".as_bytes());

        assert_eq!(
            x_of(&painted, "\u{e600}"),
            origin + cell_w,
            "the icon was pulled left of its own cell"
        );
    }

    /// Growth is for icons.  A letter that happens to arrive from an over-wide
    /// fallback face keeps its cell, so ordinary text never moves — which is
    /// what makes growing safe to do without a switch.
    #[test]
    fn an_over_wide_letter_is_not_grown() {
        let ctx = ctx_with_oversized_fallback();
        let config = Config::default();
        let screen = Vec2::new(640.0, 480.0);
        let (origin, cell_w) = cell_geometry(&ctx, &config, screen);

        let painted = painted_row(&ctx, &config, screen, "M\u{fb01}".as_bytes());

        let glyph_w =
            ctx.fonts(|f| f.glyph_width(&FontId::monospace(config.font.egui_size()), '\u{fb01}'));
        assert!(glyph_w > cell_w * 1.25, "the fixture's letter is not over-wide");
        assert_eq!(x_of(&painted, "\u{fb01}"), origin + cell_w, "a letter was grown");
    }

    /// One shape from a painted frame, reduced to the two kinds whose order
    /// decides what survives: a solid fill, and a glyph drawn on top of it.
    #[derive(Debug, PartialEq)]
    enum Painted {
        Fill(Color32),
        Glyph(String),
    }

    /// Everything a frame painted, in paint order.
    fn painted_order(
        ctx: &egui::Context,
        session: &mut Session,
        config: &Config,
        caches: &mut Caches,
        screen: Vec2,
    ) -> Vec<Painted> {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(
                    ui,
                    session,
                    config,
                    &crate::fonts::FaceMetrics::default(),
                    true,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
                    None,
                    &mut caches.detached_jobs,
                );
            });
        });
        let mut painted = Vec::new();
        for clipped in &out.shapes {
            collect_order(&clipped.shape, &mut painted);
        }
        painted
    }

    fn collect_order(shape: &egui::Shape, out: &mut Vec<Painted>) {
        match shape {
            egui::Shape::Rect(rect) => out.push(Painted::Fill(rect.fill)),
            egui::Shape::Text(text) => out.push(Painted::Glyph(text.galley.text().to_owned())),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_order(s, out)),
            _ => {},
        }
    }

    /// A run's background is an opaque fill over the whole run, so painting the
    /// runs one at a time cuts off whatever the run before overhung into its
    /// first cell — an icon a shade wider than its cell loses its right edge
    /// wherever a colour changes.  Backgrounds all go down first instead.
    #[test]
    fn every_background_is_painted_before_any_glyph() {
        let config = Config::default();
        let ctx = ctx_with_terminal_faces();
        let (mut session, _dir) = headless_session(&ctx, &config);
        let mut caches = Caches::new();
        let screen = Vec2::new(640.0, 480.0);

        painted_order(&ctx, &mut session, &config, &mut caches, screen);
        let (cols, rows) = (session.size.columns, session.size.screen_lines);
        let red = {
            let mut term = session.term.lock();
            term.resize(TermSize::new(cols, rows));
            // A block cursor fills its cell after every run has painted, which
            // would fail this on its own; DECTCEM leaves the run passes alone.
            Processor::<StdSyncHandler>::new().advance(&mut *term, b"\x1b[?25lM\x1b[41mX");
            rgb_to_color32(resolve(
                AnsiColor::Named(alacritty_terminal::vte::ansi::NamedColor::Red),
                Flags::empty(),
                term.colors(),
                &config.palette,
                false,
            ))
        };

        let painted = painted_order(&ctx, &mut session, &config, &mut caches, screen);

        let fill = painted
            .iter()
            .position(|p| *p == Painted::Fill(red))
            .expect("the red run painted no background");
        let glyph = painted
            .iter()
            .position(|p| matches!(p, Painted::Glyph(g) if g == "M"))
            .expect("M was not painted");
        assert!(fill < glyph, "a background was painted over the glyph before it");
    }

    /// The snapshot resolves every cell's foreground, background and face
    /// before the terminal lock is released, so a cell that reverses video,
    /// picks a palette colour, or asks for bold has to come out of that copy
    /// looking the way the terminal asked.
    #[test]
    fn styled_cells_keep_their_colours_and_faces_through_the_snapshot() {
        let config = Config::default();
        let ctx = ctx_with_terminal_faces();
        let (mut session, _dir) = headless_session(&ctx, &config);
        let mut caches = Caches::new();
        let screen = Vec2::new(640.0, 480.0);

        painted_cells(&ctx, &mut session, &config, &mut caches, screen);
        let (cols, rows) = (session.size.columns, session.size.screen_lines);
        {
            let mut term = session.term.lock();
            term.resize(TermSize::new(cols, rows));
            Processor::<StdSyncHandler>::new().advance(
                &mut *term,
                b"\x1b[0mP\x1b[7mR\x1b[0m\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[31mC",
            );
        }

        let (glyphs, fills) = painted_cells(&ctx, &mut session, &config, &mut caches, screen);
        let at = |ch: &str| glyphs.iter().find(|g| g.ch == ch).expect("glyph was not painted");

        let fg = rgb_to_color32(resolve(
            AnsiColor::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            Flags::empty(),
            session.term.lock().colors(),
            &config.palette,
            true,
        ));
        let bg = background(&config.palette);

        assert_eq!(at("P").color, fg, "a plain cell");
        assert_eq!(at("P").family, FontFamily::Monospace);

        // Reverse video swaps the pair, so the glyph takes the default
        // background and the run paints the default foreground behind it.
        assert_eq!(at("R").color, bg, "a reverse-video cell");
        assert!(fills.contains(&fg), "reverse video painted no background behind the glyph");

        assert_eq!(at("B").family, FontFamily::Name(BOLD_FAMILY.into()), "a bold cell");
        assert_eq!(at("I").family, FontFamily::Name(ITALIC_FAMILY.into()), "an italic cell");

        assert_ne!(at("C").color, fg, "SGR 31 painted in the default foreground");
    }

    /// Typing snaps the view back to the prompt (`on_terminal_input_start`), so
    /// the frame carrying the keystroke has to paint the bottom of the buffer.
    /// Consuming input only after the grid is built paints the stale
    /// scrolled-back view for one more frame, and delays the bytes reaching the
    /// PTY by a whole paint.
    #[test]
    fn a_keystroke_reaches_the_terminal_before_the_grid_is_painted() {
        let config = Config::default();
        let ctx = egui::Context::default();
        let (mut session, _dir) = headless_session(&ctx, &config);
        let mut caches = Caches::new();
        let screen = Vec2::new(640.0, 480.0);

        // The first frame is what tells the session how big the grid is, and
        // `Session::resize` forwards to the `Term` only when a PTY sender
        // exists, so a headless session needs the grid resized by hand.
        painted_text(&ctx, &mut session, &config, &mut caches, screen, Vec::new());
        let (cols, rows) = (session.size.columns, session.size.screen_lines);
        {
            let mut term = session.term.lock();
            term.resize(TermSize::new(cols, rows));
            let mut output = Vec::new();
            for line in 0..rows * 4 {
                output.extend_from_slice(format!("L{line}\r\n").as_bytes());
            }
            Processor::<StdSyncHandler>::new().advance(&mut *term, &output);
            term.scroll_display(Scroll::Delta(rows as i32));
        }
        let last = format!("L{}", rows * 4 - 1);

        let scrolled_back =
            painted_text(&ctx, &mut session, &config, &mut caches, screen, Vec::new());
        assert!(!scrolled_back.contains(&last), "the grid is not scrolled back to begin with");

        let typed = painted_text(
            &ctx,
            &mut session,
            &config,
            &mut caches,
            screen,
            vec![Event::Text("a".to_owned())],
        );

        assert!(
            typed.contains(&last),
            "the frame carrying the keystroke painted the stale scrolled-back view"
        );
    }

    /// The snapshot's run text and run list are reused frame to frame, so an
    /// unchanged grid has to paint the same thing twice — a missed clear would
    /// append the second frame's runs to the first's.
    #[test]
    fn a_reused_snapshot_paints_an_unchanged_grid_the_same_way() {
        let config = Config::default();
        let ctx = egui::Context::default();
        let (mut session, _dir) = headless_session(&ctx, &config);
        let mut caches = Caches::new();
        let screen = Vec2::new(640.0, 480.0);

        painted_text(&ctx, &mut session, &config, &mut caches, screen, Vec::new());
        let (cols, rows) = (session.size.columns, session.size.screen_lines);
        {
            let mut term = session.term.lock();
            term.resize(TermSize::new(cols, rows));
            Processor::<StdSyncHandler>::new().advance(&mut *term, &dense_screen(cols, rows));
        }

        let first = painted_text(&ctx, &mut session, &config, &mut caches, screen, Vec::new());
        let second = painted_text(&ctx, &mut session, &config, &mut caches, screen, Vec::new());

        assert!(!first.is_empty(), "the fixture painted nothing");
        assert_eq!(first, second);
    }

    /// Where a frame's time goes.  `build` is the grid walk that turns cells
    /// into shapes — the part damage tracking can skip; `tessellate` turns
    /// those shapes into vertices and runs whether or not anything changed.
    #[derive(Default, Clone, Copy)]
    struct FrameCost {
        build: std::time::Duration,
        tessellate: std::time::Duration,
        vertices: usize,
    }

    /// How much of the grid `Term` reports as damaged, as a renderer that
    /// wanted to repaint only what changed would see it.
    fn damage_extent(term: &mut Term<EventProxy>) -> String {
        let extent = match term.damage() {
            alacritty_terminal::term::TermDamage::Full => "FULL".to_string(),
            alacritty_terminal::term::TermDamage::Partial(lines) => {
                format!("{} lines", lines.count())
            },
        };
        term.reset_damage();
        extent
    }

    /// Not a gate — run it by hand:
    /// `cargo test -p alacritree --release -- --ignored --nocapture report_damage`
    ///
    /// Decides whether damage can drive a partial repaint at all.  Scrolling
    /// the screen marks the whole terminal damaged (`Term::scroll_up_relative`
    /// calls `mark_fully_damaged`), and a program appending output to a full
    /// screen scrolls on every line.
    #[test]
    #[ignore = "reporting harness, not an assertion"]
    fn report_damage_under_output() {
        let (proxy, _events) = EventProxy::new(egui::Context::default());
        let mut term = Term::new(TermConfig::default(), &TermSize::new(80, 24), proxy);
        let mut parser = Processor::<StdSyncHandler>::new();

        parser.advance(&mut term, &dense_screen(80, 24));
        term.reset_damage();

        parser.advance(&mut term, b"\x1b[5;1Hin-place edit");
        println!("write inside the screen, no scroll: {}", damage_extent(&mut term));

        // The cursor is parked on the last row by the fill, so a newline here
        // scrolls — the steady state for any program streaming output.
        parser.advance(&mut term, b"\x1b[24;1H");
        term.reset_damage();
        parser.advance(&mut term, b"one appended line\r\n");
        println!("append one line at the bottom: {}", damage_extent(&mut term));

        parser.advance(&mut term, b"another line\r\n");
        term.reset_damage();
        parser.advance(&mut term, b"and another\r\n");
        println!("append again: {}", damage_extent(&mut term));

        term.scroll_display(Scroll::Delta(5));
        term.reset_damage();
        parser.advance(&mut term, b"output while scrolled back\r\n");
        println!("append while scrolled back: {}", damage_extent(&mut term));
    }

    /// Not a gate — run it by hand:
    /// `cargo test -p alacritree --release -- --ignored --nocapture report_lock_contention`
    ///
    /// The PTY thread applies output under the same `FairMutex` the painter
    /// holds, so whatever a frame holds it for is time the terminal cannot
    /// parse — and the echo of a keystroke waits behind it.  Measured from the
    /// PTY thread's side: how long acquiring the lock takes while frames paint.
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_lock_contention() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let config = Config::default();
        for screen in [Vec2::new(1280.0, 720.0), Vec2::new(2560.0, 1440.0)] {
            let ctx = egui::Context::default();
            let (mut session, _dir) = headless_session(&ctx, &config);
            let mut caches = Caches::new();

            paint_one_frame(&ctx, &mut session, &config, &mut caches, screen);
            let (cols, rows) = (session.size.columns, session.size.screen_lines);
            {
                let mut term = session.term.lock();
                term.resize(TermSize::new(cols, rows));
                Processor::<StdSyncHandler>::new().advance(&mut *term, &dense_screen(cols, rows));
            }

            let term = Arc::clone(&session.term);
            let stop = Arc::new(AtomicBool::new(false));
            let locker = {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let (mut waits, mut total, mut worst) =
                        (0u32, std::time::Duration::ZERO, std::time::Duration::ZERO);
                    // The PTY thread does not just take the lock, it writes
                    // under it.  A locker that only acquires and releases
                    // leaves the terminal unchanged, so every frame after the
                    // first captures the empty-damage path and holds the lock
                    // for a fraction of what a real frame holds it for.  One
                    // line per acquisition is the shape of a PTY read, and
                    // damage accumulates across the frames it spans.
                    let mut parser = Processor::<StdSyncHandler>::new();
                    let mut line = 0usize;
                    while !stop.load(Ordering::Relaxed) {
                        let output = format!(
                            "\x1b[{};1H\x1b[38;5;{}m{}",
                            line % rows + 1,
                            line % 256,
                            "sample output "
                                .repeat(cols / 14)
                                .chars()
                                .take(cols)
                                .collect::<String>(),
                        );
                        line += 1;
                        let started = std::time::Instant::now();
                        let mut guard = term.lock();
                        let waited = started.elapsed();
                        parser.advance(&mut *guard, output.as_bytes());
                        drop(guard);
                        waits += 1;
                        total += waited;
                        worst = worst.max(waited);
                        std::thread::yield_now();
                    }
                    (waits, total, worst)
                })
            };

            let iterations = 60;
            let started = std::time::Instant::now();
            for _ in 0..iterations {
                paint_one_frame(&ctx, &mut session, &config, &mut caches, screen);
            }
            let elapsed = started.elapsed();
            stop.store(true, Ordering::Relaxed);
            let (waits, total, worst) = locker.join().expect("locker thread");

            println!(
                "{}x{} logical px = {cols}x{rows} cells: {:?} per frame, PTY-side lock waits \
                 {waits} × {:?} mean (worst {worst:?}), {:.0}% of the run blocked",
                screen.x,
                screen.y,
                elapsed / iterations,
                total / waits.max(1),
                100.0 * total.as_secs_f64() / elapsed.as_secs_f64(),
            );
        }
    }

    /// Not a gate — run it by hand:
    /// `cargo test -p alacritree --release -- --ignored --nocapture --test-threads=1 report_echo_latency`
    ///
    /// What the user actually waits for: output reaching the terminal, and
    /// that output reaching the screen.  The frame loop is modelled the way
    /// eframe drives it — a frame runs only when something asked for a repaint
    /// — and the sample is written from another thread so it lands mid-frame
    /// the way real PTY output does.  The paint harnesses time one frame in
    /// isolation; this times the wait a frame is only part of.
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_echo_latency() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use alacritty_terminal::event::{Event as TermEvent, EventListener};

        /// Written to a fixed cell, so each sample overwrites the last and a
        /// stale marker can never be mistaken for the pending one.
        const MARKERS: [char; 2] = ['§', '¶'];
        const SAMPLES: usize = 150;

        let config = Config::default();
        let screen = Vec2::new(2560.0, 1440.0);

        // The last row is the control: the same background load with the
        // sessions left marked visible, which is what every session looked
        // like before off-screen output stopped waking the loop.
        for (load, background, mark_hidden, stream_visible) in [
            ("idle", 0, true, false),
            ("visible session streaming", 0, true, true),
            ("8 background sessions streaming", 8, true, false),
            ("8 background sessions, none marked hidden", 8, false, false),
        ] {
            let ctx = egui::Context::default();
            let (mut session, _dir) = headless_session(&ctx, &config);
            let mut caches = Caches::new();
            painted_text(&ctx, &mut session, &config, &mut caches, screen, Vec::new());
            let (cols, rows) = (session.size.columns, session.size.screen_lines);
            {
                let mut term = session.term.lock();
                term.resize(TermSize::new(cols, rows));
                Processor::<StdSyncHandler>::new().advance(&mut *term, &dense_screen(cols, rows));
            }

            let stop = Arc::new(AtomicBool::new(false));
            let mut threads = Vec::new();

            // A background session reaches the loop through `send_event` and
            // nothing else, so its proxy is the whole of what matters here.
            for _ in 0..background {
                let (proxy, events) = EventProxy::new(ctx.clone());
                proxy.set_visible(!mark_hidden);
                let stop = Arc::clone(&stop);
                threads.push(std::thread::spawn(move || {
                    let _events = events;
                    while !stop.load(Ordering::Relaxed) {
                        proxy.send_event(TermEvent::Wakeup);
                        std::thread::sleep(std::time::Duration::from_micros(500));
                    }
                }));
            }

            if stream_visible {
                let term = Arc::clone(&session.term);
                let ctx = ctx.clone();
                let stop = Arc::clone(&stop);
                threads.push(std::thread::spawn(move || {
                    let mut parser = Processor::<StdSyncHandler>::new();
                    while !stop.load(Ordering::Relaxed) {
                        // In place rather than appending: the load under test
                        // is repaint pressure and parse work, and scrolling
                        // would carry the marker cell off screen.
                        parser.advance(&mut *term.lock(), b"\x1b[20;1Hstreaming output");
                        ctx.request_repaint();
                        std::thread::sleep(std::time::Duration::from_micros(500));
                    }
                }));
            }

            // Timestamped before the lock is taken: waiting for the terminal
            // is part of what the output waits through.
            let pending: Arc<std::sync::Mutex<Option<(char, std::time::Instant)>>> =
                Arc::new(std::sync::Mutex::new(None));
            {
                let (term, ctx, stop, pending) = (
                    Arc::clone(&session.term),
                    ctx.clone(),
                    Arc::clone(&stop),
                    Arc::clone(&pending),
                );
                threads.push(std::thread::spawn(move || {
                    let mut parser = Processor::<StdSyncHandler>::new();
                    let mut next = 0;
                    while !stop.load(Ordering::Relaxed) {
                        if pending.lock().expect("pending").is_some() {
                            std::thread::yield_now();
                            continue;
                        }
                        // Settle first, by an interval that does not divide
                        // the frame period: injecting the moment the last
                        // sample landed would phase-lock to the loop and
                        // measure how fast it can cycle rather than how long
                        // an arbitrary write waits.  Spun rather than slept
                        // because Windows rounds a sleep up to the timer tick.
                        let gap = std::time::Duration::from_micros(2000 + (next as u64 % 7) * 1500);
                        let until = std::time::Instant::now() + gap;
                        while std::time::Instant::now() < until {
                            std::hint::spin_loop();
                        }

                        let marker = MARKERS[next % MARKERS.len()];
                        next += 1;
                        let at = std::time::Instant::now();
                        parser.advance(&mut *term.lock(), format!("\x1b[1;1H{marker}").as_bytes());
                        *pending.lock().expect("pending") = Some((marker, at));
                        ctx.request_repaint();
                    }
                }));
            }

            let mut samples: Vec<std::time::Duration> = Vec::with_capacity(SAMPLES);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while samples.len() < SAMPLES && std::time::Instant::now() < deadline {
                if !ctx.has_requested_repaint() {
                    std::thread::yield_now();
                    continue;
                }
                let painted =
                    painted_text(&ctx, &mut session, &config, &mut caches, screen, Vec::new());
                let mut slot = pending.lock().expect("pending");
                if let Some((marker, at)) = *slot
                    && painted.contains(marker)
                {
                    samples.push(at.elapsed());
                    *slot = None;
                }
            }

            stop.store(true, Ordering::Relaxed);
            for thread in threads {
                let _ = thread.join();
            }

            samples.sort_unstable();
            let at = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
            println!(
                "{load}: {} samples, p50 {:?}, p95 {:?}, p99 {:?}, worst {:?}",
                samples.len(),
                at(0.50),
                at(0.95),
                at(0.99),
                samples[samples.len() - 1],
            );
        }
    }

    /// Not a gate — run it by hand:
    /// `cargo test -p alacritree --release -- --ignored --nocapture --test-threads=1 report_cost_by_rows`
    ///
    /// A full-screen app repaints in place, so its damage is a handful of rows
    /// rather than the whole grid — the one workload where skipping unchanged
    /// rows is possible at all.  What that would be worth is how much of a
    /// frame the rows carry: blank rows emit no glyphs, so painting `filled`
    /// of `rows` approximates repainting only that many.
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_cost_by_rows() {
        let config = Config::default();
        let screen = Vec2::new(2560.0, 1440.0);
        let ctx = egui::Context::default();
        let (mut session, _dir) = headless_session(&ctx, &config);
        let mut caches = Caches::new();
        paint_one_frame(&ctx, &mut session, &config, &mut caches, screen);
        let (cols, rows) = (session.size.columns, session.size.screen_lines);
        session.term.lock().resize(TermSize::new(cols, rows));

        for filled in [rows, rows / 2, 8, 3, 1] {
            {
                let mut term = session.term.lock();
                let mut parser = Processor::<StdSyncHandler>::new();
                parser.advance(&mut *term, b"\x1b[2J");
                parser.advance(&mut *term, &dense_screen(cols, filled));
            }

            let mut cost = FrameCost::default();
            for _ in 0..10 {
                cost = paint_one_frame(&ctx, &mut session, &config, &mut caches, screen);
            }
            let iterations = 40;
            let (mut build, mut tessellate) =
                (std::time::Duration::ZERO, std::time::Duration::ZERO);
            for _ in 0..iterations {
                cost = std::hint::black_box(paint_one_frame(
                    &ctx,
                    &mut session,
                    &config,
                    &mut caches,
                    screen,
                ));
                build += cost.build;
                tessellate += cost.tessellate;
            }

            println!(
                "{filled:>3} of {rows} rows with content: build {:?} + tessellate {:?}, {} vertices",
                build / iterations,
                tessellate / iterations,
                cost.vertices,
            );
        }
    }

    /// Dense output that fills every visible cell, with a colour change every
    /// few columns so the run-splitting in `paint_grid` behaves like it does
    /// under real program output rather than collapsing to one run per line.
    ///
    /// Written once, this screen never changes again, so every frame after the
    /// first finds the terminal undamaged.  That is the wrong shape for
    /// anything measuring a renderer that skips clean rows — use
    /// `termbench_frame` there, which moves every cell every frame.
    fn dense_screen(cols: usize, rows: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for row in 0..rows {
            out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
            for col in 0..cols {
                if col % 7 == 0 {
                    out.extend_from_slice(format!("\x1b[3{}m", 1 + (col / 7) % 7).as_bytes());
                }
                out.push(b'a' + (col % 26) as u8);
            }
        }
        out
    }

    /// A row between two damaged ones holds records nothing invalidated, so
    /// the frame owes it nothing.  Handing the painter the span instead makes
    /// one edited line and one repainted status bar cost the whole screen.
    #[test]
    fn a_frame_rewrites_only_the_rows_the_terminal_damaged() {
        let scattered = [0, 82];
        assert_eq!(rows_to_rewrite(&scattered, 83, false).collect::<Vec<_>>(), scattered);
    }

    /// Clearing renumbers the table from nothing, so every record written
    /// against the old numbering now addresses some other character.  Nothing
    /// on screen is still correct, whatever the terminal reports as damaged.
    #[test]
    fn a_renumbered_glyph_table_rewrites_every_row() {
        assert_eq!(rows_to_rewrite(&[1], 3, true).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    /// A driver that rejects the shaders leaves the paint callback with
    /// nothing to draw, and the callback is the only thing that knows.  Unless
    /// the grid goes back to the mesh from the next frame on, the terminal is
    /// a blank rectangle for the life of the process.
    #[test]
    fn a_gpu_grid_that_will_not_build_paints_the_mesh() {
        let grid = crate::grid_gl::GpuGrid::new();
        let mut case = Case::new(Some(&grid));
        let screen = Vec2::new(1280.0, 720.0);
        case.advance(b"hello");

        let gpu = case.paint(screen);
        grid.mark_unavailable();
        let mesh = case.paint(screen);

        assert!(
            mesh.vertices > gpu.vertices,
            "a grid that cannot build GL painted {} vertices, no more than the {} the GL path \
             emits for its one geometry-free shape",
            mesh.vertices,
            gpu.vertices,
        );
    }

    /// A decoration is a flag on the cells it covers, and the fragment shader
    /// draws it from there.  Left to the painter it is a shape per run, which
    /// on a screen of underlined text is more geometry than the whole grid.
    #[test]
    fn a_decorated_run_costs_no_geometry() {
        let (plain_grid, decorated_grid) =
            (crate::grid_gl::GpuGrid::new(), crate::grid_gl::GpuGrid::new());
        let mut plain = Case::new(Some(&plain_grid));
        let mut decorated = Case::new(Some(&decorated_grid));
        let screen = Vec2::new(1280.0, 720.0);
        // The first frame is what tells the session how big its grid is.
        plain.paint(screen);
        decorated.paint(screen);

        plain.advance(b"struck");
        decorated.advance(b"\x1b[4;9mstruck");
        let without = plain.paint(screen);
        let with = decorated.paint(screen);

        let state = decorated_grid.state.lock().expect("grid state");
        assert_eq!(
            state.instances.glyphs[0].deco,
            decoration_sprites::tile(decoration_sprites::STRAIGHT, true),
            "the decoration never reached the cell's record",
        );
        assert_eq!(
            with.vertices,
            without.vertices,
            "a decorated run tessellated {} vertices the undecorated one did not",
            with.vertices - without.vertices,
        );
    }

    /// One painter under test: its own terminal, its own caches, its own egui
    /// context, fed the same bytes as every other case in the sweep.
    struct Case<'a> {
        gpu: Option<&'a crate::grid_gl::GpuGrid>,
        config: Config,
        ctx: egui::Context,
        session: Session,
        _dir: tempfile::TempDir,
        caches: Caches,
        parser: Processor<StdSyncHandler>,
    }

    impl<'a> Case<'a> {
        fn new(gpu: Option<&'a crate::grid_gl::GpuGrid>) -> Self {
            let mut config = Config::default();
            config.ui.gpu_grid = gpu.is_some();
            let ctx = egui::Context::default();
            let (session, dir) = headless_session(&ctx, &config);
            Self {
                gpu,
                config,
                ctx,
                session,
                _dir: dir,
                caches: Caches::new(),
                parser: Processor::new(),
            }
        }

        fn advance(&mut self, bytes: &[u8]) {
            let mut term = self.session.term.lock();
            self.parser.advance(&mut *term, bytes);
        }

        fn paint(&mut self, screen: Vec2) -> FrameCost {
            paint_one_frame_on(
                &self.ctx,
                &mut self.session,
                &self.config,
                &mut self.caches,
                screen,
                self.gpu,
            )
        }
    }

    /// Not a gate — run it by hand:
    /// `cargo test -p alacritree --release -- --ignored --nocapture paint_cost`
    ///
    /// Every PTY wakeup requests a repaint, and a repaint runs this whole path
    /// for the visible session.  What it costs is what a keystroke queues
    /// behind while a session is streaming output.
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_paint_cost() {
        let config = Config::default();
        for screen in [Vec2::new(1280.0, 720.0), Vec2::new(2560.0, 1440.0)] {
            let ctx = egui::Context::default();
            let (mut session, _dir) = headless_session(&ctx, &config);
            let mut caches = Caches::new();

            // The first frame is what tells the session how big the grid is.
            paint_one_frame(&ctx, &mut session, &config, &mut caches, screen);
            // `Session::resize` forwards to the `Term` only when a PTY sender
            // exists, so a headless session needs the grid resized by hand.
            let (cols, rows) = (session.size.columns, session.size.screen_lines);
            {
                let mut term = session.term.lock();
                term.resize(TermSize::new(cols, rows));
                Processor::<StdSyncHandler>::new().advance(&mut *term, &dense_screen(cols, rows));
            }

            let mut cost = FrameCost::default();
            for _ in 0..10 {
                cost = paint_one_frame(&ctx, &mut session, &config, &mut caches, screen);
            }

            let iterations = 60;
            let start = std::time::Instant::now();
            let (mut build, mut tessellate) =
                (std::time::Duration::ZERO, std::time::Duration::ZERO);
            for _ in 0..iterations {
                cost = std::hint::black_box(paint_one_frame(
                    &ctx,
                    &mut session,
                    &config,
                    &mut caches,
                    screen,
                ));
                build += cost.build;
                tessellate += cost.tessellate;
            }
            let each = start.elapsed() / iterations;
            let (build, tessellate) = (build / iterations, tessellate / iterations);

            let (_, counts) = crate::steady_state::measure(|| {
                paint_one_frame(&ctx, &mut session, &config, &mut caches, screen)
            });

            println!(
                "{}x{} logical px = {cols}x{rows} cells: {each:?} per frame (build {build:?} + \
                 tessellate {tessellate:?}), {} allocations ({} KiB), {} vertices",
                screen.x,
                screen.y,
                counts.allocs,
                counts.bytes / 1024,
                cost.vertices,
            );
        }
    }

    /// Full-screen apps hide the cursor with DECTCEM while they repaint, then
    /// leave it parked wherever their last write landed.  Drawing it anyway
    /// drops a block into an arbitrary spot on top of their UI.
    #[test]
    fn a_cursor_the_app_hid_is_not_drawn() {
        let term = term_running(b"\x1b[?25l\x1b[10;40Hrepainting");

        assert_eq!(
            cursor_shape(&term),
            CursorShape::Hidden,
            "the app asked for the cursor to be hidden, but it is still painted at {:?}",
            term.grid().cursor.point,
        );
    }

    #[test]
    fn a_cursor_the_app_unhid_is_drawn_again() {
        let term = term_running(b"\x1b[?25l\x1b[?25h");

        assert_ne!(cursor_shape(&term), CursorShape::Hidden);
    }

    #[test]
    fn a_cursor_no_app_touched_is_drawn() {
        let term = term_running(b"$ ");

        assert_ne!(cursor_shape(&term), CursorShape::Hidden);
    }

    /// Two frames so egui's layer memory settles: areas register during a
    /// frame, and the modal layer becomes queryable only after end-of-frame.
    fn run_frames(ctx: &egui::Context, overlay: impl Fn(&egui::Context)) {
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |_| {});
                overlay(ctx);
            });
        }
    }

    #[test]
    fn bare_grid_owns_the_pointer_inside_its_rect() {
        let ctx = egui::Context::default();
        run_frames(&ctx, |_| {});
        let grid = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));

        assert!(pointer_owns_grid(
            &ctx,
            egui::LayerId::background(),
            grid,
            Pos2::new(100.0, 100.0)
        ));
        assert!(!pointer_owns_grid(
            &ctx,
            egui::LayerId::background(),
            grid,
            Pos2::new(900.0, 100.0)
        ));
    }

    /// While a modal is open egui resolves *every* position to the modal's
    /// layer — a drag that starts inside the dialog must not stream button and
    /// motion reports to a mouse-tracking app underneath, which would visibly
    /// drag out a selection in TUIs that track the mouse.
    #[test]
    fn an_open_modal_owns_every_position() {
        let ctx = egui::Context::default();
        run_frames(&ctx, |ctx| {
            egui::Modal::new(egui::Id::new("dialog")).show(ctx, |ui| {
                ui.label("rename");
            });
        });
        let grid = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));

        assert!(!pointer_owns_grid(
            &ctx,
            egui::LayerId::background(),
            grid,
            Pos2::new(100.0, 100.0)
        ));
    }

    #[test]
    fn a_floating_window_owns_only_its_own_rect() {
        let ctx = egui::Context::default();
        run_frames(&ctx, |ctx| {
            egui::Area::new(egui::Id::new("floating")).fixed_pos(Pos2::new(100.0, 100.0)).show(
                ctx,
                |ui| {
                    ui.allocate_space(Vec2::new(50.0, 50.0));
                },
            );
        });
        let grid = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));

        assert!(!pointer_owns_grid(
            &ctx,
            egui::LayerId::background(),
            grid,
            Pos2::new(110.0, 110.0)
        ));
        assert!(pointer_owns_grid(
            &ctx,
            egui::LayerId::background(),
            grid,
            Pos2::new(400.0, 400.0)
        ));
    }

    /// Text of the topmost visible grid line, as the painter would render it.
    #[cfg(windows)]
    fn top_screen_line(session: &Session) -> String {
        let term = session.term.lock();
        let grid = term.grid();
        (0..grid.columns())
            .map(|col| {
                let cell = &grid[Line(0)][Column(col)];
                if cell.c == '\0' { ' ' } else { cell.c }
            })
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[cfg(windows)]
    fn wait_for_top_line(session: &Session, wanted: &str) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let top = top_screen_line(session);
            if top.starts_with(wanted) {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(top);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A wheel tick over the diff pane must page its pager.  ConPTY repaints
    /// the pager's alternate screen onto the primary one, so gating the
    /// arrow-key route on ALT_SCREEN alone sends the wheel into the pane's
    /// (empty) scrollback instead — the pager never moves.  Drives a real
    /// `less` under a real ConPTY through `apply_scroll`.
    #[cfg(windows)]
    #[test]
    fn a_wheel_tick_scrolls_the_diff_panes_pager() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        for i in 1..=200 {
            writeln!(file, "line {i} content").unwrap();
        }
        drop(file);

        let mut session = match Session::spawn_command(
            egui::Context::default(),
            &Config::default(),
            Some(dir.path().to_path_buf()),
            TermSize::new(80, 24),
            (8.0, 16.0),
            "less".to_string(),
            vec![path.to_string_lossy().into_owned()],
            "diff: body.txt".to_string(),
            SessionKind::Diff { key: "probe".to_string() },
        ) {
            Ok(session) => session,
            // No `less` on this machine (it ships with Git for Windows, which
            // the diff pane's delta pipeline needs anyway) — nothing to test.
            Err(e) => {
                eprintln!("skipping: could not spawn less: {e}");
                return;
            },
        };

        wait_for_top_line(&session, "line 1 ")
            .unwrap_or_else(|top| panic!("less never drew the file; top line: {top:?}"));

        // One wheel notch down: a cell height of pixels.  The default
        // `scrolling.multiplier` of 3 turns it into three pager lines.
        let config = Config::default();
        let mode = *session.term.lock().mode();
        apply_scroll(
            &mut session,
            &config,
            0.0,
            -16.0,
            8.0,
            16.0,
            Modifiers::default(),
            None,
            mode,
        );

        wait_for_top_line(&session, "line 4 ").unwrap_or_else(|top| {
            panic!("the wheel tick did not scroll the pager; top line: {top:?}")
        });
    }

    /// egui-winit raises `Event::Paste` for every `command+V` press, Shift
    /// included, so honoring it here would paste on Ctrl+V no matter what the
    /// binding table says — and leave the shortcut impossible to rebind.
    #[test]
    fn paste_event_does_not_reach_the_terminal() {
        assert!(consumed_event(&Event::Paste("hi".into()), None, TermMode::empty()).is_none());
    }

    /// egui-winit raises `Event::Copy` for Ctrl+C alongside the key press.
    /// The whole event stream has to yield ETX, or a program whose only exit
    /// is the interrupt cannot be stopped.
    #[test]
    fn ctrl_c_sends_the_interrupt_through_the_whole_event_stream() {
        let press = Event::Key {
            key: Key::C,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        let stream = [Event::Copy, press];

        let bytes: Vec<u8> = consume_events(&stream, TermMode::empty())
            .into_iter()
            .flat_map(|e| match e {
                ConsumedEvent::Bytes(b) => b,
                _ => Vec::new(),
            })
            .collect();

        assert_eq!(bytes, vec![0x03], "Ctrl+C must reach the PTY as 0x03");
    }

    /// Alacritty sends SYN on Ctrl+V; paste is a Ctrl+Shift+V binding.
    #[test]
    fn ctrl_v_sends_the_control_byte() {
        let press = Event::Key {
            key: Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        assert!(
            matches!(consumed_event(&press, None, TermMode::empty()), Some(ConsumedEvent::Bytes(ref b)) if b == &vec![0x16]),
            "Ctrl+V must reach the PTY as 0x16"
        );
    }

    /// Grid rect for the pointer-mapping tests: origin away from 0 so a
    /// pointer left of the grid produces a genuinely negative local offset.
    fn grid_rect(cell_w: f32, cell_h: f32, cols: usize, rows: usize) -> Rect {
        Rect::from_min_size(
            Pos2::new(120.0, 40.0),
            Vec2::new(cols as f32 * cell_w, rows as f32 * cell_h),
        )
    }

    #[test]
    fn dragging_past_the_left_edge_keeps_the_first_column() {
        let (cell_w, cell_h, cols, rows) = (10.0, 20.0, 80, 24);
        let rect = grid_rect(cell_w, cell_h, cols, rows);

        // Every pointer left of the grid anchors on the left of column 0, so
        // dragging out of the window never drops the leftmost character.
        for offset in [-0.1, -0.3, -0.6, -0.9, -4.0, -40.0] {
            let pos = Pos2::new(rect.min.x + offset * cell_w, rect.min.y + 0.5 * cell_h);
            let (point, side) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, 0);
            assert_eq!(
                (point.column, side),
                (Column(0), Side::Left),
                "pointer {offset} cells left of the grid must anchor left of column 0"
            );
        }
    }

    #[test]
    fn selection_dragged_off_the_left_edge_includes_the_first_character() {
        let (cell_w, cell_h, cols, rows) = (10.0, 20.0, 80, 24);
        let rect = grid_rect(cell_w, cell_h, cols, rows);
        let mut term = term_running(b"hello");

        let press = Pos2::new(rect.min.x + 4.5 * cell_w, rect.min.y + 0.5 * cell_h);
        let (anchor, anchor_side) = cell_at_pos(press, rect, cell_w, cell_h, cols, rows, 0);
        let mut selection = Selection::new(SelectionType::Simple, anchor, anchor_side);

        let dragged = Pos2::new(rect.min.x - 0.3 * cell_w, rect.min.y + 0.5 * cell_h);
        let (point, side) = cell_at_pos(dragged, rect, cell_w, cell_h, cols, rows, 0);
        selection.update(point, side);
        term.selection = Some(selection);

        assert_eq!(term.selection_to_string().as_deref(), Some("hello"));
    }

    #[test]
    fn dragging_past_the_right_edge_keeps_the_last_column() {
        let (cell_w, cell_h, cols, rows) = (10.0, 20.0, 80, 24);
        let rect = grid_rect(cell_w, cell_h, cols, rows);

        for offset in [0.1, 0.4, 1.0, 30.0] {
            let x = rect.min.x + (cols as f32 + offset) * cell_w;
            let pos = Pos2::new(x, rect.min.y + 0.5 * cell_h);
            let (point, side) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, 0);
            assert_eq!(
                (point.column, side),
                (Column(cols - 1), Side::Right),
                "pointer {offset} cells right of the grid must anchor right of the last column"
            );
        }
    }

    #[test]
    fn cell_side_splits_each_cell_at_its_midpoint() {
        let (cell_w, cell_h, cols, rows) = (10.0, 20.0, 80, 24);
        let rect = grid_rect(cell_w, cell_h, cols, rows);
        let at = |frac: f32| {
            let pos = Pos2::new(rect.min.x + frac * cell_w, rect.min.y + 0.5 * cell_h);
            cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, 0)
        };

        assert_eq!(at(0.0), (Point::new(Line(0), Column(0)), Side::Left));
        assert_eq!(at(0.49), (Point::new(Line(0), Column(0)), Side::Left));
        assert_eq!(at(0.51), (Point::new(Line(0), Column(0)), Side::Right));
        assert_eq!(at(1.2), (Point::new(Line(0), Column(1)), Side::Left));
    }
}
