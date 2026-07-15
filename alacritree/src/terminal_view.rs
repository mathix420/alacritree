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
use crate::input::event_to_bytes;
use crate::links::{self, Link};
use crate::mouse;
use crate::paste;
use crate::session::{EventProxy, Session, SessionKind, TermSize};

pub fn show(
    ui: &mut Ui,
    session: &mut Session,
    config: &Config,
    allow_focus: bool,
    builtin_glyphs: &mut BuiltinGlyphCache,
    ime: &mut crate::ime::Ime,
    color_glyphs: &mut ColorGlyphCache,
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
    paint_grid(
        &painter,
        rect,
        session,
        config,
        &font_id,
        cell_w,
        cell_h,
        ppp,
        &metrics,
        builtin_glyphs,
        color_glyphs,
        ui.ctx(),
        hovered_link.as_ref().map(|l| &l.bounds),
        // The preedit overlay replaces the cursor while composing
        // (alacritty hides it the same way, display/content.rs).
        ime.preedit().is_some(),
    );

    let preedit_caret = ime
        .preedit()
        .map(|p| p.to_owned())
        .and_then(|p| paint_preedit(&painter, rect, session, config, &font_id, cell_w, cell_h, &p));

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
                    session.write(bytes);
                },
            }
        }
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

    response
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
    if !rect.contains(pos) {
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
/// tracking.  Events outside the grid are ignored so sidebar clicks don't leak.
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
                if !rect.contains(pos) {
                    continue;
                }
                let (point, _) = cell_at_pos(pos, rect, cell_w, cell_h, cols, rows, display_offset);
                session.last_report_cell = Some(point);
                if let Some(report) = mouse::mouse_report(mode, point, code, pressed, modifiers) {
                    bytes.extend_from_slice(&report);
                }
            },
            Raw::Motion { pos, modifiers } => {
                if !rect.contains(pos) {
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

#[allow(clippy::too_many_arguments)]
fn paint_grid(
    painter: &egui::Painter,
    rect: Rect,
    session: &Session,
    config: &Config,
    font_id: &FontId,
    cell_w: f32,
    cell_h: f32,
    ppp: f32,
    metrics: &Metrics,
    builtin_glyphs: &mut BuiltinGlyphCache,
    color_glyphs: &mut ColorGlyphCache,
    ctx: &egui::Context,
    link_bounds: Option<&Match>,
    cursor_hidden: bool,
) {
    let term = session.term.lock();
    let runtime_palette = term.colors();
    let grid = term.grid();
    let display_offset = grid.display_offset() as i32;
    let screen_lines = grid.screen_lines() as i32;
    let cols = grid.columns();

    let cursor_point: Point = grid.cursor.point;
    let cursor_visible_line = cursor_point.line.0 + display_offset;
    let bg_color = background(&config.palette);

    // Resolve the active selection once per frame; the per-cell range checks
    // are cheap and avoid relocking inside paint_run.
    let selection_range = term.selection.as_ref().and_then(|s| s.to_range(&term));
    let in_link = |line: Line, column: Column| {
        link_bounds.is_some_and(|b| b.contains(&Point::new(line, column)))
    };

    for row_idx in 0..screen_lines {
        let line = Line(row_idx - display_offset);
        let row = &grid[line];
        let y = rect.min.y + row_idx as f32 * cell_h;

        let mut col = 0;
        while col < cols {
            let start = col;
            let style = Style::from_cell(&row[Column(col)], in_link(line, Column(col)));
            let selected = is_selected(selection_range.as_ref(), line, Column(col));
            let mut run = String::new();
            while col < cols {
                let cell = &row[Column(col)];
                if Style::from_cell(cell, in_link(line, Column(col))) != style
                    || is_selected(selection_range.as_ref(), line, Column(col)) != selected
                {
                    break;
                }
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    col += 1;
                    continue;
                }
                let ch =
                    if cell.c == '\0' || cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
                run.push(ch);
                col += 1;
            }
            paint_run(
                painter,
                rect,
                &run,
                start,
                y,
                cell_w,
                cell_h,
                style,
                runtime_palette,
                config,
                font_id,
                bg_color,
                selected,
                ppp,
                metrics,
                builtin_glyphs,
                color_glyphs,
                ctx,
            );
        }
    }

    if !cursor_hidden && cursor_visible_line >= 0 && cursor_visible_line < screen_lines {
        let cursor_shape = cursor_shape(&term);
        paint_cursor(
            painter,
            rect,
            &term,
            config,
            cursor_point,
            cursor_visible_line,
            cell_w,
            cell_h,
            font_id,
            cursor_shape,
        );
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
    style: Style,
    runtime: &alacritty_terminal::term::color::Colors,
    config: &Config,
    font_id: &FontId,
    default_bg: Color32,
    selected: bool,
    ppp: f32,
    metrics: &Metrics,
    builtin_glyphs: &mut BuiltinGlyphCache,
    color_glyphs: &mut ColorGlyphCache,
    ctx: &egui::Context,
) {
    if run.is_empty() {
        return;
    }
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
    // When `colors.selection.background` is set we honor it; otherwise we swap
    // fg/bg of the underlying cell so the highlight is always visible without
    // requiring a config entry.
    let (fg, bg) = if selected {
        let sel_bg = config
            .palette
            .selection_bg
            .map(rgb_to_color32)
            .unwrap_or_else(|| rgb_to_color32(cell_fg));
        let sel_fg = config.palette.selection_fg.map(rgb_to_color32).unwrap_or_else(|| {
            if config.palette.selection_bg.is_some() {
                rgb_to_color32(cell_fg)
            } else {
                rgb_to_color32(cell_bg)
            }
        });
        (sel_fg, sel_bg)
    } else {
        (rgb_to_color32(cell_fg), rgb_to_color32(cell_bg))
    };

    let width = run.chars().count() as f32 * cell_w;
    let x = rect.min.x + start_col as f32 * cell_w;
    let bg_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(width, cell_h));

    if bg != default_bg || selected {
        painter.rect_filled(bg_rect, 0.0, bg);
    }

    if !style.flags.contains(Flags::HIDDEN) {
        // Per-glyph paint: egui's run layout drifts off the cursor's `col * cell_w` grid (worse with zoom).
        let glyph_font = font_for_flags(style.flags, font_id);
        let glyph_dx = config.font.glyph_offset.x as f32;
        let glyph_dy = config.font.glyph_offset.y as f32;
        let mut buf = [0u8; 4];
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
            painter.text(
                Pos2::new(cell_x + glyph_dx, y + glyph_dy),
                egui::Align2::LEFT_TOP,
                ch.encode_utf8(&mut buf).to_string(),
                glyph_font.clone(),
                fg,
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

#[allow(clippy::too_many_arguments)]
fn paint_cursor(
    painter: &egui::Painter,
    rect: Rect,
    term: &Term<EventProxy>,
    config: &Config,
    cursor_point: Point,
    cursor_visible_line: i32,
    cell_w: f32,
    cell_h: f32,
    font_id: &FontId,
    shape: alacritty_terminal::vte::ansi::CursorShape,
) {
    use alacritty_terminal::vte::ansi::CursorShape::*;
    if matches!(shape, Hidden) {
        return;
    }

    let runtime_palette = term.colors();
    let grid = term.grid();
    let cell = &grid[Line(cursor_point.line.0)][cursor_point.column];

    let x = rect.min.x + cursor_point.column.0 as f32 * cell_w;
    let y = rect.min.y + cursor_visible_line as f32 * cell_h;
    let cursor_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));

    let cursor_color = runtime_palette[alacritty_terminal::vte::ansi::NamedColor::Cursor]
        .map(rgb_to_color32)
        .or_else(|| config.palette.cursor_bg.map(rgb_to_color32))
        .unwrap_or_else(|| foreground(&config.palette));

    match shape {
        Block => {
            painter.rect_filled(cursor_rect, 0.0, cursor_color);
        },
        HollowBlock => {
            painter.rect_stroke(
                cursor_rect,
                0.0,
                Stroke::new(1.0_f32, cursor_color),
                egui::StrokeKind::Inside,
            );
        },
        Beam => {
            let bar = Rect::from_min_size(Pos2::new(x, y), Vec2::new(2.0, cell_h));
            painter.rect_filled(bar, 0.0, cursor_color);
        },
        Underline => {
            let bar = Rect::from_min_size(Pos2::new(x, y + cell_h - 2.0), Vec2::new(cell_w, 2.0));
            painter.rect_filled(bar, 0.0, cursor_color);
        },
        Hidden => return,
    }

    // The solid block covers the glyph; redraw it in inverted color so it stays legible.
    if matches!(shape, Block) && cell.c != '\0' && !cell.flags.contains(Flags::HIDDEN) {
        let glyph_color = config.palette.cursor_fg.map(rgb_to_color32).unwrap_or_else(|| {
            rgb_to_color32(resolve(cell.bg, cell.flags, runtime_palette, &config.palette, false))
        });
        let glyph_color =
            if glyph_color == cursor_color { background(&config.palette) } else { glyph_color };
        painter.text(
            Pos2::new(x, y),
            egui::Align2::LEFT_TOP,
            cell.c.to_string(),
            font_for_flags(cell.flags, font_id),
            glyph_color,
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
