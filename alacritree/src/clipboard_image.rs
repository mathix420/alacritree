//! Turning a clipboard bitmap into a file on disk that something else can open.
//!
//! Nothing here knows about the clipboard or about sessions: it takes pixels,
//! and it returns a path.  That is what keeps it testable without a window.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use arboard::ImageData;

use crate::jobs;

/// A clipboard owner can advertise any dimensions it likes, and encoding runs
/// on the UI thread during a keystroke.  64 MP is far past any screenshot.
const MAX_PIXELS: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum EncodeError {
    TooLarge { pixels: usize },
    Inconsistent { expected: usize, actual: usize },
    Encoding(png::EncodingError),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { pixels } => {
                write!(f, "{pixels} pixels is past the {MAX_PIXELS} limit")
            },
            Self::Inconsistent { expected, actual } => {
                write!(f, "dimensions imply {expected} bytes, got {actual}")
            },
            Self::Encoding(e) => write!(f, "{e}"),
        }
    }
}

/// `Compression::Fast` buys latency on a keypress at the cost of a larger file
/// that nothing keeps.
pub fn encode_png(image: &ImageData<'_>) -> Result<Vec<u8>, EncodeError> {
    let pixels = image.width.saturating_mul(image.height);
    if pixels > MAX_PIXELS {
        return Err(EncodeError::TooLarge { pixels });
    }
    let expected = pixels.saturating_mul(4);
    if image.bytes.len() != expected {
        return Err(EncodeError::Inconsistent { expected, actual: image.bytes.len() });
    }

    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width as u32, image.height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(EncodeError::Encoding)?;
    writer.write_image_data(&image.bytes).map_err(EncodeError::Encoding)?;
    writer.finish().map_err(EncodeError::Encoding)?;
    Ok(out)
}

/// The file a set of PNG bytes belongs in.  Content-addressed, so pasting the
/// same screenshot twice reuses one file, and the full 64-bit digest rather
/// than the scratchpad's truncated one, since here a collision would paste the
/// wrong image instead of merely colliding a label.
pub fn file_name(png: &[u8]) -> String {
    format!("clipboard-{:016x}.png", crate::digest::stable_digest(png))
}

/// Write `png` into `dir` under its content-addressed name and return the path.
///
/// `cap` bounds the directory to that many generated files and is `Some` only
/// for a directory alacritree owns — a directory the user named may hold files
/// alacritree never wrote, and a filename pattern is no proof of ownership.
pub fn store(dir: &Path, png: &[u8], cap: Option<usize>) -> io::Result<PathBuf> {
    if cap.is_some() {
        prepare_managed_dir(dir)?;
    } else {
        fs::create_dir_all(dir)?;
    }
    let path = dir.join(file_name(png));
    if !reusable(&path, png.len() as u64) {
        write_atomically(dir, &path, png)?;
    }
    Ok(path)
}

/// Trim the managed directory to its cap.  Separate from `store` because the
/// stored path is pasted into the terminal the moment it exists, while the
/// sweep is housekeeping nothing reads.
pub fn sweep(dir: &Path, keep: usize, in_use: &Path, _blocking: &jobs::Blocking) {
    apply_cap(dir, keep, in_use);
}

/// Create and revalidate the directory that alacritree owns.  The open uses
/// `O_NOFOLLOW` on Unix so a fixed-name symlink in a shared cache parent cannot
/// redirect screenshots into an attacker-controlled location.
#[cfg(unix)]
fn prepare_managed_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    let metadata = handle.metadata()?;
    // SAFETY: `geteuid` takes no arguments and has no safety preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by the current user", dir.display()),
        ));
    }
    handle.set_permissions(fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn prepare_managed_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Create a new image or staging file without ever exposing its contents to
/// other local users.  `create_new` also refuses pre-planted symlinks.
fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    options.open(path)
}

/// Open an existing generated image without following a replacement symlink,
/// and repair files produced by an older permissive version.
fn open_existing_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.custom_flags(libc::O_NOFOLLOW);
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        Ok(file)
    }
}

