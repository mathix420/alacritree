use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::search::Match;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape};
use egui::{
    Color32, CursorIcon, Event, FontFamily, FontId, ImeEvent, Modifiers, MouseWheelUnit,
    PointerButton, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2,
};

use crate::builtin_font::{BuiltinGlyphCache, Metrics, is_builtin_glyph};
use crate::clipboard::{self, Target};
use crate::color_glyph::{CachedColorGlyph, ColorGlyphCache};
use crate::colors::{background, foreground, resolve, rgb_to_color32};
use crate::config::Config;
use crate::fonts::{BOLD_FAMILY, BOLD_ITALIC_FAMILY, ITALIC_FAMILY};
use crate::glyph_cache::{Face, GlyphCache};
use crate::input::event_to_bytes;
use crate::links::{self, Link};
use crate::mouse;
use crate::paste;
use crate::session::{EventProxy, Session, SessionKind, TermSize};

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    session: &mut Session,
    config: &Config,
    allow_focus: bool,
    builtin_glyphs: &mut BuiltinGlyphCache,
    ime: &mut crate::ime::Ime,
    color_glyphs: &mut ColorGlyphCache,
    glyphs: &mut GlyphCache,
    snapshot: &mut GridSnapshot,
) -> Response {
    let font_id = FontId::monospace(config.font.egui_size());
    let (cell_w_pt, cell_h_pt) = ui.ctx().fonts(|f| {
        let w = f.glyph_width(&font_id, 'M');
        let h = f.row_height(&font_id);
        (w, h)
    });
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

    let hovered_link = hovered_link(ui, &response, session, rect, cell_w, cell_h, cols, rows);
    if hovered_link.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    // Apps that negotiate mouse tracking want the raw button/motion stream, not
    // local selection — matching alacritty, Shift is the escape hatch that still
    // selects text while the app is in mouse mode.
    let mouse_mode = session.term.lock().mode().intersects(TermMode::MOUSE_MODE);
    let report_mouse = mouse_mode && !ui.input(|i| i.modifiers.shift);
    if report_mouse {
        handle_mouse_reporting(ui, session, rect, cell_w, cell_h, cols, rows);
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
            hovered_link.as_ref(),
        );
    }
    handle_wheel_scroll(ui, &response, session, config, rect, cell_w, cell_h, cols, rows);
    dispatch_input(ui, &response, session, ime, allow_focus);
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
        &session.term.lock(),
        config,
        hovered_link.as_ref().map(|l| &l.bounds),
        // The preedit overlay replaces the cursor while composing
        // (alacritty hides it the same way, display/content.rs).
        ime.preedit().is_some(),
    );
    paint_grid(
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
    );

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
            .or_else(|| cursor_cell_rect(session, rect, cell_w, cell_h))
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
) {
    if allow_focus && response.has_focus() {
        // Kitty-protocol and mouse modes negotiated by the running app decide
        // how events encode, so the encoder needs the live terminal mode.
        let mode = *session.term.lock().mode();
        let consumed: Vec<ConsumedEvent> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    Event::Ime(ev) => Some(ConsumedEvent::Ime(ev.clone())),
                    // `Event::Paste` is dropped (see `consumed_event`): keyboard
                    // paste runs through the binding table's `Paste` action, not
                    // the synthetic event egui-winit raises on every command+V.
                    _ => consumed_event(e, mode),
                })
                .collect()
        });
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

