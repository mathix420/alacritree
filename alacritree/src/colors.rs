//! Resolve ANSI / 256-color values against the runtime palette (OSC 4) → user
//! config → built-in defaults, in that order.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

use crate::config::Palette;

pub(crate) fn rgb_to_color32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// The palette colours that no cell or escape sequence decides, converted
/// once for the painters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TerminalColors {
    pub fg: Color32,
    pub bg: Color32,
    /// The configured cursor, or the foreground when none is set.
    pub cursor: Color32,
    pub cursor_fg: Option<Color32>,
    pub selection_bg: Option<Color32>,
    pub selection_fg: Option<Color32>,
}

impl TerminalColors {
    pub(crate) fn new(palette: &Palette) -> Self {
        Self {
            fg: rgb_to_color32(palette.fg),
            bg: rgb_to_color32(palette.bg),
            cursor: rgb_to_color32(palette.cursor_bg.unwrap_or(palette.fg)),
            cursor_fg: palette.cursor_fg.map(rgb_to_color32),
            selection_bg: palette.selection_bg.map(rgb_to_color32),
            selection_fg: palette.selection_fg.map(rgb_to_color32),
        }
    }
}

/// The terminal's own default background, which OSC 11 can move away from the
/// configured one.  Everything painting behind the grid has to agree on this:
/// the background pass draws no quad for a cell already carrying it.
pub(crate) fn default_background(runtime: &Colors, colors: &TerminalColors) -> Color32 {
    runtime[NamedColor::Background].map_or(colors.bg, rgb_to_color32)
}

pub(crate) fn resolve(
    color: Color,
    flags: Flags,
    runtime: &Colors,
    palette: &Palette,
    is_fg: bool,
) -> Rgb {
    // Mirrors alacritty's `compute_fg_rgb` / `compute_bg_rgb`: backgrounds
    // never apply DIM or BOLD-as-bright; only the glyph color does.
    if !is_fg {
        return match color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(i) => resolve_indexed(i, runtime, palette),
            Color::Named(named) => resolve_named_raw(named, runtime, palette),
        };
    }

    match color {
        Color::Spec(rgb) => {
            if flags.contains(Flags::DIM) {
                apply_dim(rgb)
            } else {
                rgb
            }
        },
        Color::Indexed(idx) => resolve_indexed_fg(idx, flags, runtime, palette),
        Color::Named(named) => resolve_named_fg(named, flags, runtime, palette),
    }
}

/// The color to report for an OSC 4 / 10 / 11 / 12 query.  `None` for a cursor
/// color the app never set: alacritty leaves that query unanswered rather than
/// naming a color it doesn't have, and the asking app falls back to its own.
pub(crate) fn query(index: usize, runtime: &Colors, palette: &Palette) -> Option<Rgb> {
    if let Some(rgb) = runtime[index] {
        return Some(rgb);
    }
    match index {
        0..=255 => Some(resolve_indexed(index as u8, runtime, palette)),
        i if i == NamedColor::Foreground as usize => Some(palette.fg),
        i if i == NamedColor::Background as usize => Some(palette.bg),
        _ => None,
    }
}

fn resolve_indexed(index: u8, runtime: &Colors, palette: &Palette) -> Rgb {
    if let Some(rgb) = runtime[index as usize] {
        return rgb;
    }
    if (index as usize) < 16 {
        return ansi16(index as usize, palette);
    }
    if let Some(&(_, rgb)) = palette.indexed.iter().find(|(i, _)| *i == index) {
        return rgb;
    }
    indexed_default(index)
}

