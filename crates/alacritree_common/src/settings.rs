//! Config helpers the sections of several crates share.

/// A deprecated key applies only where the file omits its replacement, so a
/// replacement written at its default still wins. Raw config structs accept
/// unknown keys, so dropping the old field would lose the override silently.
pub fn moved_key<T>(new: Option<T>, old: Option<T>, from: &str, to: &str) -> Option<T> {
    if old.is_some() {
        log::warn!("{from} is deprecated; set {to}");
    }
    new.or(old)
}
