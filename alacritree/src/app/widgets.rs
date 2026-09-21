//! Row widgets shared by the sidebars and the palette: labels and tooltips,
//! icon buttons and their resolver, the status slot, and the row layout they
//! sit in.

use super::*;

/// Bold and italic are real faces rather than a colour swap, but only the
/// terminal font registers them — an emphasized span at a proportional site
/// keeps the weight and shifts family rather than losing the weight.
fn emphasis_family(e: &TextEmphasis<Color32>, base: &egui::FontFamily) -> egui::FontFamily {
    match (e.bold, e.italic) {
        (true, true) => egui::FontFamily::Name(crate::fonts::BOLD_ITALIC_FAMILY.into()),
        (true, false) => egui::FontFamily::Name(crate::fonts::BOLD_FAMILY.into()),
        (false, true) => egui::FontFamily::Name(crate::fonts::ITALIC_FAMILY.into()),
        (false, false) => base.clone(),
    }
}

/// Add a truncating label, reporting its response and the galley it painted —
/// `elided` says whether the row had to ellipsize, `text()` spells the name out
/// in full however the label abbreviated it.
///
/// `egui::Label` offers an elided name as a tooltip by itself, but only to a
/// widget the hit test marks hovered — and a row that senses its click
/// retroactively, once its labels are already laid out, takes that mark away
/// from them.  Laying the galley out here keeps both decisions with the row:
/// which response carries the tooltip, and whether `[ui] sidebar_tooltips`
/// wants one at all.
///
/// `fallback_color` paints whatever spans the text left uncolored.  Selection
/// stays off whatever the surrounding style says: a selectable label unions
/// drag into `sense` and takes the click its row is waiting for.
pub(super) fn truncating_label(
    ui: &mut egui::Ui,
    text: impl Into<egui::WidgetText>,
    fallback_color: Color32,
    sense: egui::Sense,
) -> (egui::Response, Arc<egui::Galley>) {
    let (pos, galley, response) =
        egui::Label::new(text).truncate().selectable(false).sense(sense).layout_in_ui(ui);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), galley.text())
    });
    ui.painter().galley(pos, galley.clone(), fallback_color);
    (response, galley)
}

/// The hints for icons a row paints inside itself, with the rect each covers.
///
/// A row allocates its frame at end-of-show, so its retroactive `interact`
/// registers after the icons in egui's z-order and takes the hover mark away
/// from them — an icon's own `on_hover_text` never opens. The row answers for
/// whichever icon the pointer is over instead, the same way it already routes
/// a click that landed on a button.
#[derive(Default)]
pub(super) struct IconHints(Vec<(egui::Rect, String)>);

impl IconHints {
    pub(super) fn add(&mut self, rect: egui::Rect, hint: impl Into<String>) {
        self.0.push((rect, hint.into()));
    }

    fn at(&self, pos: egui::Pos2) -> Option<&str> {
        self.0.iter().find(|(rect, _)| rect.contains(pos)).map(|(_, hint)| hint.as_str())
    }

    /// The tooltip `resp` should carry: an icon's hint where the pointer is on
    /// one, otherwise `fallback` for the rest of the row.
    pub(super) fn apply(
        &self,
        resp: egui::Response,
        enabled: bool,
        fallback: impl FnOnce(egui::Response) -> egui::Response,
    ) -> egui::Response {
        match resp.hover_pos().and_then(|pos| self.at(pos)) {
            Some(hint) if enabled => resp.on_hover_text(hint.to_owned()),
            _ => fallback(resp),
        }
    }
}

/// Offer `hint` — what the icon under `resp` does or reports — as its tooltip,
/// unless `[ui] icon_tooltips` turns the hints off.
pub(super) fn icon_tooltip(resp: egui::Response, hint: &str, enabled: bool) -> egui::Response {
    if enabled { resp.on_hover_text(hint) } else { resp }
}

/// Offer `name` as `resp`'s tooltip, as far as the configured mode allows.
pub(super) fn name_tooltip(
    resp: egui::Response,
    name: &str,
    elided: bool,
    mode: SidebarTooltips,
) -> egui::Response {
    match mode {
        SidebarTooltips::Off => resp,
        SidebarTooltips::Elided if !elided => resp,
        _ => resp.on_hover_text(name),
    }
}