/// Whether the destination already holds these bytes *and* its timestamp was
/// refreshed.  Content addressing makes equal names strong evidence of equal
/// bytes, not proof, so the length is checked too; a link, a directory or a
/// timestamp that would not move all mean "write it again".
fn reusable(path: &Path, len: u64) -> bool {
    let Ok(file) = open_existing_private(path) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if metadata.len() != len {
        return false;
    }
    file.set_modified(SystemTime::now()).is_ok()
}

/// Write through a uniquely named temporary in the same directory so a reader
/// never opens a half-written PNG.
fn write_atomically(dir: &Path, path: &Path, png: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let tmp = dir.join(format!(
            "{}.{}.{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = match create_private_file(&tmp) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            result => result?,
        };
        if let Err(e) = file.write_all(png) {
            drop(file);
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        drop(file);

        clear_directory_at(path);
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // Another instance writing the same content first is a success,
                // but only once the destination is checked like the reuse path.
                if reusable(path, png.len() as u64) {
                    return Ok(());
                }
                return Err(e);
            },
        }
    }
}

/// `rename` replaces a file but cannot replace a directory, so a directory
/// squatting on a generated name would fail that image's every paste forever.
///
/// Only an empty one is removed.  A populated directory is something this
/// module did not create, and losing its contents to a name collision is a
/// far worse outcome than the paste failing.
fn clear_directory_at(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    if let Err(e) = fs::remove_dir(path) {
        log::debug!("could not clear the directory at {}: {e}", path.display());
    }
}

/// Keep the `keep` newest generated files, `in_use` always among them.
///
/// Failures are logged and skipped: a file that outlives its turn costs a few
/// hundred kilobytes, while giving up here would abandon the rest of the sweep.
fn apply_cap(dir: &Path, keep: usize, in_use: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            log::debug!("cannot sweep {}: {e}", dir.display());
            return;
        },
    };
    let mut generated: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                log::debug!("skipping an unreadable entry in {}: {e}", dir.display());
                continue;
            },
        };
        if !is_generated_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        if path == in_use {
            continue;
        }
        match open_existing_private(&path).and_then(|file| file.metadata()?.modified()) {
            Ok(when) => generated.push((when, path)),
            // Unranked means unswept: a file whose age cannot be read is never
            // the one chosen for deletion.
            Err(e) => log::debug!("cannot age {}: {e}", path.display()),
        }
    }

    let others = keep.saturating_sub(1);
    if generated.len() <= others {
        return;
    }
    generated.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
    for (_, stale) in generated.into_iter().skip(others) {
        if let Err(e) = fs::remove_file(&stale) {
            log::debug!("could not remove {}: {e}", stale.display());
        }
    }
}

