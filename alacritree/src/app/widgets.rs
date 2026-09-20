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
    egui::vec2(ROW_STATUS_ICON_W, 14.0) * theme.ui_scale
}

const LOADER_FRAME: Duration = Duration::from_millis(120);

/// Draw the attention dot into an already-allocated slot.
pub(super) fn paint_attention_dot(ui: &egui::Ui, rect: egui::Rect, theme: &Theme) {
    let radius = 3.0 * theme.ui_scale;
    ui.painter().circle_filled(rect.center(), radius, theme.attention);
}

/// Painted (rather than `RichText("●")`) so its size is independent of font
/// metrics — `RichText("●")` renders inconsistently across fallback fonts.
pub(super) fn attention_dot(ui: &mut egui::Ui, theme: &Theme) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
    paint_attention_dot(ui, rect, theme);
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

/// What the status slot draws for an agent's live state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentMark {
    /// Live work animates.  A static glyph would have to blink to say as much.
    Loader,
    Glyph(BakedGlyph, Color32),
}

/// Colour comes from the state, not from the row: an idle agent reads the
/// same on a selected row as on a quiet one, and the same as a harness-backed
/// row reporting the same state.  Working animates because a static glyph
/// would have to blink to say as much.
pub(super) fn agent_mark(live: LiveState, theme: &Theme) -> AgentMark {
    match live {
        LiveState::Idle => {
            AgentMark::Glyph(DEFAULT_AGENT_ICON, theme.harness_state.of(StateTone::Idle))
        },
        LiveState::Working => AgentMark::Loader,
        LiveState::Blocked => {
            AgentMark::Glyph(DEFAULT_BLOCKED_ICON, theme.harness_state.of(StateTone::Blocked))
        },
    }
}

/// Draw a harness's own state mark into an already-allocated slot.  A harness
/// that has stopped reporting leaves the slot empty rather than inventing a
/// state for it.
pub(super) fn paint_harness_mark(
    ui: &mut egui::Ui,
    mark: Option<HarnessMark>,
    rect: egui::Rect,
    theme: &Theme,
) {
    let Some(mark) = mark else { return };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        mark.glyph,
        egui::FontId::new(10.0 * theme.ui_scale, crate::fonts::ui_variant_family(false, false)),
        theme.harness_state.of(mark.tone),
    );
}

/// Draw one agent mark into an already-allocated slot.
pub(super) fn paint_agent_mark(
    ui: &mut egui::Ui,
    mark: AgentMark,
    rect: egui::Rect,
    theme: &Theme,
) {
    let s = theme.ui_scale;
    match mark {
        AgentMark::Loader => {
            paint_braille_loader(ui, rect, 10.0 * s, theme.accent);
        },
        AgentMark::Glyph(glyph, color) => {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                glyph.as_str(),
                egui::FontId::proportional(10.0 * s),
                color,
            );
        },
    }
}