/// Foreground indexed lookup with BOLD-as-bright and DIM applied per
/// alacritty's `compute_fg_rgb` table (idx+8 / idx-8 / DimBlack+idx).
fn resolve_indexed_fg(idx: u8, flags: Flags, runtime: &Colors, palette: &Palette) -> Rgb {
    let dim_bold = flags & (Flags::DIM | Flags::BOLD);
    let promote_bright = palette.draw_bold_with_bright && dim_bold == Flags::BOLD && idx < 8;
    let dim_to_normal =
        !palette.draw_bold_with_bright && dim_bold == Flags::DIM && (8..=15).contains(&idx);
    let dim_to_dim_named = !palette.draw_bold_with_bright && dim_bold == Flags::DIM && idx < 8;

    if promote_bright {
        return resolve_indexed(idx + 8, runtime, palette);
    }
    if dim_to_normal {
        return resolve_indexed(idx - 8, runtime, palette);
    }
    if dim_to_dim_named {
        let dim_named = match idx {
            0 => NamedColor::DimBlack,
            1 => NamedColor::DimRed,
            2 => NamedColor::DimGreen,
            3 => NamedColor::DimYellow,
            4 => NamedColor::DimBlue,
            5 => NamedColor::DimMagenta,
            6 => NamedColor::DimCyan,
            _ => NamedColor::DimWhite,
        };
        return resolve_named_raw(dim_named, runtime, palette);
    }
    resolve_indexed(idx, runtime, palette)
}

fn resolve_named_raw(named: NamedColor, runtime: &Colors, palette: &Palette) -> Rgb {
    if let Some(rgb) = runtime[named] {
        return rgb;
    }
    palette_named(named, palette).unwrap_or_else(|| named_fallback(named, palette))
}

fn resolve_named_fg(named: NamedColor, flags: Flags, runtime: &Colors, palette: &Palette) -> Rgb {
    let dim_bold = flags & (Flags::DIM | Flags::BOLD);
    let bold_only = dim_bold == Flags::BOLD;
    let dim_only = dim_bold == Flags::DIM;
    let dim_bold_combined = dim_bold == (Flags::DIM | Flags::BOLD);

    let promoted =
        if dim_bold_combined && named == NamedColor::Foreground && palette.bright_fg.is_none() {
            // Without a configured bright foreground, alacritty drops the bold and dims.
            NamedColor::DimForeground
        } else if palette.draw_bold_with_bright && bold_only && (named as usize) < 8 {
            named.to_bright()
        } else if (dim_only || (dim_bold_combined && !palette.draw_bold_with_bright))
            && (named as usize) < 8
        {
            named.to_dim()
        } else if dim_only && named == NamedColor::Foreground {
            NamedColor::DimForeground
        } else {
            named
        };

    if let Some(rgb) = runtime[promoted] {
        return rgb;
    }

    palette_named(promoted, palette).unwrap_or_else(|| named_fallback(promoted, palette))
}

fn apply_dim(c: Rgb) -> Rgb {
    // alacritty's DIM_FACTOR.
    Rgb { r: (c.r as f32 * 0.66) as u8, g: (c.g as f32 * 0.66) as u8, b: (c.b as f32 * 0.66) as u8 }
}

fn ansi16(index: usize, palette: &Palette) -> Rgb {
    if index < 8 { palette.normal[index] } else { palette.bright[index - 8] }
}

fn palette_named(named: NamedColor, palette: &Palette) -> Option<Rgb> {
    use NamedColor::*;
    let n = named as usize;
    if n < 8 {
        return Some(palette.normal[n]);
    }
    if (8..16).contains(&n) {
        return Some(palette.bright[n - 8]);
    }
    match named {
        Foreground => Some(palette.fg),
        Background => Some(palette.bg),
        Cursor => palette.cursor_bg,
        BrightForeground => palette.bright_fg.or(Some(palette.fg)),
        DimForeground => palette.dim_fg.or_else(|| Some(dim_rgb(palette.fg))),
        DimBlack => palette.dim.map(|d| d[0]),
        DimRed => palette.dim.map(|d| d[1]),
        DimGreen => palette.dim.map(|d| d[2]),
        DimYellow => palette.dim.map(|d| d[3]),
        DimBlue => palette.dim.map(|d| d[4]),
        DimMagenta => palette.dim.map(|d| d[5]),
        DimCyan => palette.dim.map(|d| d[6]),
        DimWhite => palette.dim.map(|d| d[7]),
        _ => None,
    }
}

fn named_fallback(named: NamedColor, palette: &Palette) -> Rgb {
    // Reached only for Dim* when [colors.dim] is unset — fake it by darkening.
    use NamedColor::*;
    let normal = match named {
        DimBlack => palette.normal[0],
        DimRed => palette.normal[1],
        DimGreen => palette.normal[2],
        DimYellow => palette.normal[3],
        DimBlue => palette.normal[4],
        DimMagenta => palette.normal[5],
        DimCyan => palette.normal[6],
        DimWhite => palette.normal[7],
        _ => palette.fg,
    };
    dim_rgb(normal)
}

