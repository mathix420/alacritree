//! The committed `schema/alacritree-config.json` must match what
//! `alacritree schema` generates.  A stale schema is worse than none: editors
//! would report valid config as invalid, and stay quiet about the keys it does
//! not know about.
//!
//! `ALACRITREE_UPDATE_SCHEMA=1 cargo test -p alacritree --test config_schema`
//! rewrites the file instead of failing, so the run that catches the drift is
//! also the one that fixes it.

use std::path::PathBuf;
use std::process::Command;

fn generated() -> String {
    // The child here is alacritree itself, a GUI-subsystem binary that
    // allocates no console at all, so there is no window to hide. This crate
    // has no lib target, so an integration test cannot reach
    // `command_ext::hidden` to build it the sanctioned way.
    #[allow(clippy::disallowed_methods)]
    let out = Command::new(env!("CARGO_BIN_EXE_alacritree")).arg("schema").output().unwrap();
    assert!(out.status.success(), "`alacritree schema` failed");
    String::from_utf8(out.stdout).unwrap()
}

fn schema_path() -> PathBuf {
    // The manifest dir is `alacritree/`; the schema is published from the
    // repository root beside it.
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../schema/alacritree-config.json"))
}

#[test]
fn the_committed_schema_matches_the_config_types() {
    let generated = generated();
    let committed = std::fs::read_to_string(schema_path()).unwrap_or_default();
    if committed == generated {
        return;
    }
    if std::env::var("ALACRITREE_UPDATE_SCHEMA").as_deref() == Ok("1") {
        std::fs::create_dir_all(schema_path().parent().unwrap()).unwrap();
        std::fs::write(schema_path(), &generated).unwrap();
        return;
    }
    // Which key moved is what tells the reader whether the config types changed
    // on purpose, so report the first differing line rather than the verdict
    // alone — the document runs to a couple of thousand lines and a full-file
    // dump buries it.
    let (line, was, now) = committed
        .lines()
        .zip(generated.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map_or((0, "", ""), |(i, (a, b))| (i + 1, a, b));
    panic!(
        "schema/alacritree-config.json is stale — regenerate with \
         `ALACRITREE_UPDATE_SCHEMA=1 cargo test -p alacritree --test config_schema` \
         (or `cargo run -p alacritree -- schema > schema/alacritree-config.json`)\n\n\
         first difference at line {line}:\n  committed: {was}\n  generated: {now}"
    );
}

/// The whole point of the published document: an editor has to be able to find
/// it, and a config has to be able to name it.
#[test]
fn the_committed_schema_names_where_it_is_published() {
    let committed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path()).unwrap()).unwrap();
    assert_eq!(
        committed["$id"],
        "https://github.com/mathix420/alacritree/releases/latest/download/alacritree-config.json"
    );
}