/// Lay a path out as the text of one truncating label.
///
/// `Zed` needs two differently-formatted spans, and one `LayoutJob` is the
/// only way to get them without an `item_spacing` gap between two labels, a
/// second response competing for the row's click, and a filename that can
/// overflow the width `row_with_trailing` is managing.  Putting the filename
/// first only *prioritizes* it: epaint truncates the tail of one linear glyph
/// stream, so a row narrower than the filename still elides it.
pub(super) fn path_text(
    ui: &egui::Ui,
    path: &str,
    base: Color32,
    theme: &Theme,
    style: PathStyle,
    family: egui::FontFamily,
    home: Option<&str>,
) -> egui::WidgetText {
    if style != PathStyle::Zed {
        return RichText::new(path_style::render(path, style, home))
            .color(base)
            .family(family)
            .small()
            .into();
    }

    let size = egui::TextStyle::Small.resolve(ui.style()).size;
    // A hand-built job does not inherit the ui's text valign the way RichText
    // does, so it must be carried across or the path sits off-centre against
    // the change glyph beside it.
    let valign = ui.text_valign();
    let parts = path_style::split(path, style, home);
    let mut job = egui::text::LayoutJob::default();
    let mut push = |text: String, e: &TextEmphasis<Color32>| {
        if text.is_empty() {
            return;
        }
        job.append(&text, 0.0, egui::TextFormat {
            font_id: egui::FontId::new(size, emphasis_family(e, &family)),
            color: e.color.unwrap_or(base),
            valign,
            ..Default::default()
        });
    };
    let emphases = [&theme.path_style.filename, &theme.path_style.parent];
    for (text, e) in zed_spans(&parts).into_iter().zip(emphases) {
        push(text, e);
    }
    job.into()
}

pub(super) fn row_status_icon_size(theme: &Theme) -> egui::Vec2 {
    egui::vec2(ROW_STATUS_ICON_W, ROW_STATUS_ICON_H) * theme.ui_scale
}

const LOADER_FRAME: Duration = Duration::from_millis(120);

/// The pinged mark in a slot of its own, for a row whose only status is that
/// something under it rang.
pub(super) fn attention_mark(
    ui: &mut egui::Ui,
    icons: &Icons<Color32>,
    theme: &Theme,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
    paint_status_mark(ui, ShownState::Pinged, icons, rect, theme);
    resp
}

/// Match Codex's own six-dot Braille cycle so working sessions keep the same
/// visual signal in the terminal and sidebar.
fn paint_braille_loader(ui: &mut egui::Ui, rect: egui::Rect, size: f32, color: Color32) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.ctx().request_repaint_after(LOADER_FRAME);

    let frame = ui.input(|i| (i.time / LOADER_FRAME.as_secs_f64()) as usize);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        loader_glyph(frame),
        egui::FontId::proportional(size),
        color,
    );
}

pub(super) fn braille_loader(ui: &mut egui::Ui, size: f32, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::ProgressIndicator));
    paint_braille_loader(ui, rect, size, color);
    response
}

/// The glyph a state draws when `[ui.icons]` leaves it unset, its colour, and
/// whether it paints bold at the larger size.  `None` is the braille loader.
///
/// Colour comes from the state, not from the row: an idle agent reads the
/// same on a selected row as on a quiet one, and a native session reads the
/// same as a multiplexer pane in the same state.
fn default_mark(
    state: ShownState,
    set: StatusIndicators,
    theme: &Theme,
) -> (Option<&'static str>, Color32, bool) {
    let dots = set == StatusIndicators::Dots;
    let colors = &theme.state_colors;
    match state {
        ShownState::Idle => (Some(DEFAULT_HOLLOW_MARK.as_str()), colors.idle, false),
        ShownState::Working => (None, theme.accent, false),
        ShownState::Pinged => (Some(DEFAULT_FILLED_MARK.as_str()), theme.attention, false),
        ShownState::Blocked if dots => (Some(DEFAULT_FILLED_MARK.as_str()), colors.blocked, false),
        ShownState::Blocked => (Some(DEFAULT_BLOCKED_SYMBOL.as_str()), colors.blocked, false),
        ShownState::Done if dots => (Some(DEFAULT_FILLED_MARK.as_str()), colors.done, false),
        ShownState::Done => (Some(DEFAULT_DONE_SYMBOL.as_str()), colors.done, false),
        ShownState::Unknown if dots => (Some(DEFAULT_HOLLOW_MARK.as_str()), colors.unknown, false),
        // ASCII, so every UI font draws it and the baked face need not.
        // Bold and larger, since a bare `?` at mark size reads as a speck.
        ShownState::Unknown => (Some("?"), colors.unknown, true),
    }
}

fn state_icon(state: ShownState, icons: &Icons<Color32>) -> &IconStyle<Color32> {
    match state {
        ShownState::Unknown => &icons.agent_unknown,
        ShownState::Idle => &icons.agent_idle,
        ShownState::Working => &icons.agent_working,
        ShownState::Pinged => &icons.attention,
        ShownState::Done => &icons.agent_done,
        ShownState::Blocked => &icons.agent_blocked,
    }
}

