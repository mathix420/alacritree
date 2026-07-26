// Embeds the application icon into the Windows .exe so File Explorer, the
// taskbar, and Scoop-generated shortcuts all show the proper icon instead of
// the default executable glyph.  Also frees target exes that running
// alacritree processes pin: a mapped image cannot be overwritten, so linking
// over it fails with "Access is denied" until the file is renamed aside.
// Finally, stages the vendored console host beside the freshly linked exe.

#[cfg(windows)]
include!("src/stale_exe.rs");

fn main() {
    // embed_resource emits its own narrow rerun-if-changed directives, which
    // would otherwise stop this script from running ahead of source-change
    // relinks.  Directory-scoped directives keep the rename-aside in step
    // with every build that writes a new exe.  (A dependency-only change
    // relinks without a rerun — accepted: the vendored crates are effectively
    // frozen in this fork.)
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=windows");
    println!("cargo:rerun-if-changed=vendor");

    #[cfg(windows)]
    {
        if let Some(profile_dir) = profile_dir() {
            free_pinned_target_exes(&profile_dir);
            stage_vendored_conpty(&profile_dir);
        }
        embed_resource::compile("./windows/alacritree.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}

/// Where cargo publishes the linked exe.
#[cfg(windows)]
fn profile_dir() -> Option<PathBuf> {
    // OUT_DIR = <target>/<profile>/build/alacritree-<hash>/out
    std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .and_then(|out| out.ancestors().nth(3).map(Path::to_path_buf))
}

/// The linker writes `deps/alacritree-<hash>.exe` and cargo publishes it as
/// `alacritree.exe` (a hardlink or a copy), so both names must be free
/// before a relink.
/// Best-effort throughout: a failed rename leaves the build to fail at link
/// exactly as it would have anyway, plus a warning naming the culprit.
#[cfg(windows)]
fn free_pinned_target_exes(profile_dir: &Path) {
    let deps_dir = profile_dir.join("deps");
    sweep_stale(profile_dir);
    sweep_stale(&deps_dir);

    let mut candidates = vec![profile_dir.join("alacritree.exe")];
    if let Ok(entries) = fs::read_dir(&deps_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("alacritree") && name.ends_with(".exe") {
                candidates.push(entry.path());
            }
        }
    }
    for exe in candidates {
        if let Err(e) = rename_aside_if_locked(&exe) {
            println!("cargo:warning=cannot move the pinned {} aside: {e}", exe.display());
        }
    }
}

/// `conpty.dll` and the `OpenConsole.exe` it launches, which together host the
/// PTY far faster than the console server in the box (`vendor/conpty`).
#[cfg(windows)]
const VENDORED_CONPTY: [&str; 2] = ["conpty.dll", "OpenConsole.exe"];

/// Copy the vendored console host next to the exe cargo just linked.
///
/// `LoadLibraryW("conpty.dll")` resolves against the executable's directory,
/// and `harden_dll_search_path` leaves that the only non-system entry in the
/// search order, so a `cargo run` build only gets the faster host if the files
/// are staged beside it.  Release archives are populated by dist's `include`
/// instead, which reads the same directory.
///
/// Best-effort: a pane running out of this profile has the DLL mapped and
/// denies the overwrite.  The stale copy is the same file on every build that
/// does not bump the vendored version, so warning and carrying on beats
/// failing the build.
#[cfg(windows)]
fn stage_vendored_conpty(profile_dir: &Path) {
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else { return };
    let vendor_dir = PathBuf::from(manifest_dir).join("vendor").join("conpty");

    for name in VENDORED_CONPTY {
        let source = vendor_dir.join(name);
        // A checkout without the vendored host is supported: alacritty_terminal
        // falls back to the console API and the pane just runs slower.
        let Ok(source_meta) = fs::metadata(&source) else { continue };
        let staged = profile_dir.join(name);
        if is_current(&source_meta, &staged) {
            continue;
        }
        if let Err(e) = fs::copy(&source, &staged) {
            println!("cargo:warning=cannot stage {name} beside the exe: {e}");
        }
    }
}

/// Whether `staged` already holds what `source` would copy over it.
#[cfg(windows)]
fn is_current(source: &fs::Metadata, staged: &Path) -> bool {
    let Ok(staged_meta) = fs::metadata(staged) else { return false };
    staged_meta.len() == source.len() && staged_meta.modified().ok() >= source.modified().ok()
}
