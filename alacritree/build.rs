// Embeds the application icon into the Windows .exe so File Explorer, the
// taskbar, and Scoop-generated shortcuts all show the proper icon instead of
// the default executable glyph.  Also frees target exes that running
// alacritree processes pin: a mapped image cannot be overwritten, so linking
// over it fails with "Access is denied" until the file is renamed aside.

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

    #[cfg(windows)]
    {
        free_pinned_target_exes();
        embed_resource::compile("./windows/alacritree.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}

/// The linker writes `deps/alacritree-<hash>.exe` and cargo publishes it as
/// `alacritree.exe` (a hardlink or a copy), so both names must be free
/// before a relink.
/// Best-effort throughout: a failed rename leaves the build to fail at link
/// exactly as it would have anyway, plus a warning naming the culprit.
#[cfg(windows)]
fn free_pinned_target_exes() {
    // OUT_DIR = <target>/<profile>/build/alacritree-<hash>/out
    let Some(profile_dir) = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .and_then(|out| out.ancestors().nth(3).map(Path::to_path_buf))
    else {
        return;
    };
    let deps_dir = profile_dir.join("deps");
    sweep_stale(&profile_dir);
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