const MARK_PX: f32 = 10.0;
const EMPHASIZED_MARK_PX: f32 = 12.0;

/// A state's mark as it paints: the glyph (`None` for the loader), its font
/// and its colour.  A glyph set in `[ui.icons]` wins over both indicator sets,
/// and on the working state it replaces the loader.
fn resolve_mark<'a>(
    state: ShownState,
    icons: &'a Icons<Color32>,
    theme: &Theme,
) -> (Option<&'a str>, egui::FontId, Color32) {
    let style = state_icon(state, icons);
    let (default_glyph, default_color, emphasized) =
        default_mark(state, theme.status_indicators, theme);
    let glyph = style.glyph.as_deref().map(str::trim).filter(|g| !g.is_empty()).or(default_glyph);
    let default_px = if emphasized { EMPHASIZED_MARK_PX } else { MARK_PX };
    let size = style.size.unwrap_or(default_px).min(ROW_STATUS_ICON_H) * theme.ui_scale;
    let family = crate::fonts::ui_variant_family(style.bold || emphasized, style.italic);
    (glyph, egui::FontId::new(size, family), style.color.unwrap_or(default_color))
}

/// Draw a state's mark into an already-allocated slot.
pub(super) fn paint_status_mark(
    ui: &mut egui::Ui,
    state: ShownState,
    icons: &Icons<Color32>,
    rect: egui::Rect,
    theme: &Theme,
) {
    let (glyph, font, color) = resolve_mark(state, icons, theme);
    match glyph {
        Some(glyph) => {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, font, color);
        },
        None => paint_braille_loader(ui, rect, font.size, color),
    }
}

/// Returns what the slot has to say on hover, for the row to register with the
/// rest of its icons. The row icon proper reports nothing the row does not
/// already spell out, so it stays silent.
pub(super) fn paint_row_status_icon(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &Icons<Color32>,
    status: RowStatus<'_>,
    style: &IconStyle<Color32>,
    default_glyph: BakedGlyph,
    is_active: bool,
) -> Option<(egui::Rect, String)> {
    match session_status_mark(&status) {
        Some((state, hint)) => {
            let (rect, _) =
                ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
            paint_status_mark(ui, state, icons, rect, theme);
            Some((rect, hint))
        },
        None => {
            // Centered into the fixed slot: laying a glyph out as text would
            // size the slot to its advance width and shift the label with it.
            let (rect, _) =
                ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
            let (glyph, font, resolved) =
                resolve_icon(style, default_glyph, theme.text_muted, 10.0, 10.0, theme);
            let color = if is_active { theme.accent } else { resolved };
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, font, color);
            None
        },
    }
}

/// Gap between adjacent action buttons. They already pad their own glyph, so
/// the default item spacing on top of that reads as a hole in the cluster.
/// Deliberately unscaled: the padding it supplements grows with `ui_scale`.
pub(super) const ICON_CLUSTER_SPACING: f32 = 2.0;

/// Resolve an icon's paint-time glyph, font, and color from its config and
/// the site's built-in defaults.  `default_glyph` covers the case where a
/// table styles a key without setting `glyph`.  `default_px` and `slot_px`
/// are deliberately separate: an action button paints its glyph at `12.0 *
/// ui_scale` inside a `16.0 * ui_scale` slot, and conflating the two would
/// resize every unconfigured icon.
///
/// One resolver, not one paint helper: `RichText` participates in layout
/// while painter text draws into preallocated geometry, so each site keeps
/// its own drawing call.
pub(super) fn resolve_icon<'a>(
    style: &'a IconStyle<Color32>,
    default_glyph: BakedGlyph,
    default_color: Color32,
    default_px: f32,
    slot_px: f32,
    theme: &Theme,
) -> (&'a str, egui::FontId, Color32) {
    let size = style.size.unwrap_or(default_px).min(slot_px) * theme.ui_scale;
    let family = crate::fonts::ui_variant_family(style.bold, style.italic);
    (
        style.or_glyph(default_glyph.as_str()),
        egui::FontId::new(size, family),
        style.color.unwrap_or(default_color),
    )
}

/// A configurable icon in a 16×16 slot: the glyph, weight, slant, size and
/// colour come from config, with the built-in glyph as the fallback.
pub(super) fn styled_icon_button(
    ui: &mut egui::Ui,
    style: &IconStyle<Color32>,
    default_glyph: BakedGlyph,
    color: Color32,
    theme: &Theme,
) -> egui::Response {
    let s = theme.ui_scale;
    let (glyph, font, color) = resolve_icon(style, default_glyph, color, 12.0, 16.0, theme);
    let size = egui::vec2(16.0 * s, 16.0 * s);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let painted = if resp.hovered() {
        Color32::from_rgb(
            color.r().saturating_add(40),
            color.g().saturating_add(40),
            color.b().saturating_add(40),
        )
    } else {
        color
    };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, glyph, font, painted);
    resp
}