/// Only names this module produces are ever deleted.  The `.tmp` suffix a
/// half-finished write leaves behind fails this too, so a crashed process
/// cannot have its leftovers swept by a later one — a trade for never
/// deleting something a user put here.
fn is_generated_name(name: &str) -> bool {
    name.strip_prefix("clipboard-")
        .and_then(|rest| rest.strip_suffix(".png"))
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::*;

    fn age(path: &Path, seconds: u64) {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - seconds);
        fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    fn image(width: usize, height: usize) -> ImageData<'static> {
        let bytes = (0..width * height * 4).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        ImageData { width, height, bytes: Cow::Owned(bytes) }
    }

    #[test]
    fn an_image_survives_the_encode_round_trip() {
        let source = image(7, 5);
        let png = encode_png(&source).expect("encodes");

        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().expect("valid png");
        let mut out = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut out).expect("one frame");

        assert_eq!((info.width, info.height), (7, 5));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&out[..info.buffer_size()], source.bytes.as_ref());
    }

    /// A clipboard owner can advertise any dimensions it likes.  Reject before
    /// allocating, because this runs on the UI thread during a keystroke.
    #[test]
    fn an_absurdly_large_image_is_rejected_before_allocating() {
        let huge = ImageData { width: usize::MAX, height: 4, bytes: Cow::Owned(Vec::new()) };
        assert!(matches!(encode_png(&huge), Err(EncodeError::TooLarge { .. })));
    }

    #[test]
    fn a_byte_count_disagreeing_with_the_dimensions_is_rejected() {
        let lying = ImageData { width: 4, height: 4, bytes: Cow::Owned(vec![0; 8]) };
        assert!(matches!(encode_png(&lying), Err(EncodeError::Inconsistent { .. })));
    }

    /// The name is the deduplication key: equal bytes must land on one file.
    #[test]
    fn the_file_name_is_a_function_of_the_content() {
        assert_eq!(file_name(b"same"), file_name(b"same"));
        assert_ne!(file_name(b"one"), file_name(b"two"));
    }

    #[test]
    fn the_file_name_is_sixteen_hex_digits_and_inert() {
        let name = file_name(b"payload");
        let hex =
            name.strip_prefix("clipboard-").and_then(|r| r.strip_suffix(".png")).expect("shape");
        assert_eq!(hex.len(), 16);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(crate::file_drop::is_terminal_safe(&name));
    }

    #[test]
    fn store_creates_a_missing_directory_and_writes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join("clipboard");

        let path = store(&dir, b"png bytes", None).unwrap();

        assert_eq!(path.file_name().unwrap(), file_name(b"png bytes").as_str());
        assert_eq!(fs::read(&path).unwrap(), b"png bytes");
    }

    #[cfg(unix)]
    #[test]
    fn managed_storage_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("clipboard");
        let path = store(&dir, b"private", Some(20)).unwrap();

        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_managed_directory_is_tightened() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("clipboard");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        store(&dir, b"private", Some(20)).unwrap();

        assert_eq!(fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_managed_directory_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let link = tmp.path().join("clipboard");
        fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();

        assert!(store(&link, b"private", Some(20)).is_err());
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_directory_symlink_is_preserved() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let link = tmp.path().join("configured");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &link).unwrap();

        let path = store(&link, b"private", None).unwrap();

        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_reused_image_is_tightened() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(file_name(b"private"));
        fs::write(&path, b"private").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        store(tmp.path(), b"private", None).unwrap();

        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn the_cap_tightens_retained_images() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let retained = store(tmp.path(), b"retained", None).unwrap();
        fs::set_permissions(&retained, fs::Permissions::from_mode(0o666)).unwrap();

        let new = store(tmp.path(), b"new", Some(2)).unwrap();
        jobs::on_this_thread(|blocking| sweep(tmp.path(), 2, &new, blocking));

        assert_eq!(fs::metadata(retained).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_symlink_is_replaced_not_followed() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("outside");
        let path = tmp.path().join(file_name(b"private"));
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, &path).unwrap();

        let stored = store(tmp.path(), b"private", None).unwrap();

        assert_eq!(stored, path);
        assert_eq!(fs::read(target).unwrap(), b"untouched");
        assert_eq!(fs::read(stored).unwrap(), b"private");
    }

    #[test]
    fn storing_the_same_bytes_twice_leaves_one_file() {
        let tmp = tempfile::tempdir().unwrap();

        let first = store(tmp.path(), b"same", None).unwrap();
        let second = store(tmp.path(), b"same", None).unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    /// Reuse must refresh the timestamp.  Without it a re-pasted old screenshot
    /// keeps its original mtime, and the next sweep — by which time it is no
    /// longer the returned path and so no longer exempt — deletes a file the
    /// user pasted moments ago.
    ///
    /// The sweep therefore has to run against a *later* store, not the reusing
    /// one: while `old` is the returned path `apply_cap` skips it outright, so
    /// a cap applied there would pass with or without the refresh.
    #[test]
    fn reuse_refreshes_the_timestamp_so_a_later_sweep_spares_it() {
        let tmp = tempfile::tempdir().unwrap();
        let old = store(tmp.path(), b"old", None).unwrap();
        age(&old, 9_000);
        for i in 0..3u8 {
            age(&store(tmp.path(), &[i], None).unwrap(), 1_000);
        }

        store(tmp.path(), b"old", None).unwrap();
        let newest = store(tmp.path(), b"newest", Some(2)).unwrap();
        jobs::on_this_thread(|blocking| sweep(tmp.path(), 2, &newest, blocking));

        assert!(old.is_file(), "a reused file was swept as though it were stale");
    }

    #[test]
    fn the_cap_keeps_the_newest_and_always_the_returned_path() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..6u8 {
            let path = store(tmp.path(), &[i], None).unwrap();
            age(&path, u64::from(6 - i) * 100);
        }

        let path = store(tmp.path(), b"newest", Some(3)).unwrap();
        jobs::on_this_thread(|blocking| sweep(tmp.path(), 3, &path, blocking));

        assert!(path.is_file());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 3);
    }

    /// A cap smaller than one still has to return a usable path.
    #[test]
    fn a_cap_of_one_keeps_only_the_returned_path() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..4u8 {
            store(tmp.path(), &[i], None).unwrap();
        }

        let path = store(tmp.path(), b"last", Some(1)).unwrap();
        jobs::on_this_thread(|blocking| sweep(tmp.path(), 1, &path, blocking));

        assert!(path.is_file());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    /// The guarantee that makes pointing image_dir at a pictures folder safe.
    #[test]
    fn no_cap_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5u8 {
            store(tmp.path(), &[i], None).unwrap();
        }

        store(tmp.path(), b"another", None).unwrap();

        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 6);
    }

    #[test]
    fn the_cap_never_touches_a_file_it_did_not_name() {
        let tmp = tempfile::tempdir().unwrap();
        let keeper = tmp.path().join("holiday.png");
        fs::write(&keeper, b"a real photo").unwrap();
        age(&keeper, 9_000);
        for i in 0..4u8 {
            store(tmp.path(), &[i], None).unwrap();
        }

        let newest = store(tmp.path(), b"newest", Some(1)).unwrap();
        jobs::on_this_thread(|blocking| sweep(tmp.path(), 1, &newest, blocking));

        assert!(keeper.is_file(), "a foreign file was deleted");
    }

    #[test]
    fn a_destination_of_the_wrong_length_is_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(file_name(b"payload"));
        fs::write(&path, b"truncated").unwrap();

        let stored = store(tmp.path(), b"payload", None).unwrap();

        assert_eq!(stored, path);
        assert_eq!(fs::read(&path).unwrap(), b"payload");
    }

    #[test]
    fn a_destination_that_is_a_directory_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(file_name(b"payload"))).unwrap();

        let stored = store(tmp.path(), b"payload", None).unwrap();

        assert_eq!(fs::read(&stored).unwrap(), b"payload");
    }

    /// The limit of that replacement.  Whatever a populated directory on this
    /// name is, it is not something this module wrote, and its contents are
    /// worth more than one paste succeeding.
    #[test]
    fn a_populated_directory_on_the_name_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let squatter = tmp.path().join(file_name(b"payload"));
        fs::create_dir(&squatter).unwrap();
        fs::write(squatter.join("precious.txt"), b"keep me").unwrap();

        assert!(store(tmp.path(), b"payload", None).is_err());
        assert_eq!(fs::read(squatter.join("precious.txt")).unwrap(), b"keep me");

        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn no_temp_file_survives_a_completed_store() {
        let tmp = tempfile::tempdir().unwrap();

        store(tmp.path(), b"payload", None).unwrap();

        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn storing_does_not_sweep() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // `store` names a file by hashing its bytes and never parses them, so
        // distinct payloads are all this needs to land under distinct names.
        for byte in 0..4_u8 {
            store(dir.path(), &[byte], Some(1)).expect("store");
        }
        let count = std::fs::read_dir(dir.path()).expect("read dir").count();
        assert_eq!(count, 4, "store must leave the cap to the sweep");
    }

    #[test]
    fn sweeping_applies_the_cap() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut last = PathBuf::new();
        for byte in 0..4_u8 {
            last = store(dir.path(), &[byte], Some(1)).expect("store");
        }
        jobs::on_this_thread(|blocking| sweep(dir.path(), 1, &last, blocking));
        let count = std::fs::read_dir(dir.path()).expect("read dir").count();
        assert_eq!(count, 1, "the sweep keeps the cap");
    }
}
