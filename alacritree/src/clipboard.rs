//! Thin wrapper around `arboard` that distinguishes the two clipboards we
//! actually care about: the regular system clipboard (Ctrl+V) and Linux's
//! PRIMARY selection (middle-click paste, alacritty's default auto-copy
//! target).  arboard's Linux backend supports both via the `SetExtLinux` /
//! `GetExtLinux` extensions when built with `wayland-data-control`.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
use arboard::{GetExtLinux, LinuxClipboardKind, SetExtLinux};

use crate::config::PasteConfig;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// `Ctrl+V` clipboard.
    Clipboard,
    /// Linux PRIMARY selection (X11 / Wayland primary).  Falls back to the
    /// regular clipboard on platforms that don't have a separate PRIMARY.
    Primary,
}

pub(crate) fn write(target: Target, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut clip = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("clipboard unavailable: {e}");
            return;
        },
    };
    let res = match target {
        Target::Clipboard => clip.set_text(text.to_owned()),
        #[cfg(target_os = "linux")]
        Target::Primary => clip.set().clipboard(LinuxClipboardKind::Primary).text(text.to_owned()),
        #[cfg(not(target_os = "linux"))]
        Target::Primary => clip.set_text(text.to_owned()),
    };
    if let Err(e) = res {
        log::warn!("clipboard write ({:?}) failed: {e}", target);
    }
}

pub(crate) fn read(target: Target) -> Option<String> {
    match read_text(target) {
        Probe::Found(text) => Some(text),
        Probe::Absent | Probe::Failed => None,
    }
}

/// What one clipboard probe found.  The distinction is load-bearing: `Absent`
/// means "try the next format", while `Failed` must stop the paste, because a
/// read that failed says nothing about whether the format was there.
pub(crate) enum Probe<T> {
    Found(T),
    Absent,
    Failed,
}

pub(crate) enum Payload {
    Text(String),
    Paths(Vec<PathBuf>),
    Image(arboard::ImageData<'static>),
    Nothing,
}

/// Turns an arboard result into a `Probe`, logging `label` (what was being
/// read, e.g. "clipboard text") against any error that isn't a plain absence.
fn classify<T>(label: &str, result: Result<T, arboard::Error>) -> Probe<T> {
    match result {
        Ok(value) => Probe::Found(value),
        Err(arboard::Error::ContentNotAvailable) => Probe::Absent,
        Err(e) => {
            log::warn!("clipboard read ({label}) failed: {e}");
            Probe::Failed
        },
    }
}

fn with_clipboard<T>(
    label: &str,
    read: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
) -> Probe<T> {
    match arboard::Clipboard::new() {
        Ok(mut clip) => classify(label, read(&mut clip)),
        Err(e) => {
            log::warn!("clipboard unavailable ({label}): {e}");
            Probe::Failed
        },
    }
}

pub(crate) fn read_text(target: Target) -> Probe<String> {
    let label = match target {
        Target::Clipboard => "clipboard text",
        Target::Primary => "primary selection",
    };
    with_clipboard(label, |clip| match target {
        Target::Clipboard => clip.get_text(),
        #[cfg(target_os = "linux")]
        Target::Primary => clip.get().clipboard(LinuxClipboardKind::Primary).text(),
        #[cfg(not(target_os = "linux"))]
        Target::Primary => clip.get_text(),
    })
}

/// Paths a file manager put on the clipboard.  Explorer's Cut advertises a move
/// effect alongside the same list; reading the paths neither performs nor
/// completes that move, so Cut and Copy paste identically.
pub(crate) fn read_files() -> Probe<Vec<PathBuf>> {
    with_clipboard("file list", |clip| clip.get().file_list())
}

pub(crate) fn read_image() -> Probe<arboard::ImageData<'static>> {
    with_clipboard("image", |clip| clip.get_image())
}