fn dim_rgb(c: Rgb) -> Rgb {
    apply_dim(c)
}

fn indexed_default(index: u8) -> Rgb {
    // Standard 6×6×6 cube + grayscale ramp for indices 16..256.
    if index < 232 {
        let i = index - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        return Rgb { r: cube_step(r), g: cube_step(g), b: cube_step(b) };
    }
    let level = 8 + 10 * (index - 232);
    Rgb { r: level, g: level, b: level }
}

fn cube_step(x: u8) -> u8 {
    match x {
        0 => 0,
        n => 55 + n * 40,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const RED: Rgb = Rgb { r: 200, g: 100, b: 50 };
    const BRIGHT_RED: Rgb = Rgb { r: 250, g: 120, b: 90 };

    fn palette() -> Palette {
        let mut palette = Config::default().palette;
        palette.normal[1] = RED;
        palette.bright[1] = BRIGHT_RED;
        palette.dim = None;
        palette.indexed.clear();
        palette
    }

    fn fg(color: Color, flags: Flags, runtime: &Colors, palette: &Palette) -> Rgb {
        resolve(color, flags, runtime, palette, true)
    }

    #[test]
    fn bold_text_takes_the_bright_color_only_when_configured_to() {
        let runtime = Colors::default();
        let mut palette = palette();

        palette.draw_bold_with_bright = true;
        assert_eq!(fg(Color::Named(NamedColor::Red), Flags::BOLD, &runtime, &palette), BRIGHT_RED);
        assert_eq!(fg(Color::Indexed(1), Flags::BOLD, &runtime, &palette), BRIGHT_RED);

        palette.draw_bold_with_bright = false;
        assert_eq!(fg(Color::Named(NamedColor::Red), Flags::BOLD, &runtime, &palette), RED);
        assert_eq!(fg(Color::Indexed(1), Flags::BOLD, &runtime, &palette), RED);
    }

    #[test]
    fn dim_text_darkens_its_color_when_no_dim_palette_is_set() {
        let runtime = Colors::default();
        let palette = palette();
        let darkened = Rgb { r: 132, g: 66, b: 33 };

        assert_eq!(fg(Color::Named(NamedColor::Red), Flags::DIM, &runtime, &palette), darkened);
        assert_eq!(fg(Color::Spec(RED), Flags::DIM, &runtime, &palette), darkened);
    }

    #[test]
    fn indexed_colors_past_the_ansi_sixteen_follow_the_cube_and_gray_ramp() {
        let runtime = Colors::default();
        let palette = palette();
        let indexed = |i| resolve(Color::Indexed(i), Flags::empty(), &runtime, &palette, false);

        assert_eq!(indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(indexed(21), Rgb { r: 0, g: 0, b: 255 });
        assert_eq!(indexed(67), Rgb { r: 95, g: 135, b: 175 });
        assert_eq!(indexed(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(indexed(255), Rgb { r: 238, g: 238, b: 238 });
    }

    #[test]
    fn a_named_color_prefers_the_runtime_then_the_palette_then_a_derived_dim() {
        let mut runtime = Colors::default();
        let mut palette = palette();
        let named = |n, runtime: &Colors, palette: &Palette| {
            resolve(Color::Named(n), Flags::empty(), runtime, palette, false)
        };

        assert_eq!(named(NamedColor::Red, &runtime, &palette), RED);
        let osc = Rgb { r: 1, g: 2, b: 3 };
        runtime[NamedColor::Red] = Some(osc);
        assert_eq!(named(NamedColor::Red, &runtime, &palette), osc);

        assert_eq!(named(NamedColor::DimRed, &runtime, &palette), Rgb { r: 132, g: 66, b: 33 });
        let configured_dim = Rgb { r: 9, g: 8, b: 7 };
        palette.dim = Some([configured_dim; 8]);
        assert_eq!(named(NamedColor::DimRed, &runtime, &palette), configured_dim);
    }
}
