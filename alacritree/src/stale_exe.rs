// Rename-aside for executables pinned by running processes.
//
// A running process's exe image cannot be overwritten or deleted on Windows,
// but it can be renamed: the directory entry changes while the process keeps
// its mapped image.  Renaming a pinned exe aside frees its name for a fresh
// binary; the leftover is deleted by a later sweep, once its process exits.
//
// build.rs pastes this file in with `include!`, so it must stay std-only and
// must not open with a `//!` inner doc comment — included mid-crate, an inner
// attribute fails to parse.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Marks a renamed-aside file.  Changing the extension also keeps leftovers
/// out of PATH lookups and cargo's uplift.
pub(crate) const STALE_MARKER: &str = ".stale-";
/// Marks a not-yet-renamed install copy, so an interrupted install is swept
/// like a stale exe.
pub(crate) const TEMP_MARKER: &str = ".tmp-";

/// Delete leftovers from earlier rename-asides and interrupted installs.
/// Best-effort on purpose: a leftover whose process still runs refuses
/// deletion and waits for a later sweep.
pub(crate) fn sweep_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("alacritree")
            && (name.contains(STALE_MARKER) || name.contains(TEMP_MARKER))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Rename `path` aside if a running process holds it, returning the new name.
/// `Ok(None)` means the name is already free to write: missing, or openable
/// for writing in place.
pub(crate) fn rename_aside_if_locked(path: &Path) -> io::Result<Option<PathBuf>> {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        // Any other refusal reads as "held": a mapped image on Windows denies
        // write sharing, a running exe on Linux is ETXTBSY.
        Err(_) => {},
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let pid = std::process::id();
    for attempt in 0u32.. {
        let target = path.with_file_name(format!("{name}{STALE_MARKER}{pid}-{attempt}"));
        if target.exists() {
            continue;
        }
        return fs::rename(path, &target).map(|()| Some(target));
    }
    unreachable!("some pid-attempt suffix is free")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn touch(path: &Path) {
        fs::write(path, "old").unwrap();
    }

    /// The probe must not disturb a file nothing is running from — the common
    /// case for an install over a stopped binary.
    #[test]
    fn a_writable_file_stays_in_place() {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join("alacritree.exe");
        touch(&exe);

        assert_eq!(rename_aside_if_locked(&exe).unwrap(), None);
        assert!(exe.exists());
    }

    #[test]
    fn a_missing_file_needs_no_rename() {
        let dir = TempDir::new().unwrap();

        assert_eq!(rename_aside_if_locked(&dir.path().join("alacritree.exe")).unwrap(), None);
    }

    /// The mechanism this whole branch rests on: write sharing denied, rename
    /// still allowed — exactly what the loader grants a mapped exe image.
    #[cfg(windows)]
    #[test]
    fn a_held_file_is_renamed_aside() {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join("alacritree.exe");
        touch(&exe);
        let _hold = crate::test_util::hold_like_a_running_image(&exe);

        let moved = rename_aside_if_locked(&exe).unwrap().expect("a rename");

        assert!(!exe.exists(), "the original name is free again");
        assert!(moved.exists());
        assert!(moved.file_name().unwrap().to_string_lossy().contains(STALE_MARKER));
    }

    /// Two rename-asides from the same pid must not collide, or the second
    /// rebuild while two bridges run would fail on the suffix.
    #[cfg(windows)]
    #[test]
    fn a_second_rename_picks_a_fresh_name() {
        let dir = TempDir::new().unwrap();
        let exe = dir.path().join("alacritree.exe");
        touch(&exe);
        let _hold_one = crate::test_util::hold_like_a_running_image(&exe);
        let first = rename_aside_if_locked(&exe).unwrap().expect("a rename");
        touch(&exe);
        let _hold_two = crate::test_util::hold_like_a_running_image(&exe);

        let second = rename_aside_if_locked(&exe).unwrap().expect("a rename");

        assert_ne!(first, second);
        assert!(first.exists() && second.exists());
    }

    /// `~/.local/bin` and `target/` are shared directories: the sweep may only
    /// ever delete files this code created, recognised by name prefix + marker.
    #[test]
    fn the_sweep_removes_only_our_leftovers() {
        let dir = TempDir::new().unwrap();
        let stale = dir.path().join("alacritree.exe.stale-7-0");
        let temp = dir.path().join("alacritree.tmp-7");
        let live = dir.path().join("alacritree.exe");
        let foreign = dir.path().join("other.exe.stale-7-0");
        for f in [&stale, &temp, &live, &foreign] {
            touch(f);
        }

        sweep_stale(dir.path());

        assert!(!stale.exists() && !temp.exists());
        assert!(live.exists(), "the live binary is not sweepable");
        assert!(foreign.exists(), "other tools' files are not ours to delete");
    }

    #[test]
    fn a_sweep_of_a_missing_directory_is_a_no_op() {
        sweep_stale(Path::new("/nowhere/alacritree-sweep-test"));
    }
}