/// Resolve the clipboard in priority order, probing lazily: text outright, then
/// copied paths, then a bitmap.  Each probe runs only once every earlier one
/// came back absent, so an ordinary text paste never opens the image formats,
/// and a format the config switched off is never probed at all.
pub(crate) fn resolve(
    cfg: &PasteConfig,
    text: impl FnOnce() -> Probe<String>,
    files: impl FnOnce() -> Probe<Vec<PathBuf>>,
    image: impl FnOnce() -> Probe<arboard::ImageData<'static>>,
) -> Payload {
    match text() {
        Probe::Found(text) => return Payload::Text(text),
        Probe::Failed => return Payload::Nothing,
        Probe::Absent => {},
    }
    if cfg.files {
        match files() {
            // An empty list is a degenerate CF_HDROP, not a decision to paste
            // nothing; fall through rather than stopping here.
            Probe::Found(paths) if !paths.is_empty() => return Payload::Paths(paths),
            Probe::Failed => return Payload::Nothing,
            _ => {},
        }
    }
    if cfg.image {
        match image() {
            Probe::Found(image) => return Payload::Image(image),
            Probe::Failed => return Payload::Nothing,
            Probe::Absent => {},
        }
    }
    Payload::Nothing
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::PathBuf;

    use super::*;
    use crate::config::PasteConfig;

    fn absent<T>() -> Probe<T> {
        Probe::Absent
    }

    fn image() -> Probe<arboard::ImageData<'static>> {
        Probe::Found(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: Cow::Owned(vec![0, 0, 0, 255]),
        })
    }

    fn paths() -> Probe<Vec<PathBuf>> {
        Probe::Found(vec![PathBuf::from("/a/one.png")])
    }

    #[test]
    fn text_wins_over_every_other_format() {
        let payload =
            resolve(&PasteConfig::default(), || Probe::Found("hello".to_string()), paths, image);
        assert!(matches!(payload, Payload::Text(t) if t == "hello"));
    }

    #[test]
    fn a_file_list_is_used_when_there_is_no_text() {
        let payload = resolve(&PasteConfig::default(), absent, paths, image);
        assert!(matches!(payload, Payload::Paths(p) if p.len() == 1));
    }

    #[test]
    fn a_bitmap_is_used_when_there_is_neither_text_nor_a_file_list() {
        let payload = resolve(&PasteConfig::default(), absent, absent, image);
        assert!(matches!(payload, Payload::Image(_)));
    }

    /// A failed read is not evidence of an absent format.  Pasting the image
    /// because the *text* read failed would paste something the user never
    /// asked for.
    #[test]
    fn a_failed_read_aborts_instead_of_falling_through() {
        let payload = resolve(&PasteConfig::default(), || Probe::Failed, paths, image);
        assert!(matches!(payload, Payload::Nothing));

        let payload = resolve(&PasteConfig::default(), absent, || Probe::Failed, image);
        assert!(matches!(payload, Payload::Nothing));
    }

    /// A degenerate CF_HDROP would otherwise stop resolution while pasting
    /// nothing at all.
    #[test]
    fn an_empty_file_list_falls_through_to_the_bitmap() {
        let payload = resolve(&PasteConfig::default(), absent, || Probe::Found(Vec::new()), image);
        assert!(matches!(payload, Payload::Image(_)));
    }

    #[test]
    fn each_fallback_can_be_switched_off_on_its_own() {
        let no_files = PasteConfig { files: false, ..PasteConfig::default() };
        let payload = resolve(&no_files, absent, paths, image);
        assert!(matches!(payload, Payload::Image(_)), "files off must skip to the bitmap");

        let no_image = PasteConfig { image: false, ..PasteConfig::default() };
        let payload = resolve(&no_image, absent, absent, image);
        assert!(matches!(payload, Payload::Nothing));
    }

    /// Both off is today's behavior exactly: no text, no paste.
    #[test]
    fn both_fallbacks_off_restores_the_original_behavior() {
        let off = PasteConfig { files: false, image: false, ..PasteConfig::default() };
        let payload = resolve(&off, absent, paths, image);
        assert!(matches!(payload, Payload::Nothing));
    }

    /// A disabled format is never probed, so a clipboard whose image read would
    /// hang or warn costs nothing when the user turned it off.
    #[test]
    fn a_disabled_format_is_never_probed() {
        let off = PasteConfig { files: false, image: false, ..PasteConfig::default() };
        resolve(
            &off,
            absent,
            || panic!("file list probed while disabled"),
            || panic!("image probed while disabled"),
        );
    }

    #[test]
    fn an_absent_format_maps_to_absent_and_other_errors_to_failed() {
        assert!(matches!(
            classify("test", Err::<(), _>(arboard::Error::ContentNotAvailable)),
            Probe::Absent
        ));
        assert!(matches!(
            classify("test", Err::<(), _>(arboard::Error::ClipboardOccupied)),
            Probe::Failed
        ));
        assert!(matches!(classify("test", Ok::<_, arboard::Error>(7)), Probe::Found(7)));
    }
}