/// Lay out a row whose `trailing` widgets pin to the right edge while `leading`
/// fills the remaining width — so a `Label::truncate()` inside `leading` knows
/// exactly how much space it has and ellipsizes cleanly when the panel is narrow.
///
/// The row is pre-sized to `interact_size.y` (mirroring `Ui::horizontal`'s own
/// internals) so it doesn't claim the parent's full remaining height when nested
/// in a vertical layout — without this, `Align::Center` would push the row's
/// content to the middle of the column and leave a giant gap before the next row.
pub(super) fn row_with_trailing<L, T>(ui: &mut egui::Ui, leading: L, trailing: T) -> egui::Rect
where
    L: FnOnce(&mut egui::Ui),
    T: FnOnce(&mut egui::Ui),
{
    let row_size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(row_size, egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let outer_spacing = ui.spacing().item_spacing.x;
        ui.spacing_mut().item_spacing.x = ICON_CLUSTER_SPACING;
        trailing(ui);
        // Restore before the leading group so only the icons cluster; the
        // labels next to them keep the panel's normal spacing.
        ui.spacing_mut().item_spacing.x = outer_spacing;
        let remaining = ui.available_width();
        if remaining <= 0.0 {
            return;
        }
        let row_h = ui.available_height();
        ui.allocate_ui_with_layout(
            egui::vec2(remaining, row_h),
            egui::Layout::left_to_right(egui::Align::Center),
            leading,
        );
    })
    .response
    .rect
}

/// Apply the configured sidebar scrollbar style to a panel's `Ui`.
///
/// `Solid` reserves a gutter right of the content instead of egui's floating
/// overlay, whose hover expansion covers the icons at the right end of the
/// rows.  Scoped to the panel so terminal-side scroll areas keep the default.
pub(super) fn apply_scrollbar_style(ui: &mut egui::Ui, scrollbar: ScrollbarStyle) {
    if scrollbar == ScrollbarStyle::Solid {
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
    }
}

/// Keyboard-cursor indicator: an outline rather than a fill so it stays
/// legible on top of the active row's lightened background.
pub(super) fn paint_cursor_outline(ui: &egui::Ui, rect: egui::Rect, theme: &Theme) {
    ui.painter().rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.0_f32, theme.accent),
        egui::StrokeKind::Inside,
    );
}

/// Footprint every leading row marker claims, whichever glyph it ends up
/// drawing. Markers vary wildly in intrinsic width (`·` vs `✳`), so sizing the
/// slot to the glyph would start each row's label at a different x.
pub(super) const ROW_STATUS_ICON_W: f32 = 10.0;
const ROW_STATUS_ICON_H: f32 = 14.0;

pub(super) const ATTENTION_HINT: &str = "needs attention";
const CODEX_LOADER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The Zed style's span decomposition: the filename text, then — when there
/// is a parent to show — the parent text with its separating space already
/// folded in. Position carries the emphasis: span 0 always paints with the
/// filename emphasis, span 1 (if present) with the parent emphasis. Shared by
/// `path_label`'s job builder and its fidelity test so a regression in the
/// split logic fails the test that exercises the real render path.
fn zed_spans(parts: &path_style::Parts) -> Vec<String> {
    if parts.parent.is_empty() {
        vec![format!("{}{}", parts.root, parts.name)]
    } else {
        vec![parts.name.clone(), format!(" {}{}", parts.root, parts.parent)]
    }
}

fn loader_glyph(frame: usize) -> &'static str {
    CODEX_LOADER_FRAMES[frame % CODEX_LOADER_FRAMES.len()]
}

/// What the status slot says on hover.  A named agent is named, so a
/// workspace running several can be told apart without opening any of them.
pub(super) fn agent_hint(state: ShownState, name: Option<&str>) -> String {
    let name = name.unwrap_or("agent");
    match state {
        ShownState::Pinged => ATTENTION_HINT.to_owned(),
        ShownState::Unknown => format!("{name}, state unknown"),
        ShownState::Idle => format!("{name} is running"),
        ShownState::Working => format!("{name} is working"),
        ShownState::Done => format!("{name} is done"),
        ShownState::Blocked => format!("{name} is waiting for you"),
    }
}

/// What a row knows about its own state.  Grouped rather than passed loose
/// because the four answer one question between them, and the slot draws
/// whichever ranks highest.
#[derive(Clone, Copy)]
pub(super) struct RowStatus<'a> {
    /// The terminal rang while nobody was looking.
    pub(super) pinged: bool,
    /// The agent finished a turn while nobody was looking.
    pub(super) done: bool,
    /// The live reading, with a multiplexer's status already folded in.
    pub(super) activity: SessionActivity,
    pub(super) managed: Option<&'a Managed>,
}

