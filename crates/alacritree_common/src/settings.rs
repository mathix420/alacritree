//! Config helpers the sections of several crates share.

use schemars::JsonSchema;
use serde::Deserialize;
use strum::IntoEnumIterator;
use vte::ansi::Rgb;

/// A deprecated key applies only where the file omits its replacement, so a
/// replacement written at its default still wins. Raw config structs accept
/// unknown keys, so dropping the old field would lose the override silently.
pub fn moved_key<T>(new: Option<T>, old: Option<T>, from: &str, to: &str) -> Option<T> {
    if old.is_some() {
        log::warn!("{from} is deprecated; set {to}");
    }
    new.or(old)
}

/// A config key whose value is one of a fixed set of spellings.
///
/// One declaration answers for all three readers of such a key. A value the
/// set does not hold warns and falls back to the default rather than
/// rejecting the whole config, the dump writes back the spelling the config
/// file accepts, and the schema publishes the set beside that default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosedSet<T>(T);

/// What a closed-set key needs of its value type: a fixed set of variants,
/// one spelling each, and one of them the default.
pub trait ClosedSetValue: IntoEnumIterator + Default + Copy + Into<&'static str> + 'static {}

impl<T> ClosedSetValue for T where
    T: IntoEnumIterator + Default + Copy + Into<&'static str> + 'static
{
}

impl<T: ClosedSetValue> ClosedSet<T> {
    pub fn get(self) -> T {
        self.0
    }

    fn spellings() -> Vec<&'static str> {
        T::iter().map(Into::into).collect()
    }

    /// The type's own name, which is what a warning has to say instead of the
    /// key path: three `[ui] path_style` keys share one value type, so no type
    /// can name the key it was read from.
    fn set_name() -> &'static str {
        std::any::type_name::<T>().rsplit("::").next().unwrap_or("value")
    }
}

impl<T: Default> Default for ClosedSet<T> {
    fn default() -> Self {
        Self(T::default())
    }
}

impl<T: ClosedSetValue> From<T> for ClosedSet<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<'de, T: ClosedSetValue> Deserialize<'de> for ClosedSet<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let found = T::iter().find(|value| raw == Into::<&str>::into(*value));
        Ok(Self(found.unwrap_or_else(|| {
            let fallback: &'static str = T::default().into();
            log::warn!(
                "unknown {} value {raw:?}, using {fallback:?} (one of {})",
                Self::set_name(),
                Self::spellings().join(", ")
            );
            T::default()
        })))
    }
}

impl<T: ClosedSetValue> serde::Serialize for ClosedSet<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.into())
    }
}

/// Publishes the spellings so an editor completes and checks them, and the
/// default so the schema says what omitting the key resolves to. Inlined
/// rather than referenced, since the set is the whole definition.
impl<T: ClosedSetValue> JsonSchema for ClosedSet<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(Self::set_name())
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let values = Self::spellings();
        let default: &'static str = T::default().into();
        schemars::json_schema!({ "type": "string", "enum": values, "default": default })
    }
}

/// A sidebar icon's glyph and how to paint it.  Parses from a bare string,
/// accepted as glyph-only, or a table that also styles color, weight, slant,
/// and size.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct IconStyle<C = Rgb> {
    pub glyph: Option<String>,
    pub color: Option<C>,
    pub bold: bool,
    pub italic: bool,
    /// Logical pixels before `ui_scale`; clamped to the icon's slot at paint.
    pub size: Option<f32>,
}

impl<C> IconStyle<C> {
    pub fn or_glyph<'a>(&'a self, default: &'a str) -> &'a str {
        self.glyph.as_deref().map(str::trim).filter(|g| !g.is_empty()).unwrap_or(default)
    }
}

impl<C: Copy> IconStyle<C> {
    pub fn map_color<D>(&self, f: impl Fn(C) -> D) -> IconStyle<D> {
        IconStyle {
            glyph: self.glyph.clone(),
            color: self.color.map(f),
            bold: self.bold,
            italic: self.italic,
            size: self.size,
        }
    }
}

// The bare form is listed first so a plain string never attempts the table
// arm.
/// A styled icon override: either a bare glyph string (`worktree = "◆"`) or a
/// table (`worktree = { glyph = "◆", color = "#ff5555", bold = true }`).
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
#[serde(untagged)]
pub enum RawIconStyle {
    /// The glyph alone.
    Glyph(String),
    /// The glyph with styling.
    Table {
        /// The character to draw.  Unset keeps the built-in glyph and applies
        /// only the styling.
        glyph: Option<String>,
        /// Glyph color.  Unset inherits the row's foreground.
        color: Option<RgbStr>,
        /// Draw the glyph bold.
        #[serde(default)]
        bold: bool,
        /// Draw the glyph italic.
        #[serde(default)]
        italic: bool,
        /// Point size, clamped to a minimum of `1.0`.  Unset uses the sidebar
        /// font size.
        size: Option<f32>,
    },
}

impl From<RawIconStyle> for IconStyle {
    fn from(raw: RawIconStyle) -> Self {
        match raw {
            RawIconStyle::Glyph(glyph) => IconStyle { glyph: Some(glyph), ..Default::default() },
            RawIconStyle::Table { glyph, color, bold, italic, size } => IconStyle {
                glyph,
                color: color.map(|c| c.0),
                bold,
                italic,
                size: size.map(|s| s.max(1.0)),
            },
        }
    }
}

/// Wrapper that parses `"0xrrggbb"`, `"#rrggbb"`, or `"rrggbb"` into an `Rgb`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgbStr(pub Rgb);

/// Hand-written because `RgbStr` deserializes from a string it parses itself,
/// so nothing about the accepted spellings is visible to a derive.
impl JsonSchema for RgbStr {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Color".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(0[xX]|#)?[0-9a-fA-F]{6}$",
            "description": "An RGB color, written as \"#rrggbb\", \"0xrrggbb\" or \"rrggbb\".",
            "examples": ["#1c1c1c", "0x6a9fb5"],
        })
    }
}

impl<'de> Deserialize<'de> for RgbStr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        parse_hex_rgb(&s)
            .map(RgbStr)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid color string: {s:?}")))
    }
}

/// Hand-written for the same reason `Deserialize` is: the accepted spellings
/// live in `parse_hex_rgb`, and a derive on the inner `Rgb` would emit an
/// object against a schema that says `"type": "string"`.
impl serde::Serialize for RgbStr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Rgb { r, g, b } = self.0;
        serializer.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}"))
    }
}

fn parse_hex_rgb(s: &str) -> Option<Rgb> {
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| s.strip_prefix('#'))
        .unwrap_or(s);
    if stripped.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&stripped[0..2], 16).ok()?;
    let g = u8::from_str_radix(&stripped[2..4], 16).ok()?;
    let b = u8::from_str_radix(&stripped[4..6], 16).ok()?;
    Some(Rgb { r, g, b })
}