/// Resolve the link under the mouse pointer, if any.  Returns `None` when the
/// pointer is outside the grid, when no link covers that cell, or when the
/// pointer is being used for an active drag (so click-to-open never fights
/// with text selection).
fn hovered_link(
    ui: &Ui,
    response: &Response,
    session: &Session,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
) -> Option<Link> {
    if response.dragged() {
        return None;
    }
    let pos = ui.input(|i| i.pointer.hover_pos())?;
    if !pointer_owns_grid(ui.ctx(), ui.layer_id(), rect, pos) {
        return None;
    }
    let term = session.term.lock();
    let display_offset = term.grid().display_offset() as i32;
    let (point, _) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
    links::link_at(&term, point)
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
            links::open(&link.uri);
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
) {
    let mode = *session.term.lock().mode();
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

    let display_offset = session.term.lock().grid().display_offset() as i32;
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
    let pointer_cell = ui.input(|i| i.pointer.hover_pos()).map(|pos| {
        let display_offset = session.term.lock().grid().display_offset() as i32;
        cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset).0
    });
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
        apply_scroll(session, config, dx_pt, dy_pt, cell_w_pt, cell_h_pt, modifiers, pointer_cell);
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
) {
    let mode = *session.term.lock().mode();
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

fn cell_at_pos(
    pos: Pos2,
    rect: Rect,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    display_offset: i32,
) -> (Point, Side) {
    let local_x = pos.x - rect.min.x;
    let local_y = pos.y - rect.min.y;
    let col_f = local_x / cell_w;
    let row_f = local_y / cell_h;
    let col = (col_f.floor() as i32).clamp(0, cols as i32 - 1) as usize;
    let row = (row_f.floor() as i32).clamp(0, rows as i32 - 1) as usize;
    let frac = col_f - col_f.floor();
    let side = if frac < 0.5 { Side::Left } else { Side::Right };
    (Point::new(Line(row as i32 - display_offset), Column(col)), side)
}

enum ConsumedEvent {
    Bytes(Vec<u8>),
    Ime(ImeEvent),
}

/// Classify an input event for the focused terminal.
///
/// `Event::Paste` is dropped rather than pasted: egui-winit synthesizes it for
/// every `command+V` press, Shift included, so acting on it would paste on
/// Ctrl+V regardless of the binding table and leave the shortcut impossible to
/// rebind or unbind.  Keyboard paste runs through `NamedAction::Paste`, which
/// reads the clipboard itself.  Text widgets outside the terminal still consume
/// the event normally.  `Event::Ime` is handled separately by the caller.
fn consumed_event(event: &Event, mode: TermMode) -> Option<ConsumedEvent> {
    match event {
        Event::Paste(_) => None,
        _ => event_to_bytes(event, mode).map(ConsumedEvent::Bytes),
    }
}

/// Viewport rect of the terminal cursor's cell; `None` while the cursor is
/// scrolled out of view.
fn cursor_cell_rect(session: &Session, rect: Rect, cell_w: f32, cell_h: f32) -> Option<Rect> {
    let term = session.term.lock();
    let grid = term.grid();
    let cursor = grid.cursor.point;
    let line = cursor.line.0 + grid.display_offset() as i32;
    if line < 0 || line >= grid.screen_lines() as i32 {
        return None;
    }
    Some(Rect::from_min_size(
        Pos2::new(rect.min.x + cursor.column.0 as f32 * cell_w, rect.min.y + line as f32 * cell_h),
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
        let mut flags = cell.flags;
        if underline_link {
            flags.insert(Flags::UNDERLINE);
        }
        Self { fg: cell.fg, bg: cell.bg, flags }
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
    text: String,
    runs: Vec<Run>,
    cursor: Option<CursorSnapshot>,
}

/// A span of cells sharing one resolved style, indexing into
/// `GridSnapshot::text`.
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

    fn capture(
        &mut self,
        term: &Term<EventProxy>,
        config: &Config,
        link_bounds: Option<&Match>,
        cursor_hidden: bool,
    ) {
        self.text.clear();
        self.runs.clear();
        self.cursor = None;

        let runtime_palette = term.colors();
        let grid = term.grid();
        let display_offset = grid.display_offset() as i32;
        let screen_lines = grid.screen_lines() as i32;
        let cols = grid.columns();

        // Resolve the active selection once per frame; the per-cell range checks
        // are cheap and avoid re-deriving it for every run.
        let selection_range = term.selection.as_ref().and_then(|s| s.to_range(term));
        let in_link = |line: Line, column: Column| {
            link_bounds.is_some_and(|b| b.contains(&Point::new(line, column)))
        };

        for row in 0..screen_lines {
            let line = Line(row - display_offset);
            let cells = &grid[line];

            let mut col = 0;
            while col < cols {
                let start = col;
                let style = Style::from_cell(&cells[Column(col)], in_link(line, Column(col)));
                let selected = is_selected(selection_range.as_ref(), line, Column(col));
                let text_start = self.text.len();
                while col < cols {
                    let cell = &cells[Column(col)];
                    if Style::from_cell(cell, in_link(line, Column(col))) != style
                        || is_selected(selection_range.as_ref(), line, Column(col)) != selected
                    {
                        break;
                    }
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        col += 1;
                        continue;
                    }
                    let ch = if cell.c == '\0' || cell.flags.contains(Flags::HIDDEN) {
                        ' '
                    } else {
                        cell.c
                    };
                    self.text.push(ch);
                    col += 1;
                }
                if self.text.len() == text_start {
                    continue;
                }
                let (fg, bg) = run_colors(style, selected, runtime_palette, config);
                self.runs.push(Run {
                    text: text_start..self.text.len(),
                    start_col: start,
                    row,
                    flags: style.flags,
                    fg,
                    bg,
                    selected,
                });
            }
        }

        let cursor_point: Point = grid.cursor.point;
        let cursor_row = cursor_point.line.0 + display_offset;
        let shape = cursor_shape(term);
        if cursor_hidden
            || matches!(shape, CursorShape::Hidden)
            || cursor_row < 0
            || cursor_row >= screen_lines
        {
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
    let bg_color = background(&config.palette);
    for run in &snapshot.runs {
        paint_run(
            painter,
            rect,
            &snapshot.text[run.text.clone()],
            run.start_col,
            rect.min.y + run.row as f32 * cell_h,
            cell_w,
            cell_h,
            run,
            config,
            font_id,
            bg_color,
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

#[allow(clippy::too_many_arguments)]
fn paint_run(
    painter: &egui::Painter,
    rect: Rect,
    run: &str,
    start_col: usize,
    y: f32,
    cell_w: f32,
    cell_h: f32,
    style: &Run,
    config: &Config,
    font_id: &FontId,
    default_bg: Color32,
    ppp: f32,
    metrics: &Metrics,
    builtin_glyphs: &mut BuiltinGlyphCache,
    color_glyphs: &mut ColorGlyphCache,
    glyphs: &mut GlyphCache,
    ctx: &egui::Context,
) {
    let (fg, bg) = (style.fg, style.bg);
    let width = run.chars().count() as f32 * cell_w;
    let x = rect.min.x + start_col as f32 * cell_w;
    let bg_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(width, cell_h));

    if bg != default_bg || style.selected {
        painter.rect_filled(bg_rect, 0.0, bg);
    }

    if !style.flags.contains(Flags::HIDDEN) {
        // Per-glyph paint: egui's run layout drifts off the cursor's `col * cell_w` grid (worse with zoom).
        let face =
            Face::new(style.flags.contains(Flags::BOLD), style.flags.contains(Flags::ITALIC));
        let glyph_dx = config.font.glyph_offset.x as f32;
        let glyph_dy = config.font.glyph_offset.y as f32;
        for (i, ch) in run.chars().enumerate() {
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
            painter.add(
                egui::epaint::TextShape::new(
                    Pos2::new(cell_x + glyph_dx, y + glyph_dy),
                    galley,
                    fg,
                )
                .with_override_text_color(fg),
            );
        }
    }

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
    }

    impl Caches {
        fn new() -> Self {
            Self {
                builtin: BuiltinGlyphCache::new(),
                colors: ColorGlyphCache::new(Vec::new(), 0),
                glyphs: GlyphCache::new(),
                ime: crate::ime::Ime::default(),
                snapshot: GridSnapshot::new(),
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
                    false,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
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
                    true,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
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
                    true,
                    &mut caches.builtin,
                    &mut caches.ime,
                    &mut caches.colors,
                    &mut caches.glyphs,
                    &mut caches.snapshot,
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
                    while !stop.load(Ordering::Relaxed) {
                        let started = std::time::Instant::now();
                        let guard = term.lock();
                        let waited = started.elapsed();
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

    /// Not a gate — run it by hand:
    /// `cargo test -p alacritree --release -- --ignored --nocapture paint_cost`
    ///
    /// Every PTY wakeup requests a repaint, and a repaint runs this whole path
    /// for the visible session.  What it costs is what a keystroke queues
    /// behind while a session is streaming output.
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_paint_cost() {
        #[cfg(windows)]
        crate::harden_dll_search_path();

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

        crate::harden_dll_search_path();
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
        apply_scroll(&mut session, &config, 0.0, -16.0, 8.0, 16.0, Modifiers::default(), None);

        wait_for_top_line(&session, "line 4 ").unwrap_or_else(|top| {
            panic!("the wheel tick did not scroll the pager; top line: {top:?}")
        });
    }

    /// egui-winit raises `Event::Paste` for every `command+V` press, Shift
    /// included, so honoring it here would paste on Ctrl+V no matter what the
    /// binding table says — and leave the shortcut impossible to rebind.
    #[test]
    fn paste_event_does_not_reach_the_terminal() {
        assert!(consumed_event(&Event::Paste("hi".into()), TermMode::empty()).is_none());
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
            matches!(consumed_event(&press, TermMode::empty()), Some(ConsumedEvent::Bytes(ref b)) if b == &vec![0x16]),
            "Ctrl+V must reach the PTY as 0x16"
        );
    }
}