impl RowStatus<'_> {
    /// A row with nothing to report beyond its live reading.
    pub(super) fn live(activity: SessionActivity) -> Self {
        Self { pinged: false, done: false, activity, managed: None }
    }
}

/// A session's status mark and the words it explains on hover, independent
/// of where it paints.  The sidebar's fixed slot and the palette's row both
/// ask here for the identical session, so the two can never disagree.
///
/// A multiplexer that reports `done` has latched it on its own side, so it
/// counts the same as alacritree's own latch.  The hover names the
/// multiplexer when the mark is the state it reported.  `None` covers a
/// plain shell with nothing latched.
pub(super) fn session_status_mark(status: &RowStatus<'_>) -> Option<(ShownState, String)> {
    let pane_status = status.managed.and_then(|managed| managed.status);
    let pane_done = pane_status == Some(PaneStatus::Done);
    let state = ShownState::of(status.activity.live(), status.done || pane_done, status.pinged)?;
    let hint = match (status.managed, pane_status) {
        (Some(managed), Some(reported)) if ShownState::from(reported) == state => {
            format!("{} says {}", managed.multiplexer, reported.label())
        },
        _ => agent_hint(state, status.activity.name()),
    };
    Some((state, hint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::{Pane, PaneStatus, Side};
    use crate::test_util::{listed_agent, managed};

    const EVERY_STATE: [ShownState; 6] = [
        ShownState::Unknown,
        ShownState::Idle,
        ShownState::Working,
        ShownState::Pinged,
        ShownState::Done,
        ShownState::Blocked,
    ];

    fn theme_with(set: StatusIndicators) -> Theme {
        let mut config = Config::default();
        config.ui.status_indicators = set;
        Theme::from_config(&config)
    }

    fn glyphs(set: StatusIndicators) -> Vec<(ShownState, Option<String>)> {
        let theme = theme_with(set);
        let icons = Icons::default().map_colors(rgb_to_color32);
        EVERY_STATE
            .iter()
            .map(|&state| (state, resolve_mark(state, &icons, &theme).0.map(str::to_owned)))
            .collect()
    }

    /// Symbols tells every state apart by shape alone, so no two share a
    /// glyph, and working keeps the loader.
    #[test]
    fn every_symbols_state_has_a_glyph_of_its_own() {
        let marks = glyphs(StatusIndicators::Symbols);
        for (i, (a, ga)) in marks.iter().enumerate() {
            for (b, gb) in &marks[i + 1..] {
                assert_ne!(ga, gb, "{a:?} and {b:?} draw the same mark");
            }
        }
        assert_eq!(marks[2], (ShownState::Working, None));
    }

    /// Dots draws every state but working as one of two same-sized circles,
    /// hollow or filled, and leaves the rest to colour.
    #[test]
    fn dots_draw_two_circles_and_tell_states_apart_by_colour() {
        let marks = glyphs(StatusIndicators::Dots);
        let glyph = |state| marks.iter().find(|(s, _)| *s == state).unwrap().1.clone();
        let hollow = Some(DEFAULT_HOLLOW_MARK.as_str().to_owned());
        let filled = Some(DEFAULT_FILLED_MARK.as_str().to_owned());
        assert_eq!(glyph(ShownState::Idle), hollow);
        assert_eq!(glyph(ShownState::Unknown), hollow);
        assert_eq!(glyph(ShownState::Pinged), filled);
        assert_eq!(glyph(ShownState::Blocked), filled);
        assert_eq!(glyph(ShownState::Done), filled);
        assert_eq!(glyph(ShownState::Working), None);

        let theme = theme_with(StatusIndicators::Dots);
        let icons = Icons::default().map_colors(rgb_to_color32);
        let colors: Vec<Color32> =
            EVERY_STATE.iter().map(|&state| resolve_mark(state, &icons, &theme).2).collect();
        for (i, a) in colors.iter().enumerate() {
            for (j, b) in colors.iter().enumerate().skip(i + 1) {
                assert_ne!(a, b, "{:?} and {:?} share a colour", EVERY_STATE[i], EVERY_STATE[j]);
            }
        }
    }

    /// Colour comes from the state, so a native session and a multiplexer
    /// pane in one state never disagree about what it looks like.
    #[test]
    fn a_mark_takes_its_colour_from_its_state() {
        let theme = theme_with(StatusIndicators::Dots);
        let icons = Icons::default().map_colors(rgb_to_color32);
        let color = |state| resolve_mark(state, &icons, &theme).2;
        assert_eq!(color(ShownState::Blocked), theme.state_colors.blocked);
        assert_eq!(color(ShownState::Done), theme.state_colors.done);
        assert_eq!(color(ShownState::Idle), theme.state_colors.idle);
        assert_eq!(color(ShownState::Unknown), theme.state_colors.unknown);
        assert_eq!(color(ShownState::Working), theme.accent);
        assert_eq!(color(ShownState::Pinged), theme.attention);
    }

    /// A glyph set in `[ui.icons]` wins in both sets, and on working it
    /// replaces the loader.
    #[test]
    fn a_configured_glyph_overrides_both_indicator_sets() {
        let mut icons = Icons::default().map_colors(rgb_to_color32);
        icons.agent_done.glyph = Some("D".into());
        icons.agent_working.glyph = Some("W".into());
        for set in [StatusIndicators::Dots, StatusIndicators::Symbols] {
            let theme = theme_with(set);
            assert_eq!(resolve_mark(ShownState::Done, &icons, &theme).0, Some("D"));
            assert_eq!(resolve_mark(ShownState::Working, &icons, &theme).0, Some("W"));
        }
    }

    /// Symbols' unknown is an ASCII `?`, which reads as a speck at mark size
    /// unless it paints bold and larger.
    #[test]
    fn the_unknown_symbol_paints_bold_and_larger() {
        let theme = theme_with(StatusIndicators::Symbols);
        let icons = Icons::default().map_colors(rgb_to_color32);
        let (glyph, font, _) = resolve_mark(ShownState::Unknown, &icons, &theme);
        assert_eq!(glyph, Some("?"));
        assert_eq!(font.family, crate::fonts::ui_variant_family(true, false));
        assert_eq!(font.size, EMPHASIZED_MARK_PX * theme.ui_scale);
    }

    /// Blocked and done outrank a ping; a ping outranks working and idle.
    /// The ping stays latched underneath either way.
    #[test]
    fn a_louder_state_hides_a_ping_and_a_quieter_one_does_not() {
        let pinged = |live| RowStatus {
            pinged: true,
            ..RowStatus::live(SessionActivity::agent(Some("claude"), live))
        };
        let mark = |status: RowStatus<'_>| session_status_mark(&status).unwrap().0;
        assert_eq!(mark(pinged(LiveState::Blocked)), ShownState::Blocked);
        assert_eq!(mark(pinged(LiveState::Working)), ShownState::Pinged);
        assert_eq!(mark(pinged(LiveState::Idle)), ShownState::Pinged);
        let done = RowStatus { done: true, ..pinged(LiveState::Idle) };
        assert_eq!(mark(done), ShownState::Done);
        assert_eq!(session_status_mark(&done).unwrap().1, "claude is done");
    }

    /// A pane-backed session's mark and hover come from the same call the
    /// sidebar makes for the identical `Managed`, so the two can never
    /// disagree about what a pane is doing.
    #[test]
    fn session_status_mark_matches_the_sidebar_for_a_pane_backed_session() {
        let agent = Pane { status: Some(PaneStatus::Working), ..listed_agent(Some("claude")) };
        let managed = managed(&agent, &Side::Native, false);
        let activity = SessionActivity::agent(Some("claude"), LiveState::Working);
        let status = RowStatus { managed: Some(&managed), ..RowStatus::live(activity) };
        let (mark, hint) =
            session_status_mark(&status).expect("a listed pane with an agent has a mark");
        assert_eq!(mark, ShownState::Working);
        assert_eq!(hint, format!("{} says working", managed.multiplexer));
    }

    /// A multiplexer that reports `done` has latched it itself, so the row
    /// shows done with no latch of alacritree's own.
    #[test]
    fn a_pane_reporting_done_shows_done() {
        let agent = Pane { status: Some(PaneStatus::Done), ..listed_agent(Some("claude")) };
        let managed = managed(&agent, &Side::Native, false);
        let activity = SessionActivity::agent(Some("claude"), LiveState::Idle);
        let status = RowStatus { managed: Some(&managed), ..RowStatus::live(activity) };
        let (mark, hint) = session_status_mark(&status).unwrap();
        assert_eq!(mark, ShownState::Done);
        assert_eq!(hint, format!("{} says done", managed.multiplexer));
    }

    #[test]
    fn loader_cycles_through_codex_braille_frames() {
        let frames: Vec<&str> = (0..CODEX_LOADER_FRAMES.len()).map(loader_glyph).collect();
        assert_eq!(frames, ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);
        assert_eq!(loader_glyph(CODEX_LOADER_FRAMES.len()), "⠋");
    }

    /// Every emphasis combination must resolve to a registered face; falling back
    /// to the base family for, say, bold-italic would silently drop the weight.
    /// An unemphasized span keeps whatever family the site already paints in.
    #[test]
    fn emphasis_resolves_to_the_registered_faces() {
        let plain = TextEmphasis::default();
        let bold = TextEmphasis { bold: true, ..Default::default() };
        let italic = TextEmphasis { italic: true, ..Default::default() };
        let both = TextEmphasis { bold: true, italic: true, ..Default::default() };

        for base in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
            assert_eq!(emphasis_family(&plain, &base), base);
            assert_eq!(
                emphasis_family(&bold, &base),
                egui::FontFamily::Name(crate::fonts::BOLD_FAMILY.into())
            );
            assert_eq!(
                emphasis_family(&italic, &base),
                egui::FontFamily::Name(crate::fonts::ITALIC_FAMILY.into())
            );
            assert_eq!(
                emphasis_family(&both, &base),
                egui::FontFamily::Name(crate::fonts::BOLD_ITALIC_FAMILY.into())
            );
        }
    }

    #[test]
    fn a_configured_size_is_clamped_to_the_slot() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { size: Some(400.0), ..Default::default() };
        let (_, font, _) =
            resolve_icon(&style, DEFAULT_WORKTREE_ICON, Color32::WHITE, 10.0, 10.0, &theme);
        assert!(
            font.size <= 10.0 * theme.ui_scale,
            "an oversized glyph must not overlap its neighbours"
        );

        let (_, font, _) =
            resolve_icon(&style, DEFAULT_CLOSE_ICON, Color32::WHITE, 12.0, 16.0, &theme);
        assert!(font.size <= 16.0 * theme.ui_scale, "a button glyph clamps to its own 16px slot");
    }

    /// With no config, every icon paints at its built-in size: buttons at 12
    /// inside a 16 slot, status markers at 10.
    #[test]
    fn an_unconfigured_icon_keeps_its_current_size() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle::default();
        let (_, font, _) =
            resolve_icon(&style, DEFAULT_CLOSE_ICON, Color32::WHITE, 12.0, 16.0, &theme);
        assert_eq!(font.size, 12.0 * theme.ui_scale);
    }

    #[test]
    fn a_configured_color_wins_over_the_site_default() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { color: Some(Color32::RED), ..Default::default() };
        let (_, _, color) =
            resolve_icon(&style, DEFAULT_WORKTREE_ICON, Color32::WHITE, 10.0, 10.0, &theme);
        assert_eq!(color, Color32::RED);
    }

    /// An unstyled icon must render `FontFamily::Proportional`, matching
    /// every icon call site with no `bold`/`italic` configured.
    #[test]
    fn an_unconfigured_icon_resolves_to_the_proportional_family() {
        let theme = Theme::from_config(&Config::default());
        let (_, font, _) = resolve_icon(
            &IconStyle::default(),
            DEFAULT_WORKTREE_ICON,
            Color32::WHITE,
            10.0,
            10.0,
            &theme,
        );
        assert_eq!(font.family, egui::FontFamily::Proportional);
    }

    /// `italic` alone (no `bold`) must resolve to the italic face, not the
    /// bold-italic one — the two flags are independent inputs to
    /// `ui_variant_family`.
    #[test]
    fn an_italic_icon_resolves_to_the_italic_family() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { italic: true, ..Default::default() };
        let (_, font, _) =
            resolve_icon(&style, DEFAULT_WORKTREE_ICON, Color32::WHITE, 10.0, 10.0, &theme);
        assert_eq!(font.family, egui::FontFamily::Name(crate::fonts::UI_ITALIC_FAMILY.into()));
    }

    /// An unconfigured action button paints its built-in glyph in the
    /// proportional family, at 12px inside its 16px slot, in the site's
    /// default colour.
    #[test]
    fn an_unconfigured_action_button_is_unchanged() {
        let theme = Theme::from_config(&Config::default());
        let icons = Icons::default().map_colors(rgb_to_color32);
        let (glyph, font, color) = resolve_icon(
            &icons.delete_worktree,
            DEFAULT_CLOSE_ICON,
            theme.text_muted,
            12.0,
            16.0,
            &theme,
        );
        assert_eq!(glyph, "×");
        assert_eq!(font.family, egui::FontFamily::Proportional);
        assert_eq!(font.size, 12.0 * theme.ui_scale);
        assert_eq!(color, theme.text_muted);
    }

    /// Styling one action button must not reach a sibling that shares its glyph.
    #[test]
    fn styling_the_destructive_button_leaves_its_siblings_alone() {
        let theme = Theme::from_config(&Config::default());
        let mut icons = Icons::default().map_colors(rgb_to_color32);
        icons.delete_worktree = IconStyle {
            glyph: Some("✖".into()),
            color: Some(Color32::RED),
            bold: true,
            ..Default::default()
        };

        let (glyph, font, color) = resolve_icon(
            &icons.delete_worktree,
            DEFAULT_CLOSE_ICON,
            theme.text_muted,
            12.0,
            16.0,
            &theme,
        );
        assert_eq!(glyph, "✖");
        assert_eq!(color, Color32::RED);
        assert_eq!(font.family, egui::FontFamily::Name(crate::fonts::UI_BOLD_FAMILY.into()));

        let (glyph, _, color) = resolve_icon(
            &icons.close_session,
            DEFAULT_CLOSE_ICON,
            theme.text_muted,
            12.0,
            16.0,
            &theme,
        );
        assert_eq!(glyph, "×");
        assert_eq!(color, theme.text_muted);
    }

    /// A table that styles a key without setting `glyph` (color/weight only)
    /// must still fall back to the site's `default_glyph` argument —
    /// `Icons::default()` never exercises this path, since its glyph is
    /// always set.
    #[test]
    fn a_glyphless_style_falls_back_to_the_site_default_glyph() {
        let theme = Theme::from_config(&Config::default());
        let style = IconStyle { color: Some(Color32::RED), bold: true, ..Default::default() };
        let (glyph, font, color) =
            resolve_icon(&style, DEFAULT_CLOSE_ICON, theme.text_muted, 12.0, 16.0, &theme);
        assert_eq!(glyph, DEFAULT_CLOSE_ICON.as_str());
        assert_eq!(color, Color32::RED);
        assert_eq!(font.family, egui::FontFamily::Name(crate::fonts::UI_BOLD_FAMILY.into()));
    }

    #[test]
    fn the_status_hint_names_the_agent_and_what_it_is_doing() {
        assert_eq!(agent_hint(ShownState::Idle, Some("claude")), "claude is running");
        assert_eq!(agent_hint(ShownState::Working, Some("codex")), "codex is working");
        assert_eq!(agent_hint(ShownState::Blocked, Some("claude")), "claude is waiting for you");
        assert_eq!(agent_hint(ShownState::Blocked, None), "agent is waiting for you");
        assert_eq!(agent_hint(ShownState::Done, Some("claude")), "claude is done");
        assert_eq!(agent_hint(ShownState::Unknown, None), "agent, state unknown");
        assert_eq!(agent_hint(ShownState::Pinged, Some("claude")), ATTENTION_HINT);
    }

    /// A shell session has no state axis to mark. The sidebar draws its own
    /// icon here instead of a status mark, and the palette leaves the slot
    /// empty, so both must read this as "no mark" rather than picking one.
    /// A finished turn belongs to an agent, so a shell cannot be done either;
    /// it can only be pinged.
    #[test]
    fn a_shell_has_no_mark_unless_it_rang() {
        let shell = RowStatus::live(SessionActivity::Shell);
        assert!(session_status_mark(&shell).is_none());
        assert!(session_status_mark(&RowStatus { done: true, ..shell }).is_none());
        let rang = RowStatus { pinged: true, ..shell };
        assert_eq!(
            session_status_mark(&rang).unwrap(),
            (ShownState::Pinged, ATTENTION_HINT.into())
        );
    }

    /// The palette paints the identical mark and hover the sidebar would for
    /// a local agent, whichever live state it is in.
    #[test]
    fn session_status_mark_picks_each_live_state_for_a_local_agent() {
        for live in [LiveState::Unknown, LiveState::Idle, LiveState::Working, LiveState::Blocked] {
            let activity = SessionActivity::agent(Some("claude"), live);
            let (mark, hint) = session_status_mark(&RowStatus::live(activity))
                .expect("an agent always has a mark");
            assert_eq!(mark, ShownState::from(live));
            assert_eq!(hint, agent_hint(mark, Some("claude")));
        }
    }

    /// The job's spans, as `path_label` itself builds them via `zed_spans`,
    /// must reassemble into exactly what `render` produces, so the emphasis
    /// only changes how the text looks, never what it says.
    #[test]
    fn the_zed_job_spells_the_same_text_as_render() {
        for (path, home) in [
            ("path/to/file.txt", None),
            ("/a/b/c.txt", None),
            ("f.txt", None),
            ("/f.txt", None),
            ("/home/lev/Git/x/y.rs", Some("/home/lev")),
        ] {
            let parts = crate::path_style::split(path, PathStyle::Zed, home);
            let spans = zed_spans(&parts).concat();
            assert_eq!(spans, crate::path_style::render(path, PathStyle::Zed, home), "{path:?}");
        }
    }
}
