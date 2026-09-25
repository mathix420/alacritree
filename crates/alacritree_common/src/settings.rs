//! Config helpers the sections of several crates share.

use schemars::JsonSchema;
use serde::Deserialize;
use strum::IntoEnumIterator;

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