/// Returns what the slot has to say on hover, for the row to register with the
/// rest of its icons. The row icon proper reports nothing the row does not
/// already spell out, so it stays silent.
pub(super) fn paint_row_status_icon(
    ui: &mut egui::Ui,
    theme: &Theme,
    status: RowStatus<'_>,
    style: &IconStyle<Color32>,
    default_glyph: BakedGlyph,
    is_active: bool,
) -> Option<(egui::Rect, String)> {
    match session_status_mark(&status) {
        Some((SessionMark::Attention, hint)) => Some((attention_dot(ui, theme).rect, hint)),
        Some((SessionMark::Harness(mark), hint)) => {
            let (rect, _) =
                ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
            paint_harness_mark(ui, Some(mark), rect, theme);
            Some((rect, hint))
        },
        Some((SessionMark::Agent(live), hint)) => {
            let (rect, _) =
                ui.allocate_exact_size(row_status_icon_size(theme), egui::Sense::hover());
            paint_agent_mark(ui, agent_mark(live, theme), rect, theme);
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
pub(super) fn agent_hint(live: LiveState, name: Option<&str>) -> String {
    let doing = match live {
        LiveState::Idle => "is running",
        LiveState::Working => "is working",
        LiveState::Blocked => "is waiting for you",
    };
    format!("{} {doing}", name.unwrap_or("agent"))
}

/// What a row knows about its own state, in the order the status slot ranks
/// it.  Grouped rather than passed loose because the three answer one
/// question between them, and the slot draws whichever ranks highest.
pub(super) struct RowStatus<'a> {
    pub(super) attention: bool,
    pub(super) activity: SessionActivity,
    pub(super) managed: Option<&'a Managed>,
}

/// A session's status mark, independent of where it paints — the sidebar's
/// fixed slot and the palette's row both ask `session_status_mark` for the
/// identical session, so the two can never disagree about its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionMark {
    Attention,
    Harness(HarnessMark),
    Agent(LiveState),
}

/// Priority: attention dot > the harness's own state mark > the agent's live
/// state.  A harness outranks the live axis because it watches the pane from
/// outside and alacritree only reads its title, so where both have a reading
/// the harness's is the better one — and drawing it in the harness's
/// vocabulary is what keeps a pane looking the same listed and attached.
///
/// Returns the word the mark explains on hover alongside it.  `None` covers a
/// shell session, which has no state to mark.
pub(super) fn session_status_mark(status: &RowStatus<'_>) -> Option<(SessionMark, String)> {
    if status.attention {
        return Some((SessionMark::Attention, ATTENTION_HINT.to_owned()));
    }
    if let Some(managed) = status.managed
        && let Some(mark) = managed.mark
    {
        return Some((
            SessionMark::Harness(mark),
            format!("{} says {}", managed.multiplexer, mark.label),
        ));
    }
    match status.activity {
        SessionActivity::Agent { name, live } => {
            Some((SessionMark::Agent(live), agent_hint(live, name)))
        },
        SessionActivity::Shell => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::{Pane, PaneStatus, Side};
    use crate::test_util::{listed_agent, managed};

    /// A native agent's mark is coloured by the state it reports, the same
    /// way a harness-backed row's mark is, so two rows in one state never
    /// disagree about what that state looks like.
    #[test]
    fn an_agent_mark_takes_its_colour_from_the_state_it_reports() {
        let theme = Theme::from_config(&Config::default());
        assert_eq!(
            agent_mark(LiveState::Idle, &theme),
            AgentMark::Glyph(DEFAULT_AGENT_ICON, theme.harness_state.of(StateTone::Idle))
        );
        assert_eq!(
            agent_mark(LiveState::Blocked, &theme),
            AgentMark::Glyph(DEFAULT_BLOCKED_ICON, theme.harness_state.of(StateTone::Blocked))
        );
        assert_eq!(agent_mark(LiveState::Working, &theme), AgentMark::Loader);
    }

    /// Attention outranks every other reading a row could have, a harness's
    /// included — a state that wants a human cannot also be quiet.
    #[test]
    fn session_status_mark_puts_attention_first() {
        let agent = listed_agent(Some("claude"));
        let managed = managed(&agent, &Side::Native, false);
        let status = RowStatus {
            attention: true,
            activity: SessionActivity::Shell,
            managed: Some(&managed),
        };
        let (mark, hint) = session_status_mark(&status).expect("attention always has a mark");
        assert_eq!(mark, SessionMark::Attention);
        assert_eq!(hint, ATTENTION_HINT);
    }

    /// A pane-backed session's mark and hover come from the same call the
    /// sidebar makes for the identical `Managed`, so the two can never
    /// disagree about what a pane is doing.
    #[test]
    fn session_status_mark_matches_the_sidebar_for_a_pane_backed_session() {
        let agent = Pane { status: Some(PaneStatus::Working), ..listed_agent(Some("claude")) };
        let managed = managed(&agent, &Side::Native, false);
        let activity = SessionActivity::agent(Some("claude"), LiveState::Idle);
        let status = RowStatus { attention: false, activity, managed: Some(&managed) };
        let (mark, hint) =
            session_status_mark(&status).expect("a listed pane with an agent has a mark");
        let harness_mark = managed.mark.expect("a listed agent always has one");
        assert_eq!(mark, SessionMark::Harness(harness_mark));
        assert_eq!(hint, format!("{} says {}", managed.multiplexer, harness_mark.label));
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
        assert_eq!(agent_hint(LiveState::Idle, Some("claude")), "claude is running");
        assert_eq!(agent_hint(LiveState::Working, Some("codex")), "codex is working");
        assert_eq!(agent_hint(LiveState::Blocked, Some("claude")), "claude is waiting for you");
        assert_eq!(agent_hint(LiveState::Blocked, None), "agent is waiting for you");
    }

    /// A shell session has no state axis to mark. The sidebar draws its own
    /// icon here instead of a status mark, and the palette leaves the slot
    /// empty, so both must read this as "no mark" rather than picking one.
    #[test]
    fn session_status_mark_picks_none_for_a_shell() {
        let status =
            RowStatus { attention: false, activity: SessionActivity::Shell, managed: None };
        assert!(session_status_mark(&status).is_none());
    }

    /// The palette paints the identical mark and hover the sidebar would for
    /// a local agent, whichever of the three live states it is in.
    #[test]
    fn session_status_mark_picks_each_live_state_for_a_local_agent() {
        for live in [LiveState::Idle, LiveState::Working, LiveState::Blocked] {
            let activity = SessionActivity::agent(Some("claude"), live);
            let status = RowStatus { attention: false, activity, managed: None };
            let (mark, hint) = session_status_mark(&status).expect("an agent always has a mark");
            assert_eq!(mark, SessionMark::Agent(live));
            assert_eq!(hint, agent_hint(live, Some("claude")));
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
