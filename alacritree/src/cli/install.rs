//! `alacritree install` — copy the running binary into a bin directory.
//!
//! Reading a running image is always allowed, so the source is simply
//! `current_exe()`.  What may be pinned is the *destination*: a window or MCP
//! bridge still running from an earlier install.  Its image cannot be
//! overwritten, but it can be renamed — the running process keeps working
//! from the renamed file, and a later install sweeps it once the process has
//! exited.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::stale_exe;

pub fn run(dest: Option<PathBuf>, as_json: bool) -> i32 {
    let installed = std::env::current_exe().and_then(|source| {
        let dir = destination(dest)?;
        install_file(&source, &dir)
            .map_err(|e| io::Error::other(format!("installing into {}: {e}", dir.display())))
    });
    match installed {
        Ok(installed) => {
            report(&installed, as_json);
            0
        },
        // In JSON mode the error goes to stdout as JSON too, matching how the
        // IPC-backed commands behave.
        Err(e) if as_json => {
            println!("{:#}", serde_json::json!({ "error": e.to_string() }));
            1
        },
        Err(e) => {
            eprintln!("alacritree: {e}");
            1
        },
    }
}

struct Installed {
    target: PathBuf,
    renamed_aside: Option<PathBuf>,
}

fn destination(dest: Option<PathBuf>) -> io::Result<PathBuf> {
    match dest {
        Some(dir) => Ok(dir),
        None => home::home_dir()
            .map(|home| home.join(".local").join("bin"))
            .ok_or_else(|| io::Error::other("no home directory — pass --dest")),
    }
}

/// The target name never points at a partial file: the copy lands under a
/// temp name and takes the target name in one rename.
fn install_file(source: &Path, dir: &Path) -> io::Result<Installed> {
    fs::create_dir_all(dir)?;
    stale_exe::sweep_stale(dir);
    let target = dir.join(format!("alacritree{}", std::env::consts::EXE_SUFFIX));
    // The source may be the target itself — a self-install from the installed
    // binary — so the copy must land before the target's name is freed.
    let temp = dir.join(format!("alacritree{}{}", stale_exe::TEMP_MARKER, std::process::id()));
    fs::copy(source, &temp)?;
    let renamed_aside = match stale_exe::rename_aside_if_locked(&target) {
        Ok(moved) => moved,
        Err(e) => {
            let _ = fs::remove_file(&temp);
            return Err(e);
        },
    };
    if let Err(e) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    Ok(Installed { target, renamed_aside })
}

fn report(installed: &Installed, as_json: bool) {
    if as_json {
        println!(
            "{:#}",
            serde_json::json!({
                "installed": &installed.target,
                "renamed_aside": &installed.renamed_aside,
            })
        );
        return;
    }
    println!("installed {}", installed.target.display());
    if let Some(old) = &installed.renamed_aside {
        println!(
            "a running alacritree still holds the old binary — moved to {} until it exits",
            old.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn source_exe(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("source-build.exe");
        fs::write(&path, content).unwrap();
        path
    }

    fn target_in(dir: &Path) -> PathBuf {
        dir.join(format!("alacritree{}", std::env::consts::EXE_SUFFIX))
    }

    /// `--dest` may name a directory that does not exist yet; a first install
    /// must not demand a manual mkdir.
    #[test]
    fn installs_into_a_directory_that_does_not_exist_yet() {
        let dir = TempDir::new().unwrap();
        let source = source_exe(dir.path(), "v2");
        let dest = dir.path().join("bin");

        let installed = install_file(&source, &dest).unwrap();

        assert_eq!(installed.target, target_in(&dest));
        assert_eq!(fs::read_to_string(&installed.target).unwrap(), "v2");
        assert_eq!(installed.renamed_aside, None);
    }

    #[test]
    fn replaces_a_previous_install_nothing_is_running_from() {
        let dir = TempDir::new().unwrap();
        let source = source_exe(dir.path(), "v2");
        let dest = dir.path().join("bin");
        fs::create_dir_all(&dest).unwrap();
        fs::write(target_in(&dest), "v1").unwrap();

        let installed = install_file(&source, &dest).unwrap();

        assert_eq!(fs::read_to_string(&installed.target).unwrap(), "v2");
        assert_eq!(installed.renamed_aside, None);
    }

    /// The point of the subcommand: installing over a binary the window or a
    /// bridge still runs from must succeed, not fail with access denied.
    #[cfg(windows)]
    #[test]
    fn a_pinned_previous_install_is_renamed_aside_and_replaced() {
        let dir = TempDir::new().unwrap();
        let source = source_exe(dir.path(), "v2");
        let dest = dir.path().join("bin");
        fs::create_dir_all(&dest).unwrap();
        fs::write(target_in(&dest), "v1").unwrap();
        let _running = crate::test_util::hold_like_a_running_image(&target_in(&dest));

        let installed = install_file(&source, &dest).unwrap();

        assert_eq!(fs::read_to_string(&installed.target).unwrap(), "v2");
        let aside = installed.renamed_aside.expect("the pinned exe was moved");
        assert_eq!(fs::read_to_string(&aside).unwrap(), "v1", "the running image is intact");
    }

    #[test]
    fn leftovers_from_earlier_installs_are_swept() {
        let dir = TempDir::new().unwrap();
        let source = source_exe(dir.path(), "v2");
        let dest = dir.path().join("bin");
        fs::create_dir_all(&dest).unwrap();
        let leftover = dest.join("alacritree.exe.stale-9-0");
        fs::write(&leftover, "v0").unwrap();

        install_file(&source, &dest).unwrap();

        assert!(!leftover.exists());
    }

    #[test]
    fn the_default_destination_is_local_bin() {
        let dest = destination(None).unwrap();

        assert!(dest.ends_with(Path::new(".local").join("bin")), "{}", dest.display());
    }

    /// `alacritree install` run from the installed binary itself: the source
    /// IS the target, and the process holds it.  The copy must land in the
    /// temp file before the target's name is freed, or a self-install deletes
    /// the very binary it is installing.
    #[cfg(windows)]
    #[test]
    fn a_self_install_survives_the_source_being_the_target() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("bin");
        fs::create_dir_all(&dest).unwrap();
        let target = target_in(&dest);
        fs::write(&target, "v1").unwrap();
        let _running = crate::test_util::hold_like_a_running_image(&target);

        let installed = install_file(&target, &dest).unwrap();

        assert_eq!(fs::read_to_string(&installed.target).unwrap(), "v1");
        assert!(installed.renamed_aside.is_some(), "the held image was moved aside");
    }
}
